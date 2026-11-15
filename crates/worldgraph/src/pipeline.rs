//! Node graph → analysis → schema (one slot per output) → DynPipeline.
//!
//! Wiring is explicit: an input reads exactly the output it is connected
//! to. Execution order is a topological sort of the edges (ties by node id,
//! never by layout). Every output of a runnable node gets its own slot, so
//! re-processing nodes write new values instead of overwriting old ones.
//! Invalid nodes are not fatal: they are reported as blocked (with a
//! reason) and skipped, along with everything downstream of them.

use std::{
    borrow::Cow,
    collections::{BTreeMap, HashMap},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::Instant,
};

use layergen::{
    ChunkStep, DataSet, NeighborChunks, NeighborhoodChunkStep, NeighborhoodView,
    PipelineStepResult, StepContext,
};
use serde::Serialize;

#[cfg(feature = "scripting")]
use crate::script::{ScriptDef, ScriptStep};
use crate::{
    atlas::{
        Atlas, AtlasContext, AtlasGrid, AtlasHooks, AtlasIo, AtlasRun, BoxedAtlasStage, SampleStep,
        ATLAS_SLOTS,
    },
    can_connect,
    graph::{id_order, GraphDoc, NodeDoc, SocketRef},
    steps,
    world::WorldStore,
    ColumnData, FieldDef, FieldScope, Voxel, COLUMN_SLOTS, VOXEL_SLOTS, WORLD_SLOTS,
};

pub type BoxedStep = Box<dyn ChunkStep<Voxel, ColumnData, WorldStore>>;
pub type BoxedNeighborhoodStep = Box<dyn NeighborhoodChunkStep<Voxel, ColumnData, WorldStore>>;
pub type Text = Cow<'static, str>;

/// Per-axis neighborhood radius in cells (LayerGen `Neighborhood::axes`).
pub type Radius = (u32, u32, u32);

/// One world-scope step: a pure function of the world position. World steps
/// run per sample point (a chunk column, an overview grid sample) into a small
/// slot array, in graph order. Whatever state a step needs (a plate layout, a
/// noise seed) is built once when the world is created and is read-only
/// afterwards.
///
/// Slots are f64 so chains of world nodes lose no precision; values are cast
/// to f32 where they are sampled into chunk data.
pub trait WorldStep: Send + Sync + 'static {
    fn run(&self, x: i64, y: i64, slots: &mut [f64; WORLD_SLOTS]);
}

pub type BoxedWorldStep = Box<dyn WorldStep>;

/// What world steps may read when they are built (once per world).
#[derive(Clone, Copy, Debug)]
pub struct WorldBuildContext {
    pub seed: u64,
    /// World bounds in cells.
    pub origin: (i64, i64),
    pub size: (u32, u32),
    /// World depth in cells (the elevation range).
    pub depth: u32,
}

/// The world-scope part of a graph, compiled: runs every world node at a point.
#[derive(Default)]
pub struct WorldGraph {
    steps: Vec<BoxedWorldStep>,
}

impl WorldGraph {
    /// All world slots at `(x, y)`; slots not written by a node are 0.
    pub fn eval(&self, x: i64, y: i64) -> [f64; WORLD_SLOTS] {
        let mut slots = [0.0; WORLD_SLOTS];
        for step in &self.steps {
            step.run(x, y, &mut slots);
        }
        slots
    }

    pub fn is_empty(&self) -> bool {
        self.steps.is_empty()
    }
}

pub const SCRIPT_KIND: &str = "script";

// ---------------------------------------------------------------------------
// Contracts

#[derive(Clone, Debug, PartialEq)]
pub struct InputSpec {
    pub name: Text,
    pub scope: FieldScope,
}

/// An output socket with its display defaults.
#[derive(Clone, Debug, PartialEq)]
pub struct FieldSpec {
    pub name: Text,
    pub scope: FieldScope,
    pub unit: Text,
    pub min: f32,
    pub max: f32,
    pub ramp: Text,
    /// Copy unit/range/ramp from the field wired into this input
    /// (re-processing nodes like Smooth).
    pub display_from: Option<Text>,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ParamSpec {
    pub name: &'static str,
    pub default: f64,
    pub min: f64,
    pub max: f64,
}

/// Parameter values of one node (doc values over kind defaults, clamped).
#[derive(Clone, Debug, Default)]
pub struct Params {
    values: BTreeMap<String, f64>,
}

impl Params {
    fn resolve(specs: &[ParamSpec], doc: &BTreeMap<String, f64>) -> Self {
        Self {
            values: specs
                .iter()
                .map(|spec| {
                    let value = doc.get(spec.name).copied().unwrap_or(spec.default);
                    (spec.name.to_owned(), value.clamp(spec.min, spec.max))
                })
                .collect(),
        }
    }

    pub fn get(&self, name: &str) -> f64 {
        self.values.get(name).copied().unwrap_or(0.0)
    }
}

/// Resolved slots for one runnable node's sockets.
#[derive(Clone, Debug, Default)]
pub struct Bindings {
    inputs: HashMap<String, usize>,
    outputs: HashMap<String, usize>,
}

impl Bindings {
    pub fn input(&self, name: &str) -> Result<usize, String> {
        self.inputs
            .get(name)
            .copied()
            .ok_or_else(|| format!("input `{name}` is not bound"))
    }

    pub fn output(&self, name: &str) -> Result<usize, String> {
        self.outputs
            .get(name)
            .copied()
            .ok_or_else(|| format!("output `{name}` is not bound"))
    }
}

/// How a built-in node kind is constructed for a revision.
#[derive(Clone, Copy)]
pub enum Build {
    Chunk(fn(&Bindings, &Params) -> Result<BoxedStep, String>),
    /// Cross-chunk node: at most one per graph (LayerGen's single stage).
    Neighborhood {
        radius: fn(&Params) -> Radius,
        build: fn(&Bindings, &Params) -> Result<BoxedNeighborhoodStep, String>,
    },
    /// Named sink for game code: no runtime work, publishes its input's
    /// slot under the node's label (see `Analysis::exports`).
    Export,
    /// Wiring helper (reroute): one input, one output of the same type, no
    /// runtime work. The output aliases the input's slot and adds no field.
    Alias,
    /// World-scope node: built once when the world is created, then evaluated
    /// per sample point. Inputs and outputs are world slots.
    World(fn(&Bindings, &Params, &WorldBuildContext) -> Result<BoxedWorldStep, String>),
    /// Atlas-scope node: built and run once when the world is created, over
    /// whole grids. Inputs and outputs are atlas slots.
    Atlas(fn(&Bindings, &Params, &AtlasContext) -> Result<BoxedAtlasStage, String>),
    /// world -> atlas adapter (`rasterize`): one world input, one atlas output.
    Rasterize,
    /// atlas -> world adapter (`sample`): one atlas input, one world output.
    Sample,
}

/// A registered built-in node kind.
pub struct NodeKind {
    pub id: &'static str,
    pub label: &'static str,
    pub description: &'static str,
    pub inputs: &'static [InputSpec],
    pub outputs: &'static [FieldSpec],
    pub params: &'static [ParamSpec],
    pub build: Build,
}

/// The data contract of one node instance.
#[derive(Clone, Debug)]
pub struct Contract {
    pub label: String,
    pub description: String,
    pub inputs: Vec<InputSpec>,
    pub outputs: Vec<FieldSpec>,
    pub params: Vec<ParamSpec>,
    pub neighborhood: Option<Radius>,
    /// Export node: publishes its single input by name.
    pub export: bool,
    /// Reroute: passes its single input through in the same slot.
    pub alias: bool,
}

impl Contract {
    /// Where the node runs: its outputs' scope (world, chunk columns or
    /// voxels); sinks without outputs (exports) follow their input.
    pub fn scope(&self) -> FieldScope {
        self.outputs
            .first()
            .map(|output| output.scope)
            .or_else(|| self.inputs.first().map(|input| input.scope))
            .unwrap_or(FieldScope::Column)
    }
}

#[derive(Clone)]
pub enum Implementation {
    Builtin(Build),
    #[cfg(feature = "scripting")]
    Script(Arc<ScriptDef>),
}

/// A doc node resolved against the registry (or its script parsed).
#[derive(Clone)]
pub struct ResolvedNode {
    pub contract: Result<Arc<Contract>, String>,
    pub implementation: Option<Implementation>,
    pub params: Params,
}

pub fn resolve(node: &NodeDoc) -> ResolvedNode {
    if node.kind == SCRIPT_KIND {
        return resolve_script(node);
    }

    let Some(kind) = steps::find_kind(&node.kind) else {
        return ResolvedNode {
            contract: Err(format!("unknown node kind `{}`", node.kind)),
            implementation: None,
            params: Params::default(),
        };
    };

    let params = Params::resolve(kind.params, &node.params);
    let neighborhood = match kind.build {
        Build::Neighborhood { radius, .. } => Some(radius(&params)),
        _ => None,
    };

    ResolvedNode {
        contract: Ok(Arc::new(Contract {
            label: kind.label.to_owned(),
            description: kind.description.to_owned(),
            inputs: kind.inputs.to_vec(),
            outputs: kind.outputs.to_vec(),
            params: kind.params.to_vec(),
            neighborhood,
            export: matches!(kind.build, Build::Export),
            alias: matches!(kind.build, Build::Alias),
        })),
        implementation: Some(Implementation::Builtin(kind.build)),
        params,
    }
}

#[cfg(not(feature = "scripting"))]
fn resolve_script(_node: &NodeDoc) -> ResolvedNode {
    ResolvedNode {
        contract: Err("script nodes need the `scripting` feature".to_owned()),
        implementation: None,
        params: Params::default(),
    }
}

#[cfg(feature = "scripting")]
fn resolve_script(node: &NodeDoc) -> ResolvedNode {
    let parsed = node
        .script
        .as_deref()
        .ok_or_else(|| "script node has no source".to_owned())
        .and_then(ScriptDef::parse);

    match parsed {
        Ok(def) => ResolvedNode {
            contract: Ok(Arc::new(def.contract.clone())),
            implementation: Some(Implementation::Script(Arc::new(def))),
            params: Params::default(),
        },
        Err(error) => ResolvedNode {
            contract: Err(format!("script: {error}")),
            implementation: None,
            params: Params::default(),
        },
    }
}

// ---------------------------------------------------------------------------
// Analysis output

/// Fields produced by runnable nodes, keyed `node.socket`, with slots.
#[derive(Clone, Debug, Default)]
pub struct Schema {
    pub fields: Vec<FieldDef>,
}

impl Schema {
    /// Slot of a field by key (`node.socket`). Prefer exports in game code.
    pub fn slot(&self, key: &str) -> Result<usize, String> {
        self.fields
            .iter()
            .find(|field| field.key == key)
            .map(|field| field.slot)
            .ok_or_else(|| format!("Field `{key}` is not in the schema"))
    }

    pub fn column_fields(&self) -> Vec<FieldDef> {
        self.scoped(FieldScope::Column)
    }

    pub fn voxel_fields(&self) -> Vec<FieldDef> {
        self.scoped(FieldScope::Voxel)
    }

    /// Outputs of world-scope nodes (slots index `WorldGraph::eval`).
    pub fn world_fields(&self) -> Vec<FieldDef> {
        self.scoped(FieldScope::World)
    }

    /// Outputs of atlas-scope nodes (slots index the atlas grids).
    pub fn atlas_fields(&self) -> Vec<FieldDef> {
        self.scoped(FieldScope::Atlas)
    }

    fn scoped(&self, scope: FieldScope) -> Vec<FieldDef> {
        self.fields
            .iter()
            .filter(|field| field.scope == scope)
            .cloned()
            .collect()
    }
}

/// Where an active node runs when the graph has a neighborhood node.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Stage {
    /// Needed by the neighborhood node: re-run for every dependency chunk.
    Prefix,
    Neighborhood,
    /// Runs once per requested chunk, after the neighborhood node.
    Suffix,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum NodeStatus {
    Active,
    Disabled,
    Blocked,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SocketReport {
    pub name: String,
    pub scope: FieldScope,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ParamReport {
    #[serde(flatten)]
    pub spec: ParamSpec,
    pub value: f64,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NodeReport {
    pub id: String,
    pub kind: String,
    /// User label if set, else the kind's label. For Export nodes: the export name.
    pub label: String,
    pub kind_label: String,
    /// Where the node runs: world, chunk (column) or voxel.
    pub scope: FieldScope,
    pub export: bool,
    pub description: String,
    pub enabled: bool,
    pub status: NodeStatus,
    pub reason: Option<String>,
    /// Position in execution order (active nodes only).
    pub order: Option<usize>,
    pub inputs: Vec<SocketReport>,
    pub outputs: Vec<SocketReport>,
    pub params: Vec<ParamReport>,
    pub script: bool,
    pub stage: Option<Stage>,
    pub neighborhood: Option<Radius>,
    /// Filled from the current doc by the session (layout only).
    pub position: (f64, f64),
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EdgeReport {
    pub from: SocketRef,
    pub to: SocketRef,
    /// Socket type, when both ends exist and agree.
    pub scope: Option<FieldScope>,
    /// False for dangling, mistyped or duplicate edges (input then unbound).
    pub valid: bool,
}

/// A named, game-facing output: the slot holding an Export node's input.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Export {
    pub name: String,
    /// The Export node id.
    pub node: String,
    pub scope: FieldScope,
    pub slot: usize,
    /// Key (`node.socket`) of the exported field.
    pub field: String,
}

/// Default export name when an Export node has no label.
pub const DEFAULT_EXPORT_NAME: &str = "export";

pub fn export_name(node: &NodeDoc) -> String {
    node.label
        .as_deref()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .unwrap_or(DEFAULT_EXPORT_NAME)
        .to_owned()
}

pub struct Analysis {
    /// Node indices in execution order. With a neighborhood node: prefix,
    /// then the neighborhood node at `order[prefix_len]`, then suffix.
    pub order: Vec<usize>,
    pub prefix_len: Option<usize>,
    pub schema: Schema,
    /// Per node index; `Some` for runnable nodes.
    pub bindings: Vec<Option<Bindings>>,
    pub nodes: Vec<NodeReport>,
    pub edges: Vec<EdgeReport>,
    /// Game-facing named outputs, from runnable Export nodes.
    pub exports: Vec<Export>,
    /// World outputs that chunk-scope inputs read: each is copied from the
    /// world graph into a column slot once per column (implicit sampling).
    pub world_samples: Vec<WorldSampleBinding>,
}

/// One world output sampled into the column data of every chunk.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WorldSampleBinding {
    pub world_slot: usize,
    pub column_slot: usize,
}

// ---------------------------------------------------------------------------
// Analysis

pub fn analyze(doc: &GraphDoc, resolved: &[ResolvedNode]) -> Analysis {
    let n = doc.nodes.len();
    let index: HashMap<&str, usize> = doc
        .nodes
        .iter()
        .enumerate()
        .map(|(i, node)| (node.id.as_str(), i))
        .collect();
    let contract = |i: usize| resolved[i].contract.as_ref().ok();
    let title = |i: usize| -> String {
        doc.nodes[i]
            .label
            .clone()
            .filter(|l| !l.trim().is_empty())
            .or_else(|| contract(i).map(|c| c.label.clone()))
            .unwrap_or_else(|| doc.nodes[i].kind.clone())
    };
    let label = |i: usize| format!("{} ({})", title(i), doc.nodes[i].id);

    let mut reasons: Vec<Option<String>> = (0..n)
        .map(|i| resolved[i].contract.as_ref().err().cloned())
        .collect();

    // Ids in creation order: the deterministic tie-break everywhere.
    let mut by_id: Vec<usize> = (0..n).collect();
    by_id.sort_by(|&a, &b| id_order(&doc.nodes[a].id).cmp(&id_order(&doc.nodes[b].id)));

    // 1. One neighborhood node (first enabled by id wins).
    let mut neighborhood_owner = None;
    for &i in &by_id {
        let wants = doc.nodes[i].enabled
            && reasons[i].is_none()
            && contract(i).is_some_and(|c| c.neighborhood.is_some());
        if !wants {
            continue;
        }
        match neighborhood_owner {
            None => neighborhood_owner = Some(i),
            Some(owner) => {
                reasons[i] = Some(format!(
                    "only one neighborhood node is supported ({} is enabled)",
                    label(owner)
                ))
            }
        }
    }

    // 2. Validate edges; build input → source output map.
    let mut incoming: HashMap<(usize, String), (usize, String)> = HashMap::new();
    let mut edges = Vec::new();

    for edge in &doc.edges {
        let from = index.get(edge.from.node.as_str()).copied();
        let to = index.get(edge.to.node.as_str()).copied();
        let out_scope = from.and_then(|f| {
            contract(f)?
                .outputs
                .iter()
                .find(|o| o.name == edge.from.socket)
                .map(|o| o.scope)
        });
        let in_scope = to.and_then(|t| {
            contract(t)?
                .inputs
                .iter()
                .find(|i| i.name == edge.to.socket)
                .map(|i| i.scope)
        });

        let valid = match (from, to, out_scope, in_scope) {
            (Some(f), Some(t), Some(a), Some(b)) if can_connect(a, b) && f != t => {
                !incoming.contains_key(&(t, edge.to.socket.clone()))
                    && incoming
                        .insert((t, edge.to.socket.clone()), (f, edge.from.socket.clone()))
                        .is_none()
            }
            _ => false,
        };

        edges.push(EdgeReport {
            from: edge.from.clone(),
            to: edge.to.clone(),
            scope: out_scope
                .zip(in_scope)
                .filter(|(a, b)| can_connect(*a, *b))
                .map(|(a, _)| a),
            valid,
        });
    }

    // 3. Topological order with slot allocation.
    let eligible: Vec<bool> = (0..n)
        .map(|i| doc.nodes[i].enabled && reasons[i].is_none())
        .collect();
    let mut runnable = vec![false; n];
    let mut order = Vec::new();
    let mut slots: HashMap<(usize, String), usize> = HashMap::new();
    let (mut next_column, mut next_voxel, mut next_world, mut next_atlas) =
        (0usize, 0usize, 0usize, 0usize);
    let mut export_names: HashMap<String, usize> = HashMap::new();
    // A chunk-scope input wired to a world output reads a column slot that
    // the world graph fills per column (one per distinct world output).
    let mut samples: HashMap<(usize, String), usize> = HashMap::new();
    let mut world_samples: Vec<WorldSampleBinding> = Vec::new();
    // The slot each runnable node reads for each of its inputs.
    let mut input_slots: HashMap<(usize, String), usize> = HashMap::new();
    let output_scope = |node: usize, socket: &str| {
        contract(node)?
            .outputs
            .iter()
            .find(|o| o.name == socket)
            .map(|o| o.scope)
    };

    loop {
        let ready = by_id.iter().copied().find(|&i| {
            eligible[i]
                && !runnable[i]
                && reasons[i].is_none()
                && contract(i).is_some_and(|c| {
                    c.inputs.iter().all(|input| {
                        incoming
                            .get(&(i, input.name.to_string()))
                            .is_some_and(|(source, _)| runnable[*source])
                    })
                })
        });

        let Some(i) = ready else { break };

        if contract(i).is_some_and(|c| c.export) {
            let name = export_name(&doc.nodes[i]);
            if let Some(&owner) = export_names.get(&name) {
                reasons[i] = Some(format!(
                    "export name `{name}` is already used by {}",
                    label(owner)
                ));
                continue;
            }
            export_names.insert(name, i);
        }

        let c = contract(i).expect("eligible nodes resolved");

        // World outputs read by column inputs need a sampled column slot.
        let mut new_samples: Vec<(usize, String)> = Vec::new();
        for input in &c.inputs {
            let source = &incoming[&(i, input.name.to_string())];
            if input.scope == FieldScope::Column
                && output_scope(source.0, &source.1) == Some(FieldScope::World)
                && !samples.contains_key(source)
                && !new_samples.contains(source)
            {
                new_samples.push(source.clone());
            }
        }

        // Reroutes take no slot of their own: their output is the input's slot.
        let outputs: &[FieldSpec] = if c.alias { &[] } else { &c.outputs };
        let count = |scope| outputs.iter().filter(|o| o.scope == scope).count();
        let (columns, voxels, worlds, atlases) = (
            count(FieldScope::Column) + new_samples.len(),
            count(FieldScope::Voxel),
            count(FieldScope::World),
            count(FieldScope::Atlas),
        );

        if next_column + columns > COLUMN_SLOTS
            || next_voxel + voxels > VOXEL_SLOTS
            || next_world + worlds > WORLD_SLOTS
            || next_atlas + atlases > ATLAS_SLOTS
        {
            reasons[i] = Some(
                "no free output slots (raise COLUMN_SLOTS/VOXEL_SLOTS/WORLD_SLOTS/ATLAS_SLOTS)"
                    .into(),
            );
            continue;
        }

        for source in new_samples {
            samples.insert(source.clone(), next_column);
            world_samples.push(WorldSampleBinding {
                world_slot: slots[&source],
                column_slot: next_column,
            });
            next_column += 1;
        }

        for input in &c.inputs {
            let source = &incoming[&(i, input.name.to_string())];
            let slot = if input.scope == FieldScope::Column
                && output_scope(source.0, &source.1) == Some(FieldScope::World)
            {
                samples[source]
            } else {
                slots[source]
            };
            input_slots.insert((i, input.name.to_string()), slot);
        }

        if c.alias {
            let slot = input_slots[&(i, c.inputs[0].name.to_string())];
            slots.insert((i, c.outputs[0].name.to_string()), slot);
        }

        for output in outputs {
            let counter = match output.scope {
                FieldScope::Voxel => &mut next_voxel,
                FieldScope::Column => &mut next_column,
                FieldScope::World => &mut next_world,
                FieldScope::Atlas => &mut next_atlas,
            };
            slots.insert((i, output.name.to_string()), *counter);
            *counter += 1;
        }

        runnable[i] = true;
        order.push(i);
    }

    // World and atlas nodes run once per world, not per chunk: they come
    // first (in topological order among themselves).
    let is_world = |i: usize| contract(i).is_some_and(|c| c.scope().is_world_stage());
    order.sort_by_key(|&i| !is_world(i));

    // 4. Explain eligible nodes that did not run.
    let pending: Vec<usize> = by_id
        .iter()
        .copied()
        .filter(|&i| eligible[i] && !runnable[i] && reasons[i].is_none())
        .collect();

    for &i in &pending {
        for input in &contract(i).expect("eligible").inputs {
            match incoming.get(&(i, input.name.to_string())) {
                None => {
                    reasons[i] = Some(format!("input `{}` is not connected", input.name));
                    break;
                }
                Some((source, _)) if !doc.nodes[*source].enabled => {
                    reasons[i] = Some(format!(
                        "input `{}` comes from disabled node {}",
                        input.name,
                        label(*source)
                    ));
                    break;
                }
                _ => {}
            }
        }
    }

    loop {
        let mut changed = false;
        for &i in &pending {
            if reasons[i].is_some() {
                continue;
            }
            let blocked = contract(i)
                .expect("eligible")
                .inputs
                .iter()
                .find_map(|input| {
                    let (source, _) = incoming.get(&(i, input.name.to_string()))?;
                    reasons[*source].is_some().then_some(*source)
                });
            if let Some(source) = blocked {
                reasons[i] = Some(format!("depends on blocked node {}", label(source)));
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }

    for &i in &pending {
        reasons[i].get_or_insert_with(|| "dependency cycle".to_owned());
    }

    // 5. Neighborhood split: prefix = the neighborhood node's ancestors.
    let mut stages: Vec<Option<Stage>> = vec![None; n];
    let mut prefix_len = None;

    if let Some(nb) = order
        .iter()
        .copied()
        .find(|&i| contract(i).is_some_and(|c| c.neighborhood.is_some()))
    {
        let mut ancestors = vec![false; n];
        let mut stack = vec![nb];

        while let Some(node) = stack.pop() {
            for input in &contract(node).expect("runnable").inputs {
                if let Some((source, _)) = incoming.get(&(node, input.name.to_string())) {
                    // World nodes are not chunk work: never part of a stage.
                    if !ancestors[*source] && !is_world(*source) {
                        ancestors[*source] = true;
                        stack.push(*source);
                    }
                }
            }
        }

        let worlds: Vec<usize> = order.iter().copied().filter(|&i| is_world(i)).collect();
        let prefix: Vec<usize> = order.iter().copied().filter(|&i| ancestors[i]).collect();
        let suffix: Vec<usize> = order
            .iter()
            .copied()
            .filter(|&i| !ancestors[i] && i != nb && !is_world(i))
            .collect();

        prefix.iter().for_each(|&i| stages[i] = Some(Stage::Prefix));
        stages[nb] = Some(Stage::Neighborhood);
        suffix.iter().for_each(|&i| stages[i] = Some(Stage::Suffix));

        // Position of the neighborhood node in `order`: world nodes, prefix, it.
        prefix_len = Some(worlds.len() + prefix.len());
        order = worlds
            .into_iter()
            .chain(prefix)
            .chain([nb])
            .chain(suffix)
            .collect();
    }

    // 6. Schema (execution order) and bindings.
    let key = |node: usize, socket: &str| format!("{}.{socket}", doc.nodes[node].id);
    // The real producer of a value: reroutes are looked through.
    let origin = |mut source: (usize, String)| {
        while let Some(c) = contract(source.0).filter(|c| c.alias) {
            match incoming.get(&(source.0, c.inputs[0].name.to_string())) {
                Some(next) => source = next.clone(),
                None => break,
            }
        }
        source
    };
    let mut fields: Vec<FieldDef> = Vec::new();

    for &i in &order {
        let c = contract(i).expect("runnable");
        if c.alias {
            continue; // no slot, no field of its own
        }
        for output in &c.outputs {
            // Re-processing outputs inherit display from their input's source.
            let source_field = output.display_from.as_ref().and_then(|input| {
                let (source, socket) = origin(incoming.get(&(i, input.to_string()))?.clone());
                fields
                    .iter()
                    .find(|f| f.key == key(source, &socket))
                    .cloned()
            });

            fields.push(FieldDef {
                key: key(i, &output.name),
                node: doc.nodes[i].id.clone(),
                node_label: title(i),
                name: output.name.clone(),
                scope: output.scope,
                slot: slots[&(i, output.name.to_string())],
                unit: source_field
                    .as_ref()
                    .map_or(output.unit.clone(), |f| f.unit.clone()),
                min: source_field.as_ref().map_or(output.min, |f| f.min),
                max: source_field.as_ref().map_or(output.max, |f| f.max),
                ramp: source_field.map_or(output.ramp.clone(), |f| f.ramp),
            });
        }
    }

    let bindings = (0..n)
        .map(|i| {
            runnable[i].then(|| {
                let c = contract(i).expect("runnable");
                Bindings {
                    inputs: c
                        .inputs
                        .iter()
                        .map(|input| {
                            let slot = input_slots[&(i, input.name.to_string())];
                            (input.name.to_string(), slot)
                        })
                        .collect(),
                    outputs: c
                        .outputs
                        .iter()
                        .map(|o| (o.name.to_string(), slots[&(i, o.name.to_string())]))
                        .collect(),
                }
            })
        })
        .collect();

    let sockets = |list: &[InputSpec]| -> Vec<SocketReport> {
        list.iter()
            .map(|s| SocketReport {
                name: s.name.to_string(),
                scope: s.scope,
            })
            .collect()
    };

    let nodes = (0..n)
        .map(|i| {
            let doc_node = &doc.nodes[i];
            let c = contract(i);
            NodeReport {
                id: doc_node.id.clone(),
                kind: doc_node.kind.clone(),
                label: title(i),
                kind_label: c.map_or_else(|| doc_node.kind.clone(), |c| c.label.clone()),
                scope: c.map_or(FieldScope::Column, |c| c.scope()),
                export: c.is_some_and(|c| c.export),
                description: c.map_or_else(String::new, |c| c.description.clone()),
                enabled: doc_node.enabled,
                status: if !doc_node.enabled {
                    NodeStatus::Disabled
                } else if runnable[i] {
                    NodeStatus::Active
                } else {
                    NodeStatus::Blocked
                },
                reason: reasons[i].clone(),
                order: order.iter().position(|&o| o == i),
                inputs: c.map_or_else(Vec::new, |c| sockets(&c.inputs)),
                outputs: c.map_or_else(Vec::new, |c| {
                    c.outputs
                        .iter()
                        .map(|s| SocketReport {
                            name: s.name.to_string(),
                            scope: s.scope,
                        })
                        .collect()
                }),
                params: c.map_or_else(Vec::new, |c| {
                    c.params
                        .iter()
                        .map(|spec| ParamReport {
                            spec: *spec,
                            value: resolved[i].params.get(spec.name),
                        })
                        .collect()
                }),
                script: doc_node.kind == SCRIPT_KIND,
                stage: stages[i],
                neighborhood: c.and_then(|c| c.neighborhood),
                position: doc_node.position,
            }
        })
        .collect();

    let exports = order
        .iter()
        .filter(|&&i| contract(i).is_some_and(|c| c.export))
        .filter_map(|&i| {
            let input = contract(i)?.inputs.first()?;
            let (source, socket) = origin(incoming.get(&(i, input.name.to_string()))?.clone());
            Some(Export {
                name: export_name(&doc.nodes[i]),
                node: doc.nodes[i].id.clone(),
                scope: input.scope,
                slot: input_slots[&(i, input.name.to_string())],
                field: key(source, &socket),
            })
        })
        .collect();

    Analysis {
        order,
        prefix_len,
        schema: Schema { fields },
        bindings,
        nodes,
        edges,
        exports,
        world_samples,
    }
}

/// Would connecting an output of `from_node` into `to_node` create a cycle?
pub fn creates_cycle(doc: &GraphDoc, from_node: &str, to_node: &str) -> bool {
    // A cycle exists if `from_node` is reachable downstream of `to_node`.
    let mut stack = vec![to_node.to_owned()];
    let mut seen = std::collections::HashSet::new();

    while let Some(node) = stack.pop() {
        if node == from_node {
            return true;
        }
        if !seen.insert(node.clone()) {
            continue;
        }
        for edge in doc.edges.iter().filter(|e| e.from.node == node) {
            stack.push(edge.to.node.clone());
        }
    }

    false
}

// ---------------------------------------------------------------------------
// Execution

/// Accumulated wall time per node for one revision.
pub struct Timings {
    ids: Vec<String>,
    nanos: Vec<AtomicU64>,
    runs: Vec<AtomicU64>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StepTiming {
    pub id: String,
    /// Chunks this node ran for (cache hits do not run nodes).
    pub runs: u64,
    pub mean_ms: f64,
}

impl Timings {
    /// One slot per node, in `analysis.order` order, plus a last slot for the
    /// per-column world sampling (id `world`) when chunk nodes read world values.
    pub fn for_analysis(doc: &GraphDoc, analysis: &Analysis) -> Self {
        let ids: Vec<String> = analysis
            .order
            .iter()
            .map(|&i| doc.nodes[i].id.clone())
            .chain((!analysis.world_samples.is_empty()).then(|| WORLD_SAMPLING_ID.to_owned()))
            .collect();
        let zeros = || (0..ids.len()).map(|_| AtomicU64::new(0)).collect();

        Self {
            nanos: zeros(),
            runs: zeros(),
            ids,
        }
    }

    fn record(&self, slot: usize, started: Instant) {
        let elapsed = started.elapsed().as_nanos().min(u128::from(u64::MAX)) as u64;
        self.nanos[slot].fetch_add(elapsed, Ordering::Relaxed);
        self.runs[slot].fetch_add(1, Ordering::Relaxed);
    }

    pub fn snapshot(&self) -> Vec<StepTiming> {
        self.ids
            .iter()
            .enumerate()
            .map(|(i, id)| {
                let runs = self.runs[i].load(Ordering::Relaxed);
                let nanos = self.nanos[i].load(Ordering::Relaxed);
                StepTiming {
                    id: id.clone(),
                    runs,
                    mean_ms: if runs == 0 {
                        0.0
                    } else {
                        nanos as f64 / runs as f64 / 1e6
                    },
                }
            })
            .collect()
    }
}

/// A contiguous run of `analysis.order` (whole pipeline, prefix or suffix)
/// as one LayerGen chunk step.
pub struct DynPipeline {
    steps: Vec<(usize, BoxedStep)>,
    timings: Arc<Timings>,
}

impl DynPipeline {
    pub fn build(
        doc: &GraphDoc,
        resolved: &[ResolvedNode],
        analysis: &Analysis,
        #[cfg_attr(not(feature = "scripting"), allow(unused_variables))] world: &Arc<WorldStore>,
        range: std::ops::Range<usize>,
        timings: &Arc<Timings>,
    ) -> Result<Self, String> {
        // Sampling world outputs into column slots comes before every chunk
        // node (and runs in the prefix when there is a neighborhood node).
        let sampling = (range.start == 0 && !analysis.world_samples.is_empty()).then(|| {
            let step: BoxedStep = Box::new(WorldSampleStep {
                bindings: analysis.world_samples.clone(),
            });
            (analysis.order.len(), step)
        });

        let steps = range
            .filter(|&position| {
                !resolved[analysis.order[position]]
                    .contract
                    .as_ref()
                    .is_ok_and(|c| c.export || c.alias || c.scope().is_world_stage())
            })
            .map(|position| {
                let i = analysis.order[position];
                let node = &resolved[i];
                let bindings = analysis.bindings[i]
                    .as_ref()
                    .ok_or_else(|| "node is not runnable".to_owned())?;

                let step = match &node.implementation {
                    Some(Implementation::Builtin(Build::Chunk(build))) => {
                        build(bindings, &node.params)
                    }
                    #[cfg(feature = "scripting")]
                    Some(Implementation::Script(def)) => {
                        ScriptStep::build(def, bindings, world).map(|s| Box::new(s) as BoxedStep)
                    }
                    _ => Err("not a chunk node".to_owned()),
                };

                step.map(|step| (position, step))
                    .map_err(|error| format!("{}: {error}", doc.nodes[i].id))
            })
            .collect::<Result<Vec<_>, String>>()?;
        let steps = sampling.into_iter().chain(steps).collect();

        Ok(Self {
            steps,
            timings: Arc::clone(timings),
        })
    }

    /// True when no node in the range writes voxel outputs (its whole effect
    /// is column chunk data, safe to cache and restore).
    pub fn columns_only(
        resolved: &[ResolvedNode],
        analysis: &Analysis,
        range: std::ops::Range<usize>,
    ) -> bool {
        range.into_iter().all(|position| {
            resolved[analysis.order[position]]
                .contract
                .as_ref()
                .is_ok_and(|c| {
                    c.alias
                        || c.scope().is_world_stage()
                        || c.outputs.iter().all(|o| o.scope == FieldScope::Column)
                })
        })
    }
}

impl ChunkStep<Voxel, ColumnData, WorldStore> for DynPipeline {
    fn run(
        &self,
        cells: &mut DataSet<Voxel>,
        chunk: &mut ColumnData,
        context: &StepContext,
        world: &WorldStore,
    ) -> PipelineStepResult {
        for (slot, step) in &self.steps {
            let started = Instant::now();
            step.run(cells, chunk, context, world)?;
            self.timings.record(*slot, started);
        }

        Ok(())
    }
}

/// Timing id of the per-column world sampling step.
pub const WORLD_SAMPLING_ID: &str = "world";

/// Copies the world outputs that chunk nodes read into column slots.
struct WorldSampleStep {
    bindings: Vec<WorldSampleBinding>,
}

impl ChunkStep<Voxel, ColumnData, WorldStore> for WorldSampleStep {
    fn run(
        &self,
        _cells: &mut DataSet<Voxel>,
        chunk: &mut ColumnData,
        context: &StepContext,
        world: &WorldStore,
    ) -> PipelineStepResult {
        let origin = context.origin();

        for (column, x, y, _) in chunk.columns.iter_mut() {
            let values = world.graph().eval(origin.0 + x as i64, origin.1 + y as i64);
            for binding in &self.bindings {
                column[binding.column_slot] = values[binding.world_slot] as f32;
            }
        }

        Ok(())
    }
}

/// Where the atlas grids of a world build come from.
#[derive(Clone, Copy)]
pub enum AtlasSource<'a> {
    /// Run the atlas nodes (with progress / cancellation hooks).
    Compute(AtlasHooks<'a>),
    /// A saved atlas: no atlas node runs, the grids must all be there.
    Stored(&'a Atlas),
    /// Zero-filled grids, nothing computed: a structurally complete world
    /// for editors while the real atlas builds in the background.
    Placeholder,
}

/// Compiles the runnable world-scope nodes of an analyzed graph and runs its
/// atlas nodes, in graph order (or takes their grids from `source`).
pub fn build_world_stage(
    doc: &GraphDoc,
    resolved: &[ResolvedNode],
    analysis: &Analysis,
    context: &WorldBuildContext,
    atlas_context: AtlasContext,
    source: AtlasSource<'_>,
) -> Result<(WorldGraph, Atlas), String> {
    let implementation = |i: usize| match &resolved[i].implementation {
        Some(Implementation::Builtin(build)) => Some(*build),
        _ => None,
    };
    let total = analysis
        .order
        .iter()
        .filter(|&&i| matches!(implementation(i), Some(Build::Atlas(_) | Build::Rasterize)))
        .count();

    let mut steps: Vec<(usize, BoxedWorldStep)> = Vec::new();
    let mut atlas = Atlas::empty(atlas_context);
    let hooks = match source {
        AtlasSource::Compute(hooks) => hooks,
        _ => AtlasHooks::default(),
    };
    let stored = match source {
        AtlasSource::Stored(atlas) => Some(atlas),
        _ => None,
    };
    let mut run = AtlasRun::new(hooks, total);
    let mut done = 0;

    if let Some(stored) = stored {
        if stored.context() != &atlas_context {
            return Err("the saved atlas belongs to a different world size or seed".to_owned());
        }
        atlas.grids = stored.grids.clone();
        atlas.rivers = stored.rivers.clone();
    }

    for &i in &analysis.order {
        let Some(build) = implementation(i) else {
            continue;
        };
        let id = &doc.nodes[i].id;
        let bindings = || {
            analysis.bindings[i]
                .as_ref()
                .ok_or_else(|| format!("{id}: node is not runnable"))
        };
        let in_slot = |name: &str| bindings()?.input(name).map_err(|e| format!("{id}: {e}"));
        let out_slot = |name: &str| bindings()?.output(name).map_err(|e| format!("{id}: {e}"));
        let outputs = || {
            resolved[i]
                .contract
                .as_ref()
                .map(|c| c.outputs.clone())
                .map_err(Clone::clone)
        };
        let params = &resolved[i].params;

        match build {
            Build::World(build) => {
                let step = build(bindings()?, params, context).map_err(|e| format!("{id}: {e}"))?;
                steps.push((i, step));
            }
            Build::Sample => {
                let grid = atlas
                    .grids
                    .get(in_slot("in")?)
                    .cloned()
                    .flatten()
                    .ok_or_else(|| format!("{id}: its atlas input has no grid"))?;
                steps.push((
                    i,
                    Box::new(SampleStep {
                        grid,
                        context: atlas_context,
                        out: out_slot("out")?,
                        bilinear: params.get("mode") >= 0.5,
                    }),
                ));
            }
            Build::Rasterize | Build::Atlas(_) if matches!(source, AtlasSource::Placeholder) => {
                for output in outputs()? {
                    store_grid(
                        &mut atlas.grids,
                        out_slot(&output.name)?,
                        atlas_context.grid(0.0),
                    );
                }
            }
            Build::Rasterize | Build::Atlas(_) if stored.is_some() => {
                // A saved atlas: check its grids are there instead of computing them.
                for output in outputs()? {
                    if atlas.grid(out_slot(&output.name)?).is_none() {
                        return Err(format!(
                            "{id}: the saved atlas has no grid for `{}`",
                            output.name
                        ));
                    }
                }
            }
            Build::Rasterize => {
                run.begin(id, done);
                let ancestors = world_ancestors(analysis, doc, i);
                let chosen: Vec<&BoxedWorldStep> = steps
                    .iter()
                    .filter(|(node, _)| ancestors.contains(node))
                    .map(|(_, step)| step)
                    .collect();
                let grid = rasterize(
                    &chosen,
                    in_slot("in")?,
                    params.get("supersample").round().max(1.0) as u32,
                    &atlas_context,
                    &mut run,
                )
                .map_err(|e| format!("{id}: {e}"))?;
                store_grid(&mut atlas.grids, out_slot("out")?, grid);
                done += 1;
                run.progress(1.0);
            }
            Build::Atlas(build) => {
                run.begin(id, done);
                let stage =
                    build(bindings()?, params, &atlas_context).map_err(|e| format!("{id}: {e}"))?;
                let mut io = AtlasIo::new(&atlas_context, &atlas.grids);
                stage
                    .run(&mut io, &mut run)
                    .map_err(|e| format!("{id}: {e}"))?;
                let (written, rivers) = io.finish();

                for output in outputs()? {
                    let slot = out_slot(&output.name)?;
                    if !written.iter().any(|(s, _)| *s == slot) {
                        return Err(format!("{id}: output `{}` was not written", output.name));
                    }
                }
                for (slot, grid) in written {
                    store_grid(&mut atlas.grids, slot, grid);
                }
                if let Some(rivers) = rivers {
                    atlas.rivers.push((id.clone(), rivers));
                }
                done += 1;
                run.progress(1.0);
            }
            _ => {}
        }
    }

    atlas.snapshots = run.into_snapshots();
    Ok((
        WorldGraph {
            steps: steps.into_iter().map(|(_, step)| step).collect(),
        },
        atlas,
    ))
}

fn store_grid(grids: &mut Vec<Option<Arc<AtlasGrid>>>, slot: usize, grid: AtlasGrid) {
    if grids.len() <= slot {
        grids.resize(slot + 1, None);
    }
    grids[slot] = Some(Arc::new(grid));
}

/// Runnable nodes upstream of node `i` (through valid edges).
fn world_ancestors(analysis: &Analysis, doc: &GraphDoc, i: usize) -> Vec<usize> {
    let index: HashMap<&str, usize> = doc
        .nodes
        .iter()
        .enumerate()
        .map(|(k, node)| (node.id.as_str(), k))
        .collect();
    let mut found = Vec::new();
    let mut stack = vec![i];

    while let Some(node) = stack.pop() {
        let id = &doc.nodes[node].id;
        for edge in analysis
            .edges
            .iter()
            .filter(|e| e.valid && &e.to.node == id)
        {
            if let Some(&source) = index.get(edge.from.node.as_str()) {
                if !found.contains(&source) {
                    found.push(source);
                    stack.push(source);
                }
            }
        }
    }

    found
}

/// Evaluates world steps at every atlas cell (averaging `supersample`^2
/// points per cell), with rows split across threads.
fn rasterize(
    steps: &[&BoxedWorldStep],
    slot: usize,
    supersample: u32,
    context: &AtlasContext,
    run: &mut AtlasRun<'_>,
) -> Result<AtlasGrid, String> {
    let (width, height) = context.grid;
    let row_len = width as usize;
    let threads = std::thread::available_parallelism()
        .map_or(4, |n| n.get())
        .min(16);
    let batch_rows = threads * 4;
    let mut values = vec![0.0f32; row_len * height as usize];
    let s = f64::from(supersample);

    let eval_row = |cy: u32, row: &mut [f32]| {
        for (cx, value) in row.iter_mut().enumerate() {
            let mut sum = 0.0;
            for sy in 0..supersample {
                for sx in 0..supersample {
                    let (x, y) = context.cell_point(
                        cx as u32,
                        cy,
                        (f64::from(sx) + 0.5) / s,
                        (f64::from(sy) + 0.5) / s,
                    );
                    let mut slots = [0.0; WORLD_SLOTS];
                    for step in steps {
                        step.run(x, y, &mut slots);
                    }
                    sum += slots[slot];
                }
            }
            *value = (sum / (s * s)) as f32;
        }
    };

    let mut first = 0usize;
    for rows in values.chunks_mut(row_len * batch_rows) {
        run.check()?;
        let rows_here = rows.len() / row_len;
        let per_thread = rows_here.div_ceil(threads).max(1);
        std::thread::scope(|scope| {
            for (k, block) in rows.chunks_mut(row_len * per_thread).enumerate() {
                let start = first + k * per_thread;
                let eval_row = &eval_row;
                scope.spawn(move || {
                    for (r, row) in block.chunks_mut(row_len).enumerate() {
                        eval_row((start + r) as u32, row);
                    }
                });
            }
        });
        first += rows_here;
        run.progress(first as f64 / f64::from(height));
    }

    AtlasGrid::from_values(width, height, values)
}

/// `world_signature` of a graph without world nodes.
pub const EMPTY_WORLD_SIGNATURE: &str = "[[],[]]";

/// Identifies the world-scope part of a graph: the nodes (kind, enabled,
/// params) and the wires between them. Two docs with the same signature (and
/// seed) build the same world, so editors can keep an existing world while
/// chunk and voxel nodes change.
pub fn world_signature(doc: &GraphDoc) -> String {
    let world_nodes: Vec<(&NodeDoc, ResolvedNode)> = doc
        .nodes
        .iter()
        .map(|node| (node, resolve(node)))
        .filter(|(_, resolved)| {
            resolved
                .contract
                .as_ref()
                .is_ok_and(|c| c.scope().is_world_stage())
        })
        .collect();

    let nodes: Vec<_> = world_nodes
        .iter()
        .map(|(node, resolved)| (&node.id, &node.kind, node.enabled, &resolved.params.values))
        .collect();
    let edges: Vec<_> = doc
        .edges
        .iter()
        .filter(|edge| world_nodes.iter().any(|(node, _)| node.id == edge.to.node))
        .collect();

    // The extent decides the world size and atlas grid: part of the world.
    // Format 2 graphs (no extent) keep their old signature.
    match &doc.extent {
        None => serde_json::to_string(&(nodes, edges)),
        Some(extent) => serde_json::to_string(&(nodes, edges, extent)),
    }
    .expect("signature values always serialize")
}

/// Builds the neighborhood node at `order[position]`, timed in that slot.
pub fn build_neighborhood(
    doc: &GraphDoc,
    resolved: &[ResolvedNode],
    analysis: &Analysis,
    position: usize,
    timings: &Arc<Timings>,
) -> Result<(Radius, TimedNeighborhood), String> {
    let i = analysis.order[position];
    let node = &resolved[i];
    let bindings = analysis.bindings[i]
        .as_ref()
        .ok_or_else(|| "node is not runnable".to_owned())?;

    match node.implementation {
        Some(Implementation::Builtin(Build::Neighborhood { radius, build })) => Ok((
            radius(&node.params),
            TimedNeighborhood {
                slot: position,
                inner: build(bindings, &node.params)
                    .map_err(|e| format!("{}: {e}", doc.nodes[i].id))?,
                timings: Arc::clone(timings),
            },
        )),
        _ => Err(format!("{} is not a neighborhood node", doc.nodes[i].id)),
    }
}

pub struct TimedNeighborhood {
    slot: usize,
    inner: BoxedNeighborhoodStep,
    timings: Arc<Timings>,
}

impl NeighborhoodChunkStep<Voxel, ColumnData, WorldStore> for TimedNeighborhood {
    fn run(
        &self,
        input: &NeighborhoodView<'_, Voxel>,
        chunks: &NeighborChunks<'_, ColumnData>,
        output: &mut DataSet<Voxel>,
        output_chunk: &mut ColumnData,
        context: &StepContext,
        world: &WorldStore,
    ) -> PipelineStepResult {
        let started = Instant::now();
        self.inner
            .run(input, chunks, output, output_chunk, context, world)?;
        self.timings.record(self.slot, started);
        Ok(())
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::graph::EdgeDoc;

    pub fn node(id: &str, kind: &str) -> NodeDoc {
        NodeDoc {
            id: id.into(),
            kind: kind.into(),
            enabled: true,
            position: (0.0, 0.0),
            params: BTreeMap::new(),
            script: None,
            label: None,
        }
    }

    pub fn edge(from: &str, out: &str, to: &str, input: &str) -> EdgeDoc {
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

    fn run(doc: &GraphDoc) -> Analysis {
        let resolved: Vec<_> = doc.nodes.iter().map(resolve).collect();
        analyze(doc, &resolved)
    }

    fn report<'a>(analysis: &'a Analysis, id: &str) -> &'a NodeReport {
        analysis.nodes.iter().find(|n| n.id == id).unwrap()
    }

    fn named(id: &str, kind: &str, label: &str) -> NodeDoc {
        NodeDoc {
            label: Some(label.into()),
            ..node(id, kind)
        }
    }

    fn doc(nodes: Vec<NodeDoc>, edges: Vec<EdgeDoc>) -> GraphDoc {
        GraphDoc {
            format: crate::graph::FORMAT,
            seed: 1,
            frames: Vec::new(),
            world: Default::default(),
            extent: None,
            nodes,
            edges,
        }
    }

    // Chunk nodes read a world node here: `world_noise` has no inputs, and
    // its value reaches chunk inputs through an implicit sampled column slot.

    #[test]
    fn order_follows_edges_not_list_or_layout() {
        let graph = doc(
            vec![
                node("n3", "export_column"),
                node("n2", "solid"),
                node("n1", "world_noise"),
            ],
            vec![
                edge("n1", "noise", "n3", "value"),
                edge("n1", "noise", "n2", "elevation"),
            ],
        );
        let analysis = run(&graph);
        let ids: Vec<_> = analysis
            .order
            .iter()
            .map(|&i| graph.nodes[i].id.as_str())
            .collect();

        assert_eq!(ids, ["n1", "n2", "n3"]);
        assert_eq!(analysis.schema.slot("n1.noise"), Ok(0)); // world slots
        assert_eq!(analysis.world_samples[0].column_slot, 0); // sampled for n2 and n3
        assert_eq!(analysis.world_samples.len(), 1);
        assert_eq!(analysis.schema.slot("n2.solid"), Ok(0)); // voxel slots
    }

    #[test]
    fn reprocessing_gets_its_own_slot_and_inherits_display() {
        // noise → smooth → an export reads the smoothed value.
        let graph = doc(
            vec![
                node("n1", "world_noise"),
                node("n2", "smooth"),
                node("n3", "export_column"),
            ],
            vec![
                edge("n1", "noise", "n2", "value"),
                edge("n2", "smoothed", "n3", "value"),
            ],
        );
        let analysis = run(&graph);
        let smoothed = analysis
            .schema
            .fields
            .iter()
            .find(|f| f.key == "n2.smoothed")
            .unwrap();

        assert_ne!(smoothed.slot, analysis.world_samples[0].column_slot);
        assert_eq!(smoothed.ramp, "grayscale"); // inherited from the noise
        assert_eq!(smoothed.max, 1.0);
        // The neighborhood node comes right after the (non-chunk) world node.
        assert_eq!(analysis.prefix_len, Some(1));
        assert_eq!(report(&analysis, "n3").stage, Some(Stage::Suffix));
        assert_eq!(
            analysis.bindings[2].as_ref().unwrap().input("value"),
            Ok(smoothed.slot)
        );
    }

    #[test]
    fn blocked_reasons_cover_unconnected_disabled_and_downstream() {
        let mut nodes = vec![
            node("n1", "world_noise"),
            node("n2", "temperature"),
            node("n3", "humidity"),
            node("n4", "solid"),
        ];
        nodes[0].enabled = false;
        let graph = doc(
            nodes,
            vec![
                edge("n1", "noise", "n2", "elevation"),
                edge("n1", "noise", "n3", "elevation"),
                edge("n2", "temperature", "n3", "temperature"),
            ],
        );
        let analysis = run(&graph);

        assert!(analysis.order.is_empty());
        assert!(report(&analysis, "n2")
            .reason
            .as_deref()
            .unwrap()
            .contains("disabled node"));
        assert!(report(&analysis, "n3")
            .reason
            .as_deref()
            .unwrap()
            .contains("disabled node"));
        assert_eq!(
            report(&analysis, "n4").reason.as_deref(),
            Some("input `elevation` is not connected")
        );
    }

    #[test]
    fn invalid_edges_are_reported_and_unknown_kinds_block() {
        let graph = doc(
            vec![
                node("n1", "world_noise"),
                node("n2", "solid"),
                node("n3", "mystery"),
            ],
            vec![
                edge("n1", "nope", "n2", "elevation"), // dangling socket
                edge("n9", "x", "n2", "elevation"),    // dangling node
            ],
        );
        let analysis = run(&graph);

        assert!(analysis.edges.iter().all(|e| !e.valid));
        assert!(report(&analysis, "n2")
            .reason
            .as_deref()
            .unwrap()
            .contains("not connected"));
        assert!(report(&analysis, "n3")
            .reason
            .as_deref()
            .unwrap()
            .contains("unknown node kind"));
    }

    #[test]
    fn cycles_are_detected_for_connect() {
        let graph = doc(
            vec![node("n1", "smooth"), node("n2", "smooth")],
            vec![edge("n1", "smoothed", "n2", "value")],
        );

        assert!(creates_cycle(&graph, "n2", "n1"));
        assert!(!creates_cycle(&graph, "n1", "n2"));
        assert!(creates_cycle(&graph, "n1", "n1"));
    }

    #[test]
    fn second_neighborhood_node_is_blocked() {
        let graph = doc(
            vec![
                node("n1", "world_noise"),
                node("n2", "slope"),
                node("n3", "smooth"),
            ],
            vec![
                edge("n1", "noise", "n2", "elevation"),
                edge("n1", "noise", "n3", "value"),
            ],
        );
        let analysis = run(&graph);

        assert_eq!(report(&analysis, "n2").stage, Some(Stage::Neighborhood));
        assert!(report(&analysis, "n3")
            .reason
            .as_deref()
            .unwrap()
            .contains("only one neighborhood node"));
    }

    #[test]
    fn params_resolve_with_defaults_and_clamping() {
        let mut smooth = node("n1", "smooth");
        smooth.params.insert("radius".into(), 99.0);
        let resolved = resolve(&smooth);
        let contract = resolved.contract.unwrap();

        assert_eq!(resolved.params.get("radius"), 8.0);
        assert_eq!(contract.neighborhood, Some((8, 8, 0)));
    }

    #[test]
    fn exports_publish_their_input_slot_by_label_and_must_be_unique() {
        let mut height = node("n2", "export_column");
        height.label = Some("height".into());
        let mut clash = node("n3", "export_column");
        clash.label = Some("height".into());
        let unnamed = node("n4", "export_column");

        let graph = doc(
            vec![node("n1", "world_noise"), height, clash, unnamed],
            vec![
                edge("n1", "noise", "n2", "value"),
                edge("n1", "noise", "n3", "value"),
                edge("n1", "noise", "n4", "value"),
            ],
        );
        let analysis = run(&graph);
        let names: Vec<_> = analysis.exports.iter().map(|e| e.name.as_str()).collect();

        assert_eq!(names, ["height", DEFAULT_EXPORT_NAME]);
        // A world value exported to the game is its sampled column slot.
        assert_eq!(
            analysis.exports[0].slot,
            analysis.world_samples[0].column_slot
        );
        assert_eq!(analysis.exports[0].field, "n1.noise");
        assert!(report(&analysis, "n3")
            .reason
            .as_deref()
            .unwrap()
            .contains("export name `height` is already used"));
        assert_eq!(report(&analysis, "n2").label, "height");
        assert!(report(&analysis, "n2").export);
        // Exports add no slots of their own.
        assert_eq!(analysis.schema.fields.len(), 1);
    }

    #[test]
    fn reroutes_alias_slots_chain_and_are_looked_through() {
        let mut height = node("n5", "export_column");
        height.label = Some("height".into());

        // noise → reroute → reroute → two exports; solid via a voxel reroute.
        let graph = doc(
            vec![
                node("n1", "world_noise"),
                node("n2", "reroute_column"),
                node("n3", "reroute_column"),
                named("n4", "export_column", "second"),
                height,
                node("n6", "solid"),
                node("n7", "reroute_voxel"),
                node("n8", "export_voxel"),
            ],
            vec![
                edge("n1", "noise", "n2", "in"),
                edge("n2", "out", "n3", "in"),
                edge("n3", "out", "n4", "value"),
                edge("n3", "out", "n5", "value"),
                edge("n1", "noise", "n6", "elevation"),
                edge("n6", "solid", "n7", "in"),
                edge("n7", "out", "n8", "value"),
            ],
        );
        let analysis = run(&graph);
        let sampled = analysis.world_samples[0].column_slot;

        // Reroutes run, take no slots and add no fields.
        assert!(["n2", "n3", "n7"]
            .iter()
            .all(|id| report(&analysis, id).status == NodeStatus::Active));
        let keys: Vec<_> = analysis
            .schema
            .fields
            .iter()
            .map(|f| f.key.as_str())
            .collect();
        assert_eq!(keys, ["n1.noise", "n6.solid"]);
        assert_eq!(
            analysis.world_samples.len(),
            1,
            "one sampled copy serves every reader"
        );

        // Consumers read the sampled slot; exports report the real producer.
        assert_eq!(
            analysis.bindings[3].as_ref().unwrap().input("value"),
            Ok(sampled)
        );
        let export = |name: &str| analysis.exports.iter().find(|e| e.name == name).unwrap();
        assert_eq!(export("height").slot, sampled);
        assert_eq!(export("height").field, "n1.noise");
        assert_eq!(export("second").slot, sampled);
        assert_eq!(export(DEFAULT_EXPORT_NAME).field, "n6.solid");
        assert_eq!(
            export(DEFAULT_EXPORT_NAME).slot,
            analysis.schema.slot("n6.solid").unwrap()
        );

        // A reroute only passes the same type through.
        let mismatched = doc(
            vec![node("n1", "solid"), node("n2", "reroute_column")],
            vec![edge("n1", "solid", "n2", "in")],
        );
        assert!(!run(&mismatched).edges[0].valid);
    }
}
