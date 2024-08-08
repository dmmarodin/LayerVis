// camera.ts
import * as THREE from 'three';

export class CameraHandler extends THREE.OrthographicCamera {
	private zoomFactor: number = 2;

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
	}
}

export function addCameraControls(container: HTMLElement, camera: THREE.OrthographicCamera): void {
	function onWheel(event: WheelEvent) {
		const delta = Math.sign(event.deltaY) * 0.1;
		camera.zoom = Math.max(0.1, camera.zoom - delta);
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
