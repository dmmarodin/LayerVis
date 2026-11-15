//! World-scope climate nodes: temperature and humidity.
//!
//! Both are pure functions of the world position and the terrain elevation,
//! so they run at world scope: they have a zoomed-out view like the terrain,
//! and chunk nodes read them through implicit sampling.

use std::borrow::Cow;

use crate::{
    pipeline::{
        BoxedWorldStep, Build, FieldSpec, InputSpec, NodeKind, ParamSpec, WorldBuildContext,
        WorldStep,
    },
    world::fbm,
    FieldScope, WORLD_SLOTS,
};

const fn world_out(
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

const fn world_in(name: &'static str) -> InputSpec {
    InputSpec {
        name: Cow::Borrowed(name),
        scope: FieldScope::World,
    }
}

/// Signed latitude in [-1, 1] across the world's Y extent.
fn latitude(context: &WorldBuildContext, y: i64) -> f64 {
    let t = (y - context.origin.1) as f64 / f64::from(context.size.1);
    t * 2.0 - 1.0
}

// ---------------------------------------------------------------------------

/// Sea level as a fraction of the world depth (matches the terrain ramp's
/// water band and the oceanic plate height).
const SEA_LEVEL: f64 = 0.3;

pub static TEMPERATURE: NodeKind = NodeKind {
    id: "temperature",
    label: "Temperature",
    description: "Warm equator, cold poles, colder with altitude.",
    inputs: &[world_in("elevation")],
    outputs: &[world_out("temperature", "°C", -30.0, 40.0, "thermal")],
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
    build: Build::World(|bindings, params, context| {
        Ok(Box::new(TemperatureStep {
            elevation: bindings.input("elevation")?,
            out: bindings.output("temperature")?,
            equator: params.get("equator"),
            pole_drop: params.get("pole_drop"),
            lapse: params.get("lapse"),
            context: *context,
        }) as BoxedWorldStep)
    }),
};

struct TemperatureStep {
    elevation: usize,
    out: usize,
    equator: f64,
    pole_drop: f64,
    lapse: f64,
    context: WorldBuildContext,
}

impl WorldStep for TemperatureStep {
    fn run(&self, _x: i64, y: i64, slots: &mut [f64; WORLD_SLOTS]) {
        let latitude = latitude(&self.context, y).abs();

        slots[self.out] =
            self.equator - self.pole_drop * latitude - self.lapse * slots[self.elevation];
    }
}

// ---------------------------------------------------------------------------

pub static HUMIDITY: NodeKind = NodeKind {
    id: "humidity",
    label: "Humidity",
    description: "Wet oceans and lowlands, drier with altitude and heat.",
    inputs: &[world_in("elevation"), world_in("temperature")],
    outputs: &[world_out("humidity", "0–1", 0.0, 1.0, "thermal")],
    params: &[],
    build: Build::World(|bindings, _, context| {
        Ok(Box::new(HumidityStep {
            elevation: bindings.input("elevation")?,
            temperature: bindings.input("temperature")?,
            out: bindings.output("humidity")?,
            seed: context.seed ^ 0x3017,
            depth: f64::from(context.depth),
        }) as BoxedWorldStep)
    }),
};

struct HumidityStep {
    elevation: usize,
    temperature: usize,
    out: usize,
    seed: u64,
    depth: f64,
}

impl WorldStep for HumidityStep {
    fn run(&self, x: i64, y: i64, slots: &mut [f64; WORLD_SLOTS]) {
        let sea_level = self.depth * SEA_LEVEL;
        let elevation = slots[self.elevation];

        slots[self.out] = if elevation < sea_level {
            1.0
        } else {
            let altitude = (elevation - sea_level) / (self.depth - sea_level);
            let noise = f64::from(fbm(self.seed, x as f64, y as f64, 384.0, 3));
            let heat = (slots[self.temperature] - 22.0).max(0.0);

            (0.75 - 0.6 * altitude + (noise - 0.5) * 0.4 - 0.015 * heat).clamp(0.0, 1.0)
        };
    }
}

#[cfg(test)]
mod tests {
    use layergen::{ChunkSize, PipelineConfig, WorldBounds};

    use crate::{
        pipeline::{analyze, resolve, ResolvedNode},
        steps::default_graph,
        world::WorldStore,
    };

    #[test]
    fn climate_follows_latitude_altitude_and_sea_level() {
        let config = PipelineConfig::new(
            WorldBounds::new((-4096, -4096, 0), (8192, 8192, 128)).unwrap(),
            ChunkSize::new(32, 32, 32).unwrap(),
            21,
        );
        let doc = default_graph();
        let world = WorldStore::for_doc(&doc, &config).unwrap();

        let resolved: Vec<ResolvedNode> = doc.nodes.iter().map(resolve).collect();
        let schema = analyze(&doc, &resolved).schema;
        let (elevation, temperature, humidity) = (
            schema.slot("n2.elevation").unwrap(),
            schema.slot("n3.temperature").unwrap(),
            schema.slot("n4.humidity"),
        );
        // The default graph has no humidity node: climate nodes are optional.
        assert!(humidity.is_err());

        let (mut warm, mut cold) = (f64::MIN, f64::MAX);
        for i in 0..200i64 {
            let (x, y) = (-4096 + (i * 2791) % 8192, -4096 + (i * 6113) % 8192);
            let values = world.graph().eval(x, y);

            // temperature = equator - pole_drop * |latitude| - lapse * elevation
            let latitude = ((y + 4096) as f64 / 8192.0 * 2.0 - 1.0).abs();
            assert_eq!(
                values[temperature],
                32.0 - 50.0 * latitude - 0.25 * values[elevation]
            );

            if latitude < 0.1 {
                warm = warm.max(values[temperature]);
            }
            if latitude > 0.9 {
                cold = cold.min(values[temperature]);
            }
        }
        assert!(warm > cold, "equator {warm} vs pole {cold}");
    }

    #[test]
    fn humidity_is_wet_at_sea_and_dries_with_altitude_and_heat() {
        let config = PipelineConfig::new(
            WorldBounds::new((-4096, -4096, 0), (8192, 8192, 128)).unwrap(),
            ChunkSize::new(32, 32, 32).unwrap(),
            21,
        );
        // default graph + a humidity node wired to elevation and temperature
        let mut doc = default_graph();
        doc.nodes.push(crate::graph::NodeDoc {
            id: "n6".into(),
            kind: "humidity".into(),
            enabled: true,
            position: (0.0, 0.0),
            params: Default::default(),
            script: None,
            label: None,
        });
        for (from, out, input) in [
            ("n2", "elevation", "elevation"),
            ("n3", "temperature", "temperature"),
        ] {
            doc.edges.push(crate::graph::EdgeDoc {
                from: crate::graph::SocketRef {
                    node: from.into(),
                    socket: out.into(),
                },
                to: crate::graph::SocketRef {
                    node: "n6".into(),
                    socket: input.into(),
                },
            });
        }
        let world = WorldStore::for_doc(&doc, &config).unwrap();
        let resolved: Vec<ResolvedNode> = doc.nodes.iter().map(resolve).collect();
        let schema = analyze(&doc, &resolved).schema;
        let (elevation, humidity) = (
            schema.slot("n2.elevation").unwrap(),
            schema.slot("n6.humidity").unwrap(),
        );

        let (mut ocean, mut land) = (0, 0);
        for i in 0..400i64 {
            let (x, y) = (-4096 + (i * 2791) % 8192, -4096 + (i * 6113) % 8192);
            let values = world.graph().eval(x, y);
            assert!((0.0..=1.0).contains(&values[humidity]));

            if values[elevation] < 0.3 * 128.0 {
                assert_eq!(values[humidity], 1.0, "ocean is saturated");
                ocean += 1;
            } else {
                land += 1;
            }
        }
        assert!(ocean > 0 && land > 0);
    }
}
