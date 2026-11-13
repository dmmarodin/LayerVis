import * as THREE from 'three';
import { Context } from './context';
import { CameraHandler } from './camera';
import { InfiniteGrid } from './grid';

export class SceneManager {
	public grid: THREE.GridHelper | null = null;

	public constructor() {}

	/** Keeps the finite grid centered under the camera, snapped to chunk lines. */
	public followCamera(x: number, y: number, chunkSize: number): void {
		if (!this.grid) return;
		this.grid.position.x = Math.round(x / chunkSize) * chunkSize;
		this.grid.position.y = Math.round(y / chunkSize) * chunkSize;
	}

	public initializeDefaultScene(parentComponent: HTMLElement) {
		const context: Context = Context.getInstance();
		context.setScene(new THREE.Scene());
		context.setRenderer(this.createRenderer(parentComponent));
		// 1 scene unit = 1 cell; frustum half-height = 96 cells at zoom 1.
		context.setCamera(new CameraHandler(parentComponent, 96));
		context.setParentComponent(parentComponent);

		// One line per 32-cell chunk; drawn just above the tiles.
		// Follows the camera (followCamera), so it only needs to cover the view.
		const grid = InfiniteGrid.create(8192, 256);
		grid.position.z = 0.1;
		this.grid = grid;
		context.scene.background = new THREE.Color(0x1f2129);
		context.scene.add(grid);
		this.update();
	}

	private createRenderer(container: HTMLElement): THREE.WebGLRenderer {
		const renderer = new THREE.WebGLRenderer();
		renderer.setSize(container.clientWidth, container.clientHeight);
		container.appendChild(renderer.domElement);
		return renderer;
	}

	private update(): void {
		requestAnimationFrame(this.update.bind(this));

		const context: Context = Context.getInstance();
		// Do updates
		context.renderer.render(context.scene, context.camera);
	}
}
