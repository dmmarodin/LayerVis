<script lang="ts">
	import { onMount, onDestroy } from 'svelte';
	import Sidebar from '$lib/components/Sidebar.svelte';
	import { createScene, type SceneHandler } from '../lib/three/scene';

	let canvasContainer: HTMLDivElement | null = null;
	let handler: SceneHandler | null = null;

	onMount(() => {
		if (canvasContainer) {
			handler = createScene(canvasContainer);

			if (typeof window !== 'undefined') {
				window.addEventListener('resize', handler.resize);
			}
		}
	});

	onDestroy(() => {
		if (handler && typeof window !== 'undefined') {
			window.removeEventListener('resize', handler.resize);
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
