# Milestone 15 (redirected): the atlas scope

Status: **implemented, tests passing, not yet verified in the app (2026-11-15).** Differences from the plan below:
- **Background build.** The edit publishes at once with a *placeholder* world (zero-filled atlas grids). The real world is published as the next revision when it is ready. While building, chunk and world requests return "The atlas is still being built" and the map waits.
- **Default graph unchanged.** The eroded chain is the new template "Eroded continent (atlas)". The plan made it the default.
- **No deposition.** `erosion` outputs `elevation`/`drainage`/`eroded` (no `sediment`, no deposition parameter).
- **Fewer erosion inputs.** It has no optional `erodibility`/`precipitation` inputs.
- **Doc block named `extent`**, not `world` (that name belongs to the legacy format-1 block).

See LayerVis `CLAUDE.md` "Atlas" for the as-built summary.

Original plan status: **plan (2026-11-15).** It replaces the old milestone 15 ("regional world models:
erosion + rivers, lazy regions"). The design behind it is `../rogue-life/docs/world-generation.md`,
mainly sections 3, 4, 6 and 13. The user has decided every open item (marked **Decided**), and the
plan is ready to implement.

## Goal

worldgraph gets a fourth scope, **atlas**: stored grids for the whole world, computed **once,
eagerly**, when the world is built. The first real simulations run there: uplift plus stream-power
erosion, then flow and rivers. LayerVis builds atlases in the background with progress, shows atlas
fields as world views, and can scrub through simulation steps. The game can save an atlas into a
world file and load it without recomputing.

Why eager and global instead of lazy regions: river networks and erosion depend on the whole
drainage basin, so regions computed independently would disagree at their borders. Lazy computation
stays reserved for later *region detail*.

## What exists today (relevant code)

- `FieldScope { Voxel, Column, World }`, with `rank()` and `can_connect()` in `crates/worldgraph/src/lib.rs`.
- World nodes:
  - `Build::World(fn(&Bindings, &Params, &WorldBuildContext) -> BoxedWorldStep)`.
  - `WorldStep::run(x, y, &mut [f64; 16])` is a pure function per point.
  - `WorldGraph::eval(x, y)` runs every world step in order (`pipeline.rs`).
- `WorldStore::for_doc` compiles the world graph once. Its `world_signature(doc)` covers world nodes
  and their wires (`world.rs`, `pipeline.rs`).
- `src-tauri/src/generation/mod.rs::build`:
  - Builds the world **synchronously on every edit** that changes the seed or world nodes, and
    reuses it otherwise.
  - Bounds are hardcoded in `config_for`: `(-4096, -4096, 0)` to `(8192, 8192, 128)`.
  - Every edit is validated by generating chunk (0,0).

---

## Design

### Scope and wiring

- New `FieldScope::Atlas`. A socket of that type holds an **f32 grid** with one value per atlas cell.
- Wiring (**Decided: explicit adapter nodes**):
  - `can_connect` adds only **atlas → atlas**. Atlas and world never connect directly.
  - **`rasterize`** (input `world`, output `atlas`; atlas scope) evaluates a world value at every
    atlas cell center.
    - Param `supersample` (1–4 samples per axis, averaged).
    - It lets the analytic `tectonics` and noise nodes feed simulations.
  - **`sample`** (input `atlas`, output `world`; world scope) reads an atlas grid at (x, y).
    - Param `mode`: nearest or bilinear. Bicubic can come later.
    - Atlas → column goes through `sample` and then the existing implicit world → column sampling.
  - Both are ordinary nodes with one input and one output, like reroutes. A `Contract`'s scope is its
    output's scope, so the existing rules hold: `rasterize` is atlas scope, `sample` is world scope.
  - Column and voxel never feed atlas. Cycles are already rejected.
- Execution:
  - At world build time, atlas nodes run in topological order.
  - A `rasterize` node evaluates only its **ancestor** world steps at the cell centers. It never
    runs world steps that read through a `sample` of an atlas grid not built yet.
  - `sample` nodes become `WorldStep`s that read the finished grid, and `WorldGraph::eval` runs
    them in graph order like any world step.
- Slot spaces: atlas outputs get SSA slots in a new atlas slot space. Grids live in a `Vec`, so
  there is no small fixed array.

### Types

```rust
pub struct AtlasGrid { width: u32, height: u32, values: Vec<f32> }   // row-major, cell centers

pub struct AtlasContext {
    pub seed: u64,
    pub origin: (i64, i64),
    pub size: (u32, u32),     // world, in cells (tiles)
    pub depth: u32,
    pub cell_size: u32,       // tiles per atlas cell
    pub grid: (u32, u32),     // atlas cells per side
}

pub trait AtlasStage: Send + Sync + 'static {
    /// Reads input grids, writes output grids. Long stages report progress,
    /// check for cancellation, and may emit snapshots for the scrubber.
    fn run(&self, io: &mut AtlasIo, context: &AtlasContext, run: &mut AtlasRun) -> Result<(), String>;
}

Build::Atlas(fn(&Bindings, &Params, &AtlasContext) -> Result<BoxedAtlasStage, String>)
```

- `AtlasIo` gives `input(slot) -> &AtlasGrid` and `output(slot) -> &mut AtlasGrid`.
- `AtlasRun` holds:
  - `progress(fraction)`
  - `cancelled() -> bool`; returning early gives `Err("cancelled")`
  - `snapshot(label, &AtlasGrid)`: snapshots are downsampled to at most 256² and capped per node
- The finished `Atlas` holds the grids, the snapshots, the context, and the atlas signature.
- `WorldStore` gains `atlas: Option<Arc<Atlas>>`.

### World size and atlas resolution (Decided: format 3)

- The doc gets an optional `world` block: `{ size: [w, h], depth, cell_size }`.
  - Defaults: today's 8192 x 8192 x 128, with `cell_size` 16, which gives a 512² atlas, enough to
    see erosion work.
  - The game's sizes (tiny to huge, 128-tile regions, 384 deep) come from the same block.
- `config_for` reads the block.
- `format: 3`. Migration 2 → 3 adds nothing: an absent block means today's defaults, so old graphs
  generate identical worlds.
- `cell_size` is a world setting, not a node parameter, because every atlas node must share one
  grid. Changing it rebuilds the atlas.

### Signature and determinism

- `world_signature` covers atlas nodes too, plus the `world` block, since both decide the stored
  base.
- Atlas stages must be **bit-deterministic** for a given seed, parameters and size:
  - fixed iteration order
  - no parallel float reductions that depend on thread timing (rayon over rows is fine; per-row
    work stays sequential)
  - deterministic tie-breaks in the priority queues
- Test: build twice, compare the grids byte for byte.

### Background build in LayerVis (Decided)

Running an atlas can take seconds or minutes, so it can't stay inside the synchronous edit path.

- An edit that changes atlas-relevant parts (atlas nodes, their world ancestors, the `world` block,
  the seed) is **validated structurally** (analysis, bindings, slot counts) and published right
  away.
- The atlas then builds on a background thread. The world state becomes `Building { progress }` and
  then `Ready`.
- A newer edit **cancels** a running build: `AtlasRun::cancelled` checks a generation counter.
- Chunk streaming and the world overlay wait for `Ready`. The frontend shows a progress bar listing
  the node currently running.
- The chunk (0,0) validation for such edits runs once the atlas is ready. A failure there marks the
  revision as blocked, instead of rejecting the edit after the fact.
- Edits to chunk or voxel nodes reuse the ready atlas, as they reuse the world today.
- Later: a fast low-resolution preview pass (for example 128²) before the full build. Not in this
  milestone.

### Persistence

- `Atlas::write(&mut impl Write)` and `Atlas::read(&mut impl Read)` use a versioned little-endian
  binary format:
  - header: magic, version, context, signature
  - then each grid as f32 planes
  - snapshots are not saved
- `WorldStore::from_atlas(doc, config, atlas)` checks the signature and builds the world without
  running any stage. This is what the game uses with its world file.
- Test: build, write, read, then `from_atlas`. The result must give identical world samples and
  chunks.
- LayerVis may cache the last atlas on disk under app data, keyed by a hash of the signature, so a
  restart doesn't recompute it. Optional, and last in the order.

---

## First nodes

1. **`tectonic_uplift`** (world, analytic): reads the `tectonics` outputs and gives an uplift rate.
   Collisions push up, rifts sink, and the rate falls off with distance from the boundary. It reuses
   the shaping in `tectonic_elevation`.
2. **`erosion`** (atlas): implements Braun & Willett 2013 / Cordonnier et al. 2016 (uplift plus
   stream power).
   - Inputs:
     - `elevation`, the starting height (usually rasterized `tectonic_elevation`)
     - `uplift`
     - optional `erodibility` (rock hardness; strata will feed it later)
     - optional `precipitation`
   - Params: `iterations`, `time_step`, `k` (erosion constant), `m`, `n`, `talus` (thermal angle),
     `deposition`.
   - Each iteration:
     1. Pick D8 steepest-descent receivers.
     2. Treat ocean cells (below sea level) as base level.
     3. Route flow across depressions with **priority-flood** (Barnes 2014). Without this, inland
        basins would trap rivers.
     4. Order cells into a stack and accumulate drainage area, weighted by precipitation.
     5. Do the implicit height update in stack order.
     6. Apply a thermal relaxation pass.
   - Outputs: `elevation`, `drainage` (accumulated area), `sediment` (net deposition).
   - Snapshots every N iterations.
3. **`flow`** (atlas):
   - Inputs: `elevation`, optional `precipitation`.
   - Outputs:
     - `filled` (depression-filled surface)
     - `lake_depth`
     - `discharge`
     - `river` (a 0–1 strength above a threshold, for display)
   - It also records **river polylines** (downstream traces from source cells above the threshold,
     merged at confluences) on the `Atlas` as vector data. LayerVis shows them as an overlay. Chunk
     carving comes in a later milestone.
4. **`rasterize`** (world → atlas) and **`sample`** (atlas → world): the adapters described under
   "Scope and wiring".

Templates: the default template becomes:
- tectonics → `tectonic_elevation` → `rasterize` → erosion (`elevation` input)
- tectonics → `tectonic_uplift` → `rasterize` → erosion (`uplift` input)
- erosion → flow → `sample` → `edge_falloff`

So the template has two `rasterize` nodes. Keep a "classic" template with today's analytic chain for quick experiments.

`WorldStore::stats` and `elevation_world_slot` already pick the last world output named
`elevation`. Atlas elevation reaches world scope through a `sample` node, so its output name must
be `elevation` (it takes the name of its input), or the stats find nothing. Check this.

## LayerVis changes

- **Graph:**
  - The new socket type and color for atlas.
  - `canConnect` mirrors the new rule (atlas → atlas).
  - `rasterize` and `sample` appear in the add menu.
  - Auto-connect on add picks the matching scopes.
  - The node scope badge.
- **Atlas build status:** a progress bar plus the current node, and a cancel button (or an edit,
  which also cancels).
- **World views:**
  - Atlas fields join `WorldInfo.world_fields`, served as **whole grids**. One binary payload per
    field reuses `encode_planes`, so there is no per-point sampling.
  - Hover readouts include atlas values.
- **Scrubber:**
  - Pick an atlas node that has snapshots, and drag through its iterations.
  - Each snapshot is shown as a world view.
  - Snapshots are fetched on demand, as binary data.
- **River overlay:** the polylines, with stroke width from discharge, toggled in the View section.
- **Sidebar World section:** world size, depth and cell size, as one undoable edit that rebuilds
  the atlas.

## Order of work

1. `FieldScope::Atlas`, `can_connect` (atlas → atlas), and contracts. Analysis gets atlas slots,
   the `rasterize` and `sample` nodes, and the ancestor evaluation. Tests: wiring rules (world and
   atlas never connect directly), cycle rejection, rasterized values (with supersampling), and
   sampled values (nearest and bilinear).
2. `AtlasGrid`, `AtlasContext`, `AtlasStage`, `Build::Atlas`, `Atlas`, and building in
   `WorldStore::for_doc` with an `AtlasRun` (progress, cancel, snapshots). The signature covers
   atlas nodes. Test: deterministic rebuild.
3. The doc `world` block, format 3, migration and `config_for`. Test: a format 2 graph generates
   identical chunks.
4. `tectonic_uplift` and `erosion`. Tests:
   - mass and flow invariants (every land cell drains to the ocean or a lake)
   - no NaNs
   - deterministic output
   - mountains survive where uplift balances erosion
   - a timing print for 512²
5. `flow` and river polylines. Tests:
   - rivers end in the ocean, a lake or the world edge
   - discharge never decreases downstream
6. The background build in Session, the progress and cancel commands, and the frontend progress UI.
7. Frontend atlas views, the scrubber and the river overlay.
8. `Atlas::write`/`read` and `WorldStore::from_atlas`, plus a round-trip test, and the
   `game_runtime.rs` example updated to save and load an atlas.
9. Docs: `crates/worldgraph/README.md` (scopes table, atlas, persistence) and `CLAUDE.md`.

Checks, as usual:
- `cd crates/worldgraph && cargo test && cargo test --no-default-features`
- `cd src-tauri && cargo test`
- `npm run check`

Then the user verifies in `npm run tauri dev`.

## Not in this milestone (next milestones, in the design's order)

- **Geological epochs**: moving plates, sea-level cycles and glaciation. This needs a decision on
  how loops appear in the graph: a composite `geology` node or a loop frame. **Decided:** choose
  once the atlas scope exists.
  Uplift plus erosion gives believable mountains without it.
- Strata and materials: the `material` socket type (u16 registry ids), stratigraphy, erodibility
  from strata.
- Climate on the atlas: wind bands, moisture advection, rain shadow, seasonal normals.
  Precipitation then feeds erosion and flow.
- Hydrology finish (lakes as polygons, deltas, wetlands) and the biome parameter table.
- Base generation from the atlas: river carving, then the scatter placement pattern.
- Region detail (lazy, cached). A low-resolution atlas preview pass.

## Risks

- **Build time in LayerVis's dev profile** (opt-level 1): 512² × a few hundred iterations may take
  tens of seconds. Measure in step 4. If it is slow, give `worldgraph` opt-level 3 in the dev
  profile (as is done for dependencies), or lower the default iteration count.
- **Memory**: about 1 MB per 512² field and 16 MB per 2048² field. Snapshots are capped, so this is
  fine.
- **Seams**: bilinear sampling of a 16-tile grid leaves visible facets in the world height.
  Chunk-level detail noise has to hide them, and that belongs to the "base generation from atlas"
  milestone. Expect a faceted look until then.
