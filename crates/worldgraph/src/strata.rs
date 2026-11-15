//! Rock layers: the `strata` atlas node, `strata_fill` (strata → voxel
//! material ids) and `export_material`.
//!
//! A [`StrataGrid`] holds, per atlas cell, a fixed stack of
//! [`STRATA_LAYERS`] layers from the bottom up, each a material id and the
//! absolute elevation (cells) of its top. A layer spans from the previous
//! layer's top (exclusive) to its own top; unused layers repeat the last
//! layer with zero thickness. Above the top layer is open air (or water).

use std::borrow::Cow;

use layergen::{ChunkStep, DataSet, PipelineStepResult, StepContext};

use crate::{
    atlas::{AtlasIo, AtlasRun, AtlasStage, BoxedAtlasStage},
    materials::MaterialTable,
    pipeline::{BoxedStep, Build, FieldSpec, InputSpec, NodeKind, ParamSpec},
    world::{fbm, smoothstep, WorldStore},
    ColumnData, FieldScope, ValueKind, Voxel,
};

/// Layers per cell (bottom up).
pub const STRATA_LAYERS: usize = 8;

#[derive(Clone, Debug, PartialEq)]
pub struct StrataGrid {
    width: u32,
    height: u32,
    /// `cell * STRATA_LAYERS + layer`.
    materials: Vec<u16>,
    tops: Vec<f32>,
}

impl StrataGrid {
    /// Every cell one layer of `material` up to `top`.
    pub fn uniform(width: u32, height: u32, material: u16, top: f32) -> Self {
        let n = width as usize * height as usize * STRATA_LAYERS;
        Self {
            width,
            height,
            materials: vec![material; n],
            tops: vec![top; n],
        }
    }

    pub fn from_parts(
        width: u32,
        height: u32,
        materials: Vec<u16>,
        tops: Vec<f32>,
    ) -> Result<Self, String> {
        let n = width as usize * height as usize * STRATA_LAYERS;
        if materials.len() != n || tops.len() != n {
            return Err(format!("strata of {width}x{height} need {n} layers"));
        }
        Ok(Self {
            width,
            height,
            materials,
            tops,
        })
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }

    pub fn materials(&self) -> &[u16] {
        &self.materials
    }

    pub fn tops(&self) -> &[f32] {
        &self.tops
    }

    pub fn index(&self, x: u32, y: u32) -> usize {
        x as usize + y as usize * self.width as usize
    }

    /// Materials and tops of one cell, bottom up.
    pub fn layers(&self, cell: usize) -> (&[u16], &[f32]) {
        let range = cell * STRATA_LAYERS..(cell + 1) * STRATA_LAYERS;
        (&self.materials[range.clone()], &self.tops[range])
    }

    /// Replaces a cell's layers, bottom up (at most `STRATA_LAYERS`; tops are
    /// made non-decreasing; missing layers repeat the last with no thickness).
    pub fn set_layers(&mut self, cell: usize, layers: &[(u16, f32)]) {
        let base = cell * STRATA_LAYERS;
        let mut last = layers.first().copied().unwrap_or((0, 0.0));
        for k in 0..STRATA_LAYERS {
            if let Some(&(material, top)) = layers.get(k) {
                last = (material, top.max(if k == 0 { top } else { last.1 }));
            }
            self.materials[base + k] = last.0;
            self.tops[base + k] = last.1;
        }
    }

    /// The material at an elevation (the top layer's above the stack).
    pub fn material_at(&self, cell: usize, elevation: f32) -> u16 {
        let (materials, tops) = self.layers(cell);
        layer_at(materials, tops, elevation)
    }

    /// Elevation of the top of the stack.
    pub fn surface(&self, cell: usize) -> f32 {
        self.tops[cell * STRATA_LAYERS + STRATA_LAYERS - 1]
    }
}

/// The material of a layer stack at an elevation.
pub fn layer_at(materials: &[u16], tops: &[f32], elevation: f32) -> u16 {
    for k in 0..materials.len() {
        if elevation <= tops[k] {
            return materials[k];
        }
    }
    materials[materials.len() - 1]
}

const fn atlas_in(name: &'static str) -> InputSpec {
    InputSpec {
        name: Cow::Borrowed(name),
        scope: FieldScope::Atlas,
        kind: ValueKind::Scalar,
        optional: false,
    }
}

pub(crate) const fn field(
    name: &'static str,
    scope: FieldScope,
    kind: ValueKind,
    unit: &'static str,
    max: f32,
    ramp: &'static str,
) -> FieldSpec {
    FieldSpec {
        name: Cow::Borrowed(name),
        scope,
        kind,
        unit: Cow::Borrowed(unit),
        min: 0.0,
        max,
        ramp: Cow::Borrowed(ramp),
        display_from: None,
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
// strata (atlas)

pub static STRATA: NodeKind = NodeKind {
    id: "strata",
    label: "Strata",
    description: "Rock layers per atlas cell from geology: a granite (continental) or \
                  basalt (oceanic) basement, metamorphic rock (gneiss, slate, marble) near \
                  converging plate boundaries, sedimentary rock in low or flooded areas \
                  (limestone where warm and shallow, shale under sandstone elsewhere), and \
                  soil, sand or clay on top. Feeds erosion (hard rock wears slower) and \
                  strata_fill (voxel materials).",
    inputs: &[
        atlas_in("continental"),
        atlas_in("boundary_distance"),
        atlas_in("convergence"),
        atlas_in("elevation"),
    ],
    outputs: &[
        field(
            "strata",
            FieldScope::Atlas,
            ValueKind::Strata,
            "",
            1.0,
            "grayscale",
        ),
        field(
            "surface_material",
            FieldScope::Atlas,
            ValueKind::Scalar,
            "",
            16.0,
            "material",
        ),
        field(
            "erodibility",
            FieldScope::Atlas,
            ValueKind::Scalar,
            "×K",
            3.0,
            "thermal",
        ),
    ],
    params: &[
        // Soil thickness on gentle land, in cells.
        param("soil", 2.0, 0.0, 10.0),
        // Thickest sedimentary rock (in low, flooded areas), in cells.
        param("sediment", 14.0, 0.0, 60.0),
        // Thickest metamorphic band (at a head-on collision), in cells.
        param("metamorphic", 18.0, 0.0, 60.0),
        // Distance from a plate boundary (columns) over which rock is metamorphosed.
        param("boundary_width", 300.0, 20.0, 3000.0),
        // How much layer thickness varies (0 = uniform).
        param("variation", 0.5, 0.0, 1.0),
    ],
    build: Build::Atlas(|bindings, params, _| {
        Ok(Box::new(StrataStage {
            continental: bindings.input("continental")?,
            boundary_distance: bindings.input("boundary_distance")?,
            convergence: bindings.input("convergence")?,
            elevation: bindings.input("elevation")?,
            out_strata: bindings.output("strata")?,
            out_surface: bindings.output("surface_material")?,
            out_erodibility: bindings.output("erodibility")?,
            soil: params.get("soil") as f32,
            sediment: params.get("sediment") as f32,
            metamorphic: params.get("metamorphic") as f32,
            boundary_width: params.get("boundary_width") as f32,
            variation: params.get("variation") as f32,
        }) as BoxedAtlasStage)
    }),
};

struct StrataStage {
    continental: usize,
    boundary_distance: usize,
    convergence: usize,
    elevation: usize,
    out_strata: usize,
    out_surface: usize,
    out_erodibility: usize,
    soil: f32,
    sediment: f32,
    metamorphic: f32,
    boundary_width: f32,
    variation: f32,
}

/// Material ids the geology nodes use, looked up by name once.
pub(crate) struct Rocks {
    pub granite: u16,
    pub basalt: u16,
    pub gneiss: u16,
    pub slate: u16,
    pub marble: u16,
    pub sandstone: u16,
    pub shale: u16,
    pub limestone: u16,
    pub soil: u16,
    pub sand: u16,
    pub gravel: u16,
    pub clay: u16,
    pub water: u16,
    pub air: u16,
}

impl Rocks {
    pub fn new(table: &MaterialTable) -> Result<Self, String> {
        Ok(Self {
            granite: table.require("granite")?,
            basalt: table.require("basalt")?,
            gneiss: table.require("gneiss")?,
            slate: table.require("slate")?,
            marble: table.require("marble")?,
            sandstone: table.require("sandstone")?,
            shale: table.require("shale")?,
            limestone: table.require("limestone")?,
            soil: table.require("soil")?,
            sand: table.require("sand")?,
            gravel: table.require("gravel")?,
            clay: table.require("clay")?,
            water: table.require("water")?,
            air: table.require("air")?,
        })
    }
}

impl AtlasStage for StrataStage {
    fn run(&self, io: &mut AtlasIo<'_>, run: &mut AtlasRun<'_>) -> Result<(), String> {
        let context = *io.context();
        let (width, height) = context.grid;
        let sea = context.sea_level();
        let top = context.depth as f32;
        let rocks = Rocks::new(io.materials())?;
        let continental = io.input(self.continental)?.values().to_vec();
        let boundary = io.input(self.boundary_distance)?.values().to_vec();
        let convergence = io.input(self.convergence)?.values().to_vec();
        let elevation = io.input(self.elevation)?.values().to_vec();
        let mut strata = StrataGrid::uniform(width, height, rocks.granite, 0.0);
        let mut surface = vec![0.0f32; elevation.len()];
        let mut erodibility = vec![0.0f32; elevation.len()];
        let thickness_seed = context.seed ^ 0x57A7_0001;
        let choice_seed = context.seed ^ 0x57A7_0002;

        for cy in 0..height {
            run.check()?;
            let latitude = ((f64::from(cy) + 0.5) / f64::from(height) * 2.0 - 1.0).abs() as f32;
            for cx in 0..width {
                let i = (cx + cy * width) as usize;
                let (x, y) = context.world_point(f64::from(cx), f64::from(cy));
                let (x, y) = (f64::from(x), f64::from(y));
                let n1 = fbm(thickness_seed, x, y, 600.0, 3);
                let n2 = fbm(choice_seed, x, y, 900.0, 2);
                let vary = |base: f32, noise: f32| {
                    base * (1.0 + self.variation * (noise * 2.0 - 1.0))
                };
                let h = elevation[i];

                let basement = if continental[i] >= 0.5 {
                    rocks.granite
                } else {
                    rocks.basalt
                };

                // Metamorphic rock where plates collide.
                let squeeze = (convergence[i].max(0.0)
                    * (-boundary[i] / self.boundary_width).exp())
                .clamp(0.0, 1.0);
                let meta = if squeeze > 0.1 {
                    vary(self.metamorphic * squeeze, n1)
                } else {
                    0.0
                };
                let meta_material = if squeeze > 0.5 {
                    rocks.gneiss
                } else if n2 < 0.5 {
                    rocks.slate
                } else {
                    rocks.marble
                };

                // Sedimentary rock where the ground is low or under water.
                let low = smoothstep(f64::from(sea + 30.0), f64::from(sea - 5.0), f64::from(h))
                    as f32;
                let sediment = vary(self.sediment * low, n2);
                let warm = latitude < 0.4;
                let shallow = h > sea - 20.0;

                // Surface cover.
                let mountain = h > sea + 0.55 * (top - sea);
                let (cover_material, cover) = if h < sea {
                    if h > sea - 10.0 {
                        (rocks.sand, 1.5)
                    } else {
                        (rocks.clay, 1.5)
                    }
                } else if h < sea + 2.0 {
                    (rocks.sand, vary(1.5, n1))
                } else if mountain {
                    (rocks.gravel, 0.0)
                } else {
                    (rocks.soil, vary(self.soil, n1))
                };

                // Stack bottom up; thicknesses are taken from the surface down.
                let mut layers: Vec<(u16, f32)> = Vec::with_capacity(6);
                let mut level = h - cover - sediment - meta;
                layers.push((basement, level));
                if meta > 0.0 {
                    level += meta;
                    layers.push((meta_material, level));
                }
                if sediment > 0.5 {
                    if warm && shallow {
                        level += sediment;
                        layers.push((rocks.limestone, level));
                    } else {
                        level += sediment * 0.6;
                        layers.push((rocks.shale, level));
                        level += sediment * 0.4;
                        layers.push((rocks.sandstone, level));
                    }
                }
                if cover > 0.0 {
                    layers.push((cover_material, h));
                } else if let Some(last) = layers.last_mut() {
                    last.1 = h;
                }

                strata.set_layers(i, &layers);
                let material = strata.material_at(i, h);
                surface[i] = f32::from(material);
                erodibility[i] = io.materials().erodibility(material);
            }
            run.progress(f64::from(cy + 1) / f64::from(height));
        }

        io.write_strata(self.out_strata, strata)?;
        io.write_values(self.out_surface, surface)?;
        io.write_values(self.out_erodibility, erodibility)?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// strata_fill (voxel)

pub static STRATA_FILL: NodeKind = NodeKind {
    id: "strata_fill",
    label: "Strata fill",
    description: "Material id per voxel: rock layers from the strata below the column \
                  elevation (layer boundaries interpolated between atlas cells, with noise), \
                  water below sea level or the optional water level, air above. Reads the \
                  strata straight from the atlas.",
    inputs: &[
        InputSpec {
            name: Cow::Borrowed("elevation"),
            scope: FieldScope::Column,
            kind: ValueKind::Scalar,
            optional: false,
        },
        InputSpec {
            name: Cow::Borrowed("strata"),
            scope: FieldScope::Atlas,
            kind: ValueKind::Strata,
            optional: false,
        },
        InputSpec {
            name: Cow::Borrowed("water_level"),
            scope: FieldScope::Column,
            kind: ValueKind::Scalar,
            optional: true,
        },
    ],
    outputs: &[field(
        "material",
        FieldScope::Voxel,
        ValueKind::Material,
        "",
        16.0,
        "material",
    )],
    params: &[
        // Up/down noise on layer boundaries, in cells.
        param("boundary_noise", 1.5, 0.0, 10.0),
        // Wavelength of that noise, in columns.
        param("wavelength", 24.0, 2.0, 512.0),
    ],
    build: Build::Chunk(|bindings, params| {
        Ok(Box::new(StrataFillStep {
            elevation: bindings.input("elevation")?,
            strata: bindings.input("strata")?,
            water_level: bindings.optional_input("water_level"),
            out: bindings.output("material")?,
            noise: params.get("boundary_noise") as f32,
            wavelength: params.get("wavelength"),
        }) as BoxedStep)
    }),
};

struct StrataFillStep {
    elevation: usize,
    strata: usize,
    water_level: Option<usize>,
    out: usize,
    noise: f32,
    wavelength: f64,
}

impl ChunkStep<Voxel, ColumnData, WorldStore> for StrataFillStep {
    fn run(
        &self,
        cells: &mut DataSet<Voxel>,
        chunk: &mut ColumnData,
        context: &StepContext,
        world: &WorldStore,
    ) -> PipelineStepResult {
        let atlas = world.atlas();
        let strata = atlas
            .strata(self.strata)
            .ok_or("strata_fill: the atlas has no strata in its input slot")?;
        let rocks = Rocks::new(atlas.materials())?;
        let ac = *atlas.context();
        let origin = context.origin();
        let (w, h, _) = context.dimensions();
        let sea = world.sea_level();
        let seed = world.seed() ^ 0x57A7_F111;

        // Per column: layer materials (nearest cell, jittered) and tops
        // (bilinear between cells, plus boundary noise).
        let mut columns: Vec<([u16; STRATA_LAYERS], [f32; STRATA_LAYERS])> =
            Vec::with_capacity(w * h);
        for y in 0..h {
            for x in 0..w {
                let (gx, gy) = (origin.0 + x as i64, origin.1 + y as i64);
                let (u, v) = ac.cell_coords(gx, gy);
                let (u0, v0) = (u.floor(), v.floor());
                let (tx, ty) = ((u - u0) as f32, (v - v0) as f32);
                let clamp_x = |c: f64| c.clamp(0.0, f64::from(ac.grid.0 - 1)) as u32;
                let clamp_y = |c: f64| c.clamp(0.0, f64::from(ac.grid.1 - 1)) as u32;
                let corners = [
                    strata.index(clamp_x(u0), clamp_y(v0)),
                    strata.index(clamp_x(u0 + 1.0), clamp_y(v0)),
                    strata.index(clamp_x(u0), clamp_y(v0 + 1.0)),
                    strata.index(clamp_x(u0 + 1.0), clamp_y(v0 + 1.0)),
                ];
                let jitter = |s: u64| {
                    (fbm(seed ^ s, gx as f64, gy as f64, self.wavelength, 2) - 0.5)
                        * ac.cell_size as f32
                };
                let (jx, jy) = (jitter(1), jitter(2));
                let (nx, ny) = ac.cell_of(gx + jx as i64, gy + jy as i64);
                let (materials, _) = strata.layers(strata.index(nx, ny));
                let mut column = ([0u16; STRATA_LAYERS], [0f32; STRATA_LAYERS]);
                let mut below = f32::MIN;
                for k in 0..STRATA_LAYERS {
                    let t = |c: usize| strata.layers(corners[c]).1[k];
                    let top = (t(0) * (1.0 - tx) + t(1) * tx) * (1.0 - ty)
                        + (t(2) * (1.0 - tx) + t(3) * tx) * ty;
                    let wobble = if k + 1 < STRATA_LAYERS && self.noise > 0.0 {
                        (fbm(seed ^ (k as u64 + 7), gx as f64, gy as f64, self.wavelength, 2)
                            - 0.5)
                            * 2.0
                            * self.noise
                    } else {
                        0.0
                    };
                    below = (top + wobble).max(below);
                    column.0[k] = materials[k];
                    column.1[k] = below;
                }
                columns.push(column);
            }
        }

        let origin_z = origin.2;
        for (voxel, x, y, z) in cells.iter_mut() {
            let col = &chunk.columns[(x, y, 0)];
            let ground = col[self.elevation];
            let water = self.water_level.map_or(sea, |slot| col[slot].max(sea));
            let height = (origin_z + z as i64) as f32;
            voxel.materials[self.out] = if height < ground {
                let (materials, tops) = &columns[x + y * w];
                layer_at(materials, tops, height)
            } else if height < water {
                rocks.water
            } else {
                rocks.air
            };
        }

        Ok(())
    }
}

// ---------------------------------------------------------------------------

/// Named voxel material ids for game code. The node's label is the export name.
pub static EXPORT_MATERIAL: NodeKind = NodeKind {
    id: "export_material",
    label: "Export (material)",
    description: "Publishes a voxel material id under this node's name for game code \
                  (`voxel.materials[export.slot]`, an id in the world's material table).",
    inputs: &[InputSpec {
        name: Cow::Borrowed("value"),
        scope: FieldScope::Voxel,
        kind: ValueKind::Material,
        optional: false,
    }],
    outputs: &[],
    params: &[],
    build: Build::Export,
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layers_are_found_bottom_up_and_padded() {
        let mut grid = StrataGrid::uniform(2, 1, 0, 0.0);
        grid.set_layers(1, &[(3, 10.0), (5, 25.0), (7, 30.0)]);
        assert_eq!(grid.material_at(1, -100.0), 3);
        assert_eq!(grid.material_at(1, 10.0), 3);
        assert_eq!(grid.material_at(1, 10.5), 5);
        assert_eq!(grid.material_at(1, 29.0), 7);
        assert_eq!(grid.material_at(1, 99.0), 7, "above the stack: top layer");
        assert_eq!(grid.surface(1), 30.0);
        // Tops never decrease.
        grid.set_layers(0, &[(1, 10.0), (2, 4.0)]);
        assert_eq!(grid.layers(0).1[1], 10.0);
    }
}
