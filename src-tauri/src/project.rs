//! The open project: which file the graph belongs to, whether it has unsaved
//! changes, and recently opened files.
//!
//! The working copy (`graph.json`, autosaved after every edit) is separate
//! from the project file the user saves explicitly. Quitting with unsaved
//! changes therefore loses nothing: the next start restores the working copy
//! and still shows it as modified.

use std::{
    fs,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use worldgraph::GraphDoc;

const WORKSPACE_FILE: &str = "workspace.json";
const MAX_RECENT: usize = 10;

pub const UNTITLED: &str = "Untitled";

pub struct Project {
    /// The project file, or None for an unsaved (untitled) graph.
    pub path: Option<PathBuf>,
    /// The doc as last saved or opened. None when the project file is gone
    /// or unreadable, so the working copy counts as modified.
    pub saved: Option<GraphDoc>,
    pub recent: Vec<PathBuf>,
}

/// What survives a restart (next to the autosaved working copy).
#[derive(Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct Workspace {
    project_path: Option<PathBuf>,
    recent: Vec<PathBuf>,
}

/// What the UI shows in its menu and title.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectState {
    pub path: Option<String>,
    pub name: String,
    pub dirty: bool,
    pub recent: Vec<String>,
    pub can_undo: bool,
    pub can_redo: bool,
}

impl Project {
    /// A new untitled project starting from the default graph.
    pub fn untitled(recent: Vec<PathBuf>) -> Self {
        Self {
            path: None,
            saved: Some(worldgraph::steps::default_graph()),
            recent,
        }
    }

    /// Restores the project identity saved by the previous run.
    pub fn restore(dir: &Path) -> Self {
        let workspace: Workspace = fs::read_to_string(dir.join(WORKSPACE_FILE))
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default();

        let Some(path) = workspace.project_path else {
            return Self::untitled(workspace.recent);
        };

        let saved = fs::read_to_string(&path)
            .ok()
            .and_then(|text| GraphDoc::from_json(&text).ok());

        Self {
            path: Some(path),
            saved,
            recent: workspace.recent,
        }
    }

    /// Persists the project identity (best effort, like autosave).
    pub fn persist(&self, dir: &Path) {
        let workspace = Workspace {
            project_path: self.path.clone(),
            recent: self.recent.clone(),
        };
        let result = serde_json::to_string_pretty(&workspace)
            .map_err(|error| error.to_string())
            .and_then(|text| {
                fs::write(dir.join(WORKSPACE_FILE), text).map_err(|error| error.to_string())
            });

        if let Err(error) = result {
            eprintln!("LayerVis: could not save the workspace: {error}");
        }
    }

    pub fn name(&self) -> String {
        self.path.as_deref().and_then(Path::file_stem).map_or_else(
            || UNTITLED.to_owned(),
            |stem| stem.to_string_lossy().into_owned(),
        )
    }

    pub fn is_dirty(&self, doc: &GraphDoc) -> bool {
        self.saved.as_ref() != Some(doc)
    }

    /// The doc was written to `path` (Save / Save As).
    pub fn mark_saved(&mut self, path: PathBuf, doc: GraphDoc) {
        self.touch_recent(&path);
        self.path = Some(path);
        self.saved = Some(doc);
    }

    /// A project file was opened.
    pub fn mark_opened(&mut self, path: PathBuf, doc: GraphDoc) {
        self.mark_saved(path, doc);
    }

    /// New project: untitled, starting from `doc` (not modified yet).
    pub fn mark_new(&mut self, doc: GraphDoc) {
        self.path = None;
        self.saved = Some(doc);
    }

    pub fn touch_recent(&mut self, path: &Path) {
        self.recent.retain(|p| p != path);
        self.recent.insert(0, path.to_owned());
        self.recent.truncate(MAX_RECENT);
    }

    pub fn forget_recent(&mut self, path: &Path) {
        self.recent.retain(|p| p != path);
    }

    pub fn state(&self, doc: &GraphDoc, can_undo: bool, can_redo: bool) -> ProjectState {
        ProjectState {
            path: self.path.as_ref().map(|p| p.display().to_string()),
            name: self.name(),
            dirty: self.is_dirty(doc),
            recent: self
                .recent
                .iter()
                .map(|p| p.display().to_string())
                .collect(),
            can_undo,
            can_redo,
        }
    }
}

/// Writes a graph document atomically (temp file + rename).
pub fn write_doc(path: &Path, doc: &GraphDoc) -> Result<(), String> {
    let mut temp = path.as_os_str().to_owned();
    temp.push(".tmp");
    let temp = PathBuf::from(temp);

    fs::write(&temp, doc.to_json())
        .and_then(|()| fs::rename(&temp, path))
        .map_err(|error| format!("Could not write {}: {error}", path.display()))
}

/// Reads and parses a graph document (format migrations happen in `from_json`).
pub fn read_doc(path: &Path) -> Result<GraphDoc, String> {
    let text = fs::read_to_string(path)
        .map_err(|error| format!("Could not read {}: {error}", path.display()))?;

    GraphDoc::from_json(&text)
        .map_err(|error| format!("{} is not a valid graph: {error}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("layervis-test-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn dirty_compares_against_the_saved_doc() {
        let mut project = Project::untitled(Vec::new());
        let mut doc = worldgraph::steps::default_graph();
        assert!(!project.is_dirty(&doc));
        assert_eq!(project.name(), UNTITLED);

        doc.nodes[0].position = (1.0, 2.0);
        assert!(project.is_dirty(&doc));

        project.mark_saved(PathBuf::from("worlds/island.json"), doc.clone());
        assert!(!project.is_dirty(&doc));
        assert_eq!(project.name(), "island");

        // Undoing back to the saved state is clean again.
        let mut other = doc.clone();
        other.nodes[0].position = (9.0, 9.0);
        assert!(project.is_dirty(&other));
        assert!(!project.is_dirty(&doc));
    }

    #[test]
    fn recent_files_are_unique_newest_first_and_capped() {
        let mut project = Project::untitled(Vec::new());
        for i in 0..12 {
            project.touch_recent(Path::new(&format!("{i}.json")));
        }
        project.touch_recent(Path::new("5.json"));

        assert_eq!(project.recent.len(), MAX_RECENT);
        assert_eq!(project.recent[0], PathBuf::from("5.json"));
        assert_eq!(
            project
                .recent
                .iter()
                .filter(|p| p.ends_with("5.json"))
                .count(),
            1
        );

        project.forget_recent(Path::new("5.json"));
        assert!(!project.recent.iter().any(|p| p.ends_with("5.json")));
    }

    #[test]
    fn workspace_round_trips_and_missing_project_files_count_as_modified() {
        let dir = temp_dir("workspace");
        let file = dir.join("world.json");
        let doc = worldgraph::steps::default_graph();

        write_doc(&file, &doc).unwrap();
        assert_eq!(read_doc(&file).unwrap(), doc);

        let mut project = Project::untitled(Vec::new());
        project.mark_saved(file.clone(), doc.clone());
        project.persist(&dir);

        let restored = Project::restore(&dir);
        assert_eq!(restored.path.as_deref(), Some(file.as_path()));
        assert!(!restored.is_dirty(&doc));
        assert_eq!(restored.recent, vec![file.clone()]);

        fs::remove_file(&file).unwrap();
        assert!(Project::restore(&dir).is_dirty(&doc));

        fs::write(&file, "not json").unwrap();
        assert!(read_doc(&file).is_err());
        let _ = fs::remove_dir_all(&dir);
    }
}
