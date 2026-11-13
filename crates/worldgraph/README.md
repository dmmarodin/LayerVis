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
    seed,
);

// World models (tectonics…) are built once per seed and shared.
let world = Arc::new(WorldStore::new(&config));
let graph = GraphPipeline::build(&doc, config, &world)?;

// Read values by export name, never by node id.
let height = graph.export("height").ok_or("graph has no `height` export")?.clone();

app.add_plugins(LayerGenPlugin::new(graph.definition(&world)));

// When a chunk completes (ChunkGenerator<Voxel, ColumnData>):
let h = chunk.chunk_data().columns[(x, y, 0)][height.slot];   // column export
// voxel exports: chunk.data()[(x, y, z)].slots[export.slot]
```

`ChunkManagerPlugin::<Voxel, ColumnData>::with_chunk_data(...)` works the same
way for managed chunks. `GraphPipeline::generate` is the synchronous variant.

Invalid nodes never fail the build: they are skipped and reported in
`graph.analysis().nodes` (`status`, `reason`). Check for the exports you need.

## Exports: the contract between graph and game

An **Export** node (`export_column` / `export_voxel`) takes one input and
publishes it under the node's `label` (default `export`). Export names are
unique per graph. Exports cost nothing at runtime: they alias the slot of the
output wired into them. Rewire or replace upstream nodes freely; game code keeps
reading the same name.

## File format (`format: 1`)

```json
{
  "format": 1,
  "nodes": [
    { "id": "n1", "kind": "elevation", "position": [0, 0] },
    { "id": "n2", "kind": "smooth", "params": { "radius": 2 }, "position": [300, 0] },
    { "id": "n3", "kind": "export_column", "label": "height", "position": [600, 0] },
    { "id": "n4", "kind": "script", "script": "//! name: …", "enabled": false }
  ],
  "edges": [
    { "from": { "node": "n1", "socket": "elevation" }, "to": { "node": "n2", "socket": "value" } },
    { "from": { "node": "n2", "socket": "smoothed" }, "to": { "node": "n3", "socket": "value" } }
  ]
}
```

- `id`: unique, stable. `enabled` (default true), `position` (layout only),
  `params` (missing → defaults, values clamped), `label`, `script` are optional.
- An input takes one edge; outputs fan out. Socket types must match
  (`column` or `voxel`). Dangling or mistyped edges are reported, not fatal.
- Execution order is the topological order of edges (ties: node id). Layout
  never affects results.
- Newer formats are rejected; older ones are migrated by `GraphDoc::from_json`.

## Node kinds

| kind | inputs → outputs | params |
|---|---|---|
| `elevation` | → `elevation` (column, tectonics) | |
| `temperature` | `elevation` → `temperature` | `equator`, `pole_drop`, `lapse` |
| `humidity` | `elevation`, `temperature` → `humidity` | |
| `slope` | `elevation` → `slope` (neighborhood r=1) | |
| `smooth` | `value` → `smoothed` (neighborhood) | `radius` 1–8 |
| `solid` | `elevation` → `solid` (voxel) | |
| `export_column` / `export_voxel` | `value` → (named export) | name = `label` |
| `script` | declared in the Rhai header | (feature `scripting`) |

At most one neighborhood node runs per graph (LayerGen has one cross-chunk
stage); its ancestors form a minimal prefix whose results are cached.

## Determinism

Every node kind and world model is a pure function of the seed, world bounds,
chunk coordinate and its inputs. Same graph + same config → identical chunks in
the editor and the game, independent of request order.

## Features

- `scripting` (default): Rhai script nodes. Without it, script nodes load but
  report as blocked, so a game build can refuse scripts and require them to be
  ported to Rust node kinds.
