//! Consumes a saved graph the way the game will: load the file, build the
//! pipeline, install it with LayerGen's Bevy plugin, read values by export
//! name (never by internal node ids).

use std::{
    sync::Arc,
    thread,
    time::{Duration, Instant},
};

use bevy_app::App;
use layergen::{ChunkCoord, ChunkGenerator, ChunkSize, LayerGenPlugin, PipelineConfig, WorldBounds};
use worldgraph::{steps, ColumnData, GraphDoc, GraphPipeline, Voxel, WorldStore};

fn config() -> PipelineConfig {
    PipelineConfig::new(
        WorldBounds::new((-1024, -1024, 0), (2048, 2048, 128)).unwrap(),
        ChunkSize::new(32, 32, 32).unwrap(),
        77,
    )
}

/// The default editor graph, round-tripped through its file format.
fn saved_graph() -> GraphDoc {
    GraphDoc::from_json(&steps::default_graph().to_json()).unwrap()
}

#[test]
fn game_reads_exports_through_the_bevy_runtime() {
    let config = config();
    let world = Arc::new(WorldStore::new(&config));
    let graph = GraphPipeline::build(&saved_graph(), config, &world).unwrap();

    let height = graph.export("height").expect("default graph exports height").clone();
    assert!(graph.analysis().nodes.iter().all(|n| n.reason.is_none()));

    let mut app = App::new();
    app.add_plugins(LayerGenPlugin::new(graph.definition(&world)));

    let coord = ChunkCoord(33, 31, 0);
    app.world_mut()
        .resource_mut::<ChunkGenerator<Voxel, ColumnData>>()
        .request_batch(coord)
        .unwrap();

    let deadline = Instant::now() + Duration::from_secs(10);
    let chunk = loop {
        app.update();
        let completion = app
            .world_mut()
            .resource_mut::<ChunkGenerator<Voxel, ColumnData>>()
            .take_completed();
        if let Some(completion) = completion {
            break completion.result.unwrap();
        }
        assert!(Instant::now() < deadline, "generation timed out");
        thread::sleep(Duration::from_millis(1));
    };

    let origin = chunk.origin();
    for (column, x, y, _) in chunk.chunk_data().columns.iter() {
        let expected = world.elevation_at(origin.0 + x as i64, origin.1 + y as i64);
        assert_eq!(column[height.slot], expected);
    }
}

#[test]
fn missing_exports_and_blocked_nodes_are_reported_not_fatal() {
    let mut doc = saved_graph();
    // Break the graph: drop elevation, which everything depends on.
    doc.nodes.retain(|n| n.id != "n1");

    let config = config();
    let world = Arc::new(WorldStore::new(&config));
    let graph = GraphPipeline::build(&doc, config, &world).unwrap();

    assert!(graph.export("height").is_none());
    assert!(graph
        .analysis()
        .nodes
        .iter()
        .all(|n| n.reason.as_deref().is_some_and(|r| r.contains("not connected"))));
    // Still generates (nothing runs).
    assert!(graph.validate(config, &world).is_ok());
}
