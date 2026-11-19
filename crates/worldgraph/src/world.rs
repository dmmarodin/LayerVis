//! World stage: the shared data `D` every chunk job reads.
//!
//! A [`WorldStore`] is built once when a world is created. It holds the
//! compiled world-scope nodes of the graph (tectonics, elevation filters,
//! noise, …, see `terrain.rs` and `steps.rs`), which are pure functions of
//! `(x, y)`, and the atlas: whole-world grids the graph's atlas nodes computed
//! once (see `atlas.rs`). Everything is read-only afterwards. Chunk nodes read
//! world values through implicit sampling.

use std::sync::Arc;

use layergen::PipelineConfig;
use serde::Serialize;

use crate::{
    atlas::{Atlas, AtlasContext, AtlasHooks, DEFAULT_CELL_SIZE},
    biomes::BiomeTable,
    materials::MaterialTable,
    graph::GraphDoc,
    pipeline::{
        analyze, build_world_stage, resolve, world_signature, AtlasSource, Build, Implementation,
        ResolvedNode, WorldBuildContext, WorldGraph, EMPTY_WORLD_SIGNATURE,
    },
};

/// Upper bound on plate ids (the `plate` field's display range).
pub const MAX_PLATES: usize = 256;

/// Coarse facts about a world, for editor feedback and tests.
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorldStats {
    /// Fraction of the world at or above sea level.
    pub land_fraction: f64,
    /// Connected landmasses of meaningful size (tiny islets are ignored).
    pub landmasses: usize,
    /// Fraction of the world's border samples that are land (0 = ocean all round).
    pub edge_land: f64,
}

/// LayerGen shared data `D`: the world stage of one seed and graph.
pub struct WorldStore {
    world_min: (i64, i64, i64),
    world_size: (u32, u32, u32),
    seed: u64,
    /// The compiled world-scope nodes of the graph (see `for_doc`).
    graph: WorldGraph,
    /// `world_signature` of the doc `graph` was compiled from.
    signature: String,
    /// The atlas grids (empty when the graph has no atlas nodes).
    atlas: Arc<Atlas>,
    /// Built with zero-filled atlas grids (`placeholder`), not the real ones.
    placeholder: bool,
    /// Sea level in cells (`Extent::sea_level`); None: 0.3 × depth.
    sea: Option<u32>,
}

impl WorldStore {
    /// A world without world nodes (nothing to sample).
    pub fn new(config: &PipelineConfig) -> Self {
        let bounds = config.world();

        let origin = bounds.origin();
        let size = bounds.dimensions();

        Self {
            world_min: origin,
            world_size: size,
            seed: config.seed(),
            graph: WorldGraph::default(),
            signature: EMPTY_WORLD_SIGNATURE.to_owned(),
            placeholder: false,
            sea: None,
            atlas: Arc::new(Atlas::empty(AtlasContext::new(
                config.seed(),
                (origin.0, origin.1),
                (size.0, size.1),
                size.2,
                DEFAULT_CELL_SIZE,
            ))),
        }
    }

    /// The world a graph doc defines: its seed is `config`'s seed (callers
    /// build the config from `doc.seed`) and its world-scope nodes are
    /// compiled here, once. `GraphPipeline::build` refuses a world that was
    /// built from different world nodes.
    ///
    /// Atlas nodes run here, which can take a while: see `for_doc_with` for
    /// progress and cancellation.
    pub fn for_doc(doc: &GraphDoc, config: &PipelineConfig) -> Result<Self, String> {
        Self::build(doc, config, AtlasSource::Compute(AtlasHooks::default()))
    }

    /// `for_doc` with progress reports and cancellation for the atlas build
    /// (a cancelled build returns `Err(ATLAS_CANCELLED)`).
    pub fn for_doc_with(
        doc: &GraphDoc,
        config: &PipelineConfig,
        hooks: AtlasHooks<'_>,
    ) -> Result<Self, String> {
        Self::build(doc, config, AtlasSource::Compute(hooks))
    }

    /// The world of a graph with zero-filled atlas grids: no atlas node runs.
    /// Structurally complete (pipelines build against it), so editors can
    /// publish an edit at once and build the real atlas in the background.
    pub fn placeholder(doc: &GraphDoc, config: &PipelineConfig) -> Result<Self, String> {
        let mut store = Self::build(doc, config, AtlasSource::Placeholder)?;
        store.placeholder = true;
        Ok(store)
    }

    /// True for `placeholder` worlds.
    pub fn is_placeholder(&self) -> bool {
        self.placeholder
    }

    /// Does building this graph's world run atlas nodes (the slow part)?
    pub fn has_atlas_work(doc: &GraphDoc) -> bool {
        let resolved: Vec<ResolvedNode> = doc.nodes.iter().map(resolve).collect();
        let analysis = analyze(doc, &resolved);
        analysis.order.iter().any(|&i| {
            matches!(
                resolved[i].implementation,
                Some(Implementation::Builtin(Build::Atlas(_) | Build::Rasterize))
            )
        })
    }

    /// The world of a graph with a saved atlas (`Atlas::read`): no atlas node
    /// runs. The atlas must come from the same graph's world nodes, seed and
    /// size.
    pub fn from_atlas(
        doc: &GraphDoc,
        config: &PipelineConfig,
        atlas: &Atlas,
    ) -> Result<Self, String> {
        if atlas.signature() != world_signature(doc) {
            return Err("the saved atlas was built from different world nodes".to_owned());
        }
        Self::build(doc, config, AtlasSource::Stored(atlas))
    }

    fn build(
        doc: &GraphDoc,
        config: &PipelineConfig,
        source: AtlasSource<'_>,
    ) -> Result<Self, String> {
        let mut store = Self::new(config);
        store.sea = doc.extent.and_then(|extent| extent.sea_level);
        let cell_size = match &doc.extent {
            None => DEFAULT_CELL_SIZE,
            Some(extent) => {
                extent.validate()?;
                let bounds = config.world();
                if bounds.dimensions() != (extent.size.0, extent.size.1, extent.depth)
                    || bounds.origin() != extent.origin()
                {
                    return Err(
                        "the pipeline config's world bounds do not match the graph's \
                                extent (build the config from `Extent`)"
                            .to_owned(),
                    );
                }
                extent.cell_size
            }
        };

        let resolved: Vec<ResolvedNode> = doc.nodes.iter().map(resolve).collect();
        let analysis = analyze(doc, &resolved);
        let context = WorldBuildContext {
            seed: config.seed(),
            origin: (store.world_min.0, store.world_min.1),
            size: (store.world_size.0, store.world_size.1),
            depth: store.world_size.2,
            sea_level: store.sea,
        };
        let atlas_context = AtlasContext::new(
            context.seed,
            context.origin,
            context.size,
            context.depth,
            cell_size,
        )
        .with_sea_level(store.sea);

        let (graph, mut atlas) =
            build_world_stage(doc, &resolved, &analysis, &context, atlas_context, source)?;
        store.graph = graph;
        store.signature = world_signature(doc);
        atlas.signature = store.signature.clone();
        store.atlas = Arc::new(atlas);
        Ok(store)
    }

    /// The atlas grids, rivers and snapshots (empty without atlas nodes).
    pub fn atlas(&self) -> &Arc<Atlas> {
        &self.atlas
    }

    /// The world seed.
    pub fn seed(&self) -> u64 {
        self.seed
    }

    /// The world's material ids (voxel materials, strata, material grids).
    pub fn materials(&self) -> &Arc<MaterialTable> {
        self.atlas.materials()
    }

    /// The world's biome ids (biome grids and exports).
    pub fn biomes(&self) -> &Arc<BiomeTable> {
        self.atlas.biomes()
    }

    /// The compiled world-scope nodes (empty for `new`).
    pub fn graph(&self) -> &WorldGraph {
        &self.graph
    }

    pub fn signature(&self) -> &str {
        &self.signature
    }

    pub fn contains_column(&self, x: i64, y: i64) -> bool {
        x >= self.world_min.0
            && y >= self.world_min.1
            && x < self.world_min.0 + i64::from(self.world_size.0)
            && y < self.world_min.1 + i64::from(self.world_size.1)
    }

    /// World depth in cells (elevation range).
    pub fn depth(&self) -> f32 {
        self.world_size.2 as f32
    }

    /// Elevation below which a column is ocean. Matches the terrain ramp's
    /// water band and the oceanic plate base height.
    pub fn sea_level(&self) -> f32 {
        match self.sea {
            Some(sea) => sea as f32,
            None => self.depth() * 0.3,
        }
    }

    /// The graph's sea level setting (`Extent::sea_level`), if any.
    pub fn sea_level_setting(&self) -> Option<u32> {
        self.sea
    }

    /// Low-frequency noise in [0, 1] for moisture variation.
    pub fn moisture_noise(&self, x: i64, y: i64) -> f32 {
        fbm(self.seed ^ 0x3017, x as f64, y as f64, 384.0, 3)
    }

    /// Seeded fractal noise in [0, 1] for scripts.
    pub fn script_noise(&self, x: i64, y: i64, wavelength: f64) -> f32 {
        fbm(
            self.seed ^ 0x5C21,
            x as f64,
            y as f64,
            wavelength.max(1.0),
            4,
        )
    }

    /// Signed latitude in [-1, 1] across the world's Y extent.
    pub fn latitude_at(&self, y: i64) -> f32 {
        let span = self.world_size.1 as f64;
        let t = (y - self.world_min.1) as f64 / span;

        (t * 2.0 - 1.0) as f32
    }

    /// Land fraction, landmass count and border land on a coarse grid
    /// (`resolution` samples per side), reading the world slot `elevation`
    /// (an elevation field in cells). Cheap enough to run per world.
    pub fn stats(&self, elevation: usize, resolution: usize) -> WorldStats {
        let n = resolution.max(4);
        let (w, h) = (f64::from(self.world_size.0), f64::from(self.world_size.1));
        let sea = f64::from(self.sea_level());

        let land: Vec<bool> = (0..n * n)
            .map(|i| {
                let x = self.world_min.0 + (((i % n) as f64 + 0.5) / n as f64 * w) as i64;
                let y = self.world_min.1 + (((i / n) as f64 + 0.5) / n as f64 * h) as i64;
                self.graph.eval(x, y)[elevation] >= sea
            })
            .collect();

        let land_cells = land.iter().filter(|&&l| l).count();
        let border: Vec<bool> = (0..n)
            .flat_map(|i| [(i, 0), (i, n - 1), (0, i), (n - 1, i)])
            .map(|(x, y)| land[x + y * n])
            .collect();

        // 4-connected components; islets under 0.4% of the map do not count.
        let min_cells = (n * n / 250).max(2);
        let mut seen = vec![false; n * n];
        let mut landmasses = 0;

        for start in 0..n * n {
            if !land[start] || seen[start] {
                continue;
            }
            let mut size = 0;
            let mut stack = vec![start];
            seen[start] = true;

            while let Some(cell) = stack.pop() {
                size += 1;
                let (cx, cy) = (cell % n, cell / n);
                let neighbors = [
                    (cx > 0).then(|| cell - 1),
                    (cx + 1 < n).then(|| cell + 1),
                    (cy > 0).then(|| cell - n),
                    (cy + 1 < n).then(|| cell + n),
                ];
                for next in neighbors.into_iter().flatten() {
                    if land[next] && !seen[next] {
                        seen[next] = true;
                        stack.push(next);
                    }
                }
            }

            if size >= min_cells {
                landmasses += 1;
            }
        }

        WorldStats {
            land_fraction: land_cells as f64 / (n * n) as f64,
            landmasses,
            edge_land: border.iter().filter(|&&l| l).count() as f64 / border.len() as f64,
        }
    }
}

pub(crate) fn smoothstep(edge0: f64, edge1: f64, x: f64) -> f64 {
    let t = ((x - edge0) / (edge1 - edge0)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// Small deterministic RNG for world-model generation.
pub(crate) struct SplitMix(u64);

impl SplitMix {
    pub fn new(seed: u64) -> Self {
        Self(seed)
    }

    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    pub fn next_f64(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }
}

pub(crate) fn hash(seed: u64, x: i64, y: i64) -> u64 {
    let mut h = seed ^ 0x9E37_79B9_7F4A_7C15;
    h ^= (x as u64).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    h = h.rotate_left(31).wrapping_mul(0x94D0_49BB_1331_11EB);
    h ^= (y as u64).wrapping_mul(0xD6E8_FEB8_6659_FD93);
    h ^= h >> 32;
    h = h.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    h ^ (h >> 29)
}

fn lattice(seed: u64, x: i64, y: i64) -> f64 {
    (hash(seed, x, y) >> 11) as f64 / (1u64 << 53) as f64
}

fn smooth(t: f64) -> f64 {
    t * t * (3.0 - 2.0 * t)
}

fn value_noise(seed: u64, x: f64, y: f64) -> f64 {
    let (x0, y0) = (x.floor(), y.floor());
    let (tx, ty) = (smooth(x - x0), smooth(y - y0));
    let (xi, yi) = (x0 as i64, y0 as i64);

    let a = lattice(seed, xi, yi);
    let b = lattice(seed, xi + 1, yi);
    let c = lattice(seed, xi, yi + 1);
    let d = lattice(seed, xi + 1, yi + 1);

    let top = a + (b - a) * tx;
    let bottom = c + (d - c) * tx;

    top + (bottom - top) * ty
}

/// Fractal value noise in [0, 1].
pub(crate) fn fbm(seed: u64, x: f64, y: f64, wavelength: f64, octaves: u32) -> f32 {
    let mut total = 0.0;
    let mut amplitude = 1.0;
    let mut norm = 0.0;
    let mut frequency = 1.0 / wavelength;

    for octave in 0..octaves {
        let octave_seed = seed.wrapping_add(u64::from(octave) * 0x1000_0001);
        total += value_noise(octave_seed, x * frequency, y * frequency) * amplitude;
        norm += amplitude;
        amplitude *= 0.5;
        frequency *= 2.0;
    }

    (total / norm) as f32
}
