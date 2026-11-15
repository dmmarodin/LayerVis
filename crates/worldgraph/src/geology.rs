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
    pipeline::{BoxedWorldStep, Build, FieldSpec, InputSpec, NodeKind, ParamSpec, WorldStep},
    FieldScope, WORLD_SLOTS,
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
    }
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
struct Drainage {
    /// Depression-filled surface (with `epsilon` slope across flats).
    filled: Vec<f32>,
    order: Vec<u32>,
    /// Receiver cell; outlets are their own receiver.
    receiver: Vec<u32>,
    /// Distance to the receiver in cells (1 or √2; 0 for outlets).
    distance: Vec<f32>,
}

impl Drainage {
    fn compute(heights: &[f32], width: u32, height: u32, sea: f32, epsilon: f32) -> Self {
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
    fn accumulate(&self, weight: Option<&[f32]>) -> Vec<f32> {
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
                  level) and the atlas border are base level.",
    inputs: &[
        input("elevation", FieldScope::Atlas),
        input("uplift", FieldScope::Atlas),
    ],
    outputs: &[
        spec("elevation", FieldScope::Atlas, "cells", 128.0, "terrain"),
        spec("drainage", FieldScope::Atlas, "cells", 2000.0, "thermal"),
        spec("eroded", FieldScope::Atlas, "cells", 40.0, "thermal"),
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
            out_elevation: bindings.output("elevation")?,
            out_drainage: bindings.output("drainage")?,
            out_eroded: bindings.output("eroded")?,
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
    out_elevation: usize,
    out_drainage: usize,
    out_eroded: usize,
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

impl AtlasStage for ErosionStage {
    fn run(&self, io: &mut AtlasIo<'_>, run: &mut AtlasRun<'_>) -> Result<(), String> {
        let context = *io.context();
        let (width, height) = context.grid;
        let sea = context.sea_level();
        let top = context.depth as f32 - 1.0;
        let mut heights = io.input(self.elevation)?.values().to_vec();
        let uplift: Vec<f32> = io
            .input(self.uplift)?
            .values()
            .iter()
            .map(|u| u * self.uplift_scale)
            .collect();
        let mut eroded = vec![0.0f32; heights.len()];
        let mut area = vec![1.0f32; heights.len()];
        let dt = self.time_step;
        // Talus limit per cell step, in cells of height.
        let max_drop = self.talus * context.cell_size as f32;
        let frame_every = (self.iterations / EROSION_FRAMES).max(1);

        run.snapshot(
            "start",
            &AtlasGrid::from_values(width, height, heights.clone())?,
        );

        for iteration in 0..self.iterations {
            run.check()?;
            let drainage = Drainage::compute(&heights, width, height, sea, EPSILON);
            area = drainage.accumulate(None);

            // Implicit stream-power update, downstream first: a receiver's new
            // height is known when its donors are updated.
            for &i in &drainage.order {
                let i = i as usize;
                let r = drainage.receiver[i] as usize;
                if r == i {
                    continue; // base level
                }
                let before = heights[i];
                let raised = before + dt * uplift[i];
                let next = if raised <= heights[r] {
                    raised // in a filled depression: a lake bed, not eroded
                } else {
                    let f = self.strength * dt * area[i].powf(self.area_exponent)
                        / drainage.distance[i];
                    (raised + f * heights[r]) / (1.0 + f)
                };
                heights[i] = next.clamp(0.0, top);
                eroded[i] += raised - heights[i];
            }

            thermal(&mut heights, width, height, sea, max_drop);

            if (iteration + 1) % frame_every == 0 || iteration + 1 == self.iterations {
                run.snapshot(
                    format!("step {}", iteration + 1),
                    &AtlasGrid::from_values(width, height, heights.clone())?,
                );
            }
            run.progress(f64::from(iteration + 1) / f64::from(self.iterations));
        }

        io.write(
            self.out_elevation,
            AtlasGrid::from_values(width, height, heights)?,
        )?;
        io.write(
            self.out_drainage,
            AtlasGrid::from_values(width, height, area)?,
        )?;
        io.write(
            self.out_eroded,
            AtlasGrid::from_values(width, height, eroded)?,
        )?;
        Ok(())
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
    inputs: &[input("elevation", FieldScope::Atlas)],
    outputs: &[
        spec("water_level", FieldScope::Atlas, "cells", 128.0, "terrain"),
        spec("lake_depth", FieldScope::Atlas, "cells", 20.0, "thermal"),
        spec("discharge", FieldScope::Atlas, "cells", 2000.0, "thermal"),
        spec("river", FieldScope::Atlas, "", 1.0, "grayscale"),
    ],
    params: &[
        // Drainage area (cells) where a river starts.
        param("river_threshold", 400.0, 4.0, 1_000_000.0),
    ],
    build: Build::Atlas(|bindings, params, _| {
        Ok(Box::new(FlowStage {
            elevation: bindings.input("elevation")?,
            out_level: bindings.output("water_level")?,
            out_lake: bindings.output("lake_depth")?,
            out_discharge: bindings.output("discharge")?,
            out_river: bindings.output("river")?,
            threshold: params.get("river_threshold") as f32,
        }) as BoxedAtlasStage)
    }),
};

struct FlowStage {
    elevation: usize,
    out_level: usize,
    out_lake: usize,
    out_discharge: usize,
    out_river: usize,
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
        let discharge = drainage.accumulate(None);
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

        let rivers = trace_rivers(&drainage, &discharge, &heights, sea, threshold, &context);
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
        io.set_rivers(rivers);
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
        };
        let mut i = source;
        loop {
            path.points.push(point(i));
            path.discharge.push(discharge[i]);
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
