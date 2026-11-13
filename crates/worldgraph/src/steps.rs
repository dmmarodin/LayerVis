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
        Bindings, BoxedNeighborhoodStep, BoxedStep, Build, FieldSpec, InputSpec, NodeKind,
        ParamSpec, Params,
    },
    world::WorldStore,
    ColumnData, FieldScope, Voxel,
};

static REGISTRY: [&NodeKind; 8] = [
    &ELEVATION,
    &TEMPERATURE,
    &HUMIDITY,
    &SLOPE,
    &SMOOTH,
    &SOLID,
    &EXPORT_COLUMN,
    &EXPORT_VOXEL,
];

/// Every registered built-in node kind.
pub fn registry() -> &'static [&'static NodeKind] {
    &REGISTRY
}

pub fn find_kind(id: &str) -> Option<&'static NodeKind> {
    registry().iter().copied().find(|kind| kind.id == id)
}

/// Graph of a fresh session: elevation → temperature, elevation → solid,
/// with elevation exported as `height` for game code.
pub fn default_graph() -> GraphDoc {
    let node = |id: &str, kind: &str, x: f64, y: f64| NodeDoc {
        id: id.into(),
        kind: kind.into(),
        enabled: true,
        position: (x, y),
        params: Default::default(),
        script: None,
        label: None,
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

    GraphDoc {
        format: crate::graph::FORMAT,
        nodes: vec![
            node("n1", "elevation", 0.0, 0.0),
            node("n2", "temperature", 320.0, -60.0),
            node("n3", "solid", 320.0, 120.0),
            NodeDoc {
                label: Some("height".into()),
                ..node("n4", "export_column", 320.0, 300.0)
            },
        ],
        edges: vec![
            edge("n1", "elevation", "n2", "elevation"),
            edge("n1", "elevation", "n3", "elevation"),
            edge("n1", "elevation", "n4", "value"),
        ],
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

pub static ELEVATION: NodeKind = NodeKind {
    id: "elevation",
    label: "Elevation",
    description: "Samples tectonic elevation once per column.",
    inputs: &[],
    outputs: &[column("elevation", "cells", 0.0, 128.0, "terrain")],
    params: &[],
    world_reads: &["tectonics"],
    build: Build::Chunk(|bindings, _| {
        Ok(Box::new(ElevationStep {
            out: bindings.output("elevation")?,
        }) as BoxedStep)
    }),
};

struct ElevationStep {
    out: usize,
}

impl ChunkStep<Voxel, ColumnData, WorldStore> for ElevationStep {
    fn run(
        &self,
        _cells: &mut DataSet<Voxel>,
        chunk: &mut ColumnData,
        context: &StepContext,
        world: &WorldStore,
    ) -> PipelineStepResult {
        let origin = context.origin();

        for (column, x, y, _) in chunk.columns.iter_mut() {
            column[self.out] = world.elevation_at(origin.0 + x as i64, origin.1 + y as i64);
        }

        Ok(())
    }
}

// ---------------------------------------------------------------------------

pub static TEMPERATURE: NodeKind = NodeKind {
    id: "temperature",
    label: "Temperature",
    description: "Warm equator, cold poles, colder with altitude.",
    inputs: &[input("elevation")],
    outputs: &[column("temperature", "°C", -30.0, 40.0, "thermal")],
    params: &[
        ParamSpec {
            name: "equator",
            default: 32.0,
            min: -20.0,
            max: 60.0,
        },
        ParamSpec {
            name: "pole_drop",
            default: 50.0,
            min: 0.0,
            max: 120.0,
        },
        ParamSpec {
            name: "lapse",
            default: 0.25,
            min: 0.0,
            max: 2.0,
        },
    ],
    world_reads: &[],
    build: Build::Chunk(|bindings, params| {
        Ok(Box::new(TemperatureStep {
            elevation: bindings.input("elevation")?,
            out: bindings.output("temperature")?,
            equator: params.get("equator") as f32,
            pole_drop: params.get("pole_drop") as f32,
            lapse: params.get("lapse") as f32,
        }) as BoxedStep)
    }),
};

struct TemperatureStep {
    elevation: usize,
    out: usize,
    equator: f32,
    pole_drop: f32,
    lapse: f32,
}

impl ChunkStep<Voxel, ColumnData, WorldStore> for TemperatureStep {
    fn run(
        &self,
        _cells: &mut DataSet<Voxel>,
        chunk: &mut ColumnData,
        context: &StepContext,
        world: &WorldStore,
    ) -> PipelineStepResult {
        let origin = context.origin();

        for (column, _, y, _) in chunk.columns.iter_mut() {
            let latitude = world.latitude_at(origin.1 + y as i64).abs();
            column[self.out] =
                self.equator - self.pole_drop * latitude - self.lapse * column[self.elevation];
        }

        Ok(())
    }
}

// ---------------------------------------------------------------------------

pub static HUMIDITY: NodeKind = NodeKind {
    id: "humidity",
    label: "Humidity",
    description: "Wet oceans and lowlands, drier with altitude and heat.",
    inputs: &[input("elevation"), input("temperature")],
    outputs: &[column("humidity", "0–1", 0.0, 1.0, "thermal")],
    params: &[],
    world_reads: &[],
    build: Build::Chunk(|bindings, _| {
        Ok(Box::new(HumidityStep {
            elevation: bindings.input("elevation")?,
            temperature: bindings.input("temperature")?,
            out: bindings.output("humidity")?,
        }) as BoxedStep)
    }),
};

struct HumidityStep {
    elevation: usize,
    temperature: usize,
    out: usize,
}

impl ChunkStep<Voxel, ColumnData, WorldStore> for HumidityStep {
    fn run(
        &self,
        _cells: &mut DataSet<Voxel>,
        chunk: &mut ColumnData,
        context: &StepContext,
        world: &WorldStore,
    ) -> PipelineStepResult {
        let origin = context.origin();
        let sea_level = world.sea_level();
        let depth = world.depth();

        for (column, x, y, _) in chunk.columns.iter_mut() {
            let elevation = column[self.elevation];

            column[self.out] = if elevation < sea_level {
                1.0
            } else {
                let altitude = (elevation - sea_level) / (depth - sea_level);
                let noise = world.moisture_noise(origin.0 + x as i64, origin.1 + y as i64);
                let heat = (column[self.temperature] - 22.0).max(0.0);

                (0.75 - 0.6 * altitude + (noise - 0.5) * 0.4 - 0.015 * heat).clamp(0.0, 1.0)
            };
        }

        Ok(())
    }
}

// ---------------------------------------------------------------------------

pub static SLOPE: NodeKind = NodeKind {
    id: "slope",
    label: "Slope",
    description: "Elevation gradient from neighboring columns, across chunk borders.",
    inputs: &[input("elevation")],
    outputs: &[column("slope", "cells/cell", 0.0, 2.0, "grayscale")],
    params: &[],
    world_reads: &[],
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
    world_reads: &[],
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
    world_reads: &[],
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
    world_reads: &[],
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
    world_reads: &[],
    build: Build::Export,
};
