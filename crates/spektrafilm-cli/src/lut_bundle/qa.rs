//! Compare the written CUBE chain against the live transform for each paper.

use std::{
    fs::{self, File},
    io::{BufRead, BufReader},
    path::Path,
};

use anyhow::{Context, Result, ensure};
use serde::Serialize;

const SAMPLE_COUNT: usize = 50_000;
const RNG_SEED: u64 = 20_260_515;

pub(super) fn reference_samples() -> Vec<[f64; 3]> {
    // NumPy PCG64 state and increment after SeedSequence(RNG_SEED), as used by
    // the Python reference sampler. Its encoded samples are rounded to f32.
    let mut state = 0xe266_3509_8516_7732_3541_84ff_b93c_a9f7_u128;
    let mut sample = || {
        state = state
            .wrapping_mul(0x2360_ed05_1fc6_5da4_4385_df64_9fcc_f645)
            .wrapping_add(0xba06_a268_7c52_68e2_626c_4d66_6044_bdc9);
        let word = ((state >> 64) ^ state) as u64;
        let value = word.rotate_right((state >> 122) as u32);
        let uniform = (value >> 11) as f64 / (1_u64 << 53) as f64;
        f64::from(uniform as f32)
    };
    (0..SAMPLE_COUNT)
        .map(|_| std::array::from_fn(|_| sample()))
        .collect()
}

pub(crate) struct Cube {
    pub(crate) size: usize,
    pub(crate) data: Vec<[f64; 3]>,
}

impl Cube {
    pub(crate) fn read(path: &Path) -> Result<Self> {
        let reader = BufReader::new(File::open(path)?);
        let mut size = None;
        let mut data = Vec::new();
        for line in reader.lines() {
            let line = line?;
            let line = line.split('#').next().unwrap_or_default().trim();
            let mut tokens = line.split_whitespace();
            match tokens.next() {
                None | Some("TITLE") => {}
                Some("LUT_3D_SIZE") => {
                    let n: usize = tokens.next().context("missing CUBE size")?.parse()?;
                    ensure!(
                        (2..=129).contains(&n) && size.is_none(),
                        "invalid CUBE size"
                    );
                    data.reserve(n * n * n);
                    size = Some(n);
                }
                Some("DOMAIN_MIN") => {
                    ensure!(rgb_row(tokens)? == [0.0; 3], "CUBE domain must start at 0")
                }
                Some("DOMAIN_MAX") => {
                    ensure!(rgb_row(tokens)? == [1.0; 3], "CUBE domain must end at 1")
                }
                Some(first) => {
                    let rgb = rgb_row(std::iter::once(first).chain(tokens))?;
                    ensure!(
                        rgb.iter().all(|v| (0.0..=1.0).contains(v)),
                        "CUBE samples must be in [0, 1]"
                    );
                    data.push(rgb);
                }
            }
        }
        let size = size.context("missing LUT_3D_SIZE")?;
        ensure!(
            data.len() == size * size * size,
            "wrong number of CUBE samples"
        );
        Ok(Self { size, data })
    }

    fn cell(&self, rgb: [f64; 3]) -> ([usize; 3], [f64; 3]) {
        let coords = rgb.map(|v| v.clamp(0.0, 1.0) * (self.size - 1) as f64);
        let base = coords.map(|v| (v as usize).min(self.size - 2));
        let fraction = std::array::from_fn(|i| coords[i] - base[i] as f64);
        (base, fraction)
    }

    fn get(&self, [r, g, b]: [usize; 3]) -> [f64; 3] {
        self.data[(b * self.size + g) * self.size + r]
    }

    fn trilinear(&self, rgb: [f64; 3]) -> [f64; 3] {
        let ([r, g, b], [fr, fg, fb]) = self.cell(rgb);
        let mut result = [0.0; 3];
        for (db, wb) in [1.0 - fb, fb].into_iter().enumerate() {
            for (dg, wg) in [1.0 - fg, fg].into_iter().enumerate() {
                for (dr, wr) in [1.0 - fr, fr].into_iter().enumerate() {
                    let corner = self.get([r + dr, g + dg, b + db]);
                    for (out, component) in result.iter_mut().zip(corner) {
                        *out += component * wr * wg * wb;
                    }
                }
            }
        }
        result
    }

    fn tetrahedral(&self, rgb: [f64; 3]) -> [f64; 3] {
        let (mut index, fraction) = self.cell(rgb);
        let mut axes = [0, 1, 2];
        axes.sort_unstable_by(|&a, &b| fraction[b].total_cmp(&fraction[a]));
        let [high, mid, low] = axes.map(|axis| fraction[axis]);
        let mut result = self.get(index).map(|v| v * (1.0 - high));
        for (axis, weight) in axes.into_iter().zip([high - mid, mid - low, low]) {
            index[axis] += 1;
            for (out, component) in result.iter_mut().zip(self.get(index)) {
                *out += weight * component;
            }
        }
        result
    }
}

fn rgb_row<'a>(mut tokens: impl Iterator<Item = &'a str>) -> Result<[f64; 3]> {
    let mut rgb = [0.0; 3];
    for value in &mut rgb {
        *value = tokens.next().context("short CUBE sample")?.parse()?;
    }
    ensure!(tokens.next().is_none(), "extra value in CUBE sample");
    Ok(rgb)
}

#[derive(Clone, Copy, Serialize)]
struct Summary {
    delta_e00_mean: f64,
    delta_e00_p50: f64,
    delta_e00_p99: f64,
    delta_e00_max: f64,
}

impl Summary {
    fn new(mut values: Vec<f64>) -> Self {
        values.sort_unstable_by(f64::total_cmp);
        let percentile = |p: f64| {
            let x = p * (values.len() - 1) as f64;
            let lower = x.floor() as usize;
            let upper = x.ceil() as usize;
            values[lower] + (values[upper] - values[lower]) * x.fract()
        };
        Self {
            delta_e00_mean: values.iter().sum::<f64>() / values.len() as f64,
            delta_e00_p50: percentile(0.50),
            delta_e00_p99: percentile(0.99),
            delta_e00_max: values[values.len() - 1],
        }
    }
}

#[derive(Clone, Copy, Serialize)]
struct InterpolationMetrics {
    trilinear: Summary,
    tetrahedral: Summary,
    within_sdr_target: bool,
}

#[derive(Serialize)]
pub(super) struct Metrics {
    sequential: InterpolationMetrics,
    resampled: InterpolationMetrics,
}

pub(super) fn evaluate(
    directory: &Path,
    paths: &[&str],
    samples: &[[f64; 3]],
    expected: &[[f64; 3]],
    to_lab: impl Fn([f64; 3]) -> [f64; 3],
) -> Result<Metrics> {
    ensure!(!paths.is_empty(), "QA needs at least one CUBE");
    ensure!(
        !samples.is_empty() && samples.len() == expected.len(),
        "QA reference size mismatch"
    );
    let cubes: Vec<_> = paths
        .iter()
        .map(|path| {
            Cube::read(&directory.join(path)).with_context(|| format!("reading QA CUBE {path}"))
        })
        .collect::<Result<_>>()?;

    let apply_chain = |input, interpolate: fn(&Cube, [f64; 3]) -> [f64; 3]| {
        cubes
            .iter()
            .fold(input, |value, cube| interpolate(cube, value))
    };
    let sequential = measure(samples, expected, &to_lab, |input| {
        (
            apply_chain(input, Cube::trilinear),
            apply_chain(input, Cube::tetrahedral),
        )
    })?;
    let resampled = if cubes.len() == 1 {
        sequential
    } else {
        // The Python QA suite evaluates a virtual CUBE made by applying
        // each stage trilinearly at the bundle's original grid nodes.
        let size = cubes[0].size;
        let cube = Cube {
            size,
            data: super::cube_grid(size as u32)
                .map(|input| apply_chain(input, Cube::trilinear))
                .collect(),
        };
        measure(samples, expected, &to_lab, |input| {
            (cube.trilinear(input), cube.tetrahedral(input))
        })?
    };
    Ok(Metrics {
        sequential,
        resampled,
    })
}

fn measure(
    samples: &[[f64; 3]],
    expected: &[[f64; 3]],
    to_lab: &impl Fn([f64; 3]) -> [f64; 3],
    interpolate: impl Fn([f64; 3]) -> ([f64; 3], [f64; 3]),
) -> Result<InterpolationMetrics> {
    let mut trilinear = Vec::with_capacity(samples.len());
    let mut tetrahedral = Vec::with_capacity(samples.len());
    for (&input, &output) in samples.iter().zip(expected) {
        ensure!(
            input.into_iter().chain(output).all(f64::is_finite),
            "non-finite QA sample"
        );
        let target = to_lab(output);
        let (tri, tetra) = interpolate(input);
        let tri = to_lab(tri);
        let tetra = to_lab(tetra);
        ensure!(
            target
                .into_iter()
                .chain(tri)
                .chain(tetra)
                .all(f64::is_finite),
            "non-finite Lab color"
        );
        let tri_error = delta_e00(tri, target);
        let tetra_error = delta_e00(tetra, target);
        ensure!(
            tri_error.is_finite() && tetra_error.is_finite(),
            "non-finite color difference"
        );
        trilinear.push(tri_error);
        tetrahedral.push(tetra_error);
    }
    let trilinear = Summary::new(trilinear);
    let tetrahedral = Summary::new(tetrahedral);
    let within_sdr_target = [&trilinear, &tetrahedral]
        .into_iter()
        .all(|s| s.delta_e00_max <= 2.0 && s.delta_e00_p99 <= 1.0);
    Ok(InterpolationMetrics {
        trilinear,
        tetrahedral,
        within_sdr_target,
    })
}

pub(super) fn write_report(directory: &Path, prints: Vec<(String, Metrics)>) -> Result<()> {
    let prints = prints
        .into_iter()
        .collect::<std::collections::BTreeMap<_, _>>();
    let report = serde_json::json!({
        "schema_version": 1,
        "method": "CUBE readback vs live simulation, both sequential and trilinearly resampled to the bundle grid",
        "rng": "PCG64",
        "seed": RNG_SEED,
        "samples_per_print": SAMPLE_COUNT,
        "prints": prints,
    });
    fs::write(
        directory.join("qa.json"),
        serde_json::to_vec_pretty(&report)?,
    )
    .context("writing LUT QA report")
}

pub(super) fn xyz_to_lab(xyz: [f64; 3], white: [f64; 3]) -> [f64; 3] {
    const DELTA: f64 = 6.0 / 29.0;
    let [x, y, z] = std::array::from_fn(|i| {
        let t = xyz[i] / white[i];
        if t > DELTA.powi(3) {
            t.cbrt()
        } else {
            t / (3.0 * DELTA.powi(2)) + 4.0 / 29.0
        }
    });
    [116.0 * y - 16.0, 500.0 * (x - y), 200.0 * (y - z)]
}

fn delta_e00([l1, a1, b1]: [f64; 3], [l2, a2, b2]: [f64; 3]) -> f64 {
    let mean_chroma = (a1.hypot(b1) + a2.hypot(b2)) * 0.5;
    let chroma_factor = |c: f64| {
        let c7 = c.powi(7);
        (c7 / (c7 + 25.0_f64.powi(7))).sqrt()
    };
    let a_scale = 1.0 + 0.5 * (1.0 - chroma_factor(mean_chroma));
    let hue = |a: f64, b: f64| {
        if a == 0.0 && b == 0.0 {
            0.0
        } else {
            b.atan2(a).to_degrees().rem_euclid(360.0)
        }
    };
    let a1 = a1 * a_scale;
    let a2 = a2 * a_scale;
    let c1 = a1.hypot(b1);
    let c2 = a2.hypot(b2);
    let h1 = hue(a1, b1);
    let h2 = hue(a2, b2);
    let hue_difference = h2 - h1;
    let zero_chroma = c1 == 0.0 || c2 == 0.0;
    let hue_delta = if zero_chroma {
        0.0
    } else if hue_difference.abs() <= 180.0 {
        hue_difference
    } else {
        hue_difference - 360.0 * hue_difference.signum()
    };
    let mean_hue = if zero_chroma {
        h1 + h2
    } else if hue_difference.abs() <= 180.0 {
        (h1 + h2) * 0.5
    } else {
        ((h1 + h2 + 360.0) * 0.5).rem_euclid(360.0)
    };
    let mean_lightness = (l1 + l2) * 0.5;
    let mean_chroma = (c1 + c2) * 0.5;
    let t = 1.0 - 0.17 * (mean_hue - 30.0).to_radians().cos()
        + 0.24 * (2.0 * mean_hue).to_radians().cos()
        + 0.32 * (3.0 * mean_hue + 6.0).to_radians().cos()
        - 0.20 * (4.0 * mean_hue - 63.0).to_radians().cos();
    let lightness_offset = (mean_lightness - 50.0).powi(2);
    let lightness_weight = 1.0 + 0.015 * lightness_offset / (20.0 + lightness_offset).sqrt();
    let chroma_weight = 1.0 + 0.045 * mean_chroma;
    let hue_weight = 1.0 + 0.015 * mean_chroma * t;
    let rotation = -2.0
        * chroma_factor(mean_chroma)
        * (60.0 * (-((mean_hue - 275.0) / 25.0).powi(2)).exp())
            .to_radians()
            .sin();

    let dl = (l2 - l1) / lightness_weight;
    let dc = (c2 - c1) / chroma_weight;
    let dh = 2.0 * (c1 * c2).sqrt() * (0.5 * hue_delta).to_radians().sin() / hue_weight;
    (dl * dl + dc * dc + dh * dh + rotation * dc * dh)
        .max(0.0)
        .sqrt()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sampling_matches_the_python_reference() {
        let expected = [
            [
                0xcbd4_0c95_57b7_0cbb_u64,
                0x1145_cbc9_c7a0_c646,
                0x6c00_0da5_eac4_d9d8,
            ],
            [
                0xdf9e_e90b_0a09_a93e,
                0x91da_7ecd_a94d_0e5b,
                0xe313_8743_1809_de98,
            ],
        ]
        .map(|row| row.map(|raw| f64::from(((raw >> 11) as f64 / (1_u64 << 53) as f64) as f32)));
        assert_eq!(reference_samples()[..2], expected);
    }

    #[test]
    fn interpolates_red_fastest_and_preserves_cube_bounds() {
        let mut data = Vec::new();
        for b in [0.0, 1.0] {
            for g in [0.0, 1.0] {
                for r in [0.0, 1.0] {
                    data.push([r * g, g, b]);
                }
            }
        }
        let cube = Cube { size: 2, data };
        assert_eq!(cube.trilinear([0.75, 0.25, 0.5]), [0.1875, 0.25, 0.5]);
        assert_eq!(cube.tetrahedral([0.75, 0.25, 0.5]), [0.25, 0.25, 0.5]);
        assert_eq!(cube.tetrahedral([1.0, 1.0, 1.0]), [1.0; 3]);
    }

    #[test]
    fn color_difference_matches_published_reference_pairs() {
        for (a, b, expected) in [
            ([50.0, 2.6772, -79.7751], [50.0, 0.0, -82.7485], 2.0425),
            ([50.0, 3.1571, -77.2803], [50.0, 0.0, -82.7485], 2.8615),
            ([50.0, 0.0, 0.0], [50.0, -1.0, 2.0], 2.3669),
        ] {
            assert!((delta_e00(a, b) - expected).abs() < 5e-5);
        }
        assert_eq!(
            xyz_to_lab([0.9505, 1.0, 1.0888], [0.9505, 1.0, 1.0888]),
            [100.0, 0.0, 0.0]
        );
    }
}
