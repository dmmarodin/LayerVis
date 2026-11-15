//! Tectonics world model: a tiny analytic plate layout.
//!
//! Stored data is only the plate list (center, velocity, kind). Every query
//! is a warped Voronoi lookup: nearest and second-nearest plate give the
//! plate id, distance to the shared boundary, and boundary convergence.
//! Immutable after generation; no world-sized allocations.

use crate::world::{fbm, SplitMix};

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Plate {
    pub center: (f64, f64),
    /// Cells per (abstract) time unit; magnitude in [0, 1].
    pub velocity: (f64, f64),
    pub continental: bool,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PlateSample {
    pub plate: usize,
    pub neighbor: usize,
    /// Distance in cells from the (warped) sample point to the boundary
    /// between `plate` and `neighbor`. Zero on the boundary.
    pub boundary_distance: f64,
    /// Relative motion along the boundary normal, in [-2, 2].
    /// Positive: plates converge (collision). Negative: diverge (rift).
    pub convergence: f64,
}

pub struct Tectonics {
    seed: u64,
    plates: Vec<Plate>,
    warp_amplitude: f64,
}

/// Layout parameters (the `tectonics` node's params).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TectonicsConfig {
    pub plate_count: usize,
    /// 0 = every plate is continental by an independent coin flip (scattered
    /// land); N >= 1 = land is clustered into N continents.
    pub continents: usize,
    /// Share of plates that are continental.
    pub continental_fraction: f64,
    /// Boundary irregularity, in units of the plate cell size.
    pub warp: f64,
}

impl Default for TectonicsConfig {
    fn default() -> Self {
        Self {
            plate_count: 30,
            continents: 0,
            continental_fraction: 0.4,
            warp: 0.35,
        }
    }
}

impl Tectonics {
    /// Plates are placed on a jittered grid over the world's XY extent,
    /// which keeps sizes roughly even without iterative relaxation.
    pub fn generate(
        seed: u64,
        world_min: (i64, i64),
        world_size: (u32, u32),
        config: &TectonicsConfig,
    ) -> Self {
        let plate_count = config.plate_count;
        let mut rng = SplitMix::new(seed ^ 0x7EC7_0000);
        let aspect = f64::from(world_size.0) / f64::from(world_size.1);
        let rows = ((plate_count as f64 / aspect).sqrt().round() as usize).max(1);
        let columns = plate_count.div_ceil(rows).max(1);
        let cell_w = f64::from(world_size.0) / columns as f64;
        let cell_h = f64::from(world_size.1) / rows as f64;

        let mut plates = Vec::with_capacity(rows * columns);

        for row in 0..rows {
            for column in 0..columns {
                let jitter_x = 0.15 + 0.7 * rng.next_f64();
                let jitter_y = 0.15 + 0.7 * rng.next_f64();
                let angle = rng.next_f64() * std::f64::consts::TAU;
                let speed = 0.2 + 0.8 * rng.next_f64();

                plates.push(Plate {
                    center: (
                        world_min.0 as f64 + (column as f64 + jitter_x) * cell_w,
                        world_min.1 as f64 + (row as f64 + jitter_y) * cell_h,
                    ),
                    velocity: (angle.cos() * speed, angle.sin() * speed),
                    continental: rng.next_f64() < config.continental_fraction,
                });
            }
        }

        if config.continents > 0 {
            cluster_continents(&mut plates, &mut rng, world_min, world_size, config);
        }

        Self {
            seed,
            plates,
            warp_amplitude: config.warp * cell_w.min(cell_h),
        }
    }

    pub fn plates(&self) -> &[Plate] {
        &self.plates
    }

    /// The same layout after the plates moved for `time` (abstract) time
    /// units: every center moves by velocity × time (negative = the past).
    /// Plate kinds, warp and ids stay, so boundaries shift and collide.
    pub fn moved(&self, time: f64) -> Self {
        Self {
            seed: self.seed,
            plates: self
                .plates
                .iter()
                .map(|plate| Plate {
                    center: (
                        plate.center.0 + plate.velocity.0 * time,
                        plate.center.1 + plate.velocity.1 * time,
                    ),
                    ..*plate
                })
                .collect(),
            warp_amplitude: self.warp_amplitude,
        }
    }

    pub fn sample(&self, x: i64, y: i64) -> PlateSample {
        let (x, y) = (x as f64, y as f64);

        // Domain warp gives boundaries an irregular, natural shape.
        let wavelength = self.warp_amplitude * 3.0;
        let wx = x
            + (fbm(self.seed ^ 0xA1, x, y, wavelength, 3) as f64 - 0.5) * 2.0 * self.warp_amplitude;
        let wy = y
            + (fbm(self.seed ^ 0xB2, x, y, wavelength, 3) as f64 - 0.5) * 2.0 * self.warp_amplitude;

        let (mut first, mut second) = ((usize::MAX, f64::MAX), (usize::MAX, f64::MAX));

        for (index, plate) in self.plates.iter().enumerate() {
            let dx = wx - plate.center.0;
            let dy = wy - plate.center.1;
            let distance = dx * dx + dy * dy;

            if distance < first.1 {
                second = first;
                first = (index, distance);
            } else if distance < second.1 {
                second = (index, distance);
            }
        }

        if second.0 == usize::MAX {
            return PlateSample {
                plate: first.0,
                neighbor: first.0,
                boundary_distance: f64::MAX,
                convergence: 0.0,
            };
        }

        let a = &self.plates[first.0];
        let b = &self.plates[second.0];
        let (nx, ny) = (b.center.0 - a.center.0, b.center.1 - a.center.1);
        let separation = (nx * nx + ny * ny).sqrt().max(1e-9);
        let (nx, ny) = (nx / separation, ny / separation);

        // Distance to the perpendicular bisector of the two centers.
        let boundary_distance = ((second.1 - first.1) / (2.0 * separation)).max(0.0);

        // Positive when `a` moves toward `b` faster than `b` moves away.
        let convergence = (a.velocity.0 - b.velocity.0) * nx + (a.velocity.1 - b.velocity.1) * ny;

        PlateSample {
            plate: first.0,
            neighbor: second.0,
            boundary_distance,
            convergence,
        }
    }
}

/// Makes land continental plates clustered around `continents` cores: cores
/// are spread apart (each the plate farthest from the earlier ones, among
/// plates away from the world edge), and the plates nearest a core become
/// continental until `continental_fraction` of all plates is land.
fn cluster_continents(
    plates: &mut [Plate],
    rng: &mut SplitMix,
    world_min: (i64, i64),
    world_size: (u32, u32),
    config: &TectonicsConfig,
) {
    let (w, h) = (f64::from(world_size.0), f64::from(world_size.1));
    let inside = |plate: &Plate| {
        let fx = (plate.center.0 - world_min.0 as f64) / w;
        let fy = (plate.center.1 - world_min.1 as f64) / h;
        (0.2..=0.8).contains(&fx) && (0.2..=0.8).contains(&fy)
    };

    let mut pool: Vec<usize> = (0..plates.len()).filter(|&i| inside(&plates[i])).collect();
    if pool.len() < config.continents {
        pool = (0..plates.len()).collect();
    }

    let distance = |a: usize, b: usize| {
        let (pa, pb) = (&plates[a], &plates[b]);
        ((pa.center.0 - pb.center.0).powi(2) + (pa.center.1 - pb.center.1).powi(2)).sqrt()
    };

    let mut cores = vec![pool[(rng.next_f64() * pool.len() as f64) as usize % pool.len()]];
    while cores.len() < config.continents.min(pool.len()) {
        let next = pool
            .iter()
            .copied()
            .filter(|candidate| !cores.contains(candidate))
            .max_by(|&a, &b| {
                let nearest = |p: usize| {
                    cores
                        .iter()
                        .map(|&core| distance(p, core))
                        .fold(f64::MAX, f64::min)
                };
                nearest(a).total_cmp(&nearest(b))
            })
            .expect("pool is larger than the cores chosen so far");
        cores.push(next);
    }

    let land = ((config.continental_fraction * plates.len() as f64).round() as usize)
        .clamp(cores.len(), plates.len());
    let mut by_distance: Vec<(f64, usize)> = (0..plates.len())
        .map(|p| {
            let nearest = cores
                .iter()
                .map(|&c| distance(p, c))
                .fold(f64::MAX, f64::min);
            (nearest, p)
        })
        .collect();
    by_distance.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)));

    for (rank, &(_, plate)) in by_distance.iter().enumerate() {
        plates[plate].continental = rank < land;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model() -> Tectonics {
        Tectonics::generate(9, (-4096, -4096), (8192, 8192), &TectonicsConfig::default())
    }

    #[test]
    fn generation_is_deterministic_and_inside_world() {
        let a = model();
        let b = model();

        assert_eq!(a.plates(), b.plates());
        assert!(a.plates().len() >= 30);

        for plate in a.plates() {
            assert!((-4096.0..4096.0).contains(&plate.center.0));
            assert!((-4096.0..4096.0).contains(&plate.center.1));
        }
    }

    #[test]
    fn both_sides_of_a_boundary_agree_on_convergence() {
        let model = model();

        // Find a point pair straddling a boundary along X.
        for x in -4000..4000 {
            let here = model.sample(x, 123);
            let next = model.sample(x + 1, 123);

            if here.plate != next.plate
                && here.neighbor == next.plate
                && next.neighbor == here.plate
            {
                // (va - vb)·n_ab == (vb - va)·n_ba: a collision is a
                // collision from either side.
                assert!((here.convergence - next.convergence).abs() < 1e-9);
                assert!(here.boundary_distance < 50.0 && next.boundary_distance < 50.0);
                return;
            }
        }

        panic!("No boundary crossing found along the test line");
    }
}
