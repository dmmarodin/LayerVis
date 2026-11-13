<script lang="ts">
	import { onMount, onDestroy } from 'svelte';
	import Sidebar from '$lib/components/Sidebar.svelte';
	import PipelineGraph from '$lib/components/graph/PipelineGraph.svelte';
	import ScriptEditor from '$lib/components/ScriptEditor.svelte';
	import { Visualizer, type HoverInfo, type VisualizerCallbacks } from '$lib/visualizer/visualizer';
	import type { ViewMode } from '$lib/visualizer/fieldMaterial';
	import { MAX_VISIBLE_CHUNKS, type StreamStats } from '$lib/visualizer/streaming';
	import type { OverlayStats } from '$lib/visualizer/overlay';
	import {
		addNode,
		connect,
		disconnect,
		graphFile,
		moveNodes,
		pipelineGraph,
		removeNode,
		scriptSource,
		scriptTemplate,
		setNodeEnabled,
		setLabel,
		setParam,
		updateScript,
		worldInfo,
		type PipelineGraph as Graph,
		type SocketRef,
		type WorldInfo
	} from '$lib/bridge';

	let canvasContainer: HTMLDivElement | null = null;
	let visualizer: Visualizer | null = null;
	let world: WorldInfo | null = null;
	let graph: Graph | null = null;
	let savedTo: string | null = null;
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
			[world, graph, savedTo] = await Promise.all([worldInfo(), pipelineGraph(), graphFile()]);
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
	}

	/** Runs several edits in sequence as one user action. */
	async function sequence<T>(items: T[], edit: (item: T) => Promise<Graph>): Promise<Graph> {
		let last: Graph | null = null;
		for (const item of items) last = await edit(item);
		return last ?? (await pipelineGraph());
	}

	const graphActions = {
		onToggle: (id: string, enabled: boolean) => editGraph(() => setNodeEnabled(id, enabled)),
		onRemove: (ids: string[]) => editGraph(() => sequence(ids, removeNode)),
		onAdd: (kind: string, x: number, y: number) =>
			editGraph(async () => (await addNode(kind, x, y)).graph),
		onConnect: (from: SocketRef, to: SocketRef) => editGraph(() => connect(from, to)),
		onDisconnect: (inputs: SocketRef[]) => editGraph(() => sequence(inputs, disconnect)),
		onParam: (id: string, name: string, value: number) =>
			editGraph(() => setParam(id, name, value)),
		onLabel: (id: string, label: string) => editGraph(() => setLabel(id, label)),
		// Layout only: no new revision, no chunk reload.
		onMove: (moves: Array<{ id: string; x: number; y: number }>) => {
			if (moves.length) moveNodes(moves).catch((error) => (graphError = String(error)));
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

	$: showGraph, onCanvasResize();
</script>

<div class="app-container">
	<Sidebar
		{world}
		{hover}
		{focusField}
		{stageFields}
		statusLines={[chunkStatus, worldStatus].filter(Boolean)}
		{onViewChange}
	/>

	<div class="main">
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
			<div class="graph-container">
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
	.app-container {
		display: flex;
		height: 100%;
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
		height: 320px;
		flex-shrink: 0;
	}
</style>
