<script lang="ts">
	import { onDestroy, onMount } from 'svelte';
	import { EditorView, basicSetup } from 'codemirror';
	import { keymap } from '@codemirror/view';
	import { rust } from '@codemirror/lang-rust';
	import { oneDark } from '@codemirror/theme-one-dark';
	import type { ScriptTemplate } from '$lib/bridge';

	interface Props {
		source: string;
		/** Step id being edited, or null for a new script. */
		replaceId: string | null;
		busy: boolean;
		error: string | null;
		onApply: (source: string) => void;
		onClose: () => void;
		/** Column fields that currently exist (readable via `reads:`). */
		fields: Array<{ name: string; unit: string; node: string; nodeLabel: string }>;
		/** Loads starting text for a new script. */
		loadTemplate: (kind: ScriptTemplate) => Promise<string>;
	}

	let { source, replaceId, busy, error, onApply, onClose, fields, loadTemplate }: Props =
		$props();

	let host: HTMLDivElement;
	let view: EditorView | null = null;

	const FUNCTIONS = [
		['world_elevation(x, y)', 'tectonic elevation (cells)'],
		['world_plate(x, y)', 'plate id'],
		['world_boundary_distance(x, y)', 'cells to plate boundary'],
		['world_convergence(x, y)', '>0 collision, <0 rift'],
		['latitude(y)', '-1 … 1'],
		['sea_level(), depth()', 'elevation constants'],
		['noise(x, y, wavelength)', 'seeded 0 … 1'],
		['clamp(v, lo, hi)', ''],
		['lerp(a, b, t)', ''],
		['smoothstep(e0, e1, x)', ''],
		['min(a, b), max(a, b)', 'floats']
	] as const;

	async function useTemplate(kind: ScriptTemplate) {
		if (!view) return;
		const text = await loadTemplate(kind);
		view.dispatch({ changes: { from: 0, to: view.state.doc.length, insert: text } });
		view.focus();
	}

	const apply = () => {
		if (view && !busy) onApply(view.state.doc.toString());
		return true;
	};

	onMount(() => {
		// Rhai is close enough to Rust for highlighting.
		view = new EditorView({
			doc: source,
			parent: host,
			extensions: [
				basicSetup,
				rust(),
				oneDark,
				keymap.of([
					{ key: 'Mod-Enter', run: apply },
					{ key: 'Mod-s', run: apply, preventDefault: true },
					{ key: 'Escape', run: () => (onClose(), true) }
				])
			]
		});
		view.focus();
	});

	onDestroy(() => view?.destroy());
</script>

<div class="backdrop" role="presentation" onmousedown={(e) => e.target === e.currentTarget && onClose()}>
	<div class="dialog" role="dialog" aria-label="Script editor">
		<header>
			<strong>{replaceId ? `Edit script node ${replaceId}` : 'New script node'}</strong>
			<span class="hint">Ctrl+Enter to apply · Esc to close</span>
			<span class="spacer"></span>
			{#if !replaceId}
				<span class="hint">Start from:</span>
				<button onclick={() => useTemplate('blank')}>Blank</button>
				<button onclick={() => useTemplate('example')}>Example</button>
			{/if}
			<button onclick={onClose}>Cancel</button>
			<button class="primary" disabled={busy} onclick={apply}>
				{busy ? 'Validating…' : 'Apply'}
			</button>
		</header>

		<div class="body">
			<div class="editor" bind:this={host}></div>

			<aside class="reference">
				<h3>Header</h3>
				<code>//! name: Any title</code>
				<code>//! input: a, b</code>
				<code>//! output: c 0..1 thermal</code>
				<p>
					Inputs and outputs become sockets on the node. Wire them in the graph; a new node's
					inputs auto-connect to existing outputs with the same name.
				</p>

				<h3>Existing outputs</h3>
				{#each fields as field}
					<code title="{field.nodeLabel} {field.node}">{field.name} <small>{field.node}</small></code>
				{:else}
					<p>No column outputs yet.</p>
				{/each}
				<p>Always: <code class="inline">this.x</code>, <code class="inline">this.y</code></p>

				<h3>Functions</h3>
				{#each FUNCTIONS as [signature, note]}
					<code title={note}>{signature}</code>
				{/each}
				<p>Use float literals (0.0, not 0).</p>
			</aside>
		</div>

		{#if error}
			<pre class="error">{error}</pre>
		{/if}
	</div>
</div>

<style>
	.backdrop {
		position: fixed;
		inset: 0;
		z-index: 20;
		background: #0008;
		display: flex;
		align-items: center;
		justify-content: center;
		padding: 24px;
	}

	.dialog {
		width: min(900px, 100%);
		height: min(720px, 100%);
		display: flex;
		flex-direction: column;
		background: #1b1d25;
		border: 1px solid #3c4055;
		border-radius: 8px;
		overflow: hidden;
		color: #c3c6d2;
		font-size: 13px;
	}

	header {
		display: flex;
		align-items: center;
		gap: 10px;
		padding: 8px 12px;
		border-bottom: 1px solid #2d303d;
	}

	.hint {
		color: #8a8fa6;
		font-size: 12px;
	}

	.spacer {
		flex: 1;
	}

	button {
		background: #2d303d;
		border-radius: 4px;
		padding: 2px 10px;
	}

	button.primary {
		background: #3b5bab;
		color: white;
	}

	button:disabled {
		opacity: 0.6;
	}

	.body {
		flex: 1;
		min-height: 0;
		display: flex;
	}

	.editor {
		flex: 1;
		min-width: 0;
		overflow: auto;
	}

	.reference {
		width: 240px;
		flex-shrink: 0;
		overflow-y: auto;
		padding: 8px 12px;
		border-left: 1px solid #2d303d;
		font-size: 12px;
		display: flex;
		flex-direction: column;
		gap: 3px;
	}

	.reference h3 {
		margin-top: 8px;
		color: #8a8fa6;
		font-size: 11px;
		text-transform: uppercase;
		letter-spacing: 0.05em;
	}

	.reference code {
		color: #9ec3e6;
		font-size: 11.5px;
	}

	.reference code.inline {
		display: inline;
	}

	.reference p {
		color: #8a8fa6;
		font-size: 11px;
	}

	.editor :global(.cm-editor) {
		height: 100%;
	}

	.error {
		margin: 0;
		max-height: 30%;
		overflow: auto;
		padding: 8px 12px;
		border-top: 1px solid #5a2e25;
		background: #2a1b18;
		color: #f0a593;
		white-space: pre-wrap;
		font-size: 12px;
	}
</style>
