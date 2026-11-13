import { invoke } from '@tauri-apps/api/core';

export type FieldScope = 'voxel' | 'column' | 'world';

export interface FieldDef {
	/** Unique id: `node.socket` for node outputs, the name for world fields. */
	key: string;
	/** Producing node id / label (empty for world fields). */
	node: string;
	nodeLabel: string;
	/** Socket name. */
	name: string;
	scope: FieldScope;
	slot: number;
	unit: string;
	min: number;
	max: number;
	ramp: string;
}

export interface WorldInfo {
	origin: [number, number, number];
	dimensions: [number, number, number];
	chunkSize: [number, number, number];
	/** Decimal string (u64). */
	seed: string;
	/** Pipeline revision of this schema; chunk payloads carry it too. */
	revision: number;
	/** Order matches the field planes in chunk payloads. */
	columnFields: FieldDef[];
	voxelFields: FieldDef[];
	/** Order matches the field planes in world payloads. */
	worldFields: FieldDef[];
	/** Game-facing named outputs (Export nodes). */
	exports: Export[];
	plateCount: number;
}

/** Regular grid of field samples: sample (i, j) is at (originX + i*step, originY + j*step). */
export interface FieldGrid {
	originX: number;
	originY: number;
	/** Cells per sample (1 for chunk columns). */
	step: number;
	/** Pipeline revision (low 32 bits) for chunk payloads; 0 for world grids. */
	revision: number;
	width: number;
	height: number;
	/** One plane per field, row-major (i + j * width). NaN = outside world. */
	fields: Float32Array[];
}

export type NodeStatus = 'active' | 'disabled' | 'blocked';
/** Socket value types; connections require equal types. */
export type SocketScope = 'column' | 'voxel';

export interface SocketRef {
	node: string;
	socket: string;
}

export interface ParamReport {
	name: string;
	default: number;
	min: number;
	max: number;
	value: number;
}

export interface NodeReport {
	id: string;
	kind: string;
	/** User label if set, else the kind's label. Export nodes: the export name. */
	label: string;
	kindLabel: string;
	/** Export node: publishes its input under `label` for game code. */
	export: boolean;
	description: string;
	enabled: boolean;
	status: NodeStatus;
	reason: string | null;
	/** Execution position for active nodes. */
	order: number | null;
	inputs: Array<{ name: string; scope: SocketScope }>;
	outputs: Array<{ name: string; scope: SocketScope }>;
	params: ParamReport[];
	worldReads: string[];
	/** Rhai script node (editable source). */
	script: boolean;
	/** Set on active nodes when the graph has a neighborhood node. */
	stage: 'prefix' | 'neighborhood' | 'suffix' | null;
	/** Per-axis radius of a neighborhood (cross-chunk) node. */
	neighborhood: [number, number, number] | null;
	/** Editor layout (flow coordinates). */
	position: [number, number];
}

export interface EdgeReport {
	from: SocketRef;
	to: SocketRef;
	scope: SocketScope | null;
	/** False for dangling or mistyped edges kept from a saved file. */
	valid: boolean;
}

/** Game-facing named output (Export node). */
export interface Export {
	name: string;
	/** Export node id. */
	node: string;
	scope: SocketScope;
	slot: number;
	/** Key (`node.socket`) of the exported field. */
	field: string;
}

/** Labelled box drawn behind nodes (layout only; never affects generation). */
export interface Frame {
	id: string;
	label: string;
	/** Top-left corner in graph units. */
	position: [number, number];
	size: [number, number];
}

export interface PipelineGraph {
	revision: number;
	nodes: NodeReport[];
	frames: Frame[];
	exports: Export[];
	edges: EdgeReport[];
	/** Display-only: world model → node that queries it. */
	worldEdges: Array<{ model: string; to: string }>;
	/** Built-in node kinds that can be added. */
	kinds: Array<{ id: string; label: string; description: string }>;
	worldModels: string[];
}

export const pipelineGraph = () => invoke<PipelineGraph>('pipeline_graph');

/** Adds a node; `kind: 'script'` needs `source`. Inputs auto-connect by name. */
export const addNode = (kind: string, x: number, y: number, source: string | null = null) =>
	invoke<{ id: string; graph: PipelineGraph }>('add_node', { kind, x, y, source });
export const removeNode = (id: string) => invoke<PipelineGraph>('remove_node', { id });
/** Deletes nodes and/or frames as one undo step. */
export const removeItems = (ids: string[]) => invoke<PipelineGraph>('remove_items', { ids });
/** Cuts several inputs' wires as one undo step. */
export const disconnectMany = (inputs: SocketRef[]) =>
	invoke<PipelineGraph>('disconnect_many', { inputs });
/** Splits a wire with a reroute node. */
export const insertReroute = (from: SocketRef, to: SocketRef, x: number, y: number) =>
	invoke<{ id: string; graph: PipelineGraph }>('insert_reroute', { from, to, x, y });
export const addFrame = (x: number, y: number, width: number, height: number) =>
	invoke<{ id: string; graph: PipelineGraph }>('add_frame', { x, y, width, height });
/** Frame label, position and/or size as one undo step (position/size need both parts). */
export const updateFrame = (
	id: string,
	changes: { label?: string; x?: number; y?: number; width?: number; height?: number }
) =>
	invoke<PipelineGraph>('update_frame', {
		id,
		label: changes.label ?? null,
		x: changes.x ?? null,
		y: changes.y ?? null,
		width: changes.width ?? null,
		height: changes.height ?? null
	});
export const setNodeEnabled = (id: string, enabled: boolean) =>
	invoke<PipelineGraph>('set_node_enabled', { id, enabled });
/** Node title; for Export nodes the export name game code reads. Empty clears. */
export const setLabel = (id: string, label: string) =>
	invoke<PipelineGraph>('set_label', { id, label });
export const setParam = (id: string, name: string, value: number) =>
	invoke<PipelineGraph>('set_param', { id, name, value });
export const connect = (from: SocketRef, to: SocketRef) =>
	invoke<PipelineGraph>('connect', { from, to });
export const disconnect = (to: SocketRef) => invoke<PipelineGraph>('disconnect', { to });
/** Layout only (no new revision); autosaved. */
export const moveNodes = (moves: Array<{ id: string; x: number; y: number }>) =>
	invoke<void>('move_nodes', { moves });
/** Path of the autosaved graph document. */
export const graphFile = () => invoke<string>('graph_file');

export interface ProjectState {
	/** Project file, or null for a graph never saved to a file. */
	path: string | null;
	name: string;
	/** Differs from what was last saved/opened. */
	dirty: boolean;
	recent: string[];
	canUndo: boolean;
	canRedo: boolean;
}
export const projectState = () => invoke<ProjectState>('project_state');
/** Project file commands resolve to null when the user cancels a dialog. */
export const newProject = () => invoke<ProjectState | null>('new_project');
export const openProject = (path: string | null = null) =>
	invoke<ProjectState | null>('open_project', { path });
export const saveProject = () => invoke<ProjectState | null>('save_project');
export const saveProjectAs = () => invoke<ProjectState | null>('save_project_as');
export const exportCopy = () => invoke<ProjectState | null>('export_copy');
/** Seeds are decimal strings: u64 does not fit a JS number exactly. */
export const setSeed = (seed: string) => invoke<PipelineGraph>('set_seed', { seed });
export const undo = () => invoke<PipelineGraph>('undo');
export const redo = () => invoke<PipelineGraph>('redo');

/** Replaces a script node's source; rejects with the error text. */
export const updateScript = (id: string, source: string) =>
	invoke<PipelineGraph>('update_script', { id, source });
export const scriptSource = (id: string) => invoke<string>('script_source', { id });
export type ScriptTemplate = 'example' | 'blank';
export const scriptTemplate = (kind: ScriptTemplate = 'example') =>
	invoke<string>('script_template', { kind });

export interface StepTimings {
	revision: number;
	/** Execution order; only active nodes (ids). Cache hits do not run nodes. */
	steps: Array<{ id: string; runs: number; meanMs: number }>;
}
export const stepTimings = () => invoke<StepTimings>('step_timings');

export interface WorldSample {
	plate: number;
	boundaryDistance: number;
	convergence: number;
	elevation: number;
}

const PAYLOAD_VERSION = 2;
const HEADER_BYTES = 40;

export function worldInfo(): Promise<WorldInfo> {
	return invoke<WorldInfo>('world_info');
}

export async function generateChunk(x: number, y: number, z = 0): Promise<FieldGrid> {
	return parseFieldGrid(await invoke<ArrayBuffer>('generate_chunk', { x, y, z }));
}

export async function sampleWorld(
	x: number,
	y: number,
	step: number,
	width: number,
	height: number
): Promise<FieldGrid> {
	return parseFieldGrid(
		await invoke<ArrayBuffer>('sample_world', { x, y, step, width, height })
	);
}

export function worldPoint(x: number, y: number): Promise<WorldSample | null> {
	return invoke<WorldSample | null>('world_point', { x, y });
}

/** See `encode_planes` in src-tauri/src/generation/mod.rs. */
export function parseFieldGrid(buffer: ArrayBuffer): FieldGrid {
	const view = new DataView(buffer);
	const version = view.getUint32(0, true);

	if (version !== PAYLOAD_VERSION) throw new Error(`Unsupported payload version ${version}`);

	const width = view.getUint32(4, true);
	const height = view.getUint32(8, true);
	const fieldCount = view.getUint32(12, true);
	const originX = Number(view.getBigInt64(16, true));
	const originY = Number(view.getBigInt64(24, true));
	const step = view.getUint32(32, true);
	const revision = view.getUint32(36, true);
	const plane = width * height;

	const expected = HEADER_BYTES + fieldCount * plane * 4;
	if (buffer.byteLength !== expected) {
		throw new Error(`Payload is ${buffer.byteLength} bytes, expected ${expected}`);
	}

	const fields: Float32Array[] = [];
	for (let i = 0; i < fieldCount; i++) {
		fields.push(new Float32Array(buffer, HEADER_BYTES + i * plane * 4, plane));
	}

	return { originX, originY, step, revision, width, height, fields };
}
