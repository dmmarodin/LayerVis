mod generation;

use std::{
    fs,
    path::{Path, PathBuf},
    sync::Arc,
};

use generation::{CacheStats, PipelineGraph, Session, StepTiming, WorldInfo, WorldSample};
use worldgraph::{GraphDoc, SocketRef};
use layergen::ChunkCoord;
use serde::{Deserialize, Serialize};
use tauri::{ipc::Response, Manager, State};

/// World seed for the session (world stage is built once per seed).
const SEED: u64 = 12345;
const GRAPH_FILE: &str = "graph.json";
const INVALID_GRAPH_FILE: &str = "graph.invalid.json";

type SharedSession = Arc<Session>;

/// Where the graph document is autosaved.
struct GraphFile(PathBuf);

// ---------------------------------------------------------------------------
// Persistence

/// Loads the saved graph, or the default graph when there is none. An
/// unreadable file is kept aside (never overwritten) and reported.
fn load_session(path: &Path) -> Session {
    let Ok(text) = fs::read_to_string(path) else {
        return Session::new(SEED).expect("default graph builds");
    };

    match GraphDoc::from_json(&text).and_then(|doc| Session::with_doc(SEED, doc)) {
        Ok(session) => session,
        Err(error) => {
            let aside = path.with_file_name(INVALID_GRAPH_FILE);
            eprintln!(
                "LayerVis: could not load {} ({error}); kept it as {} and started a default graph",
                path.display(),
                aside.display()
            );
            let _ = fs::rename(path, aside);
            Session::new(SEED).expect("default graph builds")
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

#[tauri::command]
fn graph_file(file: State<'_, GraphFile>) -> String {
    file.0.display().to_string()
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .setup(|app| {
            let dir = app.path().app_data_dir()?;
            fs::create_dir_all(&dir)?;
            let path = dir.join(GRAPH_FILE);

            app.manage(Arc::new(load_session(&path)) as SharedSession);
            app.manage(GraphFile(path));
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
            graph_file
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
