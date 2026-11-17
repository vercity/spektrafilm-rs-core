//! Compare the written CUBE chain against the live transform for each paper.

use std::{
    fs::{self, File},
    io::{BufRead, BufReader},
    path::Path,
};

use anyhow::{Context, Result, ensure};
use serde::Serialize;
use spektrafilm_math::colorspace::XYZ_TO_REC2020_F64;

use super::transport::Encoding;

mod curves;
mod diagnostics;
mod gamut_edge;
mod hull;
mod input_gamut;
mod model_usage;
mod noise_field;
mod noise_gradient;
mod output_gamut;
mod report;
mod rng;
mod spectral_envelope;

const SAMPLE_COUNT: usize = 50_000;
const RNG_SEED: u64 = 20_260_515;

pub(super) fn reference_samples() -> Vec<[f64; 3]> {
    // NumPy PCG64 state and increment after SeedSequence(RNG_SEED), as used by
    // the Python reference sampler. Its encoded samples are rounded to f32.
    let mut rng = rng::Pcg64::new(
        0xe266_3509_8516_7732_3541_84ff_b93c_a9f7,
        0xba06_a268_7c52_68e2_626c_4d66_6044_bdc9,
    );
    (0..SAMPLE_COUNT)
        .map(|_| std::array::from_fn(|_| f64::from(rng.uniform() as f32)))
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
        let mix =
            |a: [f64; 3], b: [f64; 3], t| std::array::from_fn(|i| a[i] * (1.0 - t) + b[i] * t);
        let plane = |b| {
            mix(
                mix(self.get([r, g, b]), self.get([r + 1, g, b]), fr),
                mix(self.get([r, g + 1, b]), self.get([r + 1, g + 1, b]), fr),
                fg,
            )
        };
        mix(plane(b), plane(b + 1), fb)
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
    mean: f64,
    p50: f64,
    p95: f64,
    p99: f64,
    max: f64,
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
            mean: values.iter().sum::<f64>() / values.len() as f64,
            p50: percentile(0.50),
            p95: percentile(0.95),
            p99: percentile(0.99),
            max: values[values.len() - 1],
        }
    }
}

#[derive(Clone, Copy, Serialize)]
struct ErrorMetrics {
    delta_e00_mean: f64,
    delta_e00_p50: f64,
    delta_e00_p95: f64,
    delta_e00_p99: f64,
    delta_e00_max: f64,
    delta_itp_mean: f64,
    delta_itp_p50: f64,
    delta_itp_p95: f64,
    delta_itp_p99: f64,
    delta_itp_max: f64,
}

impl ErrorMetrics {
    fn new(delta_e00: Vec<f64>, delta_itp: Vec<f64>) -> Self {
        let de = Summary::new(delta_e00);
        let itp = Summary::new(delta_itp);
        Self {
            delta_e00_mean: de.mean,
            delta_e00_p50: de.p50,
            delta_e00_p95: de.p95,
            delta_e00_p99: de.p99,
            delta_e00_max: de.max,
            delta_itp_mean: itp.mean,
            delta_itp_p50: itp.p50,
            delta_itp_p95: itp.p95,
            delta_itp_p99: itp.p99,
            delta_itp_max: itp.max,
        }
    }
}

#[derive(Clone, Copy, Serialize)]
struct InterpolationMetrics {
    trilinear: ErrorMetrics,
    tetrahedral: ErrorMetrics,
    within_sdr_target: bool,
}

#[derive(Serialize)]
pub(super) struct Metrics {
    sequential: InterpolationMetrics,
    resampled: InterpolationMetrics,
    diagnostics: diagnostics::Diagnostics,
    characteristic_curve: curves::Characteristic,
    planckian_sweep: curves::Planckian,
    dynamic_range_usage: model_usage::DynamicRange,
    hue_twist_oklab: model_usage::HueTwist,
    rg_plane_slices: model_usage::RgPlanes,
    spectral_locus_envelope: spectral_envelope::Envelope,
    output_gamut_compression: output_gamut::Compression,
    noise_gradient: noise_gradient::Gradient,
    #[serde(skip_serializing_if = "Option::is_none")]
    output_gamut_edge_stress: Option<gamut_edge::Outcome>,
    #[serde(skip_serializing_if = "Option::is_none")]
    input_gamut_compression: Option<input_gamut::Outcome>,
}

#[derive(Clone, Copy)]
pub(super) struct Frame {
    pub(super) input: Encoding,
    pub(super) output: Encoding,
    pub(super) gain: f64,
}

pub(super) fn evaluate(
    directory: &Path,
    paths: &[&str],
    samples: &[[f64; 3]],
    expected: &[[f64; 3]],
    frame: Frame,
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
    let output = frame.output;
    let expected: Vec<_> = expected
        .iter()
        .map(|&rgb| qa_coordinates(rgb, output))
        .collect();
    let sequential = measure(samples, &expected, output, |input| {
        (
            apply_chain(input, Cube::trilinear),
            apply_chain(input, Cube::tetrahedral),
        )
    })?;
    // Python QA uses this effective CUBE for both the off-grid measurement
    // and the structural diagnostics. The sequential section measures the
    // actual host order separately.
    let single = cubes.len() == 1;
    let effective = if single {
        cubes.into_iter().next().expect("one CUBE")
    } else {
        let size = cubes[0].size;
        let step = 1.0 / (size - 1) as f64;
        let code = |i| if i == size - 1 { 1.0 } else { i as f64 * step };
        let grid = (0..size).flat_map(|b| {
            (0..size).flat_map(move |g| (0..size).map(move |r| [code(r), code(g), code(b)]))
        });
        Cube {
            size,
            data: grid
                .map(|input| apply_chain(input, Cube::trilinear))
                .collect(),
        }
    };
    let resampled = if single {
        sequential
    } else {
        measure(samples, &expected, output, |input| {
            (effective.trilinear(input), effective.tetrahedral(input))
        })?
    };
    let diagnostics = diagnostics::evaluate(&effective, frame);
    let output_gamut_compression = output_gamut::Compression::evaluate(
        &effective,
        frame,
        diagnostics.gamut_face_folds.fold_triangles,
        diagnostics.gamut_face_folds.fold_fraction,
    );
    Ok(Metrics {
        sequential,
        resampled,
        diagnostics,
        output_gamut_compression,
        noise_gradient: noise_gradient::evaluate(&effective, frame)?,
        characteristic_curve: curves::characteristic(&effective),
        planckian_sweep: curves::planckian(&effective, frame),
        dynamic_range_usage: model_usage::dynamic_range(&effective, frame),
        hue_twist_oklab: model_usage::hue_twist(&effective, frame),
        rg_plane_slices: model_usage::rg_planes(&effective, frame)?,
        spectral_locus_envelope: spectral_envelope::evaluate(&effective, frame)?,
        output_gamut_edge_stress: None,
        input_gamut_compression: None,
    })
}

impl Metrics {
    pub(super) fn gamut_diagnostics(
        &mut self,
        frame: Frame,
        pipeline: &spektrafilm_core::pipeline::LutPipeline,
    ) {
        self.input_gamut_compression = Some(
            match input_gamut::evaluate(self.rg_plane_slices.cube_resolution, frame, pipeline) {
                Ok(reports) => input_gamut::Outcome::Reports(Box::new(reports)),
                Err(error) => input_gamut::Outcome::Error {
                    error: format!("{error:#}"),
                },
            },
        );
        self.output_gamut_compression.evaluate_rim(frame, pipeline);
        self.output_gamut_edge_stress = Some(match gamut_edge::evaluate(frame, pipeline) {
            Ok(panels) => gamut_edge::Outcome::Panels(panels),
            Err(error) => gamut_edge::Outcome::Error {
                error: format!("{error:#}"),
            },
        });
    }
}

fn measure(
    samples: &[[f64; 3]],
    expected: &[QaCoordinates],
    output: Encoding,
    interpolate: impl Fn([f64; 3]) -> ([f64; 3], [f64; 3]),
) -> Result<InterpolationMetrics> {
    let mut trilinear_de = Vec::with_capacity(samples.len());
    let mut tetrahedral_de = Vec::with_capacity(samples.len());
    let mut trilinear_itp = Vec::with_capacity(samples.len());
    let mut tetrahedral_itp = Vec::with_capacity(samples.len());
    for (&input, target) in samples.iter().zip(expected) {
        ensure!(input.into_iter().all(f64::is_finite), "non-finite QA input");
        let (tri, tetra) = interpolate(input);
        let tri = qa_coordinates(tri, output);
        let tetra = qa_coordinates(tetra, output);
        let tri_de = delta_e00(tri.lab, target.lab);
        let tetra_de = delta_e00(tetra.lab, target.lab);
        let tri_itp = delta_itp(tri.ictcp, target.ictcp);
        let tetra_itp = delta_itp(tetra.ictcp, target.ictcp);
        ensure!(
            [tri_de, tetra_de, tri_itp, tetra_itp]
                .into_iter()
                .all(f64::is_finite),
            "non-finite color difference"
        );
        trilinear_de.push(tri_de);
        tetrahedral_de.push(tetra_de);
        trilinear_itp.push(tri_itp);
        tetrahedral_itp.push(tetra_itp);
    }
    let trilinear = ErrorMetrics::new(trilinear_de, trilinear_itp);
    let tetrahedral = ErrorMetrics::new(tetrahedral_de, tetrahedral_itp);
    let within_sdr_target = [&trilinear, &tetrahedral]
        .into_iter()
        .all(|s| s.delta_e00_max <= 2.0 && s.delta_e00_p99 <= 1.0);
    Ok(InterpolationMetrics {
        trilinear,
        tetrahedral,
        within_sdr_target,
    })
}

pub(super) fn write_report(
    directory: &Path,
    film: &str,
    prints: Vec<(String, Metrics)>,
) -> Result<()> {
    for (paper, metrics) in &prints {
        report::write_html(directory, film, paper, metrics)?;
    }
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

const D65: [f64; 3] = [0.950_455_927_051_671_6, 1.0, 1.089_057_750_759_878_4];

fn mat_vec(matrix: [[f64; 3]; 3], vector: [f64; 3]) -> [f64; 3] {
    matrix.map(|row| row.iter().zip(vector).map(|(a, b)| a * b).sum())
}

fn to_xyz_qa(code: [f64; 3], output: Encoding) -> [f64; 3] {
    let linear = output
        .decode(code)
        .map(|value| value / output.output_gain());
    mat_vec(to_xyz_matrix(output), linear)
}

fn to_xyz_matrix(encoding: Encoding) -> [[f64; 3]; 3] {
    encoding.space().to_xyz()
}

fn from_xyz_matrix(encoding: Encoding) -> [[f64; 3]; 3] {
    encoding.space().from_xyz()
}

struct QaCoordinates {
    lab: [f64; 3],
    ictcp: [f64; 3],
}

fn qa_coordinates(code: [f64; 3], output: Encoding) -> QaCoordinates {
    let xyz = to_xyz_qa(code, output);
    // Python's to_xyz_qa retains native primaries and its XYZ_to_Lab call
    // uses the default D65 reference, including for ProPhoto RGB.
    QaCoordinates {
        lab: xyz_to_lab(xyz, D65),
        ictcp: xyz_to_ictcp(xyz),
    }
}

fn xyz_to_ictcp(xyz: [f64; 3]) -> [f64; 3] {
    let rgb = mat_vec(XYZ_TO_REC2020_F64, xyz);
    let lms = mat_vec(
        [
            [1688.0, 2146.0, 262.0],
            [683.0, 2951.0, 462.0],
            [99.0, 309.0, 3688.0],
        ],
        rgb,
    );
    // Dolby 2016 with the ST 2084 inverse EOTF, as used by Colour's
    // XYZ_to_ICtCp on the reflectance-scale XYZ of Python's QA suite.
    let lms = lms.map(|value| {
        let power = signed_pow(value / (4096.0 * 10_000.0), 2610.0 / 16384.0);
        signed_pow(
            (3424.0 / 4096.0 + (2413.0 / 128.0) * power) / (1.0 + (2392.0 / 128.0) * power),
            2523.0 / 32.0,
        )
    });
    mat_vec(
        [
            [2048.0, 2048.0, 0.0],
            [6610.0, -13613.0, 7003.0],
            [17933.0, -17390.0, -543.0],
        ],
        lms,
    )
    .map(|value| value / 4096.0)
}

fn signed_pow(value: f64, power: f64) -> f64 {
    value.abs().powf(power).copysign(value)
}

fn delta_itp(a: [f64; 3], b: [f64; 3]) -> f64 {
    let [i, t, p] = std::array::from_fn(|index| a[index] - b[index]);
    720.0 * (i * i + 0.25 * t * t + p * p).sqrt()
}

fn xyz_to_lab(xyz: [f64; 3], white: [f64; 3]) -> [f64; 3] {
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
    fn encoded_color_errors_match_python_d65_lab_and_ictcp() {
        let a = [0.6, 0.18, 0.9];
        let b = [0.48, 0.25, 0.86];
        // Python 0.3.4 qa.metrics, using Colour 0.4.7.
        for (space, de, itp) in [
            (
                Encoding::Srgb,
                5.071_269_143_041_611_5,
                10.688_941_268_084_59,
            ),
            (
                Encoding::Prophoto,
                8.787_045_156_965_615,
                26.299_023_980_977_022,
            ),
            (
                Encoding::Rec709,
                5.071_093_430_310_547_5,
                11.142_747_874_035_01,
            ),
            (
                Encoding::Rec2020,
                6.632_852_942_837_3,
                18.563_571_013_037_055,
            ),
            (
                Encoding::Rec2100Pq,
                11.078_143_028_218_383,
                19.280_061_856_468_617,
            ),
        ] {
            let a = qa_coordinates(a, space);
            let b = qa_coordinates(b, space);
            assert!((delta_e00(a.lab, b.lab) - de).abs() < 1e-10, "{space:?}");
            assert!(
                (delta_itp(a.ictcp, b.ictcp) - itp).abs() < 1e-10,
                "{space:?}"
            );
        }
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
