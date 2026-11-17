use rustfft::{FftPlanner, num_complex::Complex};
use serde::Serialize;

use super::{Cube, Frame, Summary, from_xyz_matrix, mat_vec, to_xyz_qa};

#[derive(Serialize)]
pub(super) struct Diagnostics {
    monotonicity: Monotonicity,
    jacobian_condition: Option<Summary>,
    total_variation: TotalVariation,
    pub(super) gamut_face_folds: GamutFolds,
    noise_sensitivity: Option<Noise>,
    #[serde(skip)]
    pub(super) centerlines: [[f64; 65]; 3],
    #[serde(skip)]
    pub(super) condition_planes: [Vec<f64>; 3],
}

#[derive(Serialize)]
struct Monotonicity {
    violations: usize,
    worst_negative_diff: f64,
    centerline_pin_encoded: f64,
    centerline_violations: usize,
    passed: bool,
}

#[derive(Serialize)]
struct TotalVariation {
    tv: f64,
    tv_r: f64,
    tv_g: f64,
    tv_b: f64,
    axial_highband_ratio_r: f64,
    axial_highband_ratio_g: f64,
    axial_highband_ratio_b: f64,
    axial_highband_ratio_mean: f64,
}

#[derive(Serialize)]
pub(super) struct GamutFolds {
    pub(super) fold_triangles: usize,
    pub(super) fold_fraction: f64,
    triangles: usize,
}

#[derive(Serialize)]
struct Noise {
    input_samples: usize,
    sigma_in_encoded: f64,
    l_slice: f64,
    sigma1: Summary,
    anisotropy: Summary,
    sigma_lightness: Summary,
    sigma_chroma: Summary,
    max_hue_rotation_deg: f64,
    worst_hue_deg: f64,
}

pub(super) fn evaluate(cube: &Cube, frame: Frame) -> Diagnostics {
    let mut abs_differences = [0.0_f64; 3];
    let mut violations = 0;
    let mut worst_negative_diff = 0.0_f64;
    for b in 0..cube.size {
        for g in 0..cube.size {
            for r in 0..cube.size {
                let index = [r, g, b];
                let value = cube.get(index);
                for axis in 0..3 {
                    let mut next = index;
                    next[axis] += 1;
                    if next[axis] == cube.size {
                        continue;
                    }
                    let difference = subtract(cube.get(next), value);
                    abs_differences[axis] += difference.iter().map(|v| v.abs()).sum::<f64>();
                    if difference[axis] < 0.0 {
                        violations += 1;
                        worst_negative_diff = worst_negative_diff.min(difference[axis]);
                    }
                }
            }
        }
    }

    let pin = frame.input.encode([0.18 / frame.gain; 3])[0];
    let centerlines: [[f64; 65]; 3] = std::array::from_fn(|axis| {
        std::array::from_fn(|i| {
            let mut sample = [f64::from(pin as f32); 3];
            sample[axis] = i as f64 / 64.0;
            cube.trilinear(sample)[axis]
        })
    });
    let centerline_violations = centerlines
        .iter()
        .flat_map(|s| s.windows(2))
        .filter(|pair| pair[1] < pair[0])
        .count();

    let n = cube.size;
    let interior_size = n - 2;
    let mut conditions = Vec::with_capacity(interior_size.pow(3));
    let mut condition_planes = std::array::from_fn(|_| vec![0.0_f64; interior_size.pow(2)]);
    for b in 1..n - 1 {
        for g in 1..n - 1 {
            for r in 1..n - 1 {
                let j = [
                    subtract(cube.get([r + 1, g, b]), cube.get([r - 1, g, b])),
                    subtract(cube.get([r, g + 1, b]), cube.get([r, g - 1, b])),
                    subtract(cube.get([r, g, b + 1]), cube.get([r, g, b - 1])),
                ];
                let [min, _, max] = singular_values(j);
                let cond = (max / min.max(1e-12)).max(1.0).log10();
                conditions.push(cond);
                let [x, y, z] = [r - 1, g - 1, b - 1];
                let offsets = [
                    z * interior_size + y,
                    z * interior_size + x,
                    y * interior_size + x,
                ];
                for (plane, offset) in condition_planes.iter_mut().zip(offsets) {
                    plane[offset] = plane[offset].max(cond);
                }
            }
        }
    }
    let [tv_r, tv_g, tv_b] = abs_differences.map(|sum| sum / (3 * n * n * (n - 1)) as f64);
    let [r, g, b] = axial_fft(cube);
    Diagnostics {
        monotonicity: Monotonicity {
            violations,
            worst_negative_diff,
            centerline_pin_encoded: pin,
            centerline_violations,
            passed: violations == 0,
        },
        jacobian_condition: (!conditions.is_empty()).then(|| Summary::new(conditions)),
        total_variation: TotalVariation {
            tv: tv_r + tv_g + tv_b,
            tv_r,
            tv_g,
            tv_b,
            axial_highband_ratio_r: r,
            axial_highband_ratio_g: g,
            axial_highband_ratio_b: b,
            axial_highband_ratio_mean: (r + g + b) / 3.0,
        },
        gamut_face_folds: face_folds(cube),
        noise_sensitivity: noise(cube, frame),
        centerlines,
        condition_planes,
    }
}

fn subtract(a: [f64; 3], b: [f64; 3]) -> [f64; 3] {
    std::array::from_fn(|i| a[i] - b[i])
}

fn dot(a: [f64; 3], b: [f64; 3]) -> f64 {
    a.iter().zip(b).map(|(p, q)| p * q).sum()
}

// One-sided Jacobi rotations retain relative accuracy for small singular
// values. Forming JᵀJ explicitly would square the condition number.
fn singular_values(mut columns: [[f64; 3]; 3]) -> [f64; 3] {
    for _ in 0..12 {
        let mut rotated = false;
        for (i, j) in [(0, 1), (0, 2), (1, 2)] {
            let a = columns[i];
            let b = columns[j];
            let aa = dot(a, a);
            let bb = dot(b, b);
            let ab = dot(a, b);
            if ab.abs() <= 2.0 * f64::EPSILON * (aa * bb).sqrt() {
                continue;
            }
            let tau = (bb - aa) / (2.0 * ab);
            let t = tau.signum() / (tau.abs() + tau.hypot(1.0));
            let c = 1.0 / t.hypot(1.0);
            let s = c * t;
            columns[i] = std::array::from_fn(|k| c * a[k] - s * b[k]);
            columns[j] = std::array::from_fn(|k| s * a[k] + c * b[k]);
            rotated = true;
        }
        if !rotated {
            break;
        }
    }
    let mut values = columns.map(|column| dot(column, column).sqrt());
    values.sort_by(f64::total_cmp);
    values
}

fn axial_fft(cube: &Cube) -> [f64; 3] {
    let n = cube.size;
    let mut planner = FftPlanner::<f64>::new();
    let fft = planner.plan_fft_forward(n);
    let mut scratch = vec![Complex::default(); fft.get_inplace_scratch_len()];
    let mut line = vec![Complex::default(); n];
    // Upstream labels axes in storage order [b,g,r], despite the r/g/b keys.
    // Retain its ordering so reports agree with qa.metrics.
    [2, 1, 0].map(|axis| {
        let [u, v] = [(axis + 1) % 3, (axis + 2) % 3];
        let start_high = (n / 4).max(1);
        let mut total = 0.0;
        let mut high = 0.0;
        let mut index = [0; 3];
        for a in 0..n {
            for b in 0..n {
                index[u] = a;
                index[v] = b;
                for channel in 0..3 {
                    for (i, sample) in line.iter_mut().enumerate() {
                        index[axis] = i;
                        *sample = Complex::new(cube.get(index)[channel], 0.0);
                    }
                    fft.process_with_scratch(&mut line, &mut scratch);
                    for (k, coefficient) in line[..=n / 2].iter().enumerate() {
                        let amplitude = coefficient.norm();
                        total += amplitude;
                        if k >= start_high {
                            high += amplitude;
                        }
                    }
                }
            }
        }
        high / total.max(1e-12)
    })
}

fn face_folds(cube: &Cube) -> GamutFolds {
    let n = cube.size;
    let mut folds = 0;
    let mut triangles = 0;
    if n >= 3 {
        for axis in 0..3 {
            let keep = [(axis + 1) % 3, (axis + 2) % 3];
            let [u, v] = [2 - keep[0], 2 - keep[1]];
            for fixed in [0, n - 1] {
                let mut positive = 0;
                let mut negative = 0;
                for a in 0..n - 1 {
                    for b in 0..n - 1 {
                        let mut pos = [0; 3];
                        pos[2 - axis] = fixed;
                        pos[u] = a;
                        pos[v] = b;
                        let p = cube.get(pos);
                        pos[u] += 1;
                        let du = subtract(cube.get(pos), p);
                        pos[u] -= 1;
                        pos[v] += 1;
                        let dv = subtract(cube.get(pos), p);
                        let det = du[keep[0]] * dv[keep[1]] - du[keep[1]] * dv[keep[0]];
                        positive += usize::from(det > 0.0);
                        negative += usize::from(det < 0.0);
                    }
                }
                folds += positive.min(negative);
                triangles += (n - 1).pow(2);
            }
        }
    }
    GamutFolds {
        fold_triangles: folds,
        fold_fraction: folds as f64 / triangles.max(1) as f64,
        triangles,
    }
}

pub(super) fn to_oklab(xyz: [f64; 3]) -> [f64; 3] {
    let lms = mat_vec(
        [
            [0.8189330101, 0.3618667424, -0.1288597137],
            [0.0329845436, 0.9293118715, 0.0361456387],
            [0.0482003018, 0.2643662691, 0.633851707],
        ],
        xyz,
    )
    .map(f64::cbrt);
    mat_vec(
        [
            [0.2104542553, 0.793617785, -0.0040720468],
            [1.9779984951, -2.428592205, 0.4505937099],
            [0.0259040371, 0.7827717662, -0.808675766],
        ],
        lms,
    )
}

pub(super) fn from_oklab(lab: [f64; 3]) -> [f64; 3] {
    let lms = mat_vec(
        [
            [
                0.999_999_998_450_519_6,
                0.396_337_792_173_767_8,
                0.215_803_758_060_758_77,
            ],
            [
                1.000_000_008_881_760_7,
                -0.105_561_342_323_656_33,
                -0.063_854_174_771_705_9,
            ],
            [
                1.000_000_054_672_410_8,
                -0.089_484_182_094_965_77,
                -1.291_485_537_864_091_7,
            ],
        ],
        lab,
    )
    .map(|v| v.powi(3));
    mat_vec(
        [
            [
                1.227_013_851_103_521_1,
                -0.557_799_980_651_822_2,
                0.281_256_148_966_467_8,
            ],
            [
                -0.040_580_178_423_280_6,
                1.112_256_869_616_83,
                -0.071_676_678_665_601_19,
            ],
            [
                -0.076_381_284_505_706_9,
                -0.421_481_978_418_012_6,
                1.586_163_220_440_794_7,
            ],
        ],
        lms,
    )
}

fn noise(cube: &Cube, frame: Frame) -> Option<Noise> {
    const L: f64 = super::noise_gradient::MIDGRAY_OKLAB_L;
    const SIGMA: f64 = 0.005;
    const EPS: f64 = 0.005;
    let mut sigma1 = Vec::new();
    let mut anisotropy = Vec::new();
    let mut sigma_l = Vec::new();
    let mut sigma_ab = Vec::new();
    let mut worst_rotation = 0.0;
    let mut worst_hue = 0.0;
    let hue = |v: [f64; 3]| v[2].atan2(v[1]).to_degrees();
    let oklab_out = |code| to_oklab(to_xyz_qa(cube.trilinear(code), frame.output));
    let sample_labs =
        std::iter::once([L, 0.0, 0.0]).chain([0.07, 0.14, 0.21].into_iter().flat_map(|chroma| {
            (0..16).map(move |i| {
                let angle = f64::from(i) * std::f64::consts::TAU / 16.0;
                [L, chroma * angle.cos(), chroma * angle.sin()]
            })
        }));
    let matrix = from_xyz_matrix(frame.input);
    for lab in sample_labs {
        let linear = mat_vec(matrix, from_oklab(lab));
        if !linear.iter().all(|v| (0.0..=1.0).contains(v)) {
            continue;
        }
        let code = frame.input.encode(linear.map(|v| v / frame.gain));
        if !code.iter().all(|&v| v > 0.006 && v < 0.994) {
            continue;
        }
        let jacobian = std::array::from_fn(|axis| {
            let mut plus = code;
            let mut minus = code;
            plus[axis] += EPS;
            minus[axis] -= EPS;
            subtract(oklab_out(plus), oklab_out(minus)).map(|v| v / (2.0 * EPS))
        });
        let [min, _, max] = singular_values(jacobian);
        sigma1.push(max);
        anisotropy.push(max / min.max(1e-9));
        sigma_l.push(SIGMA * jacobian.iter().map(|j| j[0].powi(2)).sum::<f64>().sqrt());
        sigma_ab.push(
            SIGMA
                * jacobian
                    .iter()
                    .map(|j| j[1].powi(2) + j[2].powi(2))
                    .sum::<f64>()
                    .sqrt(),
        );
        let in_lab = to_oklab(to_xyz_qa(code, frame.input));
        let angle = hue(in_lab);
        let rotation = ((hue(oklab_out(code)) - angle + 180.0).rem_euclid(360.0) - 180.0).abs();
        if in_lab[1].hypot(in_lab[2]) > 1e-3 && rotation > worst_rotation {
            worst_rotation = rotation;
            worst_hue = angle;
        }
    }
    (!sigma1.is_empty()).then(|| Noise {
        input_samples: sigma1.len(),
        sigma_in_encoded: SIGMA,
        l_slice: L,
        sigma1: Summary::new(sigma1),
        anisotropy: Summary::new(anisotropy),
        sigma_lightness: Summary::new(sigma_l),
        sigma_chroma: Summary::new(sigma_ab),
        max_hue_rotation_deg: worst_rotation,
        worst_hue_deg: worst_hue,
    })
}
