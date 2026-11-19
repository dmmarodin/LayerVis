//! Material registry: what voxels and rock layers are made of.
//!
//! Materials are data rows, not an enum: the built-in table is
//! `data/materials.json`, and mods will add rows later, so code always looks
//! materials up **by name**. A material id is an index into one world's
//! [`MaterialTable`]; the table (names in id order) is saved with the world's
//! atlas, so ids stay valid when the registry changes. A name the registry
//! no longer knows loads as an explicit "unknown" material, never silently
//! as a different one.

use std::sync::{Arc, OnceLock};

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum MaterialCategory {
    Igneous,
    Metamorphic,
    Sedimentary,
    Soil,
    Sediment,
    Fluid,
    Air,
    /// Built by the game (planks, bricks, paving); never generated. Solid.
    Constructed,
    /// A saved id whose name is not in the registry (a removed mod).
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MaterialDef {
    pub name: String,
    pub category: MaterialCategory,
    /// Multiplies the erosion constant: hard rock < 1 < soft rock.
    pub erodibility: f32,
    /// Map color (sRGB).
    pub color: [u8; 3],
}

impl MaterialDef {
    /// Solid ground (not air or a fluid).
    pub fn is_solid(&self) -> bool {
        !matches!(
            self.category,
            MaterialCategory::Air | MaterialCategory::Fluid
        )
    }
}

/// The built-in rows, in registry order.
pub fn registry() -> &'static [MaterialDef] {
    static ROWS: OnceLock<Vec<MaterialDef>> = OnceLock::new();
    ROWS.get_or_init(|| {
        serde_json::from_str(include_str!("data/materials.json"))
            .expect("the built-in material table is valid JSON")
    })
}

/// One world's material ids: id = index.
#[derive(Clone, Debug, PartialEq)]
pub struct MaterialTable {
    defs: Vec<MaterialDef>,
    names: Vec<String>,
}

impl MaterialTable {
    /// The table of a new world: every registry row, in registry order.
    pub fn builtin() -> Arc<Self> {
        static TABLE: OnceLock<Arc<MaterialTable>> = OnceLock::new();
        Arc::clone(TABLE.get_or_init(|| Arc::new(Self::from_defs(registry().to_vec()))))
    }

    fn from_defs(defs: Vec<MaterialDef>) -> Self {
        let names = defs.iter().map(|d| d.name.clone()).collect();
        Self { defs, names }
    }

    /// A saved table: ids keep their names; names the registry does not know
    /// become `Unknown` materials.
    pub fn from_names(names: &[String]) -> Self {
        Self::from_defs(
            names
                .iter()
                .map(|name| {
                    registry()
                        .iter()
                        .find(|d| &d.name == name)
                        .cloned()
                        .unwrap_or_else(|| MaterialDef {
                            name: name.clone(),
                            category: MaterialCategory::Unknown,
                            erodibility: 1.0,
                            color: [255, 0, 255],
                        })
                })
                .collect(),
        )
    }

    pub fn id(&self, name: &str) -> Option<u16> {
        self.names.iter().position(|n| n == name).map(|i| i as u16)
    }

    /// Id of a material nodes rely on; an error names it when missing.
    pub fn require(&self, name: &str) -> Result<u16, String> {
        self.id(name)
            .ok_or_else(|| format!("material `{name}` is not in this world's material table"))
    }

    pub fn get(&self, id: u16) -> Option<&MaterialDef> {
        self.defs.get(usize::from(id))
    }

    /// Erodibility of a material id (1 for unknown ids).
    pub fn erodibility(&self, id: u16) -> f32 {
        self.get(id).map_or(1.0, |d| d.erodibility)
    }

    pub fn defs(&self) -> &[MaterialDef] {
        &self.defs
    }

    /// Names in id order (what the atlas file stores).
    pub fn names(&self) -> &[String] {
        &self.names
    }

    pub fn len(&self) -> usize {
        self.defs.len()
    }

    pub fn is_empty(&self) -> bool {
        self.defs.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_table_has_the_planned_materials() {
        let table = MaterialTable::builtin();
        for name in [
            "granite", "basalt", "gneiss", "marble", "slate", "sandstone", "shale", "limestone",
            "soil", "sand", "gravel", "clay", "water", "air", "plank", "log", "thatch", "brick",
            "stone_brick", "cobblestone", "path", "plaster", "roof_tile", "flagstone",
        ] {
            assert!(table.id(name).is_some(), "{name} missing");
        }
        let granite = table.require("granite").unwrap();
        assert!(table.erodibility(granite) < table.erodibility(table.id("shale").unwrap()));
    }

    #[test]
    fn saved_names_keep_ids_and_unknown_names_stay_explicit() {
        let names = vec!["sand".to_owned(), "mithril".to_owned(), "granite".to_owned()];
        let table = MaterialTable::from_names(&names);
        assert_eq!(table.id("granite"), Some(2));
        assert_eq!(table.get(1).unwrap().category, MaterialCategory::Unknown);
        assert_eq!(table.names(), names.as_slice());
    }
}
