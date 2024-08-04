import * as THREE from 'three';
import { createCamera } from './camera';

export type SceneHandler = {
	resize: () => void;
};

function createRenderer(container: HTMLElement): THREE.WebGLRenderer {
	const renderer = new THREE.WebGLRenderer();
	renderer.setSize(container.clientWidth, container.clientHeight);
	container.appendChild(renderer.domElement);
	return renderer;
}

function createSceneObjects(scene: THREE.Scene): void {
	const geometry = new THREE.BoxGeometry();
	const material = new THREE.MeshBasicMaterial({ color: 0x00ff00 });
	const cube = new THREE.Mesh(geometry, material);
	scene.add(cube);
	scene.background = new THREE.Color(0x1f2129);
}

function createGrid(scene: THREE.Scene, size: number, divisions: number): void {
	const gridHelper = new THREE.GridHelper(size, divisions, 0x3c4055, 0x2d303d);
	gridHelper.rotation.x = Math.PI / 2;
	scene.add(gridHelper);
}

function createTile(x: number, y: number, size: number): THREE.Mesh {
	const geometry = new THREE.PlaneGeometry(size, size);
	const material = new THREE.MeshBasicMaterial({ color: 0x00ff00, side: THREE.DoubleSide });
	const tile = new THREE.Mesh(geometry, material);
	tile.position.set(x, y, 0);
	// tile.rotation.set(-Math.PI / 2, 0, 0);
	return tile;
}

export function createScene(container: HTMLElement): SceneHandler {
	const scene = new THREE.Scene();
	const zoomFactor = 2;
	const camera = createCamera(container, zoomFactor);
	const renderer = createRenderer(container);

	createGrid(scene, 200, 200);
	const tile1 = createTile(0, 0, 1);
	const tile2 = createTile(3, 1, 1);
	scene.add(tile1, tile2);
	createSceneObjects(scene);

	function animate() {
		requestAnimationFrame(animate);

		scene.children.forEach((child) => {
			if (child instanceof THREE.Mesh) {
				// child.rotation.x += 0.01;
				// child.rotation.y += 0.01;
			}
		});

		renderer.render(scene, camera);
	}

	animate();

	return {
		resize: () => {
			const aspect = container.clientWidth / container.clientHeight;
			camera.left = -aspect * zoomFactor;
			camera.right = aspect * zoomFactor;
			camera.top = 1 * zoomFactor;
			camera.bottom = -1 * zoomFactor;
			camera.updateProjectionMatrix();
			renderer.setSize(container.clientWidth, container.clientHeight);
		}
	};
}
