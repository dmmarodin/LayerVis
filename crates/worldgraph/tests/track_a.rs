//! Track A: value kinds, optional inputs, strata and materials, climate,
//! biomes, base generation from the atlas and geological epochs.

use std::sync::Arc;

use layergen::{ChunkCoord, ChunkSize, PipelineConfig, WorldBounds};
use worldgraph::{
    can_connect, steps, Atlas, FieldScope, GraphDoc, GraphPipeline, NodeStatus, SocketType,
    ValueKind, WorldStore,
};

fn config() -> PipelineConfig {
    PipelineConfig::new(
        WorldBounds::new((-1024, -1024, 0), (2048, 2048, 128)).unwrap(),
        ChunkSize::new(32, 32, 32).unwrap(),
        99,
    )
}

/// The atlas template with short simulations (debug-build speed).
fn quick(mut doc: GraphDoc) -> GraphDoc {
    for node in doc.nodes.iter_mut() {
        let (name, value) = match node.kind.as_str() {
            "erosion" => ("iterations", 12.0),
            "geology" => ("iterations", 6.0),
            "moisture" => ("passes", 60.0),
            _ => continue,
        };
        node.params.insert(name.into(), value);
    }
    doc
}

fn build(doc: &GraphDoc) -> (Arc<WorldStore>, GraphPipeline) {
    let world = Arc::new(WorldStore::for_doc(doc, &config()).unwrap());
    let graph = GraphPipeline::build(doc, config(), &world).unwrap();
    for node in &graph.analysis().nodes {
        assert_eq!(
            node.status,
            NodeStatus::Active,
            "{} ({}): {:?}",
            node.id,
            node.kind,
            node.reason
        );
    }
    (world, graph)
}

#[test]
fn kinds_must_match_to_connect() {
    let t = SocketType::new;
    use FieldScope::*;
    use ValueKind::*;
    assert!(can_connect(t(Voxel, Material), t(Voxel, Material)));
    assert!(!can_connect(t(Voxel, Material), t(Voxel, Scalar)));
    assert!(!can_connect(t(Voxel, Scalar), t(Voxel, Material)));
    assert!(can_connect(t(Atlas, Strata), t(Atlas, Strata)));
    assert!(!can_connect(t(Atlas, Strata), t(Atlas, Scalar)));
    assert!(!can_connect(t(Atlas, Rivers), t(Atlas, Strata)));
    // World scalars still feed column scalars (implicit sampling).
    assert!(can_connect(t(World, Scalar), t(Column, Scalar)));
}

#[test]
fn optional_inputs_may_stay_unconnected_and_mistyped_wires_are_invalid() {
    let mut doc = quick(steps::eroded_graph());
    // Erosion and flow without rain or strata still run.
    doc.edges.retain(|e| {
        !(["n6", "n7"].contains(&e.to.node.as_str())
            && ["strata", "precipitation"].contains(&e.to.socket.as_str()))
    });
    let (_, graph) = build(&doc);
    let erosion = graph
        .analysis()
        .nodes
        .iter()
        .find(|n| n.id == "n6")
        .unwrap();
    assert!(erosion.inputs.iter().any(|i| i.name == "strata" && i.optional));

    // A material wired into a scalar export is not a valid edge.
    let mut doc = quick(steps::eroded_graph());
    doc.edges.push(worldgraph::EdgeDoc {
        from: worldgraph::SocketRef {
            node: "n32".into(),
            socket: "material".into(),
        },
        to: worldgraph::SocketRef {
            node: "n11".into(),
            socket: "elevation".into(),
        },
    });
    let world = Arc::new(WorldStore::for_doc(&doc, &config()).unwrap());
    let graph = GraphPipeline::build(&doc, config(), &world).unwrap();
    let edge = graph
        .analysis()
        .edges
        .iter()
        .find(|e| e.from.node == "n32" && e.to.node == "n11")
        .unwrap();
    assert!(!edge.valid);
}

#[test]
fn atlas_template_is_deterministic_and_complete() {
    let doc = quick(steps::eroded_graph());
    let (world, graph) = build(&doc);
    let again = WorldStore::for_doc(&doc, &config()).unwrap();
    let mut a = Vec::new();
    let mut b = Vec::new();
    world.atlas().write(&mut a).unwrap();
    again.atlas().write(&mut b).unwrap();
    assert!(a == b, "two builds give the same atlas bytes");

    // Exports for the game, with their kinds.
    let export = |name: &str| graph.export(name).unwrap_or_else(|| panic!("{name}"));
    for name in ["height", "biome", "surface_material", "river_depth", "tree"] {
        assert_eq!(export(name).kind, ValueKind::Scalar, "{name}");
    }
    assert_eq!(export("material").kind, ValueKind::Material);
    assert_eq!(export("material").scope, FieldScope::Voxel);
}

#[test]
fn strata_give_varied_rock_and_erosion_wears_soft_rock_faster() {
    let doc = quick(steps::eroded_graph());
    let (world, graph) = build(&doc);
    let atlas = world.atlas();
    let schema = graph.schema();
    let strata_slot = schema
        .fields_of(SocketType::new(FieldScope::Atlas, ValueKind::Strata))
        .into_iter()
        .find(|f| f.key == "n16.strata")
        .unwrap()
        .slot;
    let strata = atlas.strata(strata_slot).unwrap();
    let materials = world.materials();

    // Rock below the surface: continental basement is granite somewhere.
    let granite = materials.id("granite").unwrap();
    let basalt = materials.id("basalt").unwrap();
    let cells = (atlas.context().grid.0 * atlas.context().grid.1) as usize;
    let bottoms: Vec<u16> = (0..cells).map(|i| strata.layers(i).0[0]).collect();
    assert!(bottoms.contains(&granite) && bottoms.contains(&basalt));

    // Every layer stack is ordered bottom up and topped at the surface.
    let elevation = atlas.grid(schema.slot("n4.out").unwrap()).unwrap();
    for i in 0..cells {
        let tops = strata.layers(i).1;
        assert!(tops.windows(2).all(|t| t[1] >= t[0]));
        assert!((strata.surface(i) - elevation.values()[i]).abs() < 1e-3);
    }

    // Erosion with strata differs from erosion of uniform rock.
    let mut plain = doc.clone();
    plain
        .edges
        .retain(|e| !(e.to.node == "n6" && e.to.socket == "strata"));
    let plain_world = WorldStore::for_doc(&plain, &config()).unwrap();
    let key = schema.slot("n6.elevation").unwrap();
    assert_ne!(
        atlas.grid(key).unwrap().values(),
        plain_world.atlas().grid(key).unwrap().values()
    );

    // The eroded surface shows several materials.
    let surface = atlas.grid(schema.slot("n6.surface_material").unwrap()).unwrap();
    let mut seen: Vec<u16> = surface.values().iter().map(|&v| v as u16).collect();
    seen.sort_unstable();
    seen.dedup();
    assert!(seen.len() >= 3, "{seen:?}");
}

#[test]
fn voxels_get_rock_water_and_air_materials() {
    let doc = quick(steps::eroded_graph());
    let (world, graph) = build(&doc);
    let height = graph.export("height").unwrap().slot;
    let material = graph.export("material").unwrap().slot;
    let table = world.materials();
    let air = table.id("air").unwrap();
    let water = table.id("water").unwrap();
    let sea = world.sea_level();

    let (mut rock, mut wet, mut open) = (0, 0, 0);
    for coord in [ChunkCoord(32, 32, 1), ChunkCoord(10, 30, 1), ChunkCoord(5, 5, 1)] {
        let chunk = graph.generate(coord, &world).unwrap();
        let origin_z = coord.2 as f32 * 32.0;
        for (voxel, x, y, z) in chunk.data().iter() {
            let ground = chunk.chunk_data().columns[(x, y, 0)][height];
            let h = origin_z + z as f32;
            let id = voxel.materials[material];
            if h < ground {
                assert!(table.get(id).unwrap().is_solid(), "solid below ground");
                rock += 1;
            } else if h < sea {
                assert_eq!(id, water);
                wet += 1;
            } else if id == air {
                open += 1;
            }
        }
    }
    assert!(rock > 0 && wet > 0 && open > 0, "{rock} {wet} {open}");
}

#[test]
fn climate_and_biomes_are_plausible() {
    let doc = quick(steps::eroded_graph());
    let (world, graph) = build(&doc);
    let atlas = world.atlas();
    let grid = |key: &str| atlas.grid(graph.schema().slot(key).unwrap()).unwrap();
    let sea = world.sea_level();
    let elevation = grid("n6.elevation").values();
    let biome = grid("n20.biome").values();
    let table = world.biomes();

    let summer = grid("n19.temperature_summer").values();
    let winter = grid("n19.temperature_winter").values();
    assert!(summer.iter().zip(winter).all(|(s, w)| s >= w));
    assert!(grid("n18.precipitation").values().iter().all(|p| *p >= 0.0));

    for (i, &e) in elevation.iter().enumerate() {
        let name = &table.get(biome[i] as u16).unwrap().name;
        let water = ["ocean", "deep_ocean", "frozen_ocean"].contains(&name.as_str());
        assert_eq!(water, e < sea, "cell {i}: {name} at {e}");
    }
}

#[test]
fn saved_atlas_keeps_strata_rivers_and_id_tables() {
    let doc = quick(steps::eroded_graph());
    let (world, graph) = build(&doc);
    let mut bytes = Vec::new();
    world.atlas().write(&mut bytes).unwrap();
    let saved = Atlas::read(&mut bytes.as_slice()).unwrap();
    let loaded = Arc::new(WorldStore::from_atlas(&doc, &config(), &saved).unwrap());
    assert_eq!(loaded.materials().names(), world.materials().names());
    assert_eq!(loaded.biomes().names(), world.biomes().names());

    let coord = ChunkCoord(32, 32, 1);
    let a = graph.generate(coord, &world).unwrap();
    let b = GraphPipeline::build(&doc, config(), &loaded)
        .unwrap()
        .generate(coord, &loaded)
        .unwrap();
    assert_eq!(a.data(), b.data(), "voxel materials survive a save");
    assert_eq!(a.chunk_data(), b.chunk_data());

    // Version 1 files are refused with a clear message.
    let mut old = bytes.clone();
    old[8..12].copy_from_slice(&1u32.to_le_bytes());
    assert!(Atlas::read(&mut old.as_slice())
        .err()
        .unwrap()
        .contains("version 1"));
}

#[test]
fn rivers_carve_and_trees_scatter() {
    let doc = quick(steps::eroded_graph());
    let (world, graph) = build(&doc);
    let schema = graph.schema();
    let depth = schema.slot("n25.river_depth").unwrap();
    let carved = schema.slot("n25.elevation").unwrap();
    let before = schema.slot("n24.elevation").unwrap();
    let points = schema.slot("n28.points").unwrap();

    // Along every river line: water in the channel, ground cut down.
    let rivers = world.atlas().rivers();
    let path = rivers
        .iter()
        .flat_map(|r| r.paths.iter())
        .find(|p| p.points.len() > 4)
        .expect("the template traces rivers");
    let (x, y) = path.points[path.points.len() / 2];
    let v = world.graph().eval(x as i64, y as i64);
    assert!(v[depth] > 0.0, "water in the channel");
    assert!(v[carved] <= v[before]);

    let mut trees = 0;
    for y in (-512..512).step_by(3) {
        for x in -512..512 {
            trees += world.graph().eval(x, y)[points] as usize;
        }
    }
    assert!(trees > 0, "some trees on the continent");
}

#[test]
fn geological_epochs_template_runs_deterministically() {
    let doc = quick(steps::epochs_graph());
    let (world, graph) = build(&doc);
    let again = WorldStore::for_doc(&doc, &config()).unwrap();
    let key = graph.schema().slot("n6.elevation").unwrap();
    assert_eq!(
        world.atlas().grid(key).unwrap().values(),
        again.atlas().grid(key).unwrap().values()
    );
    let glaciated = world
        .atlas()
        .grid(graph.schema().slot("n6.glaciated").unwrap())
        .unwrap();
    assert!(glaciated.values().iter().any(|&g| g > 0.0), "cold epochs grow ice");
    let epochs = world
        .atlas()
        .snapshots()
        .iter()
        .filter(|s| s.node == "n6" && s.label.starts_with("epoch"))
        .count();
    assert_eq!(epochs, 4);
}
