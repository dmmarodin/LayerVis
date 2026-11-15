//! World / chunk / voxel scopes: world nodes run once per world, chunk nodes
//! read them through implicit per-column sampling, and wires never run from a
//! lower scope up into a higher one.

use std::sync::Arc;

use layergen::{ChunkCoord, ChunkSize, PipelineConfig, WorldBounds};
use worldgraph::{
    SocketType,
    steps, EdgeDoc, FieldScope, GraphDoc, GraphPipeline, NodeDoc, NodeStatus, SocketRef, WorldStore,
};

fn config() -> PipelineConfig {
    PipelineConfig::new(
        WorldBounds::new((-1024, -1024, 0), (2048, 2048, 128)).unwrap(),
        ChunkSize::new(32, 32, 32).unwrap(),
        99,
    )
}

fn node(id: &str, kind: &str) -> NodeDoc {
    NodeDoc {
        id: id.into(),
        kind: kind.into(),
        enabled: true,
        position: (0.0, 0.0),
        params: Default::default(),
        script: None,
        label: None,
    }
}

fn named(id: &str, kind: &str, label: &str) -> NodeDoc {
    NodeDoc {
        label: Some(label.into()),
        ..node(id, kind)
    }
}

fn with_param(mut node: NodeDoc, name: &str, value: f64) -> NodeDoc {
    node.params.insert(name.into(), value);
    node
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
        ..steps::default_graph()
    }
}

fn build(doc: &GraphDoc) -> (Arc<WorldStore>, GraphPipeline) {
    let config = config();
    let world = Arc::new(WorldStore::for_doc(doc, &config).unwrap());
    let graph = GraphPipeline::build(doc, config, &world).unwrap();
    (world, graph)
}

fn report<'a>(graph: &'a GraphPipeline, id: &str) -> &'a worldgraph::NodeReport {
    graph.analysis().nodes.iter().find(|n| n.id == id).unwrap()
}

/// noise → remap (10..20) → solid.elevation, plus a column export of the remap.
fn sampled_graph() -> GraphDoc {
    doc(
        vec![
            node("n1", "world_noise"),
            with_param(
                with_param(node("n2", "world_remap"), "out_min", 10.0),
                "out_max",
                20.0,
            ),
            node("n3", "solid"),
            named("n4", "export_column", "remapped"),
        ],
        vec![
            edge("n1", "noise", "n2", "value"),
            edge("n2", "remapped", "n3", "elevation"),
            edge("n2", "remapped", "n4", "value"),
        ],
    )
}

#[test]
fn world_nodes_run_first_and_chunk_nodes_read_them_by_implicit_sampling() {
    let doc = sampled_graph();
    let (_, graph) = build(&doc);
    let analysis = graph.analysis();

    let ids: Vec<_> = analysis
        .order
        .iter()
        .map(|&i| doc.nodes[i].id.as_str())
        .collect();
    assert_eq!(&ids[..2], ["n1", "n2"], "world nodes first");

    assert_eq!(report(&graph, "n1").scope, FieldScope::World);
    assert_eq!(report(&graph, "n3").scope, FieldScope::Voxel);
    assert!(analysis
        .nodes
        .iter()
        .all(|n| n.status == NodeStatus::Active));

    // World outputs live in world slots, chunk outputs in column slots; the
    // sampled copy of n2.remapped takes the first column slot.
    let world_fields = analysis.schema.world_fields();
    let keys: Vec<_> = world_fields.iter().map(|f| f.key.as_str()).collect();
    assert_eq!(keys, ["n1.noise", "n2.remapped"]);
    assert_eq!((world_fields[0].slot, world_fields[1].slot), (0, 1));

    assert_eq!(analysis.world_samples.len(), 1);
    assert_eq!(analysis.world_samples[0].world_slot, 1);
    assert_eq!(analysis.world_samples[0].column_slot, 0);

    // Sampled copies are not fields: the chunk nodes here produce no column outputs.
    assert!(analysis.schema.column_fields().is_empty());
    assert_eq!(analysis.schema.slot("n3.solid"), Ok(0));
    assert_eq!(
        analysis.bindings[2].as_ref().unwrap().input("elevation"),
        Ok(0)
    );

    // A world output wired into an export is a column value for the game.
    let remapped = graph.export("remapped").unwrap();
    assert_eq!(remapped.scope, FieldScope::Column);
    assert_eq!(remapped.slot, 0);
    assert_eq!(remapped.field, "n2.remapped");

    // The wire from the world node is reported as a world value.
    let wire = analysis.edges.iter().find(|e| e.to.node == "n3").unwrap();
    assert!(wire.valid);
    assert_eq!(wire.scope, Some(FieldScope::World));
}

#[test]
fn sampled_values_are_the_world_graph_evaluated_at_each_column() {
    let doc = sampled_graph();
    let (world, graph) = build(&doc);
    let remapped = graph.export("remapped").unwrap().clone();
    let slot = graph.analysis().schema.world_fields()[1].slot;

    let chunk = graph.generate(ChunkCoord(33, 31, 0), &world).unwrap();
    let origin = chunk.origin();

    for (column, x, y, _) in chunk.chunk_data().columns.iter() {
        let at = world.graph().eval(origin.0 + x as i64, origin.1 + y as i64);

        assert_eq!(column[remapped.slot], at[slot] as f32);
        assert!((10.0..=20.0).contains(&column[remapped.slot]));
    }

    // The remap really maps the noise: 10 + 10 * noise.
    let (x, y) = (origin.0 + 5, origin.1 + 9);
    let at = world.graph().eval(x, y);
    assert!((at[1] - (10.0 + 10.0 * at[0])).abs() < 1e-4);
}

#[test]
fn wires_only_run_downward_through_the_scopes() {
    // Column output into a world input, and a world output into a voxel input.
    let bad = doc(
        vec![
            node("n1", "elevation"),
            node("n2", "world_remap"),
            node("n3", "world_noise"),
            node("n4", "export_voxel"),
        ],
        vec![
            edge("n1", "elevation", "n2", "value"),
            edge("n3", "noise", "n4", "value"),
        ],
    );
    let (_, graph) = build(&bad);

    assert!(graph.analysis().edges.iter().all(|e| !e.valid));
    assert_eq!(report(&graph, "n2").status, NodeStatus::Blocked);
    assert_eq!(report(&graph, "n4").status, NodeStatus::Blocked);

    assert!(worldgraph::can_connect(
        SocketType::scalar(FieldScope::World),
        SocketType::scalar(FieldScope::Column),
    ));
    assert!(worldgraph::can_connect(
        SocketType::scalar(FieldScope::World),
        SocketType::scalar(FieldScope::World),
    ));
    assert!(!worldgraph::can_connect(
        SocketType::scalar(FieldScope::Column),
        SocketType::scalar(FieldScope::World),
    ));
    assert!(!worldgraph::can_connect(
        SocketType::scalar(FieldScope::Voxel),
        SocketType::scalar(FieldScope::Column),
    ));
    assert!(!worldgraph::can_connect(
        SocketType::scalar(FieldScope::World),
        SocketType::scalar(FieldScope::Voxel),
    ));
}

#[test]
fn a_world_must_be_built_from_the_graph_it_serves() {
    let config = config();
    let doc = sampled_graph();

    let wrong = Arc::new(WorldStore::new(&config));
    assert!(GraphPipeline::build(&doc, config, &wrong)
        .err()
        .unwrap()
        .contains("world"));

    // Chunk-scope edits keep the world valid; world-scope edits do not.
    let world = Arc::new(WorldStore::for_doc(&doc, &config).unwrap());
    let mut chunk_edit = doc.clone();
    chunk_edit.nodes[3].label = Some("renamed".into());
    assert!(GraphPipeline::build(&chunk_edit, config, &world).is_ok());

    let mut world_edit = doc.clone();
    world_edit.nodes[0] = with_param(node("n1", "world_noise"), "wavelength", 64.0);
    assert!(GraphPipeline::build(&world_edit, config, &world).is_err());
}

#[test]
fn world_values_reach_the_neighborhood_stage_across_chunk_borders() {
    // noise (world) → smooth (neighborhood, chunk) → export.
    let doc = doc(
        vec![
            node("n1", "world_noise"),
            with_param(node("n2", "smooth"), "radius", 1.0),
            named("n3", "export_column", "smoothed"),
        ],
        vec![
            edge("n1", "noise", "n2", "value"),
            edge("n2", "smoothed", "n3", "value"),
        ],
    );
    let (world, graph) = build(&doc);
    assert!(graph.analysis().prefix_len.is_some());
    assert_eq!(
        report(&graph, "n1").stage,
        None,
        "world nodes are not chunk stages"
    );

    let smoothed = graph.export("smoothed").unwrap().slot;
    let chunk = graph.generate(ChunkCoord(33, 31, 0), &world).unwrap();
    let origin = chunk.origin();

    // The chunk's corner column averages samples from three neighbor chunks.
    for (x, y) in [(0usize, 0usize), (31, 31), (0, 17)] {
        let (gx, gy) = (origin.0 + x as i64, origin.1 + y as i64);
        let mut total = 0.0;
        for dy in -1..=1 {
            for dx in -1..=1 {
                total += world.graph().eval(gx + dx, gy + dy)[0];
            }
        }

        let value = chunk.chunk_data().columns[(x, y, 0)][smoothed];
        assert!(
            (f64::from(value) - total / 9.0).abs() < 1e-5,
            "({x}, {y}): {value} vs {}",
            total / 9.0
        );
    }
}

#[test]
fn world_reroutes_chain_and_feed_columns() {
    let doc = doc(
        vec![
            node("n1", "world_noise"),
            node("n2", "reroute_world"),
            node("n3", "reroute_world"),
            node("n4", "reroute_column"),
            named("n5", "export_column", "value"),
        ],
        vec![
            edge("n1", "noise", "n2", "in"),
            edge("n2", "out", "n3", "in"),
            edge("n3", "out", "n4", "in"),
            edge("n4", "out", "n5", "value"),
        ],
    );
    let (world, graph) = build(&doc);

    assert!(graph
        .analysis()
        .nodes
        .iter()
        .all(|n| n.status == NodeStatus::Active));
    assert_eq!(
        graph.analysis().schema.world_fields().len(),
        1,
        "reroutes add no fields"
    );
    assert_eq!(graph.export("value").unwrap().field, "n1.noise");

    let slot = graph.export("value").unwrap().slot;
    let chunk = graph.generate(ChunkCoord(40, 40, 0), &world).unwrap();
    let origin = chunk.origin();
    let expected = world.graph().eval(origin.0 + 3, origin.1 + 4)[0] as f32;

    assert_eq!(chunk.chunk_data().columns[(3, 4, 0)][slot], expected);
}

#[test]
fn identical_world_nodes_give_identical_worlds_and_signatures_track_world_edits() {
    let a = sampled_graph();
    let mut b = a.clone();
    b.nodes[3].label = Some("renamed".into()); // chunk node

    assert_eq!(
        worldgraph::world_signature(&a),
        worldgraph::world_signature(&b)
    );
    b.nodes[1] = with_param(node("n2", "world_remap"), "out_max", 3.0); // world node
    assert_ne!(
        worldgraph::world_signature(&a),
        worldgraph::world_signature(&b)
    );
    // No world nodes, no world signature.
    assert_eq!(
        worldgraph::world_signature(&doc(vec![node("n1", "export_column")], vec![])),
        worldgraph::pipeline::EMPTY_WORLD_SIGNATURE
    );

    let config = config();
    let (w1, w2) = (
        WorldStore::for_doc(&a, &config).unwrap(),
        WorldStore::for_doc(&a, &config).unwrap(),
    );
    assert_eq!(w1.graph().eval(10, -20), w2.graph().eval(10, -20));
}
