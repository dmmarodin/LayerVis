//! Geology on the atlas: tectonic uplift (world), stream-power erosion and
//! flow/rivers (atlas).
//!
//! Erosion follows Braun & Willett 2013 / Cordonnier et al. 2016: every
//! iteration routes water over a depression-filled surface (priority-flood,
//! Barnes et al. 2014), accumulates drainage area in stack order and lowers
//! each land cell implicitly towards its receiver
//! (`dh/dt = U - K A^m S`, n = 1), so the update is stable at any time step.
//! A thermal pass keeps slopes under the talus angle. Ocean cells (below sea
//! level) and the atlas border are base level: they are not eroded.
//!
//! Everything is sequential with deterministic tie-breaks, so the same inputs
//! give bit-identical grids.

use std::{borrow::Cow, cmp::Reverse, collections::BinaryHeap};

use crate::{
    atlas::{AtlasContext, AtlasGrid, AtlasIo, AtlasRun, AtlasStage, BoxedAtlasStage, RiverPath},
    materials::MaterialTable,
    pipeline::{BoxedWorldStep, Build, FieldSpec, InputSpec, NodeKind, ParamSpec, WorldStep},
    strata::{field, Rocks, StrataGrid, STRATA_LAYERS},
    tectonics::{Tectonics, TectonicsConfig},
    world::MAX_PLATES,
    FieldScope, ValueKind, WORLD_SLOTS,
};

const fn spec(
    name: &'static str,
    scope: FieldScope,
    unit: &'static str,
    max: f32,
    ramp: &'static str,
) -> FieldSpec {
    FieldSpec {
        name: Cow::Borrowed(name),
        scope,
        kind: ValueKind::Scalar,
        unit: Cow::Borrowed(unit),
        min: 0.0,
        max,
        ramp: Cow::Borrowed(ramp),
        display_from: None,
    }
}

const fn input(name: &'static str, scope: FieldScope) -> InputSpec {
    InputSpec {
        name: Cow::Borrowed(name),
        scope,
        kind: ValueKind::Scalar,
        optional: false,
    }
}

const fn optional(name: &'static str, kind: ValueKind) -> InputSpec {
    InputSpec {
        name: Cow::Borrowed(name),
        scope: FieldScope::Atlas,
        kind,
        optional: true,
    }
}

/// Rain weight per cell for drainage accumulation: precipitation in units of
/// 1000 mm/yr (so discharge stays "cells drained" at 1000 mm of rain).
fn rain_weight(precipitation: Option<&AtlasGrid>) -> Option<Vec<f32>> {
    precipitation.map(|p| p.values().iter().map(|mm| (mm / 1000.0).max(0.0)).collect())
}

const fn param(name: &'static str, default: f64, min: f64, max: f64) -> ParamSpec {
    ParamSpec {
        name,
        default,
        min,
        max,
    }
}

// ---------------------------------------------------------------------------
// Tectonic uplift (world)

/// Uplift rate from plate motion: what pushes mountains up while erosion
/// wears them down.
pub static TECTONIC_UPLIFT: NodeKind = NodeKind {
    id: "tectonic_uplift",
    label: "Tectonic uplift",
    description: "Uplift rate (cells per time unit) from plate motion: colliding plates \
                  push the crust up near their boundary, most where two continents meet. \
                  Feeds erosion's `uplift` input (through a rasterize node).",
    inputs: &[
        input("boundary_distance", FieldScope::World),
        input("convergence", FieldScope::World),
        input("continental", FieldScope::World),
        input("neighbor_continental", FieldScope::World),
    ],
    outputs: &[spec("uplift", FieldScope::World, "cells/t", 0.5, "thermal")],
    params: &[
        // Peak uplift rate (cells per time unit) of a head-on continental collision.
        param("rate", 0.3, 0.0, 2.0),
        // Distance (columns) over which uplift fades away from the boundary.
        param("width", 220.0, 20.0, 2000.0),
        // Background uplift of continental crust (keeps old land above the sea).
        param("continental", 0.02, 0.0, 0.5),
    ],
    build: Build::World(|bindings, params, _| {
        Ok(Box::new(UpliftStep {
            boundary_distance: bindings.input("boundary_distance")?,
            convergence: bindings.input("convergence")?,
            continental: bindings.input("continental")?,
            neighbor_continental: bindings.input("neighbor_continental")?,
            out: bindings.output("uplift")?,
            rate: params.get("rate"),
            width: params.get("width"),
            base: params.get("continental"),
        }) as BoxedWorldStep)
    }),
};

struct UpliftStep {
    boundary_distance: usize,
    convergence: usize,
    continental: usize,
    neighbor_continental: usize,
    out: usize,
    rate: f64,
    width: f64,
    base: f64,
}

impl WorldStep for UpliftStep {
    fn run(&self, _x: i64, _y: i64, slots: &mut [f64; WORLD_SLOTS]) {
        let own = slots[self.continental] >= 0.5;
        let other = slots[self.neighbor_continental] >= 0.5;
        let falloff = (-slots[self.boundary_distance] / self.width).exp();
        let convergence = slots[self.convergence].max(0.0);

        let strength = match (own, other) {
            (true, true) => 1.0,
            (true, false) | (false, true) => 0.7,
            (false, false) => 0.3,
        };
        let background = if own { self.base } else { 0.0 };

        slots[self.out] = (convergence * strength * falloff * self.rate + background).max(0.0);
    }
}

// ---------------------------------------------------------------------------
// Drainage (shared by erosion and flow)

const DIAGONAL: f32 = std::f32::consts::SQRT_2;
const NEIGHBORS: [(i32, i32, f32); 8] = [
    (-1, -1, DIAGONAL),
    (0, -1, 1.0),
    (1, -1, DIAGONAL),
    (-1, 0, 1.0),
    (1, 0, 1.0),
    (-1, 1, DIAGONAL),
    (0, 1, 1.0),
    (1, 1, DIAGONAL),
];

/// Total order key for f32 heights (smaller pops first).
fn key(value: f32) -> u32 {
    let bits = value.to_bits();
    if bits >> 31 == 1 {
        !bits
    } else {
        bits | (1 << 31)
    }
}

/// Water routing over a grid: every cell drains to an outlet (ocean or the
/// atlas border) through `receiver`, and `order` lists cells downstream
/// first (every receiver comes before the cells draining into it).
pub(crate) struct Drainage {
    /// Depression-filled surface (with `epsilon` slope across flats).
    filled: Vec<f32>,
    order: Vec<u32>,
    /// Receiver cell; outlets are their own receiver.
    receiver: Vec<u32>,
    /// Distance to the receiver in cells (1 or √2; 0 for outlets).
    distance: Vec<f32>,
}

impl Drainage {
    pub(crate) fn compute(heights: &[f32], width: u32, height: u32, sea: f32, epsilon: f32) -> Self {
        let n = heights.len();
        let (w, h) = (width as i32, height as i32);
        let mut filled = heights.to_vec();
        let mut visited = vec![false; n];
        let mut heap: BinaryHeap<Reverse<(u32, u32)>> = BinaryHeap::new();
        let mut order = Vec::with_capacity(n);

        for i in 0..n {
            let (x, y) = ((i % width as usize) as i32, (i / width as usize) as i32);
            if heights[i] < sea || x == 0 || y == 0 || x == w - 1 || y == h - 1 {
                visited[i] = true;
                heap.push(Reverse((key(filled[i]), i as u32)));
            }
        }

        while let Some(Reverse((_, i))) = heap.pop() {
            order.push(i);
            let (x, y) = ((i % width) as i32, (i / width) as i32);
            for (dx, dy, _) in NEIGHBORS {
                let (nx, ny) = (x + dx, y + dy);
                if nx < 0 || ny < 0 || nx >= w || ny >= h {
                    continue;
                }
                let j = (nx + ny * w) as usize;
                if !visited[j] {
                    visited[j] = true;
                    filled[j] = filled[j].max(filled[i as usize] + epsilon);
                    heap.push(Reverse((key(filled[j]), j as u32)));
                }
            }
        }

        let mut receiver: Vec<u32> = (0..n as u32).collect();
        let mut distance = vec![0.0f32; n];

        for i in 0..n {
            let (x, y) = ((i % width as usize) as i32, (i / width as usize) as i32);
            let outlet = heights[i] < sea || x == 0 || y == 0 || x == w - 1 || y == h - 1;
            if outlet {
                continue;
            }
            let mut best = (0.0f32, i, 0.0f32);
            for (dx, dy, d) in NEIGHBORS {
                let j = ((x + dx) + (y + dy) * w) as usize;
                let drop = (filled[i] - filled[j]) / d;
                if drop > best.0 {
                    best = (drop, j, d);
                }
            }
            receiver[i] = best.1 as u32;
            distance[i] = best.2;
        }

        Self {
            filled,
            order,
            receiver,
            distance,
        }
    }

    /// Drainage area (in cells, weighted by `weight` when given), accumulated
    /// from the sources down.
    pub(crate) fn accumulate(&self, weight: Option<&[f32]>) -> Vec<f32> {
        let mut area: Vec<f32> = match weight {
            Some(weight) => weight.iter().map(|w| w.max(0.0)).collect(),
            None => vec![1.0; self.receiver.len()],
        };
        for &i in self.order.iter().rev() {
            let r = self.receiver[i as usize];
            if r != i {
                area[r as usize] += area[i as usize];
            }
        }
        area
    }
}

// ---------------------------------------------------------------------------
// Erosion (atlas)

/// Uplift + stream-power erosion over a number of time steps.
pub static EROSION: NodeKind = NodeKind {
    id: "erosion",
    label: "Erosion",
    description: "Stream-power erosion with tectonic uplift: rivers cut valleys whose \
                  depth grows with the area they drain, while `uplift` pushes the land up. \
                  Hard limits on slope (talus) keep cliffs plausible. Ocean (below sea \
                  level) and the atlas border are base level. With `strata`, hard rock \
                  wears slower than soft rock (ridges stay, deeper layers get exposed); \
                  with `precipitation`, wet regions drain (and cut) more.",
    inputs: &[
        input("elevation", FieldScope::Atlas),
        input("uplift", FieldScope::Atlas),
        optional("strata", ValueKind::Strata),
        optional("precipitation", ValueKind::Scalar),
    ],
    outputs: &[
        spec("elevation", FieldScope::Atlas, "cells", 128.0, "terrain"),
        spec("drainage", FieldScope::Atlas, "cells", 2000.0, "thermal"),
        spec("eroded", FieldScope::Atlas, "cells", 40.0, "thermal"),
        field(
            "surface_material",
            FieldScope::Atlas,
            ValueKind::Scalar,
            "",
            16.0,
            "material",
        ),
        field(
            "strata",
            FieldScope::Atlas,
            ValueKind::Strata,
            "",
            1.0,
            "grayscale",
        ),
    ],
    params: &[
        param("iterations", 150.0, 1.0, 2000.0),
        // Time per iteration (the implicit update is stable at any step).
        param("time_step", 2.0, 0.1, 50.0),
        // Erosion constant K: higher cuts deeper, gentler valleys.
        param("strength", 0.05, 0.0, 2.0),
        // Drainage-area exponent m (the slope exponent n is 1).
        param("area_exponent", 0.5, 0.2, 1.0),
        // Scales the uplift input.
        param("uplift_scale", 1.0, 0.0, 10.0),
        // Steepest slope kept by the thermal pass (height / horizontal distance).
        param("talus", 1.0, 0.05, 10.0),
    ],
    build: Build::Atlas(|bindings, params, _| {
        Ok(Box::new(ErosionStage {
            elevation: bindings.input("elevation")?,
            uplift: bindings.input("uplift")?,
            strata: bindings.optional_input("strata"),
            precipitation: bindings.optional_input("precipitation"),
            out_elevation: bindings.output("elevation")?,
            out_drainage: bindings.output("drainage")?,
            out_eroded: bindings.output("eroded")?,
            out_surface: bindings.output("surface_material")?,
            out_strata: bindings.output("strata")?,
            iterations: params.get("iterations").round() as u32,
            time_step: params.get("time_step") as f32,
            strength: params.get("strength") as f32,
            area_exponent: params.get("area_exponent") as f32,
            uplift_scale: params.get("uplift_scale") as f32,
            talus: params.get("talus") as f32,
        }) as BoxedAtlasStage)
    }),
};

struct ErosionStage {
    elevation: usize,
    uplift: usize,
    strata: Option<usize>,
    precipitation: Option<usize>,
    out_elevation: usize,
    out_drainage: usize,
    out_eroded: usize,
    out_surface: usize,
    out_strata: usize,
    iterations: u32,
    time_step: f32,
    strength: f32,
    area_exponent: f32,
    uplift_scale: f32,
    talus: f32,
}

/// Flat-crossing slope added by the depression fill, in cells.
const EPSILON: f32 = 1e-3;

/// Snapshots per erosion run (the scrubber's frames).
const EROSION_FRAMES: u32 = 32;

/// Stream-power constants of one erosion run.
#[derive(Clone, Copy)]
pub(crate) struct StreamPower {
    pub strength: f32,
    pub dt: f32,
    pub area_exponent: f32,
    /// Talus limit per cell step, in cells of height.
    pub max_drop: f32,
}

/// The evolving surface of an erosion run (shared by `erosion` and the
/// epochs of `geology`).
pub(crate) struct Surface {
    pub width: u32,
    pub height: u32,
    pub heights: Vec<f32>,
    /// Uplift each cell received since its strata were laid down.
    pub lifted: Vec<f32>,
    pub eroded: Vec<f32>,
    pub area: Vec<f32>,
}

impl Surface {
    pub fn new(heights: Vec<f32>, width: u32, height: u32) -> Self {
        let n = heights.len();
        Self {
            width,
            height,
            heights,
            lifted: vec![0.0; n],
            eroded: vec![0.0; n],
            area: vec![1.0; n],
        }
    }

    /// One iteration: drainage (weighted by `rain`), the implicit
    /// stream-power update with erodibility from the strata layer at the
    /// surface times `factor`, then the thermal pass. `strata` comes with the
    /// erodibility of each of its layers. Without strata and factor every
    /// cell has erodibility 1.
    #[allow(clippy::too_many_arguments)]
    pub fn iterate(
        &mut self,
        uplift: &[f32],
        rain: Option<&[f32]>,
        strata: Option<(&StrataGrid, &[f32])>,
        factor: Option<&[f32]>,
        sea: f32,
        top: f32,
        p: StreamPower,
    ) {
        let (width, height) = (self.width, self.height);
        let heights = &mut self.heights;
        let drainage = Drainage::compute(heights, width, height, sea, EPSILON);
        self.area = drainage.accumulate(rain);
        let area = &self.area;
        let lifted = &mut self.lifted;
        // Erodibility of the layer at the surface (times `factor`).
        let erodibility = |i: usize, surface: f32, lifted: f32| -> f32 {
            let base = match strata {
                Some((s, layers)) => {
                    let tops = s.layers(i).1;
                    let depth = surface - lifted;
                    let layer = tops
                        .iter()
                        .position(|&top| depth <= top)
                        .unwrap_or(STRATA_LAYERS - 1);
                    layers[i * STRATA_LAYERS + layer]
                }
                None => 1.0,
            };
            base * factor.map_or(1.0, |f| f[i])
        };

        // Implicit stream-power update, downstream first: a receiver's new
        // height is known when its donors are updated.
        for &i in &drainage.order {
            let i = i as usize;
            let r = drainage.receiver[i] as usize;
            if r == i {
                continue; // base level
            }
            let before = heights[i];
            let k = erodibility(i, before, lifted[i]);
            let raised = before + p.dt * uplift[i];
            lifted[i] += p.dt * uplift[i];
            let next = if raised <= heights[r] {
                raised // in a filled depression: a lake bed, not eroded
            } else {
                let f = p.strength * k * p.dt * area[i].powf(p.area_exponent)
                    / drainage.distance[i];
                (raised + f * heights[r]) / (1.0 + f)
            };
            heights[i] = next.clamp(0.0, top);
            self.eroded[i] += raised - heights[i];
        }

        thermal(heights, width, height, sea, p.max_drop);
    }
}

/// Erodibility of every strata layer (cells × layers).
pub(crate) fn layer_erodibility(strata: &StrataGrid, table: &MaterialTable) -> Vec<f32> {
    strata
        .materials()
        .iter()
        .map(|&m| table.erodibility(m))
        .collect()
}

impl AtlasStage for ErosionStage {
    fn run(&self, io: &mut AtlasIo<'_>, run: &mut AtlasRun<'_>) -> Result<(), String> {
        let context = *io.context();
        let (width, height) = context.grid;
        let sea = context.sea_level();
        let top = context.depth as f32 - 1.0;
        let uplift: Vec<f32> = io
            .input(self.uplift)?
            .values()
            .iter()
            .map(|u| u * self.uplift_scale)
            .collect();
        let strata = self.strata.map(|slot| io.strata_input(slot)).transpose()?;
        let rain = rain_weight(io.optional(self.precipitation)?);
        let rocks = Rocks::new(io.materials())?;
        let layers = strata.map(|s| layer_erodibility(s, io.materials()));
        let mut surface = Surface::new(io.input(self.elevation)?.values().to_vec(), width, height);
        let power = StreamPower {
            strength: self.strength,
            dt: self.time_step,
            area_exponent: self.area_exponent,
            max_drop: self.talus * context.cell_size as f32,
        };
        let frame_every = (self.iterations / EROSION_FRAMES).max(1);

        run.snapshot(
            "start",
            &AtlasGrid::from_values(width, height, surface.heights.clone())?,
        );

        for iteration in 0..self.iterations {
            run.check()?;
            surface.iterate(
                &uplift,
                rain.as_deref(),
                strata.zip(layers.as_deref()),
                None,
                sea,
                top,
                power,
            );

            if (iteration + 1) % frame_every == 0 || iteration + 1 == self.iterations {
                run.snapshot(
                    format!("step {}", iteration + 1),
                    &AtlasGrid::from_values(width, height, surface.heights.clone())?,
                );
            }
            run.progress(f64::from(iteration + 1) / f64::from(self.iterations));
        }

        let (out_strata, materials) = eroded_strata(
            strata,
            &surface.heights,
            &surface.lifted,
            &rocks,
            strata.map(|_| SoilCover::for_context(&context)),
            width,
            height,
        );
        io.write_strata(self.out_strata, out_strata)?;
        io.write_values(self.out_surface, materials)?;
        io.write_values(self.out_elevation, surface.heights)?;
        io.write_values(self.out_drainage, surface.area)?;
        io.write_values(self.out_eroded, surface.eroded)?;
        Ok(())
    }
}

/// The strata after erosion: layers moved up by the uplift each cell got and
/// cut off at the new surface; material deposited above the old stack (by
/// the thermal pass) becomes gravel. Without input strata: granite up to the
/// surface. Also returns the surface material per cell.
pub(crate) fn eroded_strata(
    strata: Option<&StrataGrid>,
    heights: &[f32],
    lifted: &[f32],
    rocks: &Rocks,
    cover: Option<SoilCover>,
    width: u32,
    height: u32,
) -> (StrataGrid, Vec<f32>) {
    let mut out = StrataGrid::uniform(width, height, rocks.granite, 0.0);
    let mut surface = vec![0.0f32; heights.len()];
    let (w, hh) = (width as usize, height as usize);
    let loose = |m: u16| [rocks.soil, rocks.sand, rocks.gravel, rocks.clay].contains(&m);
    for i in 0..heights.len() {
        let h = heights[i];
        let mut layers: Vec<(u16, f32)> = Vec::with_capacity(STRATA_LAYERS);
        match strata {
            None => layers.push((rocks.granite, h)),
            Some(s) => {
                let (materials, tops) = s.layers(i);
                for k in 0..STRATA_LAYERS {
                    let top = (tops[k] + lifted[i]).min(h);
                    let bottom = if k == 0 {
                        f32::MIN
                    } else {
                        (tops[k - 1] + lifted[i]).min(h)
                    };
                    if k == 0 || top > bottom {
                        layers.push((materials[k], top));
                    }
                }
                let stack_top = tops[STRATA_LAYERS - 1] + lifted[i];
                if h > stack_top + 0.05 {
                    if layers.len() < STRATA_LAYERS {
                        layers.push((rocks.gravel, h));
                    } else if let Some(last) = layers.last_mut() {
                        last.1 = h;
                    }
                }

                // Weathering: bare rock on gentle land below the mountains
                // gets a thin soil again.
                if let Some(cover) = cover {
                    let (x, y) = (i % w, i / w);
                    let at = |x: usize, y: usize| heights[x.min(w - 1) + y.min(hh - 1) * w];
                    let slope = [
                        at(x + 1, y),
                        at(x.saturating_sub(1), y),
                        at(x, y + 1),
                        at(x, y.saturating_sub(1)),
                    ]
                    .iter()
                    .map(|n| (n - h).abs())
                    .fold(0.0f32, f32::max);
                    let rock = layers.last().is_some_and(|l| !loose(l.0));
                    if rock && h >= cover.sea + 0.5 && h < cover.mountain && slope < cover.slope
                    {
                        let base = h - cover.depth;
                        let mut kept: Vec<(u16, f32)> = Vec::with_capacity(STRATA_LAYERS);
                        for &(m, top) in &layers {
                            let top = top.min(base);
                            if kept.is_empty() || top > kept[kept.len() - 1].1 {
                                kept.push((m, top));
                            }
                        }
                        kept.truncate(STRATA_LAYERS - 1);
                        kept.push((rocks.soil, h));
                        layers = kept;
                    }
                }
            }
        }
        out.set_layers(i, &layers);
        let material = out.material_at(i, h);
        surface[i] = f32::from(material);
    }
    (out, surface)
}

/// Soil that forms again on eroded, gentle land (see `eroded_strata`).
#[derive(Clone, Copy)]
pub(crate) struct SoilCover {
    pub sea: f32,
    /// Above this height the ground stays bare.
    pub mountain: f32,
    /// Steepest height difference (cells) to a neighbor cell that keeps soil.
    pub slope: f32,
    /// Soil thickness in cells.
    pub depth: f32,
}

impl SoilCover {
    pub fn for_context(context: &AtlasContext) -> Self {
        let sea = context.sea_level();
        Self {
            sea,
            mountain: sea + 0.55 * (context.depth as f32 - sea),
            slope: 0.25 * context.cell_size as f32,
            depth: 1.0,
        }
    }
}

/// Moves material down slopes steeper than `max_drop` per cell step (half the
/// excess per pass, to the steepest neighbor). Ocean cells take material but
/// are not lowered.
fn thermal(heights: &mut [f32], width: u32, height: u32, sea: f32, max_drop: f32) {
    let (w, h) = (width as i32, height as i32);
    let source = heights.to_vec();

    for y in 1..h - 1 {
        for x in 1..w - 1 {
            let i = (x + y * w) as usize;
            if source[i] < sea {
                continue;
            }
            let mut steepest = (0.0f32, i);
            for (dx, dy, d) in NEIGHBORS {
                let j = ((x + dx) + (y + dy) * w) as usize;
                let excess = source[i] - source[j] - max_drop * d;
                if excess > steepest.0 {
                    steepest = (excess, j);
                }
            }
            if steepest.0 > 0.0 {
                let moved = steepest.0 * 0.25;
                heights[i] -= moved;
                heights[steepest.1] += moved;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Flow (atlas)

/// Lakes, discharge and river lines over a finished terrain.
pub static FLOW: NodeKind = NodeKind {
    id: "flow",
    label: "Flow",
    description: "Routes rain over the terrain: fills depressions into lakes, accumulates \
                  discharge (cells drained) and traces rivers wherever discharge passes \
                  `river_threshold`. River lines are kept on the atlas for the river overlay \
                  and later carving.",
    inputs: &[
        input("elevation", FieldScope::Atlas),
        optional("precipitation", ValueKind::Scalar),
    ],
    outputs: &[
        spec("water_level", FieldScope::Atlas, "cells", 128.0, "terrain"),
        spec("lake_depth", FieldScope::Atlas, "cells", 20.0, "thermal"),
        spec("discharge", FieldScope::Atlas, "cells", 2000.0, "thermal"),
        spec("river", FieldScope::Atlas, "", 1.0, "grayscale"),
        field(
            "rivers",
            FieldScope::Atlas,
            ValueKind::Rivers,
            "",
            1.0,
            "grayscale",
        ),
    ],
    params: &[
        // Drainage area (cells) where a river starts.
        param("river_threshold", 400.0, 4.0, 1_000_000.0),
    ],
    build: Build::Atlas(|bindings, params, _| {
        Ok(Box::new(FlowStage {
            elevation: bindings.input("elevation")?,
            precipitation: bindings.optional_input("precipitation"),
            out_level: bindings.output("water_level")?,
            out_lake: bindings.output("lake_depth")?,
            out_discharge: bindings.output("discharge")?,
            out_river: bindings.output("river")?,
            out_rivers: bindings.output("rivers")?,
            threshold: params.get("river_threshold") as f32,
        }) as BoxedAtlasStage)
    }),
};

struct FlowStage {
    elevation: usize,
    precipitation: Option<usize>,
    out_level: usize,
    out_lake: usize,
    out_discharge: usize,
    out_river: usize,
    out_rivers: usize,
    threshold: f32,
}

impl AtlasStage for FlowStage {
    fn run(&self, io: &mut AtlasIo<'_>, run: &mut AtlasRun<'_>) -> Result<(), String> {
        let context = *io.context();
        let (width, height) = context.grid;
        let sea = context.sea_level();
        let heights = io.input(self.elevation)?.values().to_vec();

        // Lake surfaces: the fill without a slope across flats.
        let level = Drainage::compute(&heights, width, height, sea, 0.0).filled;
        run.progress(0.3);
        run.check()?;
        let drainage = Drainage::compute(&heights, width, height, sea, EPSILON);
        let rain = rain_weight(io.optional(self.precipitation)?);
        let discharge = drainage.accumulate(rain.as_deref());
        run.progress(0.6);

        let lake: Vec<f32> = heights
            .iter()
            .zip(&level)
            .map(|(&h, &l)| if h >= sea { (l - h).max(0.0) } else { 0.0 })
            .collect();
        let water_level: Vec<f32> = heights
            .iter()
            .zip(&level)
            .map(|(&h, &l)| h.max(l).max(sea))
            .collect();

        // River strength: 0 below the threshold, 1 at 100x the threshold.
        let threshold = self.threshold.max(1.0);
        let river: Vec<f32> = discharge
            .iter()
            .zip(&heights)
            .map(|(&q, &h)| {
                if q < threshold || h < sea {
                    0.0
                } else {
                    (0.25 + 0.75 * (q / threshold).log10() / 2.0).clamp(0.0, 1.0)
                }
            })
            .collect();

        let rivers = trace_rivers(
            &drainage,
            &discharge,
            &heights,
            &water_level,
            sea,
            threshold,
            &context,
        );
        run.progress(1.0);

        io.write(
            self.out_level,
            AtlasGrid::from_values(width, height, water_level)?,
        )?;
        io.write(self.out_lake, AtlasGrid::from_values(width, height, lake)?)?;
        io.write(
            self.out_discharge,
            AtlasGrid::from_values(width, height, discharge)?,
        )?;
        io.write(
            self.out_river,
            AtlasGrid::from_values(width, height, river)?,
        )?;
        io.write_rivers(self.out_rivers, rivers);
        Ok(())
    }
}

/// River lines: from every source (a river cell no other river cell drains
/// into) down its receivers to an outlet or to a river already traced (the
/// confluence point ends the line).
fn trace_rivers(
    drainage: &Drainage,
    discharge: &[f32],
    heights: &[f32],
    level: &[f32],
    sea: f32,
    threshold: f32,
    context: &AtlasContext,
) -> Vec<RiverPath> {
    let n = discharge.len();
    let width = context.grid.0 as usize;
    let is_river = |i: usize| discharge[i] >= threshold && heights[i] >= sea;
    let mut fed = vec![false; n];
    for i in 0..n {
        let r = drainage.receiver[i] as usize;
        if r != i && is_river(i) {
            fed[r] = true;
        }
    }

    let point = |i: usize| context.world_point((i % width) as f64, (i / width) as f64);
    let mut visited = vec![false; n];
    let mut rivers = Vec::new();

    for source in 0..n {
        if !is_river(source) || fed[source] {
            continue;
        }
        let mut path = RiverPath {
            points: Vec::new(),
            discharge: Vec::new(),
            level: Vec::new(),
        };
        let mut i = source;
        loop {
            path.points.push(point(i));
            path.discharge.push(discharge[i]);
            path.level.push(level[i]);
            let r = drainage.receiver[i] as usize;
            if visited[i] || r == i {
                break;
            }
            visited[i] = true;
            i = r;
        }
        if path.points.len() >= 2 {
            rivers.push(path);
        }
    }

    rivers
}

// ---------------------------------------------------------------------------
// Geological epochs (atlas, milestone A5)

/// Uplift rate of a plate sample (the `tectonic_uplift` formula).
fn plate_uplift(
    tectonics: &Tectonics,
    x: i64,
    y: i64,
    rate: f64,
    width: f64,
    background: f64,
) -> f32 {
    let sample = tectonics.sample(x, y);
    let plates = tectonics.plates();
    let own = plates[sample.plate].continental;
    let other = plates[sample.neighbor].continental;
    let falloff = (-sample.boundary_distance / width).exp();
    let strength = match (own, other) {
        (true, true) => 1.0,
        (true, false) | (false, true) => 0.7,
        (false, false) => 0.3,
    };
    let base = if own { background } else { 0.0 };
    (sample.convergence.max(0.0) * strength * falloff * rate + base).max(0.0) as f32
}

pub static GEOLOGY: NodeKind = NodeKind {
    id: "geology",
    label: "Geology (epochs)",
    description: "Several geological epochs in one node: plates drift (boundaries and \
                  mountain belts move between epochs; the last epoch is today's layout), \
                  the sea level rises and falls, cold epochs grow glaciers that carve \
                  harder, and every epoch runs stream-power erosion on the strata. High \
                  seas lay down new sediment (limestone where warm, shale elsewhere) on \
                  flooded land. Set the plate params like the graph's `tectonics` node. \
                  A heavier alternative to `erosion`.",
    inputs: &[
        input("elevation", FieldScope::Atlas),
        optional("strata", ValueKind::Strata),
        optional("precipitation", ValueKind::Scalar),
    ],
    outputs: &[
        spec("elevation", FieldScope::Atlas, "cells", 128.0, "terrain"),
        spec("drainage", FieldScope::Atlas, "cells", 2000.0, "thermal"),
        spec("glaciated", FieldScope::Atlas, "", 1.0, "thermal"),
        field(
            "surface_material",
            FieldScope::Atlas,
            ValueKind::Scalar,
            "",
            16.0,
            "material",
        ),
        field(
            "strata",
            FieldScope::Atlas,
            ValueKind::Strata,
            "",
            1.0,
            "grayscale",
        ),
    ],
    params: &[
        // The plate layout (same meaning and defaults as `tectonics`).
        param("plate_size", 1500.0, 400.0, 4096.0),
        param("continents", 0.0, 0.0, 4.0),
        param("continental_fraction", 0.4, 0.05, 0.9),
        param("warp", 0.35, 0.0, 0.8),
        param("epochs", 4.0, 1.0, 12.0),
        // How far plates move per epoch (columns at full plate speed).
        param("drift", 150.0, 0.0, 2000.0),
        param("iterations", 40.0, 1.0, 500.0),
        param("time_step", 2.0, 0.1, 50.0),
        param("strength", 0.05, 0.0, 2.0),
        param("area_exponent", 0.5, 0.2, 1.0),
        param("talus", 1.0, 0.05, 10.0),
        // Uplift (the `tectonic_uplift` params).
        param("rate", 0.3, 0.0, 2.0),
        param("width", 220.0, 20.0, 2000.0),
        param("continental", 0.02, 0.0, 0.5),
        // Sea level swing between epochs, in cells.
        param("sea_cycle", 6.0, 0.0, 30.0),
        // Erosion multiplier under glaciers.
        param("glaciation", 2.5, 1.0, 10.0),
        // Glaciers form where cold-epoch temperatures are below this (°C).
        param("ice_temperature", -2.0, -30.0, 15.0),
        // Sediment laid down on flooded land per high-sea epoch, in cells.
        param("sediment", 3.0, 0.0, 20.0),
    ],
    build: Build::Atlas(|bindings, params, context| {
        let size = params.get("plate_size");
        let area = f64::from(context.size.0) * f64::from(context.size.1);
        let plate_count = ((area / (size * size)).round() as usize).clamp(2, MAX_PLATES);
        let tectonics = Tectonics::generate(
            context.seed,
            context.origin,
            context.size,
            &TectonicsConfig {
                plate_count,
                continents: params.get("continents").round() as usize,
                continental_fraction: params.get("continental_fraction"),
                warp: params.get("warp"),
            },
        );
        Ok(Box::new(GeologyStage {
            elevation: bindings.input("elevation")?,
            strata: bindings.optional_input("strata"),
            precipitation: bindings.optional_input("precipitation"),
            out_elevation: bindings.output("elevation")?,
            out_drainage: bindings.output("drainage")?,
            out_glaciated: bindings.output("glaciated")?,
            out_surface: bindings.output("surface_material")?,
            out_strata: bindings.output("strata")?,
            tectonics,
            epochs: params.get("epochs").round() as u32,
            drift: params.get("drift"),
            iterations: params.get("iterations").round() as u32,
            time_step: params.get("time_step") as f32,
            strength: params.get("strength") as f32,
            area_exponent: params.get("area_exponent") as f32,
            talus: params.get("talus") as f32,
            rate: params.get("rate"),
            width: params.get("width"),
            background: params.get("continental"),
            sea_cycle: params.get("sea_cycle") as f32,
            glaciation: params.get("glaciation") as f32,
            ice_temperature: params.get("ice_temperature") as f32,
            sediment: params.get("sediment") as f32,
        }) as BoxedAtlasStage)
    }),
};

struct GeologyStage {
    elevation: usize,
    strata: Option<usize>,
    precipitation: Option<usize>,
    out_elevation: usize,
    out_drainage: usize,
    out_glaciated: usize,
    out_surface: usize,
    out_strata: usize,
    tectonics: Tectonics,
    epochs: u32,
    drift: f64,
    iterations: u32,
    time_step: f32,
    strength: f32,
    area_exponent: f32,
    talus: f32,
    rate: f64,
    width: f64,
    background: f64,
    sea_cycle: f32,
    glaciation: f32,
    ice_temperature: f32,
    sediment: f32,
}

impl AtlasStage for GeologyStage {
    fn run(&self, io: &mut AtlasIo<'_>, run: &mut AtlasRun<'_>) -> Result<(), String> {
        let context = *io.context();
        let (width, height) = context.grid;
        let sea = context.sea_level();
        let top = context.depth as f32 - 1.0;
        let table = io.materials().clone();
        let rocks = Rocks::new(&table)?;
        let rain = rain_weight(io.optional(self.precipitation)?);
        let heights = io.input(self.elevation)?.values().to_vec();
        let n = heights.len();
        let mut strata = match self.strata {
            Some(slot) => io.strata_input(slot)?.clone(),
            None => {
                let mut s = StrataGrid::uniform(width, height, rocks.granite, 0.0);
                for (i, &h) in heights.iter().enumerate() {
                    s.set_layers(i, &[(rocks.granite, h)]);
                }
                s
            }
        };
        let mut surface = Surface::new(heights, width, height);
        let mut glaciated = vec![0.0f32; n];
        let power = StreamPower {
            strength: self.strength,
            dt: self.time_step,
            area_exponent: self.area_exponent,
            max_drop: self.talus * context.cell_size as f32,
        };
        let epochs = self.epochs.max(1);
        let total = f64::from(epochs * self.iterations.max(1));
        let mut uplift = vec![0.0f32; n];
        let mut factor = vec![1.0f32; n];

        run.snapshot(
            "start",
            &AtlasGrid::from_values(width, height, surface.heights.clone())?,
        );

        for epoch in 0..epochs {
            // Epochs run forward in time; the last one is today's layout.
            let age = f64::from(epochs - 1 - epoch);
            let plates = self.tectonics.moved(-age * self.drift);
            let glacial = (epochs - 1 - epoch) % 2 == 1;
            let sea_now = if epoch + 1 == epochs {
                sea
            } else if glacial {
                sea - self.sea_cycle
            } else {
                sea + self.sea_cycle * 0.5
            };

            for cy in 0..height {
                run.check()?;
                let latitude = ((f64::from(cy) + 0.5) / f64::from(height) * 2.0 - 1.0).abs();
                for cx in 0..width {
                    let i = (cx + cy * width) as usize;
                    let (x, y) = context.cell_point(cx, cy, 0.5, 0.5);
                    uplift[i] =
                        plate_uplift(&plates, x, y, self.rate, self.width, self.background);
                    // Glacial climate: the `temperature` node's formula, 6 °C colder.
                    let temperature = 32.0
                        - 50.0 * latitude as f32
                        - 0.25 * surface.heights[i].max(sea_now)
                        - 6.0;
                    let ice = glacial
                        && surface.heights[i] >= sea_now
                        && temperature < self.ice_temperature;
                    factor[i] = if ice { self.glaciation } else { 1.0 };
                    if ice {
                        glaciated[i] += 1.0 / epochs as f32;
                    }
                }
            }

            let layers = layer_erodibility(&strata, &table);
            for iteration in 0..self.iterations {
                run.check()?;
                surface.iterate(
                    &uplift,
                    rain.as_deref(),
                    Some((&strata, &layers)),
                    Some(&factor),
                    sea_now,
                    top,
                    power,
                );
                run.progress(f64::from(epoch * self.iterations + iteration + 1) / total);
            }

            // Rebase the strata on the new surface (uplift moved them).
            strata = eroded_strata(
                Some(&strata),
                &surface.heights,
                &surface.lifted,
                &rocks,
                Some(SoilCover::for_context(&context)),
                width,
                height,
            )
            .0;
            surface.lifted.fill(0.0);

            // High seas lay down sediment on flooded land.
            if sea_now > sea && self.sediment > 0.0 {
                for cy in 0..height {
                    let warm = ((f64::from(cy) + 0.5) / f64::from(height) * 2.0 - 1.0).abs() < 0.4;
                    for cx in 0..width {
                        let i = (cx + cy * width) as usize;
                        let h = surface.heights[i];
                        if h < sea_now && h > sea_now - 20.0 {
                            let raised = (h + self.sediment).min(sea_now);
                            if raised > h {
                                let material = if warm { rocks.limestone } else { rocks.shale };
                                let (materials, tops) = strata.layers(i);
                                let mut layers: Vec<(u16, f32)> = materials
                                    .iter()
                                    .zip(tops)
                                    .enumerate()
                                    .filter(|(k, (_, &t))| *k == 0 || t > tops[k - 1])
                                    .map(|(_, (&m, &t))| (m, t))
                                    .collect();
                                if layers.len() < STRATA_LAYERS {
                                    layers.push((material, raised));
                                } else if let Some(last) = layers.last_mut() {
                                    *last = (material, raised);
                                }
                                strata.set_layers(i, &layers);
                                surface.heights[i] = raised;
                            }
                        }
                    }
                }
            }

            run.snapshot(
                format!(
                    "epoch {} ({})",
                    epoch + 1,
                    if glacial { "glacial" } else { "warm" }
                ),
                &AtlasGrid::from_values(width, height, surface.heights.clone())?,
            );
        }

        let surface_material: Vec<f32> = (0..n)
            .map(|i| f32::from(strata.material_at(i, surface.heights[i])))
            .collect();
        io.write_strata(self.out_strata, strata)?;
        io.write_values(self.out_surface, surface_material)?;
        io.write_values(self.out_glaciated, glaciated)?;
        io.write_values(self.out_drainage, surface.area)?;
        io.write_values(self.out_elevation, surface.heights)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A tilted plane with a bowl: water must still reach the low edge.
    fn bowl(width: u32, height: u32) -> Vec<f32> {
        (0..width * height)
            .map(|i| {
                let (x, y) = ((i % width) as f32, (i / width) as f32);
                let (cx, cy) = (width as f32 / 2.0, height as f32 / 2.0);
                let bowl = -20.0 * (-((x - cx).powi(2) + (y - cy).powi(2)) / 30.0).exp();
                60.0 + x * 0.5 + bowl
            })
            .collect()
    }

    #[test]
    fn every_cell_drains_to_an_outlet_downstream_first() {
        let (w, h) = (24, 20);
        let heights = bowl(w, h);
        let d = Drainage::compute(&heights, w, h, 38.4, EPSILON);
        let mut position = vec![usize::MAX; heights.len()];
        for (k, &i) in d.order.iter().enumerate() {
            position[i as usize] = k;
        }
        assert!(
            position.iter().all(|&p| p != usize::MAX),
            "every cell is ordered"
        );

        for i in 0..heights.len() {
            let r = d.receiver[i] as usize;
            if r != i {
                assert!(position[r] < position[i], "receivers come first");
                assert!(d.filled[r] < d.filled[i]);
            }
            // Follow receivers: must end at an outlet within n steps.
            let mut cell = i;
            for _ in 0..heights.len() {
                let next = d.receiver[cell] as usize;
                if next == cell {
                    break;
                }
                cell = next;
            }
            assert_eq!(d.receiver[cell] as usize, cell);
        }

        // The bowl is filled, not a sink: its center drains.
        let center = (w / 2 + h / 2 * w) as usize;
        assert!(d.filled[center] > heights[center] + 5.0);
        // Area is conserved: outlets receive every cell exactly once.
        let area = d.accumulate(None);
        let total: f32 = (0..heights.len())
            .filter(|&i| d.receiver[i] as usize == i)
            .map(|i| area[i])
            .sum();
        assert_eq!(total, heights.len() as f32);
    }

    #[test]
    fn key_orders_floats() {
        let mut values = [3.5f32, -1.0, 0.0, -7.25, 2.0];
        values.sort_by_key(|v| key(*v));
        assert_eq!(values, [-7.25, -1.0, 0.0, 2.0, 3.5]);
    }
}
