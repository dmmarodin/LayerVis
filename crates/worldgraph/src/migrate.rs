//! Upgrades older graph files to the current format (`graph::FORMAT`).
//!
//! * **1 → 2**: the world models used to be built in. The chunk `elevation`
//!   node sampled them, and a `world` params block (plate size, continents,
//!   edge vignette, …) configured them. Format 2 makes them world-scope
//!   nodes: every `elevation` node becomes a `tectonic_elevation` node (same
//!   id, same `elevation` output, so every wire out of it keeps working), a
//!   `tectonics` node is added and wired into them, and the old params move
//!   onto the nodes. A used edge vignette becomes an `edge_falloff` filter
//!   node right behind each elevation node.

use std::collections::BTreeMap;

use crate::graph::{EdgeDoc, GraphDoc, NodeDoc, SocketRef};

/// Sockets `tectonics` provides to `tectonic_elevation`.
const TECTONICS_TO_ELEVATION: [&str; 4] = [
    "boundary_distance",
    "convergence",
    "continental",
    "neighbor_continental",
];

pub fn upgrade(doc: &mut GraphDoc) {
    if doc.format < 2 {
        migrate_v1(doc);
        doc.format = 2;
    }
}

fn node(id: String, kind: &str, position: (f64, f64), params: BTreeMap<String, f64>) -> NodeDoc {
    NodeDoc {
        id,
        kind: kind.to_owned(),
        enabled: true,
        position,
        params,
        script: None,
        label: None,
    }
}

fn wire(from: &str, out: &str, to: &str, input: &str) -> EdgeDoc {
    EdgeDoc {
        from: SocketRef {
            node: from.to_owned(),
            socket: out.to_owned(),
        },
        to: SocketRef {
            node: to.to_owned(),
            socket: input.to_owned(),
        },
    }
}

fn migrate_v1(doc: &mut GraphDoc) {
    let legacy = std::mem::take(&mut doc.world);
    let pick = |names: &[&str]| -> BTreeMap<String, f64> {
        names
            .iter()
            .filter_map(|name| legacy.get(*name).map(|value| ((*name).to_owned(), *value)))
            .collect()
    };

    let elevations: Vec<String> = doc
        .nodes
        .iter()
        .filter(|node| node.kind == "elevation")
        .map(|node| node.id.clone())
        .collect();
    if elevations.is_empty() {
        return;
    }

    // One tectonics node serves every elevation node, to the left of them.
    let left = doc
        .nodes
        .iter()
        .filter(|node| elevations.contains(&node.id))
        .map(|node| node.position)
        .fold((f64::MAX, 0.0), |(x, y), p| if p.0 < x { p } else { (x, y) });
    let tectonics = doc.next_id();
    doc.nodes.push(node(
        tectonics.clone(),
        "tectonics",
        (left.0 - 340.0, left.1),
        pick(&["plate_size", "continents", "continental_fraction", "warp"]),
    ));

    let vignette = legacy.get("edge_margin").is_some_and(|v| *v > 0.0)
        || legacy.get("edge_falloff").is_some_and(|v| *v > 0.0);

    for id in &elevations {
        let position = {
            let elevation = doc.node_mut(id).expect("collected from the doc");
            elevation.kind = "tectonic_elevation".to_owned();
            elevation.position
        };
        for socket in TECTONICS_TO_ELEVATION {
            doc.edges.push(wire(&tectonics, socket, id, socket));
        }

        if vignette {
            let falloff = doc.next_id();
            let params = BTreeMap::from_iter(
                [
                    ("margin", "edge_margin"),
                    ("falloff", "edge_falloff"),
                    ("noise", "edge_noise"),
                    ("sea_floor", "edge_depth"),
                ]
                .into_iter()
                .filter_map(|(name, old)| legacy.get(old).map(|v| (name.to_owned(), *v))),
            );
            doc.nodes
                .push(node(falloff.clone(), "edge_falloff", (position.0, position.1 + 160.0), params));

            // Everything that read the elevation now reads the filtered one.
            for edge in &mut doc.edges {
                if edge.from.node == *id && edge.from.socket == "elevation" {
                    edge.from.node = falloff.clone();
                }
            }
            doc.edges.push(wire(id, "elevation", &falloff, "elevation"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v1(world: &str) -> GraphDoc {
        let text = format!(
            r#"{{"format": 1, "seed": 7, "world": {world},
                "nodes": [
                  {{"id": "n1", "kind": "elevation", "position": [0, 0]}},
                  {{"id": "n2", "kind": "temperature", "position": [320, -60]}},
                  {{"id": "n4", "kind": "export_column", "label": "height"}}],
                "edges": [
                  {{"from": {{"node": "n1", "socket": "elevation"}}, "to": {{"node": "n2", "socket": "elevation"}}}},
                  {{"from": {{"node": "n1", "socket": "elevation"}}, "to": {{"node": "n4", "socket": "value"}}}}]}}"#
        );
        GraphDoc::from_json(&text).unwrap()
    }

    #[test]
    fn elevation_becomes_world_nodes_and_keeps_its_wires() {
        let doc = v1("{}");

        assert_eq!(doc.format, 2);
        assert!(doc.world.is_empty());
        assert_eq!(doc.node("n1").unwrap().kind, "tectonic_elevation");

        // A tectonics node was added (next free id) and wired in.
        let tectonics = doc.node("n5").unwrap();
        assert_eq!(tectonics.kind, "tectonics");
        assert_eq!(tectonics.position, (-340.0, 0.0));
        for socket in TECTONICS_TO_ELEVATION {
            assert!(doc
                .edges
                .iter()
                .any(|e| e.from.node == "n5" && e.from.socket == socket && e.to.node == "n1"));
        }

        // Wires out of the old elevation node are untouched.
        assert!(doc.edges.iter().any(|e| e.from.node == "n1" && e.to.node == "n2"));
        assert!(doc.edges.iter().any(|e| e.from.node == "n1" && e.to.node == "n4"));
        assert!(doc.nodes.iter().all(|n| n.kind != "edge_falloff"));
    }

    #[test]
    fn old_world_params_move_onto_the_nodes_and_a_used_vignette_becomes_a_filter() {
        let doc = v1(
            r#"{"plate_size": 1100, "continents": 1, "edge_margin": 0.08, "edge_falloff": 0.14, "edge_noise": 0.05}"#,
        );

        let tectonics = doc.nodes.iter().find(|n| n.kind == "tectonics").unwrap();
        assert_eq!(tectonics.params["plate_size"], 1100.0);
        assert_eq!(tectonics.params["continents"], 1.0);

        let falloff = doc.nodes.iter().find(|n| n.kind == "edge_falloff").unwrap();
        assert_eq!(falloff.params["margin"], 0.08);
        assert_eq!(falloff.params["falloff"], 0.14);
        assert_eq!(falloff.params["noise"], 0.05);

        // elevation → filter → consumers.
        assert!(doc
            .edges
            .iter()
            .any(|e| e.from.node == "n1" && e.to.node == falloff.id && e.to.socket == "elevation"));
        assert!(doc
            .edges
            .iter()
            .any(|e| e.from.node == falloff.id && e.to.node == "n2" && e.to.socket == "elevation"));
        assert!(doc
            .edges
            .iter()
            .any(|e| e.from.node == falloff.id && e.to.node == "n4"));
        assert!(!doc
            .edges
            .iter()
            .any(|e| e.from.node == "n1" && (e.to.node == "n2" || e.to.node == "n4")));
    }

    #[test]
    fn current_files_are_left_alone_and_newer_formats_are_rejected() {
        let current = crate::steps::default_graph();
        assert_eq!(GraphDoc::from_json(&current.to_json()).unwrap(), current);

        assert!(GraphDoc::from_json(r#"{"format": 99, "nodes": [], "edges": []}"#).is_err());
    }
}
