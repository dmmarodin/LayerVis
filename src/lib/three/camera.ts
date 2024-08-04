// camera.ts
import * as THREE from 'three';

export function createCamera(container: HTMLElement, zoomFactor: number): THREE.OrthographicCamera {
	const aspect = container.clientWidth / container.clientHeight;
	const camera = new THREE.OrthographicCamera(
		-aspect * zoomFactor,
		aspect * zoomFactor,
		1 * zoomFactor,
		-1 * zoomFactor,
		0.1,
		1000
	);
	camera.position.z = 5;
	addCameraControls(container, camera);
	return camera;
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
