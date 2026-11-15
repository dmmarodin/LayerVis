//! Consumes a saved graph the way the game will: load the file, build the
//! pipeline, install it with LayerGen's Bevy plugin, read values by export
//! name (never by internal node ids).

use std::{
    sync::Arc,
    thread,
    time::{Duration, Instant},
};

use bevy_app::App;
use layergen::{
    ChunkCoord, ChunkGenerator, ChunkSize, LayerGenPlugin, PipelineConfig, WorldBounds,
};
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
    let doc = saved_graph();
    let world = Arc::new(WorldStore::for_doc(&doc, &config).unwrap());
    let graph = GraphPipeline::build(&doc, config, &world).unwrap();

    let height = graph
        .export("height")
        .expect("default graph exports height")
        .clone();
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

    // The `height` export is the terrain elevation, sampled from the world.
    let elevation = graph
        .elevation_world_slot()
        .expect("the terrain has an elevation");
    let origin = chunk.origin();
    for (column, x, y, _) in chunk.chunk_data().columns.iter() {
        let expected = world.graph().eval(origin.0 + x as i64, origin.1 + y as i64)[elevation];
        assert_eq!(column[height.slot], expected as f32);
    }
}

#[test]
fn missing_exports_and_blocked_nodes_are_reported_not_fatal() {
    let mut doc = saved_graph();
    // Break the graph: drop the elevation node, which everything depends on.
    doc.nodes.retain(|n| n.id != "n2");

    let config = config();
    let world = Arc::new(WorldStore::for_doc(&doc, &config).unwrap());
    let graph = GraphPipeline::build(&doc, config, &world).unwrap();

    assert!(graph.export("height").is_none());
    assert!(graph
        .analysis()
        .nodes
        .iter()
        .filter(|n| n.kind != "tectonics")
        .all(|n| n
            .reason
            .as_deref()
            .is_some_and(|r| r.contains("not connected"))));
    // Still generates (nothing runs).
    assert!(graph.validate(config, &world).is_ok());
}

/// The game's path for slow worlds: the atlas builds in LayerGen's
/// preparation step, and chunks follow once it is ready.
#[test]
fn deferred_definition_builds_the_atlas_during_preparation() {
    use layergen::PreparationStatus;
    use worldgraph::DeferredGraphDefinition;

    let mut doc = steps::eroded_graph();
    doc.node_mut("n6")
        .unwrap()
        .params
        .insert("iterations".into(), 8.0);
    let definition = DeferredGraphDefinition::new(doc.clone(), config());
    let height = definition.export("height").expect("height export").clone();
    let height_world = definition
        .export_world_slot("height")
        .expect("height is a world value");
    let progress = definition.progress_handle();

    let mut app = App::new();
    app.add_plugins(LayerGenPlugin::new(definition));

    let coord = ChunkCoord(32, 32, 0);
    app.world_mut()
        .resource_mut::<ChunkGenerator<Voxel, ColumnData>>()
        .request_batch(coord)
        .unwrap();

    let deadline = Instant::now() + Duration::from_secs(60);
    let chunk = loop {
        app.update();
        let mut generator = app
            .world_mut()
            .resource_mut::<ChunkGenerator<Voxel, ColumnData>>();
        assert!(!matches!(
            generator.preparation_status(),
            PreparationStatus::Failed(_)
        ));
        if let Some(completion) = generator.take_completed() {
            break completion.result.unwrap();
        }
        assert!(
            Instant::now() < deadline,
            "preparation or generation timed out"
        );
        thread::sleep(Duration::from_millis(1));
    };

    let world = app
        .world()
        .resource::<ChunkGenerator<Voxel, ColumnData>>()
        .shared::<WorldStore>()
        .expect("prepared world");
    assert!(!world.atlas().rivers().is_empty(), "the atlas ran");
    assert!(progress
        .lock()
        .unwrap()
        .as_ref()
        .is_some_and(|p| p.total == 12));

    // Column exports and point queries agree.
    let origin = chunk.origin();
    for (column, x, y, _) in chunk.chunk_data().columns.iter() {
        let expected = world.graph().eval(origin.0 + x as i64, origin.1 + y as i64)[height_world];
        assert_eq!(column[height.slot], expected as f32);
    }
}
