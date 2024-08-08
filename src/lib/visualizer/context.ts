import * as THREE from 'three';
import type { CameraHandler } from './camera';

/**
 * Global context class for managing shared state and resources across the ThreeJS application.
 */
export class Context {
	private _scene: THREE.Scene | null = null;
	private _camera: CameraHandler | null = null;
	private _renderer: THREE.WebGLRenderer | null = null;
	private _parentComponent: HTMLElement | null = null;

	private static _instance: Context;

	/**
	 * Retrieves the singleton instance of the Context class.
	 * If the instance does not exist, it creates a new one.
	 *
	 * @returns The singleton instance of the Context class.
	 */
	public static getInstance(): Context {
		if (!this._instance) {
			this._instance = new Context();
		}
		return this._instance;
	}

	public get scene(): THREE.Scene {
		if (!this._scene) throw new Error('Scene not initialized');
		return this._scene;
	}

	public setScene(value: THREE.Scene) {
		if (this._scene) throw new Error('Scene already initialized');
		this._scene = value;
	}

	public get camera(): CameraHandler {
		if (!this._camera) throw new Error('Camera not initialized');
		return this._camera;
	}

	public setCamera(value: CameraHandler) {
		if (this._camera) throw new Error('Camera already initialized');
		this._camera = value;
	}

	public get renderer(): THREE.WebGLRenderer {
		if (!this._renderer) throw new Error('Renderer not initialized');
		return this._renderer;
	}

	public setRenderer(value: THREE.WebGLRenderer) {
		if (this._renderer) throw new Error('Renderer already initialized');
		this._renderer = value;
	}

	public get parentComponent(): HTMLElement {
		if (!this._parentComponent) throw new Error('Parent component not set');
		return this._parentComponent;
	}

	public setParentComponent(value: HTMLElement) {
		if (this._parentComponent) throw new Error('Parent component already set');
		this._parentComponent = value;
	}
}
