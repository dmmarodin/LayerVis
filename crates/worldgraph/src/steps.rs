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
    graph::{EdgeDoc, GraphDoc, NodeDoc, SocketRef},
    pipeline::{
        Bindings, BoxedNeighborhoodStep, BoxedStep, BoxedWorldStep, Build, FieldSpec, InputSpec,
        NodeKind, ParamSpec, Params, WorldStep,
    },
    climate::{HUMIDITY, TEMPERATURE},
    terrain::{EDGE_FALLOFF, TECTONICS, TECTONIC_ELEVATION},
    world::WorldStore,
    ColumnData, FieldScope, Voxel, WORLD_SLOTS,
};

static REGISTRY: [&NodeKind; 15] = [
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
pub static TEMPLATES: [GraphTemplate; 5] = [
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
    let falloff = [
        ("margin", 0.08),
        ("falloff", 0.14),
        ("noise", 0.05),
    ];
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
        nodes,
        edges,
    }
}

const fn input(name: &'static str) -> InputSpec {
    InputSpec {
        name: Cow::Borrowed(name),
        scope: FieldScope::Column,
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
                    if let Some(v) = neighbor_column(chunks, context, gx + dx, gy + dy, self.value)? {
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
    }],
    outputs: &[FieldSpec {
        name: Cow::Borrowed("out"),
        scope: FieldScope::Voxel,
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
            seed: context.seed ^ 0x6E01_0000 ^ (params.get("variant") as u64).wrapping_mul(0x9E37_79B9),
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
