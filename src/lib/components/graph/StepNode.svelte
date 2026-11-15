<script lang="ts">
	import { Handle, Position, useUpdateNodeInternals, type NodeProps } from '@xyflow/svelte';
	import type { StepNodeData } from './types';

	let { id, data }: NodeProps & { data: StepNodeData } = $props();
	let node = $derived(data.node);

	// Script edits can change sockets: re-measure handles when they do.
	const updateNodeInternals = useUpdateNodeInternals();
	let socketKey = $derived(
		[...node.inputs, ...node.outputs].map((s) => `${s.name}:${s.scope}`).join('|')
	);
	$effect(() => {
		socketKey;
		updateNodeInternals(id);
	});

	let editing = $state(false);

	function commitLabel(value: string) {
		if (!editing) return;
		editing = false;
		// Unchanged (or the kind default) means no edit.
		if (value.trim() !== node.label) data.onLabel(node.id, value);
	}

	const SCOPE_LABEL = { atlas: 'atlas', world: 'world', column: 'chunk', voxel: 'voxel' } as const;
	const SCOPE_HELP = {
		atlas: 'Atlas node: runs once when the world is created, over whole-world grids (simulations). Reads atlas grids; connect world values through Rasterize, read results back through Sample.',
		world: 'World node: runs once when the world is created, then read-only. Can only read other world nodes.',
		column: 'Chunk node: runs per chunk column. Can read world and chunk values.',
		voxel: 'Voxel node: runs per cell. Can read world, chunk and voxel values.'
	} as const;

	const STAGE_HELP = {
		prefix: 'Needed by the neighborhood node: re-run for each neighbor chunk (cached when it only writes column outputs).',
		neighborhood: 'Cross-chunk node: reads prefix data of neighboring chunks within its radius (X×Y×Z cells).',
		suffix: 'Runs once per chunk, after the neighborhood node.'
	} as const;
</script>

<div
	class="step-node {node.status} scope-{node.scope}"
	class:script={node.script}
	class:selected={data.selected}
	title={node.status === 'active'
		? `${node.description}\nClick to view the map after this node.`
		: node.description}
>
	<header>
		<input
			type="checkbox"
			class="nodrag"
			checked={node.enabled}
			title={node.enabled ? 'Disable node' : 'Enable node'}
			onchange={(e) => data.onToggle(node.id, e.currentTarget.checked)}
		/>
		{#if editing}
			<!-- svelte-ignore a11y_autofocus -->
			<input
				class="nodrag title-input"
				value={node.label}
				autofocus
				placeholder={node.export ? 'export name' : node.kindLabel}
				onkeydown={(e) => {
					if (e.key === 'Enter') commitLabel(e.currentTarget.value);
					if (e.key === 'Escape') editing = false;
				}}
				onblur={(e) => commitLabel(e.currentTarget.value)}
			/>
		{:else}
			<strong
				class="title"
				title={node.export
					? 'Export name read by game code — double-click to rename'
					: `${node.kindLabel} — double-click to rename`}
				ondblclick={() => (editing = true)}
			>
				{#if node.export}<span class="export-tag">export</span>{/if}
				{node.label}
			</strong>
		{/if}
		{#if node.order !== null}
			<span class="muted">#{node.order + 1}</span>
		{/if}
		<span class="spacer"></span>
		{#if node.script}
			<button class="nodrag link" title="Edit script" onclick={() => data.onEditScript(node.id)}>
				edit
			</button>
		{/if}
		<button class="nodrag remove" title="Remove node" onclick={() => data.onRemove(node.id)}>
			×
		</button>
	</header>

	<div class="sockets">
		<div class="column">
			{#each node.inputs as input (input.name)}
				<div class="socket in" title="{input.name} ({input.scope})">
					<Handle type="target" position={Position.Left} id={input.name} class="handle {input.scope}" />
					<span>{input.name}</span>
				</div>
			{/each}
		</div>
		<div class="column right">
			{#each node.outputs as output (output.name)}
				<div class="socket out" title="{output.name} ({output.scope})">
					<span>{output.name}</span>
					<Handle type="source" position={Position.Right} id={output.name} class="handle {output.scope}" />
				</div>
			{/each}
		</div>
	</div>

	{#if node.params.length}
		<div class="params">
			{#each node.params as param (param.name)}
				<label class="nodrag" title="{param.min} … {param.max} (default {param.default})">
					<span>{param.name}</span>
					<input
						type="number"
						step="any"
						min={param.min}
						max={param.max}
						value={param.value}
						onchange={(e) => {
							const value = Number(e.currentTarget.value);
							if (Number.isFinite(value)) data.onParam(node.id, param.name, value);
						}}
					/>
				</label>
			{/each}
		</div>
	{/if}

	<footer>
		<span class="scope-tag {node.scope}" title={SCOPE_HELP[node.scope]}>{SCOPE_LABEL[node.scope]}</span>
		{#if node.stage}
			<span class="stage {node.stage}" title={STAGE_HELP[node.stage]}>
				{node.stage}{node.neighborhood ? ` · r ${node.neighborhood.join('×')}` : ''}
			</span>
		{:else if node.neighborhood}
			<span class="stage neighborhood" title={STAGE_HELP.neighborhood}>
				neighborhood · r {node.neighborhood.join('×')}
			</span>
		{/if}
		<span class="spacer"></span>
		{#if data.meanMs !== null}
			<span class="muted" title="Mean time per generated chunk">
				{data.meanMs < 0.1 ? '<0.1' : data.meanMs.toFixed(1)} ms
			</span>
		{/if}
	</footer>

	{#if node.reason}
		<div class="reason">{node.reason}</div>
	{/if}
</div>

<style>
	.step-node {
		min-width: 190px;
		max-width: 240px;
		padding: 6px 0;
		border-radius: 6px;
		border: 1px solid #4b5068;
		background: #2a2d3a;
		color: #d6d8e0;
		font-size: 12px;
	}

	.step-node.disabled {
		opacity: 0.5;
	}

	.step-node.blocked {
		border-color: #c0573f;
	}

	.step-node.script {
		border-style: dashed;
	}

	.title {
		cursor: text;
	}

	.title-input {
		width: 120px;
		background: #1f2129;
		border-radius: 3px;
		padding: 0 4px;
	}

	.export-tag {
		margin-right: 4px;
		padding: 0 5px;
		border-radius: 6px;
		background: #253a2f;
		color: #8fd0a4;
		font-size: 10px;
		font-weight: normal;
		text-transform: uppercase;
	}

	.step-node.selected {
		border-color: #6fa8ff;
		box-shadow: 0 0 0 2px #6fa8ff55;
	}

	header,
	footer {
		display: flex;
		align-items: center;
		gap: 6px;
		padding: 0 10px;
	}

	.spacer {
		flex: 1;
	}

	.muted {
		color: #8a8fa6;
		font-size: 11px;
		font-variant-numeric: tabular-nums;
	}

	.link {
		color: #9ec3e6;
		font-size: 11px;
	}

	.remove {
		color: #8a8fa6;
		padding: 0 2px;
	}

	.remove:hover {
		color: #e07a64;
	}

	.sockets {
		display: flex;
		justify-content: space-between;
		gap: 12px;
		margin: 4px 0;
	}

	.column {
		display: flex;
		flex-direction: column;
		gap: 2px;
	}

	.column.right {
		align-items: flex-end;
	}

	.socket {
		position: relative;
		padding: 0 10px;
		line-height: 18px;
		color: #b9bdd0;
	}

	.socket :global(.handle) {
		width: 9px;
		height: 9px;
		border: 1px solid #1b1d25;
	}

	.socket :global(.handle.column) {
		background: #6fa8ff;
	}

	.socket :global(.handle.voxel) {
		background: #e0a24f;
	}

	.socket :global(.handle.world) {
		background: #b58cf0;
	}

	.socket :global(.handle.atlas) {
		background: #4fd1b5;
	}

	.scope-tag {
		padding: 0 6px;
		border-radius: 8px;
		font-size: 10px;
		text-transform: uppercase;
		letter-spacing: 0.05em;
		border: 1px solid currentColor;
	}

	.scope-tag.world {
		color: #b58cf0;
	}

	.scope-tag.atlas {
		color: #4fd1b5;
	}

	.scope-tag.column {
		color: #6fa8ff;
	}

	.scope-tag.voxel {
		color: #e0a24f;
	}

	/* World nodes run once per world, not per chunk. */
	.step-node.scope-world {
		border-left: 3px solid #b58cf0;
	}

	/* Atlas nodes run once per world, over whole grids. */
	.step-node.scope-atlas {
		border-left: 3px solid #4fd1b5;
	}

	.params {
		display: flex;
		flex-direction: column;
		gap: 2px;
		padding: 2px 10px 4px;
		border-top: 1px solid #353949;
	}

	.params label {
		display: flex;
		justify-content: space-between;
		align-items: center;
		gap: 8px;
		color: #9a9fb5;
	}

	.params input {
		width: 70px;
		background: #1f2129;
		border-radius: 3px;
		padding: 0 4px;
		text-align: right;
	}

	.stage {
		padding: 0 6px;
		border-radius: 8px;
		font-size: 10px;
		text-transform: uppercase;
		letter-spacing: 0.04em;
	}

	.stage.prefix {
		background: #3a3420;
		color: #e0c27a;
	}

	.stage.neighborhood {
		background: #2a2440;
		color: #b9a4f0;
	}

	.stage.suffix {
		background: #1f3328;
		color: #8fd0a4;
	}

	.reason {
		margin-top: 4px;
		padding: 0 10px;
		color: #e0947f;
	}
</style>
