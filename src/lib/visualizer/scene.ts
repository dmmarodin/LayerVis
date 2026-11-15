import * as THREE from 'three';
import { Context } from './context';
import { CameraHandler } from './camera';
import { InfiniteGrid } from './grid';

export class SceneManager {
	public grid: InfiniteGrid | null = null;

	public constructor() {}

	/** Keeps the grid covering the view; its lines stay on world (chunk) lines. */
	public followCamera(
		bounds: { minX: number; minY: number; maxX: number; maxY: number },
		chunkSize: number
	): void {
		this.grid?.cover(bounds.minX, bounds.minY, bounds.maxX, bounds.maxY, chunkSize);
	}

	public setGridVisible(visible: boolean): void {
		if (this.grid) this.grid.mesh.visible = visible;
	}

	public initializeDefaultScene(parentComponent: HTMLElement) {
		const context: Context = Context.getInstance();
		context.setScene(new THREE.Scene());
		context.setRenderer(this.createRenderer(parentComponent));
		// 1 scene unit = 1 cell; frustum half-height = 96 cells at zoom 1.
		context.setCamera(new CameraHandler(parentComponent, 96));
		context.setParentComponent(parentComponent);

		// Lines on chunk borders (coarser when zoomed out); covers the view
		// (followCamera) while its lines stay fixed in the world.
		this.grid = new InfiniteGrid(32);
		context.scene.background = new THREE.Color(0x1f2129);
		context.scene.add(this.grid.mesh);
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
