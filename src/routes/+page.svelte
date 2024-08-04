<script lang="ts">
	import { onMount } from 'svelte';
	import Sidebar from '$lib/components/Sidebar.svelte';
	import { createScene } from '../scene.js';
	import { Button } from 'svelte-foundry';

	let canvasContainer: HTMLDivElement | null = null;

	onMount(() => {
		if (canvasContainer) {
			createScene(canvasContainer);
		}
	});
</script>

<div class="app-container">
	<Sidebar />
	<main>
		<Button class="">asdasd</Button>
		<div bind:this={canvasContainer} class="canvas-container"></div>
	</main>
</div>

<style>
	.app-container {
		display: flex;
	}

	main {
		flex-grow: 1;
		padding: 20px;
	}

	.canvas-container {
		width: 100%;
		height: 400px;
	}
</style>
