<script lang="ts">
	import type { FieldDef, WorldInfo } from '$lib/bridge';
	import { RAMPS, type FieldSource, type Ramp, type ViewMode } from '$lib/visualizer/fieldMaterial';
	import type { HoverInfo } from '$lib/visualizer/visualizer';

	export let world: WorldInfo | null = null;
	export let statusLines: string[] = [];
	export let hover: HoverInfo | null = null;
	export let onViewChange: (view: ViewMode) => void = () => {};
	/** Request to select a column field by key (e.g. a clicked node's output). */
	export let focusField: { key: string; nonce: number } | null = null;
	/** Keys of column fields computed at the selected node; null = full graph. */
	export let stageFields: string[] | null = null;

	let focusedNonce = 0;
	$: if (world && focusField && focusField.nonce !== focusedNonce) {
		focusedNonce = focusField.nonce;
		const index = world.columnFields.findIndex((def) => def.key === focusField?.key);
		if (index >= 0) selectField('column', index);
	}

	const computed = (key: string) => !stageFields || stageFields.includes(key);

	let source: FieldSource = 'column';
	let field = 0;
	let ramp: Ramp = 'terrain';
	let min = 0;
	let max = 1;
	let initializedFor: WorldInfo | null = null;

	// On a new world/schema, keep the selected field (by key) if it still
	// exists; otherwise fall back to the first column field, then world field.
	$: if (world && world !== initializedFor) {
		const key = initializedFor ? previousKey(initializedFor) : undefined;
		initializedFor = world;
		restoreSelection(key);
	}

	function previousKey(previousWorld: WorldInfo): string | undefined {
		const list = source === 'column' ? previousWorld.columnFields : previousWorld.worldFields;
		return list[field]?.key;
	}

	function restoreSelection(key: string | undefined) {
		const index = key ? fieldsOf(source).findIndex((def) => def.key === key) : -1;

		if (index >= 0) {
			field = index; // keep the user's ramp and range
		} else if (fieldsOf('column').length) {
			selectField('column', 0);
		} else {
			selectField('world', 0);
		}
	}

	$: onViewChange({ source, field, ramp, min, max });

	function fieldsOf(scope: FieldSource): FieldDef[] {
		if (!world) return [];
		return scope === 'column' ? world.columnFields : world.worldFields;
	}

	function selectField(scope: FieldSource, index: number) {
		const def = fieldsOf(scope)[index];
		if (!def) return;
		source = scope;
		field = index;
		ramp = (RAMPS as readonly string[]).includes(def.ramp) ? (def.ramp as Ramp) : 'grayscale';
		min = def.min;
		max = def.max;
	}

	function onFieldSelect(value: string) {
		const [scope, index] = value.split(':');
		selectField(scope as FieldSource, Number(index));
	}

	function label(def: FieldDef): string {
		const unit = def.unit ? ` (${def.unit})` : '';
		// Column outputs: socket name plus the node that produced it.
		return def.node ? `${def.name}${unit} · ${def.nodeLabel} ${def.node}` : `${def.name}${unit}`;
	}

	function format(value: number | null | undefined): string {
		if (value === null || value === undefined) return '–';
		return Number.isInteger(value) ? String(value) : value.toFixed(2);
	}
</script>

<aside class="sidebar flex flex-col gap-4 p-6 min-w-64 max-w-72 overflow-y-auto">
	<h2 class="text-lg font-semibold">View mode</h2>

	{#if world}
		<label class="flex flex-col gap-1">
			<span>Field</span>
			<select value={`${source}:${field}`} on:change={(e) => onFieldSelect(e.currentTarget.value)}>
				<optgroup label="Chunk columns">
					{#each world.columnFields as def, index}
						<option value={`column:${index}`}>
							{label(def)}{computed(def.key) ? '' : ' — not yet'}
						</option>
					{/each}
				</optgroup>
				<optgroup label="World models">
					{#each world.worldFields as def, index}
						<option value={`world:${index}`}>{label(def)}</option>
					{/each}
				</optgroup>
			</select>
		</label>

		<label class="flex flex-col gap-1">
			<span>Ramp</span>
			<select bind:value={ramp}>
				{#each RAMPS as name}
					<option value={name}>{name}</option>
				{/each}
			</select>
		</label>

		<div class="flex gap-2">
			<label class="flex flex-col gap-1">
				<span>Min</span>
				<input type="number" step="any" bind:value={min} disabled={ramp === 'categorical'} />
			</label>
			<label class="flex flex-col gap-1">
				<span>Max</span>
				<input type="number" step="any" bind:value={max} disabled={ramp === 'categorical'} />
			</label>
		</div>

		<p class="text-xs">
			Seed {world.seed} · {world.plateCount} plates · chunk {world.chunkSize.join('×')}
		</p>

		<h2 class="text-lg font-semibold mt-2">Exports</h2>
		{#each world.exports as exp}
			<button
				class="export text-xs"
				title="Game code reads this by name; shows {exp.field}"
				on:click={() => {
					const index = world?.columnFields.findIndex((f) => f.key === exp.field) ?? -1;
					if (index >= 0) selectField('column', index);
				}}
			>
				<strong>{exp.name}</strong> ← {exp.field} <span class="muted">({exp.scope})</span>
			</button>
		{:else}
			<p class="text-xs">None. Add an Export node to name outputs for game code.</p>
		{/each}

		<h2 class="text-lg font-semibold mt-2">Cursor</h2>
		{#if hover}
			<dl class="readout text-xs">
				<dt>position</dt>
				<dd>{hover.x}, {hover.y}</dd>

				{#each world.columnFields as def, index}
					<dt title="{def.nodeLabel} {def.node}">{def.name} <small>{def.node}</small></dt>
					<dd>{computed(def.key) ? format(hover.column?.[index]) : 'not yet'}</dd>
				{/each}

				{#if hover.world}
					<dt>plate</dt>
					<dd>{hover.world.plate}</dd>
					<dt>boundary</dt>
					<dd>{format(hover.world.boundaryDistance)} cells</dd>
					<dt>convergence</dt>
					<dd>{format(hover.world.convergence)}</dd>
				{:else}
					<dt>world</dt>
					<dd>outside</dd>
				{/if}
			</dl>
		{:else}
			<p class="text-xs">Hover the map</p>
		{/if}
	{/if}

	{#each statusLines as line}
		<p class="text-xs">{line}</p>
	{/each}
</aside>

<style lang="scss">
	@use 'svelte-foundry/colors' as colors;

	.sidebar {
		background-color: colors.$background-mid;
		color: colors.$text-mute;
		border-right: 2px solid colors.$background-light;
	}

	select,
	input {
		background-color: colors.$background-light;
		border-radius: 4px;
		padding: 2px 6px;
		width: 100%;
	}

	.export {
		text-align: left;
		padding: 0;
	}

	.export .muted {
		opacity: 0.7;
	}

	input:disabled {
		opacity: 0.5;
	}

	.readout {
		display: grid;
		grid-template-columns: auto 1fr;
		gap: 2px 12px;

		dd {
			text-align: right;
			font-variant-numeric: tabular-nums;
		}
	}
</style>
