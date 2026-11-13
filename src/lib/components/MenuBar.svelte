<script lang="ts">
	import type { ProjectState } from '$lib/bridge';

	interface Props {
		project: ProjectState | null;
		showGraph: boolean;
		busy?: boolean;
		error?: string | null;
		onNew: () => void;
		onOpen: () => void;
		onOpenRecent: (path: string) => void;
		onSave: () => void;
		onSaveAs: () => void;
		onExport: () => void;
		onUndo: () => void;
		onRedo: () => void;
		onToggleGraph: () => void;
		onDismissError?: () => void;
	}

	let {
		project,
		showGraph,
		busy = false,
		error = null,
		onNew,
		onOpen,
		onOpenRecent,
		onSave,
		onSaveAs,
		onExport,
		onUndo,
		onRedo,
		onToggleGraph,
		onDismissError
	}: Props = $props();

	type MenuId = 'file' | 'edit' | 'view';
	type Item =
		| { kind: 'item'; label: string; shortcut?: string; disabled?: boolean; run: () => void }
		| { kind: 'separator' }
		| { kind: 'header'; label: string };

	let open = $state<MenuId | null>(null);
	let bar: HTMLElement | null = $state(null);

	const fileName = (path: string) => path.split(/[\\/]/).pop() ?? path;

	const menus: Record<MenuId, () => Item[]> = {
		file: () => {
			const items: Item[] = [
				{ kind: 'item', label: 'New project', shortcut: 'Ctrl+N', run: onNew },
				{ kind: 'item', label: 'Open…', shortcut: 'Ctrl+O', run: onOpen },
				{ kind: 'separator' },
				{ kind: 'item', label: 'Save', shortcut: 'Ctrl+S', run: onSave },
				{ kind: 'item', label: 'Save as…', shortcut: 'Ctrl+Shift+S', run: onSaveAs },
				{ kind: 'item', label: 'Export a copy…', run: onExport }
			];

			const recent = project?.recent ?? [];
			if (recent.length) {
				items.push({ kind: 'separator' }, { kind: 'header', label: 'Recent' });
				for (const path of recent) {
					items.push({ kind: 'item', label: fileName(path), run: () => onOpenRecent(path) });
				}
			}
			return items;
		},
		edit: () => [
			{
				kind: 'item',
				label: 'Undo',
				shortcut: 'Ctrl+Z',
				disabled: !project?.canUndo,
				run: onUndo
			},
			{
				kind: 'item',
				label: 'Redo',
				shortcut: 'Ctrl+Shift+Z',
				disabled: !project?.canRedo,
				run: onRedo
			}
		],
		view: () => [
			{
				kind: 'item',
				label: showGraph ? 'Hide node graph' : 'Show node graph',
				run: onToggleGraph
			}
		]
	};

	const titles: Record<MenuId, string> = { file: 'File', edit: 'Edit', view: 'View' };
	const ids = Object.keys(menus) as MenuId[];

	function choose(item: Extract<Item, { kind: 'item' }>) {
		if (item.disabled || busy) return;
		open = null;
		item.run();
	}

	function onWindowPointerDown(event: PointerEvent) {
		if (open && bar && !bar.contains(event.target as Node)) open = null;
	}

	function onWindowKeyDown(event: KeyboardEvent) {
		if (event.key === 'Escape') open = null;
	}
</script>

<svelte:window onpointerdown={onWindowPointerDown} onkeydown={onWindowKeyDown} />

<div class="menubar" bind:this={bar}>
	{#each ids as id}
		<div class="menu">
			<button
				class="title"
				class:active={open === id}
				onclick={() => (open = open === id ? null : id)}
				onpointerenter={() => open && (open = id)}
			>
				{titles[id]}
			</button>

			{#if open === id}
				<div class="dropdown" role="menu">
					{#each menus[id]() as item}
						{#if item.kind === 'separator'}
							<hr />
						{:else if item.kind === 'header'}
							<div class="header">{item.label}</div>
						{:else}
							<button
								class="item"
								role="menuitem"
								disabled={item.disabled || busy}
								onclick={() => choose(item)}
							>
								<span class="label">{item.label}</span>
								{#if item.shortcut}<span class="shortcut">{item.shortcut}</span>{/if}
							</button>
						{/if}
					{/each}
				</div>
			{/if}
		</div>
	{/each}

	<div class="project" title={project?.path ?? 'Not saved to a file yet'}>
		<strong>{project?.name ?? ''}</strong>{project?.dirty ? ' •' : ''}
		{#if project && !project.path}<span class="muted">(unsaved file)</span>{/if}
	</div>

	{#if error}
		<button class="error" title="Click to dismiss" onclick={onDismissError}>{error}</button>
	{/if}
</div>

<style>
	.menubar {
		display: flex;
		align-items: center;
		gap: 2px;
		height: 28px;
		padding: 0 6px;
		flex-shrink: 0;
		background: #23252f;
		border-bottom: 1px solid #343846;
		color: #c3c6d2;
		font-size: 12px;
		user-select: none;
	}

	.menu {
		position: relative;
	}

	.title {
		padding: 3px 10px;
		border-radius: 4px;
		color: inherit;
	}

	.title:hover,
	.title.active {
		background: #343846;
	}

	.dropdown {
		position: absolute;
		top: 100%;
		left: 0;
		z-index: 50;
		min-width: 230px;
		padding: 4px;
		background: #2a2d39;
		border: 1px solid #3d4152;
		border-radius: 6px;
		box-shadow: 0 8px 24px #0008;
	}

	.item {
		display: flex;
		justify-content: space-between;
		gap: 24px;
		width: 100%;
		padding: 4px 10px;
		border-radius: 4px;
		text-align: left;
		color: inherit;
	}

	.item:hover:not(:disabled) {
		background: #3a4a78;
	}

	.item:disabled {
		opacity: 0.4;
	}

	.shortcut {
		color: #8a8fa6;
	}

	.label {
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
	}

	.header {
		padding: 4px 10px 2px;
		color: #8a8fa6;
		font-size: 11px;
		text-transform: uppercase;
	}

	hr {
		margin: 4px 0;
		border: 0;
		border-top: 1px solid #3d4152;
	}

	.project {
		flex: 1;
		min-width: 0;
		text-align: center;
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
	}

	.muted {
		color: #8a8fa6;
	}

	.error {
		max-width: 40%;
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
		color: #ff8c8c;
	}
</style>
