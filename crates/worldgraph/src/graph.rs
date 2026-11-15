//! Graph document: the saved, portable world-generation definition.
//!
//! This is the file format the game will load (see CLAUDE.md "Product
//! direction"). Keep it stable: bump `FORMAT` and add a migration in
//! `GraphDoc::from_json` instead of breaking old files.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

pub const FORMAT: u32 = 3;

/// Seed of documents that do not store one (and of new projects).
pub const DEFAULT_SEED: u64 = 12345;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GraphDoc {
    pub format: u32,
    /// World seed. Part of the world definition: the same graph with the same
    /// seed generates the same world. Optional in files (older files and
    /// hand-written ones get `DEFAULT_SEED`), so still `format: 1`.
    #[serde(default = "default_seed")]
    pub seed: u64,
    /// Format 1 only: the old built-in world parameters. Read so old files
    /// can be upgraded (they become node params, see `migrate`), never written.
    #[serde(default, skip_serializing)]
    pub world: BTreeMap<String, f64>,
    /// World size, depth and atlas cell size (format 3). Absent: the caller's
    /// `PipelineConfig` decides the size and the atlas uses
    /// `DEFAULT_CELL_SIZE`, which is how format 2 graphs behaved.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extent: Option<Extent>,
    pub nodes: Vec<NodeDoc>,
    pub edges: Vec<EdgeDoc>,
    /// Editor annotations (labelled boxes around nodes). Layout only: they
    /// never affect execution, and the game can ignore them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub frames: Vec<FrameDoc>,
}

/// The world a graph defines: its size in columns, its depth in cells and
/// the atlas resolution. Part of the world definition (and of
/// `world_signature`): changing it builds a new world.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Extent {
    /// Width and height in columns. The world is centered on (0, 0).
    pub size: (u32, u32),
    /// Height range in cells.
    pub depth: u32,
    /// Columns per atlas cell side.
    #[serde(default = "default_cell_size")]
    pub cell_size: u32,
}

fn default_cell_size() -> u32 {
    crate::atlas::DEFAULT_CELL_SIZE
}

impl Extent {
    /// LayerVis's world before format 3: 8192 x 8192 columns, 128 deep.
    pub const DEFAULT: Extent = Extent {
        size: (8192, 8192),
        depth: 128,
        cell_size: crate::atlas::DEFAULT_CELL_SIZE,
    };

    /// Minimum corner of the world bounds (centered on the origin).
    pub fn origin(&self) -> (i64, i64, i64) {
        (-i64::from(self.size.0 / 2), -i64::from(self.size.1 / 2), 0)
    }

    /// Checks the ranges a world can be built with.
    pub fn validate(&self) -> Result<(), String> {
        if !(256..=1 << 20).contains(&self.size.0) || !(256..=1 << 20).contains(&self.size.1) {
            return Err("world size must be between 256 and 1048576 columns".to_owned());
        }
        if !(32..=4096).contains(&self.depth) {
            return Err("world depth must be between 32 and 4096 cells".to_owned());
        }
        if !(1..=1024).contains(&self.cell_size) {
            return Err("atlas cell size must be between 1 and 1024 columns".to_owned());
        }
        let cells = u64::from(self.size.0.div_ceil(self.cell_size))
            * u64::from(self.size.1.div_ceil(self.cell_size));
        if cells > 4096 * 4096 {
            return Err(
                "the atlas would exceed 4096 x 4096 cells (raise the cell size)".to_owned(),
            );
        }
        Ok(())
    }
}

/// A labelled rectangle drawn behind nodes to group them visually.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FrameDoc {
    /// Unique, stable (`f<k>`); never collides with node ids (`n<k>`).
    pub id: String,
    #[serde(default)]
    pub label: String,
    /// Top-left corner, in graph units.
    #[serde(default)]
    pub position: (f64, f64),
    /// Width and height, in graph units.
    pub size: (f64, f64),
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NodeDoc {
    /// Unique, stable id (`n<k>`). Edges and saved views refer to it.
    pub id: String,
    /// Registered node kind (`elevation`, `smooth`, …) or `script`.
    pub kind: String,
    #[serde(default = "enabled_default")]
    pub enabled: bool,
    /// Editor layout only; never affects execution.
    #[serde(default)]
    pub position: (f64, f64),
    /// Parameter values; missing entries use the kind's defaults.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub params: BTreeMap<String, f64>,
    /// Rhai source for `script` nodes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub script: Option<String>,
    /// User-facing title; for Export nodes this is the export name the game
    /// reads. Optional: defaults to the kind's label.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

fn enabled_default() -> bool {
    true
}

fn default_seed() -> u64 {
    DEFAULT_SEED
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SocketRef {
    pub node: String,
    pub socket: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EdgeDoc {
    /// An output socket.
    pub from: SocketRef,
    /// An input socket; at most one edge per input.
    pub to: SocketRef,
}

impl GraphDoc {
    pub fn from_json(text: &str) -> Result<Self, String> {
        let mut doc: Self =
            serde_json::from_str(text).map_err(|e| format!("invalid graph file: {e}"))?;

        if doc.format > FORMAT {
            return Err(format!(
                "graph format {} is newer than supported ({FORMAT})",
                doc.format
            ));
        }

        let mut ids = std::collections::HashSet::new();
        for node in &doc.nodes {
            if !ids.insert(node.id.as_str()) {
                return Err(format!("duplicate node id `{}`", node.id));
            }
        }

        let mut frame_ids = std::collections::HashSet::new();
        for frame in &doc.frames {
            if !frame_ids.insert(frame.id.as_str()) || ids.contains(frame.id.as_str()) {
                return Err(format!("duplicate id `{}`", frame.id));
            }
        }

        // Older files are upgraded in place (after validation, before use).
        crate::migrate::upgrade(&mut doc);

        Ok(doc)
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).expect("graph documents always serialize")
    }

    pub fn node(&self, id: &str) -> Option<&NodeDoc> {
        self.nodes.iter().find(|node| node.id == id)
    }

    pub fn node_mut(&mut self, id: &str) -> Option<&mut NodeDoc> {
        self.nodes.iter_mut().find(|node| node.id == id)
    }

    pub fn frame_mut(&mut self, id: &str) -> Option<&mut FrameDoc> {
        self.frames.iter_mut().find(|frame| frame.id == id)
    }

    /// Next free `f<k>` frame id.
    pub fn next_frame_id(&self) -> String {
        let next = self
            .frames
            .iter()
            .filter_map(|frame| frame.id.strip_prefix('f')?.parse::<u64>().ok())
            .max()
            .map_or(1, |max| max + 1);
        format!("f{next}")
    }

    /// Next free `n<k>` id.
    pub fn next_id(&self) -> String {
        let next = self
            .nodes
            .iter()
            .filter_map(|node| node.id.strip_prefix('n')?.parse::<u64>().ok())
            .max()
            .map_or(1, |max| max + 1);
        format!("n{next}")
    }
}

/// Numeric order of `n<k>` ids (creation order); other ids sort after.
pub fn id_order(id: &str) -> (u64, &str) {
    let number = id
        .strip_prefix('n')
        .and_then(|n| n.parse().ok())
        .unwrap_or(u64::MAX);
    (number, id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_and_defaults_optional_fields() {
        let text = r#"{
            "format": 2,
            "nodes": [{ "id": "n1", "kind": "tectonics" }],
            "edges": []
        }"#;
        let doc = GraphDoc::from_json(text).unwrap();

        assert!(doc.nodes[0].enabled);
        assert_eq!(doc.nodes[0].position, (0.0, 0.0));
        assert_eq!(doc.seed, DEFAULT_SEED);
        assert!(doc.frames.is_empty());
        assert!(doc.world.is_empty());
        assert!(
            !doc.to_json().contains("\"world\""),
            "the legacy world block is never written"
        );
        assert_eq!(doc.format, FORMAT);
        assert!(
            !doc.to_json().contains("frames"),
            "empty frames are not written"
        );

        assert!(doc.extent.is_none());
        assert!(
            !doc.to_json().contains("extent"),
            "an absent extent is not written"
        );

        let sized = GraphDoc {
            extent: Some(Extent {
                size: (4096, 2048),
                depth: 384,
                cell_size: 32,
            }),
            ..doc.clone()
        };
        assert_eq!(GraphDoc::from_json(&sized.to_json()).unwrap(), sized);
        let no_cell = r#"{"format": 3, "extent": {"size": [512, 512], "depth": 64}, "nodes": [], "edges": []}"#;
        assert_eq!(
            GraphDoc::from_json(no_cell)
                .unwrap()
                .extent
                .unwrap()
                .cell_size,
            crate::atlas::DEFAULT_CELL_SIZE
        );
        assert!(Extent::DEFAULT.validate().is_ok());
        assert!(Extent {
            depth: 8,
            ..Extent::DEFAULT
        }
        .validate()
        .is_err());
        assert!(
            Extent {
                cell_size: 1,
                ..Extent::DEFAULT
            }
            .validate()
            .is_err(),
            "8192² cells is too many"
        );

        let seeded = GraphDoc {
            seed: u64::MAX,
            ..doc.clone()
        };
        assert_eq!(
            GraphDoc::from_json(&seeded.to_json()).unwrap().seed,
            u64::MAX
        );
        assert_eq!(GraphDoc::from_json(&doc.to_json()).unwrap(), doc);
        assert_eq!(doc.next_id(), "n2");
    }

    #[test]
    fn rejects_newer_formats_and_duplicate_ids() {
        assert!(
            GraphDoc::from_json(r#"{"format": 99, "nodes": [], "edges": []}"#)
                .unwrap_err()
                .contains("newer")
        );

        let duplicate = r#"{"format": 1, "edges": [], "nodes": [
            {"id": "n1", "kind": "a"}, {"id": "n1", "kind": "b"}]}"#;
        assert!(GraphDoc::from_json(duplicate)
            .unwrap_err()
            .contains("duplicate"));
    }

    #[test]
    fn frames_round_trip_and_ids_must_be_unique() {
        let text = r#"{"format": 1, "nodes": [{"id": "n1", "kind": "elevation"}], "edges": [],
            "frames": [{"id": "f2", "label": "Climate", "position": [10, 20], "size": [300, 200]}]}"#;
        let doc = GraphDoc::from_json(text).unwrap();

        assert_eq!(doc.frames[0].size, (300.0, 200.0));
        assert_eq!(doc.next_frame_id(), "f3");
        assert_eq!(GraphDoc::from_json(&doc.to_json()).unwrap(), doc);

        let clash = text.replace("\"f2\"", "\"n1\"");
        assert!(GraphDoc::from_json(&clash)
            .unwrap_err()
            .contains("duplicate"));
    }
}
