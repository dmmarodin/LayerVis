//! Biome table and the `biome` atlas node (Minecraft-1.18-style lookup in a
//! parameter space; design doc §4.5).
//!
//! Biomes are data rows (`data/biomes.json`), looked up by name like
//! materials; a biome id is an index into one world's [`BiomeTable`], which
//! is saved with the atlas. Every biome declares ranges over climate and
//! terrain parameters; a cell gets the biome whose ranges it is closest to
//! (distance 0 inside every range, ties go to the earlier row), plus the
//! runner-up and a blend weight so borders can be mixed.

use std::sync::{Arc, OnceLock};

use serde::{Deserialize, Serialize};

use crate::{
    atlas::{AtlasIo, AtlasRun, AtlasStage, BoxedAtlasStage},
    climate::ocean_distance,
    pipeline::{Build, InputSpec, NodeKind, ParamSpec},
    strata::field,
    FieldScope, ValueKind,
};

const OPEN: [f32; 2] = [-1.0e9, 1.0e9];

fn open() -> [f32; 2] {
    OPEN
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BiomeDef {
    pub name: String,
    pub label: String,
    pub color: [u8; 3],
    /// Map glyph for text views.
    pub glyph: String,
    /// Annual mean temperature, °C.
    #[serde(default = "open")]
    pub temperature: [f32; 2],
    /// Annual precipitation, mm.
    #[serde(default = "open")]
    pub precipitation: [f32; 2],
    /// Height above sea level in cells (negative under water).
    #[serde(default = "open")]
    pub elevation: [f32; 2],
    /// Terrain slope (height per column).
    #[serde(default = "open")]
    pub slope: [f32; 2],
    /// Surface water: 0 dry, rising near rivers, 1 in a lake.
    #[serde(default = "open")]
    pub water: [f32; 2],
    /// Distance to the sea in columns.
    #[serde(default = "open")]
    pub coast: [f32; 2],
    /// Tree density (trees per scatter point, 0 … 1).
    #[serde(default)]
    pub trees: f32,
    /// Relative terrain detail amplitude (0 flat … 1 rugged).
    #[serde(default)]
    pub roughness: f32,
    /// Surface material name.
    #[serde(default)]
    pub surface: String,
}

impl BiomeDef {
    fn unknown(name: &str) -> Self {
        Self {
            name: name.to_owned(),
            label: format!("Unknown ({name})"),
            color: [255, 0, 255],
            glyph: "?".into(),
            temperature: [1.0e9, 1.0e9],
            precipitation: OPEN,
            elevation: OPEN,
            slope: OPEN,
            water: OPEN,
            coast: OPEN,
            trees: 0.0,
            roughness: 0.0,
            surface: String::new(),
        }
    }
}

/// The built-in rows, in registry order.
pub fn registry() -> &'static [BiomeDef] {
    static ROWS: OnceLock<Vec<BiomeDef>> = OnceLock::new();
    ROWS.get_or_init(|| {
        serde_json::from_str(include_str!("data/biomes.json"))
            .expect("the built-in biome table is valid JSON")
    })
}

/// One world's biome ids: id = index.
#[derive(Clone, Debug, PartialEq)]
pub struct BiomeTable {
    defs: Vec<BiomeDef>,
    names: Vec<String>,
}

impl BiomeTable {
    pub fn builtin() -> Arc<Self> {
        static TABLE: OnceLock<Arc<BiomeTable>> = OnceLock::new();
        Arc::clone(TABLE.get_or_init(|| Arc::new(Self::from_defs(registry().to_vec()))))
    }

    fn from_defs(defs: Vec<BiomeDef>) -> Self {
        let names = defs.iter().map(|d| d.name.clone()).collect();
        Self { defs, names }
    }

    /// A saved table; names the registry no longer knows stay as explicit
    /// unknown biomes (never matched by the lookup).
    pub fn from_names(names: &[String]) -> Self {
        Self::from_defs(
            names
                .iter()
                .map(|name| {
                    registry()
                        .iter()
                        .find(|d| &d.name == name)
                        .cloned()
                        .unwrap_or_else(|| BiomeDef::unknown(name))
                })
                .collect(),
        )
    }

    pub fn id(&self, name: &str) -> Option<u16> {
        self.names.iter().position(|n| n == name).map(|i| i as u16)
    }

    pub fn get(&self, id: u16) -> Option<&BiomeDef> {
        self.defs.get(usize::from(id))
    }

    pub fn defs(&self) -> &[BiomeDef] {
        &self.defs
    }

    pub fn names(&self) -> &[String] {
        &self.names
    }

    pub fn len(&self) -> usize {
        self.defs.len()
    }

    pub fn is_empty(&self) -> bool {
        self.defs.is_empty()
    }

    /// Best and second-best biome for a point in parameter space, with the
    /// second's blend weight (0 … 0.5; 0.5 = exactly between the two).
    pub fn lookup(&self, point: &BiomePoint, blend: f32) -> (u16, u16, f32) {
        let mut best = (f32::INFINITY, 0u16);
        let mut second = (f32::INFINITY, 0u16);
        for (id, def) in self.defs.iter().enumerate() {
            let d = point.distance(def);
            if d < best.0 {
                second = best;
                best = (d, id as u16);
            } else if d < second.0 {
                second = (d, id as u16);
            }
        }
        if !second.0.is_finite() {
            return (best.1, best.1, 0.0);
        }
        let gap = (second.0.sqrt() - best.0.sqrt()).max(0.0);
        let weight = (0.5 - gap / blend.max(1e-6) * 0.5).clamp(0.0, 0.5);
        (best.1, second.1, weight)
    }
}

/// A cell in biome parameter space.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct BiomePoint {
    pub temperature: f32,
    pub precipitation: f32,
    pub elevation: f32,
    pub slope: f32,
    pub water: f32,
    pub coast: f32,
}

impl BiomePoint {
    /// Squared distance outside the biome's ranges, each parameter divided
    /// by its scale (how much one unit of mismatch matters).
    pub fn distance(&self, def: &BiomeDef) -> f32 {
        let outside = |v: f32, [lo, hi]: [f32; 2], scale: f32| {
            let d = (lo - v).max(v - hi).max(0.0) / scale;
            d * d
        };
        outside(self.temperature, def.temperature, 4.0)
            + outside(self.precipitation, def.precipitation, 250.0)
            + outside(self.elevation, def.elevation, 1.5)
            + outside(self.slope, def.slope, 0.08)
            + outside(self.water, def.water, 0.15)
            + outside(self.coast, def.coast, 64.0)
    }
}

// ---------------------------------------------------------------------------

const fn atlas_in(name: &'static str, optional: bool) -> InputSpec {
    InputSpec {
        name: std::borrow::Cow::Borrowed(name),
        scope: FieldScope::Atlas,
        kind: ValueKind::Scalar,
        optional,
    }
}

pub static BIOME: NodeKind = NodeKind {
    id: "biome",
    label: "Biome",
    description: "Picks a biome per atlas cell from the biome table by temperature, \
                  precipitation, height above sea, slope, surface water (lakes, rivers) and \
                  distance to the coast, with the runner-up biome and a blend weight for \
                  mixed borders. Also outputs the biome's tree density and terrain \
                  roughness for scatter and terrain detail.",
    inputs: &[
        atlas_in("elevation", false),
        atlas_in("temperature", false),
        atlas_in("precipitation", false),
        atlas_in("lake_depth", true),
        atlas_in("discharge", true),
    ],
    outputs: &[
        field(
            "biome",
            FieldScope::Atlas,
            ValueKind::Scalar,
            "",
            32.0,
            "biome",
        ),
        field(
            "biome_second",
            FieldScope::Atlas,
            ValueKind::Scalar,
            "",
            32.0,
            "biome",
        ),
        field(
            "biome_blend",
            FieldScope::Atlas,
            ValueKind::Scalar,
            "",
            0.5,
            "grayscale",
        ),
        field(
            "tree_density",
            FieldScope::Atlas,
            ValueKind::Scalar,
            "",
            1.0,
            "thermal",
        ),
        field(
            "roughness",
            FieldScope::Atlas,
            ValueKind::Scalar,
            "",
            1.0,
            "thermal",
        ),
    ],
    params: &[
        // Width of mixed borders, in parameter-space distance units.
        ParamSpec {
            name: "blend",
            default: 1.0,
            min: 0.01,
            max: 10.0,
        },
        // The table's height ranges (cells above sea) are written for a
        // 128-deep world (about 90 cells of land relief). Heights are divided
        // by this before the lookup: about 2.3 suits 384 levels with the sea
        // at 180.
        ParamSpec {
            name: "height_scale",
            default: 1.0,
            min: 0.1,
            max: 10.0,
        },
        // Discharge (cells drained) that counts as a big river (water = 0.8).
        ParamSpec {
            name: "river_discharge",
            default: 4000.0,
            min: 10.0,
            max: 1.0e7,
        },
    ],
    build: Build::Atlas(|bindings, params, _| {
        Ok(Box::new(BiomeStage {
            elevation: bindings.input("elevation")?,
            temperature: bindings.input("temperature")?,
            precipitation: bindings.input("precipitation")?,
            lake_depth: bindings.optional_input("lake_depth"),
            discharge: bindings.optional_input("discharge"),
            out_biome: bindings.output("biome")?,
            out_second: bindings.output("biome_second")?,
            out_blend: bindings.output("biome_blend")?,
            out_trees: bindings.output("tree_density")?,
            out_roughness: bindings.output("roughness")?,
            blend: params.get("blend") as f32,
            height_scale: params.get("height_scale") as f32,
            river_discharge: params.get("river_discharge") as f32,
        }) as BoxedAtlasStage)
    }),
};

struct BiomeStage {
    elevation: usize,
    temperature: usize,
    precipitation: usize,
    lake_depth: Option<usize>,
    discharge: Option<usize>,
    out_biome: usize,
    out_second: usize,
    out_blend: usize,
    out_trees: usize,
    out_roughness: usize,
    blend: f32,
    height_scale: f32,
    river_discharge: f32,
}

impl AtlasStage for BiomeStage {
    fn run(&self, io: &mut AtlasIo<'_>, run: &mut AtlasRun<'_>) -> Result<(), String> {
        let context = *io.context();
        let (width, height) = context.grid;
        let (w, h) = (width as usize, height as usize);
        let sea = context.sea_level();
        let cell = context.cell_size as f32;
        let elevation = io.input(self.elevation)?.values();
        let temperature = io.input(self.temperature)?.values();
        let precipitation = io.input(self.precipitation)?.values();
        let lake = io.optional(self.lake_depth)?.map(|g| g.values());
        let discharge = io.optional(self.discharge)?.map(|g| g.values());
        let coast = ocean_distance(elevation, width, height, sea, cell);
        let table = io.biomes().clone();
        run.progress(0.1);

        let n = w * h;
        let (mut biome, mut second, mut blend) = (vec![0.0; n], vec![0.0; n], vec![0.0; n]);
        let (mut trees, mut roughness) = (vec![0.0; n], vec![0.0; n]);
        let at = |x: usize, y: usize| elevation[x.min(w - 1) + y.min(h - 1) * w];

        for y in 0..h {
            run.check()?;
            for x in 0..w {
                let i = x + y * w;
                let gx = (at(x + 1, y) - at(x.saturating_sub(1), y)) / 2.0;
                let gy = (at(x, y + 1) - at(x, y.saturating_sub(1))) / 2.0;
                let slope = (gx * gx + gy * gy).sqrt() / cell;
                let lake_here = lake.is_some_and(|l| l[i] > 0.5);
                let river = discharge.map_or(0.0, |q| {
                    (q[i].max(1.0).log10() / self.river_discharge.log10()).clamp(0.0, 1.0) * 0.8
                });
                let point = BiomePoint {
                    temperature: temperature[i],
                    precipitation: precipitation[i],
                    elevation: (elevation[i] - sea) / self.height_scale,
                    slope,
                    water: if lake_here { 1.0 } else { river },
                    coast: coast[i],
                };
                let (a, b, weight) = table.lookup(&point, self.blend);
                let (da, db) = (table.get(a), table.get(b));
                let mix = |f: fn(&BiomeDef) -> f32| {
                    let va = da.map_or(0.0, f);
                    let vb = db.map_or(0.0, f);
                    va * (1.0 - weight) + vb * weight
                };
                biome[i] = f32::from(a);
                second[i] = f32::from(b);
                blend[i] = weight;
                trees[i] = mix(|d| d.trees);
                // Steep ground is rougher whatever the biome.
                roughness[i] = (mix(|d| d.roughness) * (0.6 + (slope * 4.0).min(1.0) * 0.4))
                    .clamp(0.0, 1.0);
            }
            run.progress(0.1 + 0.9 * (y + 1) as f64 / h as f64);
        }

        io.write_values(self.out_biome, biome)?;
        io.write_values(self.out_second, second)?;
        io.write_values(self.out_blend, blend)?;
        io.write_values(self.out_trees, trees)?;
        io.write_values(self.out_roughness, roughness)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn name(table: &BiomeTable, id: u16) -> &str {
        &table.get(id).unwrap().name
    }

    #[test]
    fn lookup_picks_plausible_biomes() {
        let table = BiomeTable::builtin();
        let land = BiomePoint {
            elevation: 10.0,
            coast: 500.0,
            ..Default::default()
        };
        let pick = |p: BiomePoint| name(&table, table.lookup(&p, 1.0).0).to_owned();

        assert_eq!(
            pick(BiomePoint {
                temperature: 25.0,
                precipitation: 3000.0,
                ..land
            }),
            "tropical_rainforest"
        );
        assert_eq!(
            pick(BiomePoint {
                temperature: 24.0,
                precipitation: 100.0,
                ..land
            }),
            "desert"
        );
        assert_eq!(
            pick(BiomePoint {
                temperature: 10.0,
                precipitation: 1200.0,
                ..land
            }),
            "temperate_forest"
        );
        assert_eq!(
            pick(BiomePoint {
                temperature: 10.0,
                elevation: -30.0,
                ..land
            }),
            "deep_ocean"
        );
        assert_eq!(
            pick(BiomePoint {
                temperature: 10.0,
                precipitation: 1200.0,
                water: 1.0,
                ..land
            }),
            "lake"
        );
    }

    #[test]
    fn blend_weight_grows_towards_a_border() {
        let table = BiomeTable::builtin();
        let deep = BiomePoint {
            temperature: 10.0,
            precipitation: 1200.0,
            elevation: 10.0,
            coast: 500.0,
            ..Default::default()
        };
        let edge = BiomePoint {
            precipitation: 1880.0,
            ..deep
        };
        assert!(table.lookup(&edge, 1.0).2 > table.lookup(&deep, 1.0).2);
    }
}
