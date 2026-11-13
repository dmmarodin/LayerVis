<script lang="ts">
	import { onMount } from 'svelte';

	/** Persisted per id, so collapsed sections stay collapsed across restarts. */
	export let id: string;
	export let title: string;
	export let open = true;

	const storageKey = () => `layervis.section.${id}`;

	onMount(() => {
		try {
			const saved = localStorage.getItem(storageKey());
			if (saved !== null) open = saved === '1';
		} catch {
			// Storage can be unavailable; sections just start open.
		}
	});

	function onToggle(event: Event) {
		open = (event.currentTarget as HTMLDetailsElement).open;
		try {
			localStorage.setItem(storageKey(), open ? '1' : '0');
		} catch {
			// Not persisting is fine.
		}
	}
</script>

<details class="section" {open} on:toggle={onToggle}>
	<summary>{title}</summary>
	<div class="body">
		<slot />
	</div>
</details>

<style lang="scss">
	@use 'svelte-foundry/colors' as colors;

	.section {
		border-bottom: 1px solid colors.$background-light;
		padding: 0 16px;
	}

	summary {
		cursor: pointer;
		list-style: none;
		display: flex;
		align-items: center;
		justify-content: space-between;
		padding: 10px 0;
		font-size: 11px;
		font-weight: 600;
		letter-spacing: 0.06em;
		text-transform: uppercase;
		color: colors.$text-mute;
		user-select: none;
	}

	summary::-webkit-details-marker {
		display: none;
	}

	summary::after {
		content: '▾';
		transition: transform 0.12s;
	}

	.section:not([open]) summary::after {
		transform: rotate(-90deg);
	}

	.body {
		display: flex;
		flex-direction: column;
		gap: 10px;
		padding-bottom: 14px;
	}
</style>
