import type * as THREE from 'three';
import { generateChunk, type WorldInfo } from '../bridge';
import type { ViewBounds } from './camera';
import { ChunkTile, type RenderView } from './tiles';
import { sampleGrid } from './fieldMaterial';

/** Above this many visible chunks, stop requesting (zoomed-out views come later). */
export const MAX_VISIBLE_CHUNKS = 1024;
/** GPU tiles kept resident; least-recently-visible off-screen tiles are evicted. */
const MAX_TILES = 1600;
/** Concurrent backend requests. */
const MAX_IN_FLIGHT = 6;
/** Extra ring of chunks requested around the viewport. */
const MARGIN = 1;

export interface StreamStats {
	loaded: number;
	visible: number;
	queued: number;
	inFlight: number;
	failed: number;
	/** True when zoomed out past MAX_VISIBLE_CHUNKS; nothing is requested. */
	tooFar: boolean;
	/** False while a world-field view is active; chunks are hidden and not requested. */
	enabled: boolean;
}

interface Resident {
	tile: ChunkTile;
	lastVisible: number;
}

interface Range {
	x0: number;
	x1: number;
	y0: number;
	y1: number;
}

const key = (x: number, y: number) => `${x},${y}`;

/**
 * Camera-driven chunk loading: requests visible chunks nearest the center
 * first, drops queued work that scrolled away, and evicts old tiles.
 */
export class ChunkStreamer {
	private resident = new Map<string, Resident>();
	private inFlight = new Set<string>();
	private failed = new Set<string>();
	private queue: Array<[number, number]> = [];
	private frame = 0;
	private visibleCount = 0;
	private tooFar = false;
	private enabled = true;
	private disposed = false;

	public constructor(
		private world: WorldInfo,
		private scene: THREE.Scene,
		private getView: () => RenderView,
		private onStats: (stats: StreamStats) => void
	) {}

	public setEnabled(enabled: boolean): void {
		if (this.enabled === enabled) return;
		this.enabled = enabled;
		this.resident.forEach(({ tile }) => (tile.mesh.visible = enabled));
		if (!enabled) this.queue = [];
		this.emit();
	}

	/** Column field values at a world column, or null if its chunk is not loaded. */
	public columnValuesAt(x: number, y: number): number[] | null {
		const [cx, cy] = this.chunkAt(x, y);
		const resident = this.resident.get(key(cx, cy));
		if (!resident) return null;

		const grid = resident.tile.columns;
		const values = grid.fields.map((_, field) => sampleGrid(grid, field, x, y));
		return values.some((value) => value === null) ? null : (values as number[]);
	}

	public update(bounds: ViewBounds): void {
		if (!this.enabled) return;
		this.frame++;

		const visible = this.chunkRange(bounds, 0);
		this.visibleCount = rangeSize(visible);
		this.tooFar = this.visibleCount > MAX_VISIBLE_CHUNKS;

		if (this.tooFar) {
			this.queue = [];
			this.emit();
			return;
		}

		for (let y = visible.y0; y <= visible.y1; y++) {
			for (let x = visible.x0; x <= visible.x1; x++) {
				const resident = this.resident.get(key(x, y));
				if (resident) resident.lastVisible = this.frame;
			}
		}

		const wanted = this.chunkRange(bounds, MARGIN);
		const [cx, cy] = this.chunkAt(bounds.centerX, bounds.centerY);
		const queue: Array<[number, number]> = [];

		for (let y = wanted.y0; y <= wanted.y1; y++) {
			for (let x = wanted.x0; x <= wanted.x1; x++) {
				const k = key(x, y);
				if (!this.resident.has(k) && !this.inFlight.has(k) && !this.failed.has(k)) {
					queue.push([x, y]);
				}
			}
		}

		// Nearest to the view center first.
		queue.sort(
			([ax, ay], [bx, by]) =>
				(ax - cx) ** 2 + (ay - cy) ** 2 - ((bx - cx) ** 2 + (by - cy) ** 2)
		);

		// Replacing the queue drops work that scrolled out of range.
		this.queue = queue;
		this.evict(wanted);
		this.pump();
		this.emit();
	}

	public applyView(view: RenderView): void {
		this.resident.forEach(({ tile }) => tile.applyView(view));
	}

	/** Removes all tiles; in-flight responses are discarded when they land. */
	public dispose(): void {
		this.disposed = true;
		this.queue = [];
		this.resident.forEach(({ tile }) => {
			this.scene.remove(tile.mesh);
			tile.dispose();
		});
		this.resident.clear();
	}

	private pump(): void {
		while (this.inFlight.size < MAX_IN_FLIGHT && this.queue.length > 0) {
			const [x, y] = this.queue.shift()!;
			this.request(x, y);
		}
	}

	private async request(x: number, y: number): Promise<void> {
		const k = key(x, y);
		this.inFlight.add(k);

		try {
			const columns = await generateChunk(x, y, 0);

			// Superseded by a pipeline edit: never mix schemas.
			if (this.disposed || columns.revision !== (this.world.revision >>> 0)) return;

			const tile = new ChunkTile(columns, this.getView());
			tile.mesh.visible = this.enabled;

			this.resident.set(k, { tile, lastVisible: this.frame });
			this.scene.add(tile.mesh);
		} catch (error) {
			console.error(`Chunk ${k} failed`, error);
			this.failed.add(k);
		} finally {
			this.inFlight.delete(k);
			if (!this.disposed) {
				this.pump();
				this.emit();
			}
		}
	}

	private evict(keep: Range): void {
		if (this.resident.size <= MAX_TILES) return;

		const candidates = [...this.resident.entries()]
			.filter(([k]) => {
				const [x, y] = k.split(',').map(Number);
				return x < keep.x0 || x > keep.x1 || y < keep.y0 || y > keep.y1;
			})
			.sort(([, a], [, b]) => a.lastVisible - b.lastVisible);

		for (const [k, { tile }] of candidates) {
			if (this.resident.size <= MAX_TILES) break;
			this.scene.remove(tile.mesh);
			tile.dispose();
			this.resident.delete(k);
		}
	}

	private chunkAt(worldX: number, worldY: number): [number, number] {
		const [sx, sy] = this.world.chunkSize;
		return [
			Math.floor((worldX - this.world.origin[0]) / sx),
			Math.floor((worldY - this.world.origin[1]) / sy)
		];
	}

	/** Chunk range covering the bounds (+ margin), clamped to the world. */
	private chunkRange(bounds: ViewBounds, margin: number): Range {
		const [sx, sy] = this.world.chunkSize;
		const maxX = Math.ceil(this.world.dimensions[0] / sx) - 1;
		const maxY = Math.ceil(this.world.dimensions[1] / sy) - 1;
		const [x0, y0] = this.chunkAt(bounds.minX, bounds.minY);
		const [x1, y1] = this.chunkAt(bounds.maxX, bounds.maxY);

		// View entirely outside the world: nothing to load.
		if (x1 + margin < 0 || y1 + margin < 0 || x0 - margin > maxX || y0 - margin > maxY) {
			return { x0: 0, x1: -1, y0: 0, y1: -1 };
		}

		return {
			x0: clamp(x0 - margin, 0, maxX),
			x1: clamp(x1 + margin, 0, maxX),
			y0: clamp(y0 - margin, 0, maxY),
			y1: clamp(y1 + margin, 0, maxY)
		};
	}

	private emit(): void {
		this.onStats({
			loaded: this.resident.size,
			visible: this.visibleCount,
			queued: this.queue.length,
			inFlight: this.inFlight.size,
			failed: this.failed.size,
			tooFar: this.tooFar,
			enabled: this.enabled
		});
	}
}

function clamp(value: number, min: number, max: number): number {
	return Math.min(max, Math.max(min, value));
}

function rangeSize(range: Range): number {
	if (range.x1 < range.x0 || range.y1 < range.y0) return 0;
	return (range.x1 - range.x0 + 1) * (range.y1 - range.y0 + 1);
}
