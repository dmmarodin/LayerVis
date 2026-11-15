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
/** Height slice settings (the level slider beside the map). */
export interface SliceView {
	enabled: boolean;
	/** World field index of the terrain height (cells), or null. */
	heightField: number | null;
	/** The level shown (cells). */
	level: number;
	/** Depth (cells) over which lower ground fades into fog. */
	fogDepth: number;
	seaLevel: number;
}

const sliceVertex = /* glsl */ `
	varying vec2 vUv;
	void main() {
		vUv = uv;
		gl_Position = projectionMatrix * modelViewMatrix * vec4(position, 1.0);
	}
`;

/**
 * Drawn over the map: ground above the level is cut away (dark hatching, as
 * a slicer shows the part above the current layer), ground at the level is
 * untouched, and lower ground fades into fog the further down it is (water
 * blue below sea level).
 */
const sliceFragment = /* glsl */ `
	uniform sampler2D height;
	uniform float level;
	uniform float fogDepth;
	uniform float seaLevel;
	varying vec2 vUv;

	void main() {
		float h = texture2D(height, vUv).r;
		if (isnan(h)) discard;
		if (h >= level + 1.0) {
			float stripe = step(0.5, fract((gl_FragCoord.x + gl_FragCoord.y) / 10.0));
			gl_FragColor = vec4(vec3(0.07, 0.075, 0.09), mix(0.72, 0.86, stripe));
			return;
		}
		if (h >= level) discard; // the current layer
		float depth = (level - h) / max(fogDepth, 1.0);
		float fog = 1.0 - exp(-2.2 * depth);
		vec3 haze = level <= seaLevel ? vec3(0.05, 0.16, 0.36) : vec3(0.55, 0.62, 0.72);
		gl_FragColor = vec4(haze, clamp(fog, 0.0, 0.92));
	}
`;

export class WorldOverlay {
	public readonly mesh: THREE.Mesh<THREE.PlaneGeometry, THREE.ShaderMaterial>;
	private grid: FieldGrid | null = null;
	private textures: THREE.DataTexture[] = [];
	private field: number | null = null;
	private view: ViewMode | null = null;
	private slice: SliceView | null = null;
	private readonly sliceMesh: THREE.Mesh<THREE.PlaneGeometry, THREE.ShaderMaterial>;

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

		this.sliceMesh = new THREE.Mesh(
			new THREE.PlaneGeometry(1, 1),
			new THREE.ShaderMaterial({
				vertexShader: sliceVertex,
				fragmentShader: sliceFragment,
				transparent: true,
				depthWrite: false,
				uniforms: {
					height: { value: null },
					level: { value: 0 },
					fogDepth: { value: 24 },
					seaLevel: { value: 0 }
				}
			})
		);
		this.sliceMesh.visible = false;
		this.sliceMesh.renderOrder = 2;
		scene.add(this.sliceMesh);
	}

	/** Shows or hides the height slice; samples the world while it is on. */
	public setSlice(slice: SliceView): void {
		this.slice = slice;
		this.refreshMaterial();
	}

	private get sliceActive(): boolean {
		return !!this.slice?.enabled && this.slice.heightField !== null;
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
		if (this.field === null && !this.sliceActive) return;

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
		this.sliceMesh.scale.set(spanX, spanY, 1);
		// Above chunk tiles and atlas grids, below river lines.
		this.sliceMesh.position.set(grid.originX + spanX / 2, grid.originY + spanY / 2, 0.3);
		this.refreshMaterial();
	}

	private refreshMaterial(): void {
		const texture = this.field === null ? null : (this.textures[this.field] ?? null);
		this.mesh.visible = texture !== null;
		if (this.view) applyFieldView(this.mesh.material, texture, this.view);

		const slice = this.slice;
		const height =
			this.sliceActive && slice?.heightField != null
				? (this.textures[slice.heightField] ?? null)
				: null;
		this.sliceMesh.visible = height !== null;
		if (slice) {
			const u = this.sliceMesh.material.uniforms;
			u.height.value = height;
			u.level.value = slice.level;
			u.fogDepth.value = slice.fogDepth;
			u.seaLevel.value = slice.seaLevel;
		}
	}

	private emit(): void {
		this.onStats({
			step: this.grid && (this.field !== null || this.sliceActive) ? this.grid.step : 0,
			loading: this.inFlight,
			error: this.error
		});
	}
}
