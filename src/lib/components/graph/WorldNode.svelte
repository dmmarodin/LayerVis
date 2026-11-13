<script lang="ts">
	import { Handle, Position, type NodeProps } from '@xyflow/svelte';
	import type { WorldNodeData } from './types';

	let { data }: NodeProps & { data: WorldNodeData } = $props();
</script>

<div class="world-node" title="World model (built once per seed)">
	<span class="kind">world</span>
	<strong>{data.model}</strong>
	<Handle type="source" position={Position.Right} />
</div>

<style>
	.world-node {
		padding: 6px 10px;
		border-radius: 6px;
		border: 1px dashed #6b7aa6;
		background: #232736;
		color: #c3cbe8;
		font-size: 12px;
		display: flex;
		gap: 6px;
		align-items: baseline;
	}

	.kind {
		color: #8a8fa6;
		font-size: 11px;
	}
</style>
