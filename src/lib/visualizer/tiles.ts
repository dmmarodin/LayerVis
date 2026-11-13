import * as THREE from 'three';
import type { FieldGrid } from '../bridge';
import {
	applyFieldView,
	createFieldMaterial,
	createFieldTextures,
	type RenderView
} from './fieldMaterial';

export { RAMPS, type Ramp, type ViewMode, type RenderView } from './fieldMaterial';

/** One generated chunk drawn as a quad; one float texture per column field. */
export class ChunkTile {
	public readonly mesh: THREE.Mesh<THREE.PlaneGeometry, THREE.ShaderMaterial>;
	public readonly columns: FieldGrid;
	private readonly textures: THREE.DataTexture[];

	public constructor(columns: FieldGrid, view: RenderView) {
		this.columns = columns;
		this.textures = createFieldTextures(columns);
		this.mesh = new THREE.Mesh(
			new THREE.PlaneGeometry(columns.width, columns.height),
			createFieldMaterial()
		);
		// Texture row 0 = global y origin = quad bottom edge.
		this.mesh.position.set(
			columns.originX + columns.width / 2,
			columns.originY + columns.height / 2,
			0
		);

		this.applyView(view);
	}

	/** Only column views are drawn by tiles; world views use the overlay. */
	public applyView(view: RenderView): void {
		const shown = view.source === 'column' && !view.hidden;
		const texture = shown ? (this.textures[view.field] ?? null) : null;
		applyFieldView(this.mesh.material, texture, view);
	}

	public dispose(): void {
		this.textures.forEach((texture) => texture.dispose());
		this.mesh.geometry.dispose();
		this.mesh.material.dispose();
	}
}
