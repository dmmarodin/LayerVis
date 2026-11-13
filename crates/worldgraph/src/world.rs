//! World stage: small analytic world models built once per seed.
//!
//! Must stay analytic (no world-sized allocations): worlds are DF-sized.
//! Every world field is a pure function of (x, y), so the same function feeds
//! chunk steps and zoomed-out world views.

use layergen::PipelineConfig;
use serde::Serialize;

use crate::tectonics::Tectonics;

pub const PLATE_COUNT: usize = 30;

/// LayerGen shared data `D`. Holds every world model for one seed.
pub struct WorldStore {
    world_min: (i64, i64, i64),
    world_size: (u32, u32, u32),
    seed: u64,
    pub tectonics: Tectonics,
}

/// All world fields at one (x, y). Order of `values()` matches `world_fields()`.
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorldSample {
    pub plate: f32,
    pub boundary_distance: f32,
    pub convergence: f32,
    pub elevation: f32,
}

impl WorldSample {
    pub const FIELD_COUNT: usize = 4;

    pub fn values(&self) -> [f32; Self::FIELD_COUNT] {
        [
            self.plate,
            self.boundary_distance,
            self.convergence,
            self.elevation,
        ]
    }
}

impl WorldStore {
    pub fn new(config: &PipelineConfig) -> Self {
        let world = config.world();
        let origin = world.origin();
        let size = world.dimensions();

        Self {
            world_min: origin,
            world_size: size,
            seed: config.seed(),
            tectonics: Tectonics::generate(
                config.seed(),
                (origin.0, origin.1),
                (size.0, size.1),
                PLATE_COUNT,
            ),
        }
    }

    pub fn contains_column(&self, x: i64, y: i64) -> bool {
        x >= self.world_min.0
            && y >= self.world_min.1
            && x < self.world_min.0 + i64::from(self.world_size.0)
            && y < self.world_min.1 + i64::from(self.world_size.1)
    }

    pub fn sample(&self, x: i64, y: i64) -> WorldSample {
        let plates = self.tectonics.plates();
        let tectonic = self.tectonics.sample(x, y);
        let depth = f64::from(self.world_size.2);

        let base = |index: usize| if plates[index].continental { 0.55 } else { 0.22 };

        // Blend plate base heights near the boundary to avoid cliffs.
        let own = base(tectonic.plate);
        let blend = smoothstep(0.0, 180.0, tectonic.boundary_distance);
        let mut height = (own + base(tectonic.neighbor)) / 2.0 * (1.0 - blend) + own * blend;

        // Boundary effects fall off with distance from the boundary.
        let falloff = (-tectonic.boundary_distance / 160.0).exp();
        let conv = tectonic.convergence;
        let both_continental =
            plates[tectonic.plate].continental && plates[tectonic.neighbor].continental;
        let any_continental =
            plates[tectonic.plate].continental || plates[tectonic.neighbor].continental;

        if conv > 0.0 {
            let strength = if both_continental {
                0.32
            } else if any_continental {
                0.22
            } else {
                0.10
            };
            height += conv * strength * falloff;
        } else if plates[tectonic.plate].continental {
            height += conv * 0.10 * falloff; // continental rift
        } else {
            height -= conv * 0.05 * falloff; // mid-ocean ridge
        }

        let detail = f64::from(fbm(self.seed, x as f64, y as f64, 96.0, 4)) - 0.5;
        height += detail * 0.12;

        WorldSample {
            plate: tectonic.plate as f32,
            boundary_distance: tectonic.boundary_distance.min(1e6) as f32,
            convergence: conv as f32,
            elevation: (height * depth).clamp(0.0, depth - 1.0) as f32,
        }
    }

    /// Terrain elevation in cells, in [0, world depth).
    pub fn elevation_at(&self, x: i64, y: i64) -> f32 {
        self.sample(x, y).elevation
    }

    /// World depth in cells (elevation range).
    pub fn depth(&self) -> f32 {
        self.world_size.2 as f32
    }

    /// Elevation below which a column is ocean. Matches the terrain ramp's
    /// water band and the oceanic plate base height.
    pub fn sea_level(&self) -> f32 {
        self.depth() * 0.3
    }

    /// Low-frequency noise in [0, 1] for moisture variation.
    pub fn moisture_noise(&self, x: i64, y: i64) -> f32 {
        fbm(self.seed ^ 0x3017, x as f64, y as f64, 384.0, 3)
    }

    /// Seeded fractal noise in [0, 1] for scripts.
    pub fn script_noise(&self, x: i64, y: i64, wavelength: f64) -> f32 {
        fbm(self.seed ^ 0x5C21, x as f64, y as f64, wavelength.max(1.0), 4)
    }

    /// Signed latitude in [-1, 1] across the world's Y extent.
    pub fn latitude_at(&self, y: i64) -> f32 {
        let span = self.world_size.1 as f64;
        let t = (y - self.world_min.1) as f64 / span;

        (t * 2.0 - 1.0) as f32
    }
}

fn smoothstep(edge0: f64, edge1: f64, x: f64) -> f64 {
    let t = ((x - edge0) / (edge1 - edge0)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// Small deterministic RNG for world-model generation.
pub(crate) struct SplitMix(u64);

impl SplitMix {
    pub fn new(seed: u64) -> Self {
        Self(seed)
    }

    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    pub fn next_f64(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }
}

fn hash(seed: u64, x: i64, y: i64) -> u64 {
    let mut h = seed ^ 0x9E37_79B9_7F4A_7C15;
    h ^= (x as u64).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    h = h.rotate_left(31).wrapping_mul(0x94D0_49BB_1331_11EB);
    h ^= (y as u64).wrapping_mul(0xD6E8_FEB8_6659_FD93);
    h ^= h >> 32;
    h = h.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    h ^ (h >> 29)
}

fn lattice(seed: u64, x: i64, y: i64) -> f64 {
    (hash(seed, x, y) >> 11) as f64 / (1u64 << 53) as f64
}

fn smooth(t: f64) -> f64 {
    t * t * (3.0 - 2.0 * t)
}

fn value_noise(seed: u64, x: f64, y: f64) -> f64 {
    let (x0, y0) = (x.floor(), y.floor());
    let (tx, ty) = (smooth(x - x0), smooth(y - y0));
    let (xi, yi) = (x0 as i64, y0 as i64);

    let a = lattice(seed, xi, yi);
    let b = lattice(seed, xi + 1, yi);
    let c = lattice(seed, xi, yi + 1);
    let d = lattice(seed, xi + 1, yi + 1);

    let top = a + (b - a) * tx;
    let bottom = c + (d - c) * tx;

    top + (bottom - top) * ty
}

/// Fractal value noise in [0, 1].
pub(crate) fn fbm(seed: u64, x: f64, y: f64, wavelength: f64, octaves: u32) -> f32 {
    let mut total = 0.0;
    let mut amplitude = 1.0;
    let mut norm = 0.0;
    let mut frequency = 1.0 / wavelength;

    for octave in 0..octaves {
        let octave_seed = seed.wrapping_add(u64::from(octave) * 0x1000_0001);
        total += value_noise(octave_seed, x * frequency, y * frequency) * amplitude;
        norm += amplitude;
        amplitude *= 0.5;
        frequency *= 2.0;
    }

    (total / norm) as f32
}
