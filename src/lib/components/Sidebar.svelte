<script lang="ts">
	import type { AtlasStatus, Extent, FieldDef, WorldInfo } from '$lib/bridge';
	import {
		RAMPS,
		isCategoricalRamp,
		isPaletteRamp,
		rampGradient,
		type FieldSource,
		type Ramp,
		type ViewMode
	} from '$lib/visualizer/fieldMaterial';
	import type { HoverInfo } from '$lib/visualizer/visualizer';
	import SidebarSection from './SidebarSection.svelte';

	export let world: WorldInfo | null = null;
	export let statusLines: string[] = [];
	export let hover: HoverInfo | null = null;
	export let onViewChange: (view: ViewMode) => void = () => {};
	/** Request to select a column field by key (e.g. a clicked node's output). */
	export let focusField: { key: string; nonce: number } | null = null;
	/** Keys of column fields computed at the selected node; null = full graph. */
	export let stageFields: string[] | null = null;
	/** Applies a new seed (decimal string); resolves once the world has reloaded. */
	export let onSeed: (seed: string) => Promise<void> | void = () => {};
	export let onGoTo: (x: number, y: number) => void = () => {};
	export let onFitWorld: () => void = () => {};
	/** Value range of the selected field in view, for "Auto" range. */
	export let getDataRange: () => [number, number] | null = () => null;
	/** Disables the seed controls while a graph edit is running. */
	export let busy = false;
	/** Background atlas build, while one runs (or failed). */
	export let atlasState: AtlasStatus | null = null;
	/** Applies a new world size / depth / atlas cell size. */
	export let onExtent: (extent: Extent) => Promise<void> | void = () => {};
	/** Shows a scrubber frame (index into `world.atlas.snapshots`), or null for the live view. */
	export let onSnapshot: (index: number | null) => void = () => {};
	export let onRivers: (visible: boolean) => void = () => {};
	export let onGrid: (visible: boolean) => void = () => {};

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
		const list =
			source === 'column'
				? previousWorld.columnFields
				: source === 'atlas'
					? previousWorld.atlas.fields
					: previousWorld.worldFields;
		return list[field]?.key;
	}

	function restoreSelection(key: string | undefined) {
		const index = key ? fieldsOf(source).findIndex((def) => def.key === key) : -1;

		if (index >= 0) {
			field = index; // keep the user's ramp and range
		} else if (world && world.worldFields.some((def) => def.name === 'elevation')) {
			// The terrain first: the last (filtered) world elevation.
			const last = world.worldFields.map((def) => def.name).lastIndexOf('elevation');
			selectField('world', last);
		} else if (fieldsOf('column').length) {
			selectField('column', 0);
		} else {
			selectField('world', 0);
		}
	}

	$: onViewChange({ source, field, ramp, min, max });

	function fieldsOf(scope: FieldSource): FieldDef[] {
		if (!world) return [];
		if (scope === 'atlas') return world.atlas.fields;
		return scope === 'column' ? world.columnFields : world.worldFields;
	}

	function selectField(scope: FieldSource, index: number) {
		const def = fieldsOf(scope)[index];
		if (!def) return;
		source = scope;
		field = index;
		resetRange();
	}

	/** The field's own ramp and range (what selecting it starts with). */
	function resetRange() {
		const def = fieldsOf(source)[field];
		if (!def) return;
		ramp = (RAMPS as readonly string[]).includes(def.ramp) ? (def.ramp as Ramp) : 'grayscale';
		min = def.min;
		max = def.max;
		if (ramp === 'terrain' && world) {
			// Heights: the ramp's coast (30%) at this world's sea level. A
			// default world (sea at 0.3 × depth) gets 0 … depth.
			const depth = world.dimensions[2];
			const sea = world.seaLevel;
			const low = sea - ((depth - sea) * 0.3) / 0.7;
			min = Math.abs(low) < 1e-6 ? 0 : low;
			max = depth;
		}
	}

	function autoRange() {
		const range = getDataRange();
		if (!range) return;
		const [lo, hi] = range;
		min = lo;
		// A constant field still needs a nonzero span to color.
		max = hi > lo ? hi : lo + 1;
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

	/** A value as the field shows it: material and biome ids by name. */
	function formatField(def: FieldDef | undefined, value: number | null | undefined): string {
		if (value === null || value === undefined || !def || !world) return format(value);
		const table =
			def.ramp === 'material' ? world.materials : def.ramp === 'biome' ? world.biomes : null;
		if (!table) return format(value);
		const entry = table[Math.round(value)];
		return entry ? `${entry.label} (${Math.round(value)})` : format(value);
	}

	const swatch = (color: [number, number, number]) => `rgb(${color[0]} ${color[1]} ${color[2]})`;

	$: gradient = rampGradient(ramp);
	$: selectedDef = fieldsOf(source)[field];

	// --- Seed ---------------------------------------------------------------

	const U64_MAX = BigInt('18446744073709551615');

	let seedText = '';
	let seedError: string | null = null;
	let seededFor: string | null = null;
	let committing = false;
	let copied = false;

	// Follow the world's seed (after load, undo, apply) unless an edit is pending.
	$: if (world && world.seed !== seededFor) {
		seededFor = world.seed;
		seedText = world.seed;
		seedError = null;
	}

	/** Normalized decimal seed, or an error message. */
	function parseSeed(text: string): { seed: string } | { error: string } {
		if (!/^\d+$/.test(text)) return { error: 'Digits only (0 to 18446744073709551615)' };
		const value = BigInt(text);
		if (value > U64_MAX) return { error: 'Too large (max 18446744073709551615)' };
		return { seed: value.toString() };
	}

	async function applySeed(text: string) {
		if (!world || committing) return;
		const parsed = parseSeed(text.trim());

		if ('error' in parsed) {
			seedError = parsed.error;
			return;
		}

		seedError = null;
		seedText = parsed.seed;
		if (parsed.seed === world.seed) return;

		committing = true;
		try {
			await onSeed(parsed.seed);
		} finally {
			committing = false;
			// A rejected seed leaves the world unchanged: show the one in use.
			if (world) seedText = world.seed;
		}
	}

	function randomSeed() {
		const [value] = crypto.getRandomValues(new BigUint64Array(1));
		seedText = value.toString();
		applySeed(seedText);
	}

	async function copySeed() {
		if (!world) return;
		try {
			await navigator.clipboard.writeText(world.seed);
			copied = true;
			setTimeout(() => (copied = false), 1200);
		} catch {
			// Clipboard can be denied; nothing else to do.
		}
	}

	// --- World summary --------------------------------------------------------

	$: stats = world?.worldStats;
	$: landPercent = stats ? Math.round(stats.landFraction * 100) : 0;
	$: edgeOcean = !!stats && stats.edgeLand === 0;

	// --- Extent ---------------------------------------------------------------

	let extentWidth = 0;
	let extentHeight = 0;
	let extentDepth = 0;
	let extentCell = 0;
	/** Sea level in cells; empty: 0.3 × depth. */
	let extentSea: number | null = null;
	let extentFor: WorldInfo | null = null;

	$: if (world && world !== extentFor) {
		extentFor = world;
		[extentWidth, extentHeight, extentDepth] = world.dimensions;
		extentCell = world.atlas.cellSize;
		extentSea = world.seaLevelSetting;
	}

	$: extentChanged =
		!!world &&
		(extentWidth !== world.dimensions[0] ||
			extentHeight !== world.dimensions[1] ||
			extentDepth !== world.dimensions[2] ||
			extentCell !== world.atlas.cellSize ||
			seaSetting(extentSea) !== world.seaLevelSetting);

	/** An empty or invalid sea level input means "the default". */
	function seaSetting(value: number | null | string): number | null {
		const n = typeof value === 'string' ? Number(value) : value;
		return n === null || value === '' || !Number.isFinite(n) || n <= 0 ? null : Math.round(n);
	}

	function applyExtent() {
		onExtent({
			size: [Math.round(extentWidth), Math.round(extentHeight)],
			depth: Math.round(extentDepth),
			cellSize: Math.round(extentCell),
			seaLevel: seaSetting(extentSea)
		});
	}

	// --- Atlas: rivers and simulation frames --------------------------------

	let showRivers = false;
	$: onRivers(showRivers);
	// Remembered per machine (a view preference, not part of the project).
	let showGrid = readGridSetting();
	$: onGrid(showGrid);
	$: writeGridSetting(showGrid);

	function readGridSetting(): boolean {
		try {
			return localStorage.getItem('layervis.grid') !== 'off';
		} catch {
			return true;
		}
	}

	function writeGridSetting(visible: boolean) {
		try {
			localStorage.setItem('layervis.grid', visible ? 'on' : 'off');
		} catch {
			// storage unavailable: the setting just isn't remembered
		}
	}

	/** Nodes with scrubber frames, and the selected one. */
	$: frameNodes = [...new Set(world?.atlas.snapshots.map((s) => s.node) ?? [])];
	let frameNode = '';
	let frameIndex = 0;
	let showFrame = false;
	$: if (frameNodes.length && !frameNodes.includes(frameNode)) frameNode = frameNodes[frameNodes.length - 1];
	$: nodeFrames = (world?.atlas.snapshots ?? [])
		.map((snapshot, index) => ({ ...snapshot, index }))
		.filter((snapshot) => snapshot.node === frameNode);
	$: if (frameIndex >= nodeFrames.length) frameIndex = Math.max(0, nodeFrames.length - 1);
	$: currentFrame = nodeFrames[frameIndex];
	$: onSnapshot(showFrame && currentFrame ? currentFrame.index : null);

	// --- Navigation ---------------------------------------------------------

	let goX = 0;
	let goY = 0;

	function goTo() {
		if (Number.isFinite(goX) && Number.isFinite(goY)) onGoTo(Math.round(goX), Math.round(goY));
	}

	const chunkOf = (value: number, origin: number, size: number) =>
		Math.floor((value - origin) / size);
</script>

<aside class="sidebar">
	{#if world}
		<SidebarSection id="world" title="World">
			<div class="field">
				<label for="seed">Seed</label>
				<div class="row">
					<input
						id="seed"
						class="mono"
						class:invalid={seedError}
						inputmode="numeric"
						spellcheck="false"
						autocomplete="off"
						disabled={busy}
						bind:value={seedText}
						on:keydown={(e) => e.key === 'Enter' && e.currentTarget.blur()}
						on:blur={() => applySeed(seedText)}
					/>
					<button
						class="icon"
						title="Random seed"
						aria-label="Random seed"
						disabled={busy}
						on:click={randomSeed}
					>
						🎲
					</button>
					<button
						class="icon"
						title={copied ? 'Copied' : 'Copy seed'}
						aria-label="Copy seed"
						on:click={copySeed}
					>
						{copied ? '✓' : '⧉'}
					</button>
				</div>
				{#if seedError}
					<p class="error">{seedError}</p>
				{:else}
					<p class="hint">Enter to apply · saved with the graph</p>
				{/if}
			</div>

			<div class="field">
				<span class="label">Size · depth · atlas cell · sea level</span>
				<div class="row">
					<input class="mono" type="number" min="256" step="256" title="Width" disabled={busy} bind:value={extentWidth} />
					<input class="mono" type="number" min="256" step="256" title="Height" disabled={busy} bind:value={extentHeight} />
				</div>
				<div class="row">
					<input class="mono" type="number" min="32" step="32" title="Depth (cells)" disabled={busy} bind:value={extentDepth} />
					<input class="mono" type="number" min="1" title="Atlas cell size (columns)" disabled={busy} bind:value={extentCell} />
				</div>
				<div class="row">
					<input
						class="mono"
						type="number"
						min="1"
						placeholder="sea {(extentDepth * 0.3).toFixed(1)}"
						title="Sea level (cells); empty: 0.3 × depth. Height parameters stretch around it."
						disabled={busy}
						bind:value={extentSea}
					/>
				</div>
				{#if extentChanged}
					<button class="small" disabled={busy} on:click={applyExtent}>Apply size (new world)</button>
				{/if}
			</div>

			<dl class="facts">
				<dt>Chunk</dt>
				<dd>{world.chunkSize.join(' × ')}</dd>
				{#if world.atlas.fields.length}
					<dt>Atlas</dt>
					<dd>{world.atlas.grid[0]} × {world.atlas.grid[1]} cells</dd>
				{/if}
			</dl>

			{#if atlasState?.state === 'failed'}
				<p class="error">Atlas build failed: {atlasState.error}</p>
			{:else if world.atlas.pending}
				<p class="hint">Building the atlas…</p>
			{/if}

			{#if stats && world.worldFields.length}
				<p class="stats" class:warn={!edgeOcean}>
					Land {landPercent}% · {stats.landmasses}
					{stats.landmasses === 1 ? 'landmass' : 'landmasses'} ·
					{edgeOcean ? 'ocean on every edge' : `land touches the edge (${Math.round(stats.edgeLand * 100)}%)`}
				</p>
			{/if}
		</SidebarSection>

		<SidebarSection id="view" title="View">
			<div class="field">
				<label for="field">Field</label>
				<select
					id="field"
					value={`${source}:${field}`}
					on:change={(e) => onFieldSelect(e.currentTarget.value)}
				>
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
					{#if world.atlas.fields.length}
						<optgroup label="Atlas grids">
							{#each world.atlas.fields as def, index}
								<option value={`atlas:${index}`}>{label(def)}</option>
							{/each}
						</optgroup>
					{/if}
				</select>
			</div>

			<label class="check">
				<input type="checkbox" bind:checked={showGrid} /> Grid (chunk lines)
			</label>

			{#if world.atlas.fields.length}
				<label class="check">
					<input type="checkbox" bind:checked={showRivers} /> River lines (flow nodes)
				</label>
			{/if}

			<div class="field">
				<label for="ramp">Ramp</label>
				<select id="ramp" bind:value={ramp}>
					{#each RAMPS as name}
						<option value={name}>{name}</option>
					{/each}
				</select>
			</div>

			<div class="legend">
				{#if gradient}
					<div class="bar" style:background={gradient}></div>
					<div class="ends mono">
						<span>{format(min)}</span>
						{#if selectedDef?.unit}<span class="hint">{selectedDef.unit}</span>{/if}
						<span>{format(max)}</span>
					</div>
				{:else if isPaletteRamp(ramp) && world}
					<ul class="palette">
						{#each ramp === 'material' ? world.materials : world.biomes as entry, id}
							<li title="{entry.name} ({id})">
								<span class="swatch" style:background={swatch(entry.color)}></span>{entry.label}
							</li>
						{/each}
					</ul>
				{:else}
					<p class="hint">Categorical: a distinct color per integer value.</p>
				{/if}
			</div>

			<div class="row">
				<div class="field grow">
					<label for="min">Min</label>
					<input id="min" type="number" step="any" bind:value={min} disabled={isCategoricalRamp(ramp)} />
				</div>
				<div class="field grow">
					<label for="max">Max</label>
					<input id="max" type="number" step="any" bind:value={max} disabled={isCategoricalRamp(ramp)} />
				</div>
			</div>

			<div class="row">
				<button
					class="small"
					title="Fit the range to the values currently loaded in view"
					disabled={isCategoricalRamp(ramp)}
					on:click={autoRange}
				>
					Auto range
				</button>
				<button class="small" title="Back to this field's own ramp and range" on:click={resetRange}>
					Reset
				</button>
			</div>
		</SidebarSection>

		{#if frameNodes.length}
			<SidebarSection id="simulation" title="Simulation">
				<div class="field">
					<label for="frame-node">Node</label>
					<select id="frame-node" bind:value={frameNode}>
						{#each frameNodes as node}
							<option value={node}>{node}</option>
						{/each}
					</select>
				</div>
				<label class="check">
					<input type="checkbox" bind:checked={showFrame} /> Show frame (uses the View ramp)
				</label>
				<div class="field">
					<label for="frame">Frame {frameIndex + 1}/{nodeFrames.length}: {currentFrame?.label ?? ''}</label>
					<input
						id="frame"
						type="range"
						min="0"
						max={Math.max(0, nodeFrames.length - 1)}
						bind:value={frameIndex}
						disabled={!showFrame}
					/>
				</div>
			</SidebarSection>
		{/if}

		<SidebarSection id="navigate" title="Navigate">
			<div class="row">
				<div class="field grow">
					<label for="go-x">X</label>
					<input id="go-x" type="number" bind:value={goX} on:keydown={(e) => e.key === 'Enter' && goTo()} />
				</div>
				<div class="field grow">
					<label for="go-y">Y</label>
					<input id="go-y" type="number" bind:value={goY} on:keydown={(e) => e.key === 'Enter' && goTo()} />
				</div>
			</div>
			<div class="row">
				<button class="small" on:click={goTo}>Go to</button>
				<button class="small" title="Zoom out to see the whole world" on:click={onFitWorld}>
					Fit world
				</button>
				<button
					class="small"
					title="Center on the middle of the world"
					on:click={() => onGoTo(0, 0)}
				>
					World center
				</button>
			</div>
		</SidebarSection>

		<SidebarSection id="exports" title="Exports">
			{#each world.exports as exp}
				<button
					class="export"
					title="Game code reads this by name; shows {exp.field}"
					on:click={() => {
						const index = world?.columnFields.findIndex((f) => f.key === exp.field) ?? -1;
						if (index >= 0) selectField('column', index);
					}}
				>
					<strong>{exp.name}</strong> ← {exp.field} <span class="hint">({exp.scope})</span>
				</button>
			{:else}
				<p class="hint">None. Add an Export node to name outputs for game code.</p>
			{/each}
		</SidebarSection>

		<SidebarSection id="materials" title="Materials">
			{#if world.materials.length}
				<ul class="palette">
					{#each world.materials as entry, id}
						<li title="id {id}">
							<span class="swatch" style:background={swatch(entry.color)}></span>
							{entry.label} <span class="hint">{entry.detail}</span>
						</li>
					{/each}
				</ul>
			{:else}
				<p class="hint">No material table.</p>
			{/if}
		</SidebarSection>

		<SidebarSection id="biomes" title="Biomes">
			{#if world.biomes.length}
				<ul class="palette">
					{#each world.biomes as entry, id}
						<li title="{entry.name} · id {id}">
							<span class="swatch" style:background={swatch(entry.color)}></span>
							{entry.label}
						</li>
					{/each}
				</ul>
			{:else}
				<p class="hint">No biome table.</p>
			{/if}
		</SidebarSection>

		<SidebarSection id="cursor" title="Cursor">
			{#if hover}
				<dl class="facts">
					<dt>position</dt>
					<dd>{hover.x}, {hover.y}</dd>
					<dt>chunk</dt>
					<dd>
						{chunkOf(hover.x, world.origin[0], world.chunkSize[0])},
						{chunkOf(hover.y, world.origin[1], world.chunkSize[1])}
					</dd>

					{#each world.columnFields as def, index}
						<dt title="{def.nodeLabel} {def.node}">{def.name} <small>{def.node}</small></dt>
						<dd>{computed(def.key) ? formatField(def, hover.column?.[index]) : 'not yet'}</dd>
					{/each}

					{#if hover.atlasValue !== null}
						<dt>atlas <small>shown</small></dt>
						<dd>{formatField(selectedDef, hover.atlasValue)}</dd>
					{/if}

					{#if hover.worldValues}
						{#each world.worldFields as def, index}
							<dt title="{def.nodeLabel} {def.node}">{def.name} <small>{def.node}</small></dt>
							<dd>{formatField(def, hover.worldValues[index])}</dd>
						{/each}
					{:else}
						<dt>world</dt>
						<dd>outside</dd>
					{/if}
				</dl>
			{:else}
				<p class="hint">Hover the map</p>
			{/if}
		</SidebarSection>
	{/if}

	{#if statusLines.length}
		<footer class="status">
			{#each statusLines as line}
				<p>{line}</p>
			{/each}
		</footer>
	{/if}
</aside>

<style lang="scss">
	@use 'svelte-foundry/colors' as colors;

	.sidebar {
		display: flex;
		flex-direction: column;
		width: 288px;
		flex-shrink: 0;
		overflow-y: auto;
		background-color: colors.$background-mid;
		color: colors.$text-mute;
		border-right: 2px solid colors.$background-light;
		font-size: 13px;
	}

	.field {
		display: flex;
		flex-direction: column;
		gap: 3px;
		min-width: 0;
	}

	.field label {
		font-size: 11px;
		opacity: 0.8;
	}

	.grow {
		flex: 1;
	}

	.row {
		display: flex;
		gap: 6px;
		align-items: stretch;
	}

	select,
	input {
		background-color: colors.$background-light;
		border: 1px solid transparent;
		border-radius: 4px;
		padding: 3px 6px;
		width: 100%;
		min-width: 0;
	}

	input:focus,
	select:focus {
		outline: none;
		border-color: #6fa8ff;
	}

	input.invalid {
		border-color: #ff8c8c;
	}

	input:disabled {
		opacity: 0.5;
	}

	.mono {
		font-family: ui-monospace, 'Cascadia Mono', Consolas, monospace;
		font-size: 12px;
	}

	button {
		border-radius: 4px;
		background-color: colors.$background-light;
	}

	button:hover:not(:disabled) {
		filter: brightness(1.2);
	}

	button:disabled {
		opacity: 0.5;
	}

	button.icon {
		flex-shrink: 0;
		width: 28px;
	}

	button.small {
		flex: 1;
		padding: 3px 8px;
		font-size: 12px;
	}

	.hint {
		font-size: 11px;
		opacity: 0.65;
	}

	.error {
		font-size: 11px;
		color: #ff8c8c;
	}

	.facts {
		display: grid;
		grid-template-columns: auto 1fr;
		gap: 2px 12px;
		font-size: 12px;

		dd {
			text-align: right;
			font-variant-numeric: tabular-nums;
		}
	}



	.stats {
		font-size: 11px;
	}

	.label {
		font-size: 11px;
		opacity: 0.8;
	}

	.check {
		display: flex;
		align-items: center;
		gap: 6px;
		font-size: 12px;

		input {
			width: auto;
		}
	}

	.stats.warn {
		color: #e0c27f;
	}






	.palette {
		list-style: none;
		margin: 0;
		padding: 0;
		display: grid;
		grid-template-columns: 1fr 1fr;
		gap: 2px 8px;
		font-size: 0.85em;
	}

	.palette li {
		display: flex;
		align-items: center;
		gap: 5px;
		white-space: nowrap;
		overflow: hidden;
		text-overflow: ellipsis;
	}

	.swatch {
		flex: none;
		width: 10px;
		height: 10px;
		border-radius: 2px;
		border: 1px solid rgba(0, 0, 0, 0.4);
	}

	.legend {
		display: flex;
		flex-direction: column;
		gap: 3px;
	}

	.bar {
		height: 12px;
		border-radius: 3px;
		border: 1px solid colors.$background-light;
	}

	.ends {
		display: flex;
		justify-content: space-between;
		font-size: 11px;
	}

	.export {
		text-align: left;
		padding: 3px 6px;
		font-size: 12px;
	}

	.status {
		margin-top: auto;
		padding: 10px 16px;
		border-top: 1px solid colors.$background-light;
		font-size: 11px;
		opacity: 0.8;
	}
</style>
