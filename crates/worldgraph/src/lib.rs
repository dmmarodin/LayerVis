//! World-generation node graphs on LayerGen.
//!
//! A [`GraphDoc`] (the saved file, see `graph.rs`) is analyzed into an
//! execution order and schema and built into one LayerGen pipeline by
//! [`GraphPipeline::build`]. The editor (LayerVis) and the game use the same
//! node kinds, world models and building code, so a graph generates the same
//! terrain in both.
//!
//! Game-facing contract: wire values into **Export** nodes; read them by
//! export name ([`GraphPipeline::export`]) instead of internal node ids.
//!
//! ```ignore
//! let doc = GraphDoc::from_json(&std::fs::read_to_string("graph.json")?)?;
//! let config = PipelineConfig::new(bounds, chunk_size, seed);
//! let world = Arc::new(WorldStore::new(&config));
//! let graph = GraphPipeline::build(&doc, config, &world)?;
//! let height = graph.export("height").ok_or("no height export")?;
//! app.add_plugins(LayerGenPlugin::new(graph.definition(&world)));
//! // later: chunk.chunk_data().columns[(x, y, 0)][height.slot]
//! ```

pub mod graph;
pub mod lru;
pub mod pipeline;
#[cfg(feature = "scripting")]
pub mod script;
pub mod steps;
pub mod tectonics;
pub mod world;

use std::sync::{Arc, Mutex};

use layergen::{
    Chunk, ChunkCoord, ChunkStep, DataSet, GenerationDefinition, Neighborhood, NeighborhoodLimits,
    PipelineBuilder, PipelineConfig, PipelineStepResult, StepContext,
};
use serde::Serialize;

pub use graph::{EdgeDoc, FrameDoc, GraphDoc, NodeDoc, SocketRef, DEFAULT_SEED};
pub use lru::{ByteLru, Weigh};
pub use pipeline::{
    analyze, creates_cycle, resolve, Analysis, EdgeReport, Export, NodeReport, NodeStatus,
    Schema, StepTiming, Text,
};
pub use world::{WorldSample, WorldStore};

use pipeline::{build_neighborhood, DynPipeline, ResolvedNode, Timings};

/// One slot per node output (SSA), so column slots are generous.
pub const VOXEL_SLOTS: usize = 4;
pub const COLUMN_SLOTS: usize = 16;

/// Field-bag voxel: outputs map to fixed slots via the schema.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Voxel {
    pub slots: [f32; VOXEL_SLOTS],
}

/// LayerGen chunk data `C`: one slot array per (x, y) column.
#[derive(Clone, Debug, PartialEq)]
pub struct ColumnData {
    pub columns: DataSet<[f32; COLUMN_SLOTS]>,
}

impl ColumnData {
    pub fn new(context: &StepContext) -> Self {
        let (width, height, _) = context.dimensions();

        Self {
            columns: DataSet::new(width, height, 1, |_| [0.0; COLUMN_SLOTS])
                .expect("Chunk dimensions are nonzero"),
        }
    }
}

impl Weigh for Arc<ColumnData> {
    fn weight(&self) -> usize {
        self.columns.len() * COLUMN_SLOTS * std::mem::size_of::<f32>()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum FieldScope {
    Voxel,
    Column,
    World,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FieldDef {
    /// Unique field id: `node.socket` for node outputs, the name for world fields.
    pub key: String,
    /// Producing node id (empty for world fields).
    pub node: String,
    /// Producing node's label (empty for world fields).
    pub node_label: String,
    /// Socket name.
    pub name: Text,
    pub scope: FieldScope,
    pub slot: usize,
    pub unit: Text,
    /// Default display range for view modes.
    pub min: f32,
    pub max: f32,
    /// Default color ramp name.
    pub ramp: Text,
}

/// The LayerGen definition type every graph builds into.
pub type Definition =
    dyn GenerationDefinition<Voxel, Shared = WorldStore, ChunkData = ColumnData>;

/// Neighborhood prefix results are column data per chunk: this holds a few
/// thousand chunks per pipeline.
const PREFIX_CACHE_BYTES: usize = 64 * 1024 * 1024;

/// The neighborhood prefix, with its results cached per chunk coord.
///
/// LayerGen re-runs the prefix for every dependency chunk of every request
/// (radius 1 in X/Y: 9 prefix runs per chunk). When every prefix node only
/// writes column outputs, a prefix result is entirely the column data, so it
/// is computed once per chunk and restored afterwards. Cells keep their
/// initializer values in that case, so restoring column data is exact.
struct CachedPrefix {
    inner: DynPipeline,
    cache: Option<Mutex<ByteLru<ChunkCoord, Arc<ColumnData>>>>,
}

impl CachedPrefix {
    fn new(inner: DynPipeline, cacheable: bool) -> Self {
        Self {
            inner,
            cache: cacheable.then(|| Mutex::new(ByteLru::new(PREFIX_CACHE_BYTES))),
        }
    }
}

impl ChunkStep<Voxel, ColumnData, WorldStore> for CachedPrefix {
    fn run(
        &self,
        cells: &mut DataSet<Voxel>,
        chunk: &mut ColumnData,
        context: &StepContext,
        world: &WorldStore,
    ) -> PipelineStepResult {
        let Some(cache) = &self.cache else {
            return self.inner.run(cells, chunk, context, world);
        };

        let coord = context.chunk_coord();
        let lock = || cache.lock().unwrap_or_else(|p| p.into_inner());

        if let Some(hit) = lock().get(coord) {
            *chunk = ColumnData::clone(&hit);
            return Ok(());
        }

        self.inner.run(cells, chunk, context, world)?;
        lock().insert(coord, Arc::new(chunk.clone()));
        Ok(())
    }
}

/// A graph doc built into a ready LayerGen pipeline.
pub struct GraphPipeline {
    analysis: Analysis,
    definition: Arc<Definition>,
    timings: Arc<Timings>,
}

impl GraphPipeline {
    /// Analyzes and builds a graph. Invalid nodes do not fail the build:
    /// they are reported as blocked in [`GraphPipeline::nodes`] and skipped.
    pub fn build(
        doc: &GraphDoc,
        config: PipelineConfig,
        world: &Arc<WorldStore>,
    ) -> Result<Self, String> {
        let resolved: Vec<ResolvedNode> = doc.nodes.iter().map(resolve).collect();
        let analysis = analyze(doc, &resolved);
        let timings = Arc::new(Timings::for_analysis(doc, &analysis));
        let steps = analysis.order.len();

        // `prepare` only fixes the shared type; callers supply an already
        // built WorldStore (see `definition`).
        let base = || {
            PipelineBuilder::new(config)
                .prepare(|config| Ok::<_, String>(WorldStore::new(config)))
                .chunk_data(|context, _world: &WorldStore| ColumnData::new(context))
        };
        let range = |range| DynPipeline::build(doc, &resolved, &analysis, world, range, &timings);

        let definition: Arc<Definition> = match analysis.prefix_len {
            None => Arc::new(base().add_chunk_step(range(0..steps)?).build(|_| Voxel::default())),
            Some(prefix_len) => {
                // LayerGen stages: prefix (re-run per dependency chunk), then
                // one neighborhood node, then suffix (once per requested chunk).
                let cacheable = DynPipeline::columns_only(&resolved, &analysis, 0..prefix_len);
                let prefix = CachedPrefix::new(range(0..prefix_len)?, cacheable);
                let (radius, neighborhood) =
                    build_neighborhood(doc, &resolved, &analysis, prefix_len, &timings)?;
                let suffix = range(prefix_len + 1..steps)?;

                Arc::new(
                    base()
                        .add_chunk_step(prefix)
                        .build(|_| Voxel::default())
                        .with_chunk_neighborhood(
                            Neighborhood::axes(radius.0, radius.1, radius.2),
                            NeighborhoodLimits::default(),
                            neighborhood,
                        )
                        .add_chunk_step(suffix),
                )
            }
        };

        Ok(Self {
            analysis,
            definition,
            timings,
        })
    }

    /// Generates one chunk synchronously.
    pub fn generate(
        &self,
        coord: ChunkCoord,
        world: &WorldStore,
    ) -> Result<Chunk<Voxel, ColumnData>, String> {
        self.definition.generate_chunk_with_shared(coord, world)
    }

    /// Generates the chunk containing (0, 0) (or the world's first column):
    /// a cheap end-to-end check that every node runs, e.g. after an edit.
    pub fn validate(&self, config: PipelineConfig, world: &WorldStore) -> Result<(), String> {
        let origin = config.world().origin();
        let (coord, _) = config
            .locate((0.max(origin.0), 0.max(origin.1), origin.2))
            .ok_or("World has no validation chunk")?;

        self.generate(coord, world).map(|_| ())
    }

    /// A LayerGen definition for `LayerGenPlugin` / `ChunkManagerPlugin`,
    /// sharing this pipeline and an already built world.
    pub fn definition(&self, world: &Arc<WorldStore>) -> GraphDefinition {
        GraphDefinition {
            inner: Arc::clone(&self.definition),
            world: Arc::clone(world),
        }
    }

    /// Named outputs for game code (see Export nodes).
    pub fn exports(&self) -> &[Export] {
        &self.analysis.exports
    }

    pub fn export(&self, name: &str) -> Option<&Export> {
        self.analysis.exports.iter().find(|e| e.name == name)
    }

    pub fn schema(&self) -> &Schema {
        &self.analysis.schema
    }

    pub fn analysis(&self) -> &Analysis {
        &self.analysis
    }

    /// Per-node mean time per generated chunk.
    pub fn timings(&self) -> Vec<StepTiming> {
        self.timings.snapshot()
    }
}

/// `GenerationDefinition` over a built graph with ready shared world data,
/// so it can be installed with `LayerGenPlugin::new(...)`.
pub struct GraphDefinition {
    inner: Arc<Definition>,
    world: Arc<WorldStore>,
}

impl GenerationDefinition<Voxel> for GraphDefinition {
    type Shared = WorldStore;
    type ChunkData = ColumnData;

    fn config(&self) -> PipelineConfig {
        self.inner.config()
    }

    fn requires_preparation(&self) -> bool {
        false
    }

    fn prepare_shared(&self) -> Result<Arc<WorldStore>, String> {
        Ok(Arc::clone(&self.world))
    }

    fn generate_chunk_with_shared(
        &self,
        coord: ChunkCoord,
        shared: &WorldStore,
    ) -> Result<Chunk<Voxel, ColumnData>, String> {
        self.inner.generate_chunk_with_shared(coord, shared)
    }
}

/// World-model fields, in `WorldSample::values()` order.
pub fn world_fields() -> Vec<FieldDef> {
    let field = |slot: usize, name: &'static str, unit: &'static str, min, max, ramp: &'static str| {
        FieldDef {
            key: name.to_owned(),
            node: String::new(),
            node_label: String::new(),
            name: name.into(),
            scope: FieldScope::World,
            slot,
            unit: unit.into(),
            min,
            max,
            ramp: ramp.into(),
        }
    };

    vec![
        field(0, "plate", "id", 0.0, world::PLATE_COUNT as f32, "categorical"),
        field(1, "boundary_distance", "cells", 0.0, 400.0, "grayscale"),
        field(2, "convergence", "", -1.5, 1.5, "diverging"),
        field(3, "elevation", "cells", 0.0, 128.0, "terrain"),
    ]
}
