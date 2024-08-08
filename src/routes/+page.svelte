<script lang="ts">
	import { onMount, onDestroy } from 'svelte';
	import Sidebar from '$lib/components/Sidebar.svelte';
	import { Visualizer } from '../lib/visualizer/visualizer';

	let canvasContainer: HTMLDivElement | null = null;
	let visualizer: Visualizer | null = null;

	onMount(() => {
		if (canvasContainer) {
			visualizer = new Visualizer(canvasContainer);

			if (typeof window !== 'undefined') {
				window.addEventListener('resize', visualizer.resize);
			}
		}
	});

	onDestroy(() => {
		if (visualizer && typeof window !== 'undefined') {
			window.removeEventListener('resize', visualizer.resize);
		}
	});
</script>

<div class="app-container">
	<Sidebar />
	<div bind:this={canvasContainer} class="canvas-container"></div>
</div>

<style>
	.app-container {
		display: flex;
		height: 100%;
	}

	.canvas-container {
		flex-grow: 1;
		height: 100%;
		overflow: hidden;
	}
</style>
