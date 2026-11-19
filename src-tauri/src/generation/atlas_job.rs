//! Background atlas builds.
//!
//! Atlas nodes (erosion, flow, …) can take seconds to minutes, so an edit
//! that needs a new atlas is published at once with a placeholder world
//! (zero-filled atlas grids, see `WorldStore::placeholder`) and the real
//! world is built here, on its own thread. When it is ready it is published
//! as the next revision, with the same doc. A newer edit that needs a
//! different atlas cancels the running build. Finished worlds are kept in a
//! small cache, so undoing back to a recent world does not rebuild it.

use std::{
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex, RwLock,
    },
    thread,
};

use layergen::PipelineConfig;
use serde::Serialize;
use worldgraph::{AtlasHooks, AtlasProgress, GraphDoc, GraphPipeline, WorldStore};

use super::{world_stats, Active, State};

/// Finished worlds kept for reuse (a few undo steps' worth).
const READY_WORLDS: usize = 3;

/// What the atlas builder is doing (polled by the frontend).
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "state", rename_all = "camelCase")]
pub enum AtlasStatus {
    /// Nothing building: the published world is complete.
    Idle,
    #[serde(rename_all = "camelCase")]
    Building {
        /// Id of the atlas node running ("" before the first one starts).
        node: String,
        index: usize,
        total: usize,
        fraction: f64,
    },
    /// The last build failed (the placeholder world stays published).
    Failed { error: String },
}

/// Identifies a world: seed and `world_signature`.
type WorldKey = (u64, String);

pub(super) struct AtlasJobs {
    /// Bumped by every new build (and by cancelling): a build whose
    /// generation is no longer current stops and publishes nothing.
    generation: AtomicU64,
    status: Mutex<AtlasStatus>,
    running: Mutex<Option<(WorldKey, u64)>>,
    ready: Mutex<Vec<(WorldKey, Arc<WorldStore>)>>,
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|p| p.into_inner())
}

impl AtlasJobs {
    pub fn new() -> Self {
        Self {
            generation: AtomicU64::new(0),
            status: Mutex::new(AtlasStatus::Idle),
            running: Mutex::new(None),
            ready: Mutex::new(Vec::new()),
        }
    }

    pub fn status(&self) -> AtlasStatus {
        lock(&self.status).clone()
    }

    /// A finished world for this seed and signature, if one is cached.
    pub fn lookup(&self, seed: u64, signature: &str) -> Option<Arc<WorldStore>> {
        lock(&self.ready)
            .iter()
            .find(|((s, sig), _)| *s == seed && sig == signature)
            .map(|(_, world)| Arc::clone(world))
    }

    fn remember(&self, key: WorldKey, world: Arc<WorldStore>) {
        let mut ready = lock(&self.ready);
        ready.retain(|(k, _)| *k != key);
        ready.push((key, world));
        if ready.len() > READY_WORLDS {
            ready.remove(0);
        }
    }

    /// Stops any running build (nothing it computes is published).
    fn cancel(&self) {
        self.generation.fetch_add(1, Ordering::SeqCst);
        *lock(&self.running) = None;
        *lock(&self.status) = AtlasStatus::Idle;
    }

    /// Makes sure the real world for the published revision is being built:
    /// starts a build when the active world is a placeholder (unless one for
    /// the same world is already running), cancels builds that are no longer
    /// needed.
    pub fn sync(self: &Arc<Self>, state: &Arc<RwLock<State>>) {
        let (doc, active) = {
            let state = state.read().unwrap_or_else(|p| p.into_inner());
            (state.doc.clone(), Arc::clone(&state.active))
        };

        if !active.world.is_placeholder() {
            if lock(&self.running).is_some() {
                self.cancel();
            }
            return;
        }

        let key: WorldKey = (active.config.seed(), active.world.signature().to_owned());
        let mut running = lock(&self.running);
        if running.as_ref().is_some_and(|(k, _)| *k == key) {
            return;
        }

        let generation = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        *running = Some((key.clone(), generation));
        *lock(&self.status) = AtlasStatus::Building {
            node: String::new(),
            index: 0,
            total: 0,
            fraction: 0.0,
        };
        drop(running);

        let jobs = Arc::clone(self);
        let state = Arc::clone(state);
        let config = active.config;
        thread::Builder::new()
            .name("atlas-build".into())
            .spawn(move || jobs.build(&state, doc, config, key, generation))
            .expect("the atlas build thread starts");
    }

    fn current(&self, generation: u64) -> bool {
        self.generation.load(Ordering::SeqCst) == generation
    }

    fn build(
        &self,
        state: &RwLock<State>,
        doc: GraphDoc,
        config: PipelineConfig,
        key: WorldKey,
        generation: u64,
    ) {
        let progress = |p: &AtlasProgress| {
            if self.current(generation) {
                *lock(&self.status) = AtlasStatus::Building {
                    node: p.node.clone(),
                    index: p.index,
                    total: p.total,
                    fraction: p.fraction,
                };
            }
        };
        let cancelled = || !self.current(generation);
        let result = WorldStore::for_doc_with(
            &doc,
            &config,
            AtlasHooks {
                progress: Some(&progress),
                cancelled: Some(&cancelled),
                ..AtlasHooks::default()
            },
        );

        let outcome = result.and_then(|world| {
            let world = Arc::new(world);
            self.remember(key.clone(), Arc::clone(&world));
            self.publish(state, world, &key, generation)
        });

        if self.current(generation) {
            *lock(&self.running) = None;
            *lock(&self.status) = match outcome {
                Ok(()) => AtlasStatus::Idle,
                Err(error) => AtlasStatus::Failed { error },
            };
        }
    }

    /// Publishes the finished world as the next revision, if the published
    /// revision still waits for exactly this world.
    fn publish(
        &self,
        state: &RwLock<State>,
        world: Arc<WorldStore>,
        key: &WorldKey,
        generation: u64,
    ) -> Result<(), String> {
        let mut state = state.write().unwrap_or_else(|p| p.into_inner());
        let active = Arc::clone(&state.active);
        let waiting = active.world.is_placeholder()
            && active.config.seed() == key.0
            && active.world.signature() == key.1;
        if !self.current(generation) || !waiting {
            return Ok(());
        }

        let pipeline = GraphPipeline::build(&state.doc, active.config, &world)?;
        pipeline.validate(active.config, &world)?;
        let stats = world_stats(&pipeline, &world);

        state.active = Arc::new(Active {
            revision: active.revision + 1,
            config: active.config,
            world,
            stats,
            world_generation: active.world_generation + 1,
            pipeline,
        });
        Ok(())
    }
}
