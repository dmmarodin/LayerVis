import * as THREE from 'three';
import type { FieldGrid } from '../bridge';

export const RAMPS = ['grayscale', 'terrain', 'thermal', 'diverging', 'categorical'] as const;
export type Ramp = (typeof RAMPS)[number];

export type FieldSource = 'column' | 'world' | 'atlas';

/** Declarative view mode. Coloring runs on the GPU; changing it never hits the backend. */
export interface ViewMode {
	source: FieldSource;
	/** Index into WorldInfo.columnFields, worldFields or atlas.fields. */
	field: number;
	ramp: Ramp;
	min: number;
	max: number;
}

/**
 * ViewMode as rendered: `hidden` is set when the selected column field is
 * not computed yet at the selected pipeline step (mid-pipeline view).
 */
export interface RenderView extends ViewMode {
	hidden: boolean;
}

const vertexShader = /* glsl */ `
	varying vec2 vUv;
	void main() {
		vUv = uv;
		gl_Position = projectionMatrix * modelViewMatrix * vec4(position, 1.0);
	}
`;

const fragmentShader = /* glsl */ `
	uniform sampler2D field;
	uniform float minValue;
	uniform float maxValue;
	uniform int ramp;
	uniform bool hidden;
	varying vec2 vUv;

	vec3 terrain(float t) {
		if (t < 0.30) return mix(vec3(0.05, 0.12, 0.35), vec3(0.15, 0.40, 0.70), t / 0.30);
		if (t < 0.34) return mix(vec3(0.80, 0.75, 0.50), vec3(0.35, 0.60, 0.25), (t - 0.30) / 0.04);
		if (t < 0.65) return mix(vec3(0.35, 0.60, 0.25), vec3(0.20, 0.40, 0.15), (t - 0.34) / 0.31);
		if (t < 0.85) return mix(vec3(0.45, 0.38, 0.30), vec3(0.55, 0.52, 0.50), (t - 0.65) / 0.20);
		return mix(vec3(0.85, 0.85, 0.90), vec3(1.0), (t - 0.85) / 0.15);
	}

	vec3 thermal(float t) {
		if (t < 0.33) return mix(vec3(0.10, 0.15, 0.60), vec3(0.20, 0.70, 0.90), t / 0.33);
		if (t < 0.66) return mix(vec3(0.20, 0.70, 0.90), vec3(0.95, 0.90, 0.30), (t - 0.33) / 0.33);
		return mix(vec3(0.95, 0.90, 0.30), vec3(0.85, 0.15, 0.10), (t - 0.66) / 0.34);
	}

	vec3 diverging(float t) {
		if (t < 0.5) return mix(vec3(0.15, 0.30, 0.75), vec3(0.95), t / 0.5);
		return mix(vec3(0.95), vec3(0.80, 0.15, 0.15), (t - 0.5) / 0.5);
	}

	vec3 hsv(float h, float s, float v) {
		vec3 k = clamp(abs(fract(h + vec3(0.0, 2.0 / 3.0, 1.0 / 3.0)) * 6.0 - 3.0) - 1.0, 0.0, 1.0);
		return v * mix(vec3(1.0), k, s);
	}

	// Stable distinct colors per integer id (min/max ignored).
	vec3 categorical(float value) {
		float id = floor(value + 0.5);
		float hue = fract(id * 0.61803398875 + 0.13);
		float shade = 0.70 + 0.25 * fract(id * 0.414);
		return hsv(hue, 0.55, shade);
	}

	void main() {
		if (hidden) discard; // no field to show (not computed yet / none)
		float value = texture2D(field, vUv).r;
		if (isnan(value)) discard; // outside the world

		float t = clamp((value - minValue) / max(maxValue - minValue, 1e-6), 0.0, 1.0);
		vec3 color =
			ramp == 1 ? terrain(t) :
			ramp == 2 ? thermal(t) :
			ramp == 3 ? diverging(t) :
			ramp == 4 ? categorical(value) :
			vec3(t);
		gl_FragColor = vec4(color, 1.0);
	}
`;

type Stop = [position: number, r: number, g: number, b: number];

/**
 * Color stops mirroring the shader ramps, for the sidebar legend only.
 * Keep in sync with the GLSL above (repeated positions are hard edges).
 */
const RAMP_STOPS: Record<Exclude<Ramp, 'categorical'>, Stop[]> = {
	grayscale: [
		[0, 0, 0, 0],
		[1, 1, 1, 1]
	],
	terrain: [
		[0, 0.05, 0.12, 0.35],
		[0.3, 0.15, 0.4, 0.7],
		[0.3, 0.8, 0.75, 0.5],
		[0.34, 0.35, 0.6, 0.25],
		[0.65, 0.2, 0.4, 0.15],
		[0.65, 0.45, 0.38, 0.3],
		[0.85, 0.55, 0.52, 0.5],
		[0.85, 0.85, 0.85, 0.9],
		[1, 1, 1, 1]
	],
	thermal: [
		[0, 0.1, 0.15, 0.6],
		[0.33, 0.2, 0.7, 0.9],
		[0.66, 0.95, 0.9, 0.3],
		[1, 0.85, 0.15, 0.1]
	],
	diverging: [
		[0, 0.15, 0.3, 0.75],
		[0.5, 0.95, 0.95, 0.95],
		[1, 0.8, 0.15, 0.15]
	]
};

/** CSS gradient of a ramp (left = min, right = max); null for categorical. */
export function rampGradient(ramp: Ramp): string | null {
	if (ramp === 'categorical') return null;

	const channel = (value: number) => Math.round(value * 255);
	const stops = RAMP_STOPS[ramp].map(
		([at, r, g, b]) => `rgb(${channel(r)} ${channel(g)} ${channel(b)}) ${at * 100}%`
	);
	return `linear-gradient(to right, ${stops.join(', ')})`;
}

/** Min and max of the finite values in `planes`, or null when there are none. */
export function finiteRange(planes: Iterable<ArrayLike<number>>): [number, number] | null {
	let lo = Infinity;
	let hi = -Infinity;

	for (const plane of planes) {
		for (let i = 0; i < plane.length; i++) {
			const value = plane[i];
			if (value < lo) lo = value;
			if (value > hi) hi = value;
		}
	}

	// NaN (outside the world) never compares, so it is skipped.
	return lo <= hi ? [lo, hi] : null;
}

export function createFieldMaterial(): THREE.ShaderMaterial {
	return new THREE.ShaderMaterial({
		vertexShader,
		fragmentShader,
		uniforms: {
			field: { value: null },
			minValue: { value: 0 },
			maxValue: { value: 1 },
			ramp: { value: 0 },
			hidden: { value: true }
		}
	});
}

export function createFieldTextures(grid: FieldGrid): THREE.DataTexture[] {
	return grid.fields.map((plane) => {
		const texture = new THREE.DataTexture(
			plane,
			grid.width,
			grid.height,
			THREE.RedFormat,
			THREE.FloatType
		);
		texture.magFilter = THREE.NearestFilter;
		texture.minFilter = THREE.NearestFilter;
		texture.needsUpdate = true;
		return texture;
	});
}

export function applyFieldView(
	material: THREE.ShaderMaterial,
	texture: THREE.Texture | null,
	view: Pick<ViewMode, 'ramp' | 'min' | 'max'>
): void {
	material.uniforms.field.value = texture;
	material.uniforms.hidden.value = texture === null;
	material.uniforms.minValue.value = view.min;
	material.uniforms.maxValue.value = view.max;
	material.uniforms.ramp.value = RAMPS.indexOf(view.ramp);
}

/** Value of a field grid at a world column, or null if not covered / outside world. */
export function sampleGrid(grid: FieldGrid, field: number, x: number, y: number): number | null {
	const i = Math.floor((x - grid.originX) / grid.step);
	const j = Math.floor((y - grid.originY) / grid.step);
	if (i < 0 || j < 0 || i >= grid.width || j >= grid.height) return null;

	const value = grid.fields[field]?.[i + j * grid.width];
	return value === undefined || Number.isNaN(value) ? null : value;
}
