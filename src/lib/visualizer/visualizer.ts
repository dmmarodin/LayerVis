import { Context } from './context';
import { SceneManager } from './scene';

// Public facing API for visualizer
export class Visualizer {
	private scene: SceneManager;

	public constructor(parentComponent: HTMLElement) {
		this.scene = new SceneManager();
		this.scene.initializeDefaultScene(parentComponent);
	}

	//NOTE: Extends some abstraction over scene object?
	public spawn() {}

	public resize() {
		const context = Context.getInstance();
		const container = context.parentComponent;
		context.camera.resize(container);
		context.renderer.setSize(container.clientWidth, container.clientHeight);
	}
}
