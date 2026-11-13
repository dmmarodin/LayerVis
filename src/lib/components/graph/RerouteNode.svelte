<script lang="ts">
	import { Handle, Position, type NodeProps } from '@xyflow/svelte';
	import type { RerouteNodeData } from './types';

	let { data }: NodeProps & { data: RerouteNodeData } = $props();
	let node = $derived(data.node);
	let scope = $derived(node.inputs[0]?.scope ?? 'column');
</script>

<!-- A wire waypoint: passes its input through unchanged. -->
<div
	class="reroute {scope} {node.status}"
	class:selected={data.selected}
	title="Reroute ({scope}) · {node.id}{node.reason ? ` — ${node.reason}` : ''}"
>
	<Handle type="target" position={Position.Left} id="in" class="handle {scope}" />
	<Handle type="source" position={Position.Right} id="out" class="handle {scope}" />
</div>

<style>
	.reroute {
		width: 18px;
		height: 18px;
		border-radius: 50%;
		border: 2px solid #6fa8ff;
		background: #1b1d25;
	}

	.reroute.voxel {
		border-color: #e0a24f;
	}

	.reroute.blocked,
	.reroute.disabled {
		border-style: dashed;
		opacity: 0.6;
	}

	.reroute.selected {
		background: #6fa8ff55;
	}

	.reroute :global(.handle.column) {
		background: #6fa8ff;
	}

	.reroute :global(.handle.voxel) {
		background: #e0a24f;
	}

	.reroute :global(.svelte-flow__handle) {
		width: 8px;
		height: 8px;
		min-width: 0;
		min-height: 0;
	}
</style>
