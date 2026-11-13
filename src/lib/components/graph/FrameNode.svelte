<script lang="ts">
	import { NodeResizer, type NodeProps } from '@xyflow/svelte';
	import type { FrameNodeData } from './types';

	let { id, data, selected }: NodeProps & { data: FrameNodeData } = $props();
	let frame = $derived(data.frame);
	let editing = $state(false);

	function commit(value: string) {
		if (!editing) return;
		editing = false;
		if (value.trim() !== frame.label) data.onLabel(id, value);
	}
</script>

<NodeResizer
	minWidth={60}
	minHeight={60}
	isVisible={selected}
	onResizeEnd={(_, params) => data.onResize(id, params.x, params.y, params.width, params.height)}
/>

<div class="frame">
	<!-- The whole title bar drags the frame (see `dragHandle` in PipelineGraph);
	     double-click the title to rename. -->
	<div class="frame-title" ondblclick={() => (editing = true)} role="presentation">
		{#if editing}
			<!-- svelte-ignore a11y_autofocus -->
			<input
				class="nodrag"
				value={frame.label}
				placeholder="Frame title"
				spellcheck="false"
				autofocus
				onkeydown={(e) => {
					if (e.key === 'Enter') commit(e.currentTarget.value);
					if (e.key === 'Escape') editing = false;
				}}
				onblur={(e) => commit(e.currentTarget.value)}
			/>
		{:else}
			<span class:placeholder={!frame.label} title="Drag to move · double-click to rename">
				{frame.label || 'Frame (double-click to name)'}
			</span>
		{/if}
	</div>
</div>

<style>
	.frame {
		width: 100%;
		height: 100%;
		border: 1px dashed #4a5070;
		border-radius: 8px;
		background: #8a90b014;
	}

	.frame-title {
		cursor: grab;
		padding: 4px 8px;
		min-height: 26px;
		border-radius: 8px 8px 0 0;
		background: #8a90b022;
		font-size: 12px;
		font-weight: 600;
		color: #aab0cc;
		user-select: none;
	}

	.placeholder {
		color: #6a7090;
		font-weight: 400;
	}

	input {
		width: 100%;
		background: transparent;
		border: 0;
		color: inherit;
		font: inherit;
		outline: none;
	}
</style>
