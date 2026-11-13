<script lang="ts">
	import { onMount, onDestroy } from 'svelte';
	import Sidebar from '$lib/components/Sidebar.svelte';
	import PipelineGraph from '$lib/components/graph/PipelineGraph.svelte';
	import ScriptEditor from '$lib/components/ScriptEditor.svelte';
	import MenuBar from '$lib/components/MenuBar.svelte';
	import { Visualizer, type HoverInfo, type VisualizerCallbacks } from '$lib/visualizer/visualizer';
	import type { ViewMode } from '$lib/visualizer/fieldMaterial';
	import { MAX_VISIBLE_CHUNKS, type StreamStats } from '$lib/visualizer/streaming';
	import type { OverlayStats } from '$lib/visualizer/overlay';
	import {
		addNode,
		connect,
		addFrame,
		disconnectMany,
		exportCopy,
		insertReroute,
		graphFile,
		moveNodes,
		newProject,
		openProject,
		pipelineGraph,
		projectState,
		redo,
		removeItems,
		saveProject,
		saveProjectAs,
		setSeed,
		undo,
		updateFrame,
		scriptSource,
		scriptTemplate,
		setNodeEnabled,
		setLabel,
		setParam,
		updateScript,
		worldInfo,
		type PipelineGraph as Graph,
		type ProjectState,
		type SocketRef,
		type WorldInfo
	} from '$lib/bridge';

	let canvasContainer: HTMLDivElement | null = null;
	let visualizer: Visualizer | null = null;
	let world: WorldInfo | null = null;
	let graph: Graph | null = null;
	let savedTo: string | null = null;
	let project: ProjectState | null = null;
	let graphBusy = false;
	let graphError: string | null = null;
	let showGraph = true;
	/** `nodeId` null = new script node placed at `position`. */
	let editor: { source: string; nodeId: string | null; position: [number, number] } | null = null;
	let selectedStep: string | null = null;
	let focusField: { key: string; nonce: number } | null = null;

	// Mid-pipeline view: outputs of active nodes up to the selected one.
	// A selection whose node stopped running (edited away) shows the final state.
	$: selected = graph?.nodes.find((n) => n.id === selectedStep && n.status === 'active') ?? null;
	$: selectedOrder = selected?.order ?? null;
	$: stageFields =
		selectedOrder === null
			? null
			: (graph?.nodes ?? [])
					.filter((n) => n.status === 'active' && n.order !== null && n.order <= selectedOrder)
					.flatMap((n) => n.outputs.map((output) => `${n.id}.${output.name}`));
	$: visualizer?.setStage(stageFields);

	function selectStep(id: string | null) {
		selectedStep = id;
		const node = graph?.nodes.find((n) => n.id === id);
		const output = node?.outputs.find((o) => o.scope === 'column');
		// Show what the selected node produced.
		if (node && output) {
			focusField = { key: `${node.id}.${output.name}`, nonce: (focusField?.nonce ?? 0) + 1 };
		}
	}

	let editorBusy = false;
	let editorError: string | null = null;
	let hover: HoverInfo | null = null;
	let chunkStatus = 'Connecting to backend…';
	let worldStatus = '';

	const callbacks: VisualizerCallbacks = {
		onStream: (stats) => (chunkStatus = describeChunks(stats)),
		onOverlay: (stats) => (worldStatus = describeWorld(stats)),
		onHover: (info) => (hover = info)
	};

	const onResize = () => visualizer?.resize();

	onMount(async () => {
		if (!canvasContainer) return;

		visualizer = new Visualizer(canvasContainer);
		window.addEventListener('resize', onResize);

		try {
			[world, graph, savedTo, project] = await Promise.all([
				worldInfo(),
				pipelineGraph(),
				graphFile(),
				projectState()
			]);
			visualizer.start(world, callbacks);
		} catch (error) {
			chunkStatus = `Backend unavailable: ${error}`;
		}
	});

	onDestroy(() => {
		if (typeof window !== 'undefined') window.removeEventListener('resize', onResize);
	});

	/** Applies a graph edit, then reloads the schema and restarts chunk streaming. */
	async function editGraph(edit: () => Promise<Graph>) {
		graphBusy = true;
		graphError = null;

		try {
			await applyEdit(edit);
		} catch (error) {
			graphError = String(error);
			// Re-sync the view (e.g. drop a locally deleted edge the backend kept).
			graph = await pipelineGraph().catch(() => graph);
		} finally {
			graphBusy = false;
		}
	}

	async function applyEdit(edit: () => Promise<Graph>) {
		graph = await edit();
		if (!graph.nodes.some((n) => n.id === selectedStep && n.status === 'active')) {
			selectedStep = null;
		}
		world = await worldInfo();
		visualizer?.start(world, callbacks);
		await refreshProject();
	}

	/** Re-reads dirty / undo state (also updates the window title). */
	async function refreshProject() {
		project = await projectState().catch(() => project);
	}

	/**
	 * New / Open: swaps the whole graph, so selection and the script editor reset.
	 * A cancelled dialog (null) changes nothing.
	 */
	async function replaceProject(run: () => Promise<ProjectState | null>) {
		if (graphBusy) return;
		graphBusy = true;
		graphError = null;

		try {
			const next = await run();
			if (next) {
				selectedStep = null;
				editor = null;
				await applyEdit(pipelineGraph);
			}
		} catch (error) {
			graphError = String(error);
			await refreshProject();
		} finally {
			graphBusy = false;
		}
	}

	/** Save / Save as / Export: the graph itself is unchanged. */
	async function writeProject(run: () => Promise<ProjectState | null>) {
		if (graphBusy) return;
		graphBusy = true;
		graphError = null;

		try {
			project = (await run()) ?? project;
		} catch (error) {
			graphError = String(error);
		} finally {
			graphBusy = false;
		}
	}

	const menuActions = {
		onNew: () => replaceProject(newProject),
		onOpen: () => replaceProject(() => openProject()),
		onOpenRecent: (path: string) => replaceProject(() => openProject(path)),
		onSave: () => writeProject(saveProject),
		onSaveAs: () => writeProject(saveProjectAs),
		onExport: () => writeProject(exportCopy),
		onUndo: () => {
			if (project?.canUndo) editGraph(undo);
		},
		onRedo: () => {
			if (project?.canRedo) editGraph(redo);
		},
		onToggleGraph: () => (showGraph = !showGraph)
	};

	function onKeyDown(event: KeyboardEvent) {
		if (editor || !(event.ctrlKey || event.metaKey) || event.altKey) return;

		// Text fields keep their own undo.
		const target = event.target as HTMLElement | null;
		const typing = target?.closest('input, textarea, select, [contenteditable]') != null;
		const key = event.key.toLowerCase();

		if (key === 'n') menuActions.onNew();
		else if (key === 'o') menuActions.onOpen();
		else if (key === 's') (event.shiftKey ? menuActions.onSaveAs : menuActions.onSave)();
		else if (typing) return;
		else if (key === 'z') (event.shiftKey ? menuActions.onRedo : menuActions.onUndo)();
		else if (key === 'y') menuActions.onRedo();
		else return;

		event.preventDefault();
	}

	/**
	 * Layout-only edits (frames): update the graph and dirty state without
	 * reloading the world or restarting chunk streaming.
	 */
	async function editLayout(edit: () => Promise<Graph>) {
		graphError = null;
		try {
			graph = await edit();
			await refreshProject();
		} catch (error) {
			graphError = String(error);
			graph = await pipelineGraph().catch(() => graph);
		}
	}

	const graphActions = {
		onToggle: (id: string, enabled: boolean) => editGraph(() => setNodeEnabled(id, enabled)),
		// One undo step per delete; frames-only deletes never touch generation.
		onRemove: (ids: string[]) =>
			ids.every((id) => id.startsWith('f'))
				? editLayout(() => removeItems(ids))
				: editGraph(() => removeItems(ids)),
		onInsertReroute: (from: SocketRef, to: SocketRef, x: number, y: number) =>
			editGraph(async () => (await insertReroute(from, to, x, y)).graph),
		onAddFrame: (x: number, y: number, width: number, height: number) =>
			editLayout(async () => (await addFrame(x, y, width, height)).graph),
		onFrameLabel: (id: string, label: string) => editLayout(() => updateFrame(id, { label })),
		onFrameResize: (id: string, x: number, y: number, width: number, height: number) =>
			editLayout(() => updateFrame(id, { x, y, width, height })),
		onAdd: (kind: string, x: number, y: number) =>
			editGraph(async () => (await addNode(kind, x, y)).graph),
		onConnect: (from: SocketRef, to: SocketRef) => editGraph(() => connect(from, to)),
		onDisconnect: (inputs: SocketRef[]) => editGraph(() => disconnectMany(inputs)),
		onParam: (id: string, name: string, value: number) =>
			editGraph(() => setParam(id, name, value)),
		onLabel: (id: string, label: string) => editGraph(() => setLabel(id, label)),
		// Layout only: no new revision, no chunk reload.
		onMove: (moves: Array<{ id: string; x: number; y: number }>) => {
			if (moves.length) {
				moveNodes(moves)
					.then(refreshProject)
					.catch((error) => (graphError = String(error)));
			}
		}
	};

	async function openEditor(nodeId: string | null, position: [number, number] = [0, 0]) {
		try {
			const source = nodeId ? await scriptSource(nodeId) : await scriptTemplate();
			editorError = null;
			editor = { source, nodeId, position };
		} catch (error) {
			graphError = String(error);
		}
	}

	/** Errors stay in the editor so the script can be fixed in place. */
	async function applyScript(source: string) {
		if (!editor) return;
		const { nodeId, position } = editor;
		editorBusy = true;
		editorError = null;

		try {
			await applyEdit(async () =>
				nodeId
					? updateScript(nodeId, source)
					: (await addNode('script', position[0], position[1], source)).graph
			);
			editor = null;
		} catch (error) {
			editorError = String(error);
		} finally {
			editorBusy = false;
		}
	}

	function describeChunks(stats: StreamStats): string {
		if (!stats.enabled) return `Chunks hidden (world view) · ${stats.loaded} kept`;
		if (stats.tooFar) {
			return `Chunks paused: ${stats.visible} visible > ${MAX_VISIBLE_CHUNKS} · ${stats.loaded} kept`;
		}
		const failed = stats.failed ? ` · ${stats.failed} failed` : '';
		return `Chunks: ${stats.loaded} loaded · ${stats.inFlight} loading · ${stats.queued} queued${failed}`;
	}

	function describeWorld(stats: OverlayStats): string {
		if (stats.error) return `World sampling failed: ${stats.error}`;
		if (stats.loading) return 'World: sampling…';
		return stats.step ? `World: ${stats.step} cells/sample` : '';
	}

	function onViewChange(view: ViewMode) {
		visualizer?.setViewMode(view);
	}

	function onCanvasResize() {
		// No visualizer during server-side rendering (or before mount).
		if (!visualizer) return;
		// Let layout settle before measuring the canvas.
		requestAnimationFrame(() => visualizer?.resize());
	}

	$: showGraph, graphHeight, onCanvasResize();

	// --- Resizable graph panel ----------------------------------------------

	const DEFAULT_GRAPH_HEIGHT = 320;
	const MIN_GRAPH_HEIGHT = 120;
	/** Always leave this much room for the map. */
	const MIN_MAP_HEIGHT = 160;

	let graphHeight = DEFAULT_GRAPH_HEIGHT;
	let mainElement: HTMLDivElement | null = null;
	let resizing = false;

	try {
		const saved = Number(localStorage.getItem('layervis.graphHeight'));
		if (Number.isFinite(saved) && saved >= MIN_GRAPH_HEIGHT) graphHeight = saved;
	} catch {
		// Storage unavailable: use the default.
	}

	const clampGraphHeight = (value: number) => {
		const max = (mainElement?.clientHeight ?? 1000) - MIN_MAP_HEIGHT;
		return Math.round(Math.max(MIN_GRAPH_HEIGHT, Math.min(max, value)));
	};

	function saveGraphHeight() {
		try {
			localStorage.setItem('layervis.graphHeight', String(graphHeight));
		} catch {
			// Not persisting is fine.
		}
	}

	function startResize(event: PointerEvent) {
		resizing = true;
		(event.currentTarget as HTMLElement).setPointerCapture(event.pointerId);
		event.preventDefault();
	}

	function onResizeMove(event: PointerEvent) {
		if (!resizing || !mainElement) return;
		// The panel is anchored to the bottom of the main column.
		graphHeight = clampGraphHeight(mainElement.getBoundingClientRect().bottom - event.clientY);
	}

	function endResize() {
		if (!resizing) return;
		resizing = false;
		saveGraphHeight();
	}

	/** Double-click the bar: toggle between the default and a tall panel. */
	function toggleTall() {
		const tall = clampGraphHeight((mainElement?.clientHeight ?? 800) * 0.7);
		graphHeight = graphHeight >= tall - 4 ? DEFAULT_GRAPH_HEIGHT : tall;
		saveGraphHeight();
	}
</script>

<svelte:window on:keydown={onKeyDown} />

<div class="shell">
<MenuBar
	{project}
	{showGraph}
	busy={graphBusy}
	error={showGraph ? null : graphError}
	onDismissError={() => (graphError = null)}
	{...menuActions}
/>

<div class="app-container">
	<Sidebar
		{world}
		{hover}
		{focusField}
		{stageFields}
		statusLines={[chunkStatus, worldStatus].filter(Boolean)}
		{onViewChange}
		busy={graphBusy}
		onSeed={(seed) => editGraph(() => setSeed(seed))}
		onGoTo={(x, y) => visualizer?.goTo(x, y)}
		getDataRange={() => visualizer?.dataRange() ?? null}
	/>

	<div class="main" bind:this={mainElement}>
		<div bind:this={canvasContainer} class="canvas-container">
			<button class="graph-toggle" on:click={() => (showGraph = !showGraph)}>
				{showGraph ? 'Hide graph' : 'Show graph'}
			</button>

			{#if selected && selectedOrder !== null}
				<div class="stage-banner">
					Viewing after node #{selectedOrder + 1}: <strong>{selected.label}</strong>
					<span class="muted">{selected.id} · later outputs hidden</span>
					<button on:click={() => selectStep(null)}>Show final</button>
				</div>
			{/if}
		</div>

		{#if showGraph}
			<div
				class="splitter"
				class:active={resizing}
				role="separator"
				aria-orientation="horizontal"
				aria-label="Resize node graph (double-click to toggle tall)"
				title="Drag to resize · double-click to toggle tall"
				on:pointerdown={startResize}
				on:pointermove={onResizeMove}
				on:pointerup={endResize}
				on:pointercancel={endResize}
				on:dblclick={toggleTall}
			></div>
			<div class="graph-container" style:height="{graphHeight}px">
				<PipelineGraph
					{graph}
					busy={graphBusy}
					error={graphError}
					{savedTo}
					{selectedStep}
					onSelectStep={selectStep}
					{...graphActions}
					onEditScript={(id) => openEditor(id)}
					onNewScript={(x, y) => openEditor(null, [x, y])}
				/>
			</div>
		{/if}
	</div>
</div>
</div>

{#if editor}
	<ScriptEditor
		source={editor.source}
		replaceId={editor.nodeId}
		busy={editorBusy}
		error={editorError}
		onApply={applyScript}
		onClose={() => (editor = null)}
		fields={world?.columnFields ?? []}
		loadTemplate={scriptTemplate}
	/>
{/if}

<style>
	.shell {
		display: flex;
		flex-direction: column;
		height: 100%;
	}

	.app-container {
		display: flex;
		flex: 1;
		min-height: 0;
	}

	.main {
		flex-grow: 1;
		display: flex;
		flex-direction: column;
		min-width: 0;
	}

	.canvas-container {
		position: relative;
		flex-grow: 1;
		min-height: 0;
		overflow: hidden;
	}

	.graph-toggle {
		position: absolute;
		top: 8px;
		right: 8px;
		z-index: 1;
		background: #2d303dcc;
		color: #c3c6d2;
		border-radius: 4px;
		padding: 2px 10px;
		font-size: 12px;
	}

	.stage-banner {
		position: absolute;
		top: 8px;
		left: 50%;
		transform: translateX(-50%);
		z-index: 1;
		display: flex;
		align-items: center;
		gap: 8px;
		padding: 4px 12px;
		border-radius: 6px;
		border: 1px solid #6fa8ff;
		background: #1b2233e6;
		color: #c3cbe8;
		font-size: 12px;
	}

	.stage-banner .muted {
		color: #8a8fa6;
	}

	.stage-banner button {
		background: #2d303d;
		border-radius: 4px;
		padding: 1px 8px;
	}

	.graph-container {
		flex-shrink: 0;
		min-height: 0;
	}

	.splitter {
		height: 6px;
		flex-shrink: 0;
		cursor: row-resize;
		background: #2d303d;
		touch-action: none;
	}

	.splitter:hover,
	.splitter.active {
		background: #6fa8ff;
	}
</style>
