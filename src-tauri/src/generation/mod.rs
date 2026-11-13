//! LayerVis editing session over a `worldgraph` graph.
//!
//! The session owns the graph document. Every edit is applied to a copy,
//! built with `worldgraph::GraphPipeline`, validated on one chunk and only
//! then published as a new revision. Graph semantics (format, node kinds,
//! analysis, building) live in the `worldgraph` crate shared with the game;
//! this module adds editor concerns: revisions, auto-connect, payload
//! encoding and caching.

mod cache;

use std::sync::{Arc, Mutex, RwLock};

use cache::{CacheKey, PayloadCache};
use layergen::{ChunkCoord, ChunkSize, PipelineConfig, WorldBounds};
use serde::Serialize;
use worldgraph::{
    creates_cycle, graph::id_order, pipeline::SCRIPT_KIND, resolve, steps, ColumnData, EdgeDoc,
    Export, FieldDef, FieldScope, GraphDoc, GraphPipeline, NodeDoc, NodeReport, NodeStatus,
    SocketRef, Voxel,
};

#[cfg(test)]
use layergen::Chunk;

pub use worldgraph::{EdgeReport, StepTiming, WorldSample, WorldStore};

use rayon::prelude::*;

/// Upper bound on samples in one world view request (~4 MB per field).
const MAX_WORLD_SAMPLES: u64 = 1024 * 1024;

/// Column payloads are a few KB per 32×32 chunk; this holds thousands.
const PAYLOAD_CACHE_BYTES: usize = 64 * 1024 * 1024;

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorldInfo {
    pub origin: (i64, i64, i64),
    pub dimensions: (u32, u32, u32),
    pub chunk_size: (u32, u32, u32),
    pub seed: u64,
    /// Pipeline revision this schema belongs to (also tagged in payloads).
    pub revision: u64,
    /// Column outputs in the order they appear in chunk payloads.
    pub column_fields: Vec<FieldDef>,
    pub voxel_fields: Vec<FieldDef>,
    /// World-model fields, in the order they appear in world payloads.
    pub world_fields: Vec<FieldDef>,
    /// Game-facing named outputs (Export nodes).
    pub exports: Vec<Export>,
    pub plate_count: usize,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct KindInfo {
    pub id: &'static str,
    pub label: &'static str,
    pub description: &'static str,
}

/// World model → node (display only: world models are always available).
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorldEdge {
    pub model: String,
    pub to: String,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PipelineGraph {
    pub revision: u64,
    pub nodes: Vec<NodeReport>,
    pub edges: Vec<EdgeReport>,
    pub world_edges: Vec<WorldEdge>,
    pub exports: Vec<Export>,
    /// Built-in kinds that can be added.
    pub kinds: Vec<KindInfo>,
    pub world_models: Vec<&'static str>,
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
    pipeline: GraphPipeline,
}

/// Mutable session state: the doc (incl. layout) and the published revision.
struct State {
    doc: GraphDoc,
    active: Arc<Active>,
}

pub struct Session {
    config: PipelineConfig,
    world: Arc<WorldStore>,
    state: RwLock<State>,
    cache: Mutex<PayloadCache>,
}

impl Session {
    pub fn new(seed: u64) -> Result<Self, String> {
        Self::with_doc(seed, steps::default_graph())
    }

    /// Loads a saved graph. Not validated by generation (a script that fails
    /// at runtime should still open, so it can be fixed in the editor).
    pub fn with_doc(seed: u64, doc: GraphDoc) -> Result<Self, String> {
        let config = PipelineConfig::new(
            WorldBounds::new((-4096, -4096, 0), (8192, 8192, 128))?,
            ChunkSize::new(32, 32, 32)?,
            seed,
        );

        // World stage runs once per session (seed) and serves every revision.
        let world = Arc::new(WorldStore::new(&config));
        let pipeline = GraphPipeline::build(&doc, config, &world)?;

        Ok(Self {
            config,
            world,
            state: RwLock::new(State {
                doc,
                active: Arc::new(Active {
                    revision: 0,
                    pipeline,
                }),
            }),
            cache: Mutex::new(PayloadCache::new(PAYLOAD_CACHE_BYTES)),
        })
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
        let active = self.active();
        let key = CacheKey {
            revision: active.revision,
            coord,
        };

        if let Some(bytes) = self.lock_cache().get(key) {
            return Ok(bytes);
        }

        let chunk = active.pipeline.generate(coord, &self.world)?;
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
        self.cache.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub fn world_info(&self) -> WorldInfo {
        let world = self.config.world();
        let active = self.active();

        WorldInfo {
            origin: world.origin(),
            dimensions: world.dimensions(),
            chunk_size: self.config.chunk_size().dimensions(),
            seed: self.config.seed(),
            revision: active.revision,
            column_fields: active.pipeline.schema().column_fields(),
            voxel_fields: active.pipeline.schema().voxel_fields(),
            world_fields: worldgraph::world_fields(),
            exports: active.pipeline.exports().to_vec(),
            plate_count: self.world.tectonics.plates().len(),
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

        let world_edges = nodes
            .iter()
            .flat_map(|node| {
                node.world_reads.iter().map(|model| WorldEdge {
                    model: model.clone(),
                    to: node.id.clone(),
                })
            })
            .collect();

        PipelineGraph {
            revision: state.active.revision,
            nodes,
            edges: analysis.edges.clone(),
            world_edges,
            exports: analysis.exports.clone(),
            kinds: steps::registry()
                .iter()
                .map(|kind| KindInfo {
                    id: kind.id,
                    label: kind.label,
                    description: kind.description,
                })
                .collect(),
            world_models: vec!["tectonics"],
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
                    if let Some(from) = latest_output(&active, &input.name, input.scope) {
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
            let socket_scope = |node: &str, socket: &str, output: bool| -> Result<FieldScope, String> {
                let node_doc = doc
                    .node(node)
                    .ok_or_else(|| format!("Node `{node}` does not exist"))?;
                let contract = resolve(node_doc).contract?;
                let scope = if output {
                    contract.outputs.iter().find(|o| o.name == socket).map(|o| o.scope)
                } else {
                    contract.inputs.iter().find(|i| i.name == socket).map(|i| i.scope)
                };
                scope.ok_or_else(|| {
                    let side = if output { "output" } else { "input" };
                    format!("Node `{node}` has no {side} `{socket}`")
                })
            };

            let out_scope = socket_scope(&from.node, &from.socket, true)?;
            let in_scope = socket_scope(&to.node, &to.socket, false)?;

            if out_scope != in_scope {
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

    /// Layout only: updates positions without publishing a revision.
    pub fn move_nodes(&self, moves: &[(String, (f64, f64))]) {
        let mut state = self.state.write().unwrap_or_else(|p| p.into_inner());
        for (id, position) in moves {
            if let Some(node) = state.doc.node_mut(id) {
                node.position = *position;
            }
        }
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

            let pipeline = GraphPipeline::build(&doc, self.config, &self.world)?;
            pipeline.validate(self.config, &self.world)?;
            state.doc = doc;
            state.active = Arc::new(Active {
                revision: state.active.revision + 1,
                pipeline,
            });
        }

        Ok(self.pipeline_graph())
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

        let rows: Vec<Vec<[f32; WorldSample::FIELD_COUNT]>> = (0..h)
            .into_par_iter()
            .map(|j| {
                let y = origin.1 + j as i64 * step_cells;

                (0..w)
                    .map(|i| {
                        let x = origin.0 + i as i64 * step_cells;

                        if self.world.contains_column(x, y) {
                            self.world.sample(x, y).values()
                        } else {
                            [f32::NAN; WorldSample::FIELD_COUNT]
                        }
                    })
                    .collect()
            })
            .collect();

        let planes: Vec<Vec<f32>> = (0..WorldSample::FIELD_COUNT)
            .map(|field| rows.iter().flatten().map(|values| values[field]).collect())
            .collect();

        Ok(encode_planes(origin, step, width, height, 0, &planes))
    }

    /// All world fields at one column, or None outside the world.
    pub fn world_point(&self, x: i64, y: i64) -> Option<WorldSample> {
        self.world
            .contains_column(x, y)
            .then(|| self.world.sample(x, y))
    }

    #[cfg(test)]
    fn generate(&self, coord: ChunkCoord) -> Result<Chunk<Voxel, ColumnData>, String> {
        self.active().pipeline.generate(coord, &self.world)
    }

    #[cfg(test)]
    fn slot(&self, key: &str) -> usize {
        self.active().pipeline.schema().slot(key).unwrap()
    }
}

/// Latest-ordered active output with this name and type (for auto-connect),
/// falling back to any node declaring it.
fn latest_output(active: &Active, name: &str, scope: FieldScope) -> Option<SocketRef> {
    let mut candidates: Vec<(Option<usize>, &NodeReport)> = active
        .pipeline
        .analysis()
        .nodes
        .iter()
        .filter(|node| {
            node.outputs
                .iter()
                .any(|o| o.name == name && o.scope == scope)
        })
        .map(|node| (node.order.filter(|_| node.status == NodeStatus::Active), node))
        .collect();

    candidates.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| id_order(&a.1.id).cmp(&id_order(&b.1.id))));

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

    fn socket(node: &str, socket: &str) -> SocketRef {
        SocketRef {
            node: node.into(),
            socket: socket.into(),
        }
    }

    fn node<'a>(graph: &'a PipelineGraph, id: &str) -> &'a NodeReport {
        graph.nodes.iter().find(|n| n.id == id).unwrap()
    }

    #[test]
    fn generates_deterministic_column_payload() {
        let session = Session::new(42).unwrap();
        let fields = session.active().pipeline.schema().column_fields();
        let coord = ChunkCoord(128, 128, 0);
        let first = encode_columns(&session.generate(coord).unwrap(), &fields, 0);
        let again = encode_columns(&session.generate(coord).unwrap(), &fields, 0);

        assert_eq!(first, again);
        assert_eq!(first.len(), PAYLOAD_HEADER_BYTES + 2 * 32 * 32 * 4);
        assert_eq!(&first[0..4], &PAYLOAD_VERSION.to_le_bytes());
        assert_eq!(i64::from_le_bytes(first[16..24].try_into().unwrap()), 0);
        assert_eq!(u32::from_le_bytes(first[32..36].try_into().unwrap()), 1);

        let keys: Vec<_> = fields.iter().map(|f| f.key.as_str()).collect();
        assert_eq!(keys, ["n1.elevation", "n2.temperature"]);
    }

    #[test]
    fn world_samples_match_chunk_columns_at_the_same_positions() {
        let session = Session::new(5).unwrap();
        let chunk = session.generate(ChunkCoord(130, 127, 0)).unwrap();
        let origin = chunk.origin();
        let payload = session
            .world_payload((origin.0, origin.1), 1, 32, 32)
            .unwrap();

        // Elevation is world field 3.
        let plane = PAYLOAD_HEADER_BYTES + 3 * 32 * 32 * 4;

        for (column, x, y, _) in chunk.chunk_data().columns.iter() {
            let offset = plane + (x + y * 32) * 4;
            let world = f32::from_le_bytes(payload[offset..offset + 4].try_into().unwrap());

            assert_eq!(world, column[session.slot("n1.elevation")]);
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
        assert!(session.world_point(0, 0).is_some());
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

        for (voxel, x, y, z) in chunk.data().iter() {
            let elevation = chunk.chunk_data().columns[(x, y, 0)][session.slot("n1.elevation")];
            let expected = if (z as f32) < elevation { 1.0 } else { 0.0 };

            assert_eq!(voxel.slots[session.slot("n3.solid")], expected);
        }
    }

    #[test]
    fn column_values_are_seamless_across_chunk_borders() {
        let session = Session::new(3).unwrap();
        let left = session.generate(ChunkCoord(10, 10, 0)).unwrap();
        let right = session.generate(ChunkCoord(11, 10, 0)).unwrap();
        let world = &session.world;

        let elevation = session.slot("n1.elevation");
        let left_edge = left.chunk_data().columns[(31, 5, 0)][elevation];
        let right_edge = right.chunk_data().columns[(0, 5, 0)][elevation];
        let (lx, ly) = (left.origin().0 + 31, left.origin().1 + 5);

        assert_eq!(left_edge, world.elevation_at(lx, ly));
        assert_eq!(right_edge, world.elevation_at(lx + 1, ly));
    }

    #[test]
    fn adding_nodes_auto_connects_by_name_and_publishes_revisions() {
        let session = Session::new(1).unwrap();
        let (id, graph) = session.add_node("humidity", (600.0, 0.0), None).unwrap();

        assert_eq!(id, "n5");
        assert_eq!(graph.revision, 1);
        assert_eq!(node(&graph, "n5").status, NodeStatus::Active);
        assert!(graph.edges.iter().any(|e| e.from == socket("n1", "elevation")
            && e.to == socket("n5", "elevation")));
        assert!(graph.edges.iter().any(|e| e.from == socket("n2", "temperature")
            && e.to == socket("n5", "temperature")));

        let keys: Vec<_> = session
            .world_info()
            .column_fields
            .iter()
            .map(|f| f.key.clone())
            .collect();
        assert_eq!(keys, ["n1.elevation", "n2.temperature", "n5.humidity"]);

        // Disabling the source blocks everything downstream.
        let graph = session.set_enabled("n1", false).unwrap();
        assert!(["n2", "n3", "n4", "n5"]
            .iter()
            .all(|id| node(&graph, id).status == NodeStatus::Blocked));
        assert!(session.world_info().column_fields.is_empty());

        // Removing a node removes its edges.
        let graph = session.remove_node("n2").unwrap();
        assert!(graph.edges.iter().all(|e| e.from.node != "n2" && e.to.node != "n2"));
        assert_eq!(graph.revision, 3);
    }

    #[test]
    fn connect_rejects_mismatched_types_unknown_sockets_and_cycles() {
        let session = Session::new(1).unwrap();
        session.add_node("smooth", (0.0, 0.0), None).unwrap(); // n5, auto: none named "value"
        session.add_node("smooth", (0.0, 0.0), None).unwrap(); // n6

        let voxel_to_column = session.connect(socket("n3", "solid"), socket("n5", "value"));
        assert!(voxel_to_column.unwrap_err().contains("Cannot connect"));

        assert!(session
            .connect(socket("n1", "nope"), socket("n5", "value"))
            .unwrap_err()
            .contains("no output"));

        session
            .connect(socket("n1", "elevation"), socket("n5", "value"))
            .unwrap();
        session
            .connect(socket("n5", "smoothed"), socket("n6", "value"))
            .unwrap();
        assert!(session
            .connect(socket("n6", "smoothed"), socket("n5", "value"))
            .unwrap_err()
            .contains("cycle"));

        // Reconnecting an input replaces its edge.
        let graph = session
            .connect(socket("n5", "smoothed"), socket("n2", "elevation"))
            .unwrap();
        let into_n2: Vec<_> = graph.edges.iter().filter(|e| e.to.node == "n2").collect();
        assert_eq!(into_n2.len(), 1);
        assert_eq!(into_n2[0].from, socket("n5", "smoothed"));

        assert!(session.disconnect(socket("n2", "elevation")).is_ok());
        assert!(session.disconnect(socket("n2", "elevation")).is_err());
    }

    #[test]
    fn smooth_reprocesses_a_value_that_downstream_nodes_read() {
        let session = Session::new(8).unwrap();
        session.add_node("smooth", (0.0, 0.0), None).unwrap(); // n5
        session
            .connect(socket("n1", "elevation"), socket("n5", "value"))
            .unwrap();
        session.set_param("n5", "radius", 1.0).unwrap();
        // Temperature now reads smoothed elevation; raw elevation still exists.
        session
            .connect(socket("n5", "smoothed"), socket("n2", "elevation"))
            .unwrap();

        let chunk = session.generate(ChunkCoord(140, 90, 0)).unwrap();
        let (origin, smoothed) = (chunk.origin(), session.slot("n5.smoothed"));
        let e = |x: i64, y: i64| f64::from(session.world.elevation_at(x, y));

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
            assert_ne!(column[smoothed], column[session.slot("n1.elevation")]);
        }

        let graph = session.pipeline_graph();
        assert_eq!(node(&graph, "n5").neighborhood, Some((1, 1, 0)));
        assert!(session.set_param("n5", "bogus", 1.0).is_err());
    }

    #[test]
    fn slope_reads_across_chunk_borders_exactly() {
        let session = Session::new(6).unwrap();
        let (id, graph) = session.add_node("slope", (0.0, 0.0), None).unwrap();
        assert_eq!(node(&graph, &id).status, NodeStatus::Active);

        let chunk = session.generate(ChunkCoord(140, 90, 0)).unwrap();
        let (origin, out) = (chunk.origin(), session.slot(&format!("{id}.slope")));
        let e = |x: i64, y: i64| session.world.elevation_at(x, y);

        for (x, y) in [(0usize, 0usize), (31, 31), (0, 17), (31, 3), (12, 0), (5, 31)] {
            let (gx, gy) = (origin.0 + x as i64, origin.1 + y as i64);
            let dx = (e(gx + 1, gy) - e(gx - 1, gy)) / 2.0;
            let dy = (e(gx, gy + 1) - e(gx, gy - 1)) / 2.0;

            assert_eq!(chunk.chunk_data().columns[(x, y, 0)][out], (dx * dx + dy * dy).sqrt());
        }
    }

    #[test]
    fn neighborhood_prefix_results_are_reused_between_requests() {
        let session = Session::new(6).unwrap();
        session.add_node("slope", (0.0, 0.0), None).unwrap();

        let elevation_runs = || {
            session
                .step_timings()
                .1
                .iter()
                .find(|t| t.id == "n1")
                .unwrap()
                .runs
        };

        let before = elevation_runs();
        session.column_payload(ChunkCoord(60, 60, 0)).unwrap();
        assert_eq!(elevation_runs() - before, 9);

        session.column_payload(ChunkCoord(61, 60, 0)).unwrap();
        assert_eq!(elevation_runs() - before, 12);
    }

    #[test]
    fn script_nodes_auto_connect_and_update_keeps_matching_edges() {
        let session = Session::new(4).unwrap();
        let (id, graph) = session
            .add_node(SCRIPT_KIND, (0.0, 0.0), Some(worldgraph::script::TEMPLATE.into()))
            .unwrap();

        let script = node(&graph, &id);
        assert!(script.script);
        assert_eq!(script.status, NodeStatus::Active);
        assert!(graph.edges.iter().any(|e| e.from == socket("n2", "temperature")
            && e.to == socket(&id, "temperature")));

        let moisture = session.slot(&format!("{id}.moisture"));
        let chunk = session.generate(ChunkCoord(128, 128, 0)).unwrap();
        assert!(chunk
            .chunk_data()
            .columns
            .iter()
            .all(|(c, ..)| (0.0..=1.0).contains(&c[moisture])));

        // Dropping the temperature input removes only that edge.
        let edited = worldgraph::script::TEMPLATE
            .replace("input: elevation, temperature", "input: elevation")
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
        assert_eq!(session.graph_doc().nodes.len(), 4);
    }

    #[test]
    fn moving_nodes_changes_layout_only_and_docs_round_trip() {
        let session = Session::new(2).unwrap();
        session.add_node("humidity", (0.0, 0.0), None).unwrap();
        let revision = session.pipeline_graph().revision;

        session.move_nodes(&[("n5".into(), (123.0, 45.0))]);
        let graph = session.pipeline_graph();
        assert_eq!(graph.revision, revision);
        assert_eq!(node(&graph, "n5").position, (123.0, 45.0));

        let saved = session.graph_doc().to_json();
        let reloaded = Session::with_doc(2, GraphDoc::from_json(&saved).unwrap()).unwrap();
        let again = reloaded.pipeline_graph();

        assert_eq!(reloaded.graph_doc(), session.graph_doc());
        assert_eq!(node(&again, "n5").position, (123.0, 45.0));
        assert_eq!(node(&again, "n5").status, NodeStatus::Active);
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
        assert_eq!(ids, ["n1", "n2", "n3", "n4"]);

        let coord = ChunkCoord(100, 100, 0);
        session.column_payload(coord).unwrap();
        session.column_payload(coord).unwrap(); // cache hit: no node runs

        let (_, after) = session.step_timings();
        // Export node n4 has no runtime work.
        for (old, new) in before.iter().zip(&after).filter(|(t, _)| t.id != "n4") {
            assert_eq!(new.runs, old.runs + 1);
        }
    }

    #[test]
    fn default_graph_exports_height_and_labels_rename_exports() {
        let session = Session::new(3).unwrap();
        let info = session.world_info();

        assert_eq!(info.exports.len(), 1);
        assert_eq!(info.exports[0].name, "height");
        assert_eq!(info.exports[0].field, "n1.elevation");

        let graph = session.set_label("n4", "  terrain_height ").unwrap();
        assert_eq!(graph.exports[0].name, "terrain_height");
        assert_eq!(node(&graph, "n4").label, "terrain_height");

        // Clearing the label falls back to the default export name.
        let graph = session.set_label("n4", "").unwrap();
        assert_eq!(graph.exports[0].name, worldgraph::pipeline::DEFAULT_EXPORT_NAME);
        assert!(session.set_label("n99", "x").is_err());
    }
}
