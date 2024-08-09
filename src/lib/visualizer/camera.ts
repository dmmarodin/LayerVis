// camera.ts
import * as THREE from 'three';

export class CameraHandler extends THREE.OrthographicCamera {
	private currentZoom: number = 6;

	public constructor(container: HTMLElement, defaultZoom: number) {
		const aspect = container.clientWidth / container.clientHeight;
		super(
			-aspect * defaultZoom,
			aspect * defaultZoom,
			1 * defaultZoom,
			-1 * defaultZoom,
			0.1,
			1000
		);

		this.currentZoom = defaultZoom;
		this.position.set(0, 0, 10);
		this.lookAt(0, 0, 0);
		addCameraControls(container, this);
	}

	public resize(container: HTMLElement): void {
		const aspect = container.clientWidth / container.clientHeight;
		this.left = -aspect * this.currentZoom;
		this.right = aspect * this.currentZoom;
		this.top = 1 * this.currentZoom;
		this.bottom = -1 * this.currentZoom;
		this.updateProjectionMatrix();
	}
}

export function addCameraControls(container: HTMLElement, camera: THREE.OrthographicCamera): void {
	function onWheel(event: WheelEvent) {
		const zoomSpeed = 1.1;
		const delta = event.deltaY < 0 ? zoomSpeed : 1 / zoomSpeed;
		camera.zoom = Math.max(0.02, camera.zoom * delta);
		camera.updateProjectionMatrix();
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
	}

	function onMouseUp() {
		isPanning = false;
	}

	container.addEventListener('wheel', onWheel);
	container.addEventListener('mousedown', onMouseDown);
	container.addEventListener('mousemove', onMouseMove);
	container.addEventListener('mouseup', onMouseUp);
	container.addEventListener('mouseleave', onMouseUp);
}
