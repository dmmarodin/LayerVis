import * as THREE from 'three';
import { Context } from './context';
import { CameraHandler } from './camera';
import { InfiniteGrid } from './grid';

export class SceneManager {
	public constructor() {}

	public initializeDefaultScene(parentComponent: HTMLElement) {
		const context: Context = Context.getInstance();
		context.setScene(new THREE.Scene());
		context.setRenderer(this.createRenderer(parentComponent));
		context.setCamera(new CameraHandler(parentComponent, 2));
		context.setParentComponent(parentComponent);

		const grid = InfiniteGrid.create(100, 100);
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
