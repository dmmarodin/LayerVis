//! Base generation from the atlas (milestone A4): world-scope nodes that turn
//! coarse atlas results into column detail.
//!
//! - `terrain_detail`: bounded fractal detail on a (bicubic-sampled) atlas
//!   elevation, scaled by biome roughness and faded near sea level, so
//!   coasts and valleys only move a bounded amount.
//! - `river_carve`: reads the atlas river lines (spatial index per atlas
//!   cell) and carves channels and valleys sized by discharge.
//! - `scatter`: deterministic jittered points (trees, …) from
//!   `hash(seed, kind, cell)`, accepted by a density; a column is a point or
//!   not, so no neighborhood stage is needed.
//!
//! All three are pure functions of (x, y) once built.

use std::{borrow::Cow, sync::Arc};

use crate::{
    atlas::RiverNetwork,
    pipeline::{BoxedWorldStep, Build, FieldSpec, InputSpec, NodeKind, ParamSpec, WorldStep},
    world::{fbm, hash, smoothstep},
    FieldScope, ValueKind, WORLD_SLOTS,
};

const fn world_out(
    name: &'static str,
    unit: &'static str,
    max: f32,
    ramp: &'static str,
    display_from: Option<&'static str>,
) -> FieldSpec {
    FieldSpec {
        name: Cow::Borrowed(name),
        scope: FieldScope::World,
        kind: ValueKind::Scalar,
        unit: Cow::Borrowed(unit),
        min: 0.0,
        max,
        ramp: Cow::Borrowed(ramp),
        display_from: match display_from {
            Some(name) => Some(Cow::Borrowed(name)),
            None => None,
        },
    }
}

const fn world_in(name: &'static str, optional: bool) -> InputSpec {
    InputSpec {
        name: Cow::Borrowed(name),
        scope: FieldScope::World,
        kind: ValueKind::Scalar,
        optional,
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

pub static TERRAIN_DETAIL: NodeKind = NodeKind {
    id: "terrain_detail",
    label: "Terrain detail",
    description: "Adds fractal detail to a smooth elevation, at most `amplitude` cells up \
                  or down, scaled by `roughness` (e.g. the biome's) and faded out near sea \
                  level so coastlines move only a little. Use it on a bicubic `sample` of \
                  the atlas elevation to hide the atlas cell grid.",
    inputs: &[world_in("elevation", false), world_in("roughness", true)],
    outputs: &[world_out(
        "elevation",
        "cells",
        128.0,
        "terrain",
        Some("elevation"),
    )],
    params: &[
        param("amplitude", 4.0, 0.0, 32.0),
        param("wavelength", 48.0, 4.0, 2048.0),
        param("octaves", 4.0, 1.0, 6.0),
        // Height band (cells) around sea level where detail fades out.
        param("coast_band", 3.0, 0.0, 20.0),
    ],
    build: Build::World(|bindings, params, context| {
        Ok(Box::new(DetailStep {
            elevation: bindings.input("elevation")?,
            roughness: bindings.optional_input("roughness"),
            out: bindings.output("elevation")?,
            seed: context.seed ^ 0xDE7A_1100,
            amplitude: params.get("amplitude"),
            wavelength: params.get("wavelength"),
            octaves: params.get("octaves").round() as u32,
            coast_band: params.get("coast_band"),
            sea: f64::from(context.depth) * 0.3,
            top: f64::from(context.depth) - 1.0,
        }) as BoxedWorldStep)
    }),
};

struct DetailStep {
    elevation: usize,
    roughness: Option<usize>,
    out: usize,
    seed: u64,
    amplitude: f64,
    wavelength: f64,
    octaves: u32,
    coast_band: f64,
    sea: f64,
    top: f64,
}

impl WorldStep for DetailStep {
    fn run(&self, x: i64, y: i64, slots: &mut [f64; WORLD_SLOTS]) {
        let e = slots[self.elevation];
        let rough = self
            .roughness
            .map_or(1.0, |slot| 0.15 + 0.85 * slots[slot].clamp(0.0, 1.0));
        let coast = if self.coast_band > 0.0 {
            smoothstep(0.0, self.coast_band, (e - self.sea).abs())
        } else {
            1.0
        };
        let noise = f64::from(fbm(
            self.seed,
            x as f64,
            y as f64,
            self.wavelength,
            self.octaves,
        )) * 2.0
            - 1.0;
        slots[self.out] = (e + noise * self.amplitude * rough * coast).clamp(0.0, self.top);
    }
}

// ---------------------------------------------------------------------------

pub static RIVER_CARVE: NodeKind = NodeKind {
    id: "river_carve",
    label: "River carve",
    description: "Carves the atlas river lines into the terrain: a channel whose width and \
                  depth grow with discharge, and banks that slope down into a valley. \
                  Outputs the carved elevation, the water depth in the channel and the \
                  water surface (the ground elsewhere). Reads the rivers straight from the \
                  atlas.",
    inputs: &[
        world_in("elevation", false),
        InputSpec {
            name: Cow::Borrowed("rivers"),
            scope: FieldScope::Atlas,
            kind: ValueKind::Rivers,
            optional: false,
        },
    ],
    outputs: &[
        world_out("elevation", "cells", 128.0, "terrain", Some("elevation")),
        world_out("river_depth", "cells", 6.0, "thermal", None),
        world_out("water_level", "cells", 128.0, "terrain", None),
    ],
    params: &[
        // Channel half-width in columns at 100 cells of discharge (grows with √q).
        param("width", 1.5, 0.1, 20.0),
        // Channel depth in cells at 100 cells of discharge (grows with q^0.3).
        param("depth", 1.2, 0.0, 10.0),
        param("max_width", 24.0, 1.0, 200.0),
        // Width of the valley sides beyond the channel, in columns.
        param("valley", 24.0, 0.0, 200.0),
    ],
    build: Build::WorldAtlas(|bindings, params, _, atlas| {
        let slot = bindings.input("rivers")?;
        let rivers = atlas
            .river_network(slot)
            .map(|n| Arc::new(n.clone()))
            .ok_or("the atlas has no rivers in this input's slot")?;
        Ok(Box::new(CarveStep {
            elevation: bindings.input("elevation")?,
            rivers,
            out_elevation: bindings.output("elevation")?,
            out_depth: bindings.output("river_depth")?,
            out_level: bindings.output("water_level")?,
            width: params.get("width") as f32,
            depth: params.get("depth") as f32,
            max_width: params.get("max_width") as f32,
            valley: params.get("valley") as f32,
        }) as BoxedWorldStep)
    }),
};

struct CarveStep {
    elevation: usize,
    rivers: Arc<RiverNetwork>,
    out_elevation: usize,
    out_depth: usize,
    out_level: usize,
    width: f32,
    depth: f32,
    max_width: f32,
    valley: f32,
}

impl CarveStep {
    /// (carved elevation, water depth, water surface).
    fn carve(&self, x: i64, y: i64, ground: f32) -> (f32, f32, f32) {
        let reach = f64::from(self.max_width + self.valley);
        let Some(hit) = self.rivers.nearest(x as f64 + 0.5, y as f64 + 0.5, reach) else {
            return (ground, 0.0, ground);
        };
        let q = (hit.discharge / 100.0).max(0.0);
        let half = (self.width * q.sqrt()).clamp(0.75, self.max_width);
        let depth = (self.depth * q.powf(0.3)).clamp(0.3, 8.0);
        let level = hit.level;
        let d = hit.distance;

        if d < half {
            let t = d / half;
            let bed = level - depth * (1.0 - t * t);
            let carved = ground.min(bed);
            let water = (level - carved).max(0.0);
            (carved, water, if water > 0.0 { level } else { carved })
        } else if d < half + self.valley {
            // Banks slope from just above the water up to the original ground.
            let bank = level + 0.5;
            let t = smoothstep(0.0, 1.0, f64::from((d - half) / self.valley.max(1e-3))) as f32;
            let carved = if ground > bank {
                ground.min(bank + (ground - bank) * t)
            } else {
                ground
            };
            (carved, 0.0, carved)
        } else {
            (ground, 0.0, ground)
        }
    }
}

impl WorldStep for CarveStep {
    fn run(&self, x: i64, y: i64, slots: &mut [f64; WORLD_SLOTS]) {
        let (e, depth, level) = self.carve(x, y, slots[self.elevation] as f32);
        slots[self.out_elevation] = f64::from(e);
        slots[self.out_depth] = f64::from(depth);
        slots[self.out_level] = f64::from(level);
    }
}

// ---------------------------------------------------------------------------

pub static SCATTER: NodeKind = NodeKind {
    id: "scatter",
    label: "Scatter",
    description: "Deterministic scattered points (trees, boulders, …): one candidate per \
                  `spacing`² block at a jittered column from hash(seed, kind, block), kept \
                  with probability `density` (e.g. the biome's tree density). `points` is 1 \
                  on a kept point's column; `instance` is a per-point random value (0 … 1) \
                  for picking a species or variant. `kind` separates feature kinds.",
    inputs: &[world_in("density", false)],
    outputs: &[
        world_out("points", "", 1.0, "grayscale", None),
        world_out("instance", "", 1.0, "categorical", None),
    ],
    params: &[
        param("spacing", 5.0, 1.0, 256.0),
        param("jitter", 1.0, 0.0, 1.0),
        param("kind", 0.0, 0.0, 10000.0),
    ],
    build: Build::World(|bindings, params, context| {
        Ok(Box::new(ScatterStep {
            density: bindings.input("density")?,
            out_points: bindings.output("points")?,
            out_instance: bindings.output("instance")?,
            seed: context.seed ^ (params.get("kind") as u64).wrapping_mul(0x5CA7_7E11),
            spacing: params.get("spacing").round().max(1.0) as i64,
            jitter: params.get("jitter"),
        }) as BoxedWorldStep)
    }),
};

struct ScatterStep {
    density: usize,
    out_points: usize,
    out_instance: usize,
    seed: u64,
    spacing: i64,
    jitter: f64,
}

/// A uniform value in [0, 1) from a hash.
fn unit(h: u64) -> f64 {
    (h >> 11) as f64 / (1u64 << 53) as f64
}

impl ScatterStep {
    /// The candidate column of a block and its accept / instance values.
    fn candidate(&self, bx: i64, by: i64) -> ((i64, i64), f64, f64) {
        let s = self.spacing;
        let h = hash(self.seed, bx, by);
        let centre = (s - 1) as f64 / 2.0;
        let offset = |u: f64| ((centre + (u * s as f64 - centre) * self.jitter).floor() as i64)
            .clamp(0, s - 1);
        let ox = offset(unit(h));
        let oy = offset(unit(hash(h, 1, 0)));
        let accept = unit(hash(h, 2, 0));
        let instance = unit(hash(h, 3, 0));
        ((bx * s + ox, by * s + oy), accept, instance)
    }
}

impl WorldStep for ScatterStep {
    fn run(&self, x: i64, y: i64, slots: &mut [f64; WORLD_SLOTS]) {
        let (bx, by) = (x.div_euclid(self.spacing), y.div_euclid(self.spacing));
        let (point, accept, instance) = self.candidate(bx, by);
        let kept = point == (x, y) && accept < slots[self.density].clamp(0.0, 1.0);
        slots[self.out_points] = if kept { 1.0 } else { 0.0 };
        slots[self.out_instance] = if kept { instance } else { 0.0 };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::atlas::{AtlasContext, RiverPath};

    #[test]
    fn scatter_has_one_candidate_per_block_and_follows_density() {
        let step = ScatterStep {
            density: 0,
            out_points: 1,
            out_instance: 2,
            seed: 9,
            spacing: 5,
            jitter: 1.0,
        };
        let count = |density: f64| {
            let mut n = 0;
            for y in -50..50 {
                for x in -50..50 {
                    let mut slots = [0.0; WORLD_SLOTS];
                    slots[0] = density;
                    step.run(x, y, &mut slots);
                    n += slots[1] as usize;
                }
            }
            n
        };
        assert_eq!(count(1.0), 400, "every 5x5 block has exactly one point");
        assert_eq!(count(0.0), 0);
        let half = count(0.5);
        assert!((150..250).contains(&half), "{half}");
    }

    #[test]
    fn carving_cuts_a_channel_below_the_water_level() {
        let context = AtlasContext::new(1, (0, 0), (256, 256), 128, 16);
        let rivers = Arc::new(RiverNetwork::new(
            "n1",
            vec![RiverPath {
                points: vec![(0.0, 128.0), (256.0, 128.0)],
                discharge: vec![400.0, 400.0],
                level: vec![50.0, 50.0],
            }],
            context,
        ));
        let step = CarveStep {
            elevation: 0,
            rivers,
            out_elevation: 1,
            out_depth: 2,
            out_level: 3,
            width: 1.5,
            depth: 1.2,
            max_width: 24.0,
            valley: 24.0,
        };
        let (bed, depth, level) = step.carve(100, 128, 55.0);
        assert!(bed < 50.0 && depth > 0.0 && level == 50.0);
        let (bank, depth, _) = step.carve(100, 140, 60.0);
        assert!(bank < 60.0 && bank > 50.0 && depth == 0.0);
        assert_eq!(step.carve(100, 250, 60.0), (60.0, 0.0, 60.0));
    }
}
