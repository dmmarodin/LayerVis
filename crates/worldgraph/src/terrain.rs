//! World-scope terrain nodes: tectonics → tectonic elevation → edge falloff.
//!
//! All three are pure functions of the world position. The plate layout is
//! built once when the world is created (`Build::World`) and is read-only
//! afterwards. Values flow between the nodes through world slots (f64), so
//! the default chain is numerically identical to the original hardcoded world
//! model.

use std::borrow::Cow;

use crate::{
    pipeline::{BoxedWorldStep, Build, FieldSpec, InputSpec, NodeKind, ParamSpec, WorldStep},
    tectonics::{Tectonics, TectonicsConfig},
    world::{fbm, smoothstep, MAX_PLATES},
    FieldScope, ValueKind, WORLD_SLOTS,
};

const fn world_out(
    name: &'static str,
    unit: &'static str,
    min: f32,
    max: f32,
    ramp: &'static str,
    display_from: Option<&'static str>,
) -> FieldSpec {
    FieldSpec {
        name: Cow::Borrowed(name),
        scope: FieldScope::World,
        kind: ValueKind::Scalar,
        unit: Cow::Borrowed(unit),
        min,
        max,
        ramp: Cow::Borrowed(ramp),
        display_from: match display_from {
            Some(input) => Some(Cow::Borrowed(input)),
            None => None,
        },
    }
}

const fn world_in(name: &'static str) -> InputSpec {
    InputSpec {
        name: Cow::Borrowed(name),
        scope: FieldScope::World,
        kind: ValueKind::Scalar,
        optional: false,
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

/// Plates on a jittered grid with warped Voronoi boundaries.
pub static TECTONICS: NodeKind = NodeKind {
    id: "tectonics",
    label: "Tectonics",
    description: "Tectonic plates: which plate each point belongs to, distance to the \
                  plate boundary and how the plates move against each other there.",
    inputs: &[],
    outputs: &[
        world_out("plate", "id", 0.0, MAX_PLATES as f32, "categorical", None),
        world_out("boundary_distance", "cells", 0.0, 400.0, "grayscale", None),
        world_out("convergence", "", -1.5, 1.5, "diverging", None),
        world_out("continental", "", 0.0, 1.0, "grayscale", None),
        world_out("neighbor_continental", "", 0.0, 1.0, "grayscale", None),
    ],
    params: &[
        // Target plate diameter; the plate count follows from the world area.
        param("plate_size", 1500.0, 400.0, 4096.0),
        // 0 = every plate is land by an independent coin flip (scattered land);
        // N >= 1 = land plates are clustered around N continent cores.
        param("continents", 0.0, 0.0, 4.0),
        param("continental_fraction", 0.4, 0.05, 0.9),
        // Plate boundary irregularity, in plate sizes.
        param("warp", 0.35, 0.0, 0.8),
    ],
    build: Build::World(|bindings, params, context| {
        let size = params.get("plate_size");
        let area = f64::from(context.size.0) * f64::from(context.size.1);
        let plate_count = ((area / (size * size)).round() as usize).clamp(2, MAX_PLATES);

        Ok(Box::new(TectonicsStep {
            tectonics: Tectonics::generate(
                context.seed,
                context.origin,
                context.size,
                &TectonicsConfig {
                    plate_count,
                    continents: params.get("continents").round() as usize,
                    continental_fraction: params.get("continental_fraction"),
                    warp: params.get("warp"),
                },
            ),
            plate: bindings.output("plate")?,
            boundary_distance: bindings.output("boundary_distance")?,
            convergence: bindings.output("convergence")?,
            continental: bindings.output("continental")?,
            neighbor_continental: bindings.output("neighbor_continental")?,
        }) as BoxedWorldStep)
    }),
};

struct TectonicsStep {
    tectonics: Tectonics,
    plate: usize,
    boundary_distance: usize,
    convergence: usize,
    continental: usize,
    neighbor_continental: usize,
}

impl WorldStep for TectonicsStep {
    fn run(&self, x: i64, y: i64, slots: &mut [f64; WORLD_SLOTS]) {
        let sample = self.tectonics.sample(x, y);
        let plates = self.tectonics.plates();
        let flag = |continental: bool| if continental { 1.0 } else { 0.0 };

        slots[self.plate] = sample.plate as f64;
        slots[self.boundary_distance] = sample.boundary_distance;
        slots[self.convergence] = sample.convergence;
        slots[self.continental] = flag(plates[sample.plate].continental);
        slots[self.neighbor_continental] = flag(plates[sample.neighbor].continental);
    }
}

// ---------------------------------------------------------------------------

/// Terrain height from plate type and boundary motion.
pub static TECTONIC_ELEVATION: NodeKind = NodeKind {
    id: "tectonic_elevation",
    label: "Tectonic elevation",
    description: "Terrain height in cells: continental plates stand high, oceanic plates \
                  low; colliding plates raise mountains, rifts and ridges shape the sea \
                  floor; fractal detail on top.",
    inputs: &[
        world_in("boundary_distance"),
        world_in("convergence"),
        world_in("continental"),
        world_in("neighbor_continental"),
    ],
    outputs: &[world_out("elevation", "cells", 0.0, 128.0, "terrain", None)],
    params: &[
        // Plate base heights, as a fraction of the world depth (sea level is 0.3).
        param("land_height", 0.55, 0.0, 1.0),
        param("ocean_height", 0.22, 0.0, 1.0),
        // Scales the mountains raised where plates collide.
        param("mountains", 1.0, 0.0, 3.0),
        // Amplitude of the fractal detail.
        param("detail", 0.12, 0.0, 0.5),
    ],
    build: Build::World(|bindings, params, context| {
        Ok(Box::new(TectonicElevationStep {
            boundary_distance: bindings.input("boundary_distance")?,
            convergence: bindings.input("convergence")?,
            continental: bindings.input("continental")?,
            neighbor_continental: bindings.input("neighbor_continental")?,
            out: bindings.output("elevation")?,
            seed: context.seed,
            depth: f64::from(context.depth),
            land_height: params.get("land_height"),
            ocean_height: params.get("ocean_height"),
            mountains: params.get("mountains"),
            detail: params.get("detail"),
        }) as BoxedWorldStep)
    }),
};

struct TectonicElevationStep {
    boundary_distance: usize,
    convergence: usize,
    continental: usize,
    neighbor_continental: usize,
    out: usize,
    seed: u64,
    depth: f64,
    land_height: f64,
    ocean_height: f64,
    mountains: f64,
    detail: f64,
}

impl WorldStep for TectonicElevationStep {
    fn run(&self, x: i64, y: i64, slots: &mut [f64; WORLD_SLOTS]) {
        let distance = slots[self.boundary_distance];
        let convergence = slots[self.convergence];
        let own_continental = slots[self.continental] >= 0.5;
        let neighbor_continental = slots[self.neighbor_continental] >= 0.5;

        let base = |continental: bool| {
            if continental {
                self.land_height
            } else {
                self.ocean_height
            }
        };

        // Blend plate base heights near the boundary to avoid cliffs.
        let own = base(own_continental);
        let blend = smoothstep(0.0, 180.0, distance);
        let mut height = (own + base(neighbor_continental)) / 2.0 * (1.0 - blend) + own * blend;

        // Boundary effects fall off with distance from the boundary.
        let falloff = (-distance / 160.0).exp();
        let both_continental = own_continental && neighbor_continental;
        let any_continental = own_continental || neighbor_continental;

        if convergence > 0.0 {
            let strength = if both_continental {
                0.32
            } else if any_continental {
                0.22
            } else {
                0.10
            };
            height += convergence * strength * self.mountains * falloff;
        } else if own_continental {
            height += convergence * 0.10 * falloff; // continental rift
        } else {
            height -= convergence * 0.05 * falloff; // mid-ocean ridge
        }

        let detail = f64::from(fbm(self.seed, x as f64, y as f64, 96.0, 4)) - 0.5;
        height += detail * self.detail;

        slots[self.out] = (height * self.depth).clamp(0.0, self.depth - 1.0);
    }
}

// ---------------------------------------------------------------------------

/// The "vignette": pulls terrain down to the sea floor near the world edges.
pub static EDGE_FALLOFF: NodeKind = NodeKind {
    id: "edge_falloff",
    label: "Edge falloff",
    description: "Filter that pulls elevation down to the sea floor near the world edges, \
                  so the map is ocean all round. The first `margin` is always ocean; \
                  terrain fades back in over `falloff`.",
    inputs: &[world_in("elevation")],
    outputs: &[world_out(
        "elevation",
        "cells",
        0.0,
        128.0,
        "terrain",
        Some("elevation"),
    )],
    params: &[
        // Always-ocean strip along every edge (fraction of the smaller world side).
        param("margin", 0.08, 0.0, 0.3),
        // Width over which terrain fades in after the margin.
        param("falloff", 0.14, 0.0, 0.4),
        // Makes the coast irregular; only ever pushes it inward, so the margin holds.
        param("noise", 0.05, 0.0, 0.15),
        // Sea floor height at the edges (fraction of the world depth).
        param("sea_floor", 0.12, 0.0, 0.28),
    ],
    build: Build::World(|bindings, params, context| {
        Ok(Box::new(EdgeFalloffStep {
            input: bindings.input("elevation")?,
            out: bindings.output("elevation")?,
            seed: context.seed,
            origin: context.origin,
            size: (i64::from(context.size.0), i64::from(context.size.1)),
            depth: f64::from(context.depth),
            margin: params.get("margin"),
            falloff: params.get("falloff"),
            noise: params.get("noise"),
            sea_floor: params.get("sea_floor"),
        }) as BoxedWorldStep)
    }),
};

struct EdgeFalloffStep {
    input: usize,
    out: usize,
    seed: u64,
    origin: (i64, i64),
    size: (i64, i64),
    depth: f64,
    margin: f64,
    falloff: f64,
    noise: f64,
    sea_floor: f64,
}

impl WorldStep for EdgeFalloffStep {
    fn run(&self, x: i64, y: i64, slots: &mut [f64; WORLD_SLOTS]) {
        let elevation = slots[self.input];
        let scale = self.size.0.min(self.size.1) as f64;

        let edge_distance = (x - self.origin.0)
            .min(self.origin.0 + self.size.0 - 1 - x)
            .min(y - self.origin.1)
            .min(self.origin.1 + self.size.1 - 1 - y) as f64;

        let wobble = f64::from(fbm(self.seed ^ 0xED6E, x as f64, y as f64, scale * 0.06, 3));
        let distance = edge_distance - self.noise * scale * wobble;

        let start = self.margin * scale;
        let mask = smoothstep(start, start + (self.falloff * scale).max(1.0), distance);
        let floor = self.sea_floor * self.depth;

        slots[self.out] = (floor + (elevation - floor) * mask).clamp(0.0, self.depth - 1.0);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use layergen::{ChunkCoord, ChunkSize, PipelineConfig, WorldBounds};

    use super::*;
    use crate::{
        graph::GraphDoc,
        pipeline::{analyze, resolve, ResolvedNode},
        steps::{default_graph, GraphTemplate, TEMPLATES},
        world::{WorldStats, WorldStore},
        GraphPipeline,
    };

    fn config(seed: u64) -> PipelineConfig {
        PipelineConfig::new(
            WorldBounds::new((-4096, -4096, 0), (8192, 8192, 128)).unwrap(),
            ChunkSize::new(32, 32, 32).unwrap(),
            seed,
        )
    }

    /// World slot of the (last) `elevation` field of a doc's world nodes.
    fn elevation_slot(doc: &GraphDoc) -> usize {
        let resolved: Vec<ResolvedNode> = doc.nodes.iter().map(resolve).collect();
        analyze(doc, &resolved)
            .schema
            .world_fields()
            .iter()
            .rev()
            .find(|field| field.name == "elevation")
            .expect("the doc has world elevation")
            .slot
    }

    /// The original hardcoded world model (before world nodes), for regression.
    fn legacy_elevation(tectonics: &Tectonics, seed: u64, x: i64, y: i64) -> f64 {
        let plates = tectonics.plates();
        let sample = tectonics.sample(x, y);
        let depth = 128.0;

        let base = |index: usize| {
            if plates[index].continental {
                0.55
            } else {
                0.22
            }
        };
        let own = base(sample.plate);
        let blend = smoothstep(0.0, 180.0, sample.boundary_distance);
        let mut height = (own + base(sample.neighbor)) / 2.0 * (1.0 - blend) + own * blend;

        let falloff = (-sample.boundary_distance / 160.0).exp();
        let conv = sample.convergence;
        let both = plates[sample.plate].continental && plates[sample.neighbor].continental;
        let any = plates[sample.plate].continental || plates[sample.neighbor].continental;

        if conv > 0.0 {
            let strength = if both {
                0.32
            } else if any {
                0.22
            } else {
                0.10
            };
            height += conv * strength * falloff;
        } else if plates[sample.plate].continental {
            height += conv * 0.10 * falloff;
        } else {
            height -= conv * 0.05 * falloff;
        }

        height += (f64::from(fbm(seed, x as f64, y as f64, 96.0, 4)) - 0.5) * 0.12;
        (height * depth).clamp(0.0, depth - 1.0)
    }

    fn legacy_tectonics(seed: u64) -> Tectonics {
        Tectonics::generate(
            seed,
            (-4096, -4096),
            (8192, 8192),
            &TectonicsConfig::default(),
        )
    }

    #[test]
    fn the_default_graph_reproduces_the_original_world_exactly() {
        let seed = 12345;
        let doc = default_graph();
        let world = WorldStore::for_doc(&doc, &config(seed)).unwrap();
        let slot = elevation_slot(&doc);
        let reference = legacy_tectonics(seed);
        assert_eq!(reference.plates().len(), 30);

        for i in 0..400i64 {
            let (x, y) = (-4096 + (i * 2791) % 8192, -4096 + (i * 6113) % 8192);
            assert_eq!(
                world.graph().eval(x, y)[slot],
                legacy_elevation(&reference, seed, x, y),
                "({x}, {y})"
            );
        }
    }

    #[test]
    fn old_files_load_into_the_same_world_through_the_migration() {
        // A format-1 file as the editor used to write it.
        let old = r#"{"format": 1, "seed": 12345,
            "nodes": [
              {"id": "n1", "kind": "elevation"}, {"id": "n2", "kind": "temperature"},
              {"id": "n3", "kind": "solid"}, {"id": "n4", "kind": "export_column", "label": "height"}],
            "edges": [
              {"from": {"node": "n1", "socket": "elevation"}, "to": {"node": "n2", "socket": "elevation"}},
              {"from": {"node": "n1", "socket": "elevation"}, "to": {"node": "n3", "socket": "elevation"}},
              {"from": {"node": "n1", "socket": "elevation"}, "to": {"node": "n4", "socket": "value"}}]}"#;
        let doc = GraphDoc::from_json(old).unwrap();

        let config = config(12345);
        let world = Arc::new(WorldStore::for_doc(&doc, &config).unwrap());
        let graph = GraphPipeline::build(&doc, config, &world).unwrap();
        assert!(graph.analysis().nodes.iter().all(|n| n.reason.is_none()));

        let height = graph.export("height").unwrap().slot;
        let reference = legacy_tectonics(12345);
        let chunk = graph.generate(ChunkCoord(130, 120, 0), &world).unwrap();
        let origin = chunk.origin();

        for (column, x, y, _) in chunk.chunk_data().columns.iter() {
            let expected =
                legacy_elevation(&reference, 12345, origin.0 + x as i64, origin.1 + y as i64);
            assert_eq!(column[height], expected as f32);
        }
    }

    /// Land statistics of a template world for a seed.
    fn template_stats(template: &GraphTemplate, seed: u64) -> WorldStats {
        let doc = (template.build)();
        let slot = elevation_slot(&doc);
        WorldStore::for_doc(&doc, &config(seed))
            .unwrap()
            .stats(slot, 96)
    }

    #[test]
    fn continent_templates_leave_ocean_on_all_four_edges() {
        for template in TEMPLATES.iter().filter(|t| t.id != "default") {
            let mut doc = (template.build)();
            // Atlas templates simulate erosion: keep it short in debug tests
            // (the edge falloff runs after it either way).
            let atlas = WorldStore::has_atlas_work(&doc);
            for node in doc.nodes.iter_mut() {
                match node.kind.as_str() {
                    "erosion" => {
                        node.params.insert("iterations".into(), 4.0);
                    }
                    "geology" => {
                        node.params.insert("iterations".into(), 2.0);
                        node.params.insert("epochs".into(), 2.0);
                    }
                    "moisture" => {
                        node.params.insert("passes".into(), 10.0);
                    }
                    _ => {}
                }
            }
            let slot = elevation_slot(&doc);
            let falloff = doc.nodes.iter().find(|n| n.kind == "edge_falloff").unwrap();
            let margin = falloff.params["margin"] * 8192.0;

            for seed in 1..=(if atlas { 1 } else { 6 }) {
                let world = WorldStore::for_doc(&doc, &config(seed)).unwrap();
                let sea = f64::from(world.sea_level());
                let (lo, hi) = (-4096, 4095);
                let inset = (margin * 0.99) as i64;

                for step in 0..=512 {
                    let t = lo + (step * (hi - lo)) / 512; // includes both corners
                    for (x, y) in [
                        (t, lo),
                        (t, hi),
                        (lo, t),
                        (hi, t),
                        (t, lo + inset),
                        (t, hi - inset),
                        (lo + inset, t),
                        (hi - inset, t),
                    ] {
                        assert!(
                            world.graph().eval(x, y)[slot] < sea,
                            "{} seed {seed}: land at ({x}, {y})",
                            template.id
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn templates_make_the_advertised_number_of_landmasses() {
        let template = |id: &str| TEMPLATES.iter().find(|t| t.id == id).unwrap();

        // The default world scatters land all the way to the border.
        assert!(template_stats(template("default"), 1).edge_land > 0.1);

        for seed in 1..=20 {
            let single = template_stats(template("single_continent"), seed);
            assert_eq!(single.landmasses, 1, "seed {seed}");
            assert_eq!(single.edge_land, 0.0);
            assert!(
                (0.12..0.45).contains(&single.land_fraction),
                "seed {seed}: {}",
                single.land_fraction
            );

            assert!(
                template_stats(template("archipelago"), seed).landmasses >= 3,
                "seed {seed}"
            );
            assert_eq!(
                template_stats(template("pangaea"), seed).landmasses,
                1,
                "seed {seed}"
            );
        }

        // Two continents; island arcs can occasionally join neighbors into one.
        let two: Vec<usize> = (1..=20)
            .map(|s| template_stats(template("two_continents"), s).landmasses)
            .collect();
        assert!(two.iter().all(|&n| (1..=3).contains(&n)), "{two:?}");
        assert!(two.iter().filter(|&&n| n == 2).count() >= 16, "{two:?}");
    }

    #[test]
    fn a_migrated_vignette_keeps_the_ocean_edge() {
        let old = r#"{"format": 1, "seed": 3,
            "world": {"plate_size": 1100, "continents": 1, "edge_margin": 0.08, "edge_falloff": 0.14, "edge_noise": 0.05},
            "nodes": [{"id": "n1", "kind": "elevation"}, {"id": "n2", "kind": "export_column", "label": "height"}],
            "edges": [{"from": {"node": "n1", "socket": "elevation"}, "to": {"node": "n2", "socket": "value"}}]}"#;
        let doc = GraphDoc::from_json(old).unwrap();
        let slot = elevation_slot(&doc);
        let stats = WorldStore::for_doc(&doc, &config(3))
            .unwrap()
            .stats(slot, 96);

        assert_eq!(stats.edge_land, 0.0);
        assert_eq!(stats.landmasses, 1);
    }
}
