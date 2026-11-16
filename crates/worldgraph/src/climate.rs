//! World-scope climate nodes: temperature and humidity.
//!
//! Both are pure functions of the world position and the terrain elevation,
//! so they run at world scope: they have a zoomed-out view like the terrain,
//! and chunk nodes read them through implicit sampling.

use std::borrow::Cow;

use crate::{
    atlas::{AtlasIo, AtlasRun, AtlasStage, BoxedAtlasStage},
    pipeline::{
        BoxedWorldStep, Build, FieldSpec, InputSpec, NodeKind, ParamSpec, WorldBuildContext,
        WorldStep,
    },
    world::fbm,
    FieldScope, ValueKind, WORLD_SLOTS,
};

const fn world_out(
    name: &'static str,
    unit: &'static str,
    min: f32,
    max: f32,
    ramp: &'static str,
) -> FieldSpec {
    FieldSpec {
        name: Cow::Borrowed(name),
        scope: FieldScope::World,
        kind: ValueKind::Scalar,
        unit: Cow::Borrowed(unit),
        min,
        max,
        ramp: Cow::Borrowed(ramp),
        display_from: None,
    }
}

const fn world_in(name: &'static str) -> InputSpec {
    InputSpec {
        name: Cow::Borrowed(name),
        scope: FieldScope::World,
        kind: ValueKind::Scalar,
        optional: false,
    }
}

/// Signed latitude in [-1, 1] across the world's Y extent.
fn latitude(context: &WorldBuildContext, y: i64) -> f64 {
    let t = (y - context.origin.1) as f64 / f64::from(context.size.1);
    t * 2.0 - 1.0
}

// ---------------------------------------------------------------------------

/// Sea level as a fraction of the world depth (matches the terrain ramp's
/// water band and the oceanic plate height).
const SEA_LEVEL: f64 = 0.3;

pub static TEMPERATURE: NodeKind = NodeKind {
    id: "temperature",
    label: "Temperature",
    description: "Warm equator, cold poles, colder with altitude.",
    inputs: &[world_in("elevation")],
    outputs: &[world_out("temperature", "°C", -30.0, 40.0, "thermal")],
    params: &[
        ParamSpec {
            name: "equator",
            default: 32.0,
            min: -20.0,
            max: 60.0,
        },
        ParamSpec {
            name: "pole_drop",
            default: 50.0,
            min: 0.0,
            max: 120.0,
        },
        ParamSpec {
            name: "lapse",
            default: 0.25,
            min: 0.0,
            max: 2.0,
        },
    ],
    build: Build::World(|bindings, params, context| {
        Ok(Box::new(TemperatureStep {
            elevation: bindings.input("elevation")?,
            out: bindings.output("temperature")?,
            equator: params.get("equator"),
            pole_drop: params.get("pole_drop"),
            lapse: params.get("lapse"),
            context: *context,
        }) as BoxedWorldStep)
    }),
};

struct TemperatureStep {
    elevation: usize,
    out: usize,
    equator: f64,
    pole_drop: f64,
    lapse: f64,
    context: WorldBuildContext,
}

impl WorldStep for TemperatureStep {
    fn run(&self, _x: i64, y: i64, slots: &mut [f64; WORLD_SLOTS]) {
        let latitude = latitude(&self.context, y).abs();

        slots[self.out] =
            self.equator - self.pole_drop * latitude - self.lapse * slots[self.elevation];
    }
}

// ---------------------------------------------------------------------------

pub static HUMIDITY: NodeKind = NodeKind {
    id: "humidity",
    label: "Humidity",
    description: "Wet oceans and lowlands, drier with altitude and heat.",
    inputs: &[world_in("elevation"), world_in("temperature")],
    outputs: &[world_out("humidity", "0–1", 0.0, 1.0, "thermal")],
    params: &[],
    build: Build::World(|bindings, _, context| {
        Ok(Box::new(HumidityStep {
            elevation: bindings.input("elevation")?,
            temperature: bindings.input("temperature")?,
            out: bindings.output("humidity")?,
            seed: context.seed ^ 0x3017,
            depth: f64::from(context.depth),
        }) as BoxedWorldStep)
    }),
};

struct HumidityStep {
    elevation: usize,
    temperature: usize,
    out: usize,
    seed: u64,
    depth: f64,
}

impl WorldStep for HumidityStep {
    fn run(&self, x: i64, y: i64, slots: &mut [f64; WORLD_SLOTS]) {
        let sea_level = self.depth * SEA_LEVEL;
        let elevation = slots[self.elevation];

        slots[self.out] = if elevation < sea_level {
            1.0
        } else {
            let altitude = (elevation - sea_level) / (self.depth - sea_level);
            let noise = f64::from(fbm(self.seed, x as f64, y as f64, 384.0, 3));
            let heat = (slots[self.temperature] - 22.0).max(0.0);

            (0.75 - 0.6 * altitude + (noise - 0.5) * 0.4 - 0.015 * heat).clamp(0.0, 1.0)
        };
    }
}

// ---------------------------------------------------------------------------
// Atlas climate (milestone A2): wind → moisture → seasons.
//
// Everything is sequential (Jacobi passes in a fixed order), so the grids
// are bit-deterministic.

const fn atlas_out(
    name: &'static str,
    unit: &'static str,
    min: f32,
    max: f32,
    ramp: &'static str,
) -> FieldSpec {
    FieldSpec {
        name: Cow::Borrowed(name),
        scope: FieldScope::Atlas,
        kind: ValueKind::Scalar,
        unit: Cow::Borrowed(unit),
        min,
        max,
        ramp: Cow::Borrowed(ramp),
        display_from: None,
    }
}

const fn atlas_in(name: &'static str, optional: bool) -> InputSpec {
    InputSpec {
        name: Cow::Borrowed(name),
        scope: FieldScope::Atlas,
        kind: ValueKind::Scalar,
        optional,
    }
}

const fn param(name: &'static str, default: f64, min: f64, max: f64) -> ParamSpec {
    ParamSpec {
        name,
        default,
        min,
        max,
    }
}

/// Signed latitude (-1 … 1) of an atlas row's center.
fn row_latitude(row: u32, rows: u32) -> f32 {
    ((f64::from(row) + 0.5) / f64::from(rows) * 2.0 - 1.0) as f32
}

/// Distance (columns) from every cell to the nearest ocean cell (below
/// `sea`), by a two-pass 8-neighbor chamfer transform. 0 in the ocean; the
/// grid border does not count as ocean. Deterministic.
pub(crate) fn ocean_distance(
    heights: &[f32],
    width: u32,
    height: u32,
    sea: f32,
    cell_size: f32,
) -> Vec<f32> {
    let (w, h) = (width as usize, height as usize);
    let far = 1.0e9f32;
    let mut d: Vec<f32> = heights
        .iter()
        .map(|&e| if e < sea { 0.0 } else { far })
        .collect();
    let (a, b) = (cell_size, cell_size * std::f32::consts::SQRT_2);
    for y in 0..h {
        for x in 0..w {
            let i = x + y * w;
            let mut v = d[i];
            if x > 0 {
                v = v.min(d[i - 1] + a);
            }
            if y > 0 {
                v = v.min(d[i - w] + a);
                if x > 0 {
                    v = v.min(d[i - w - 1] + b);
                }
                if x + 1 < w {
                    v = v.min(d[i - w + 1] + b);
                }
            }
            d[i] = v;
        }
    }
    for y in (0..h).rev() {
        for x in (0..w).rev() {
            let i = x + y * w;
            let mut v = d[i];
            if x + 1 < w {
                v = v.min(d[i + 1] + a);
            }
            if y + 1 < h {
                v = v.min(d[i + w] + a);
                if x + 1 < w {
                    v = v.min(d[i + w + 1] + b);
                }
                if x > 0 {
                    v = v.min(d[i + w - 1] + b);
                }
            }
            d[i] = v;
        }
    }
    d
}

/// `values` blurred by `passes` 3×3 box filters (edges clamped).
fn smoothed(values: &[f32], w: usize, h: usize, passes: u32) -> Vec<f32> {
    let mut out = values.to_vec();
    let mut tmp = vec![0.0f32; out.len()];
    for _ in 0..passes {
        for y in 0..h {
            for x in 0..w {
                let mut sum = 0.0;
                for dy in [-1i64, 0, 1] {
                    for dx in [-1i64, 0, 1] {
                        let sx = (x as i64 + dx).clamp(0, w as i64 - 1) as usize;
                        let sy = (y as i64 + dy).clamp(0, h as i64 - 1) as usize;
                        sum += out[sx + sy * w];
                    }
                }
                tmp[x + y * w] = sum / 9.0;
            }
        }
        std::mem::swap(&mut out, &mut tmp);
    }
    out
}

/// Prevailing surface wind at a latitude (-1 … 1): trade winds (easterly,
/// towards the equator) in the tropics, westerlies (poleward) at mid
/// latitudes, polar easterlies near the poles. `(u, v)` with +x east, +y
/// towards larger latitude values.
pub fn prevailing_wind(latitude: f32) -> (f32, f32) {
    let a = latitude.abs();
    let band = (3.0 * std::f32::consts::PI * a).sin();
    let u = -band;
    let poleward = -0.2 * band;
    (u, poleward * latitude.signum())
}

pub static WIND: NodeKind = NodeKind {
    id: "wind",
    label: "Wind",
    description: "Prevailing surface wind per latitude band: trade winds in the tropics, \
                  westerlies at mid latitudes, polar easterlies near the poles, with some \
                  turbulence. `season` shifts the bands (-1 northern winter, 0 annual mean, \
                  1 northern summer). Feeds moisture.",
    inputs: &[],
    outputs: &[
        atlas_out("wind_u", "", -1.5, 1.5, "diverging"),
        atlas_out("wind_v", "", -1.5, 1.5, "diverging"),
    ],
    params: &[
        param("season", 0.0, -1.0, 1.0),
        param("turbulence", 0.3, 0.0, 2.0),
        // Wavelength of the turbulence, in columns.
        param("wavelength", 2000.0, 100.0, 20000.0),
    ],
    build: Build::Atlas(|bindings, params, _| {
        Ok(Box::new(WindStage {
            out_u: bindings.output("wind_u")?,
            out_v: bindings.output("wind_v")?,
            season: params.get("season") as f32,
            turbulence: params.get("turbulence") as f32,
            wavelength: params.get("wavelength"),
        }) as BoxedAtlasStage)
    }),
};

struct WindStage {
    out_u: usize,
    out_v: usize,
    season: f32,
    turbulence: f32,
    wavelength: f64,
}

/// How far (in latitude units) the bands move between annual mean and a
/// solstice.
const SEASON_SHIFT: f32 = 0.08;

impl AtlasStage for WindStage {
    fn run(&self, io: &mut AtlasIo<'_>, _run: &mut AtlasRun<'_>) -> Result<(), String> {
        let context = *io.context();
        let (width, height) = context.grid;
        let n = width as usize * height as usize;
        let (mut us, mut vs) = (vec![0.0f32; n], vec![0.0f32; n]);
        let (seed_u, seed_v) = (context.seed ^ 0x3171_0001, context.seed ^ 0x3171_0002);

        for cy in 0..height {
            // In northern summer the bands move north (towards +latitude).
            let latitude = row_latitude(cy, height) - self.season * SEASON_SHIFT;
            let (u0, v0) = prevailing_wind(latitude.clamp(-1.0, 1.0));
            for cx in 0..width {
                let i = (cx + cy * width) as usize;
                let (x, y) = context.world_point(f64::from(cx), f64::from(cy));
                let (x, y) = (f64::from(x), f64::from(y));
                let nu = fbm(seed_u, x, y, self.wavelength, 3) - 0.5;
                let nv = fbm(seed_v, x, y, self.wavelength, 3) - 0.5;
                let (mut u, mut v) = (
                    u0 + self.turbulence * nu as f32 * 1.6,
                    v0 + self.turbulence * nv as f32 * 1.6,
                );
                // Calm bands still move air a little (no stagnant cells).
                let speed = (u * u + v * v).sqrt();
                if speed < 0.3 {
                    if speed < 1e-6 {
                        (u, v) = (0.3, 0.0);
                    } else {
                        (u, v) = (u / speed * 0.3, v / speed * 0.3);
                    }
                }
                us[i] = u;
                vs[i] = v;
            }
        }

        io.write_values(self.out_u, us)?;
        io.write_values(self.out_v, vs)?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------

pub static MOISTURE: NodeKind = NodeKind {
    id: "moisture",
    label: "Moisture",
    description: "Carries moisture from the ocean along the wind (after red blob games' \
                  mapgen4): air picks up water over the sea and rains it out over land, \
                  most where the terrain rises (windward slopes), leaving rain shadows \
                  behind mountains. The rain belt at the equator is wet, the subtropics \
                  dry. Output `precipitation` in mm per year.",
    inputs: &[
        atlas_in("elevation", false),
        atlas_in("wind_u", false),
        atlas_in("wind_v", false),
        atlas_in("temperature", true),
    ],
    outputs: &[
        atlas_out("precipitation", "mm/yr", 0.0, 3000.0, "thermal"),
        atlas_out("humidity", "0–1", 0.0, 1.0, "thermal"),
    ],
    params: &[
        // Advection passes: how far (cells ≈ passes × 1.5) moisture can travel.
        param("passes", 300.0, 10.0, 2000.0),
        // Share of the carried moisture that rains out per step everywhere.
        param("rain_rate", 0.01, 0.0, 0.2),
        // Extra rain per cell of terrain rise (windward slopes).
        param("orographic", 0.012, 0.0, 0.5),
        // Share of the rain over land that evaporates again (plants, soil) and
        // travels on: what keeps continental interiors from drying out.
        param("recycle", 0.55, 0.0, 0.95),
        // Extra rain at the equator (the rain belt).
        param("rain_belt", 0.02, 0.0, 0.2),
        // How much the subtropics (sinking dry air) suppress rain, 0 … 1.
        param("subtropics", 0.7, 0.0, 1.0),
        // Band shift: -1 northern winter, 0 annual mean, 1 northern summer.
        param("season", 0.0, -1.0, 1.0),
        // Precipitation (mm/yr) of one unit of rain per step.
        param("scale", 120000.0, 1000.0, 1000000.0),
        // Eddy mixing: share of the carried moisture exchanged with the air a
        // few cells away each pass. Reaches where the prevailing winds
        // diverge (the subtropical highs), so no land is bone dry. 0 = off.
        param("mixing", 0.0, 0.0, 0.5),
    ],
    build: Build::Atlas(|bindings, params, _| {
        Ok(Box::new(MoistureStage {
            elevation: bindings.input("elevation")?,
            wind_u: bindings.input("wind_u")?,
            wind_v: bindings.input("wind_v")?,
            temperature: bindings.optional_input("temperature"),
            out_precipitation: bindings.output("precipitation")?,
            out_humidity: bindings.output("humidity")?,
            params: MoistureParams {
                passes: params.get("passes").round() as u32,
                rain_rate: params.get("rain_rate") as f32,
                orographic: params.get("orographic") as f32,
                rain_belt: params.get("rain_belt") as f32,
                recycle: params.get("recycle") as f32,
                subtropics: params.get("subtropics") as f32,
                season: params.get("season") as f32,
                scale: params.get("scale") as f32,
                mixing: params.get("mixing") as f32,
            },
        }) as BoxedAtlasStage)
    }),
};

#[derive(Clone, Copy)]
struct MoistureParams {
    passes: u32,
    rain_rate: f32,
    orographic: f32,
    rain_belt: f32,
    recycle: f32,
    subtropics: f32,
    season: f32,
    scale: f32,
    mixing: f32,
}

struct MoistureStage {
    elevation: usize,
    wind_u: usize,
    wind_v: usize,
    temperature: Option<usize>,
    out_precipitation: usize,
    out_humidity: usize,
    params: MoistureParams,
}

/// Advection step length in cells per unit of wind speed.
const STEP: f32 = 1.5;
/// Most of the carried moisture that can rain out in one step.
const MAX_FALLOUT: f32 = 0.12;
/// Distance (cells) of the eddy mixing exchange.
const MIX_REACH: usize = 3;

/// Steady-state moisture transport: returns (precipitation mm/yr, humidity).
#[allow(clippy::too_many_arguments)]
fn moisture(
    elevation: &[f32],
    wind_u: &[f32],
    wind_v: &[f32],
    temperature: Option<&[f32]>,
    width: u32,
    height: u32,
    sea: f32,
    p: MoistureParams,
    run: &mut AtlasRun<'_>,
) -> Result<(Vec<f32>, Vec<f32>), String> {
    let (w, h) = (width as usize, height as usize);
    let n = w * h;

    // Upwind sample point of every cell, fixed for all passes: 4 corner
    // indices and bilinear weights.
    // Orographic lift follows the large-scale terrain, not its fine detail.
    let raw = elevation;
    let elevation = &smoothed(raw, w, h, 2);
    let mut corners = vec![[0u32; 4]; n];
    let mut weights = vec![[0f32; 4]; n];
    let mut fallout = vec![0f32; n];
    let mut saturation = vec![0f32; n];
    let mut ocean = vec![false; n];

    for y in 0..h {
        let latitude = row_latitude(y as u32, height) - p.season * SEASON_SHIFT;
        let a = latitude.abs();
        let belt = (-(latitude / 0.1).powi(2)).exp();
        let dry = 1.0 - p.subtropics * (-((a - 0.33) / 0.08).powi(2)).exp();
        for x in 0..w {
            let i = x + y * w;
            let px = (x as f32 - wind_u[i] * STEP).clamp(0.0, (w - 1) as f32);
            let py = (y as f32 - wind_v[i] * STEP).clamp(0.0, (h - 1) as f32);
            let (x0, y0) = (px.floor() as usize, py.floor() as usize);
            let (x1, y1) = ((x0 + 1).min(w - 1), (y0 + 1).min(h - 1));
            let (tx, ty) = (px - x0 as f32, py - y0 as f32);
            corners[i] = [
                (x0 + y0 * w) as u32,
                (x1 + y0 * w) as u32,
                (x0 + y1 * w) as u32,
                (x1 + y1 * w) as u32,
            ];
            weights[i] = [
                (1.0 - tx) * (1.0 - ty),
                tx * (1.0 - ty),
                (1.0 - tx) * ty,
                tx * ty,
            ];
            let upwind: f32 = (0..4)
                .map(|k| elevation[corners[i][k] as usize] * weights[i][k])
                .sum();
            let rise = (elevation[i].max(sea) - upwind.max(sea)).max(0.0);
            fallout[i] = ((p.rain_rate + p.rain_belt * belt) * dry + p.orographic * rise)
                .min(MAX_FALLOUT);
            ocean[i] = raw[i] < sea;
            // Warm water evaporates more; frozen seas barely.
            saturation[i] = match temperature {
                Some(t) => ((t[i] + 10.0) / 35.0).clamp(0.05, 1.2),
                None => (1.0 - a * 0.8).max(0.1),
            };
        }
    }
    run.progress(0.05);

    let mut carried = vec![0f32; n];
    let mut next = vec![0f32; n];
    let mut rain = vec![0f32; n];
    for pass in 0..p.passes {
        if pass % 16 == 0 {
            run.check()?;
        }
        for i in 0..n {
            let c = &corners[i];
            let wt = &weights[i];
            let incoming = carried[c[0] as usize] * wt[0]
                + carried[c[1] as usize] * wt[1]
                + carried[c[2] as usize] * wt[2]
                + carried[c[3] as usize] * wt[3];
            let fall = incoming * fallout[i];
            let mut left = incoming - fall;
            if ocean[i] {
                // The sea tops the air up towards saturation.
                left += (saturation[i] - left).max(0.0) * 0.5;
            } else {
                left += fall * p.recycle;
            }
            next[i] = left;
            rain[i] = fall;
        }
        if p.mixing > 0.0 {
            mix(&next, &mut carried, w, h, p.mixing);
        } else {
            std::mem::swap(&mut carried, &mut next);
        }
        run.progress(0.05 + 0.95 * f64::from(pass + 1) / f64::from(p.passes));
    }

    let precipitation = rain.iter().map(|r| r * p.scale).collect();
    let humidity = carried
        .iter()
        .zip(&saturation)
        .map(|(c, s)| (c / s.max(0.05)).clamp(0.0, 1.0))
        .collect();
    Ok((precipitation, humidity))
}

/// Eddy mixing: `out` = `from` blended (share `k`) with the mean of the
/// cells `MIX_REACH` away in the four directions (clamped at the border).
fn mix(from: &[f32], out: &mut [f32], w: usize, h: usize, k: f32) {
    let r = MIX_REACH;
    for y in 0..h {
        let (up, down) = (y.saturating_sub(r) * w, (y + r).min(h - 1) * w);
        let row = y * w;
        for x in 0..w {
            let (left, right) = (x.saturating_sub(r), (x + r).min(w - 1));
            let around =
                (from[row + left] + from[row + right] + from[up + x] + from[down + x]) * 0.25;
            out[row + x] = from[row + x] * (1.0 - k) + around * k;
        }
    }
}

impl AtlasStage for MoistureStage {
    fn run(&self, io: &mut AtlasIo<'_>, run: &mut AtlasRun<'_>) -> Result<(), String> {
        let context = *io.context();
        let (width, height) = context.grid;
        let (precipitation, humidity) = moisture(
            io.input(self.elevation)?.values(),
            io.input(self.wind_u)?.values(),
            io.input(self.wind_v)?.values(),
            io.optional(self.temperature)?.map(|g| g.values()),
            width,
            height,
            context.sea_level(),
            self.params,
            run,
        )?;
        let (w, h) = context.grid;
        run.snapshot(
            "precipitation",
            &crate::atlas::AtlasGrid::from_values(w, h, precipitation.clone())?,
        );
        io.write_values(self.out_precipitation, precipitation)?;
        io.write_values(self.out_humidity, humidity)?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------

pub static SEASONS: NodeKind = NodeKind {
    id: "seasons",
    label: "Seasons",
    description: "Seasonal climate normals that gameplay weather samples: annual mean, \
                  summer and winter temperature (warm equator, cold poles, colder with \
                  altitude; bigger swings far from the sea and at high latitudes), and \
                  summer and winter precipitation (monsoon rains in the tropics' summer, \
                  dry summers on mid-latitude coasts). Summer and winter are local: \
                  each hemisphere's own.",
    inputs: &[
        atlas_in("elevation", false),
        atlas_in("precipitation", true),
    ],
    outputs: &[
        atlas_out("temperature", "°C", -30.0, 40.0, "thermal"),
        atlas_out("temperature_summer", "°C", -30.0, 40.0, "thermal"),
        atlas_out("temperature_winter", "°C", -30.0, 40.0, "thermal"),
        atlas_out("precipitation_summer", "mm/yr", 0.0, 3000.0, "thermal"),
        atlas_out("precipitation_winter", "mm/yr", 0.0, 3000.0, "thermal"),
    ],
    params: &[
        // Sea-level annual mean at the equator (°C).
        param("equator", 28.0, -20.0, 60.0),
        // How much colder the poles are (falls off like 1 - cos(latitude)).
        param("pole_drop", 50.0, 0.0, 120.0),
        // Cooling per cell of height above sea level.
        param("lapse", 0.2, 0.0, 2.0),
        // Half the summer-winter difference at the poles, far inland (°C).
        param("seasonality", 25.0, 0.0, 60.0),
        // Distance from the sea (columns) where the climate is fully continental.
        param("continental", 1500.0, 50.0, 20000.0),
        // Strength of the summer monsoon (0 … 1).
        param("monsoon", 0.7, 0.0, 1.0),
    ],
    build: Build::Atlas(|bindings, params, _| {
        Ok(Box::new(SeasonsStage {
            elevation: bindings.input("elevation")?,
            precipitation: bindings.optional_input("precipitation"),
            outs: [
                bindings.output("temperature")?,
                bindings.output("temperature_summer")?,
                bindings.output("temperature_winter")?,
                bindings.output("precipitation_summer")?,
                bindings.output("precipitation_winter")?,
            ],
            equator: params.get("equator") as f32,
            pole_drop: params.get("pole_drop") as f32,
            lapse: params.get("lapse") as f32,
            seasonality: params.get("seasonality") as f32,
            continental: params.get("continental") as f32,
            monsoon: params.get("monsoon") as f32,
        }) as BoxedAtlasStage)
    }),
};

struct SeasonsStage {
    elevation: usize,
    precipitation: Option<usize>,
    outs: [usize; 5],
    equator: f32,
    pole_drop: f32,
    lapse: f32,
    seasonality: f32,
    continental: f32,
    monsoon: f32,
}

impl AtlasStage for SeasonsStage {
    fn run(&self, io: &mut AtlasIo<'_>, run: &mut AtlasRun<'_>) -> Result<(), String> {
        let context = *io.context();
        let (width, height) = context.grid;
        let sea = context.sea_level();
        let elevation = io.input(self.elevation)?.values();
        let precipitation = io.optional(self.precipitation)?.map(|g| g.values());
        let coast = ocean_distance(elevation, width, height, sea, context.cell_size as f32);
        let n = elevation.len();
        let mut out: [Vec<f32>; 5] = std::array::from_fn(|_| vec![0.0; n]);

        for cy in 0..height {
            run.check()?;
            let a = row_latitude(cy, height).abs();
            let tropic = (-((a - 0.18) / 0.1).powi(2)).exp();
            let mediterranean = (-((a - 0.4) / 0.06).powi(2)).exp();
            for cx in 0..width {
                let i = (cx + cy * width) as usize;
                let land = elevation[i] >= sea;
                // Earth-like: mild changes in the tropics, steep at mid
                // latitudes; the sea surface for ocean cells.
                let polar = 1.0 - (a * std::f32::consts::FRAC_PI_2).cos();
                let mean = self.equator
                    - self.pole_drop * polar
                    - self.lapse * (elevation[i] - sea).max(0.0);
                let inland = 1.0 - (-coast[i] / self.continental).exp();
                let swing = self.seasonality * a * (0.35 + 0.65 * inland);
                out[0][i] = mean;
                out[1][i] = mean + swing;
                out[2][i] = mean - swing;

                if let Some(p) = precipitation {
                    let monsoon = self.monsoon * tropic * if land { 1.0 } else { 0.5 };
                    let dry_summer = 0.6 * mediterranean * (1.0 - inland);
                    let m = (monsoon - dry_summer).clamp(-0.9, 0.9);
                    out[3][i] = p[i] * (1.0 + m);
                    out[4][i] = p[i] * (1.0 - m);
                }
            }
        }

        for (slot, values) in self.outs.iter().zip(out) {
            io.write_values(*slot, values)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use layergen::{ChunkSize, PipelineConfig, WorldBounds};

    use crate::{
        pipeline::{analyze, resolve, ResolvedNode},
        steps::default_graph,
        world::WorldStore,
    };

    #[test]
    fn mountains_rain_on_the_windward_side_and_leave_a_shadow() {
        use super::{moisture, MoistureParams};
        use crate::atlas::{AtlasHooks, AtlasRun};

        // Ocean in the west, land with a ridge in the middle; wind from the west.
        let (w, h) = (120u32, 8u32);
        let sea = 38.4;
        let elevation: Vec<f32> = (0..w * h)
            .map(|i| {
                let x = (i % w) as f32;
                if x < 20.0 {
                    10.0
                } else {
                    45.0 + 40.0 * (-((x - 60.0) / 8.0).powi(2)).exp()
                }
            })
            .collect();
        let wind_u = vec![1.0; (w * h) as usize];
        let wind_v = vec![0.0; (w * h) as usize];
        let params = MoistureParams {
            passes: 200,
            rain_rate: 0.01,
            orographic: 0.012,
            rain_belt: 0.0,
            recycle: 0.5,
            subtropics: 0.0,
            season: 0.0,
            scale: 120000.0,
            mixing: 0.0,
        };
        let mut run = AtlasRun::new(AtlasHooks::default(), 1);
        let (rain, _) =
            moisture(&elevation, &wind_u, &wind_v, None, w, h, sea, params, &mut run).unwrap();
        let at = |x: u32| rain[(x + 4 * w) as usize];
        assert!(at(55) > at(35), "windward slope is wetter than the plain");
        assert!(at(75) < at(55), "rain shadow behind the ridge");
    }

    #[test]
    fn mixing_brings_some_rain_where_the_wind_blows_off_the_land() {
        use super::{moisture, MoistureParams};
        use crate::atlas::{AtlasHooks, AtlasRun};

        // Land in the west, ocean in the east; a slow wind from the west
        // (offshore), as where the prevailing winds part.
        let (w, h) = (80u32, 8u32);
        let sea = 38.4;
        let elevation: Vec<f32> =
            (0..w * h).map(|i| if i % w < 60 { 45.0 } else { 10.0 }).collect();
        let wind_u = vec![0.3; (w * h) as usize];
        let wind_v = vec![0.0; (w * h) as usize];
        let rain = |mixing| {
            let params = MoistureParams {
                passes: 300,
                rain_rate: 0.01,
                orographic: 0.0,
                rain_belt: 0.0,
                recycle: 0.5,
                subtropics: 0.0,
                season: 0.0,
                scale: 120000.0,
                mixing,
            };
            let mut run = AtlasRun::new(AtlasHooks::default(), 1);
            moisture(&elevation, &wind_u, &wind_v, None, w, h, sea, params, &mut run)
                .unwrap()
                .0
        };
        let (still, mixed) = (rain(0.0), rain(0.3));
        let at = |r: &[f32], x: u32| r[(x + 4 * w) as usize];
        assert_eq!(at(&still, 55), 0.0, "no moisture ever reaches the lee coast");
        assert!(at(&mixed, 55) > 1.0, "eddies carry some inland");
        assert!(at(&mixed, 55) > at(&mixed, 40), "and the most near the sea");
    }

    #[test]
    fn climate_follows_latitude_altitude_and_sea_level() {
        let config = PipelineConfig::new(
            WorldBounds::new((-4096, -4096, 0), (8192, 8192, 128)).unwrap(),
            ChunkSize::new(32, 32, 32).unwrap(),
            21,
        );
        let doc = default_graph();
        let world = WorldStore::for_doc(&doc, &config).unwrap();

        let resolved: Vec<ResolvedNode> = doc.nodes.iter().map(resolve).collect();
        let schema = analyze(&doc, &resolved).schema;
        let (elevation, temperature, humidity) = (
            schema.slot("n2.elevation").unwrap(),
            schema.slot("n3.temperature").unwrap(),
            schema.slot("n4.humidity"),
        );
        // The default graph has no humidity node: climate nodes are optional.
        assert!(humidity.is_err());

        let (mut warm, mut cold) = (f64::MIN, f64::MAX);
        for i in 0..200i64 {
            let (x, y) = (-4096 + (i * 2791) % 8192, -4096 + (i * 6113) % 8192);
            let values = world.graph().eval(x, y);

            // temperature = equator - pole_drop * |latitude| - lapse * elevation
            let latitude = ((y + 4096) as f64 / 8192.0 * 2.0 - 1.0).abs();
            assert_eq!(
                values[temperature],
                32.0 - 50.0 * latitude - 0.25 * values[elevation]
            );

            if latitude < 0.1 {
                warm = warm.max(values[temperature]);
            }
            if latitude > 0.9 {
                cold = cold.min(values[temperature]);
            }
        }
        assert!(warm > cold, "equator {warm} vs pole {cold}");
    }

    #[test]
    fn humidity_is_wet_at_sea_and_dries_with_altitude_and_heat() {
        let config = PipelineConfig::new(
            WorldBounds::new((-4096, -4096, 0), (8192, 8192, 128)).unwrap(),
            ChunkSize::new(32, 32, 32).unwrap(),
            21,
        );
        // default graph + a humidity node wired to elevation and temperature
        let mut doc = default_graph();
        doc.nodes.push(crate::graph::NodeDoc {
            id: "n6".into(),
            kind: "humidity".into(),
            enabled: true,
            position: (0.0, 0.0),
            params: Default::default(),
            script: None,
            label: None,
        });
        for (from, out, input) in [
            ("n2", "elevation", "elevation"),
            ("n3", "temperature", "temperature"),
        ] {
            doc.edges.push(crate::graph::EdgeDoc {
                from: crate::graph::SocketRef {
                    node: from.into(),
                    socket: out.into(),
                },
                to: crate::graph::SocketRef {
                    node: "n6".into(),
                    socket: input.into(),
                },
            });
        }
        let world = WorldStore::for_doc(&doc, &config).unwrap();
        let resolved: Vec<ResolvedNode> = doc.nodes.iter().map(resolve).collect();
        let schema = analyze(&doc, &resolved).schema;
        let (elevation, humidity) = (
            schema.slot("n2.elevation").unwrap(),
            schema.slot("n6.humidity").unwrap(),
        );

        let (mut ocean, mut land) = (0, 0);
        for i in 0..400i64 {
            let (x, y) = (-4096 + (i * 2791) % 8192, -4096 + (i * 6113) % 8192);
            let values = world.graph().eval(x, y);
            assert!((0.0..=1.0).contains(&values[humidity]));

            if values[elevation] < 0.3 * 128.0 {
                assert_eq!(values[humidity], 1.0, "ocean is saturated");
                ocean += 1;
            } else {
                land += 1;
            }
        }
        assert!(ocean > 0 && land > 0);
    }
}
