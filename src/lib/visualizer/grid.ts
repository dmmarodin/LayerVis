import * as THREE from 'three';

export class Grid {
	public static create(size: number, divisions: number): THREE.GridHelper {
		const gridHelper = new THREE.GridHelper(size, divisions, 0x2d303d, 0x2d303d);
		gridHelper.rotation.x = Math.PI / 2;
		return gridHelper;
	}
}
