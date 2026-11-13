mod generation;
mod project;

use std::{
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use generation::{CacheStats, PipelineGraph, Session, StepTiming, WorldInfo, WorldSample};
use worldgraph::{GraphDoc, SocketRef, DEFAULT_SEED};
use layergen::ChunkCoord;
use serde::{Deserialize, Serialize};
use project::{Project, ProjectState};
use tauri::{ipc::Response, Manager, State, WebviewWindow};
use tauri_plugin_dialog::{DialogExt, MessageDialogButtons, MessageDialogKind};

const GRAPH_FILE: &str = "graph.json";
const INVALID_GRAPH_FILE: &str = "graph.invalid.json";

type SharedSession = Arc<Session>;

/// Where the working copy of the graph is autosaved (separate from the
/// project file the user saves explicitly).
struct GraphFile(PathBuf);

/// App data directory (workspace state lives next to the working copy).
struct AppDir(PathBuf);

type SharedProject = Mutex<Project>;

// ---------------------------------------------------------------------------
// Persistence

/// Loads the saved graph, or the default graph when there is none. An
/// unreadable file is kept aside (never overwritten) and reported.
fn load_session(path: &Path) -> Session {
    let Ok(text) = fs::read_to_string(path) else {
        return Session::new(DEFAULT_SEED).expect("default graph builds");
    };

    match GraphDoc::from_json(&text).and_then(|doc| Session::with_doc(doc)) {
        Ok(session) => session,
        Err(error) => {
            let aside = path.with_file_name(INVALID_GRAPH_FILE);
            eprintln!(
                "LayerVis: could not load {} ({error}); kept it as {} and started a default graph",
                path.display(),
                aside.display()
            );
            let _ = fs::rename(path, aside);
            Session::new(DEFAULT_SEED).expect("default graph builds")
        }
    }
}

/// Writes the current doc atomically (temp file + rename).
fn save(session: &Session, file: &GraphFile) {
    let temp = file.0.with_extension("json.tmp");
    let result = fs::write(&temp, session.graph_doc().to_json())
        .and_then(|()| fs::rename(&temp, &file.0));

    if let Err(error) = result {
        eprintln!("LayerVis: autosave to {} failed: {error}", file.0.display());
    }
}

/// Runs a graph edit off the UI thread (edits validate by generating a
/// chunk), then autosaves on success.
async fn edit<T: Send + 'static>(
    session: &State<'_, SharedSession>,
    file: &State<'_, GraphFile>,
    change: impl FnOnce(&Session) -> Result<T, String> + Send + 'static,
) -> Result<T, String> {
    let worker = Arc::clone(session);
    let result = tauri::async_runtime::spawn_blocking(move || change(&worker))
        .await
        .map_err(|error| format!("Edit task failed: {error}"))??;

    save(session, file);
    Ok(result)
}

// ---------------------------------------------------------------------------
// World and chunks

#[tauri::command]
fn world_info(session: State<'_, SharedSession>) -> WorldInfo {
    session.world_info()
}

#[tauri::command]
fn cache_stats(session: State<'_, SharedSession>) -> CacheStats {
    session.cache_stats()
}

/// One chunk's column outputs as a binary payload (see
/// `generation::encode_columns`), generated off the UI thread on a miss.
#[tauri::command]
async fn generate_chunk(
    session: State<'_, SharedSession>,
    x: u32,
    y: u32,
    z: u32,
) -> Result<Response, String> {
    let session = Arc::clone(&session);

    let bytes = tauri::async_runtime::spawn_blocking(move || {
        session.column_payload(ChunkCoord(x, y, z))
    })
    .await
    .map_err(|error| format!("Generation task failed: {error}"))??;

    Ok(Response::new(bytes.as_ref().clone()))
}

/// Samples all world fields on a grid (see `Session::world_payload`).
#[tauri::command]
async fn sample_world(
    session: State<'_, SharedSession>,
    x: i64,
    y: i64,
    step: u32,
    width: u32,
    height: u32,
) -> Result<Response, String> {
    let session = Arc::clone(&session);

    let bytes = tauri::async_runtime::spawn_blocking(move || {
        session.world_payload((x, y), step, width, height)
    })
    .await
    .map_err(|error| format!("World sampling task failed: {error}"))??;

    Ok(Response::new(bytes))
}

#[tauri::command]
fn world_point(session: State<'_, SharedSession>, x: i64, y: i64) -> Option<WorldSample> {
    session.world_point(x, y)
}

// ---------------------------------------------------------------------------
// Graph

#[tauri::command]
fn pipeline_graph(session: State<'_, SharedSession>) -> PipelineGraph {
    session.pipeline_graph()
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AddedNode {
    id: String,
    graph: PipelineGraph,
}

/// Adds a built-in node, or a script node when `kind` is `script`.
#[tauri::command]
async fn add_node(
    session: State<'_, SharedSession>,
    file: State<'_, GraphFile>,
    kind: String,
    x: f64,
    y: f64,
    source: Option<String>,
) -> Result<AddedNode, String> {
    edit(&session, &file, move |s| {
        s.add_node(&kind, (x, y), source)
            .map(|(id, graph)| AddedNode { id, graph })
    })
    .await
}

#[tauri::command]
async fn remove_node(
    session: State<'_, SharedSession>,
    file: State<'_, GraphFile>,
    id: String,
) -> Result<PipelineGraph, String> {
    edit(&session, &file, move |s| s.remove_node(&id)).await
}

/// Deletes nodes and/or frames (ids) as one undoable step.
#[tauri::command]
async fn remove_items(
    session: State<'_, SharedSession>,
    file: State<'_, GraphFile>,
    ids: Vec<String>,
) -> Result<PipelineGraph, String> {
    edit(&session, &file, move |s| s.remove_items(&ids)).await
}

/// Cuts several inputs' edges as one undoable step.
#[tauri::command]
async fn disconnect_many(
    session: State<'_, SharedSession>,
    file: State<'_, GraphFile>,
    inputs: Vec<SocketRef>,
) -> Result<PipelineGraph, String> {
    edit(&session, &file, move |s| s.disconnect_many(&inputs)).await
}

/// Splits a connection with a reroute node (one undoable step).
#[tauri::command]
async fn insert_reroute(
    session: State<'_, SharedSession>,
    file: State<'_, GraphFile>,
    from: SocketRef,
    to: SocketRef,
    x: f64,
    y: f64,
) -> Result<AddedNode, String> {
    edit(&session, &file, move |s| {
        s.insert_reroute(from, to, (x, y))
            .map(|(id, graph)| AddedNode { id, graph })
    })
    .await
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AddedFrame {
    id: String,
    graph: PipelineGraph,
}

#[tauri::command]
async fn add_frame(
    session: State<'_, SharedSession>,
    file: State<'_, GraphFile>,
    x: f64,
    y: f64,
    width: f64,
    height: f64,
) -> Result<AddedFrame, String> {
    edit(&session, &file, move |s| {
        s.add_frame((x, y), (width, height))
            .map(|(id, graph)| AddedFrame { id, graph })
    })
    .await
}

/// Frame label, position and/or size, as one undoable step.
#[tauri::command]
async fn update_frame(
    session: State<'_, SharedSession>,
    file: State<'_, GraphFile>,
    id: String,
    label: Option<String>,
    x: Option<f64>,
    y: Option<f64>,
    width: Option<f64>,
    height: Option<f64>,
) -> Result<PipelineGraph, String> {
    let position = x.zip(y);
    let size = width.zip(height);
    edit(&session, &file, move |s| s.update_frame(&id, label, position, size)).await
}

#[tauri::command]
async fn set_node_enabled(
    session: State<'_, SharedSession>,
    file: State<'_, GraphFile>,
    id: String,
    enabled: bool,
) -> Result<PipelineGraph, String> {
    edit(&session, &file, move |s| s.set_enabled(&id, enabled)).await
}

/// Node title; for Export nodes this is the export name game code reads.
#[tauri::command]
async fn set_label(
    session: State<'_, SharedSession>,
    file: State<'_, GraphFile>,
    id: String,
    label: String,
) -> Result<PipelineGraph, String> {
    edit(&session, &file, move |s| s.set_label(&id, &label)).await
}

#[tauri::command]
async fn set_param(
    session: State<'_, SharedSession>,
    file: State<'_, GraphFile>,
    id: String,
    name: String,
    value: f64,
) -> Result<PipelineGraph, String> {
    edit(&session, &file, move |s| s.set_param(&id, &name, value)).await
}

#[tauri::command]
async fn connect(
    session: State<'_, SharedSession>,
    file: State<'_, GraphFile>,
    from: SocketRef,
    to: SocketRef,
) -> Result<PipelineGraph, String> {
    edit(&session, &file, move |s| s.connect(from, to)).await
}

#[tauri::command]
async fn disconnect(
    session: State<'_, SharedSession>,
    file: State<'_, GraphFile>,
    to: SocketRef,
) -> Result<PipelineGraph, String> {
    edit(&session, &file, move |s| s.disconnect(to)).await
}

#[derive(Deserialize)]
struct NodeMove {
    id: String,
    x: f64,
    y: f64,
}

/// Layout only: no new revision, but autosaved.
#[tauri::command]
fn move_nodes(session: State<'_, SharedSession>, file: State<'_, GraphFile>, moves: Vec<NodeMove>) {
    let moves: Vec<_> = moves.into_iter().map(|m| (m.id, (m.x, m.y))).collect();
    session.move_nodes(&moves);
    save(&session, &file);
}

/// Replaces a script node's source; rejected (pipeline unchanged) if it
/// fails to parse, compile, or run on the validation chunk.
#[tauri::command]
async fn update_script(
    session: State<'_, SharedSession>,
    file: State<'_, GraphFile>,
    id: String,
    source: String,
) -> Result<PipelineGraph, String> {
    edit(&session, &file, move |s| s.update_script(&id, &source)).await
}

#[tauri::command]
fn script_source(session: State<'_, SharedSession>, id: String) -> Result<String, String> {
    session.script_source(&id)
}

/// Starting text for a new script: `"blank"` or the default worked example.
#[tauri::command]
fn script_template(kind: Option<String>) -> &'static str {
    match kind.as_deref() {
        Some("blank") => worldgraph::script::BLANK,
        _ => worldgraph::script::TEMPLATE,
    }
}

#[derive(Serialize)]
struct Timings {
    revision: u64,
    steps: Vec<StepTiming>,
}

#[tauri::command]
fn step_timings(session: State<'_, SharedSession>) -> Timings {
    let (revision, steps) = session.step_timings();
    Timings { revision, steps }
}

// ---------------------------------------------------------------------------
// Project files (menu bar)

fn lock_project(project: &SharedProject) -> std::sync::MutexGuard<'_, Project> {
    project.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn describe(session: &Session, project: &SharedProject) -> ProjectState {
    lock_project(project).state(&session.graph_doc(), session.can_undo(), session.can_redo())
}

fn confirm_discard(window: &WebviewWindow, name: &str) -> bool {
    window
        .dialog()
        .message(format!("\"{name}\" has unsaved changes. Discard them?"))
        .title("Unsaved changes")
        .kind(MessageDialogKind::Warning)
        .parent(window)
        .buttons(MessageDialogButtons::OkCancelCustom(
            "Discard changes".to_owned(),
            "Cancel".to_owned(),
        ))
        .blocking_show()
}

fn pick_open(window: &WebviewWindow) -> Result<Option<PathBuf>, String> {
    window
        .dialog()
        .file()
        .set_parent(window)
        .set_title("Open graph")
        .add_filter("LayerVis graph (.json)", &["json"])
        .blocking_pick_file()
        .map(|file| file.into_path().map_err(|error| error.to_string()))
        .transpose()
}

fn pick_save(window: &WebviewWindow, title: &str, name: &str) -> Result<Option<PathBuf>, String> {
    window
        .dialog()
        .file()
        .set_parent(window)
        .set_title(title)
        .set_file_name(format!("{name}.json"))
        .add_filter("LayerVis graph (.json)", &["json"])
        .blocking_save_file()
        .map(|file| file.into_path().map_err(|error| error.to_string()))
        .transpose()
}

/// Runs blocking dialog/file work off the async runtime's threads.
async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, String> + Send + 'static,
) -> Result<T, String> {
    tauri::async_runtime::spawn_blocking(work)
        .await
        .map_err(|error| format!("Task failed: {error}"))?
}

/// Asks before throwing away unsaved changes. False = the user cancelled.
fn may_discard(window: &WebviewWindow) -> bool {
    let app = window.app_handle();
    let session = app.state::<SharedSession>();
    let project = app.state::<SharedProject>();
    let (name, dirty) = {
        let project = lock_project(&project);
        (project.name(), project.is_dirty(&session.graph_doc()))
    };

    !dirty || confirm_discard(window, &name)
}

/// Replaces the open graph and records it as the project (or untitled).
fn load_into_session(
    window: &WebviewWindow,
    doc: GraphDoc,
    finish: impl FnOnce(&mut Project, GraphDoc),
) -> Result<ProjectState, String> {
    let app = window.app_handle();
    let session = app.state::<SharedSession>();
    let project = app.state::<SharedProject>();

    session.replace_doc(doc.clone())?;
    {
        let mut project = lock_project(&project);
        finish(&mut project, doc);
        project.persist(&app.state::<AppDir>().0);
    }
    save(&session, &app.state::<GraphFile>());

    Ok(describe(&session, &project))
}

fn do_new(window: &WebviewWindow) -> Result<Option<ProjectState>, String> {
    if !may_discard(window) {
        return Ok(None);
    }

    load_into_session(window, worldgraph::steps::default_graph(), |project, _| {
        project.mark_new()
    })
    .map(Some)
}

fn do_open(window: &WebviewWindow, path: Option<PathBuf>) -> Result<Option<ProjectState>, String> {
    let path = match path {
        Some(path) => path,
        None => match pick_open(window)? {
            Some(path) => path,
            None => return Ok(None),
        },
    };

    let app = window.app_handle();
    let doc = match project::read_doc(&path) {
        Ok(doc) => doc,
        Err(error) => {
            let project = app.state::<SharedProject>();
            let mut project = lock_project(&project);
            project.forget_recent(&path);
            project.persist(&app.state::<AppDir>().0);
            return Err(error);
        }
    };

    if !may_discard(window) {
        return Ok(None);
    }

    load_into_session(window, doc, |project, doc| project.mark_opened(path, doc)).map(Some)
}

#[derive(Clone, Copy, PartialEq)]
enum SaveMode {
    /// Write to the project file (asks for one when untitled).
    Save,
    SaveAs,
    /// Write a copy; the open project stays as it is.
    ExportCopy,
}

fn do_save(window: &WebviewWindow, mode: SaveMode) -> Result<Option<ProjectState>, String> {
    let app = window.app_handle();
    let session = app.state::<SharedSession>();
    let project = app.state::<SharedProject>();
    let doc = session.graph_doc();
    let (current, name) = {
        let project = lock_project(&project);
        (project.path.clone(), project.name())
    };

    let target = match (mode, current) {
        (SaveMode::Save, Some(path)) => path,
        (mode, _) => {
            let title = if mode == SaveMode::ExportCopy { "Export a copy" } else { "Save graph as" };
            match pick_save(window, title, &name)? {
                Some(path) => path,
                None => return Ok(None),
            }
        }
    };

    project::write_doc(&target, &doc)?;

    if mode != SaveMode::ExportCopy {
        let mut project = lock_project(&project);
        project.mark_saved(target, doc);
        project.persist(&app.state::<AppDir>().0);
    }

    Ok(Some(describe(&session, &project)))
}

/// Current project (also refreshes the window title).
#[tauri::command]
fn project_state(
    window: WebviewWindow,
    session: State<'_, SharedSession>,
    project: State<'_, SharedProject>,
) -> ProjectState {
    let state = describe(&session, &project);
    let marker = if state.dirty { "*" } else { "" };
    let _ = window.set_title(&format!("{}{marker} — LayerVis", state.name));
    state
}

/// Starts an untitled project from the default graph. `None` = cancelled.
#[tauri::command]
async fn new_project(window: WebviewWindow) -> Result<Option<ProjectState>, String> {
    blocking(move || do_new(&window)).await
}

/// Opens a project file: `path`, or one picked in a dialog. `None` = cancelled.
#[tauri::command]
async fn open_project(
    window: WebviewWindow,
    path: Option<String>,
) -> Result<Option<ProjectState>, String> {
    blocking(move || do_open(&window, path.map(PathBuf::from))).await
}

#[tauri::command]
async fn save_project(window: WebviewWindow) -> Result<Option<ProjectState>, String> {
    blocking(move || do_save(&window, SaveMode::Save)).await
}

#[tauri::command]
async fn save_project_as(window: WebviewWindow) -> Result<Option<ProjectState>, String> {
    blocking(move || do_save(&window, SaveMode::SaveAs)).await
}

#[tauri::command]
async fn export_copy(window: WebviewWindow) -> Result<Option<ProjectState>, String> {
    blocking(move || do_save(&window, SaveMode::ExportCopy)).await
}

/// Sets the world seed (a decimal string: u64 does not fit a JS number).
#[tauri::command]
async fn set_seed(
    session: State<'_, SharedSession>,
    file: State<'_, GraphFile>,
    seed: String,
) -> Result<PipelineGraph, String> {
    let seed: u64 = seed
        .trim()
        .parse()
        .map_err(|_| format!("`{}` is not a whole number from 0 to {}", seed.trim(), u64::MAX))?;

    edit(&session, &file, move |s| s.set_seed(seed)).await
}

#[tauri::command]
async fn undo(
    session: State<'_, SharedSession>,
    file: State<'_, GraphFile>,
) -> Result<PipelineGraph, String> {
    edit(&session, &file, |s| s.undo()).await
}

#[tauri::command]
async fn redo(
    session: State<'_, SharedSession>,
    file: State<'_, GraphFile>,
) -> Result<PipelineGraph, String> {
    edit(&session, &file, |s| s.redo()).await
}

#[tauri::command]
fn graph_file(file: State<'_, GraphFile>) -> String {
    file.0.display().to_string()
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .setup(|app| {
            let dir = app.path().app_data_dir()?;
            fs::create_dir_all(&dir)?;
            let path = dir.join(GRAPH_FILE);

            app.manage(Arc::new(load_session(&path)) as SharedSession);
            app.manage(GraphFile(path));
            app.manage(Mutex::new(Project::restore(&dir)) as SharedProject);
            app.manage(AppDir(dir));
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            world_info,
            cache_stats,
            generate_chunk,
            sample_world,
            world_point,
            pipeline_graph,
            add_node,
            remove_node,
            remove_items,
            insert_reroute,
            disconnect_many,
            add_frame,
            update_frame,
            set_node_enabled,
            set_label,
            set_param,
            connect,
            disconnect,
            move_nodes,
            update_script,
            script_source,
            script_template,
            step_timings,
            graph_file,
            project_state,
            new_project,
            open_project,
            save_project,
            save_project_as,
            export_copy,
            set_seed,
            undo,
            redo
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
