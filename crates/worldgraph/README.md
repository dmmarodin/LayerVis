# worldgraph

World-generation node graphs on [LayerGen](../../../LayerGen). One graph file,
edited in LayerVis, generates the same terrain in the editor and in the game:
both build their LayerGen pipeline from this crate.

## Using a graph in the game

```rust
use std::sync::Arc;
use layergen::{ChunkSize, LayerGenPlugin, PipelineConfig, WorldBounds};
use worldgraph::{GraphDoc, GraphPipeline, WorldStore};

let doc = GraphDoc::from_json(&std::fs::read_to_string("graph.json")?)?;
let config = PipelineConfig::new(
    WorldBounds::new((-4096, -4096, 0), (8192, 8192, 128))?,
    ChunkSize::new(32, 32, 32)?,
    doc.seed, // the seed saved with the graph (or your own, e.g. per save game)
);

// The graph's world-scope nodes (tectonics, elevation filters, …) are built
// once per world and shared (LayerGen shared data).
let world = Arc::new(WorldStore::for_doc(&doc, &config)?);
let graph = GraphPipeline::build(&doc, config, &world)?;

// Read values by export name, never by node id.
let height = graph.export("height").ok_or("graph has no `height` export")?.clone();

app.add_plugins(LayerGenPlugin::new(graph.definition(&world)));

// When a chunk completes (ChunkGenerator<Voxel, ColumnData>):
let h = chunk.chunk_data().columns[(x, y, 0)][height.slot];   // column export
// voxel exports: chunk.data()[(x, y, z)].slots[export.slot]
```

**Slow worlds (atlas nodes):** build the world inside LayerGen's preparation
step instead, so the app keeps running meanwhile:

```rust
let definition = worldgraph::DeferredGraphDefinition::new(doc, config);
let height = definition.export("height").unwrap().slot;        // known up front
let point = definition.export_world_slot("height");             // for world.graph().eval
let progress = definition.progress_handle();                    // atlas progress
app.add_plugins(LayerGenPlugin::new(definition));
// once ChunkGenerator::preparation_status() is Ready:
// generator.shared::<WorldStore>() is the world.
```

`ChunkManagerPlugin::<Voxel, ColumnData>::with_chunk_data(...)` works the same
way for managed chunks. `GraphPipeline::generate` is the synchronous variant.

Invalid nodes never fail the build: they are skipped and reported in
`graph.analysis().nodes` (`status`, `reason`). Check for the exports you need.

## Scopes: atlas, world, chunk, voxel

Every node runs at one scope, which is the scope of its outputs (and of its
sockets: `atlas`, `world`, `column`, `voxel`). They match LayerGen's levels:

| scope | runs | LayerGen | data |
|---|---|---|---|
| atlas | once, when the world is created, over whole-world grids (simulations) | shared data `D` (`WorldStore::atlas`) | f32 grids, one value per atlas cell |
| world | once, when the world is created; read-only afterwards | shared data `D` (`WorldStore`) | world slots, a pure function of `(x, y)` |
| chunk | per chunk, per column | chunk data `C` | column slots |
| voxel | per chunk, per cell | cells `T` | voxel slots |

Wires run downward only: a world output may feed a world input or a chunk
(`column`) input; chunk and voxel values never feed world nodes, and voxel
inputs only take voxel values (`can_connect`). A chunk input wired to a world
output reads a **sampled** copy: the world graph is evaluated once per column
and the needed outputs are copied into column slots before the chunk nodes
run (`Analysis::world_samples`). Exports of a world value therefore give the
game a column value.

World nodes are built once by `WorldStore::for_doc(&doc, &config)` (the world
is *prepared* data, so it fits LayerGen's `prepare`), and `GraphPipeline::build`
refuses a world built from different world nodes (`world_signature`). Editors
keep the world while only chunk or voxel nodes change. Read it from game
systems with `ChunkGenerator::shared::<WorldStore>()` and
`WorldStore::graph().eval(x, y)`.

## Atlas: whole-world simulations

Atlas nodes (`erosion`, `flow`) run once per world over coarse grids: one value
per atlas cell of `cell_size` x `cell_size` columns (the `extent` block,
default 16). They are global and eager, because erosion and river networks
depend on whole drainage basins. Atlas sockets connect only to atlas sockets;
world values reach them through **`rasterize`** (evaluated at cell centers,
`supersample`² points per cell) and results go back through **`sample`**
(`mode` 0 = nearest cell, 1 = bilinear), which is a world node. So the usual
chain is tectonics → `rasterize` → `erosion` → `sample` → world/chunk nodes.

- Building is the slow part: `WorldStore::for_doc_with(&doc, &config, hooks)`
  reports progress (`AtlasProgress`) and stops with `ATLAS_CANCELLED` when the
  `cancelled` hook returns true. `WorldStore::has_atlas_work(&doc)` tells
  whether a graph has atlas nodes; `WorldStore::placeholder` builds a world
  with zero-filled grids at once (editors publish it while the real atlas
  builds).
- Results are bit-deterministic for the same graph, seed and extent.
- **Saving:** `world.atlas().write(&mut file)` writes the grids and river lines
  (versioned little-endian binary). A game loads them with `Atlas::read` and
  `WorldStore::from_atlas(&doc, &config, &atlas)`, which runs no atlas node and
  refuses an atlas built from different world nodes, seed or extent.
- `world.atlas()` also holds river polylines per `flow` node (`rivers()`) and
  downsampled scrubber frames (`snapshots()`, not saved).
- Atlas node kinds: `erosion` (uplift + stream-power erosion, Braun & Willett
  2013; outputs `elevation`, `drainage`, `eroded`), `flow` (`water_level`,
  `lake_depth`, `discharge`, `river` + river lines); world helper
  `tectonic_uplift`; adapters `rasterize`, `sample`, `reroute_atlas`. Template
  `eroded_continent` wires them up.

## Exports: the contract between graph and game

An **Export** node (`export_column` / `export_voxel`) takes one input and
publishes it under the node's `label` (default `export`). Export names are
unique per graph. Exports cost nothing at runtime: they alias the slot of the
output wired into them. Rewire or replace upstream nodes freely; game code keeps
reading the same name.

## File format (`format: 3`)

```json
{
  "format": 3,
  "seed": 12345,
  "extent": { "size": [8192, 8192], "depth": 128, "cellSize": 16 },
  "nodes": [
    { "id": "n1", "kind": "tectonics", "params": { "plate_size": 1100, "continents": 1 }, "position": [0, 0] },
    { "id": "n2", "kind": "tectonic_elevation", "position": [340, 0] },
    { "id": "n3", "kind": "edge_falloff", "params": { "margin": 0.08 }, "position": [680, 0] },
    { "id": "n4", "kind": "smooth", "params": { "radius": 2 }, "position": [1020, 0] },
    { "id": "n5", "kind": "export_column", "label": "height", "position": [1360, 0] }
  ],
  "edges": [
    { "from": { "node": "n1", "socket": "boundary_distance" }, "to": { "node": "n2", "socket": "boundary_distance" } },
    { "from": { "node": "n2", "socket": "elevation" }, "to": { "node": "n3", "socket": "elevation" } },
    { "from": { "node": "n3", "socket": "elevation" }, "to": { "node": "n4", "socket": "value" } },
    { "from": { "node": "n4", "socket": "smoothed" }, "to": { "node": "n5", "socket": "value" } }
  ]
}
```

(abridged: `tectonic_elevation` has four inputs, all wired from `tectonics`.)

- `extent` (optional): world size in columns (centered on 0, 0), depth in
  cells and atlas cell size (default 16). Part of the world definition: build
  the `PipelineConfig` bounds from it (`Extent::origin`, `size`, `depth`);
  `WorldStore::for_doc` rejects a config that does not match. Without it the
  config decides the size and the atlas uses 16-column cells.
- `seed` (u64, default 12345 when missing): the world seed. A JSON number; JS
  readers lose precision above 2^53, so read it with a u64-safe parser.
- `id`: unique, stable. `enabled` (default true), `position` (layout only),
  `params` (missing → defaults, values clamped), `label`, `script` are optional.
- An input takes one edge; outputs fan out. Wires follow the scope rules above
  (`can_connect`). Dangling or mistyped edges are reported, not fatal.
- Execution order is the topological order of edges (ties: node id; world
  nodes first). Layout never affects results.
- `frames` (optional, omitted when empty): `{id: "f<k>", label, position, size}`
  editor annotations drawn behind nodes. They never affect execution; the game
  can ignore them.
- Newer formats are rejected; older ones are upgraded by `GraphDoc::from_json`
  (`migrate.rs`). **1 → 2:** the built-in world models became world nodes. The
  `elevation` chunk node becomes `tectonic_elevation` (same id and `elevation`
  output, so its wires keep working), a `tectonics` node is added and wired in,
  the old `world` params block (plate size, continents, edge vignette, …) moves
  onto the nodes (a used vignette becomes an `edge_falloff` node behind each
  elevation node), and the block is no longer written. The upgraded graph
  generates the same world. **2 → 3:** adds the optional `extent` block;
  nothing to convert.

## Terrain: world parameters live on world nodes

The terrain is a chain of world-scope nodes. With default params it is the
original world, bit for bit (tested against the old hardcoded model):

- **`tectonics`** (→ `plate`, `boundary_distance`, `convergence`, `continental`,
  `neighbor_continental`): plates on a jittered grid with warped Voronoi
  boundaries, built once per world. `plate_size` (target diameter in cells;
  plate count = world area / size², 30 plates for 8192²), `continents` (0 =
  every plate is land by an independent coin flip, scattered land; N ≥ 1 =
  land plates clustered around N continent cores), `continental_fraction`,
  `warp`.
- **`tectonic_elevation`** (→ `elevation`, cells): plate base heights
  (`land_height`, `ocean_height`), mountains where plates collide
  (`mountains`), rifts and ridges, fractal `detail`.
- **`edge_falloff`** (filter, `elevation` → `elevation`): the "vignette".
  Pulls terrain down to the sea floor near the world edges: the first `margin`
  (fraction of the smaller world side) is always ocean, terrain fades in over
  `falloff`; `noise` makes the coast irregular and only ever pushes it
  inward, so the margin holds; `sea_floor` is the edge height (fraction of
  world depth; sea level is 0.3). Anything downstream (temperature, solid,
  exports) wires to the filtered output.

**Climate** is world scope too: `temperature` (warm equator, cold poles, colder
with altitude) and `humidity` (wet oceans and lowlands, drier with altitude
and heat) read the elevation (and temperature) and are pure functions of the
position, so they have zoomed-out views and chunk nodes read them through
sampling. Chunk-level processing (smoothing, slope, scripts) goes on top of
them as chunk nodes. A chunk output cannot feed a climate node (wires only
run downward).

`WorldStore::stats(slot, resolution)` reports land fraction, landmass count and
border land of an elevation output (`GraphPipeline::elevation_world_slot` finds
the terrain's). `steps::TEMPLATES` are starting graphs built from these nodes
(single continent, two continents, archipelago, pangaea; each ends in an edge
falloff, so all four edges are ocean). Two nearby continents can occasionally
be joined by island arcs (convergent ocean plates), so `continents` is a
target, not a guarantee.

## Node kinds

| kind | inputs → outputs | params |
|---|---|---|
| `tectonics` | → `plate`, `boundary_distance`, `convergence`, `continental`, `neighbor_continental` (world) | `plate_size`, `continents`, `continental_fraction`, `warp` |
| `tectonic_elevation` | the four plate values → `elevation` (world, cells) | `land_height`, `ocean_height`, `mountains`, `detail` |
| `edge_falloff` | `elevation` → `elevation` (world filter) | `margin`, `falloff`, `noise`, `sea_floor` |
| `temperature` | `elevation` → `temperature` (world, °C) | `equator`, `pole_drop`, `lapse` |
| `humidity` | `elevation`, `temperature` → `humidity` (world, 0–1) | |
| `slope` | `elevation` → `slope` (neighborhood r=1) | |
| `smooth` | `value` → `smoothed` (neighborhood) | `radius` 1–8 |
| `solid` | `elevation` → `solid` (voxel) | |
| `export_column` / `export_voxel` | `value` → (named export) | name = `label` |
| `world_noise` | → `noise` (world, 0…1) | `wavelength`, `octaves`, `variant` |
| `world_remap` | `value` (world) → `remapped` (world) | `in_min`, `in_max`, `out_min`, `out_max`, `clamp` |
| `reroute_column` / `reroute_voxel` / `reroute_world` | `in` → `out` (same type) | wiring waypoint: no runtime work, no slot, no field |
| `script` | declared in the Rhai header; world values arrive through inputs (`input: elevation, boundary_distance`) | (feature `scripting`) |

At most one neighborhood node runs per graph (LayerGen has one cross-chunk
stage); its ancestors form a minimal prefix whose results are cached.

## Determinism

Every node kind is a pure function of the seed, world bounds,
chunk coordinate and its inputs. Same graph + same config → identical chunks in
the editor and the game, independent of request order.

## Features

- `scripting` (default): Rhai script nodes. Without it, script nodes load but
  report as blocked, so a game build can refuse scripts and require them to be
  ported to Rust node kinds.
