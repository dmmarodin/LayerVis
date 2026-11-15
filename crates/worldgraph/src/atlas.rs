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
    pipeline::{Build, FieldSpec, InputSpec, NodeKind, ParamSpec, WorldStep},
    FieldScope, WORLD_SLOTS,
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
        }
    }

    /// A grid of this atlas's size.
    pub fn grid(&self, fill: f32) -> AtlasGrid {
        AtlasGrid::new(self.grid.0, self.grid.1, fill)
    }

    /// Elevation below which a column is ocean (matches `WorldStore::sea_level`).
    pub fn sea_level(&self) -> f32 {
        self.depth as f32 * 0.3
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
/// discharge (accumulated drainage, in cells) at each point.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RiverPath {
    pub points: Vec<(f32, f32)>,
    pub discharge: Vec<f32>,
}

// ---------------------------------------------------------------------------
// Stages

/// One atlas-scope node, built once per world and run once.
pub trait AtlasStage: Send + Sync + 'static {
    fn run(&self, io: &mut AtlasIo<'_>, run: &mut AtlasRun<'_>) -> Result<(), String>;
}

pub type BoxedAtlasStage = Box<dyn AtlasStage>;

/// What an atlas stage reads and writes: input grids by slot (finished
/// earlier in graph order), and its own outputs.
pub struct AtlasIo<'a> {
    context: &'a AtlasContext,
    grids: &'a [Option<Arc<AtlasGrid>>],
    written: Vec<(usize, AtlasGrid)>,
    rivers: Option<Vec<RiverPath>>,
}

impl<'a> AtlasIo<'a> {
    pub(crate) fn new(context: &'a AtlasContext, grids: &'a [Option<Arc<AtlasGrid>>]) -> Self {
        Self {
            context,
            grids,
            written: Vec::new(),
            rivers: None,
        }
    }

    pub fn context(&self) -> &AtlasContext {
        self.context
    }

    pub fn input(&self, slot: usize) -> Result<&AtlasGrid, String> {
        self.grids
            .get(slot)
            .and_then(Option::as_deref)
            .ok_or_else(|| format!("atlas input slot {slot} has no grid"))
    }

    /// Stores an output grid (must be the atlas size).
    pub fn write(&mut self, slot: usize, grid: AtlasGrid) -> Result<(), String> {
        if (grid.width, grid.height) != self.context.grid {
            return Err(format!(
                "output grid is {}x{}, the atlas is {}x{}",
                grid.width, grid.height, self.context.grid.0, self.context.grid.1
            ));
        }
        self.written.retain(|(s, _)| *s != slot);
        self.written.push((slot, grid));
        Ok(())
    }

    /// River lines this node produced (kept on the atlas under the node id).
    pub fn set_rivers(&mut self, rivers: Vec<RiverPath>) {
        self.rivers = Some(rivers);
    }

    pub(crate) fn finish(self) -> (Vec<(usize, AtlasGrid)>, Option<Vec<RiverPath>>) {
        (self.written, self.rivers)
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

/// Callbacks for a long atlas build (both optional).
#[derive(Clone, Copy, Default)]
pub struct AtlasHooks<'a> {
    pub progress: Option<&'a (dyn Fn(&AtlasProgress) + Sync)>,
    /// Polled during the build; returning true stops it with `CANCELLED`.
    pub cancelled: Option<&'a (dyn Fn() -> bool + Sync)>,
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
            self.snapshots.push(AtlasSnapshot {
                node: self.node.clone(),
                label: label.into(),
                grid: grid.downsampled(SNAPSHOT_SIDE),
            });
        }
    }

    pub(crate) fn into_snapshots(self) -> Vec<AtlasSnapshot> {
        self.snapshots
    }
}

// ---------------------------------------------------------------------------
// The finished atlas

/// Every atlas grid of one world, by atlas slot, plus vector features and
/// scrubber snapshots.
#[derive(Clone, Debug)]
pub struct Atlas {
    context: AtlasContext,
    pub(crate) grids: Vec<Option<Arc<AtlasGrid>>>,
    pub(crate) rivers: Vec<(String, Vec<RiverPath>)>,
    pub(crate) snapshots: Vec<AtlasSnapshot>,
    /// `world_signature` of the graph the atlas was built from.
    pub(crate) signature: String,
}

/// Magic and version of `Atlas::write`.
const ATLAS_MAGIC: &[u8; 8] = b"WGATLAS\0";
const ATLAS_VERSION: u32 = 1;

impl Atlas {
    pub(crate) fn empty(context: AtlasContext) -> Self {
        Self {
            context,
            grids: Vec::new(),
            rivers: Vec::new(),
            snapshots: Vec::new(),
            signature: String::new(),
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

    /// River lines per producing node id.
    pub fn rivers(&self) -> &[(String, Vec<RiverPath>)] {
        &self.rivers
    }

    pub fn snapshots(&self) -> &[AtlasSnapshot] {
        &self.snapshots
    }

    pub fn signature(&self) -> &str {
        &self.signature
    }

    /// Saves grids and rivers (not snapshots) in a versioned little-endian
    /// binary format, for a game's world file.
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

        out.write_all(&(self.rivers.len() as u32).to_le_bytes())?;
        for (node, paths) in &self.rivers {
            write_str(out, node)?;
            out.write_all(&(paths.len() as u32).to_le_bytes())?;
            for path in paths {
                out.write_all(&(path.points.len() as u32).to_le_bytes())?;
                for (&(x, y), &discharge) in path.points.iter().zip(&path.discharge) {
                    for value in [x, y, discharge] {
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

        let river_sets = reader.count(1 << 16)?;
        let mut rivers = Vec::with_capacity(river_sets);
        for _ in 0..river_sets {
            let node = reader.string()?;
            let paths = reader.count(1 << 24)?;
            let mut set = Vec::with_capacity(paths.min(1 << 16));
            for _ in 0..paths {
                let points = reader.count(1 << 24)?;
                let mut path = RiverPath {
                    points: Vec::with_capacity(points.min(1 << 16)),
                    discharge: Vec::with_capacity(points.min(1 << 16)),
                };
                for _ in 0..points {
                    path.points.push((reader.f32()?, reader.f32()?));
                    path.discharge.push(reader.f32()?);
                }
                set.push(path);
            }
            rivers.push((node, set));
        }

        Ok(Self {
            context,
            grids,
            rivers,
            snapshots: Vec::new(),
            signature,
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
    }],
    outputs: &[FieldSpec {
        name: Cow::Borrowed("out"),
        scope: FieldScope::Atlas,
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
                  nodes can use it. `mode` 0 = nearest cell, 1 = bilinear.",
    inputs: &[InputSpec {
        name: Cow::Borrowed("in"),
        scope: FieldScope::Atlas,
    }],
    outputs: &[FieldSpec {
        name: Cow::Borrowed("out"),
        scope: FieldScope::World,
        unit: Cow::Borrowed(""),
        min: 0.0,
        max: 1.0,
        ramp: Cow::Borrowed("grayscale"),
        display_from: Some(Cow::Borrowed("in")),
    }],
    params: &[ParamSpec {
        name: "mode",
        default: 1.0,
        min: 0.0,
        max: 1.0,
    }],
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
    }],
    outputs: &[FieldSpec {
        name: Cow::Borrowed("out"),
        scope: FieldScope::Atlas,
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
    pub bilinear: bool,
}

impl WorldStep for SampleStep {
    fn run(&self, x: i64, y: i64, slots: &mut [f64; WORLD_SLOTS]) {
        slots[self.out] = if self.bilinear {
            let (u, v) = self.context.cell_coords(x, y);
            f64::from(self.grid.bilinear(u, v))
        } else {
            let (cx, cy) = self.context.cell_of(x, y);
            f64::from(self.grid.get(cx, cy))
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
        atlas.rivers = vec![(
            "n9".into(),
            vec![RiverPath {
                points: vec![(1.0, 2.0), (3.0, 4.0)],
                discharge: vec![5.0, 6.0],
            }],
        )];
        atlas.signature = "sig".into();

        let mut bytes = Vec::new();
        atlas.write(&mut bytes).unwrap();
        let read = Atlas::read(&mut bytes.as_slice()).unwrap();

        assert_eq!(read.context, atlas.context);
        assert_eq!(read.grids, atlas.grids);
        assert_eq!(read.rivers, atlas.rivers);
        assert_eq!(read.signature, "sig");

        assert!(Atlas::read(&mut &bytes[..bytes.len() - 3])
            .unwrap_err()
            .contains("truncated"));
        assert!(Atlas::read(&mut &b"nope"[..]).is_err());
    }
}
