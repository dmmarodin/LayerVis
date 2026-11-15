//! Built-in node kinds. Each declares typed input/output sockets and
//! params; the graph wires sockets explicitly and steps receive resolved
//! slot indices (`Bindings`), never names. Kinds must be deterministic and
//! portable: the game builds its pipeline from these same kinds.

use std::borrow::Cow;

use layergen::{
    ChunkStep, DataSet, NeighborChunks, NeighborhoodChunkStep, NeighborhoodView,
    PipelineStepResult, StepContext,
};

use crate::{
    atlas::{RASTERIZE, REROUTE_ATLAS, SAMPLE},
    biomes::BIOME,
    climate::{HUMIDITY, MOISTURE, SEASONS, TEMPERATURE, WIND},
    detail::{RIVER_CARVE, SCATTER, TERRAIN_DETAIL},
    geology::{EROSION, FLOW, GEOLOGY, TECTONIC_UPLIFT},
    strata::{EXPORT_MATERIAL, STRATA, STRATA_FILL},
    graph::{EdgeDoc, GraphDoc, NodeDoc, SocketRef},
    pipeline::{
        Bindings, BoxedNeighborhoodStep, BoxedStep, BoxedWorldStep, Build, FieldSpec, InputSpec,
        NodeKind, ParamSpec, Params, WorldStep,
    },
    terrain::{EDGE_FALLOFF, TECTONICS, TECTONIC_ELEVATION},
    world::WorldStore,
    ColumnData, FieldScope, ValueKind, Voxel, WORLD_SLOTS,
};

static REGISTRY: [&NodeKind; 32] = [
    &TECTONICS,
    &TECTONIC_ELEVATION,
    &EDGE_FALLOFF,
    &TEMPERATURE,
    &HUMIDITY,
    &SLOPE,
    &SMOOTH,
    &SOLID,
    &EXPORT_COLUMN,
    &EXPORT_VOXEL,
    &REROUTE_COLUMN,
    &REROUTE_VOXEL,
    &WORLD_NOISE,
    &WORLD_REMAP,
    &REROUTE_WORLD,
    &RASTERIZE,
    &SAMPLE,
    &REROUTE_ATLAS,
    &TECTONIC_UPLIFT,
    &EROSION,
    &FLOW,
    &STRATA,
    &STRATA_FILL,
    &EXPORT_MATERIAL,
    &WIND,
    &MOISTURE,
    &SEASONS,
    &BIOME,
    &TERRAIN_DETAIL,
    &RIVER_CARVE,
    &SCATTER,
    &GEOLOGY,
];

/// Every registered built-in node kind.
pub fn registry() -> &'static [&'static NodeKind] {
    &REGISTRY
}

pub fn find_kind(id: &str) -> Option<&'static NodeKind> {
    registry().iter().copied().find(|kind| kind.id == id)
}

/// A starting graph and what it is for (File → New from template).
pub struct GraphTemplate {
    pub id: &'static str,
    pub label: &'static str,
    pub description: &'static str,
    pub build: fn() -> GraphDoc,
}

/// Starting graphs: the original scattered-land world and a few continent
/// layouts (each ends in an edge falloff, so the map is ocean all round).
pub static TEMPLATES: [GraphTemplate; 7] = [
    GraphTemplate {
        id: "default",
        label: "Default (scattered land)",
        description: "Many small plates, land scattered everywhere. The original world.",
        build: default_graph,
    },
    GraphTemplate {
        id: "single_continent",
        label: "Single continent",
        description: "One large continent in the middle, ocean on every edge.",
        build: || {
            continents_graph(&[
                ("plate_size", 1100.0),
                ("continents", 1.0),
                ("continental_fraction", 0.3),
            ])
        },
    },
    GraphTemplate {
        id: "two_continents",
        label: "Two continents",
        description: "Two continents separated by ocean, ocean on every edge.",
        build: || {
            continents_graph(&[
                ("plate_size", 900.0),
                ("continents", 2.0),
                ("continental_fraction", 0.24),
            ])
        },
    },
    GraphTemplate {
        id: "archipelago",
        label: "Archipelago",
        description: "Several smaller landmasses in open ocean.",
        build: || {
            continents_graph(&[
                ("plate_size", 700.0),
                ("continents", 4.0),
                ("continental_fraction", 0.2),
            ])
        },
    },
    GraphTemplate {
        id: "pangaea",
        label: "Pangaea",
        description: "One huge landmass filling most of the map, ocean only at the edges.",
        build: || {
            continents_graph(&[
                ("plate_size", 1300.0),
                ("continents", 1.0),
                ("continental_fraction", 0.6),
            ])
        },
    },
    GraphTemplate {
        id: "eroded_continent",
        label: "Eroded continent (atlas)",
        description: "One continent shaped by tectonic uplift and stream-power erosion on \
                      rock strata, with wind and rain, lakes and rivers, seasons, biomes, \
                      carved rivers, trees and voxel materials.",
        build: eroded_graph,
    },
    GraphTemplate {
        id: "geological_epochs",
        label: "Geological epochs (atlas)",
        description: "The eroded continent, shaped over several epochs instead: plates \
                      drift, the sea rises and falls, glaciers carve in cold epochs and \
                      flooded land gathers sediment. Slower to build.",
        build: epochs_graph,
    },
];

/// Graph of a fresh session: tectonics → tectonic elevation (world nodes),
/// feeding temperature and a solid fill, with elevation exported as `height`
/// for game code. This is the original world.
pub fn default_graph() -> GraphDoc {
    terrain_graph(&[], None)
}

/// The default graph with different tectonics params and an edge falloff
/// filter behind the elevation.
fn continents_graph(tectonics: &[(&str, f64)]) -> GraphDoc {
    let falloff = [("margin", 0.08), ("falloff", 0.14), ("noise", 0.05)];
    terrain_graph(tectonics, Some(&falloff))
}

fn terrain_graph(tectonics: &[(&str, f64)], falloff: Option<&[(&str, f64)]>) -> GraphDoc {
    let node = |id: &str, kind: &str, x: f64, y: f64| NodeDoc {
        id: id.into(),
        kind: kind.into(),
        enabled: true,
        position: (x, y),
        params: Default::default(),
        script: None,
        label: None,
    };
    let with_params = |mut node: NodeDoc, params: &[(&str, f64)]| {
        node.params = params.iter().map(|(k, v)| ((*k).to_owned(), *v)).collect();
        node
    };
    let edge = |from: &str, out: &str, to: &str, input: &str| EdgeDoc {
        from: SocketRef {
            node: from.into(),
            socket: out.into(),
        },
        to: SocketRef {
            node: to.into(),
            socket: input.into(),
        },
    };

    let mut nodes = vec![
        with_params(node("n1", "tectonics", 0.0, 0.0), tectonics),
        node("n2", "tectonic_elevation", 340.0, 0.0),
    ];
    let mut edges: Vec<EdgeDoc> = [
        "boundary_distance",
        "convergence",
        "continental",
        "neighbor_continental",
    ]
    .iter()
    .map(|socket| edge("n1", socket, "n2", socket))
    .collect();

    // The node whose `elevation` the chunk nodes read.
    let mut source = "n2".to_owned();
    let mut next = 3;
    let mut x = 680.0;

    if let Some(params) = falloff {
        nodes.push(with_params(node("n3", "edge_falloff", x, 0.0), params));
        edges.push(edge("n2", "elevation", "n3", "elevation"));
        source = "n3".to_owned();
        next = 4;
        x += 340.0;
    }

    let (temperature, solid, export) = (
        format!("n{next}"),
        format!("n{}", next + 1),
        format!("n{}", next + 2),
    );
    nodes.push(node(&temperature, "temperature", x, -60.0));
    nodes.push(node(&solid, "solid", x, 120.0));
    nodes.push(NodeDoc {
        label: Some("height".into()),
        ..node(&export, "export_column", x, 300.0)
    });
    edges.push(edge(&source, "elevation", &temperature, "elevation"));
    edges.push(edge(&source, "elevation", &solid, "elevation"));
    edges.push(edge(&source, "elevation", &export, "value"));

    GraphDoc {
        format: crate::graph::FORMAT,
        seed: crate::graph::DEFAULT_SEED,
        frames: Vec::new(),
        world: Default::default(),
        extent: None,
        nodes,
        edges,
    }
}

/// The atlas world (Track A): tectonics → elevation (pulled to the sea at the
/// edges) and uplift → rasterize → strata and climate (wind → moisture) →
/// erosion on the strata with rain → flow (lakes, rivers) → seasons and
/// biomes; back at world scope a bicubic sample of the eroded elevation gets
/// biome-scaled detail and carved rivers; exports for the game: `height`,
/// `biome`, `surface_material`, `river_depth`, `tree` and the voxel
/// `material` from `strata_fill`.
///
/// Ids n1 … n12 keep the original eroded template's kinds (erosion = n6,
/// flow = n7, the `height` export = n12).
pub fn eroded_graph() -> GraphDoc {
    let node = |id: &str, kind: &str, x: f64, y: f64, params: &[(&str, f64)]| NodeDoc {
        id: id.into(),
        kind: kind.into(),
        enabled: true,
        position: (x, y),
        params: params.iter().map(|(k, v)| ((*k).to_owned(), *v)).collect(),
        script: None,
        label: None,
    };
    let named = |id: &str, kind: &str, x: f64, y: f64, label: &str| NodeDoc {
        label: Some(label.into()),
        ..node(id, kind, x, y, &[])
    };
    let edge = |from: &str, out: &str, to: &str, input: &str| EdgeDoc {
        from: SocketRef {
            node: from.into(),
            socket: out.into(),
        },
        to: SocketRef {
            node: to.into(),
            socket: input.into(),
        },
    };

    let tectonics = [
        ("plate_size", 1100.0),
        ("continents", 1.0),
        ("continental_fraction", 0.3),
    ];
    let falloff = [("margin", 0.08), ("falloff", 0.14), ("noise", 0.05)];
    let nodes = vec![
        node("n1", "tectonics", 0.0, 0.0, &tectonics),
        node("n2", "tectonic_elevation", 340.0, -80.0, &[]),
        node("n3", "tectonic_uplift", 340.0, 160.0, &[]),
        node("n4", "rasterize", 1020.0, -80.0, &[("supersample", 2.0)]),
        node("n5", "rasterize", 680.0, 160.0, &[]),
        node("n6", "erosion", 1700.0, 0.0, &[]),
        node("n7", "flow", 2040.0, 160.0, &[("river_threshold", 250.0)]),
        node("n8", "sample", 2380.0, -80.0, &[("mode", 2.0)]),
        node("n9", "edge_falloff", 680.0, -80.0, &falloff),
        node("n10", "temperature", 3400.0, -260.0, &[]),
        node("n11", "solid", 3400.0, -80.0, &[]),
        named("n12", "export_column", 3400.0, 100.0, "height"),
        node("n13", "rasterize", 680.0, 360.0, &[]),
        node("n14", "rasterize", 680.0, 480.0, &[]),
        node("n15", "rasterize", 680.0, 600.0, &[]),
        node("n16", "strata", 1360.0, 400.0, &[]),
        node("n17", "wind", 1020.0, -360.0, &[]),
        node("n18", "moisture", 1360.0, -300.0, &[]),
        node("n19", "seasons", 2380.0, -400.0, &[]),
        node("n20", "biome", 2720.0, -400.0, &[]),
        node("n21", "sample", 3060.0, -560.0, &[("mode", 0.0)]),
        named("n22", "export_column", 3400.0, -560.0, "biome"),
        node("n23", "sample", 3060.0, -400.0, &[]),
        node("n24", "terrain_detail", 2720.0, -80.0, &[]),
        node("n25", "river_carve", 3060.0, -80.0, &[]),
        named("n26", "export_column", 3400.0, 280.0, "river_depth"),
        node("n27", "sample", 3060.0, -720.0, &[]),
        node("n28", "scatter", 3400.0, -720.0, &[]),
        named("n29", "export_column", 3740.0, -720.0, "tree"),
        node("n30", "sample", 2380.0, 400.0, &[("mode", 0.0)]),
        named("n31", "export_column", 3400.0, 400.0, "surface_material"),
        node("n32", "strata_fill", 3400.0, 560.0, &[]),
        named("n33", "export_material", 3740.0, 560.0, "material"),
    ];

    let mut edges = Vec::new();
    for socket in [
        "boundary_distance",
        "convergence",
        "continental",
        "neighbor_continental",
    ] {
        edges.push(edge("n1", socket, "n2", socket));
        edges.push(edge("n1", socket, "n3", socket));
    }
    edges.extend([
        // Terrain and uplift onto the atlas (the edge falloff first, so the
        // atlas sees the ocean border).
        edge("n2", "elevation", "n9", "elevation"),
        edge("n9", "elevation", "n4", "in"),
        edge("n3", "uplift", "n5", "in"),
        edge("n1", "continental", "n13", "in"),
        edge("n1", "boundary_distance", "n14", "in"),
        edge("n1", "convergence", "n15", "in"),
        // Strata and climate.
        edge("n13", "out", "n16", "continental"),
        edge("n14", "out", "n16", "boundary_distance"),
        edge("n15", "out", "n16", "convergence"),
        edge("n4", "out", "n16", "elevation"),
        edge("n4", "out", "n18", "elevation"),
        edge("n17", "wind_u", "n18", "wind_u"),
        edge("n17", "wind_v", "n18", "wind_v"),
        // Erosion on the strata with rain, then flow.
        edge("n4", "out", "n6", "elevation"),
        edge("n5", "out", "n6", "uplift"),
        edge("n16", "strata", "n6", "strata"),
        edge("n18", "precipitation", "n6", "precipitation"),
        edge("n6", "elevation", "n7", "elevation"),
        edge("n18", "precipitation", "n7", "precipitation"),
        // Seasons and biomes on the eroded terrain.
        edge("n6", "elevation", "n19", "elevation"),
        edge("n18", "precipitation", "n19", "precipitation"),
        edge("n6", "elevation", "n20", "elevation"),
        edge("n19", "temperature", "n20", "temperature"),
        edge("n18", "precipitation", "n20", "precipitation"),
        edge("n7", "lake_depth", "n20", "lake_depth"),
        edge("n7", "discharge", "n20", "discharge"),
        // Back to world scope: smooth elevation + detail + rivers.
        edge("n6", "elevation", "n8", "in"),
        edge("n20", "roughness", "n23", "in"),
        edge("n8", "out", "n24", "elevation"),
        edge("n23", "out", "n24", "roughness"),
        edge("n24", "elevation", "n25", "elevation"),
        edge("n7", "rivers", "n25", "rivers"),
        edge("n20", "biome", "n21", "in"),
        edge("n20", "tree_density", "n27", "in"),
        edge("n27", "out", "n28", "density"),
        edge("n6", "surface_material", "n30", "in"),
        // Game exports and voxels.
        edge("n25", "elevation", "n10", "elevation"),
        edge("n25", "elevation", "n11", "elevation"),
        edge("n25", "elevation", "n12", "value"),
        edge("n21", "out", "n22", "value"),
        edge("n25", "river_depth", "n26", "value"),
        edge("n28", "points", "n29", "value"),
        edge("n30", "out", "n31", "value"),
        edge("n25", "elevation", "n32", "elevation"),
        edge("n6", "strata", "n32", "strata"),
        edge("n25", "water_level", "n32", "water_level"),
        edge("n32", "material", "n33", "value"),
    ]);

    GraphDoc {
        format: crate::graph::FORMAT,
        seed: crate::graph::DEFAULT_SEED,
        frames: Vec::new(),
        world: Default::default(),
        extent: None,
        nodes,
        edges,
    }
}

/// The atlas world with `geology` (moving plates, sea-level cycles,
/// glaciers, several erosion epochs) in place of `erosion` (n6).
pub fn epochs_graph() -> GraphDoc {
    let mut doc = eroded_graph();
    // geology computes its own uplift from moving plates.
    doc.nodes.retain(|n| n.id != "n3" && n.id != "n5");
    doc.edges
        .retain(|e| !["n3", "n5"].contains(&e.from.node.as_str()) && e.to.node != "n3");
    let geology = doc.node_mut("n6").expect("n6 is erosion");
    geology.kind = "geology".into();
    geology.params = [
        ("plate_size", 1100.0),
        ("continents", 1.0),
        ("continental_fraction", 0.3),
    ]
    .iter()
    .map(|(k, v)| ((*k).to_owned(), *v))
    .collect();
    doc
}

const fn input(name: &'static str) -> InputSpec {
    InputSpec {
        name: Cow::Borrowed(name),
        scope: FieldScope::Column,
        kind: ValueKind::Scalar,
        optional: false,
    }
}

const fn column(
    name: &'static str,
    unit: &'static str,
    min: f32,
    max: f32,
    ramp: &'static str,
) -> FieldSpec {
    FieldSpec {
        name: Cow::Borrowed(name),
        scope: FieldScope::Column,
        kind: ValueKind::Scalar,
        unit: Cow::Borrowed(unit),
        min,
        max,
        ramp: Cow::Borrowed(ramp),
        display_from: None,
    }
}

/// Reads a column slot anywhere in the declared neighborhood (possibly a
/// neighboring chunk's prefix data). `None` outside the world.
fn neighbor_column(
    chunks: &NeighborChunks<'_, ColumnData>,
    context: &StepContext,
    x: i64,
    y: i64,
    slot: usize,
) -> Result<Option<f32>, String> {
    let position = (x, y, context.origin().2);
    let Some(chunk) = chunks.get_global(position)? else {
        return Ok(None);
    };
    let (_, local) = context
        .config()
        .locate(position)
        .ok_or_else(|| format!("Column ({x}, {y}) has no chunk"))?;

    Ok(Some(chunk.columns[(local.0, local.1, 0)][slot]))
}

// ---------------------------------------------------------------------------

pub static SLOPE: NodeKind = NodeKind {
    id: "slope",
    label: "Slope",
    description: "Elevation gradient from neighboring columns, across chunk borders.",
    inputs: &[input("elevation")],
    outputs: &[column("slope", "cells/cell", 0.0, 2.0, "grayscale")],
    params: &[],
    build: Build::Neighborhood {
        // Column data is identical vertically: X/Y neighbors only.
        radius: |_| (1, 1, 0),
        build: |bindings, _| {
            Ok(Box::new(SlopeStep {
                elevation: bindings.input("elevation")?,
                out: bindings.output("slope")?,
            }) as BoxedNeighborhoodStep)
        },
    },
};

struct SlopeStep {
    elevation: usize,
    out: usize,
}

impl NeighborhoodChunkStep<Voxel, ColumnData, WorldStore> for SlopeStep {
    fn run(
        &self,
        _input: &NeighborhoodView<'_, Voxel>,
        chunks: &NeighborChunks<'_, ColumnData>,
        _output: &mut DataSet<Voxel>,
        output_chunk: &mut ColumnData,
        context: &StepContext,
        _world: &WorldStore,
    ) -> PipelineStepResult {
        let origin = context.origin();

        for (column, x, y, _) in output_chunk.columns.iter_mut() {
            let (gx, gy) = (origin.0 + x as i64, origin.1 + y as i64);
            let here = column[self.elevation];
            // World edges: treat the missing neighbor as flat.
            let at = |dx: i64, dy: i64| {
                neighbor_column(chunks, context, gx + dx, gy + dy, self.elevation)
                    .map(|e| e.unwrap_or(here))
            };

            let dx = (at(1, 0)? - at(-1, 0)?) / 2.0;
            let dy = (at(0, 1)? - at(0, -1)?) / 2.0;
            column[self.out] = (dx * dx + dy * dy).sqrt();
        }

        Ok(())
    }
}

// ---------------------------------------------------------------------------

pub static SMOOTH: NodeKind = NodeKind {
    id: "smooth",
    label: "Smooth",
    description: "Box blur of any column value across chunk borders (re-processing).",
    inputs: &[input("value")],
    outputs: &[FieldSpec {
        name: Cow::Borrowed("smoothed"),
        scope: FieldScope::Column,
        kind: ValueKind::Scalar,
        unit: Cow::Borrowed(""),
        min: 0.0,
        max: 1.0,
        ramp: Cow::Borrowed("grayscale"),
        // Smoothing keeps units: show it like whatever is wired in.
        display_from: Some(Cow::Borrowed("value")),
    }],
    params: &[ParamSpec {
        name: "radius",
        default: 2.0,
        min: 1.0,
        max: 8.0,
    }],
    build: Build::Neighborhood {
        radius: |params| {
            let r = params.get("radius").round() as u32;
            (r, r, 0)
        },
        build: |bindings, params| {
            Ok(Box::new(SmoothStep {
                value: bindings.input("value")?,
                out: bindings.output("smoothed")?,
                radius: params.get("radius").round() as i64,
            }) as BoxedNeighborhoodStep)
        },
    },
};

struct SmoothStep {
    value: usize,
    out: usize,
    radius: i64,
}

impl NeighborhoodChunkStep<Voxel, ColumnData, WorldStore> for SmoothStep {
    fn run(
        &self,
        _input: &NeighborhoodView<'_, Voxel>,
        chunks: &NeighborChunks<'_, ColumnData>,
        _output: &mut DataSet<Voxel>,
        output_chunk: &mut ColumnData,
        context: &StepContext,
        _world: &WorldStore,
    ) -> PipelineStepResult {
        let origin = context.origin();

        for (column, x, y, _) in output_chunk.columns.iter_mut() {
            let (gx, gy) = (origin.0 + x as i64, origin.1 + y as i64);
            let (mut total, mut count) = (0.0f64, 0u32);

            for dy in -self.radius..=self.radius {
                for dx in -self.radius..=self.radius {
                    // Outside-world samples are skipped, not zero-padded.
                    if let Some(v) = neighbor_column(chunks, context, gx + dx, gy + dy, self.value)?
                    {
                        total += f64::from(v);
                        count += 1;
                    }
                }
            }

            column[self.out] = (total / f64::from(count.max(1))) as f32;
        }

        Ok(())
    }
}

// ---------------------------------------------------------------------------

pub static SOLID: NodeKind = NodeKind {
    id: "solid",
    label: "Solid fill",
    description: "Fills voxels below the column elevation.",
    inputs: &[input("elevation")],
    outputs: &[FieldSpec {
        name: Cow::Borrowed("solid"),
        scope: FieldScope::Voxel,
        kind: ValueKind::Scalar,
        unit: Cow::Borrowed(""),
        min: 0.0,
        max: 1.0,
        ramp: Cow::Borrowed("grayscale"),
        display_from: None,
    }],
    params: &[],
    build: Build::Chunk(|bindings: &Bindings, _: &Params| {
        Ok(Box::new(SolidStep {
            elevation: bindings.input("elevation")?,
            out: bindings.output("solid")?,
        }) as BoxedStep)
    }),
};

struct SolidStep {
    elevation: usize,
    out: usize,
}

impl ChunkStep<Voxel, ColumnData, WorldStore> for SolidStep {
    fn run(
        &self,
        cells: &mut DataSet<Voxel>,
        chunk: &mut ColumnData,
        context: &StepContext,
        _world: &WorldStore,
    ) -> PipelineStepResult {
        let origin_z = context.origin().2;

        for (voxel, x, y, z) in cells.iter_mut() {
            let elevation = chunk.columns[(x, y, 0)][self.elevation];
            let solid = ((origin_z + z as i64) as f32) < elevation;

            voxel.slots[self.out] = if solid { 1.0 } else { 0.0 };
        }

        Ok(())
    }
}

// ---------------------------------------------------------------------------

/// Named column output for game code. The node's label is the export name.
pub static EXPORT_COLUMN: NodeKind = NodeKind {
    id: "export_column",
    label: "Export (column)",
    description: "Publishes a column value under this node's name for game code.",
    inputs: &[input("value")],
    outputs: &[],
    params: &[],
    build: Build::Export,
};

/// Named voxel output for game code. The node's label is the export name.
pub static EXPORT_VOXEL: NodeKind = NodeKind {
    id: "export_voxel",
    label: "Export (voxel)",
    description: "Publishes a voxel value under this node's name for game code.",
    inputs: &[InputSpec {
        name: Cow::Borrowed("value"),
        scope: FieldScope::Voxel,
        kind: ValueKind::Scalar,
        optional: false,
    }],
    outputs: &[],
    params: &[],
    build: Build::Export,
};

/// Wiring helper for column values: passes its input through unchanged. No
/// runtime work and no slot of its own (the output aliases the input's slot).
pub static REROUTE_COLUMN: NodeKind = NodeKind {
    id: "reroute_column",
    label: "Reroute (column)",
    description: "Passes a column value through unchanged; a wire waypoint.",
    inputs: &[input("in")],
    outputs: &[FieldSpec {
        name: Cow::Borrowed("out"),
        scope: FieldScope::Column,
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

/// Wiring helper for voxel values; see `REROUTE_COLUMN`.
pub static REROUTE_VOXEL: NodeKind = NodeKind {
    id: "reroute_voxel",
    label: "Reroute (voxel)",
    description: "Passes a voxel value through unchanged; a wire waypoint.",
    inputs: &[InputSpec {
        name: Cow::Borrowed("in"),
        scope: FieldScope::Voxel,
        kind: ValueKind::Scalar,
        optional: false,
    }],
    outputs: &[FieldSpec {
        name: Cow::Borrowed("out"),
        scope: FieldScope::Voxel,
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

// ---------------------------------------------------------------------------
// World-scope nodes: built once per world, evaluated per sample point.

const fn world_field(
    name: &'static str,
    unit: &'static str,
    min: f32,
    max: f32,
    ramp: &'static str,
) -> FieldSpec {
    FieldSpec {
        name: Cow::Borrowed(name),
        scope: FieldScope::World,
        kind: ValueKind::Scalar,
        unit: Cow::Borrowed(unit),
        min,
        max,
        ramp: Cow::Borrowed(ramp),
        display_from: None,
    }
}

const fn world_input(name: &'static str) -> InputSpec {
    InputSpec {
        name: Cow::Borrowed(name),
        scope: FieldScope::World,
        kind: ValueKind::Scalar,
        optional: false,
    }
}

/// Seeded fractal noise over the world.
pub static WORLD_NOISE: NodeKind = NodeKind {
    id: "world_noise",
    label: "Noise (world)",
    description: "Seeded fractal noise in 0 … 1, sampled over the whole world.",
    inputs: &[],
    outputs: &[world_field("noise", "", 0.0, 1.0, "grayscale")],
    params: &[
        ParamSpec {
            name: "wavelength",
            default: 512.0,
            min: 16.0,
            max: 8192.0,
        },
        ParamSpec {
            name: "octaves",
            default: 3.0,
            min: 1.0,
            max: 6.0,
        },
        ParamSpec {
            name: "variant",
            default: 0.0,
            min: 0.0,
            max: 1000.0,
        },
    ],
    build: Build::World(|bindings, params, context| {
        Ok(Box::new(NoiseWorldStep {
            out: bindings.output("noise")?,
            seed: context.seed
                ^ 0x6E01_0000
                ^ (params.get("variant") as u64).wrapping_mul(0x9E37_79B9),
            wavelength: params.get("wavelength"),
            octaves: params.get("octaves").round() as u32,
        }) as BoxedWorldStep)
    }),
};

struct NoiseWorldStep {
    out: usize,
    seed: u64,
    wavelength: f64,
    octaves: u32,
}

impl WorldStep for NoiseWorldStep {
    fn run(&self, x: i64, y: i64, slots: &mut [f64; WORLD_SLOTS]) {
        slots[self.out] = f64::from(crate::world::fbm(
            self.seed,
            x as f64,
            y as f64,
            self.wavelength,
            self.octaves,
        ));
    }
}

/// Linear remap of a world value, optionally clamped.
pub static WORLD_REMAP: NodeKind = NodeKind {
    id: "world_remap",
    label: "Remap (world)",
    description: "Maps a world value from one range to another (optionally clamped).",
    inputs: &[world_input("value")],
    outputs: &[world_field("remapped", "", 0.0, 1.0, "grayscale")],
    params: &[
        ParamSpec {
            name: "in_min",
            default: 0.0,
            min: -10000.0,
            max: 10000.0,
        },
        ParamSpec {
            name: "in_max",
            default: 1.0,
            min: -10000.0,
            max: 10000.0,
        },
        ParamSpec {
            name: "out_min",
            default: 0.0,
            min: -10000.0,
            max: 10000.0,
        },
        ParamSpec {
            name: "out_max",
            default: 1.0,
            min: -10000.0,
            max: 10000.0,
        },
        ParamSpec {
            name: "clamp",
            default: 1.0,
            min: 0.0,
            max: 1.0,
        },
    ],
    build: Build::World(|bindings, params, _| {
        Ok(Box::new(RemapWorldStep {
            input: bindings.input("value")?,
            out: bindings.output("remapped")?,
            in_min: params.get("in_min"),
            in_max: params.get("in_max"),
            out_min: params.get("out_min"),
            out_max: params.get("out_max"),
            clamp: params.get("clamp") >= 0.5,
        }) as BoxedWorldStep)
    }),
};

struct RemapWorldStep {
    input: usize,
    out: usize,
    in_min: f64,
    in_max: f64,
    out_min: f64,
    out_max: f64,
    clamp: bool,
}

impl WorldStep for RemapWorldStep {
    fn run(&self, _x: i64, _y: i64, slots: &mut [f64; WORLD_SLOTS]) {
        let span = self.in_max - self.in_min;
        let mut t = if span.abs() < 1e-12 {
            0.0
        } else {
            (slots[self.input] - self.in_min) / span
        };
        if self.clamp {
            t = t.clamp(0.0, 1.0);
        }
        slots[self.out] = self.out_min + (self.out_max - self.out_min) * t;
    }
}

/// Wiring helper for world values; see `REROUTE_COLUMN`.
pub static REROUTE_WORLD: NodeKind = NodeKind {
    id: "reroute_world",
    label: "Reroute (world)",
    description: "Passes a world value through unchanged; a wire waypoint.",
    inputs: &[world_input("in")],
    outputs: &[world_field("out", "", 0.0, 1.0, "grayscale")],
    params: &[],
    build: Build::Alias,
};
