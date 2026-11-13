import { Context } from './context';
import { SceneManager } from './scene';
import type { RenderView, ViewMode } from './fieldMaterial';
import { ChunkStreamer, type StreamStats } from './streaming';
import { WorldOverlay, type OverlayStats } from './overlay';
import { worldPoint, type WorldInfo, type WorldSample } from '../bridge';

export interface HoverInfo {
	x: number;
	y: number;
	/** Column field values from the loaded chunk, in columnFields order. */
	column: number[] | null;
	/** Exact world-model values at the column (null outside the world). */
	world: WorldSample | null;
}

export interface VisualizerCallbacks {
	onStream: (stats: StreamStats) => void;
	onOverlay: (stats: OverlayStats) => void;
	onHover: (hover: HoverInfo | null) => void;
}

// Public facing API for the visualizer
export class Visualizer {
	private scene: SceneManager;
	private world: WorldInfo | null = null;
	private streamer: ChunkStreamer | null = null;
	private overlay: WorldOverlay | null = null;
	private callbacks: VisualizerCallbacks | null = null;
	private view: RenderView = {
		source: 'column',
		field: 0,
		ramp: 'terrain',
		min: 0,
		max: 1,
		hidden: false
	};
	/** Column fields computed at the selected step; null = full pipeline. */
	private stageFields: Set<string> | null = null;
	private updateScheduled = false;
	private hoverInFlight = false;
	private hoverPending: { x: number; y: number } | null = null;

	public constructor(parentComponent: HTMLElement) {
		this.scene = new SceneManager();
		this.scene.initializeDefaultScene(parentComponent);

		const context = Context.getInstance();
		context.camera.onChange(() => this.scheduleUpdate());
		parentComponent.addEventListener('mousemove', (event) => this.onMouseMove(event));
		parentComponent.addEventListener('mouseleave', () => this.callbacks?.onHover(null));
	}

	/**
	 * Starts camera-driven chunk streaming and world sampling for a world.
	 * Call again with fresh WorldInfo after a pipeline edit: chunk tiles are
	 * rebuilt for the new revision; the world overlay is kept (world models
	 * do not change with the pipeline).
	 */
	public start(world: WorldInfo, callbacks: VisualizerCallbacks): void {
		const scene = Context.getInstance().scene;

		this.streamer?.dispose();
		// A new seed changes the world models too, not just the pipeline.
		if (this.world && this.world.seed !== world.seed) this.overlay?.invalidate();
		this.world = world;
		this.callbacks = callbacks;
		this.streamer = new ChunkStreamer(world, scene, () => this.view, callbacks.onStream);
		this.overlay ??= new WorldOverlay(scene, callbacks.onOverlay);
		this.refreshView();
	}

	/**
	 * Column views stream chunks, backed by the world field of the same name
	 * (if any) for zoomed-out areas. World views show only the overlay.
	 */
	public setViewMode(view: ViewMode): void {
		this.view = { ...view, hidden: false };
		this.refreshView();
	}

	/**
	 * Mid-pipeline view: only these column field keys (`node.socket`) count
	 * as computed. Every output has its own slot, so the state after node k
	 * is exactly the final state restricted to outputs of nodes 1..k — no
	 * regeneration needed.
	 */
	public setStage(fields: string[] | null): void {
		this.stageFields = fields ? new Set(fields) : null;
		this.refreshView();
	}

	/** Centers the camera on a world column (zoom is kept). */
	public goTo(x: number, y: number): void {
		const camera = Context.getInstance().camera;
		camera.position.set(x, y, camera.position.z);
		camera.notifyChange();
	}

	/** Value range of the selected field in what is currently loaded and in view. */
	public dataRange(): [number, number] | null {
		const bounds = Context.getInstance().camera.viewBounds();
		const { source, field } = this.view;

		return source === 'column'
			? (this.streamer?.visibleRange(bounds, field) ?? null)
			: (this.overlay?.fieldRange(field) ?? null);
	}

	private refreshView(): void {
		if (!this.world) return;

		const view = this.view;
		const isColumn = view.source === 'column';
		const field = isColumn ? this.world.columnFields[view.field] : undefined;
		const name = field?.name;
		// Stage fields are keys (`node.socket`): socket names are not unique.
		view.hidden = isColumn && !!this.stageFields && !(field && this.stageFields.has(field.key));

		let overlayField: number | null = isColumn ? null : view.field;

		// Same-named world field backs a column view when zoomed out, unless
		// the field is not computed yet at the selected step.
		if (isColumn && !view.hidden) {
			const match = this.world.worldFields.findIndex((field) => field.name === name);
			overlayField = match >= 0 ? match : null;
		}

		this.streamer?.setEnabled(isColumn);
		this.streamer?.applyView(this.view);
		this.overlay?.setField(overlayField, this.view);
		this.scheduleUpdate();
	}

	public resize() {
		const context = Context.getInstance();
		const container = context.parentComponent;
		context.camera.resize(container);
		context.renderer.setSize(container.clientWidth, container.clientHeight);
	}

	/** Coalesces camera events into one streaming update per frame. */
	private scheduleUpdate(): void {
		if (this.updateScheduled) return;
		this.updateScheduled = true;

		requestAnimationFrame(() => {
			this.updateScheduled = false;
			const context = Context.getInstance();
			const bounds = context.camera.viewBounds();
			const chunkSize = this.world?.chunkSize[0] ?? 32;

			this.scene.followCamera(bounds.centerX, bounds.centerY, chunkSize);
			this.streamer?.update(bounds);
			this.overlay?.update(bounds, context.parentComponent.clientWidth);
		});
	}

	private onMouseMove(event: MouseEvent): void {
		if (!this.callbacks) return;

		const context = Context.getInstance();
		const container = context.parentComponent;
		const rect = container.getBoundingClientRect();
		const bounds = context.camera.viewBounds();

		const u = (event.clientX - rect.left) / rect.width;
		const v = (event.clientY - rect.top) / rect.height;
		const x = Math.floor(bounds.minX + u * (bounds.maxX - bounds.minX));
		const y = Math.floor(bounds.maxY - v * (bounds.maxY - bounds.minY));

		this.hoverPending = { x, y };
		this.flushHover();
	}

	/** One world_point query in flight; the latest cursor position wins. */
	private async flushHover(): Promise<void> {
		if (this.hoverInFlight || !this.hoverPending || !this.callbacks) return;

		const { x, y } = this.hoverPending;
		this.hoverPending = null;
		this.hoverInFlight = true;

		try {
			const world = await worldPoint(x, y);
			const column = this.streamer?.columnValuesAt(x, y) ?? null;
			this.callbacks.onHover({ x, y, column, world });
		} catch (error) {
			console.error('world_point failed', error);
		} finally {
			this.hoverInFlight = false;
			if (this.hoverPending) this.flushHover();
		}
	}
}
