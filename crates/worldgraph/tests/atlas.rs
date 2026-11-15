//! Atlas scope: adapters, the build (progress, cancel, determinism), the
//! erosion/flow simulations and saving/loading an atlas.

use std::{
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc,
    },
    time::Instant,
};

use layergen::{ChunkCoord, ChunkSize, PipelineConfig, WorldBounds};
use worldgraph::{
    steps, world_signature, Atlas, AtlasHooks, AtlasProgress, EdgeDoc, Extent, FieldScope,
    GraphDoc, GraphPipeline, NodeDoc, NodeStatus, SocketRef, WorldStore, ATLAS_CANCELLED,
};

fn config() -> PipelineConfig {
    PipelineConfig::new(
        WorldBounds::new((-1024, -1024, 0), (2048, 2048, 128)).unwrap(),
        ChunkSize::new(32, 32, 32).unwrap(),
        99,
    )
}

fn node(id: &str, kind: &str, params: &[(&str, f64)]) -> NodeDoc {
    NodeDoc {
        id: id.into(),
        kind: kind.into(),
        enabled: true,
        position: (0.0, 0.0),
        params: params.iter().map(|(k, v)| ((*k).to_owned(), *v)).collect(),
        script: None,
        label: None,
    }
}

fn edge(from: &str, out: &str, to: &str, input: &str) -> EdgeDoc {
    EdgeDoc {
        from: SocketRef {
            node: from.into(),
            socket: out.into(),
        },
        to: SocketRef {
            node: to.into(),
            socket: input.into(),
        },
    }
}

fn doc(nodes: Vec<NodeDoc>, edges: Vec<EdgeDoc>) -> GraphDoc {
    GraphDoc {
        nodes,
        edges,
        frames: Vec::new(),
        ..steps::default_graph()
    }
}

/// noise → rasterize → sample (nearest) and → sample (bilinear).
fn adapter_graph() -> GraphDoc {
    doc(
        vec![
            node("n1", "world_noise", &[("wavelength", 300.0)]),
            node("n2", "rasterize", &[]),
            node("n3", "sample", &[("mode", 0.0)]),
            node("n4", "sample", &[("mode", 1.0)]),
        ],
        vec![
            edge("n1", "noise", "n2", "in"),
            edge("n2", "out", "n3", "in"),
            edge("n2", "out", "n4", "in"),
        ],
    )
}

/// The eroded template with fewer iterations (debug-build speed).
fn eroded(iterations: f64) -> GraphDoc {
    let mut doc = steps::eroded_graph();
    doc.node_mut("n6")
        .unwrap()
        .params
        .insert("iterations".into(), iterations);
    doc
}

fn status(graph: &GraphPipeline, id: &str) -> NodeStatus {
    graph
        .analysis()
        .nodes
        .iter()
        .find(|n| n.id == id)
        .unwrap()
        .status
}

fn slot_of(graph: &GraphPipeline, key: &str) -> usize {
    graph.schema().slot(key).unwrap()
}

#[test]
fn world_and_atlas_connect_only_through_adapters() {
    assert!(worldgraph::can_connect(
        FieldScope::Atlas,
        FieldScope::Atlas
    ));
    assert!(!worldgraph::can_connect(
        FieldScope::World,
        FieldScope::Atlas
    ));
    assert!(!worldgraph::can_connect(
        FieldScope::Atlas,
        FieldScope::World
    ));
    assert!(!worldgraph::can_connect(
        FieldScope::Atlas,
        FieldScope::Column
    ));

    // A world output wired straight into erosion is an invalid edge.
    let direct = doc(
        vec![node("n1", "world_noise", &[]), node("n2", "erosion", &[])],
        vec![
            edge("n1", "noise", "n2", "elevation"),
            edge("n1", "noise", "n2", "uplift"),
        ],
    );
    let config = config();
    let world = Arc::new(WorldStore::for_doc(&direct, &config).unwrap());
    let graph = GraphPipeline::build(&direct, config, &world).unwrap();
    assert!(graph.analysis().edges.iter().all(|e| !e.valid));
    assert_eq!(status(&graph, "n2"), NodeStatus::Blocked);

    let adapters = adapter_graph();
    let world = Arc::new(WorldStore::for_doc(&adapters, &config).unwrap());
    let graph = GraphPipeline::build(&adapters, config, &world).unwrap();
    let scope = |id: &str| {
        graph
            .analysis()
            .nodes
            .iter()
            .find(|n| n.id == id)
            .unwrap()
            .scope
    };
    assert_eq!(scope("n2"), FieldScope::Atlas);
    assert_eq!(scope("n3"), FieldScope::World);
    assert!(["n1", "n2", "n3", "n4"]
        .iter()
        .all(|id| status(&graph, id) == NodeStatus::Active));
    assert_eq!(graph.schema().atlas_fields().len(), 1);
}

#[test]
fn rasterize_and_sample_round_trip_world_values() {
    let config = config();
    let doc = adapter_graph();
    let world = Arc::new(WorldStore::for_doc(&doc, &config).unwrap());
    let graph = GraphPipeline::build(&doc, config, &world).unwrap();
    let atlas = world.atlas();
    let context = *atlas.context();
    assert_eq!(context.grid, (128, 128));

    let grid = atlas.grid(slot_of(&graph, "n2.out")).unwrap();
    let noise = slot_of(&graph, "n1.noise");
    let nearest = slot_of(&graph, "n3.out");
    let bilinear = slot_of(&graph, "n4.out");

    for (cx, cy) in [(0, 0), (17, 90), (127, 127), (64, 3)] {
        let (x, y) = context.cell_point(cx, cy, 0.5, 0.5);
        let values = world.graph().eval(x, y);
        assert_eq!(
            grid.get(cx, cy) as f64,
            values[noise] as f32 as f64,
            "cell ({cx}, {cy})"
        );
        assert_eq!(values[nearest], f64::from(grid.get(cx, cy)));
        assert!((values[bilinear] - f64::from(grid.get(cx, cy))).abs() < 0.02);
    }

    // Bilinear sampling is continuous across a cell border; nearest is not.
    let (x, y) = context.cell_point(40, 40, 0.0, 0.5);
    let left = world.graph().eval(x - 1, y);
    let right = world.graph().eval(x, y);
    assert!((left[bilinear] - right[bilinear]).abs() < 0.01);
}

#[test]
fn atlas_build_reports_progress_and_can_be_cancelled() {
    let config = config();
    let doc = eroded(6.0);
    let reports = std::sync::Mutex::new(Vec::<AtlasProgress>::new());
    let progress = |p: &AtlasProgress| reports.lock().unwrap().push(p.clone());

    WorldStore::for_doc_with(
        &doc,
        &config,
        AtlasHooks {
            progress: Some(&progress),
            cancelled: None,
        },
    )
    .unwrap();

    let reports = reports.into_inner().unwrap();
    // Two rasterize nodes, erosion and flow do atlas work.
    assert!(reports.iter().all(|p| p.total == 4));
    let nodes: Vec<&str> = reports.iter().map(|p| p.node.as_str()).collect();
    for id in ["n4", "n5", "n6", "n7"] {
        assert!(nodes.contains(&id), "{id} reported progress");
    }
    assert!(reports
        .iter()
        .any(|p| p.node == "n7" && p.fraction == 1.0 && p.index == 3));

    // Cancel once erosion has started.
    let started = AtomicBool::new(false);
    let polls = AtomicUsize::new(0);
    let watch = |p: &AtlasProgress| {
        if p.node == "n6" {
            started.store(true, Ordering::SeqCst);
        }
    };
    let cancelled = || {
        polls.fetch_add(1, Ordering::SeqCst);
        started.load(Ordering::SeqCst)
    };
    let error = WorldStore::for_doc_with(
        &doc,
        &config,
        AtlasHooks {
            progress: Some(&watch),
            cancelled: Some(&cancelled),
        },
    )
    .err()
    .unwrap();
    assert!(error.contains(ATLAS_CANCELLED), "{error}");
    assert!(polls.load(Ordering::SeqCst) > 0);
}

#[test]
fn erosion_and_flow_are_deterministic_and_drain_to_the_sea() {
    let config = config();
    let doc = eroded(40.0);
    let started = Instant::now();
    let world = Arc::new(WorldStore::for_doc(&doc, &config).unwrap());
    eprintln!("eroded 128² atlas, 40 iterations: {:?}", started.elapsed());
    let again = WorldStore::for_doc(&doc, &config).unwrap();
    let graph = GraphPipeline::build(&doc, config, &world).unwrap();
    assert!(
        graph.analysis().nodes.iter().all(|n| n.reason.is_none()),
        "every node runs"
    );

    let atlas = world.atlas();
    for field in graph.schema().atlas_fields() {
        let a = atlas.grid(field.slot).unwrap();
        let b = again.atlas().grid(field.slot).unwrap();
        assert!(
            a.values().iter().all(|v| v.is_finite()),
            "{} is finite",
            field.key
        );
        assert!(
            a.values()
                .iter()
                .zip(b.values())
                .all(|(x, y)| x.to_bits() == y.to_bits()),
            "{} is bit-identical across builds",
            field.key
        );
    }

    let sea = atlas.context().sea_level();
    let before = atlas.grid(slot_of(&graph, "n4.out")).unwrap();
    let after = atlas.grid(slot_of(&graph, "n6.elevation")).unwrap();
    let land = after.values().iter().filter(|&&h| h >= sea).count();
    assert!(
        land > 1000,
        "the continent survives erosion ({land} land cells)"
    );
    let changed = before
        .values()
        .iter()
        .zip(after.values())
        .filter(|(a, b)| (*a - *b).abs() > 0.5)
        .count();
    assert!(
        changed > land / 4,
        "erosion reshapes the land ({changed} cells changed)"
    );
    assert!(after.values().iter().all(|&h| (0.0..128.0).contains(&h)));

    // Rivers: discharge never decreases downstream, and each ends at the sea,
    // the atlas edge, or another river.
    let rivers = &atlas.rivers()[0];
    assert_eq!(rivers.0, "n7");
    assert!(!rivers.1.is_empty());
    let context = *atlas.context();
    let ends: Vec<(f32, f32)> = rivers.1.iter().flat_map(|r| r.points.clone()).collect();
    for river in &rivers.1 {
        assert!(river.discharge.windows(2).all(|q| q[1] >= q[0]));
        let &(x, y) = river.points.last().unwrap();
        let (cx, cy) = context.cell_of(x.floor() as i64, y.floor() as i64);
        let edge = cx == 0 || cy == 0 || cx == context.grid.0 - 1 || cy == context.grid.1 - 1;
        let ocean = after.get(cx, cy) < sea;
        let joins = ends.iter().filter(|&&p| p == (x, y)).count() > 1;
        assert!(edge || ocean || joins, "river ends at ({x}, {y})");
    }

    // Snapshots for the scrubber: erosion keeps a start frame plus steps.
    let frames = atlas.snapshots().iter().filter(|s| s.node == "n6").count();
    assert!(frames >= 10, "{frames} erosion frames");

    // The chunk pipeline reads the eroded height through sample + falloff.
    graph.validate(config, &world).unwrap();
}

#[test]
fn saved_atlas_rebuilds_the_same_world_without_running_atlas_nodes() {
    let config = config();
    let doc = eroded(10.0);
    let world = Arc::new(WorldStore::for_doc(&doc, &config).unwrap());
    let graph = GraphPipeline::build(&doc, config, &world).unwrap();

    let mut bytes = Vec::new();
    world.atlas().write(&mut bytes).unwrap();
    let saved = Atlas::read(&mut bytes.as_slice()).unwrap();

    // Loading must not run erosion: a cancel hook would stop it, but
    // from_atlas never polls one, and it is fast.
    let started = Instant::now();
    let loaded = Arc::new(WorldStore::from_atlas(&doc, &config, &saved).unwrap());
    eprintln!("world from a saved atlas: {:?}", started.elapsed());

    for (x, y) in [(0, 0), (-1000, 512), (777, -333), (1023, 1023)] {
        assert_eq!(
            world.graph().eval(x, y),
            loaded.graph().eval(x, y),
            "({x}, {y})"
        );
    }
    let coord = ChunkCoord(32, 32, 1);
    let a = graph.generate(coord, &world).unwrap();
    let b = GraphPipeline::build(&doc, config, &loaded)
        .unwrap()
        .generate(coord, &loaded)
        .unwrap();
    assert_eq!(a.chunk_data(), b.chunk_data());
    assert_eq!(a.data(), b.data());
    assert_eq!(loaded.atlas().rivers(), world.atlas().rivers());

    // A different graph refuses the saved atlas.
    let mut other = doc.clone();
    other
        .node_mut("n6")
        .unwrap()
        .params
        .insert("strength".into(), 0.3);
    assert!(WorldStore::from_atlas(&other, &config, &saved)
        .err()
        .unwrap()
        .contains("different"));
}

#[test]
fn extent_sets_the_atlas_grid_and_must_match_the_config() {
    let mut doc = adapter_graph();
    let plain_signature = world_signature(&doc);
    doc.extent = Some(Extent {
        size: (2048, 2048),
        depth: 128,
        cell_size: 32,
    });
    assert_ne!(
        world_signature(&doc),
        plain_signature,
        "the extent is part of the world"
    );

    let world = WorldStore::for_doc(&doc, &config()).unwrap();
    assert_eq!(world.atlas().context().grid, (64, 64));

    doc.extent = Some(Extent {
        size: (4096, 2048),
        ..doc.extent.unwrap()
    });
    assert!(WorldStore::for_doc(&doc, &config())
        .err()
        .unwrap()
        .contains("extent"));

    // A format 2 graph (no extent) keeps its old signature format.
    assert!(!plain_signature.contains("cellSize"));
}

/// Timing of the eroded template on LayerVis's default world (512² atlas).
/// Run with `cargo test --release --test atlas -- --ignored --nocapture`.
#[test]
#[ignore]
fn timing_default_world() {
    let config = PipelineConfig::new(
        WorldBounds::new((-4096, -4096, 0), (8192, 8192, 128)).unwrap(),
        ChunkSize::new(32, 32, 32).unwrap(),
        12345,
    );
    let doc = steps::eroded_graph();
    let last = std::sync::Mutex::new((String::new(), Instant::now()));
    let progress = |p: &AtlasProgress| {
        let mut last = last.lock().unwrap();
        if last.0 != p.node {
            eprintln!("{} started after {:?}", p.node, last.1.elapsed());
            *last = (p.node.clone(), Instant::now());
        }
    };
    let started = Instant::now();
    WorldStore::for_doc_with(
        &doc,
        &config,
        AtlasHooks {
            progress: Some(&progress),
            cancelled: None,
        },
    )
    .unwrap();
    eprintln!("total {:?}", started.elapsed());
}

#[test]
fn placeholder_worlds_build_without_running_atlas_nodes() {
    assert!(WorldStore::has_atlas_work(&steps::eroded_graph()));
    assert!(!WorldStore::has_atlas_work(&steps::default_graph()));

    let config = config();
    // Full iterations: a placeholder must not run them (it returns at once).
    let doc = steps::eroded_graph();
    let started = Instant::now();
    let world = Arc::new(WorldStore::placeholder(&doc, &config).unwrap());
    assert!(
        started.elapsed().as_millis() < 500,
        "{:?}",
        started.elapsed()
    );
    assert!(world.is_placeholder());

    let graph = GraphPipeline::build(&doc, config, &world).unwrap();
    assert!(graph.analysis().nodes.iter().all(|n| n.reason.is_none()));
    let elevation = world.atlas().grid(slot_of(&graph, "n6.elevation")).unwrap();
    assert!(elevation.values().iter().all(|&v| v == 0.0));
    assert!(world.atlas().rivers().is_empty());
    graph.validate(config, &world).unwrap();
}
