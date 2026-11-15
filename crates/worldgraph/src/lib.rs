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

pub mod atlas;
pub mod biomes;
pub mod climate;
pub mod detail;
pub mod geology;
pub mod graph;
pub mod lru;
pub mod materials;
pub mod migrate;
pub mod pipeline;
#[cfg(feature = "scripting")]
pub mod script;
pub mod steps;
pub mod strata;
pub mod tectonics;
pub mod terrain;
pub mod world;

use std::sync::{Arc, Mutex, OnceLock};

use layergen::{
    Chunk, ChunkCoord, ChunkStep, DataSet, GenerationDefinition, Neighborhood, NeighborhoodLimits,
    PipelineBuilder, PipelineConfig, PipelineStepResult, StepContext,
};
use serde::Serialize;

pub use atlas::{
    Atlas, AtlasContext, AtlasGrid, AtlasHooks, AtlasIo, AtlasProgress, AtlasRun, AtlasSnapshot,
    AtlasStage, BoxedAtlasStage, RiverNetwork, RiverPath, CANCELLED as ATLAS_CANCELLED,
    DEFAULT_CELL_SIZE,
};
pub use biomes::{BiomeDef, BiomeTable};
pub use materials::{MaterialCategory, MaterialDef, MaterialTable};
pub use strata::{StrataGrid, STRATA_LAYERS};
pub use graph::Extent;
pub use graph::{EdgeDoc, FrameDoc, GraphDoc, NodeDoc, SocketRef, DEFAULT_SEED};
pub use lru::{ByteLru, Weigh};
pub use pipeline::{
    analyze, build_world_stage, creates_cycle, resolve, world_signature, Analysis, AtlasSource,
    EdgeReport, Export, NodeReport, NodeStatus, ParamReport, Schema, StepTiming, Text,
    WorldBuildContext, WorldGraph, WorldSampleBinding, WorldStep,
};
pub use world::{WorldStats, WorldStore};

use pipeline::{build_neighborhood, DynPipeline, ResolvedNode, Timings};

/// One slot per node output (SSA), so column slots are generous.
pub const VOXEL_SLOTS: usize = 4;
/// Material-id (u16 registry id) outputs of voxel nodes.
pub const MATERIAL_SLOTS: usize = 4;
pub const COLUMN_SLOTS: usize = 16;
/// World-scope outputs (one slot per world node output).
pub const WORLD_SLOTS: usize = 32;

/// Field-bag voxel: outputs map to fixed slots via the schema.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Voxel {
    pub slots: [f32; VOXEL_SLOTS],
    /// Material ids (indices into the world's `MaterialTable`), one per
    /// material output (`export_material` names them).
    pub materials: [u16; MATERIAL_SLOTS],
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

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum FieldScope {
    Voxel,
    #[default]
    Column,
    World,
    /// Whole-world grids computed once per world (see `atlas.rs`).
    Atlas,
}

impl FieldScope {
    /// Computed once per world (world and atlas scopes), never per chunk.
    pub fn is_world_stage(self) -> bool {
        matches!(self, FieldScope::World | FieldScope::Atlas)
    }

    /// Wires run downward through the scopes: world values feed chunk nodes
    /// (sampled once per column), never the other way around. Atlas and world
    /// meet only through the `rasterize` / `sample` adapter nodes.
    pub fn rank(self) -> u8 {
        match self {
            FieldScope::Atlas => 0,
            FieldScope::World => 1,
            FieldScope::Column => 2,
            FieldScope::Voxel => 3,
        }
    }
}

/// What a socket carries, orthogonal to its scope.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ValueKind {
    /// One f32 (f64 at world scope) per sample: today's sockets.
    #[default]
    Scalar,
    /// A u16 material id (an index into the world's `MaterialTable`).
    Material,
    /// Atlas only: per-cell rock layers (`StrataGrid`).
    Strata,
    /// Atlas only: river lines with a spatial index (`RiverNetwork`).
    Rivers,
}

/// The full type of a socket: where its value lives and what it is.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SocketType {
    pub scope: FieldScope,
    pub kind: ValueKind,
}

impl SocketType {
    pub const fn new(scope: FieldScope, kind: ValueKind) -> Self {
        Self { scope, kind }
    }

    pub const fn scalar(scope: FieldScope) -> Self {
        Self::new(scope, ValueKind::Scalar)
    }
}

/// Can an output of type `from` be wired into an input of type `to`?
/// Kinds must be equal. Equal scopes connect; a world output may also feed a
/// column input (it is sampled at every column). Nothing flows from chunk or
/// voxel scope into world nodes, and voxel inputs only take voxel values.
/// Atlas sockets only connect to atlas sockets (world ↔ atlas scalars go
/// through `rasterize` / `sample`); a chunk or world node may still declare an
/// atlas input (strata, rivers) and read it from the finished atlas directly.
pub fn can_connect(from: SocketType, to: SocketType) -> bool {
    from.kind == to.kind
        && (from.scope == to.scope
            || (from.scope == FieldScope::World && to.scope == FieldScope::Column))
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
    /// Value kind; `slot` indexes that kind's slot space.
    pub kind: ValueKind,
    pub slot: usize,
    pub unit: Text,
    /// Default display range for view modes.
    pub min: f32,
    pub max: f32,
    /// Default color ramp name.
    pub ramp: Text,
}

/// The LayerGen definition type every graph builds into.
pub type Definition = dyn GenerationDefinition<Voxel, Shared = WorldStore, ChunkData = ColumnData>;

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
        if world.signature() != world_signature(doc) {
            return Err("The world was not built from this graph's world nodes \
                        (build it with WorldStore::for_doc)"
                .to_owned());
        }

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
            None => Arc::new(
                base()
                    .add_chunk_step(range(0..steps)?)
                    .build(|_| Voxel::default()),
            ),
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

    /// World slot of the terrain's elevation: the last world output named
    /// `elevation` (so an elevation filter wins over the elevation it filters).
    /// What land/ocean statistics read.
    pub fn elevation_world_slot(&self) -> Option<usize> {
        self.analysis
            .schema
            .world_fields()
            .iter()
            .rev()
            .find(|field| field.name == "elevation")
            .map(|field| field.slot)
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

/// A LayerGen definition that builds its world (atlas included) in LayerGen's
/// **preparation** step: `LayerGenPlugin` runs it on the task pool when the
/// app starts, and chunk jobs wait until it is ready. Use it when the world
/// takes a while to build (atlas nodes) and the app should keep running
/// meanwhile; `GraphPipeline::definition` needs an already built world.
///
/// Exports are known up front (they only depend on the graph), so game code
/// can look up slots before the world is ready.
pub struct DeferredGraphDefinition {
    doc: GraphDoc,
    config: PipelineConfig,
    analysis: Analysis,
    pipeline: OnceLock<GraphPipeline>,
    progress: Arc<Mutex<Option<AtlasProgress>>>,
}

impl DeferredGraphDefinition {
    pub fn new(doc: GraphDoc, config: PipelineConfig) -> Self {
        let resolved: Vec<ResolvedNode> = doc.nodes.iter().map(resolve).collect();
        let analysis = analyze(&doc, &resolved);

        Self {
            doc,
            config,
            analysis,
            pipeline: OnceLock::new(),
            progress: Arc::new(Mutex::new(None)),
        }
    }

    pub fn exports(&self) -> &[Export] {
        &self.analysis.exports
    }

    pub fn export(&self, name: &str) -> Option<&Export> {
        self.analysis.exports.iter().find(|e| e.name == name)
    }

    /// The world slot behind an export of a world value (exports of world
    /// values hand chunks a sampled column copy; this is the original), for
    /// point queries anywhere: `world.graph().eval(x, y)[slot]`. None for
    /// unknown exports and exports of chunk or voxel values.
    pub fn export_world_slot(&self, name: &str) -> Option<usize> {
        let export = self.export(name)?;
        self.analysis
            .schema
            .fields
            .iter()
            .find(|field| {
                field.key == export.field
                    && field.scope == FieldScope::World
                    && field.kind == ValueKind::Scalar
            })
            .map(|field| field.slot)
    }

    /// The latest atlas build progress while preparing (None before the first
    /// report). Clone it before handing the definition to `LayerGenPlugin`.
    pub fn progress_handle(&self) -> Arc<Mutex<Option<AtlasProgress>>> {
        Arc::clone(&self.progress)
    }
}

impl GenerationDefinition<Voxel> for DeferredGraphDefinition {
    type Shared = WorldStore;
    type ChunkData = ColumnData;

    fn config(&self) -> PipelineConfig {
        self.config
    }

    fn requires_preparation(&self) -> bool {
        true
    }

    fn prepare_shared(&self) -> Result<Arc<WorldStore>, String> {
        let progress = |p: &AtlasProgress| {
            *self.progress.lock().unwrap_or_else(|e| e.into_inner()) = Some(p.clone());
        };
        let hooks = AtlasHooks {
            progress: Some(&progress),
            cancelled: None,
        };
        let world = Arc::new(WorldStore::for_doc_with(&self.doc, &self.config, hooks)?);
        // Repeated direct calls build the same world again (deterministic):
        // the first pipeline stays.
        if self.pipeline.get().is_none() {
            let _ = self
                .pipeline
                .set(GraphPipeline::build(&self.doc, self.config, &world)?);
        }
        Ok(world)
    }

    fn generate_chunk_with_shared(
        &self,
        coord: ChunkCoord,
        shared: &WorldStore,
    ) -> Result<Chunk<Voxel, ColumnData>, String> {
        self.pipeline
            .get()
            .ok_or_else(|| "the world is not prepared yet".to_owned())?
            .generate(coord, shared)
    }
}
