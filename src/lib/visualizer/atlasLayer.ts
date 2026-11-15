import * as THREE from 'three';
import type { FieldGrid, RiverPath } from '../bridge';
import {
	applyFieldView,
	createFieldMaterial,
	createFieldTextures,
	finiteRange,
	sampleGrid,
	type ViewMode
} from './fieldMaterial';

/**
 * Whole-world atlas grids (and scrubber frames), drawn as one plane over the
 * world, plus the river overlay. Grids come whole from the backend (one
 * sample per atlas cell), so nothing is resampled while the camera moves.
 */
export class AtlasLayer {
	private readonly mesh: THREE.Mesh<THREE.PlaneGeometry, THREE.ShaderMaterial>;
	private readonly rivers: THREE.LineSegments<THREE.BufferGeometry, THREE.LineBasicMaterial>;
	private grid: FieldGrid | null = null;
	private texture: THREE.DataTexture | null = null;

	public constructor(scene: THREE.Scene) {
		this.mesh = new THREE.Mesh(new THREE.PlaneGeometry(1, 1), createFieldMaterial());
		// Above the world overlay, below chunk tiles' grid lines.
		this.mesh.position.z = -0.4;
		this.mesh.visible = false;
		scene.add(this.mesh);

		this.rivers = new THREE.LineSegments(
			new THREE.BufferGeometry(),
			new THREE.LineBasicMaterial({ color: 0x5ab4ff, transparent: true, opacity: 0.9 })
		);
		this.rivers.position.z = 0.5;
		this.rivers.visible = false;
		scene.add(this.rivers);
	}

	/** Shows a grid (an atlas field or a scrubber frame); null hides the layer. */
	public show(grid: FieldGrid | null, view: ViewMode): void {
		this.texture?.dispose();
		this.texture = null;
		this.grid = grid;

		if (grid) {
			[this.texture] = createFieldTextures(grid);
			const spanX = grid.width * grid.step;
			const spanY = grid.height * grid.step;
			this.mesh.scale.set(spanX, spanY, 1);
			this.mesh.position.set(grid.originX + spanX / 2, grid.originY + spanY / 2, -0.4);
		}
		this.applyView(view);
	}

	public applyView(view: ViewMode): void {
		this.mesh.visible = this.texture !== null;
		applyFieldView(this.mesh.material, this.texture, view);
	}

	public get visible(): boolean {
		return this.mesh.visible;
	}

	public range(): [number, number] | null {
		return this.grid ? finiteRange(this.grid.fields) : null;
	}

	/** Value of the shown grid at a world column (null when hidden or outside). */
	public valueAt(x: number, y: number): number | null {
		return this.grid && this.mesh.visible ? sampleGrid(this.grid, 0, x, y) : null;
	}

	/** River lines (null hides them). Width is not supported by WebGL lines,
	 *  so small rivers are drawn fainter instead. */
	public setRivers(paths: RiverPath[] | null): void {
		const geometry = this.rivers.geometry;
		if (!paths) {
			this.rivers.visible = false;
			return;
		}

		const positions: number[] = [];
		const colors: number[] = [];
		const maxDischarge = Math.max(1, ...paths.flatMap((path) => path.discharge));

		for (const path of paths) {
			for (let i = 0; i + 1 < path.points.length; i++) {
				const [x0, y0] = path.points[i];
				const [x1, y1] = path.points[i + 1];
				positions.push(x0, y0, 0, x1, y1, 0);
				// Brighter for bigger rivers (log scale).
				const t = Math.log(1 + path.discharge[i]) / Math.log(1 + maxDischarge);
				const shade = 0.35 + 0.65 * t;
				colors.push(0.3 * shade, 0.65 * shade, shade, 0.3 * shade, 0.65 * shade, shade);
			}
		}

		geometry.setAttribute('position', new THREE.Float32BufferAttribute(positions, 3));
		geometry.setAttribute('color', new THREE.Float32BufferAttribute(colors, 3));
		geometry.computeBoundingSphere();
		this.rivers.material.vertexColors = true;
		this.rivers.material.color.set(0xffffff);
		this.rivers.material.needsUpdate = true;
		this.rivers.visible = positions.length > 0;
	}
}
