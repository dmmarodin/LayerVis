import * as THREE from 'three';
import { sampleWorld, type FieldGrid } from '../bridge';
import type { ViewBounds } from './camera';
import {
	applyFieldView,
	createFieldMaterial,
	createFieldTextures,
	finiteRange,
	type ViewMode
} from './fieldMaterial';

/** Target samples across the viewport width (before the margin). */
const TARGET_SAMPLES = 384;
/** Fraction of the viewport sampled beyond each edge, so panning shows data. */
const MARGIN = 0.25;
const DEBOUNCE_MS = 80;

export interface OverlayStats {
	/** Cells per sample of the displayed grid; 0 when nothing is displayed. */
	step: number;
	loading: boolean;
	error: string | null;
}

interface GridRequest {
	x: number;
	y: number;
	step: number;
	width: number;
	height: number;
}

/**
 * Screen-resolution sampling of analytic world fields, drawn under chunk
 * tiles. Works at any zoom, so it is the zoomed-out view and the backdrop
 * while chunks stream in.
 */
export class WorldOverlay {
	public readonly mesh: THREE.Mesh<THREE.PlaneGeometry, THREE.ShaderMaterial>;
	private grid: FieldGrid | null = null;
	private textures: THREE.DataTexture[] = [];
	private field: number | null = null;
	private view: ViewMode | null = null;

	/** Bumped by `invalidate`; samples requested before it are discarded. */
	private epoch = 0;
	private inFlight = false;
	private pending: GridRequest | null = null;
	private timer: ReturnType<typeof setTimeout> | null = null;
	private error: string | null = null;

	public constructor(
		scene: THREE.Scene,
		private onStats: (stats: OverlayStats) => void
	) {
		this.mesh = new THREE.Mesh(new THREE.PlaneGeometry(1, 1), createFieldMaterial());
		this.mesh.position.z = -0.5;
		this.mesh.visible = false;
		scene.add(this.mesh);
	}

	/** Shows world field `field` (index into worldFields), or hides the overlay. */
	public setField(field: number | null, view: ViewMode): void {
		this.field = field;
		this.view = view;
		this.refreshMaterial();
	}

	/** Drops the sampled grid (the world changed, e.g. a new seed) and resamples. */
	public invalidate(): void {
		this.epoch++;
		this.pending = null;
		this.grid = null;
		this.textures.forEach((texture) => texture.dispose());
		this.textures = [];
		this.refreshMaterial();
		this.emit();
	}

	/** Value range of a world field over the current grid (null if none). */
	public fieldRange(field: number): [number, number] | null {
		const plane = this.grid?.fields[field];
		return plane ? finiteRange([plane]) : null;
	}

	/** Requests a new grid for the viewport unless the current one still covers it. */
	public update(bounds: ViewBounds, viewportPixels: number): void {
		if (this.field === null) return;

		const request = this.plan(bounds, viewportPixels);
		if (this.covers(bounds, request.step)) return;

		this.pending = request;
		if (this.timer) clearTimeout(this.timer);
		this.timer = setTimeout(() => this.flush(), DEBOUNCE_MS);
	}

	private plan(bounds: ViewBounds, viewportPixels: number): GridRequest {
		const viewWidth = bounds.maxX - bounds.minX;
		const viewHeight = bounds.maxY - bounds.minY;
		const target = Math.max(64, Math.min(TARGET_SAMPLES, viewportPixels / 2));

		// Power-of-two steps keep sample positions stable while zooming.
		const rawStep = Math.max(1, viewWidth / target);
		const step = 2 ** Math.ceil(Math.log2(rawStep));

		const x = Math.floor((bounds.minX - viewWidth * MARGIN) / step) * step;
		const y = Math.floor((bounds.minY - viewHeight * MARGIN) / step) * step;
		const width = Math.ceil((viewWidth * (1 + 2 * MARGIN)) / step) + 1;
		const height = Math.ceil((viewHeight * (1 + 2 * MARGIN)) / step) + 1;

		return { x, y, step, width, height };
	}

	private covers(bounds: ViewBounds, step: number): boolean {
		const grid = this.grid;
		if (!grid || grid.step !== step) return false;

		return (
			bounds.minX >= grid.originX &&
			bounds.minY >= grid.originY &&
			bounds.maxX <= grid.originX + grid.width * grid.step &&
			bounds.maxY <= grid.originY + grid.height * grid.step
		);
	}

	private async flush(): Promise<void> {
		this.timer = null;
		if (this.inFlight || !this.pending) return;

		const request = this.pending;
		this.pending = null;
		this.inFlight = true;
		const epoch = this.epoch;
		this.emit();

		try {
			const grid = await sampleWorld(
				request.x,
				request.y,
				request.step,
				request.width,
				request.height
			);
			if (epoch === this.epoch) this.show(grid);
			this.error = null;
		} catch (error) {
			console.error('World sampling failed', error);
			this.error = String(error);
		} finally {
			this.inFlight = false;
			this.emit();
			// The camera may have moved while sampling; serve the latest request.
			if (this.pending) this.flush();
		}
	}

	private show(grid: FieldGrid): void {
		this.textures.forEach((texture) => texture.dispose());
		this.grid = grid;
		this.textures = createFieldTextures(grid);

		const spanX = grid.width * grid.step;
		const spanY = grid.height * grid.step;
		this.mesh.scale.set(spanX, spanY, 1);
		this.mesh.position.set(grid.originX + spanX / 2, grid.originY + spanY / 2, -0.5);
		this.refreshMaterial();
	}

	private refreshMaterial(): void {
		const texture = this.field === null ? null : (this.textures[this.field] ?? null);
		this.mesh.visible = texture !== null;
		if (this.view) applyFieldView(this.mesh.material, texture, this.view);
	}

	private emit(): void {
		this.onStats({
			step: this.grid && this.field !== null ? this.grid.step : 0,
			loading: this.inFlight,
			error: this.error
		});
	}
}
