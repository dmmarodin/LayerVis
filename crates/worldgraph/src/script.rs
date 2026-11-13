//! Rhai script nodes.
//!
//! A script declares its sockets in a header, then defines `fn column()`,
//! called once per column with `this` bound to a map holding `x`, `y`
//! (global) and the values wired into its inputs. It must assign every
//! output. Only inputs are provided and only outputs are read back, so the
//! header is enforced.
//!
//! ```text
//! //! name: Moisture
//! //! description: optional text
//! //! input: elevation, temperature
//! //! output: moisture 0..1 thermal
//! fn column() {
//!     this.moisture = clamp(1.0 - this.elevation / depth(), 0.0, 1.0);
//! }
//! ```
//!
//! `input:` takes a comma list (all column sockets for now). `output:` may
//! repeat; each entry is `name [min..max] [ramp] [unit]`. `reads:` and
//! `writes:` are accepted as older aliases.

use std::{borrow::Cow, sync::Arc};

use layergen::{ChunkStep, DataSet, PipelineStepResult, StepContext};
use rhai::{CallFnOptions, Dynamic, Engine, Map, Scope, AST, FLOAT, INT};

use crate::{
    pipeline::{Bindings, Contract, FieldSpec, InputSpec},
    world::WorldStore,
    ColumnData, FieldScope, Voxel,
};

/// Ramps the frontend knows; anything else in an `output:` entry is the unit.
const RAMPS: &[&str] = &["grayscale", "terrain", "thermal", "diverging", "categorical"];

/// Guards against runaway scripts (per column call).
const MAX_OPERATIONS: u64 = 200_000;

pub const TEMPLATE: &str = r#"//! name: Moisture
//! description: Wet near the sea and plate boundaries, drier when high or hot.
//! input: elevation, boundary_distance, temperature
//! output: moisture 0..1 thermal
//
// Runs once per column. `this` holds x, y (global cells) and the values
// wired into each input; assign every output. Use float literals (0.0).
// New nodes auto-connect inputs to existing outputs with the same name;
// rewire freely in the graph. World values (elevation, plate,
// boundary_distance, convergence, ...) arrive through inputs: wire a world
// node's output to an input and it is sampled at every column.
//
// World:   latitude(y), sea_level(), depth(), noise(x, y, wavelength)
// Math:    clamp(v, lo, hi), lerp(a, b, t), smoothstep(e0, e1, x), min, max

fn column() {
    if this.elevation < sea_level() {
        this.moisture = 1.0;
        return;
    }

    let altitude = (this.elevation - sea_level()) / (depth() - sea_level());
    let near_boundary = 1.0 - smoothstep(0.0, 300.0, this.boundary_distance);
    let heat = max(this.temperature - 20.0, 0.0);

    this.moisture = clamp(
        0.7 - 0.5 * altitude + 0.2 * near_boundary - 0.02 * heat
            + 0.3 * (noise(this.x, this.y, 256.0) - 0.5),
        0.0,
        1.0
    );
}
"#;

/// Minimal valid starting point: rename, then edit reads/writes freely.
pub const BLANK: &str = r#"//! name: My step
//! description: What this step computes.
//! input: elevation
//! output: my_field 0..1 grayscale
//
// Header (lines starting with //!):
//   name:    node title (any text; several nodes may share it)
//   input:   comma-separated input sockets (optional). Wire them in the
//            graph; names only matter for auto-connect.
//   output:  one output socket per line: name [min..max] [ramp] [unit]
//            ramps: grayscale, terrain, thermal, diverging, categorical
//
// column() runs once per column. `this` holds x, y and every input;
// assign every output.

fn column() {
    this.my_field = clamp(this.elevation / depth(), 0.0, 1.0);
}
"#;

/// A parsed, compiled-once script and its declared contract.
#[derive(Debug)]
pub struct ScriptDef {
    pub source: String,
    pub contract: Contract,
}

impl ScriptDef {
    /// Parses the header and checks that the script compiles and defines
    /// `fn column()`.
    pub fn parse(source: &str) -> Result<Self, String> {
        let mut name = None;
        let mut description = String::new();
        let mut reads: Vec<String> = Vec::new();
        let mut writes = Vec::new();

        for (number, line) in source.lines().enumerate() {
            let Some(header) = line.trim_start().strip_prefix("//!") else {
                continue;
            };
            let Some((key, value)) = header.split_once(':') else {
                continue;
            };
            let value = value.trim();
            let at = |message: String| format!("line {}: {message}", number + 1);

            match key.trim() {
                "name" => name = Some(value.to_owned()),
                "description" => description = value.to_owned(),
                "input" | "inputs" | "reads" => reads.extend(
                    value
                        .split(',')
                        .map(str::trim)
                        .filter(|r| !r.is_empty())
                        .map(str::to_owned),
                ),
                "output" | "outputs" | "writes" => writes.push(parse_write(value).map_err(at)?),
                other => return Err(at(format!("unknown header key `{other}`"))),
            }
        }

        let name = name.ok_or("missing `//! name: ...` header")?;
        if writes.is_empty() {
            return Err("declare at least one `//! output: ...` socket".to_owned());
        }

        let mut seen = std::collections::HashSet::new();
        for socket in reads.iter().map(String::as_str).chain(writes.iter().map(|w: &FieldSpec| w.name.as_ref())) {
            if !seen.insert(socket.to_owned()) {
                return Err(format!("socket `{socket}` is declared twice"));
            }
        }

        let names = reads
            .iter()
            .map(String::as_str)
            .chain(writes.iter().map(|w: &FieldSpec| w.name.as_ref()));

        for field in names {
            if !is_identifier(field) {
                return Err(format!("`{field}` is not a valid socket name"));
            }
        }

        let ast = Engine::new()
            .compile(source)
            .map_err(|error| format!("compile error: {error}"))?;

        if !ast
            .iter_functions()
            .any(|f| f.name == "column" && f.params.is_empty())
        {
            return Err("script must define `fn column()` (no parameters)".to_owned());
        }

        Ok(Self {
            source: source.to_owned(),
            contract: Contract {
                label: name,
                description,
                inputs: reads
                    .into_iter()
                    .map(|name| InputSpec {
                        name: Cow::Owned(name),
                        scope: FieldScope::Column,
                    })
                    .collect(),
                outputs: writes,
                params: Vec::new(),
                // Script nodes are per-column only for now.
                neighborhood: None,
                export: false,
                alias: false,
            },
        })
    }
}

fn parse_write(value: &str) -> Result<FieldSpec, String> {
    let mut tokens = value.split_whitespace();
    let name = tokens.next().ok_or("`output:` needs a socket name")?;
    let (mut min, mut max) = (0.0, 1.0);
    let mut ramp = "grayscale".to_owned();
    let mut unit = Vec::new();

    for token in tokens {
        if let Some((lo, hi)) = token.split_once("..") {
            min = lo.parse().map_err(|_| format!("bad range `{token}`"))?;
            max = hi.parse().map_err(|_| format!("bad range `{token}`"))?;
        } else if RAMPS.contains(&token) {
            ramp = token.to_owned();
        } else {
            unit.push(token);
        }
    }

    Ok(FieldSpec {
        name: Cow::Owned(name.to_owned()),
        scope: FieldScope::Column,
        unit: Cow::Owned(unit.join(" ")),
        min,
        max,
        ramp: Cow::Owned(ramp),
        display_from: None,
    })
}

fn is_identifier(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// A script bound to one pipeline revision's slots.
pub struct ScriptStep {
    id: String,
    engine: Engine,
    ast: AST,
    reads: Vec<(String, usize)>,
    writes: Vec<(String, usize)>,
}

impl ScriptStep {
    pub fn build(
        def: &ScriptDef,
        bindings: &Bindings,
        world: &Arc<WorldStore>,
    ) -> Result<Self, String> {
        let engine = make_engine(Arc::clone(world));
        let ast = engine
            .compile(&def.source)
            .map_err(|error| format!("compile error: {error}"))?;

        Ok(Self {
            id: def.contract.label.clone(),
            engine,
            ast,
            reads: def
                .contract
                .inputs
                .iter()
                .map(|input| Ok((input.name.to_string(), bindings.input(&input.name)?)))
                .collect::<Result<_, String>>()?,
            writes: def
                .contract
                .outputs
                .iter()
                .map(|output| Ok((output.name.to_string(), bindings.output(&output.name)?)))
                .collect::<Result<_, String>>()?,
        })
    }
}

impl ChunkStep<Voxel, ColumnData, WorldStore> for ScriptStep {
    fn run(
        &self,
        _cells: &mut DataSet<Voxel>,
        chunk: &mut ColumnData,
        context: &StepContext,
        _world: &WorldStore,
    ) -> PipelineStepResult {
        let origin = context.origin();
        let mut scope = Scope::new();

        for (column, x, y, _) in chunk.columns.iter_mut() {
            let (gx, gy) = (origin.0 + x as i64, origin.1 + y as i64);

            let mut map = Map::new();
            map.insert("x".into(), Dynamic::from_int(gx as INT));
            map.insert("y".into(), Dynamic::from_int(gy as INT));
            for (name, slot) in &self.reads {
                map.insert(name.as_str().into(), Dynamic::from_float(column[*slot] as FLOAT));
            }

            let mut this = Dynamic::from_map(map);
            let options = CallFnOptions::new()
                .eval_ast(false)
                .rewind_scope(true)
                .bind_this_ptr(&mut this);

            // The return value is ignored; results are read from `this`.
            let _ = self
                .engine
                .call_fn_with_options::<Dynamic>(options, &mut scope, &self.ast, "column", ())
                .map_err(|error| {
                    format!("{}: {error} (at column {gx}, {gy})", self.id)
                })?;

            let result = this
                .try_cast::<Map>()
                .ok_or_else(|| format!("{}: `this` was replaced with a non-map", self.id))?;

            for (name, slot) in &self.writes {
                let value = result.get(name.as_str()).ok_or_else(|| {
                    format!("{}: did not assign `{name}` (at column {gx}, {gy})", self.id)
                })?;

                column[*slot] = number(value).ok_or_else(|| {
                    format!(
                        "{}: `{name}` must be a number, got {} (at column {gx}, {gy})",
                        self.id,
                        value.type_name()
                    )
                })? as f32;
            }
        }

        Ok(())
    }
}

fn number(value: &Dynamic) -> Option<f64> {
    value
        .as_float()
        .ok()
        .or_else(|| value.as_int().ok().map(|v| v as f64))
}

fn make_engine(world: Arc<WorldStore>) -> Engine {
    let mut engine = Engine::new();
    engine.set_max_operations(MAX_OPERATIONS);
    engine.set_max_call_levels(32);

    let w = Arc::clone(&world);
    engine.register_fn("latitude", move |y: INT| w.latitude_at(y) as FLOAT);
    let w = Arc::clone(&world);
    engine.register_fn("sea_level", move || w.sea_level() as FLOAT);
    let w = Arc::clone(&world);
    engine.register_fn("depth", move || w.depth() as FLOAT);
    let w = Arc::clone(&world);
    engine.register_fn("noise", move |x: INT, y: INT, wavelength: FLOAT| {
        w.script_noise(x, y, wavelength) as FLOAT
    });

    engine.register_fn("clamp", |v: FLOAT, lo: FLOAT, hi: FLOAT| v.clamp(lo, hi.max(lo)));
    engine.register_fn("lerp", |a: FLOAT, b: FLOAT, t: FLOAT| a + (b - a) * t);
    engine.register_fn("smoothstep", |e0: FLOAT, e1: FLOAT, x: FLOAT| {
        let t = ((x - e0) / (e1 - e0)).clamp(0.0, 1.0);
        t * t * (3.0 - 2.0 * t)
    });
    engine.register_fn("min", |a: FLOAT, b: FLOAT| a.min(b));
    engine.register_fn("max", |a: FLOAT, b: FLOAT| a.max(b));

    engine
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn template_parses_into_a_contract() {
        let def = ScriptDef::parse(TEMPLATE).unwrap();

        let inputs: Vec<_> = def.contract.inputs.iter().map(|i| i.name.as_ref()).collect();

        assert_eq!(def.contract.label, "Moisture");
        assert_eq!(inputs, ["elevation", "boundary_distance", "temperature"]);
        assert_eq!(def.contract.outputs.len(), 1);
        assert_eq!(def.contract.outputs[0].name, "moisture");
        assert_eq!(def.contract.outputs[0].ramp, "thermal");
        assert_eq!((def.contract.outputs[0].min, def.contract.outputs[0].max), (0.0, 1.0));
    }

    #[test]
    fn blank_template_parses() {
        let def = ScriptDef::parse(BLANK).unwrap();
        assert_eq!(def.contract.label, "My step");
        assert_eq!(def.contract.inputs[0].name, "elevation");
        assert_eq!(def.contract.outputs[0].name, "my_field");
    }

    #[test]
    fn header_and_compile_errors_are_reported() {
        assert!(ScriptDef::parse("fn column() {}").unwrap_err().contains("name"));
        assert!(ScriptDef::parse("//! name: A\nfn column() {}")
            .unwrap_err()
            .contains("output"));
        assert!(ScriptDef::parse("//! name: A\n//! writes: a\nfn other() {}")
            .unwrap_err()
            .contains("fn column()"));
        assert!(ScriptDef::parse("//! name: A\n//! writes: a\nfn column() { let = }")
            .unwrap_err()
            .contains("compile error"));
        assert!(ScriptDef::parse("//! name: A\n//! writes: a 0..x\nfn column() {}")
            .unwrap_err()
            .contains("line 2"));
        assert!(ScriptDef::parse("//! name: A\n//! colour: red\n//! writes: a\nfn column() {}")
            .unwrap_err()
            .contains("unknown header key"));
    }

    #[test]
    fn write_entries_parse_range_ramp_and_unit() {
        let field = parse_write("heat -30..40 thermal °C").unwrap();

        assert_eq!(field.name, "heat");
        assert_eq!((field.min, field.max), (-30.0, 40.0));
        assert_eq!(field.ramp, "thermal");
        assert_eq!(field.unit, "°C");
    }

    #[test]
    fn legacy_aliases_and_duplicate_sockets() {
        let legacy = "//! name: A\n//! reads: x\n//! writes: y\nfn column() { this.y = this.x; }";
        let def = ScriptDef::parse(legacy).unwrap();
        assert_eq!(def.contract.inputs[0].name, "x");
        assert_eq!(def.contract.outputs[0].name, "y");

        let duplicate = "//! name: A\n//! input: v\n//! output: v\nfn column() {}";
        assert!(ScriptDef::parse(duplicate).unwrap_err().contains("declared twice"));
    }
}
