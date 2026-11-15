import * as THREE from 'three';

const vertexShader = /* glsl */ `
	varying vec2 vWorld;
	void main() {
		vec4 world = modelMatrix * vec4(position, 1.0);
		vWorld = world.xy;
		gl_Position = projectionMatrix * viewMatrix * world;
	}
`;

/**
 * Lines at multiples of `spacing` in world coordinates (so they stay put
 * while the camera moves). Spacing grows ×8 per level as you zoom out, so
 * lines never get denser than ~MIN_PIXELS apart; the finer level fades out
 * before it disappears, and every 8th line is brighter.
 */
const fragmentShader = /* glsl */ `
	uniform float spacing;
	varying vec2 vWorld;

	const float MIN_PIXELS = 10.0;

	float lines(vec2 p, float s, vec2 cellsPerPixel) {
		vec2 g = abs(fract(p / s - 0.5) - 0.5) * s / max(cellsPerPixel, vec2(1e-6));
		return 1.0 - min(min(g.x, g.y), 1.0);
	}

	void main() {
		vec2 cpp = fwidth(vWorld);
		float perPixel = max(cpp.x, cpp.y);
		float level = max(0.0, ceil(log(MIN_PIXELS * perPixel / spacing) / log(8.0)));
		float fine = spacing * pow(8.0, level);
		float coarse = fine * 8.0;
		// Fade the fine level in as its lines move apart on screen.
		float fade = smoothstep(MIN_PIXELS, MIN_PIXELS * 4.0, fine / perPixel);

		float a = lines(vWorld, fine, cpp) * 0.35 * fade;
		float b = lines(vWorld, coarse, cpp) * 0.6;
		float alpha = max(a, b);
		if (alpha <= 0.01) discard;
		vec3 color = mix(vec3(0.24, 0.25, 0.33), vec3(0.32, 0.34, 0.45), b >= a ? 1.0 : 0.0);
		gl_FragColor = vec4(color, alpha);
	}
`;

/** Shader grid covering the view; `cover` keeps it under the camera. */
export class InfiniteGrid {
	public readonly mesh: THREE.Mesh<THREE.PlaneGeometry, THREE.ShaderMaterial>;

	public constructor(spacing: number) {
		this.mesh = new THREE.Mesh(
			new THREE.PlaneGeometry(1, 1),
			new THREE.ShaderMaterial({
				vertexShader,
				fragmentShader,
				transparent: true,
				depthWrite: false,
				uniforms: { spacing: { value: spacing } }
			})
		);
		// Just above the tiles, below the slice and river layers.
		this.mesh.position.z = 0.1;
		this.mesh.renderOrder = 1;
		this.mesh.frustumCulled = false;
	}

	/** Covers the visible area (world units), with lines on multiples of `spacing`. */
	public cover(minX: number, minY: number, maxX: number, maxY: number, spacing: number): void {
		const width = maxX - minX;
		const height = maxY - minY;
		this.mesh.scale.set(width * 1.2, height * 1.2, 1);
		this.mesh.position.set((minX + maxX) / 2, (minY + maxY) / 2, 0.1);
		this.mesh.material.uniforms.spacing.value = spacing;
	}
}
