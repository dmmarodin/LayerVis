//! Graph document: the saved, portable world-generation definition.
//!
//! This is the file format the game will load (see CLAUDE.md "Product
//! direction"). Keep it stable: bump `FORMAT` and add a migration in
//! `GraphDoc::from_json` instead of breaking old files.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

pub const FORMAT: u32 = 1;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GraphDoc {
    pub format: u32,
    pub nodes: Vec<NodeDoc>,
    pub edges: Vec<EdgeDoc>,
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
        let doc: Self = serde_json::from_str(text).map_err(|e| format!("invalid graph file: {e}"))?;

        if doc.format > FORMAT {
            return Err(format!(
                "graph format {} is newer than supported ({FORMAT})",
                doc.format
            ));
        }

        // Format 1 is the first format: no migrations yet.
        let mut ids = std::collections::HashSet::new();
        for node in &doc.nodes {
            if !ids.insert(node.id.as_str()) {
                return Err(format!("duplicate node id `{}`", node.id));
            }
        }

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
            "format": 1,
            "nodes": [{ "id": "n1", "kind": "elevation" }],
            "edges": []
        }"#;
        let doc = GraphDoc::from_json(text).unwrap();

        assert!(doc.nodes[0].enabled);
        assert_eq!(doc.nodes[0].position, (0.0, 0.0));
        assert_eq!(GraphDoc::from_json(&doc.to_json()).unwrap(), doc);
        assert_eq!(doc.next_id(), "n2");
    }

    #[test]
    fn rejects_newer_formats_and_duplicate_ids() {
        assert!(GraphDoc::from_json(r#"{"format": 99, "nodes": [], "edges": []}"#)
            .unwrap_err()
            .contains("newer"));

        let duplicate = r#"{"format": 1, "edges": [], "nodes": [
            {"id": "n1", "kind": "a"}, {"id": "n1", "kind": "b"}]}"#;
        assert!(GraphDoc::from_json(duplicate).unwrap_err().contains("duplicate"));
    }
}
