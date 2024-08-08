import * as THREE from 'three';

export class InfiniteGrid {
	public static create(size: number, divisions: number): THREE.GridHelper {
		const gridHelper = new THREE.GridHelper(size, divisions, 0x3c4055, 0x2d303d);
		gridHelper.rotation.x = Math.PI / 2;
		return gridHelper;
	}
}
