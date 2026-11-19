//! Atlas scope: whole-world grids computed once, eagerly, when a world is
//! built (see `docs/milestone-15-atlas.md`).
//!
//! Atlas nodes run global simulations (erosion, flow) over coarse grids, one
//! value per atlas cell (`cell_size` x `cell_size` columns). They connect to
//! world nodes only through two explicit adapters: `rasterize` (world →
//! atlas, evaluated at cell centers) and `sample` (atlas → world, nearest or
//! bilinear). The finished grids are read-only shared data, like the rest of
//! `WorldStore`, and can be saved and loaded (`Atlas::write` / `Atlas::read`)
//! so a game never has to recompute them.

use std::{
    borrow::Cow,
    io::{Read, Write},
    sync::Arc,
};

use serde::Serialize;

use crate::{
    biomes::BiomeTable,
    materials::MaterialTable,
    pipeline::{Build, FieldSpec, InputSpec, NodeKind, ParamSpec, WorldStep},
    strata::{StrataGrid, STRATA_LAYERS},
    world::fbm,
    FieldScope, ValueKind, WORLD_SLOTS,
};

/// Atlas cell size (columns per side) when a graph does not set one.
pub const DEFAULT_CELL_SIZE: u32 = 16;

/// Upper bound on atlas outputs in one graph (grids live in a `Vec`; this
/// only stops runaway graphs).
pub const ATLAS_SLOTS: usize = 64;

/// Snapshots are downsampled to at most this many cells per side.
pub const SNAPSHOT_SIDE: u32 = 256;

/// Snapshots kept per node (the scrubber's frames).
pub const SNAPSHOTS_PER_NODE: usize = 64;

/// What `AtlasRun::check` returns once the build has been cancelled.
pub const CANCELLED: &str = "atlas build cancelled";

// ---------------------------------------------------------------------------
// Grids

/// One f32 per atlas cell, row-major.
#[derive(Clone, Debug, PartialEq)]
pub struct AtlasGrid {
    width: u32,
    height: u32,
    values: Vec<f32>,
}

impl AtlasGrid {
    pub fn new(width: u32, height: u32, fill: f32) -> Self {
        Self {
            width,
            height,
            values: vec![fill; width as usize * height as usize],
        }
    }

    pub fn from_values(width: u32, height: u32, values: Vec<f32>) -> Result<Self, String> {
        if values.len() != width as usize * height as usize {
            return Err(format!(
                "grid of {width}x{height} needs {} values, got {}",
                width as usize * height as usize,
                values.len()
            ));
        }
        Ok(Self {
            width,
            height,
            values,
        })
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }

    pub fn len(&self) -> usize {
        self.values.len()
    }

    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    pub fn index(&self, x: u32, y: u32) -> usize {
        x as usize + y as usize * self.width as usize
    }

    pub fn get(&self, x: u32, y: u32) -> f32 {
        self.values[self.index(x, y)]
    }

    pub fn set(&mut self, x: u32, y: u32, value: f32) {
        let index = self.index(x, y);
        self.values[index] = value;
    }

    pub fn values(&self) -> &[f32] {
        &self.values
    }

    pub fn values_mut(&mut self) -> &mut [f32] {
        &mut self.values
    }

    /// Value at continuous cell coordinates with Catmull-Rom (bicubic)
    /// interpolation: smooth across cell borders, unlike `bilinear`, so
    /// sampled terrain shows no facets at the cell grid. Clamped to the grid.
    pub fn bicubic(&self, u: f64, v: f64) -> f32 {
        let max_x = f64::from(self.width - 1);
        let max_y = f64::from(self.height - 1);
        let (u, v) = (u.clamp(0.0, max_x), v.clamp(0.0, max_y));
        let (x0, y0) = (u.floor(), v.floor());
        let (tx, ty) = (u - x0, v - y0);
        let (x0, y0) = (x0 as i64, y0 as i64);
        let at = |x: i64, y: i64| {
            let x = x.clamp(0, i64::from(self.width) - 1) as u32;
            let y = y.clamp(0, i64::from(self.height) - 1) as u32;
            f64::from(self.get(x, y))
        };
        let cubic = |p0: f64, p1: f64, p2: f64, p3: f64, t: f64| {
            p1 + 0.5
                * t
                * (p2 - p0 + t * (2.0 * p0 - 5.0 * p1 + 4.0 * p2 - p3 + t * (3.0 * (p1 - p2) + p3 - p0)))
        };
        let mut rows = [0.0; 4];
        for (k, row) in rows.iter_mut().enumerate() {
            let y = y0 - 1 + k as i64;
            *row = cubic(at(x0 - 1, y), at(x0, y), at(x0 + 1, y), at(x0 + 2, y), tx);
        }
        cubic(rows[0], rows[1], rows[2], rows[3], ty) as f32
    }

    /// Value at continuous cell coordinates (cell centers at integer + 0),
    /// clamped to the grid.
    pub fn bilinear(&self, u: f64, v: f64) -> f32 {
        let max_x = f64::from(self.width - 1);
        let max_y = f64::from(self.height - 1);
        let (u, v) = (u.clamp(0.0, max_x), v.clamp(0.0, max_y));
        let (x0, y0) = (u.floor(), v.floor());
        let (tx, ty) = (u - x0, v - y0);
        let (x0, y0) = (x0 as u32, y0 as u32);
        let (x1, y1) = ((x0 + 1).min(self.width - 1), (y0 + 1).min(self.height - 1));

        let top = f64::from(self.get(x0, y0)) * (1.0 - tx) + f64::from(self.get(x1, y0)) * tx;
        let bottom = f64::from(self.get(x0, y1)) * (1.0 - tx) + f64::from(self.get(x1, y1)) * tx;
        (top * (1.0 - ty) + bottom * ty) as f32
    }

    /// Box-filtered copy whose longer side is at most `max_side` (the grid
    /// itself when it is already small enough).
    pub fn downsampled(&self, max_side: u32) -> AtlasGrid {
        let factor = self.width.max(self.height).div_ceil(max_side.max(1)).max(1);
        if factor == 1 {
            return self.clone();
        }

        let (width, height) = (self.width.div_ceil(factor), self.height.div_ceil(factor));
        let mut out = AtlasGrid::new(width, height, 0.0);

        for y in 0..height {
            for x in 0..width {
                let (mut sum, mut count) = (0.0f64, 0u32);
                for sy in y * factor..((y + 1) * factor).min(self.height) {
                    for sx in x * factor..((x + 1) * factor).min(self.width) {
                        sum += f64::from(self.get(sx, sy));
                        count += 1;
                    }
                }
                out.set(x, y, (sum / f64::from(count.max(1))) as f32);
            }
        }

        out
    }
}

// ---------------------------------------------------------------------------
// Context

/// The atlas grid over a world: what atlas stages and adapters build against.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AtlasContext {
    pub seed: u64,
    /// World bounds in columns.
    pub origin: (i64, i64),
    pub size: (u32, u32),
    /// World depth in cells (the elevation range).
    pub depth: u32,
    /// Columns per atlas cell side.
    pub cell_size: u32,
    /// Atlas cells per side (the world size divided by `cell_size`, rounded up).
    pub grid: (u32, u32),
    /// Sea level in cells (`Extent::sea_level`); None: 0.3 × depth.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sea: Option<u32>,
}

impl AtlasContext {
    pub fn new(
        seed: u64,
        origin: (i64, i64),
        size: (u32, u32),
        depth: u32,
        cell_size: u32,
    ) -> Self {
        let cell_size = cell_size.max(1);
        Self {
            seed,
            origin,
            size,
            depth,
            cell_size,
            grid: (size.0.div_ceil(cell_size), size.1.div_ceil(cell_size)),
            sea: None,
        }
    }

    /// The same context with a sea level setting (cells).
    pub fn with_sea_level(self, sea: Option<u32>) -> Self {
        Self { sea, ..self }
    }

    /// A grid of this atlas's size.
    pub fn grid(&self, fill: f32) -> AtlasGrid {
        AtlasGrid::new(self.grid.0, self.grid.1, fill)
    }

    /// Elevation below which a column is ocean (matches `WorldStore::sea_level`).
    pub fn sea_level(&self) -> f32 {
        match self.sea {
            Some(sea) => sea as f32,
            None => self.depth as f32 * 0.3,
        }
    }

    /// The world column at the center of a cell (clamped into the world for
    /// partial edge cells), offset by `(dx, dy)` in [0, 1) of a cell.
    pub fn cell_point(&self, cx: u32, cy: u32, dx: f64, dy: f64) -> (i64, i64) {
        let cell = f64::from(self.cell_size);
        let x = self.origin.0 as f64 + (f64::from(cx) + dx) * cell;
        let y = self.origin.1 as f64 + (f64::from(cy) + dy) * cell;
        let max_x = self.origin.0 + i64::from(self.size.0) - 1;
        let max_y = self.origin.1 + i64::from(self.size.1) - 1;

        (
            (x.floor() as i64).clamp(self.origin.0, max_x),
            (y.floor() as i64).clamp(self.origin.1, max_y),
        )
    }

    /// Continuous cell coordinates of a world column (cell centers are integers).
    pub fn cell_coords(&self, x: i64, y: i64) -> (f64, f64) {
        let cell = f64::from(self.cell_size);
        (
            ((x - self.origin.0) as f64 + 0.5) / cell - 0.5,
            ((y - self.origin.1) as f64 + 0.5) / cell - 0.5,
        )
    }

    /// The cell containing a world column (clamped to the grid).
    pub fn cell_of(&self, x: i64, y: i64) -> (u32, u32) {
        let cell = i64::from(self.cell_size);
        let cx = ((x - self.origin.0).div_euclid(cell)).clamp(0, i64::from(self.grid.0) - 1);
        let cy = ((y - self.origin.1).div_euclid(cell)).clamp(0, i64::from(self.grid.1) - 1);
        (cx as u32, cy as u32)
    }

    /// World position (column units, f32) of a continuous cell coordinate.
    pub fn world_point(&self, u: f64, v: f64) -> (f32, f32) {
        let cell = f64::from(self.cell_size);
        (
            (self.origin.0 as f64 + (u + 0.5) * cell) as f32,
            (self.origin.1 as f64 + (v + 0.5) * cell) as f32,
        )
    }
}

// ---------------------------------------------------------------------------
// Vector features

/// One river line: points in world columns from source to mouth, with the
/// discharge (accumulated drainage, in cells) and the water surface
/// elevation (cells) at each point.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RiverPath {
    pub points: Vec<(f32, f32)>,
    pub discharge: Vec<f32>,
    pub level: Vec<f32>,
}

/// The nearest river to a point (see `RiverNetwork::nearest`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RiverHit {
    /// Distance in columns from the point to the river line.
    pub distance: f32,
    /// Discharge and water level interpolated at the nearest point.
    pub discharge: f32,
    pub level: f32,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct Segment {
    a: (f32, f32),
    b: (f32, f32),
    discharge: (f32, f32),
    level: (f32, f32),
}

/// River lines of one node plus a spatial index of their segments, bucketed
/// per atlas cell, so chunk and world nodes can find the nearest river fast.
#[derive(Clone, Debug)]
pub struct RiverNetwork {
    /// Id of the node that traced the rivers.
    pub node: String,
    pub paths: Vec<RiverPath>,
    segments: Vec<Segment>,
    /// CSR buckets: segments overlapping cell `c` are
    /// `items[start[c]..start[c + 1]]`.
    start: Vec<u32>,
    items: Vec<u32>,
    context: AtlasContext,
}

impl PartialEq for RiverNetwork {
    fn eq(&self, other: &Self) -> bool {
        self.node == other.node && self.paths == other.paths
    }
}

impl RiverNetwork {
    pub fn new(node: impl Into<String>, paths: Vec<RiverPath>, context: AtlasContext) -> Self {
        let mut segments = Vec::new();
        for path in &paths {
            for k in 1..path.points.len() {
                segments.push(Segment {
                    a: path.points[k - 1],
                    b: path.points[k],
                    discharge: (path.discharge[k - 1], path.discharge[k]),
                    level: (path.level[k - 1], path.level[k]),
                });
            }
        }

        let (gw, gh) = (context.grid.0 as usize, context.grid.1 as usize);
        let cells = gw * gh;
        let cell_range = |s: &Segment| {
            let (x0, x1) = (s.a.0.min(s.b.0) as i64, s.a.0.max(s.b.0) as i64);
            let (y0, y1) = (s.a.1.min(s.b.1) as i64, s.a.1.max(s.b.1) as i64);
            let (cx0, cy0) = context.cell_of(x0, y0);
            let (cx1, cy1) = context.cell_of(x1, y1);
            (cx0, cy0, cx1, cy1)
        };
        let mut counts = vec![0u32; cells + 1];
        for s in &segments {
            let (cx0, cy0, cx1, cy1) = cell_range(s);
            for cy in cy0..=cy1 {
                for cx in cx0..=cx1 {
                    counts[cx as usize + cy as usize * gw] += 1;
                }
            }
        }
        let mut start = vec![0u32; cells + 1];
        for c in 0..cells {
            start[c + 1] = start[c] + counts[c];
        }
        let mut fill = start.clone();
        let mut items = vec![0u32; start[cells] as usize];
        for (id, s) in segments.iter().enumerate() {
            let (cx0, cy0, cx1, cy1) = cell_range(s);
            for cy in cy0..=cy1 {
                for cx in cx0..=cx1 {
                    let c = cx as usize + cy as usize * gw;
                    items[fill[c] as usize] = id as u32;
                    fill[c] += 1;
                }
            }
        }

        Self {
            node: node.into(),
            paths,
            segments,
            start,
            items,
            context,
        }
    }

    /// No rivers (placeholder worlds).
    pub fn empty(context: AtlasContext) -> Self {
        Self::new(String::new(), Vec::new(), context)
    }

    /// The nearest river line within `radius` columns of `(x, y)`.
    pub fn nearest(&self, x: f64, y: f64, radius: f64) -> Option<RiverHit> {
        if self.segments.is_empty() {
            return None;
        }
        let c = &self.context;
        let (cx0, cy0) = c.cell_of((x - radius).floor() as i64, (y - radius).floor() as i64);
        let (cx1, cy1) = c.cell_of((x + radius).ceil() as i64, (y + radius).ceil() as i64);
        let gw = c.grid.0 as usize;
        let mut best: Option<RiverHit> = None;
        let (px, py) = (x as f32, y as f32);

        for cy in cy0..=cy1 {
            for cx in cx0..=cx1 {
                let cell = cx as usize + cy as usize * gw;
                for &id in &self.items[self.start[cell] as usize..self.start[cell + 1] as usize] {
                    let s = &self.segments[id as usize];
                    let (dx, dy) = (s.b.0 - s.a.0, s.b.1 - s.a.1);
                    let len2 = dx * dx + dy * dy;
                    let t = if len2 > 0.0 {
                        (((px - s.a.0) * dx + (py - s.a.1) * dy) / len2).clamp(0.0, 1.0)
                    } else {
                        0.0
                    };
                    let (qx, qy) = (s.a.0 + dx * t, s.a.1 + dy * t);
                    let distance = ((px - qx).powi(2) + (py - qy).powi(2)).sqrt();
                    if distance <= radius as f32 && best.is_none_or(|b| distance < b.distance) {
                        best = Some(RiverHit {
                            distance,
                            discharge: s.discharge.0 + (s.discharge.1 - s.discharge.0) * t,
                            level: s.level.0 + (s.level.1 - s.level.0) * t,
                        });
                    }
                }
            }
        }

        best
    }
}

// ---------------------------------------------------------------------------
// Stages

/// One atlas-scope node, built once per world and run once.
pub trait AtlasStage: Send + Sync + 'static {
    fn run(&self, io: &mut AtlasIo<'_>, run: &mut AtlasRun<'_>) -> Result<(), String>;
}

pub type BoxedAtlasStage = Box<dyn AtlasStage>;

/// What an atlas stage reads and writes: inputs by slot (finished earlier in
/// graph order), and its own outputs.
pub struct AtlasIo<'a> {
    atlas: &'a Atlas,
    written: Vec<(usize, AtlasGrid)>,
    strata: Vec<(usize, StrataGrid)>,
    rivers: Vec<(usize, Vec<RiverPath>)>,
}

/// What one atlas stage produced.
pub(crate) struct AtlasOutputs {
    pub grids: Vec<(usize, AtlasGrid)>,
    pub strata: Vec<(usize, StrataGrid)>,
    pub rivers: Vec<(usize, Vec<RiverPath>)>,
}

impl<'a> AtlasIo<'a> {
    pub(crate) fn new(atlas: &'a Atlas) -> Self {
        Self {
            atlas,
            written: Vec::new(),
            strata: Vec::new(),
            rivers: Vec::new(),
        }
    }

    pub fn context(&self) -> &AtlasContext {
        &self.atlas.context
    }

    /// The world's material table (ids in strata and material grids).
    pub fn materials(&self) -> &MaterialTable {
        &self.atlas.materials
    }

    /// The world's biome table (ids in biome grids).
    pub fn biomes(&self) -> &BiomeTable {
        &self.atlas.biomes
    }

    pub fn input(&self, slot: usize) -> Result<&AtlasGrid, String> {
        self.atlas
            .grid(slot)
            .ok_or_else(|| format!("atlas input slot {slot} has no grid"))
    }

    /// An optional scalar input: None when the slot is None.
    pub fn optional(&self, slot: Option<usize>) -> Result<Option<&AtlasGrid>, String> {
        slot.map(|slot| self.input(slot)).transpose()
    }

    pub fn strata_input(&self, slot: usize) -> Result<&StrataGrid, String> {
        self.atlas
            .strata(slot)
            .ok_or_else(|| format!("atlas strata slot {slot} has no strata"))
    }

    pub fn rivers_input(&self, slot: usize) -> Result<&RiverNetwork, String> {
        self.atlas
            .river_network(slot)
            .ok_or_else(|| format!("atlas rivers slot {slot} has no rivers"))
    }

    /// Stores strata (must be the atlas size).
    pub fn write_strata(&mut self, slot: usize, strata: StrataGrid) -> Result<(), String> {
        if (strata.width(), strata.height()) != self.atlas.context.grid {
            return Err("strata grid does not match the atlas size".to_owned());
        }
        self.strata.retain(|(s, _)| *s != slot);
        self.strata.push((slot, strata));
        Ok(())
    }

    /// Stores river lines (indexed for nearest-river queries when stored).
    pub fn write_rivers(&mut self, slot: usize, rivers: Vec<RiverPath>) {
        self.rivers.retain(|(s, _)| *s != slot);
        self.rivers.push((slot, rivers));
    }

    /// Stores an output grid (must be the atlas size).
    pub fn write(&mut self, slot: usize, grid: AtlasGrid) -> Result<(), String> {
        let context = self.atlas.context;
        if (grid.width, grid.height) != context.grid {
            return Err(format!(
                "output grid is {}x{}, the atlas is {}x{}",
                grid.width, grid.height, context.grid.0, context.grid.1
            ));
        }
        self.written.retain(|(s, _)| *s != slot);
        self.written.push((slot, grid));
        Ok(())
    }

    /// Stores a grid from values (row-major, the atlas size).
    pub fn write_values(&mut self, slot: usize, values: Vec<f32>) -> Result<(), String> {
        let (w, h) = self.atlas.context.grid;
        self.write(slot, AtlasGrid::from_values(w, h, values)?)
    }

    pub(crate) fn finish(self) -> AtlasOutputs {
        AtlasOutputs {
            grids: self.written,
            strata: self.strata,
            rivers: self.rivers,
        }
    }
}

/// Progress of an atlas build, reported by `AtlasHooks::progress`.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AtlasProgress {
    /// Id of the node running.
    pub node: String,
    /// Position of that node among the nodes that do atlas work (0-based).
    pub index: usize,
    pub total: usize,
    /// Progress of the node itself, 0 … 1.
    pub fraction: f64,
}

/// Callbacks for a long atlas build (all optional). Build it with struct
/// update syntax (`..AtlasHooks::default()`) so new hooks don't break callers.
#[derive(Clone, Copy, Default)]
pub struct AtlasHooks<'a> {
    pub progress: Option<&'a (dyn Fn(&AtlasProgress) + Sync)>,
    /// Polled during the build; returning true stops it with `CANCELLED`.
    pub cancelled: Option<&'a (dyn Fn() -> bool + Sync)>,
    /// Each scrubber frame as soon as it is taken (live "simulate" views);
    /// the same frames end up in `Atlas::snapshots` afterwards.
    pub snapshot: Option<&'a (dyn Fn(&AtlasSnapshot) + Sync)>,
}

/// One scrubber frame: a downsampled copy of a grid during a node's run.
#[derive(Clone, Debug, PartialEq)]
pub struct AtlasSnapshot {
    pub node: String,
    pub label: String,
    pub grid: AtlasGrid,
}

/// Progress, cancellation and snapshots for the node currently running.
pub struct AtlasRun<'a> {
    hooks: AtlasHooks<'a>,
    node: String,
    index: usize,
    total: usize,
    snapshots: Vec<AtlasSnapshot>,
}

impl<'a> AtlasRun<'a> {
    pub(crate) fn new(hooks: AtlasHooks<'a>, total: usize) -> Self {
        Self {
            hooks,
            node: String::new(),
            index: 0,
            total,
            snapshots: Vec::new(),
        }
    }

    pub(crate) fn begin(&mut self, node: &str, index: usize) {
        self.node = node.to_owned();
        self.index = index;
        self.progress(0.0);
    }

    /// Reports the running node's progress (0 … 1).
    pub fn progress(&mut self, fraction: f64) {
        if let Some(report) = self.hooks.progress {
            report(&AtlasProgress {
                node: self.node.clone(),
                index: self.index,
                total: self.total,
                fraction: fraction.clamp(0.0, 1.0),
            });
        }
    }

    pub fn cancelled(&self) -> bool {
        self.hooks.cancelled.is_some_and(|cancelled| cancelled())
    }

    /// `Err(CANCELLED)` once the build was cancelled: call between iterations.
    pub fn check(&self) -> Result<(), String> {
        if self.cancelled() {
            Err(CANCELLED.to_owned())
        } else {
            Ok(())
        }
    }

    /// Keeps a downsampled copy of `grid` as a scrubber frame (at most
    /// `SNAPSHOTS_PER_NODE` per node; later ones are dropped).
    pub fn snapshot(&mut self, label: impl Into<String>, grid: &AtlasGrid) {
        let taken = self
            .snapshots
            .iter()
            .filter(|s| s.node == self.node)
            .count();
        if taken < SNAPSHOTS_PER_NODE {
            let snapshot = AtlasSnapshot {
                node: self.node.clone(),
                label: label.into(),
                grid: grid.downsampled(SNAPSHOT_SIDE),
            };
            if let Some(report) = self.hooks.snapshot {
                report(&snapshot);
            }
            self.snapshots.push(snapshot);
        }
    }

    pub(crate) fn into_snapshots(self) -> Vec<AtlasSnapshot> {
        self.snapshots
    }
}

// ---------------------------------------------------------------------------
// The finished atlas

/// Every atlas value of one world by atlas slot (scalar grids, strata and
/// river networks each have their own slot space), the world's material and
/// biome id tables, and scrubber snapshots.
#[derive(Clone, Debug)]
pub struct Atlas {
    context: AtlasContext,
    pub(crate) grids: Vec<Option<Arc<AtlasGrid>>>,
    pub(crate) strata: Vec<Option<Arc<StrataGrid>>>,
    pub(crate) rivers: Vec<Option<Arc<RiverNetwork>>>,
    pub(crate) snapshots: Vec<AtlasSnapshot>,
    /// `world_signature` of the graph the atlas was built from.
    pub(crate) signature: String,
    /// Material ids in this world (strata, material grids, voxels).
    pub(crate) materials: Arc<MaterialTable>,
    /// Biome ids in this world (biome grids).
    pub(crate) biomes: Arc<BiomeTable>,
}

/// Magic and version of `Atlas::write`.
const ATLAS_MAGIC: &[u8; 8] = b"WGATLAS\0";
/// 2: material and biome id tables, strata, river levels, rivers by slot.
pub const ATLAS_VERSION: u32 = 2;

impl Atlas {
    pub(crate) fn empty(context: AtlasContext) -> Self {
        Self {
            context,
            grids: Vec::new(),
            strata: Vec::new(),
            rivers: Vec::new(),
            snapshots: Vec::new(),
            signature: String::new(),
            materials: MaterialTable::builtin(),
            biomes: BiomeTable::builtin(),
        }
    }

    pub fn context(&self) -> &AtlasContext {
        &self.context
    }

    /// The grid in an atlas slot (see `Schema::atlas_fields`).
    pub fn grid(&self, slot: usize) -> Option<&AtlasGrid> {
        self.grids.get(slot).and_then(Option::as_deref)
    }

    pub fn grid_count(&self) -> usize {
        self.grids.len()
    }

    /// The strata in a strata slot.
    pub fn strata(&self, slot: usize) -> Option<&StrataGrid> {
        self.strata.get(slot).and_then(Option::as_deref)
    }

    /// The river network in a rivers slot.
    pub fn river_network(&self, slot: usize) -> Option<&RiverNetwork> {
        self.rivers.get(slot).and_then(Option::as_deref)
    }

    /// Every river network (one per node with a `rivers` output).
    pub fn rivers(&self) -> Vec<&RiverNetwork> {
        self.rivers.iter().flatten().map(Arc::as_ref).collect()
    }

    pub fn materials(&self) -> &Arc<MaterialTable> {
        &self.materials
    }

    pub fn biomes(&self) -> &Arc<BiomeTable> {
        &self.biomes
    }

    pub fn snapshots(&self) -> &[AtlasSnapshot] {
        &self.snapshots
    }

    pub fn signature(&self) -> &str {
        &self.signature
    }

    /// Saves grids, strata, rivers and the id tables (not snapshots) in a
    /// versioned little-endian binary format, for a game's world file.
    pub fn write(&self, out: &mut impl Write) -> std::io::Result<()> {
        let c = &self.context;
        out.write_all(ATLAS_MAGIC)?;
        out.write_all(&ATLAS_VERSION.to_le_bytes())?;
        out.write_all(&c.seed.to_le_bytes())?;
        out.write_all(&c.origin.0.to_le_bytes())?;
        out.write_all(&c.origin.1.to_le_bytes())?;
        for value in [c.size.0, c.size.1, c.depth, c.cell_size, c.grid.0, c.grid.1] {
            out.write_all(&value.to_le_bytes())?;
        }
        write_str(out, &self.signature)?;
        for names in [self.materials.names(), self.biomes.names()] {
            out.write_all(&(names.len() as u32).to_le_bytes())?;
            for name in names {
                write_str(out, name)?;
            }
        }

        out.write_all(&(self.grids.len() as u32).to_le_bytes())?;
        for grid in &self.grids {
            match grid {
                None => out.write_all(&[0])?,
                Some(grid) => {
                    out.write_all(&[1])?;
                    for value in &grid.values {
                        out.write_all(&value.to_le_bytes())?;
                    }
                }
            }
        }

        out.write_all(&(self.strata.len() as u32).to_le_bytes())?;
        for strata in &self.strata {
            match strata {
                None => out.write_all(&[0])?,
                Some(strata) => {
                    out.write_all(&[1])?;
                    for &material in strata.materials() {
                        out.write_all(&material.to_le_bytes())?;
                    }
                    for &top in strata.tops() {
                        out.write_all(&top.to_le_bytes())?;
                    }
                }
            }
        }

        out.write_all(&(self.rivers.len() as u32).to_le_bytes())?;
        for network in &self.rivers {
            let Some(network) = network else {
                out.write_all(&[0])?;
                continue;
            };
            out.write_all(&[1])?;
            write_str(out, &network.node)?;
            out.write_all(&(network.paths.len() as u32).to_le_bytes())?;
            for path in &network.paths {
                out.write_all(&(path.points.len() as u32).to_le_bytes())?;
                for k in 0..path.points.len() {
                    let (x, y) = path.points[k];
                    for value in [x, y, path.discharge[k], path.level[k]] {
                        out.write_all(&value.to_le_bytes())?;
                    }
                }
            }
        }

        Ok(())
    }

    /// Reads what `write` saved. Validates the header and sizes; the
    /// signature is checked against a graph by `WorldStore::from_atlas`.
    pub fn read(input: &mut impl Read) -> Result<Self, String> {
        let mut reader = Reader(input);
        let mut magic = [0u8; 8];
        reader.bytes(&mut magic)?;
        if &magic != ATLAS_MAGIC {
            return Err("not an atlas file".to_owned());
        }
        let version = reader.u32()?;
        if version != ATLAS_VERSION {
            return Err(format!(
                "atlas version {version} is not supported ({ATLAS_VERSION})"
            ));
        }

        let seed = reader.u64()?;
        let origin = (reader.i64()?, reader.i64()?);
        let size = (reader.u32()?, reader.u32()?);
        let depth = reader.u32()?;
        let cell_size = reader.u32()?;
        let grid = (reader.u32()?, reader.u32()?);
        let context = AtlasContext::new(seed, origin, size, depth, cell_size);
        if context.grid != grid {
            return Err("atlas grid size does not match its world size".to_owned());
        }
        let signature = reader.string()?;
        let mut tables = Vec::new();
        for _ in 0..2 {
            let count = reader.count(1 << 16)?;
            let names = (0..count)
                .map(|_| reader.string())
                .collect::<Result<Vec<_>, _>>()?;
            tables.push(names);
        }
        let materials = Arc::new(MaterialTable::from_names(&tables[0]));
        let biomes = Arc::new(BiomeTable::from_names(&tables[1]));

        let cells = grid.0 as usize * grid.1 as usize;
        let grid_count = reader.count(ATLAS_SLOTS)?;
        let mut grids = Vec::with_capacity(grid_count);
        for _ in 0..grid_count {
            let mut flag = [0u8; 1];
            reader.bytes(&mut flag)?;
            grids.push(match flag[0] {
                0 => None,
                1 => {
                    let values = (0..cells)
                        .map(|_| reader.f32())
                        .collect::<Result<Vec<_>, _>>()?;
                    Some(Arc::new(AtlasGrid::from_values(grid.0, grid.1, values)?))
                }
                other => return Err(format!("bad grid flag {other}")),
            });
        }

        let strata_count = reader.count(ATLAS_SLOTS)?;
        let mut strata = Vec::with_capacity(strata_count);
        for _ in 0..strata_count {
            let mut flag = [0u8; 1];
            reader.bytes(&mut flag)?;
            strata.push(match flag[0] {
                0 => None,
                1 => {
                    let materials = (0..cells * STRATA_LAYERS)
                        .map(|_| reader.u16())
                        .collect::<Result<Vec<_>, _>>()?;
                    let tops = (0..cells * STRATA_LAYERS)
                        .map(|_| reader.f32())
                        .collect::<Result<Vec<_>, _>>()?;
                    Some(Arc::new(StrataGrid::from_parts(
                        grid.0, grid.1, materials, tops,
                    )?))
                }
                other => return Err(format!("bad strata flag {other}")),
            });
        }

        let river_sets = reader.count(ATLAS_SLOTS)?;
        let mut rivers = Vec::with_capacity(river_sets);
        for _ in 0..river_sets {
            let mut flag = [0u8; 1];
            reader.bytes(&mut flag)?;
            if flag[0] == 0 {
                rivers.push(None);
                continue;
            }
            let node = reader.string()?;
            let paths = reader.count(1 << 24)?;
            let mut set = Vec::with_capacity(paths.min(1 << 16));
            for _ in 0..paths {
                let points = reader.count(1 << 24)?;
                let mut path = RiverPath {
                    points: Vec::with_capacity(points.min(1 << 16)),
                    discharge: Vec::with_capacity(points.min(1 << 16)),
                    level: Vec::with_capacity(points.min(1 << 16)),
                };
                for _ in 0..points {
                    path.points.push((reader.f32()?, reader.f32()?));
                    path.discharge.push(reader.f32()?);
                    path.level.push(reader.f32()?);
                }
                set.push(path);
            }
            rivers.push(Some(Arc::new(RiverNetwork::new(node, set, context))));
        }

        Ok(Self {
            context,
            grids,
            strata,
            rivers,
            snapshots: Vec::new(),
            signature,
            materials,
            biomes,
        })
    }
}

fn write_str(out: &mut impl Write, text: &str) -> std::io::Result<()> {
    out.write_all(&(text.len() as u32).to_le_bytes())?;
    out.write_all(text.as_bytes())
}

struct Reader<'a, R: Read>(&'a mut R);

impl<R: Read> Reader<'_, R> {
    fn bytes(&mut self, buf: &mut [u8]) -> Result<(), String> {
        self.0
            .read_exact(buf)
            .map_err(|e| format!("truncated atlas: {e}"))
    }

    fn u16(&mut self) -> Result<u16, String> {
        let mut b = [0u8; 2];
        self.bytes(&mut b)?;
        Ok(u16::from_le_bytes(b))
    }

    fn u32(&mut self) -> Result<u32, String> {
        let mut b = [0u8; 4];
        self.bytes(&mut b)?;
        Ok(u32::from_le_bytes(b))
    }

    fn u64(&mut self) -> Result<u64, String> {
        let mut b = [0u8; 8];
        self.bytes(&mut b)?;
        Ok(u64::from_le_bytes(b))
    }

    fn i64(&mut self) -> Result<i64, String> {
        let mut b = [0u8; 8];
        self.bytes(&mut b)?;
        Ok(i64::from_le_bytes(b))
    }

    fn f32(&mut self) -> Result<f32, String> {
        let mut b = [0u8; 4];
        self.bytes(&mut b)?;
        Ok(f32::from_le_bytes(b))
    }

    fn count(&mut self, max: usize) -> Result<usize, String> {
        let n = self.u32()? as usize;
        if n > max {
            return Err(format!("atlas count {n} exceeds {max}"));
        }
        Ok(n)
    }

    fn string(&mut self) -> Result<String, String> {
        let n = self.count(1 << 20)?;
        let mut b = vec![0u8; n];
        self.bytes(&mut b)?;
        String::from_utf8(b).map_err(|_| "atlas string is not UTF-8".to_owned())
    }
}

// ---------------------------------------------------------------------------
// Adapters (built by `pipeline::build_world_stage`)

/// world → atlas: a world value averaged over `supersample`² points per cell.
pub static RASTERIZE: NodeKind = NodeKind {
    id: "rasterize",
    label: "Rasterize (world → atlas)",
    description: "Evaluates a world value at every atlas cell (averaging \
                  `supersample`² points per cell) so atlas simulations can read it.",
    inputs: &[InputSpec {
        name: Cow::Borrowed("in"),
        scope: FieldScope::World,
        kind: ValueKind::Scalar,
        optional: false,
    }],
    outputs: &[FieldSpec {
        name: Cow::Borrowed("out"),
        scope: FieldScope::Atlas,
        kind: ValueKind::Scalar,
        unit: Cow::Borrowed(""),
        min: 0.0,
        max: 1.0,
        ramp: Cow::Borrowed("grayscale"),
        display_from: Some(Cow::Borrowed("in")),
    }],
    params: &[ParamSpec {
        name: "supersample",
        default: 1.0,
        min: 1.0,
        max: 4.0,
    }],
    build: Build::Rasterize,
};

/// atlas → world: an atlas grid read at any world column.
pub static SAMPLE: NodeKind = NodeKind {
    id: "sample",
    label: "Sample (atlas → world)",
    description: "Reads an atlas grid at any world point, so world and chunk \
                  nodes can use it. `mode` 0 = nearest cell (for categories such as \
                  biomes or materials), 1 = bilinear, 2 = bicubic (smooth, no facets \
                  at cell borders). `warp` (in cells) moves each read point by fractal                   noise, so nearest-cell categories get ragged, natural borders instead                   of square blocks (1 … 2 suits biomes; 0 = off).",
    inputs: &[InputSpec {
        name: Cow::Borrowed("in"),
        scope: FieldScope::Atlas,
        kind: ValueKind::Scalar,
        optional: false,
    }],
    outputs: &[FieldSpec {
        name: Cow::Borrowed("out"),
        scope: FieldScope::World,
        kind: ValueKind::Scalar,
        unit: Cow::Borrowed(""),
        min: 0.0,
        max: 1.0,
        ramp: Cow::Borrowed("grayscale"),
        display_from: Some(Cow::Borrowed("in")),
    }],
    params: &[
        ParamSpec {
            name: "mode",
            default: 1.0,
            min: 0.0,
            max: 2.0,
        },
        ParamSpec {
            name: "warp",
            default: 0.0,
            min: 0.0,
            max: 4.0,
        },
    ],
    build: Build::Sample,
};

/// Wiring helper for atlas values; see `REROUTE_COLUMN`.
pub static REROUTE_ATLAS: NodeKind = NodeKind {
    id: "reroute_atlas",
    label: "Reroute (atlas)",
    description: "Passes an atlas grid through unchanged; a wire waypoint.",
    inputs: &[InputSpec {
        name: Cow::Borrowed("in"),
        scope: FieldScope::Atlas,
        kind: ValueKind::Scalar,
        optional: false,
    }],
    outputs: &[FieldSpec {
        name: Cow::Borrowed("out"),
        scope: FieldScope::Atlas,
        kind: ValueKind::Scalar,
        unit: Cow::Borrowed(""),
        min: 0.0,
        max: 1.0,
        ramp: Cow::Borrowed("grayscale"),
        display_from: None,
    }],
    params: &[],
    build: Build::Alias,
};

/// The world step of a `sample` node.
pub(crate) struct SampleStep {
    pub grid: Arc<AtlasGrid>,
    pub context: AtlasContext,
    pub out: usize,
    /// 0 nearest, 1 bilinear, 2 bicubic.
    pub mode: u8,
    /// Read-point displacement in cells (0 = none).
    pub warp: f64,
}

/// Seeds of the two warp noise channels (kept apart from other noise users).
const WARP_SEED_U: u64 = 0x5A3D_1E77_0B1A_C3E5;
const WARP_SEED_V: u64 = 0x2F6B_9C41_D8E0_7A13;

impl SampleStep {
    /// Atlas cell coordinates of a column, displaced by the warp noise.
    fn coords(&self, x: i64, y: i64) -> (f64, f64) {
        let (u, v) = self.context.cell_coords(x, y);
        if self.warp <= 0.0 {
            return (u, v);
        }
        // Two cells per wavelength at the coarsest octave, down to an eighth
        // of a cell, so borders wiggle at every scale.
        let wavelength = f64::from(self.context.cell_size) * 2.0;
        let (px, py) = (x as f64 + 0.5, y as f64 + 0.5);
        let seed = self.context.seed;
        let noise = |salt: u64| f64::from(fbm(seed ^ salt, px, py, wavelength, 4)) * 2.0 - 1.0;
        (u + noise(WARP_SEED_U) * self.warp, v + noise(WARP_SEED_V) * self.warp)
    }
}

impl WorldStep for SampleStep {
    fn run(&self, x: i64, y: i64, slots: &mut [f64; WORLD_SLOTS]) {
        let (u, v) = self.coords(x, y);
        slots[self.out] = match self.mode {
            0 if self.warp <= 0.0 => {
                let (cx, cy) = self.context.cell_of(x, y);
                f64::from(self.grid.get(cx, cy))
            }
            0 => {
                // `cell_coords` puts cell centers on whole numbers.
                let clamp = |c: f64, n: u32| (c.round().max(0.0) as u32).min(n - 1);
                let (cx, cy) = (clamp(u, self.context.grid.0), clamp(v, self.context.grid.1));
                f64::from(self.grid.get(cx, cy))
            }
            1 => f64::from(self.grid.bilinear(u, v)),
            _ => f64::from(self.grid.bicubic(u, v)),
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn context_maps_cells_and_columns() {
        let c = AtlasContext::new(1, (-100, -100), (200, 150), 128, 16);
        assert_eq!(c.grid, (13, 10));
        assert_eq!(c.cell_of(-100, -100), (0, 0));
        assert_eq!(c.cell_of(99, 49), (12, 9));
        assert_eq!(c.cell_point(0, 0, 0.5, 0.5), (-92, -92));
        // A partial edge cell's center is clamped into the world.
        assert_eq!(c.cell_point(12, 9, 0.5, 0.5), (99, 49));
        let (u, v) = c.cell_coords(-92, -92);
        assert!((u - 0.03125).abs() < 1e-9 && (v - 0.03125).abs() < 1e-9);
    }

    #[test]
    fn bilinear_interpolates_and_clamps() {
        let grid = AtlasGrid::from_values(2, 2, vec![0.0, 1.0, 2.0, 3.0]).unwrap();
        assert_eq!(grid.bilinear(0.0, 0.0), 0.0);
        assert_eq!(grid.bilinear(0.5, 0.0), 0.5);
        assert_eq!(grid.bilinear(0.5, 0.5), 1.5);
        assert_eq!(grid.bilinear(-3.0, 9.0), 2.0);
    }

    #[test]
    fn bicubic_passes_through_cell_centers_and_is_smooth() {
        let grid =
            AtlasGrid::from_values(4, 4, (0..16).map(|v| (v * v) as f32).collect()).unwrap();
        assert_eq!(grid.bicubic(1.0, 2.0), grid.get(1, 2));
        // Continuous across a cell border (no kink in value).
        let left = grid.bicubic(1.999, 1.5);
        let right = grid.bicubic(2.001, 1.5);
        assert!((left - right).abs() < 0.1);
    }

    #[test]
    fn warped_nearest_sampling_keeps_categories_and_breaks_up_cell_borders() {
        // Left half 3, right half 7: the border is the column x = 64.
        let context = AtlasContext::new(9, (0, 0), (128, 128), 128, 16);
        let values = (0..64).map(|i| if i % 8 < 4 { 3.0 } else { 7.0 }).collect();
        let grid = Arc::new(AtlasGrid::from_values(8, 8, values).unwrap());
        let step = |warp| SampleStep {
            grid: Arc::clone(&grid),
            context,
            out: 0,
            mode: 0,
            warp,
        };
        let read = |s: &SampleStep, x, y| {
            let mut slots = [0.0; WORLD_SLOTS];
            s.run(x, y, &mut slots);
            slots[0]
        };

        let (plain, warped) = (step(0.0), step(1.0));
        let mut moved = 0;
        for y in 0..128 {
            // Unwarped: the border is a straight line.
            assert_eq!(read(&plain, 63, y), 3.0);
            assert_eq!(read(&plain, 64, y), 7.0);
            for x in 0..128 {
                let v = read(&warped, x, y);
                assert!(v == 3.0 || v == 7.0, "only grid values: {v}");
                // Far from the border nothing changes (warp ≤ 1 cell).
                if !(40..88).contains(&x) {
                    assert_eq!(v, read(&plain, x, y));
                }
                if v != read(&plain, x, y) {
                    moved += 1;
                }
            }
            assert_eq!(read(&warped, 70, y), read(&warped, 70, y), "deterministic");
        }
        // The border wanders: a fair share of columns near it changed side.
        assert!(moved > 128 * 2, "only {moved} columns moved");
    }

    #[test]
    fn river_network_finds_the_nearest_segment() {
        let context = AtlasContext::new(1, (0, 0), (256, 256), 128, 16);
        let network = RiverNetwork::new(
            "n1",
            vec![RiverPath {
                points: vec![(8.0, 8.0), (200.0, 8.0)],
                discharge: vec![10.0, 30.0],
                level: vec![60.0, 40.0],
            }],
            context,
        );
        let hit = network.nearest(104.0, 20.0, 32.0).unwrap();
        assert!((hit.distance - 12.0).abs() < 1e-4);
        assert!((hit.discharge - 20.0).abs() < 1e-3);
        assert!((hit.level - 50.0).abs() < 1e-3);
        assert!(network.nearest(104.0, 100.0, 32.0).is_none());
    }

    #[test]
    fn downsampling_averages_blocks() {
        let grid =
            AtlasGrid::from_values(4, 2, vec![1.0, 3.0, 5.0, 7.0, 1.0, 3.0, 5.0, 7.0]).unwrap();
        let small = grid.downsampled(2);
        assert_eq!((small.width(), small.height()), (2, 1));
        assert_eq!(small.values(), &[2.0, 6.0]);
        assert_eq!(grid.downsampled(8), grid);
    }

    #[test]
    fn atlas_round_trips_through_bytes() {
        let context = AtlasContext::new(7, (-32, -32), (64, 48), 128, 16);
        let mut atlas = Atlas::empty(context);
        atlas.grids = vec![
            Some(Arc::new(context.grid(1.5))),
            None,
            Some(Arc::new(
                AtlasGrid::from_values(4, 3, (0..12).map(|v| v as f32).collect()).unwrap(),
            )),
        ];
        atlas.rivers = vec![
            None,
            Some(Arc::new(RiverNetwork::new(
                "n9",
                vec![RiverPath {
                    points: vec![(1.0, 2.0), (3.0, 4.0)],
                    discharge: vec![5.0, 6.0],
                    level: vec![50.0, 49.0],
                }],
                context,
            ))),
        ];
        let mut strata = StrataGrid::uniform(4, 3, 2, 60.0);
        strata.set_layers(5, &[(1, 10.0), (3, 70.0)]);
        atlas.strata = vec![Some(Arc::new(strata))];
        atlas.signature = "sig".into();

        let mut bytes = Vec::new();
        atlas.write(&mut bytes).unwrap();
        let read = Atlas::read(&mut bytes.as_slice()).unwrap();

        assert_eq!(read.context, atlas.context);
        assert_eq!(read.grids, atlas.grids);
        assert_eq!(read.rivers, atlas.rivers);
        assert_eq!(read.strata, atlas.strata);
        assert_eq!(read.materials.names(), atlas.materials.names());
        assert_eq!(read.biomes.names(), atlas.biomes.names());
        assert_eq!(read.signature, "sig");

        assert!(Atlas::read(&mut &bytes[..bytes.len() - 3])
            .unwrap_err()
            .contains("truncated"));
        assert!(Atlas::read(&mut &b"nope"[..]).is_err());
    }
}
