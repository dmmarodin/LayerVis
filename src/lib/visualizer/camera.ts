// camera.ts
import * as THREE from 'three';

export const MIN_ZOOM = 0.02;
export const MAX_ZOOM = 20;

/** Visible world rectangle in cells. */
export interface ViewBounds {
	minX: number;
	maxX: number;
	minY: number;
	maxY: number;
	centerX: number;
	centerY: number;
}

export class CameraHandler extends THREE.OrthographicCamera {
	private zoomFactor: number = 2;
	private listeners: Array<() => void> = [];

	public constructor(container: HTMLElement, zoomFactor: number) {
		const aspect = container.clientWidth / container.clientHeight;
		super(-aspect * zoomFactor, aspect * zoomFactor, 1 * zoomFactor, -1 * zoomFactor, 0.1, 1000);

		this.zoomFactor = zoomFactor;
		this.position.set(0, 0, 10);
		this.lookAt(0, 0, 0);
		addCameraControls(container, this);
	}

	public resize(container: HTMLElement): void {
		const aspect = container.clientWidth / container.clientHeight;
		this.left = -aspect * this.zoomFactor;
		this.right = aspect * this.zoomFactor;
		this.top = 1 * this.zoomFactor;
		this.bottom = -1 * this.zoomFactor;
		this.updateProjectionMatrix();
		this.notifyChange();
	}

	/**
	 * Note on precision: three.js multiplies camera and object matrices in
	 * JS doubles before uploading, and tile geometry is chunk-local, so large
	 * world coordinates do not jitter. No origin rebasing is needed.
	 */
	public viewBounds(): ViewBounds {
		const halfWidth = (this.right - this.left) / 2 / this.zoom;
		const halfHeight = (this.top - this.bottom) / 2 / this.zoom;
		const { x, y } = this.position;

		return {
			minX: x - halfWidth,
			maxX: x + halfWidth,
			minY: y - halfHeight,
			maxY: y + halfHeight,
			centerX: x,
			centerY: y
		};
	}

	public onChange(listener: () => void): () => void {
		this.listeners.push(listener);
		return () => {
			this.listeners = this.listeners.filter((l) => l !== listener);
		};
	}

	public notifyChange(): void {
		this.listeners.forEach((listener) => listener());
	}
}

export function addCameraControls(container: HTMLElement, camera: CameraHandler): void {
	function onWheel(event: WheelEvent) {
		event.preventDefault();
		const factor = Math.pow(1.1, -Math.sign(event.deltaY));
		camera.zoom = Math.min(MAX_ZOOM, Math.max(MIN_ZOOM, camera.zoom * factor));
		camera.updateProjectionMatrix();
		camera.notifyChange();
	}

	let isPanning = false;
	let startX = 0;
	let startY = 0;

	function onMouseDown(event: MouseEvent) {
		isPanning = true;
		startX = event.clientX;
		startY = event.clientY;
	}

	function onMouseMove(event: MouseEvent) {
		if (!isPanning) return;

		const deltaX =
			(((startX - event.clientX) / container.clientWidth) * (camera.right - camera.left)) /
			camera.zoom;
		const deltaY =
			(((startY - event.clientY) / container.clientHeight) * (camera.top - camera.bottom)) /
			camera.zoom;

		camera.position.x += deltaX;
		camera.position.y -= deltaY;

		startX = event.clientX;
		startY = event.clientY;
		camera.notifyChange();
	}

	function onMouseUp() {
		isPanning = false;
	}

	container.addEventListener('wheel', onWheel, { passive: false });
	container.addEventListener('mousedown', onMouseDown);
	container.addEventListener('mousemove', onMouseMove);
	container.addEventListener('mouseup', onMouseUp);
	container.addEventListener('mouseleave', onMouseUp);
}
