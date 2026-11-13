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
	import { stepTimings, type PipelineGraph, type SocketRef } from '$lib/bridge';
	import StepNode from './StepNode.svelte';
	import WorldNode from './WorldNode.svelte';
	import { WORLD_HANDLE } from './types';

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
		onLabel
	}: Props = $props();

	const nodeTypes = { step: StepNode, world: WorldNode };
	const TIMING_POLL_MS = 1000;

	let nodes = $state.raw<Node[]>([]);
	let edges = $state.raw<Edge[]>([]);
	let toAdd = $state('');
	let timings = $state<Map<string, number>>(new Map());

	$effect(() => {
		if (!graph) return;
		[nodes, edges] = build(graph);
	});

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
		if (connection.targetHandle === WORLD_HANDLE) return false;
		const from = scopeOf(connection.source, connection.sourceHandle, true);
		const to = scopeOf(connection.target, connection.targetHandle, false);
		return from !== null && from === to;
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
		const ids = removed.filter((n) => n.type === 'step').map((n) => n.id);
		const inputs = cut
			.filter((e) => e.targetHandle && e.targetHandle !== WORLD_HANDLE && !ids.includes(e.target))
			.map((e) => ({ node: e.target, socket: e.targetHandle! }));

		if (ids.length) onRemove(ids);
		else if (inputs.length) onDisconnect(inputs);
	}

	function onNodeClick({ node }: { node: Node }) {
		const report = graph?.nodes.find((n) => n.id === node.id);
		// Only running nodes have a well-defined "after this node" state.
		if (report?.status === 'active') onSelectStep(report.id === selectedStep ? null : report.id);
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
		const minX = Math.min(0, ...graph.nodes.map((n) => n.position[0]));
		const minY = Math.min(0, ...graph.nodes.map((n) => n.position[1]));

		const nodes: Node[] = [
			...graph.worldModels.map((model, index) => ({
				id: `world:${model}`,
				type: 'world',
				position: { x: minX - 260, y: minY + index * 80 },
				data: { model },
				deletable: false,
				connectable: false
			})),
			...graph.nodes.map((node) => ({
				id: node.id,
				type: 'step',
				position: { x: node.position[0], y: node.position[1] },
				data: {
					node,
					onToggle,
					onRemove: (id: string) => onRemove([id]),
					onEditScript,
					onParam,
					onLabel,
					selected: node.id === untrack(() => selectedStep),
					meanMs: untrack(() => timings).get(node.id) ?? null
				}
			}))
		];

		const status = new Map(graph.nodes.map((n) => [n.id, n.status]));
		const live = (from: string, to: string) =>
			(from.startsWith('world:') || status.get(from) === 'active') && status.get(to) === 'active';

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
			})),
			...graph.worldEdges.map((edge) => ({
				id: `world:${edge.model}->${edge.to}`,
				source: `world:${edge.model}`,
				target: edge.to,
				targetHandle: WORLD_HANDLE,
				deletable: false,
				selectable: false,
				style: 'stroke: #6b7aa6; stroke-dasharray: 2 6; opacity: 0.6;'
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
		<span class="muted hint">drag sockets to connect · Del removes selection</span>

		{#if graph?.kinds.length}
			<select bind:value={toAdd} disabled={busy}>
				{#each graph.kinds as kind}
					<option value={kind.id} title={kind.description}>{kind.label}</option>
				{/each}
			</select>
			<button disabled={busy || !toAdd} onclick={() => onAdd(toAdd, ...freeSpot())}>
				Add node
			</button>
		{/if}
		<button disabled={busy} onclick={() => onNewScript(...freeSpot())}>New script</button>
	</header>

	<div class="flow">
		<SvelteFlow
			bind:nodes
			bind:edges
			{nodeTypes}
			colorMode="dark"
			fitView
			deleteKey={['Delete', 'Backspace']}
			{isValidConnection}
			onbeforeconnect={onBeforeConnect}
			ondelete={onDelete}
			onnodeclick={onNodeClick}
			onpaneclick={() => onSelectStep(null)}
			onnodedragstop={({ nodes: moved }) =>
				onMove(
					moved
						.filter((n) => n.type === 'step')
						.map((n) => ({ id: n.id, x: n.position.x, y: n.position.y }))
				)}
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

	.flow :global(.svelte-flow__edge.voxel path) {
		stroke: #e0a24f;
	}
</style>
