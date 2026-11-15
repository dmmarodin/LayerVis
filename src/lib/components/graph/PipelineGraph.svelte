<script lang="ts">
	import { untrack } from 'svelte';
	import {
		SvelteFlow,
		Background,
		Controls,
		type Connection,
		type Edge,
		type Node
	} from '@xyflow/svelte';
	import '@xyflow/svelte/dist/style.css';
	import { canConnect, stepTimings, type PipelineGraph, type SocketRef } from '$lib/bridge';
	import StepNode from './StepNode.svelte';
	import FrameNode from './FrameNode.svelte';
	import RerouteNode from './RerouteNode.svelte';

	interface Props {
		graph: PipelineGraph | null;
		busy?: boolean;
		error?: string | null;
		/** Where the graph is autosaved (shown as a hint). */
		savedTo?: string | null;
		/** Node whose output the map shows (mid-pipeline view), or null. */
		selectedStep: string | null;
		onSelectStep: (id: string | null) => void;
		onToggle: (id: string, enabled: boolean) => void;
		onRemove: (ids: string[]) => void;
		onAdd: (kind: string, x: number, y: number) => void;
		onNewScript: (x: number, y: number) => void;
		onEditScript: (id: string) => void;
		onConnect: (from: SocketRef, to: SocketRef) => void;
		onDisconnect: (inputs: SocketRef[]) => void;
		onMove: (moves: Array<{ id: string; x: number; y: number }>) => void;
		onParam: (id: string, name: string, value: number) => void;
		onLabel: (id: string, label: string) => void;
		onInsertReroute: (from: SocketRef, to: SocketRef, x: number, y: number) => void;
		onAddFrame: (x: number, y: number, width: number, height: number) => void;
		onFrameLabel: (id: string, label: string) => void;
		onFrameResize: (id: string, x: number, y: number, width: number, height: number) => void;
	}

	let {
		graph,
		busy = false,
		error = null,
		savedTo = null,
		selectedStep,
		onSelectStep,
		onToggle,
		onRemove,
		onAdd,
		onNewScript,
		onEditScript,
		onConnect,
		onDisconnect,
		onMove,
		onParam,
		onLabel,
		onInsertReroute,
		onAddFrame,
		onFrameLabel,
		onFrameResize
	}: Props = $props();

	const nodeTypes = {
		step: StepNode,
		frame: FrameNode,
		reroute: RerouteNode
	};
	const isReroute = (kind: string) => kind.startsWith('reroute_');

	/** Node kinds by where they run, for the Add node list. */
	const SCOPE_GROUPS = [
		{ scope: 'atlas', label: 'Atlas (whole-world grids, once)' },
		{ scope: 'world', label: 'World (once per world)' },
		{ scope: 'column', label: 'Chunk (per column)' },
		{ scope: 'voxel', label: 'Voxel (per cell)' }
	] as const;
	const TIMING_POLL_MS = 1000;

	let nodes = $state.raw<Node[]>([]);
	let edges = $state.raw<Edge[]>([]);
	let toAdd = $state('');
	let timings = $state<Map<string, number>>(new Map());

	$effect(() => {
		if (!graph) return;
		const [built, builtEdges] = build(graph);

		// Keep the *same objects* for nodes that did not change. Svelte Flow owns
		// per-node state (measured size, handle bounds, selection, drag): replacing
		// every node on each edit left unchanged nodes hidden, and after repeated
		// rebuilds nodes that visually could not be dragged.
		const previous = new Map(untrack(() => nodes).map((node) => [node.id, node]));
		nodes = built.map((node) => {
			const old = previous.get(node.id);
			if (!old || old.type !== node.type) return node;
			if (unchanged(old, node)) return old;
			// Changed (params, status, size…): new data, keep what was measured.
			return { ...node, measured: old.measured, selected: old.selected };
		});
		edges = builtEdges;
	});

	/** Same position and same report/frame content (callbacks and live flags aside). */
	function unchanged(old: Node, next: Node): boolean {
		if (old.position.x !== next.position.x || old.position.y !== next.position.y) return false;
		if (old.width !== next.width || old.height !== next.height) return false;

		const content = (node: Node) =>
			JSON.stringify(node.data.node ?? node.data.frame ?? node.data.model ?? null);
		return content(old) === content(next);
	}

	$effect(() => {
		if (graph && !graph.kinds.some((kind) => kind.id === toAdd)) toAdd = graph.kinds[0]?.id ?? '';
	});

	// Poll per-node timings for the current revision while mounted.
	$effect(() => {
		let stopped = false;
		const poll = async () => {
			try {
				const result = await stepTimings();
				if (stopped) return;
				timings = new Map(result.steps.filter((s) => s.runs > 0).map((s) => [s.id, s.meanMs]));
			} catch {
				// Backend unavailable; keep last values.
			}
		};
		poll();
		const timer = setInterval(poll, TIMING_POLL_MS);
		return () => {
			stopped = true;
			clearInterval(timer);
		};
	});

	// Selection and timings change node data only, never positions.
	$effect(() => {
		const selected = selectedStep;
		const means = timings;
		nodes = untrack(() => nodes).map((node) =>
			node.type === 'step'
				? {
						...node,
						data: { ...node.data, selected: node.id === selected, meanMs: means.get(node.id) ?? null }
					}
				: node.type === 'reroute'
					? { ...node, data: { ...node.data, selected: node.id === selected } }
					: node
		);
	});

	/** Socket type of a node's input or output handle. */
	function scopeOf(nodeId: string, handle: string | null | undefined, output: boolean) {
		const node = graph?.nodes.find((n) => n.id === nodeId);
		const sockets = output ? node?.outputs : node?.inputs;
		return sockets?.find((s) => s.name === handle)?.scope ?? null;
	}

	/** Client-side check while dragging; the backend re-validates (incl. cycles). */
	function isValidConnection(connection: Edge | Connection): boolean {
		if (connection.source === connection.target) return false;
		const from = scopeOf(connection.source, connection.sourceHandle, true);
		const to = scopeOf(connection.target, connection.targetHandle, false);
		return from !== null && to !== null && canConnect(from, to);
	}

	/** Connections apply only once the backend accepts them. */
	function onBeforeConnect(connection: Connection): false {
		if (connection.sourceHandle && connection.targetHandle) {
			onConnect(
				{ node: connection.source, socket: connection.sourceHandle },
				{ node: connection.target, socket: connection.targetHandle }
			);
		}
		return false;
	}

	function onDelete({ nodes: removed, edges: cut }: { nodes: Node[]; edges: Edge[] }) {
		// Nodes, reroutes and frames all go in one request (one undo step).
		const ids = removed
			.filter((n) => n.type === 'step' || n.type === 'reroute' || n.type === 'frame')
			.map((n) => n.id);
		const inputs = cut
			.filter((e) => e.targetHandle && !ids.includes(e.target))
			.map((e) => ({ node: e.target, socket: e.targetHandle! }));

		if (ids.length) onRemove(ids);
		else if (inputs.length) onDisconnect(inputs);
	}

	function onNodeClick({ node }: { node: Node }) {
		const report = graph?.nodes.find((n) => n.id === node.id);
		// Only running nodes have a well-defined "after this node" state.
		if (report?.status === 'active') onSelectStep(report.id === selectedStep ? null : report.id);
	}

	/** Right-click a wire: split it with a reroute at its midpoint. */
	function onEdgeContextMenu({ edge, event }: { edge: Edge; event: MouseEvent }) {
		event.preventDefault();
		if (!edge.sourceHandle || !edge.targetHandle) return;

		const at = (id: string) => graph?.nodes.find((n) => n.id === id)?.position;
		const from = at(edge.source);
		const to = at(edge.target);
		if (!from || !to) return;

		onInsertReroute(
			{ node: edge.source, socket: edge.sourceHandle },
			{ node: edge.target, socket: edge.targetHandle },
			(from[0] + to[0]) / 2 + 100,
			(from[1] + to[1]) / 2 + 20
		);
	}

	/**
	 * Dragging a frame carries the nodes whose center is inside it, live.
	 * (Positions only; the backend gets all of them in one move on drop.)
	 */
	let frameDrag: {
		id: string;
		origin: { x: number; y: number };
		members: Map<string, { x: number; y: number }>;
	} | null = null;

	function onNodeDragStart({ targetNode, nodes: dragged }: { targetNode: Node | null; nodes: Node[] }) {
		frameDrag = null;
		const frame = graph?.frames.find((f) => f.id === targetNode?.id);
		// Several selected items drag together on their own.
		if (!frame || !targetNode || dragged.length !== 1) return;

		const [fx, fy] = [targetNode.position.x, targetNode.position.y];
		const [width, height] = frame.size;
		const members = new Map<string, { x: number; y: number }>();

		for (const node of nodes) {
			if (node.type !== 'step' && node.type !== 'reroute') continue;
			const cx = node.position.x + (node.measured?.width ?? 200) / 2;
			const cy = node.position.y + (node.measured?.height ?? 80) / 2;
			if (cx >= fx && cx <= fx + width && cy >= fy && cy <= fy + height) {
				members.set(node.id, { ...node.position });
			}
		}

		frameDrag = { id: frame.id, origin: { x: fx, y: fy }, members };
	}

	function onNodeDrag({ targetNode }: { targetNode: Node | null }) {
		if (!frameDrag || !targetNode || targetNode.id !== frameDrag.id) return;
		const { origin, members } = frameDrag;
		const [dx, dy] = [targetNode.position.x - origin.x, targetNode.position.y - origin.y];

		nodes = nodes.map((node) => {
			const start = members.get(node.id);
			return start ? { ...node, position: { x: start.x + dx, y: start.y + dy } } : node;
		});
	}

	function onNodeDragStop({ nodes: dragged }: { nodes: Node[] }) {
		const moves = dragged
			.filter((n) => n.type === 'step' || n.type === 'reroute' || n.type === 'frame')
			.map((n) => ({ id: n.id, x: n.position.x, y: n.position.y }));

		if (frameDrag) {
			const { id, members, origin } = frameDrag;
			frameDrag = null;
			const frame = dragged.find((n) => n.id === id);
			if (frame) {
				// Relative to where this drag started (`graph` positions go stale after moves).
				const dx = frame.position.x - origin.x;
				const dy = frame.position.y - origin.y;
				for (const [memberId, start] of members) {
					moves.push({ id: memberId, x: start.x + dx, y: start.y + dy });
				}
			}
		}

		onMove(moves);
	}

	/** New nodes go to the right of everything, near the top. */
	function freeSpot(): [number, number] {
		const steps = graph?.nodes ?? [];
		if (!steps.length) return [0, 0];
		const maxX = Math.max(...steps.map((n) => n.position[0]));
		const minY = Math.min(...steps.map((n) => n.position[1]));
		return [maxX + 280, minY];
	}

	function build(graph: PipelineGraph): [Node[], Edge[]] {
		const nodes: Node[] = [
			...graph.nodes.map((node) => ({
				id: node.id,
				type: isReroute(node.kind) ? 'reroute' : 'step',
				position: { x: node.position[0], y: node.position[1] },
				data: isReroute(node.kind)
					? { node, selected: node.id === untrack(() => selectedStep) }
					: {
					node,
					onToggle,
					onRemove: (id: string) => onRemove([id]),
					onEditScript,
					onParam,
					onLabel,
					selected: node.id === untrack(() => selectedStep),
					meanMs: untrack(() => timings).get(node.id) ?? null
				}
			})),
			// Frames go last in the list (appending keeps every other node's index
			// stable) and render behind via zIndex.
			...graph.frames.map((frame) => ({
				id: frame.id,
				type: 'frame',
				position: { x: frame.position[0], y: frame.position[1] },
				width: frame.size[0],
				height: frame.size[1],
				zIndex: -1,
				dragHandle: '.frame-title',
				data: { frame, onLabel: onFrameLabel, onResize: onFrameResize }
			}))
		];

		const status = new Map(graph.nodes.map((n) => [n.id, n.status]));
		const live = (from: string, to: string) =>
			status.get(from) === 'active' && status.get(to) === 'active';

		const edges: Edge[] = [
			...graph.edges.map((edge) => ({
				id: `${edge.from.node}.${edge.from.socket}->${edge.to.node}.${edge.to.socket}`,
				source: edge.from.node,
				sourceHandle: edge.from.socket,
				target: edge.to.node,
				targetHandle: edge.to.socket,
				animated: edge.valid && live(edge.from.node, edge.to.node),
				class: edge.scope ?? 'invalid',
				style: !edge.valid
					? 'stroke: #c0573f; stroke-dasharray: 2 4;'
					: live(edge.from.node, edge.to.node)
						? undefined
						: 'stroke-dasharray: 4 4; opacity: 0.5;'
			}))
		];

		return [nodes, edges];
	}
</script>

<section class="graph-panel">
	<header>
		<strong>Graph</strong>
		{#if graph}<span class="muted">revision {graph.revision}</span>{/if}
		{#if busy}<span class="muted">applying…</span>{/if}
		{#if error}<span class="error">{error}</span>{/if}

		<span class="spacer"></span>

		{#if savedTo}
			<span class="muted hint" title={savedTo}>autosaved</span>
		{/if}
		<span class="muted hint">drag sockets to connect · right-click a wire to add a reroute · Del removes selection</span>

		{#if graph?.kinds.length}
			<select bind:value={toAdd} disabled={busy}>
				{#each SCOPE_GROUPS as group}
					{@const kinds = graph.kinds.filter((kind) => kind.scope === group.scope)}
					{#if kinds.length}
						<optgroup label={group.label}>
							{#each kinds as kind}
								<option value={kind.id} title={kind.description}>{kind.label}</option>
							{/each}
						</optgroup>
					{/if}
				{/each}
			</select>
			<button disabled={busy || !toAdd} onclick={() => onAdd(toAdd, ...freeSpot())}>
				Add node
			</button>
		{/if}
		<button disabled={busy} onclick={() => onNewScript(...freeSpot())}>New script</button>
		<button disabled={busy} onclick={() => onAddFrame(...freeSpot(), 420, 260)}>Add frame</button>
	</header>

	<div class="flow">
		<SvelteFlow
			bind:nodes
			bind:edges
			{nodeTypes}
			colorMode="dark"
			fitView
			deleteKey={['Delete', 'Backspace']}
			elevateNodesOnSelect={false}
			{isValidConnection}
			onbeforeconnect={onBeforeConnect}
			ondelete={onDelete}
			onnodeclick={onNodeClick}
			onpaneclick={() => onSelectStep(null)}
			onnodedragstart={onNodeDragStart}
			onnodedrag={onNodeDrag}
			onnodedragstop={onNodeDragStop}
			onedgecontextmenu={onEdgeContextMenu}
			proOptions={{ hideAttribution: true }}
		>
			<Background />
			<Controls showLock={false} />
		</SvelteFlow>
	</div>
</section>

<style>
	.graph-panel {
		display: flex;
		flex-direction: column;
		height: 100%;
		background: #1b1d25;
		border-top: 2px solid #2d303d;
		color: #c3c6d2;
		font-size: 13px;
	}

	header {
		display: flex;
		align-items: center;
		gap: 10px;
		padding: 6px 12px;
	}

	.spacer {
		flex: 1;
	}

	.muted {
		color: #8a8fa6;
	}

	.hint {
		font-size: 11px;
	}

	.error {
		color: #e0947f;
	}

	select,
	button {
		background: #2d303d;
		border-radius: 4px;
		padding: 2px 8px;
	}

	button:disabled {
		opacity: 0.5;
	}

	.flow {
		flex: 1;
		min-height: 0;
	}

	.flow :global(.svelte-flow__edge.column path) {
		stroke: #6fa8ff;
	}

	.flow :global(.svelte-flow__edge.world path) {
		stroke: #b58cf0;
	}

	.flow :global(.svelte-flow__edge.atlas path) {
		stroke: #4fd1b5;
	}

	.flow :global(.svelte-flow__edge.voxel path) {
		stroke: #e0a24f;
	}
</style>
