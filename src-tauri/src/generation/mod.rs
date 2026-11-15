//! LayerVis editing session over a `worldgraph` graph.
//!
//! The session owns the graph document. Every edit is applied to a copy,
//! built with `worldgraph::GraphPipeline`, validated on one chunk and only
//! then published as a new revision. Graph semantics (format, node kinds,
//! analysis, building) live in the `worldgraph` crate shared with the game;
//! this module adds editor concerns: revisions, auto-connect, payload
//! encoding and caching.

mod atlas_job;
mod cache;

use std::sync::{Arc, Mutex, RwLock};

use atlas_job::AtlasJobs;
pub use atlas_job::AtlasStatus;

use cache::{CacheKey, PayloadCache};
use layergen::{ChunkCoord, ChunkSize, PipelineConfig, WorldBounds};
use serde::Serialize;
use worldgraph::{
    can_connect, creates_cycle, SocketType, graph::id_order, pipeline::SCRIPT_KIND, resolve, steps,
    world_signature, ColumnData, EdgeDoc, Export, Extent, FieldDef, FieldScope, FrameDoc, GraphDoc,
    GraphPipeline, NodeDoc, NodeReport, NodeStatus, RiverPath, SocketRef, Voxel, WorldStats,
};

#[cfg(test)]
use layergen::Chunk;

pub use worldgraph::{EdgeReport, StepTiming, WorldStore};

use rayon::prelude::*;

/// Upper bound on samples in one world view request (~4 MB per field).
const MAX_WORLD_SAMPLES: u64 = 1024 * 1024;

/// Column payloads are a few KB per 32×32 chunk; this holds thousands.
const PAYLOAD_CACHE_BYTES: usize = 64 * 1024 * 1024;

fn seed_as_string<S: serde::Serializer>(seed: &u64, serializer: S) -> Result<S::Ok, S::Error> {
    serializer.collect_str(seed)
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorldInfo {
    pub origin: (i64, i64, i64),
    pub dimensions: (u32, u32, u32),
    pub chunk_size: (u32, u32, u32),
    /// A string: u64 seeds exceed what JS numbers represent exactly.
    #[serde(serialize_with = "seed_as_string")]
    pub seed: u64,
    /// Pipeline revision this schema belongs to (also tagged in payloads).
    pub revision: u64,
    /// Column outputs in the order they appear in chunk payloads.
    pub column_fields: Vec<FieldDef>,
    pub voxel_fields: Vec<FieldDef>,
    /// Outputs of world-scope nodes, in the order they appear in world
    /// payloads (overview samples).
    pub world_fields: Vec<FieldDef>,
    /// Game-facing named outputs (Export nodes).
    pub exports: Vec<Export>,
    /// Coarse facts about the current world (land fraction, landmasses, …),
    /// read from the terrain's `elevation` world output.
    pub world_stats: WorldStats,
    /// Changes when the world stage is rebuilt (seed or world nodes): cached
    /// overview samples are stale then.
    pub world_generation: u64,
    pub atlas: AtlasInfo,
    /// The world's material ids (index = id): colors and names for the
    /// `material` ramp, legends and the cursor readout.
    pub materials: Vec<PaletteEntry>,
    /// The world's biome ids (index = id), for the `biome` ramp.
    pub biomes: Vec<PaletteEntry>,
}

/// One id of a categorical table (material or biome).
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PaletteEntry {
    pub name: String,
    pub label: String,
    pub color: [u8; 3],
    /// Material category or biome glyph.
    pub detail: String,
}

/// The atlas of the published world.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AtlasInfo {
    /// True while the real atlas is still being built (the published world
    /// has zero-filled placeholder grids: chunks and world views wait).
    pub pending: bool,
    /// Atlas cells per side.
    pub grid: (u32, u32),
    /// Columns per atlas cell.
    pub cell_size: u32,
    /// Outputs of atlas nodes (`atlas_grid` serves them by slot).
    pub fields: Vec<FieldDef>,
    /// Scrubber frames, in the order `atlas_snapshot` serves them.
    pub snapshots: Vec<SnapshotInfo>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotInfo {
    pub node: String,
    pub label: String,
}

/// River lines of one flow node.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RiverSet {
    pub node: String,
    pub rivers: Vec<RiverPath>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct KindInfo {
    pub id: &'static str,
    pub label: &'static str,
    pub description: &'static str,
    /// Where the node runs: world, chunk (`column`) or voxel.
    pub scope: FieldScope,
}

/// A starting graph (File → New from template).
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TemplateInfo {
    pub id: &'static str,
    pub label: &'static str,
    pub description: &'static str,
}

pub fn templates() -> Vec<TemplateInfo> {
    steps::TEMPLATES
        .iter()
        .map(|template| TemplateInfo {
            id: template.id,
            label: template.label,
            description: template.description,
        })
        .collect()
}

/// The doc of a template, or an error for an unknown id.
pub fn template_doc(id: &str) -> Result<GraphDoc, String> {
    steps::TEMPLATES
        .iter()
        .find(|template| template.id == id)
        .map(|template| (template.build)())
        .ok_or_else(|| format!("Unknown template `{id}`"))
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PipelineGraph {
    pub revision: u64,
    pub nodes: Vec<NodeReport>,
    pub edges: Vec<EdgeReport>,
    pub exports: Vec<Export>,
    /// Editor annotations (layout only).
    pub frames: Vec<FrameDoc>,
    /// Built-in kinds that can be added.
    pub kinds: Vec<KindInfo>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CacheStats {
    pub entries: usize,
    pub used_bytes: usize,
    pub hits: u64,
    pub misses: u64,
}

/// One immutable published revision.
struct Active {
    revision: u64,
    /// Config and world models belong to the doc's seed: changing the seed
    /// publishes a revision with a new world stage.
    config: PipelineConfig,
    world: Arc<WorldStore>,
    /// Coarse facts about `world` (land fraction, landmasses), computed once
    /// per world stage.
    stats: WorldStats,
    /// Counts world stages built in this session: the overview map is
    /// resampled when it changes (a new seed, params or world nodes), not when
    /// only chunk or voxel nodes change.
    world_generation: u64,
    pipeline: GraphPipeline,
}

/// A doc built into a pipeline, with the world stage for its seed, params and
/// world nodes.
struct Built {
    config: PipelineConfig,
    world: Arc<WorldStore>,
    stats: WorldStats,
    world_generation: u64,
    pipeline: GraphPipeline,
}

const NO_WORLD_STATS: WorldStats = WorldStats {
    land_fraction: 0.0,
    landmasses: 0,
    edge_land: 0.0,
};

/// Error of chunk and world requests while the atlas is still building.
pub const ATLAS_PENDING: &str = "The atlas is still being built";

/// Samples per side of the coarse grid behind `WorldStats`.
const STATS_RESOLUTION: usize = 96;

/// The pipeline config of a doc: its extent (format 3), or LayerVis's
/// original world (8192 x 8192 x 128) when it has none.
fn config_for(doc: &GraphDoc) -> Result<PipelineConfig, String> {
    let extent = doc.extent.unwrap_or(Extent::DEFAULT);
    extent.validate()?;
    Ok(PipelineConfig::new(
        WorldBounds::new(
            extent.origin(),
            (extent.size.0, extent.size.1, extent.depth),
        )?,
        ChunkSize::new(32, 32, 32)?,
        doc.seed,
    ))
}

/// Land statistics of a world: they read the terrain's elevation output, so
/// a world without one (or a placeholder) has nothing to report.
fn world_stats(pipeline: &GraphPipeline, world: &WorldStore) -> WorldStats {
    if world.is_placeholder() {
        return NO_WORLD_STATS;
    }
    pipeline
        .elevation_world_slot()
        .map_or(NO_WORLD_STATS, |slot| world.stats(slot, STATS_RESOLUTION))
}

/// Builds `doc`, reusing the world stage of `previous` when the seed and the
/// world-scope nodes are the same: the world is built once and serves every
/// revision until one of those changes. Editing chunk or voxel nodes never
/// rebuilds it.
///
/// A world whose atlas nodes must run is not built here: the doc gets a
/// placeholder world (zero-filled atlas grids) at once, and `AtlasJobs`
/// builds the real one in the background (or finds it among recent worlds).
fn build(doc: &GraphDoc, previous: Option<&Active>, jobs: &AtlasJobs) -> Result<Built, String> {
    let signature = world_signature(doc);
    let reusable = previous
        .filter(|active| active.config.seed() == doc.seed && active.world.signature() == signature);

    let (config, world, reused) = match reusable {
        Some(active) => (active.config, Arc::clone(&active.world), Some(active)),
        None => {
            let config = config_for(doc)?;
            let world = match jobs.lookup(doc.seed, &signature) {
                Some(world) => world,
                None if WorldStore::has_atlas_work(doc) => {
                    Arc::new(WorldStore::placeholder(doc, &config)?)
                }
                None => Arc::new(WorldStore::for_doc(doc, &config)?),
            };
            (config, world, None)
        }
    };
    let pipeline = GraphPipeline::build(doc, config, &world)?;

    let (stats, world_generation) = match reused {
        Some(active) => (active.stats, active.world_generation),
        None => (
            world_stats(&pipeline, &world),
            previous.map_or(1, |active| active.world_generation + 1),
        ),
    };

    Ok(Built {
        config,
        world,
        stats,
        world_generation,
        pipeline,
    })
}

/// Undo steps kept per session.
const HISTORY_LIMIT: usize = 200;

/// Mutable session state: the doc (incl. layout), the published revision and
/// the undo/redo history (doc snapshots).
struct State {
    doc: GraphDoc,
    active: Arc<Active>,
    undo: Vec<GraphDoc>,
    redo: Vec<GraphDoc>,
}

impl State {
    /// Remembers `previous` as an undo step (a new edit invalidates redo).
    fn record(&mut self, previous: GraphDoc) {
        if previous == self.doc {
            return;
        }
        self.undo.push(previous);
        if self.undo.len() > HISTORY_LIMIT {
            self.undo.remove(0);
        }
        self.redo.clear();
    }

    /// Publishes `doc` as the next revision.
    fn publish(&mut self, doc: GraphDoc, built: Built) {
        self.doc = doc;
        self.active = Arc::new(Active {
            revision: self.active.revision + 1,
            config: built.config,
            world: built.world,
            stats: built.stats,
            world_generation: built.world_generation,
            pipeline: built.pipeline,
        });
    }
}

pub struct Session {
    state: Arc<RwLock<State>>,
    cache: Mutex<PayloadCache>,
    atlas: Arc<AtlasJobs>,
}

impl Session {
    /// The default graph with the given seed.
    pub fn new(seed: u64) -> Result<Self, String> {
        Self::with_doc(GraphDoc {
            seed,
            ..steps::default_graph()
        })
    }

    /// Loads a saved graph (and its seed). Not validated by generation (a
    /// script that fails at runtime should still open, so it can be fixed in
    /// the editor).
    pub fn with_doc(doc: GraphDoc) -> Result<Self, String> {
        let atlas = Arc::new(AtlasJobs::new());
        let built = build(&doc, None, &atlas)?;

        let session = Self {
            state: Arc::new(RwLock::new(State {
                doc,
                active: Arc::new(Active {
                    revision: 0,
                    config: built.config,
                    world: built.world,
                    stats: built.stats,
                    world_generation: built.world_generation,
                    pipeline: built.pipeline,
                }),
                undo: Vec::new(),
                redo: Vec::new(),
            })),
            cache: Mutex::new(PayloadCache::new(PAYLOAD_CACHE_BYTES)),
            atlas,
        };
        session.atlas.sync(&session.state);
        Ok(session)
    }

    /// What the background atlas builder is doing.
    pub fn atlas_status(&self) -> AtlasStatus {
        self.atlas.status()
    }

    /// The active world, or an error while its atlas is still building.
    fn ready_active(&self) -> Result<Arc<Active>, String> {
        let active = self.active();
        if active.world.is_placeholder() {
            return Err(ATLAS_PENDING.to_owned());
        }
        Ok(active)
    }

    /// One atlas grid (by atlas slot) as a field-plane payload: one sample
    /// per atlas cell, `step` = cell size.
    pub fn atlas_payload(&self, slot: usize) -> Result<Vec<u8>, String> {
        let active = self.ready_active()?;
        let atlas = active.world.atlas();
        let grid = atlas
            .grid(slot)
            .ok_or_else(|| format!("No atlas grid in slot {slot}"))?;
        let context = atlas.context();
        Ok(encode_planes(
            context.origin,
            context.cell_size,
            grid.width(),
            grid.height(),
            0,
            &[grid.values().to_vec()],
        ))
    }

    /// One scrubber frame (index into `AtlasInfo::snapshots`). Frames are
    /// downsampled: `step` is the world columns per sample.
    pub fn atlas_snapshot_payload(&self, index: usize) -> Result<Vec<u8>, String> {
        let active = self.ready_active()?;
        let atlas = active.world.atlas();
        let snapshot = atlas
            .snapshots()
            .get(index)
            .ok_or_else(|| format!("No atlas snapshot {index}"))?;
        let context = atlas.context();
        let factor = context.grid.0.div_ceil(snapshot.grid.width()).max(1);
        Ok(encode_planes(
            context.origin,
            context.cell_size * factor,
            snapshot.grid.width(),
            snapshot.grid.height(),
            0,
            &[snapshot.grid.values().to_vec()],
        ))
    }

    /// River lines of every flow node in the published atlas.
    pub fn atlas_rivers(&self) -> Result<Vec<RiverSet>, String> {
        let active = self.ready_active()?;
        Ok(active
            .world
            .atlas()
            .rivers()
            .iter()
            .map(|network| RiverSet {
                node: network.node.clone(),
                rivers: network.paths.clone(),
            })
            .collect())
    }

    fn active(&self) -> Arc<Active> {
        Arc::clone(&self.state.read().unwrap_or_else(|p| p.into_inner()).active)
    }

    /// The current document (what gets saved).
    pub fn graph_doc(&self) -> GraphDoc {
        self.state
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .doc
            .clone()
    }

    /// Column payload for one chunk, served from the LRU cache when present.
    ///
    /// Keyed by pipeline revision, so edits never serve stale data; old
    /// revisions age out of the LRU. The cache lock is not held while
    /// generating (the frontend deduplicates requests).
    pub fn column_payload(&self, coord: ChunkCoord) -> Result<Arc<Vec<u8>>, String> {
        let active = self.ready_active()?;
        let key = CacheKey {
            revision: active.revision,
            coord,
        };

        if let Some(bytes) = self.lock_cache().get(key) {
            return Ok(bytes);
        }

        let chunk = active.pipeline.generate(coord, &active.world)?;
        let bytes = Arc::new(encode_columns(
            &chunk,
            &active.pipeline.schema().column_fields(),
            active.revision,
        ));
        self.lock_cache().insert(key, Arc::clone(&bytes));

        Ok(bytes)
    }

    pub fn cache_stats(&self) -> CacheStats {
        let cache = self.lock_cache();

        CacheStats {
            entries: cache.len(),
            used_bytes: cache.used_bytes(),
            hits: cache.hits,
            misses: cache.misses,
        }
    }

    fn lock_cache(&self) -> std::sync::MutexGuard<'_, PayloadCache> {
        // A panic while holding the lock cannot leave the cache inconsistent
        // in a way that matters for a debug cache; recover instead of failing.
        self.cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub fn world_info(&self) -> WorldInfo {
        let active = self.active();
        let bounds = active.config.world();

        WorldInfo {
            origin: bounds.origin(),
            dimensions: bounds.dimensions(),
            chunk_size: active.config.chunk_size().dimensions(),
            seed: active.config.seed(),
            revision: active.revision,
            column_fields: active.pipeline.schema().column_fields(),
            voxel_fields: active.pipeline.schema().voxel_fields(),
            // The outputs of world-scope nodes, in the order world payloads
            // carry their planes.
            world_fields: active.pipeline.schema().world_fields(),
            exports: active.pipeline.exports().to_vec(),
            world_stats: active.stats,
            world_generation: active.world_generation,
            materials: active
                .world
                .materials()
                .defs()
                .iter()
                .map(|m| PaletteEntry {
                    name: m.name.clone(),
                    label: m.name.replace('_', " "),
                    color: m.color,
                    detail: format!("{:?}", m.category).to_lowercase(),
                })
                .collect(),
            biomes: active
                .world
                .biomes()
                .defs()
                .iter()
                .map(|b| PaletteEntry {
                    name: b.name.clone(),
                    label: b.label.clone(),
                    color: b.color,
                    detail: b.glyph.clone(),
                })
                .collect(),
            atlas: {
                let atlas = active.world.atlas();
                AtlasInfo {
                    pending: active.world.is_placeholder(),
                    grid: atlas.context().grid,
                    cell_size: atlas.context().cell_size,
                    fields: active.pipeline.schema().atlas_fields(),
                    snapshots: atlas
                        .snapshots()
                        .iter()
                        .map(|s| SnapshotInfo {
                            node: s.node.clone(),
                            label: s.label.clone(),
                        })
                        .collect(),
                }
            },
        }
    }

    /// Mean per-node generation time for the current revision.
    pub fn step_timings(&self) -> (u64, Vec<StepTiming>) {
        let active = self.active();
        (active.revision, active.pipeline.timings())
    }

    pub fn pipeline_graph(&self) -> PipelineGraph {
        let state = self.state.read().unwrap_or_else(|p| p.into_inner());
        let analysis = state.active.pipeline.analysis();

        // Layout comes from the live doc (moves do not publish revisions).
        let nodes = analysis
            .nodes
            .iter()
            .map(|report| {
                let mut report = report.clone();
                if let Some(node) = state.doc.node(&report.id) {
                    report.position = node.position;
                }
                report
            })
            .collect::<Vec<_>>();

        PipelineGraph {
            revision: state.active.revision,
            nodes,
            edges: analysis.edges.clone(),
            exports: analysis.exports.clone(),
            frames: state.doc.frames.clone(),
            kinds: steps::registry()
                .iter()
                .map(|kind| KindInfo {
                    id: kind.id,
                    label: kind.label,
                    description: kind.description,
                    scope: kind
                        .outputs
                        .first()
                        .map(|output| output.scope)
                        .or_else(|| kind.inputs.first().map(|input| input.scope))
                        .unwrap_or(FieldScope::Column),
                })
                .collect(),
        }
    }

    /// Adds a node (a built-in kind, or `script` with `source`). Each input
    /// auto-connects to the latest-ordered existing output of the same name
    /// and type; rewire freely afterwards. Returns the new node id.
    pub fn add_node(
        &self,
        kind: &str,
        position: (f64, f64),
        source: Option<String>,
    ) -> Result<(String, PipelineGraph), String> {
        if kind == SCRIPT_KIND {
            if let Err(error) = resolve(&NodeDoc {
                id: String::new(),
                kind: kind.to_owned(),
                enabled: true,
                position,
                params: Default::default(),
                script: source.clone(),
                label: None,
            })
            .contract
            {
                return Err(error);
            }
        } else if steps::find_kind(kind).is_none() {
            return Err(format!("Unknown node kind `{kind}`"));
        }

        let active = self.active();
        let mut id = String::new();

        let graph = self.edit(|doc| {
            id = doc.next_id();
            let node = NodeDoc {
                id: id.clone(),
                kind: kind.to_owned(),
                enabled: true,
                position,
                params: Default::default(),
                script: source,
                label: None,
            };

            if let Ok(contract) = resolve(&node).contract {
                for input in &contract.inputs {
                    if let Some(from) = latest_output(&active, &input.name, SocketType::new(input.scope, input.kind)) {
                        doc.edges.push(EdgeDoc {
                            from,
                            to: SocketRef {
                                node: id.clone(),
                                socket: input.name.to_string(),
                            },
                        });
                    }
                }
            }

            doc.nodes.push(node);
            Ok(())
        })?;

        Ok((id, graph))
    }

    pub fn remove_node(&self, id: &str) -> Result<PipelineGraph, String> {
        self.edit(|doc| {
            let before = doc.nodes.len();
            doc.nodes.retain(|node| node.id != id);
            if doc.nodes.len() == before {
                return Err(format!("Node `{id}` does not exist"));
            }
            doc.edges.retain(|e| e.from.node != id && e.to.node != id);
            Ok(())
        })
    }

    /// Removes nodes and/or frames in one undoable step (a multi-select
    /// delete). Removing only frames is layout: no new revision.
    pub fn remove_items(&self, ids: &[String]) -> Result<PipelineGraph, String> {
        let remove = |doc: &mut GraphDoc| -> Result<(), String> {
            let (nodes, frames) = (doc.nodes.len(), doc.frames.len());
            doc.nodes.retain(|node| !ids.contains(&node.id));
            doc.frames.retain(|frame| !ids.contains(&frame.id));
            if doc.nodes.len() == nodes && doc.frames.len() == frames {
                return Err("Nothing to remove".to_owned());
            }
            doc.edges
                .retain(|e| !ids.contains(&e.from.node) && !ids.contains(&e.to.node));
            Ok(())
        };

        if self
            .graph_doc()
            .nodes
            .iter()
            .any(|node| ids.contains(&node.id))
        {
            self.edit(remove)
        } else {
            self.layout_edit(remove)
        }
    }

    /// Splits the edge `from → to` with a new reroute node (of the edge's
    /// type) at `position`, as one undoable step.
    pub fn insert_reroute(
        &self,
        from: SocketRef,
        to: SocketRef,
        position: (f64, f64),
    ) -> Result<(String, PipelineGraph), String> {
        let mut id = String::new();

        let graph = self.edit(|doc| {
            let index = doc
                .edges
                .iter()
                .position(|e| e.from == from && e.to == to)
                .ok_or("That connection does not exist")?;

            let source = doc
                .node(&from.node)
                .ok_or_else(|| format!("Node `{}` does not exist", from.node))?;
            let scope = resolve(source)
                .contract?
                .outputs
                .iter()
                .find(|o| o.name == from.socket)
                .map(|o| o.scope)
                .ok_or_else(|| format!("Node `{}` has no output `{}`", from.node, from.socket))?;
            let kind = match scope {
                FieldScope::Column => "reroute_column",
                FieldScope::Voxel => "reroute_voxel",
                FieldScope::World => "reroute_world",
                FieldScope::Atlas => "reroute_atlas",
            };

            id = doc.next_id();
            doc.nodes.push(NodeDoc {
                id: id.clone(),
                kind: kind.to_owned(),
                enabled: true,
                position,
                params: Default::default(),
                script: None,
                label: None,
            });
            doc.edges[index] = EdgeDoc {
                from: from.clone(),
                to: SocketRef {
                    node: id.clone(),
                    socket: "in".to_owned(),
                },
            };
            doc.edges.push(EdgeDoc {
                from: SocketRef {
                    node: id.clone(),
                    socket: "out".to_owned(),
                },
                to: to.clone(),
            });
            Ok(())
        })?;

        Ok((id, graph))
    }

    /// Cuts several inputs' edges in one undoable step.
    pub fn disconnect_many(&self, inputs: &[SocketRef]) -> Result<PipelineGraph, String> {
        self.edit(|doc| {
            let before = doc.edges.len();
            doc.edges.retain(|e| !inputs.contains(&e.to));
            if doc.edges.len() == before {
                return Err("None of those inputs is connected".to_owned());
            }
            Ok(())
        })
    }

    /// Adds a frame (a labelled box behind nodes). Layout only.
    pub fn add_frame(
        &self,
        position: (f64, f64),
        size: (f64, f64),
    ) -> Result<(String, PipelineGraph), String> {
        let mut id = String::new();
        let graph = self.layout_edit(|doc| {
            id = doc.next_frame_id();
            doc.frames.push(FrameDoc {
                id: id.clone(),
                label: String::new(),
                position,
                size: clamp_frame_size(size),
            });
            Ok(())
        })?;

        Ok((id, graph))
    }

    /// Changes a frame's label, position and/or size. Layout only.
    pub fn update_frame(
        &self,
        id: &str,
        label: Option<String>,
        position: Option<(f64, f64)>,
        size: Option<(f64, f64)>,
    ) -> Result<PipelineGraph, String> {
        self.layout_edit(|doc| {
            let frame = doc
                .frame_mut(id)
                .ok_or_else(|| format!("Frame `{id}` does not exist"))?;
            if let Some(label) = label {
                frame.label = label.trim().to_owned();
            }
            if let Some(position) = position {
                frame.position = position;
            }
            if let Some(size) = size {
                frame.size = clamp_frame_size(size);
            }
            Ok(())
        })
    }

    pub fn set_enabled(&self, id: &str, enabled: bool) -> Result<PipelineGraph, String> {
        self.edit(|doc| {
            doc.node_mut(id)
                .ok_or_else(|| format!("Node `{id}` does not exist"))?
                .enabled = enabled;
            Ok(())
        })
    }

    /// Sets a node's title (for Export nodes: the export name). Empty clears it.
    pub fn set_label(&self, id: &str, label: &str) -> Result<PipelineGraph, String> {
        let label = label.trim();

        self.edit(|doc| {
            doc.node_mut(id)
                .ok_or_else(|| format!("Node `{id}` does not exist"))?
                .label = (!label.is_empty()).then(|| label.to_owned());
            Ok(())
        })
    }

    pub fn set_param(&self, id: &str, name: &str, value: f64) -> Result<PipelineGraph, String> {
        if !value.is_finite() {
            return Err(format!("Param `{name}` must be a finite number"));
        }

        self.edit(|doc| {
            let node = doc
                .node_mut(id)
                .ok_or_else(|| format!("Node `{id}` does not exist"))?;
            let known = resolve(node)
                .contract
                .is_ok_and(|c| c.params.iter().any(|p| p.name == name));
            if !known {
                return Err(format!("Node `{id}` has no param `{name}`"));
            }
            node.params.insert(name.to_owned(), value);
            Ok(())
        })
    }

    /// Connects an output to an input (replacing the input's current edge).
    /// Rejects unknown sockets, type mismatches and cycles.
    pub fn connect(&self, from: SocketRef, to: SocketRef) -> Result<PipelineGraph, String> {
        self.edit(|doc| {
            let socket_scope =
                |node: &str, socket: &str, output: bool| -> Result<SocketType, String> {
                    let node_doc = doc
                        .node(node)
                        .ok_or_else(|| format!("Node `{node}` does not exist"))?;
                    let contract = resolve(node_doc).contract?;
                    let scope = if output {
                        contract
                            .outputs
                            .iter()
                            .find(|o| o.name == socket)
                            .map(|o| o.socket_type())
                    } else {
                        contract
                            .inputs
                            .iter()
                            .find(|i| i.name == socket)
                            .map(|i| i.socket_type())
                    };
                    scope.ok_or_else(|| {
                        let side = if output { "output" } else { "input" };
                        format!("Node `{node}` has no {side} `{socket}`")
                    })
                };

            let out_scope = socket_scope(&from.node, &from.socket, true)?;
            let in_scope = socket_scope(&to.node, &to.socket, false)?;

            if !can_connect(out_scope, in_scope) {
                return Err(format!(
                    "Cannot connect a {out_scope:?} output to a {in_scope:?} input"
                ));
            }

            doc.edges.retain(|e| e.to != to);

            if creates_cycle(doc, &from.node, &to.node) {
                return Err("That connection would create a cycle".to_owned());
            }

            doc.edges.push(EdgeDoc { from, to });
            Ok(())
        })
    }

    pub fn disconnect(&self, to: SocketRef) -> Result<PipelineGraph, String> {
        self.edit(|doc| {
            let before = doc.edges.len();
            doc.edges.retain(|e| e.to != to);
            if doc.edges.len() == before {
                return Err("That input is not connected".to_owned());
            }
            Ok(())
        })
    }

    /// Layout only: updates positions without publishing a revision (undoable).
    pub fn move_nodes(&self, moves: &[(String, (f64, f64))]) {
        let mut state = self.state.write().unwrap_or_else(|p| p.into_inner());
        let before = state.doc.clone();
        for (id, position) in moves {
            if let Some(node) = state.doc.node_mut(id) {
                node.position = *position;
            } else if let Some(frame) = state.doc.frame_mut(id) {
                frame.position = *position;
            }
        }
        state.record(before);
    }

    /// Replaces the whole document (New / Open) and clears the history.
    /// Like loading at startup, the new graph is built but not validated, so
    /// a graph with a broken script still opens. On error nothing changes.
    pub fn replace_doc(&self, doc: GraphDoc) -> Result<PipelineGraph, String> {
        {
            let mut state = self.state.write().unwrap_or_else(|p| p.into_inner());
            let built = build(&doc, Some(&state.active), &self.atlas)?;
            state.publish(doc, built);
            state.undo.clear();
            state.redo.clear();
        }
        self.atlas.sync(&self.state);

        Ok(self.pipeline_graph())
    }

    pub fn can_undo(&self) -> bool {
        !self
            .state
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .undo
            .is_empty()
    }

    pub fn can_redo(&self) -> bool {
        !self
            .state
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .redo
            .is_empty()
    }

    /// Steps back to the previous doc snapshot (layout included).
    pub fn undo(&self) -> Result<PipelineGraph, String> {
        self.step_history(true)
    }

    pub fn redo(&self) -> Result<PipelineGraph, String> {
        self.step_history(false)
    }

    fn step_history(&self, undo: bool) -> Result<PipelineGraph, String> {
        {
            let mut state = self.state.write().unwrap_or_else(|p| p.into_inner());
            let target = if undo {
                state.undo.last()
            } else {
                state.redo.last()
            }
            .cloned()
            .ok_or_else(|| format!("Nothing to {}", if undo { "undo" } else { "redo" }))?;

            // Snapshots were valid when they were current; building suffices.
            let built = build(&target, Some(&state.active), &self.atlas)?;
            let current = state.doc.clone();

            if undo {
                state.undo.pop();
                state.redo.push(current);
            } else {
                state.redo.pop();
                state.undo.push(current);
            }
            state.publish(target, built);
        }
        self.atlas.sync(&self.state);

        Ok(self.pipeline_graph())
    }

    /// Replaces a script node's source. Edges stay connected to sockets that
    /// still exist with the same name; others are dropped.
    pub fn update_script(&self, id: &str, source: &str) -> Result<PipelineGraph, String> {
        self.edit(|doc| {
            let node = doc
                .node_mut(id)
                .ok_or_else(|| format!("Node `{id}` does not exist"))?;
            if node.kind != SCRIPT_KIND {
                return Err(format!("Node `{id}` is not a script"));
            }
            node.script = Some(source.to_owned());
            let contract = resolve(node).contract?;

            let has_input = |name: &str| contract.inputs.iter().any(|i| i.name == name);
            let has_output = |name: &str| contract.outputs.iter().any(|o| o.name == name);

            doc.edges.retain(|e| {
                (e.to.node != id || has_input(&e.to.socket))
                    && (e.from.node != id || has_output(&e.from.socket))
            });
            Ok(())
        })
    }

    pub fn script_source(&self, id: &str) -> Result<String, String> {
        self.graph_doc()
            .node(id)
            .filter(|node| node.kind == SCRIPT_KIND)
            .and_then(|node| node.script.clone())
            .ok_or_else(|| format!("Node `{id}` is not a script"))
    }

    /// Applies an edit to a copy of the doc and publishes it as a new
    /// revision after checking that it generates a chunk without errors.
    fn edit(
        &self,
        change: impl FnOnce(&mut GraphDoc) -> Result<(), String>,
    ) -> Result<PipelineGraph, String> {
        {
            let mut state = self.state.write().unwrap_or_else(|p| p.into_inner());
            let mut doc = state.doc.clone();
            change(&mut doc)?;

            let built = build(&doc, Some(&state.active), &self.atlas)?;
            built.pipeline.validate(built.config, &built.world)?;
            let previous = std::mem::replace(&mut state.doc, doc.clone());
            state.record(previous);
            state.publish(doc, built);
        }
        self.atlas.sync(&self.state);

        Ok(self.pipeline_graph())
    }

    /// Like `edit` for changes that cannot affect generation (frames): no
    /// validation, no new revision, but undoable.
    fn layout_edit(
        &self,
        change: impl FnOnce(&mut GraphDoc) -> Result<(), String>,
    ) -> Result<PipelineGraph, String> {
        {
            let mut state = self.state.write().unwrap_or_else(|p| p.into_inner());
            let mut doc = state.doc.clone();
            change(&mut doc)?;
            let previous = std::mem::replace(&mut state.doc, doc);
            state.record(previous);
        }

        Ok(self.pipeline_graph())
    }

    /// Sets the world size, depth and atlas cell size (undoable). A new
    /// world: the atlas, if any, is rebuilt in the background.
    pub fn set_extent(&self, extent: Extent) -> Result<PipelineGraph, String> {
        extent.validate()?;
        self.edit(|doc| {
            doc.extent = Some(extent);
            Ok(())
        })
    }

    /// Sets the world seed (undoable). Builds a new world stage; the graph
    /// must still generate under the new seed.
    pub fn set_seed(&self, seed: u64) -> Result<PipelineGraph, String> {
        self.edit(|doc| {
            doc.seed = seed;
            Ok(())
        })
    }

    /// Samples every world field on a regular grid: sample (i, j) is at
    /// (x0 + i * step, y0 + j * step). Outside-world samples are NaN.
    pub fn world_payload(
        &self,
        origin: (i64, i64),
        step: u32,
        width: u32,
        height: u32,
    ) -> Result<Vec<u8>, String> {
        if step == 0 || width == 0 || height == 0 {
            return Err("World sample grid must be nonempty with a positive step".to_owned());
        }

        if u64::from(width) * u64::from(height) > MAX_WORLD_SAMPLES {
            return Err(format!(
                "World sample grid {width}×{height} exceeds {MAX_WORLD_SAMPLES} samples"
            ));
        }

        let (w, h) = (width as usize, height as usize);
        let step_cells = i64::from(step);
        let active = self.ready_active()?;
        let world = &*active.world;

        // One plane per world-node output.
        let slots: Vec<usize> = active
            .pipeline
            .schema()
            .world_fields()
            .iter()
            .map(|field| field.slot)
            .collect();

        let rows: Vec<Vec<Vec<f32>>> = (0..h)
            .into_par_iter()
            .map(|j| {
                let y = origin.1 + j as i64 * step_cells;

                (0..w)
                    .map(|i| {
                        let x = origin.0 + i as i64 * step_cells;

                        if !world.contains_column(x, y) {
                            return vec![f32::NAN; slots.len()];
                        }

                        let values = world.graph().eval(x, y);
                        slots.iter().map(|&slot| values[slot] as f32).collect()
                    })
                    .collect()
            })
            .collect();

        let planes: Vec<Vec<f32>> = (0..slots.len())
            .map(|field| rows.iter().flatten().map(|values| values[field]).collect())
            .collect();

        Ok(encode_planes(origin, step, width, height, 0, &planes))
    }

    /// All world fields at one column, in `WorldInfo::world_fields` order, or
    /// None outside the world.
    pub fn world_point(&self, x: i64, y: i64) -> Option<Vec<f64>> {
        let active = self.ready_active().ok()?;
        if !active.world.contains_column(x, y) {
            return None;
        }

        let values = active.world.graph().eval(x, y);
        Some(
            active
                .pipeline
                .schema()
                .world_fields()
                .iter()
                .map(|field| values[field.slot])
                .collect(),
        )
    }

    #[cfg(test)]
    fn generate(&self, coord: ChunkCoord) -> Result<Chunk<Voxel, ColumnData>, String> {
        let active = self.active();
        active.pipeline.generate(coord, &active.world)
    }

    #[cfg(test)]
    fn world(&self) -> Arc<WorldStore> {
        Arc::clone(&self.active().world)
    }

    #[cfg(test)]
    fn slot(&self, key: &str) -> usize {
        self.active().pipeline.schema().slot(key).unwrap()
    }
}

fn clamp_frame_size((width, height): (f64, f64)) -> (f64, f64) {
    const MIN: f64 = 60.0;
    (
        if width.is_finite() {
            width.max(MIN)
        } else {
            MIN
        },
        if height.is_finite() {
            height.max(MIN)
        } else {
            MIN
        },
    )
}

/// Latest-ordered active output with this name and type (for auto-connect),
/// falling back to any node declaring it.
fn latest_output(active: &Active, name: &str, scope: SocketType) -> Option<SocketRef> {
    let mut candidates: Vec<(Option<usize>, &NodeReport)> = active
        .pipeline
        .analysis()
        .nodes
        .iter()
        .filter(|node| {
            node.outputs
                .iter()
                .any(|o| o.name == name && can_connect(SocketType::new(o.scope, o.kind), scope))
        })
        .map(|node| {
            (
                node.order.filter(|_| node.status == NodeStatus::Active),
                node,
            )
        })
        .collect();

    candidates.sort_by(|a, b| {
        a.0.cmp(&b.0)
            .then_with(|| id_order(&a.1.id).cmp(&id_order(&b.1.id)))
    });

    candidates.last().map(|(_, node)| SocketRef {
        node: node.id.clone(),
        socket: name.to_owned(),
    })
}

pub const PAYLOAD_VERSION: u32 = 2;
pub const PAYLOAD_HEADER_BYTES: usize = 40;

/// Binary field-plane payload (little-endian), shared by chunk columns and
/// world samples:
///
/// ```text
/// u32 version (2) | u32 width | u32 height | u32 field_count
/// i64 origin_x    | i64 origin_y
/// u32 step (cells per sample) | u32 revision (low 32 bits; 0 for world grids)
/// field_count × (width * height) f32, row-major (i + j * width)
/// ```
fn encode_planes(
    origin: (i64, i64),
    step: u32,
    width: u32,
    height: u32,
    revision: u64,
    planes: &[Vec<f32>],
) -> Vec<u8> {
    let plane_len = width as usize * height as usize;
    let mut out = Vec::with_capacity(PAYLOAD_HEADER_BYTES + planes.len() * plane_len * 4);

    out.extend_from_slice(&PAYLOAD_VERSION.to_le_bytes());
    out.extend_from_slice(&width.to_le_bytes());
    out.extend_from_slice(&height.to_le_bytes());
    out.extend_from_slice(&(planes.len() as u32).to_le_bytes());
    out.extend_from_slice(&origin.0.to_le_bytes());
    out.extend_from_slice(&origin.1.to_le_bytes());
    out.extend_from_slice(&step.to_le_bytes());
    out.extend_from_slice(&(revision as u32).to_le_bytes());

    for plane in planes {
        debug_assert_eq!(plane.len(), plane_len);
        for value in plane {
            out.extend_from_slice(&value.to_le_bytes());
        }
    }

    out
}

/// Chunk column fields in schema (`WorldInfo::column_fields`) order, step 1.
fn encode_columns(
    chunk: &layergen::Chunk<Voxel, ColumnData>,
    fields: &[FieldDef],
    revision: u64,
) -> Vec<u8> {
    let columns = &chunk.chunk_data().columns;
    let (width, height, _) = columns.dimensions();
    let origin = chunk.origin();

    let planes: Vec<Vec<f32>> = fields
        .iter()
        .map(|field| {
            columns
                .as_slice()
                .iter()
                .map(|column| column[field.slot])
                .collect()
        })
        .collect();

    encode_planes(
        (origin.0, origin.1),
        1,
        width as u32,
        height as u32,
        revision,
        &planes,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    // Default graph: n1 tectonics, n2 tectonic_elevation, n3 temperature (all
    // world nodes), n4 solid, n5 export `height`. Chunk and voxel nodes read
    // world values through implicit sampled column slots, so the default
    // graph has no chunk column fields: add a chunk node (slope, smooth, …)
    // to get one.

    fn socket(node: &str, socket: &str) -> SocketRef {
        SocketRef {
            node: node.into(),
            socket: socket.into(),
        }
    }

    fn node<'a>(graph: &'a PipelineGraph, id: &str) -> &'a NodeReport {
        graph.nodes.iter().find(|n| n.id == id).unwrap()
    }

    /// World elevation at a column, as the chunk nodes see it (f32).
    fn world_elevation(session: &Session, x: i64, y: i64) -> f32 {
        let active = session.active();
        let slot = active.pipeline.elevation_world_slot().unwrap();
        active.world.graph().eval(x, y)[slot] as f32
    }

    /// Column slot holding the sampled copy of the world elevation.
    fn sampled_elevation(session: &Session) -> usize {
        let active = session.active();
        let world_slot = active.pipeline.elevation_world_slot().unwrap();
        active
            .pipeline
            .analysis()
            .world_samples
            .iter()
            .find(|binding| binding.world_slot == world_slot)
            .expect("a chunk node reads the elevation")
            .column_slot
    }

    #[test]
    fn generates_deterministic_column_payload() {
        let session = Session::new(42).unwrap();
        session.add_node("slope", (0.0, 0.0), None).unwrap(); // a chunk node: n6
        let fields = session.active().pipeline.schema().column_fields();
        let coord = ChunkCoord(128, 128, 0);
        let first = encode_columns(&session.generate(coord).unwrap(), &fields, 0);
        let again = encode_columns(&session.generate(coord).unwrap(), &fields, 0);

        assert_eq!(first, again);
        assert_eq!(first.len(), PAYLOAD_HEADER_BYTES + 32 * 32 * 4);
        assert_eq!(&first[0..4], &PAYLOAD_VERSION.to_le_bytes());
        assert_eq!(i64::from_le_bytes(first[16..24].try_into().unwrap()), 0);
        assert_eq!(u32::from_le_bytes(first[32..36].try_into().unwrap()), 1);

        // Elevation and climate are world values: the chunk column fields are
        // what chunk nodes compute.
        let keys: Vec<_> = fields.iter().map(|f| f.key.as_str()).collect();
        assert_eq!(keys, ["n6.slope"]);
    }

    #[test]
    fn world_samples_match_chunk_columns_at_the_same_positions() {
        let session = Session::new(5).unwrap();
        let chunk = session.generate(ChunkCoord(130, 127, 0)).unwrap();
        let origin = chunk.origin();
        let payload = session
            .world_payload((origin.0, origin.1), 1, 32, 32)
            .unwrap();

        let info = session.world_info();
        let keys: Vec<_> = info.world_fields.iter().map(|f| f.key.as_str()).collect();
        assert_eq!(
            keys,
            [
                "n1.plate",
                "n1.boundary_distance",
                "n1.convergence",
                "n1.continental",
                "n1.neighbor_continental",
                "n2.elevation",
                "n3.temperature"
            ]
        );
        let elevation_plane = keys.iter().position(|k| *k == "n2.elevation").unwrap();
        let plane = PAYLOAD_HEADER_BYTES + elevation_plane * 32 * 32 * 4;
        let sampled = sampled_elevation(&session);

        for (column, x, y, _) in chunk.chunk_data().columns.iter() {
            let offset = plane + (x + y * 32) * 4;
            let world = f32::from_le_bytes(payload[offset..offset + 4].try_into().unwrap());

            assert_eq!(world, column[sampled]);
        }
    }

    #[test]
    fn world_payload_marks_outside_samples_and_limits_size() {
        let session = Session::new(5).unwrap();
        let payload = session.world_payload((-4097, 0), 1, 2, 1).unwrap();
        let outside = f32::from_le_bytes(payload[40..44].try_into().unwrap());
        let inside = f32::from_le_bytes(payload[44..48].try_into().unwrap());

        assert!(outside.is_nan());
        assert!(!inside.is_nan());
        assert!(session.world_payload((0, 0), 1, 2048, 2048).is_err());
        assert!(session.world_point(-5000, 0).is_none());

        // The hover values follow the world field order.
        let values = session.world_point(0, 0).unwrap();
        assert_eq!(values.len(), session.world_info().world_fields.len());
        assert_eq!(values[5] as f32, world_elevation(&session, 0, 0));
    }

    #[test]
    fn column_payloads_are_cached() {
        let session = Session::new(42).unwrap();
        let coord = ChunkCoord(128, 128, 0);

        let first = session.column_payload(coord).unwrap();
        let second = session.column_payload(coord).unwrap();

        assert!(Arc::ptr_eq(&first, &second));

        let stats = session.cache_stats();
        assert_eq!((stats.hits, stats.misses, stats.entries), (1, 1, 1));
    }

    #[test]
    fn solid_cells_follow_column_elevation() {
        let session = Session::new(7).unwrap();
        let chunk = session.generate(ChunkCoord(128, 128, 0)).unwrap();
        let sampled = sampled_elevation(&session);

        for (voxel, x, y, z) in chunk.data().iter() {
            let elevation = chunk.chunk_data().columns[(x, y, 0)][sampled];
            let expected = if (z as f32) < elevation { 1.0 } else { 0.0 };

            assert_eq!(voxel.slots[session.slot("n4.solid")], expected);
        }
    }

    #[test]
    fn column_values_are_seamless_across_chunk_borders() {
        let session = Session::new(3).unwrap();
        let left = session.generate(ChunkCoord(10, 10, 0)).unwrap();
        let right = session.generate(ChunkCoord(11, 10, 0)).unwrap();
        let sampled = sampled_elevation(&session);

        let left_edge = left.chunk_data().columns[(31, 5, 0)][sampled];
        let right_edge = right.chunk_data().columns[(0, 5, 0)][sampled];
        let (lx, ly) = (left.origin().0 + 31, left.origin().1 + 5);

        assert_eq!(left_edge, world_elevation(&session, lx, ly));
        assert_eq!(right_edge, world_elevation(&session, lx + 1, ly));
    }

    #[test]
    fn adding_nodes_auto_connects_by_name_and_publishes_revisions() {
        let session = Session::new(1).unwrap();
        let (id, graph) = session.add_node("slope", (600.0, 0.0), None).unwrap();

        assert_eq!(id, "n6");
        assert_eq!(graph.revision, 1);
        assert_eq!(node(&graph, "n6").status, NodeStatus::Active);
        // A column input picks up the world output of the same name.
        assert!(graph
            .edges
            .iter()
            .any(|e| e.from == socket("n2", "elevation") && e.to == socket("n6", "elevation")));

        let keys: Vec<_> = session
            .world_info()
            .column_fields
            .iter()
            .map(|f| f.key.clone())
            .collect();
        assert_eq!(keys, ["n6.slope"]);

        // World nodes auto-connect to world outputs of the same name.
        let (humidity, graph) = session.add_node("humidity", (600.0, 0.0), None).unwrap();
        assert!(graph
            .edges
            .iter()
            .any(|e| e.from == socket("n3", "temperature")
                && e.to == socket(&humidity, "temperature")));
        assert_eq!(node(&graph, &humidity).scope, FieldScope::World);
        session.undo().unwrap();

        // Disabling the elevation blocks everything that reads it.
        let graph = session.set_enabled("n2", false).unwrap();
        assert!(["n3", "n4", "n5", "n6"]
            .iter()
            .all(|id| node(&graph, id).status == NodeStatus::Blocked));
        assert!(session.world_info().column_fields.is_empty());

        // Removing a node removes its edges.
        let graph = session.remove_node("n3").unwrap();
        assert!(graph
            .edges
            .iter()
            .all(|e| e.from.node != "n3" && e.to.node != "n3"));
        assert_eq!(graph.revision, 5);
    }

    #[test]
    fn connect_rejects_mismatched_types_unknown_sockets_and_cycles() {
        let session = Session::new(1).unwrap();
        session.add_node("smooth", (0.0, 0.0), None).unwrap(); // n6, auto: none named "value"
        session.add_node("smooth", (0.0, 0.0), None).unwrap(); // n7

        let voxel_to_column = session.connect(socket("n4", "solid"), socket("n6", "value"));
        assert!(voxel_to_column.unwrap_err().contains("Cannot connect"));

        // Nothing flows from chunk scope into a world node.
        let chunk_to_world = session.connect(socket("n6", "smoothed"), socket("n2", "convergence"));
        assert!(chunk_to_world.unwrap_err().contains("Cannot connect"));

        assert!(session
            .connect(socket("n2", "nope"), socket("n6", "value"))
            .unwrap_err()
            .contains("no output"));

        // A world output feeds a chunk input.
        session
            .connect(socket("n2", "elevation"), socket("n6", "value"))
            .unwrap();
        session
            .connect(socket("n6", "smoothed"), socket("n7", "value"))
            .unwrap();
        assert!(session
            .connect(socket("n7", "smoothed"), socket("n6", "value"))
            .unwrap_err()
            .contains("cycle"));

        // Reconnecting an input replaces its edge.
        let graph = session
            .connect(socket("n6", "smoothed"), socket("n5", "value"))
            .unwrap();
        let into_n5: Vec<_> = graph.edges.iter().filter(|e| e.to.node == "n5").collect();
        assert_eq!(into_n5.len(), 1);
        assert_eq!(into_n5[0].from, socket("n6", "smoothed"));

        assert!(session.disconnect(socket("n5", "value")).is_ok());
        assert!(session.disconnect(socket("n5", "value")).is_err());
    }

    #[test]
    fn smooth_reprocesses_a_value_that_downstream_nodes_read() {
        let session = Session::new(8).unwrap();
        session.add_node("smooth", (0.0, 0.0), None).unwrap(); // n6
        session
            .connect(socket("n2", "elevation"), socket("n6", "value"))
            .unwrap();
        session.set_param("n6", "radius", 1.0).unwrap();

        let chunk = session.generate(ChunkCoord(140, 90, 0)).unwrap();
        let (origin, smoothed) = (chunk.origin(), session.slot("n6.smoothed"));
        let e = |x: i64, y: i64| f64::from(world_elevation(&session, x, y));

        for (x, y) in [(0usize, 0usize), (31, 31), (15, 0)] {
            let (gx, gy) = (origin.0 + x as i64, origin.1 + y as i64);
            let mut total = 0.0;
            for dy in -1..=1 {
                for dx in -1..=1 {
                    total += e(gx + dx, gy + dy);
                }
            }
            let expected = (total / 9.0) as f32;
            let column = chunk.chunk_data().columns[(x, y, 0)];

            assert_eq!(column[smoothed], expected);
            assert_ne!(column[smoothed], column[sampled_elevation(&session)]);
        }

        let graph = session.pipeline_graph();
        assert_eq!(node(&graph, "n6").neighborhood, Some((1, 1, 0)));
        assert!(session.set_param("n6", "bogus", 1.0).is_err());
    }

    #[test]
    fn slope_reads_across_chunk_borders_exactly() {
        let session = Session::new(6).unwrap();
        let (id, graph) = session.add_node("slope", (0.0, 0.0), None).unwrap();
        assert_eq!(node(&graph, &id).status, NodeStatus::Active);

        let chunk = session.generate(ChunkCoord(140, 90, 0)).unwrap();
        let (origin, out) = (chunk.origin(), session.slot(&format!("{id}.slope")));
        let e = |x: i64, y: i64| world_elevation(&session, x, y);

        for (x, y) in [
            (0usize, 0usize),
            (31, 31),
            (0, 17),
            (31, 3),
            (12, 0),
            (5, 31),
        ] {
            let (gx, gy) = (origin.0 + x as i64, origin.1 + y as i64);
            let dx = (e(gx + 1, gy) - e(gx - 1, gy)) / 2.0;
            let dy = (e(gx, gy + 1) - e(gx, gy - 1)) / 2.0;

            assert_eq!(
                chunk.chunk_data().columns[(x, y, 0)][out],
                (dx * dx + dy * dy).sqrt()
            );
        }
    }

    #[test]
    fn neighborhood_prefix_results_are_reused_between_requests() {
        let session = Session::new(6).unwrap();
        session.add_node("slope", (0.0, 0.0), None).unwrap();

        // The world sampling runs with the prefix: once per dependency chunk.
        let sampling_runs = || {
            session
                .step_timings()
                .1
                .iter()
                .find(|t| t.id == "world")
                .unwrap()
                .runs
        };

        let before = sampling_runs();
        session.column_payload(ChunkCoord(60, 60, 0)).unwrap();
        assert_eq!(sampling_runs() - before, 9);

        session.column_payload(ChunkCoord(61, 60, 0)).unwrap();
        assert_eq!(sampling_runs() - before, 12);
    }

    #[test]
    fn script_nodes_auto_connect_and_update_keeps_matching_edges() {
        let session = Session::new(4).unwrap();
        let (id, graph) = session
            .add_node(
                SCRIPT_KIND,
                (0.0, 0.0),
                Some(worldgraph::script::TEMPLATE.into()),
            )
            .unwrap();

        let script = node(&graph, &id);
        assert!(script.script);
        assert_eq!(script.status, NodeStatus::Active);
        assert!(graph
            .edges
            .iter()
            .any(|e| e.from == socket("n3", "temperature") && e.to == socket(&id, "temperature")));
        // World values wire into script inputs by name, too.
        assert!(graph
            .edges
            .iter()
            .any(|e| e.from == socket("n1", "boundary_distance")
                && e.to == socket(&id, "boundary_distance")));

        let moisture = session.slot(&format!("{id}.moisture"));
        let chunk = session.generate(ChunkCoord(128, 128, 0)).unwrap();
        assert!(chunk
            .chunk_data()
            .columns
            .iter()
            .all(|(c, ..)| (0.0..=1.0).contains(&c[moisture])));

        // Dropping inputs removes only their edges.
        let edited = worldgraph::script::TEMPLATE
            .replace(
                "input: elevation, boundary_distance, temperature",
                "input: elevation",
            )
            .replace("this.boundary_distance", "0.0")
            .replace("max(this.temperature - 20.0, 0.0)", "0.0");
        let graph = session.update_script(&id, &edited).unwrap();
        let into: Vec<_> = graph.edges.iter().filter(|e| e.to.node == id).collect();
        assert_eq!(into.len(), 1);
        assert_eq!(into[0].to.socket, "elevation");
        assert_eq!(session.script_source(&id).unwrap(), edited);
        assert!(session.script_source("n1").is_err());
    }

    #[test]
    fn broken_scripts_are_rejected_without_changing_the_graph() {
        let session = Session::new(4).unwrap();
        let before = session.pipeline_graph().revision;

        // Reads an input it never declared (gets unit → arithmetic error).
        let undeclared =
            "//! name: Bad\n//! output: bad\nfn column() { this.bad = this.elevation * 2.0; }";
        assert!(session
            .add_node(SCRIPT_KIND, (0.0, 0.0), Some(undeclared.into()))
            .is_err());

        let silent = "//! name: Silent\n//! output: quiet\nfn column() { }";
        assert!(session
            .add_node(SCRIPT_KIND, (0.0, 0.0), Some(silent.into()))
            .unwrap_err()
            .contains("did not assign `quiet`"));

        let spin = "//! name: Spin\n//! output: s\nfn column() { loop { } }";
        assert!(session
            .add_node(SCRIPT_KIND, (0.0, 0.0), Some(spin.into()))
            .is_err());

        assert!(session
            .add_node(SCRIPT_KIND, (0.0, 0.0), Some("not a script".into()))
            .is_err());

        assert_eq!(session.pipeline_graph().revision, before);
        assert_eq!(session.graph_doc().nodes.len(), 5);
    }

    #[test]
    fn moving_nodes_changes_layout_only_and_docs_round_trip() {
        let session = Session::new(2).unwrap();
        session.add_node("humidity", (0.0, 0.0), None).unwrap();
        let revision = session.pipeline_graph().revision;

        session.move_nodes(&[("n6".into(), (123.0, 45.0))]);
        let graph = session.pipeline_graph();
        assert_eq!(graph.revision, revision);
        assert_eq!(node(&graph, "n6").position, (123.0, 45.0));

        let saved = session.graph_doc().to_json();
        let reloaded = Session::with_doc(GraphDoc::from_json(&saved).unwrap()).unwrap();
        let again = reloaded.pipeline_graph();

        assert_eq!(reloaded.graph_doc(), session.graph_doc());
        assert_eq!(node(&again, "n6").position, (123.0, 45.0));
        assert_eq!(node(&again, "n6").status, NodeStatus::Active);
        assert_eq!(
            reloaded.world_info().column_fields.len(),
            session.world_info().column_fields.len()
        );
    }

    #[test]
    fn step_timings_count_generated_chunks_only() {
        let session = Session::new(9).unwrap();
        let (revision, before) = session.step_timings();
        let ids: Vec<_> = before.iter().map(|t| t.id.as_str()).collect();

        assert_eq!(revision, 0);
        // World nodes first, then chunk nodes, then the world sampling.
        assert_eq!(ids, ["n1", "n2", "n3", "n4", "n5", "world"]);

        let coord = ChunkCoord(100, 100, 0);
        session.column_payload(coord).unwrap();
        session.column_payload(coord).unwrap(); // cache hit: nothing runs

        let (_, after) = session.step_timings();
        // World nodes run once per world and the export has no runtime work:
        // only the solid fill and the sampling run per chunk.
        for (old, new) in before.iter().zip(&after) {
            let per_chunk = ["n4", "world"].contains(&old.id.as_str());
            assert_eq!(new.runs, old.runs + u64::from(per_chunk), "{}", old.id);
        }
    }

    #[test]
    fn undo_and_redo_restore_docs_including_layout() {
        let session = Session::new(2).unwrap();
        let original = session.graph_doc();
        assert!(!session.can_undo());
        assert!(session.undo().is_err());

        session.add_node("humidity", (10.0, 10.0), None).unwrap();
        session.move_nodes(&[("n6".into(), (99.0, 77.0))]);
        session.set_label("n5", "terrain").unwrap();
        let edited = session.graph_doc();

        // Moving to where it already is records nothing.
        session.move_nodes(&[("n6".into(), (99.0, 77.0))]);

        let graph = session.undo().unwrap();
        assert_eq!(node(&graph, "n5").label, "height");
        assert_eq!(node(&graph, "n6").position, (99.0, 77.0));
        let graph = session.undo().unwrap();
        assert_eq!(node(&graph, "n6").position, (10.0, 10.0));
        session.undo().unwrap();
        assert_eq!(session.graph_doc(), original);
        assert!(!session.can_undo());

        for _ in 0..3 {
            session.redo().unwrap();
        }
        assert_eq!(session.graph_doc(), edited);
        assert!(!session.can_redo());

        // A new edit drops the redo branch.
        session.undo().unwrap();
        session.set_enabled("n4", false).unwrap();
        assert!(!session.can_redo());
    }

    #[test]
    fn changing_the_seed_rebuilds_the_world_and_is_undoable() {
        let session = Session::new(10).unwrap();
        session.add_node("slope", (0.0, 0.0), None).unwrap(); // chunk output to compare
        let coord = ChunkCoord(128, 128, 0);
        let before = session.column_payload(coord).unwrap();
        let (elevation_a, revision) = (
            world_elevation(&session, 0, 0),
            session.pipeline_graph().revision,
        );

        let graph = session.set_seed(u64::MAX).unwrap();
        let info = session.world_info();

        assert_eq!(graph.revision, revision + 1);
        assert_eq!(info.seed, u64::MAX);
        assert_eq!(session.graph_doc().seed, u64::MAX);
        assert_ne!(world_elevation(&session, 0, 0), elevation_a);
        assert_ne!(session.column_payload(coord).unwrap()[40..], before[40..]);
        assert!(serde_json::to_string(&info)
            .unwrap()
            .contains("\"seed\":\"18446744073709551615\""));

        // The seed survives a save/load round trip.
        let reloaded =
            Session::with_doc(GraphDoc::from_json(&session.graph_doc().to_json()).unwrap())
                .unwrap();
        assert_eq!(reloaded.world_info().seed, u64::MAX);

        session.undo().unwrap();
        assert_eq!(session.world_info().seed, 10);
        assert_eq!(world_elevation(&session, 0, 0), elevation_a);
        assert_eq!(&session.column_payload(coord).unwrap()[40..], &before[40..]);

        // Setting the same seed again is a no-op for history.
        let undo_len_before = session.can_undo();
        session.set_seed(10).unwrap();
        assert_eq!(session.can_undo(), undo_len_before);
    }

    #[test]
    fn replacing_the_doc_clears_history_and_publishes_a_revision() {
        let session = Session::new(2).unwrap();
        session.add_node("humidity", (0.0, 0.0), None).unwrap();
        let before = session.pipeline_graph().revision;
        assert!(session.can_undo());

        let graph = session.replace_doc(steps::default_graph()).unwrap();

        assert_eq!(graph.revision, before + 1);
        assert_eq!(session.graph_doc(), steps::default_graph());
        assert!(!session.can_undo() && !session.can_redo());
    }

    #[test]
    fn default_graph_exports_height_and_labels_rename_exports() {
        let session = Session::new(3).unwrap();
        let info = session.world_info();

        assert_eq!(info.exports.len(), 1);
        assert_eq!(info.exports[0].name, "height");
        assert_eq!(info.exports[0].field, "n2.elevation");

        let graph = session.set_label("n5", "  terrain_height ").unwrap();
        assert_eq!(graph.exports[0].name, "terrain_height");
        assert_eq!(node(&graph, "n5").label, "terrain_height");

        // Clearing the label falls back to the default export name.
        let graph = session.set_label("n5", "").unwrap();
        assert_eq!(
            graph.exports[0].name,
            worldgraph::pipeline::DEFAULT_EXPORT_NAME
        );
        assert!(session.set_label("n99", "x").is_err());
    }

    #[test]
    fn frames_are_layout_only_undoable_and_move_with_nodes() {
        let session = Session::new(2).unwrap();
        let revision = session.pipeline_graph().revision;

        let (id, graph) = session.add_frame((10.0, 20.0), (300.0, 5.0)).unwrap();
        assert_eq!(id, "f1");
        assert_eq!(graph.revision, revision, "frames never publish a revision");
        assert_eq!(graph.frames[0].size, (300.0, 60.0), "size is clamped");

        let graph = session
            .update_frame("f1", Some("  Climate ".into()), None, Some((400.0, 250.0)))
            .unwrap();
        assert_eq!(graph.frames[0].label, "Climate");
        assert_eq!(graph.frames[0].size, (400.0, 250.0));
        assert!(session.update_frame("f9", None, None, None).is_err());

        // Frames move through the same call as nodes (one undo step).
        session.move_nodes(&[("f1".into(), (50.0, 60.0)), ("n1".into(), (5.0, 5.0))]);
        let graph = session.pipeline_graph();
        assert_eq!(graph.frames[0].position, (50.0, 60.0));
        assert_eq!(node(&graph, "n1").position, (5.0, 5.0));
        session.undo().unwrap();
        assert_eq!(session.pipeline_graph().frames[0].position, (10.0, 20.0));

        // Survives a save/load round trip.
        let saved = session.graph_doc().to_json();
        assert_eq!(GraphDoc::from_json(&saved).unwrap(), session.graph_doc());

        // Deleting nodes and a frame together is one undo step.
        let graph = session
            .remove_items(&["f1".into(), "n3".into(), "n4".into()])
            .unwrap();
        assert!(graph.frames.is_empty());
        assert!(graph.nodes.iter().all(|n| n.id != "n3" && n.id != "n4"));
        let graph = session.undo().unwrap();
        assert_eq!(graph.frames.len(), 1);
        assert!(graph.nodes.iter().any(|n| n.id == "n3"));
        assert!(graph.nodes.iter().any(|n| n.id == "n4"));

        // Frames-only removal is layout: no new revision.
        let revision = session.pipeline_graph().revision;
        let graph = session.remove_items(&["f1".into()]).unwrap();
        assert_eq!(graph.revision, revision);
        assert!(session.remove_items(&["f1".into()]).is_err());
    }

    #[test]
    fn disconnect_many_is_one_undo_step_and_reroutes_generate() {
        let session = Session::new(3).unwrap();
        let original = session.graph_doc();

        let graph = session
            .disconnect_many(&[socket("n3", "elevation"), socket("n4", "elevation")])
            .unwrap();
        assert!(graph
            .edges
            .iter()
            .all(|e| e.to.node != "n3" && e.to.node != "n4"));
        assert!(session
            .disconnect_many(&[socket("n3", "elevation")])
            .is_err());
        session.undo().unwrap();
        assert_eq!(session.graph_doc(), original);

        // Insert a world reroute between the elevation and temperature.
        let fields = session.world_info().world_fields.len();
        let (id, _) = session
            .add_node("reroute_world", (100.0, 0.0), None)
            .unwrap();
        session
            .connect(socket("n2", "elevation"), socket(&id, "in"))
            .unwrap();
        let graph = session
            .connect(socket(&id, "out"), socket("n3", "elevation"))
            .unwrap();
        assert_eq!(node(&graph, &id).status, NodeStatus::Active);
        assert_eq!(
            session.world_info().world_fields.len(),
            fields,
            "a reroute adds no field"
        );

        // Splitting a world wire adds a world reroute; undoes in one step.
        let before = session.graph_doc();
        let (split, graph) = session
            .insert_reroute(
                socket("n2", "elevation"),
                socket("n4", "elevation"),
                (50.0, 50.0),
            )
            .unwrap();
        assert_eq!(node(&graph, &split).kind, "reroute_world");
        assert_eq!(node(&graph, &split).status, NodeStatus::Active);
        assert!(graph
            .edges
            .iter()
            .any(|e| e.from == socket(&split, "out") && e.to == socket("n4", "elevation")));
        assert!(session
            .insert_reroute(
                socket("n2", "elevation"),
                socket("n4", "elevation"),
                (0.0, 0.0)
            )
            .is_err());
        session.undo().unwrap();
        assert_eq!(session.graph_doc(), before);

        // Voxel edges get a voxel reroute.
        let (export, _) = session.add_node("export_voxel", (0.0, 0.0), None).unwrap();
        session
            .connect(socket("n4", "solid"), socket(&export, "value"))
            .unwrap();
        let (voxel, graph) = session
            .insert_reroute(socket("n4", "solid"), socket(&export, "value"), (0.0, 0.0))
            .unwrap();
        assert_eq!(node(&graph, &voxel).kind, "reroute_voxel");
        assert_eq!(node(&graph, &voxel).status, NodeStatus::Active);
        assert_eq!(
            graph
                .exports
                .iter()
                .find(|e| e.name == "export")
                .unwrap()
                .field,
            "n4.solid"
        );

        // The reroute chain still gives finite temperatures and generates chunks.
        let temperature = session.slot("n3.temperature");
        let world = session.world();
        assert!(world.graph().eval(100, -300)[temperature].is_finite());
        session.generate(ChunkCoord(128, 128, 0)).unwrap();
    }

    #[test]
    fn world_nodes_extend_the_overview_and_rebuild_the_world_only_when_needed() {
        let session = Session::new(6).unwrap();
        let base = session.world_info();
        assert_eq!(base.world_fields.len(), 7);

        // A world node adds an overview field and a new world stage.
        let (noise, graph) = session.add_node("world_noise", (0.0, 0.0), None).unwrap();
        assert_eq!(node(&graph, &noise).scope, FieldScope::World);
        let info = session.world_info();
        assert_eq!(info.world_fields.len(), 8);
        assert_eq!(
            info.world_fields.last().unwrap().key,
            format!("{noise}.noise")
        );
        assert_eq!(info.world_generation, base.world_generation + 1);

        // The overview payload carries a plane per field, finite inside the world.
        let payload = session.world_payload((0, 0), 64, 4, 4).unwrap();
        let fields = u32::from_le_bytes(payload[12..16].try_into().unwrap()) as usize;
        assert_eq!(fields, info.world_fields.len());
        let last = PAYLOAD_HEADER_BYTES + (fields - 1) * 16 * 4;
        let value = f32::from_le_bytes(payload[last..last + 4].try_into().unwrap());
        assert!((0.0..=1.0).contains(&value));

        // Chunk-scope edits reuse the world stage; world-scope edits rebuild it.
        let generation = session.world_info().world_generation;
        let world = session.world();
        let (slope, _) = session.add_node("slope", (0.0, 0.0), None).unwrap();
        session.set_label("n5", "terrain").unwrap();
        assert!(Arc::ptr_eq(&world, &session.world()));
        assert_eq!(session.world_info().world_generation, generation);

        // Climate is world scope: changing it rebuilds the world.
        session.set_param("n3", "equator", 10.0).unwrap();
        assert!(!Arc::ptr_eq(&world, &session.world()));
        let generation = session.world_info().world_generation;
        let world = session.world();

        session.set_param(&noise, "wavelength", 128.0).unwrap();
        assert!(!Arc::ptr_eq(&world, &session.world()));
        assert_eq!(session.world_info().world_generation, generation + 1);

        // Wires: world → column connects (sampled per column), column → world does not.
        let (remap, _) = session.add_node("world_remap", (0.0, 0.0), None).unwrap();
        assert!(session
            .connect(socket(&noise, "noise"), socket(&remap, "value"))
            .is_ok());
        let graph = session
            .connect(socket(&remap, "remapped"), socket("n3", "elevation"))
            .unwrap();
        assert_eq!(node(&graph, "n3").status, NodeStatus::Active);
        assert!(session
            .connect(socket(&slope, "slope"), socket(&remap, "value"))
            .unwrap_err()
            .contains("Cannot connect"));
        session.generate(ChunkCoord(128, 128, 0)).unwrap();

        // Splitting a world wire adds a world reroute.
        let (reroute, graph) = session
            .insert_reroute(socket(&noise, "noise"), socket(&remap, "value"), (0.0, 0.0))
            .unwrap();
        assert_eq!(node(&graph, &reroute).kind, "reroute_world");
        assert_eq!(node(&graph, &reroute).status, NodeStatus::Active);

        // World nodes undo like any edit.
        while session.can_undo() {
            session.undo().unwrap();
        }
        assert_eq!(session.world_info().world_fields.len(), 7);
    }

    #[test]
    fn templates_open_as_worlds_with_ocean_edges_and_report_what_they_made() {
        let templates = templates();
        assert!(templates.iter().any(|t| t.id == "single_continent"));
        assert!(template_doc("nope").is_err());

        // The default world scatters land to the border; continents do not.
        let session = Session::new(5).unwrap();
        assert!(session.world_info().world_stats.edge_land > 0.1);

        session
            .replace_doc(template_doc("single_continent").unwrap())
            .unwrap();
        let stats = session.world_info().world_stats;
        assert_eq!(stats.edge_land, 0.0);
        assert_eq!(stats.landmasses, 1);
        assert!(stats.land_fraction > 0.05);

        // A template is a graph like any other: edits and undo work on it.
        let falloff = session
            .graph_doc()
            .nodes
            .iter()
            .find(|n| n.kind == "edge_falloff")
            .unwrap()
            .id
            .clone();
        session.set_param("n1", "continents", 0.0).unwrap();
        session
            .set_param("n1", "continental_fraction", 0.9)
            .unwrap();
        session.set_param(&falloff, "margin", 0.0).unwrap();
        session.set_param(&falloff, "falloff", 0.0).unwrap();
        session.set_param(&falloff, "noise", 0.0).unwrap();
        assert!(
            session.world_info().world_stats.edge_land > 0.0,
            "mostly land and no falloff: land reaches the edge"
        );
        for _ in 0..5 {
            session.undo().unwrap();
        }
        assert_eq!(session.world_info().world_stats.edge_land, 0.0);
    }

    #[test]
    fn old_format_files_open_through_the_migration() {
        let old = r#"{"format": 1, "seed": 5,
            "world": {"plate_size": 1100, "continents": 1, "edge_margin": 0.08, "edge_falloff": 0.14},
            "nodes": [{"id": "n1", "kind": "elevation"}, {"id": "n2", "kind": "temperature"}],
            "edges": [{"from": {"node": "n1", "socket": "elevation"}, "to": {"node": "n2", "socket": "elevation"}}]}"#;
        let session = Session::with_doc(GraphDoc::from_json(old).unwrap()).unwrap();

        assert_eq!(session.graph_doc().format, worldgraph::graph::FORMAT);
        let graph = session.pipeline_graph();
        assert!(graph.nodes.iter().all(|n| n.status == NodeStatus::Active));
        assert_eq!(session.world_info().world_stats.edge_land, 0.0);
        assert!(!session.graph_doc().to_json().contains("\"world\""));
    }

    /// The eroded template on a small world (128² atlas) with few iterations.
    fn eroded_session(iterations: f64) -> Session {
        let mut doc = steps::eroded_graph();
        doc.extent = Some(Extent {
            size: (2048, 2048),
            depth: 128,
            cell_size: 16,
        });
        doc.node_mut("n6")
            .unwrap()
            .params
            .insert("iterations".into(), iterations);
        Session::with_doc(doc).unwrap()
    }

    /// Waits until the published world is complete (or the build failed).
    fn wait_for_atlas(session: &Session) {
        let started = std::time::Instant::now();
        while session.world_info().atlas.pending {
            if let AtlasStatus::Failed { error } = session.atlas_status() {
                panic!("atlas build failed: {error}");
            }
            assert!(started.elapsed().as_secs() < 120, "atlas build timed out");
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    #[test]
    fn atlas_builds_in_the_background_and_publishes_a_revision() {
        let session = eroded_session(20.0);
        let info = session.world_info();
        assert!(info.atlas.pending, "the first revision has a placeholder atlas");
        assert_eq!(info.atlas.grid, (128, 128));
        assert_eq!(info.dimensions, (2048, 2048, 128));
        assert_eq!(
            session.column_payload(ChunkCoord(32, 32, 0)).unwrap_err(),
            ATLAS_PENDING
        );
        assert!(session.world_point(0, 0).is_none());

        wait_for_atlas(&session);
        let ready = session.world_info();
        assert_eq!(ready.revision, info.revision + 1);
        assert_eq!(ready.world_generation, info.world_generation + 1);
        assert_eq!(session.atlas_status(), AtlasStatus::Idle);
        assert!(ready.world_stats.land_fraction > 0.05);
        assert!(ready.atlas.snapshots.iter().any(|s| s.node == "n6"));
        session.column_payload(ChunkCoord(32, 32, 0)).unwrap();

        // Atlas payloads: one plane per grid, one sample per cell.
        let slot = ready
            .atlas
            .fields
            .iter()
            .find(|f| f.key == "n6.elevation")
            .unwrap()
            .slot;
        let bytes = session.atlas_payload(slot).unwrap();
        assert_eq!(bytes.len(), PAYLOAD_HEADER_BYTES + 128 * 128 * 4);
        session.atlas_snapshot_payload(0).unwrap();
        assert!(!session.atlas_rivers().unwrap()[0].rivers.is_empty());
    }

    #[test]
    fn newer_atlas_edits_cancel_older_builds_and_undo_reuses_finished_worlds() {
        let session = eroded_session(10.0);
        wait_for_atlas(&session);
        let first = session.world_info();

        // Two edits in a row: the first build is cancelled, the second wins.
        session.set_param("n6", "iterations", 400.0).unwrap();
        assert!(session.world_info().atlas.pending);
        session.set_param("n6", "strength", 0.2).unwrap();
        wait_for_atlas(&session);
        let doc = session.graph_doc();
        assert_eq!(doc.node("n6").unwrap().params["strength"], 0.2);
        let world = session.world();
        assert!(!world.is_placeholder());
        assert_eq!(world.signature(), world_signature(&doc));

        // Renaming the export keeps the finished world (no rebuild).
        let generation = session.world_info().world_generation;
        session.set_label("n12", "terrain_height").unwrap();
        assert!(!session.world_info().atlas.pending);
        assert_eq!(session.world_info().world_generation, generation);

        // Undo back to the first world: found among finished worlds.
        session.undo().unwrap();
        session.undo().unwrap();
        session.undo().unwrap();
        let info = session.world_info();
        assert!(!info.atlas.pending, "a recent world is reused, not rebuilt");
        assert_eq!(
            session.world().signature(),
            world_signature(&session.graph_doc())
        );
        assert_eq!(info.world_stats, first.world_stats);
    }

    #[test]
    fn extent_edits_resize_the_world() {
        let session = Session::new(3).unwrap();
        assert_eq!(session.world_info().dimensions, (8192, 8192, 128));
        session
            .set_extent(Extent {
                size: (4096, 2048),
                depth: 384,
                cell_size: 32,
            })
            .unwrap();
        let info = session.world_info();
        assert_eq!(info.dimensions, (4096, 2048, 384));
        assert_eq!(info.origin, (-2048, -1024, 0));
        assert_eq!(info.atlas.grid, (128, 64));
        assert!(session
            .set_extent(Extent {
                depth: 4,
                ..Extent::DEFAULT
            })
            .is_err());
        assert!(session.graph_doc().to_json().contains("\"extent\""));
    }
}
