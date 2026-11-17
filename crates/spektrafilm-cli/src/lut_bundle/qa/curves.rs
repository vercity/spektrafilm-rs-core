use serde::Serialize;

use super::{Cube, Frame, from_xyz_matrix, mat_vec, to_xyz_qa};

const SWEEP_SAMPLES: usize = 65;
const WHITE_SAMPLES: usize = 16;

#[derive(Serialize)]
pub(super) struct Characteristic {
    system_gamma_at_mid: Option<f64>,
    max_channel_density_spread: f64,
    #[serde(skip)]
    pub(super) neutral: [[f64; 3]; SWEEP_SAMPLES],
    #[serde(skip)]
    pub(super) sweeps: Vec<DensitySweep>,
}

pub(super) struct DensitySweep {
    pub(super) pin_density: f64,
    pub(super) channels: [[[f64; 3]; SWEEP_SAMPLES]; 3],
}

pub(super) fn characteristic(cube: &Cube) -> Characteristic {
    let density = |code: f64| -code.clamp(1e-4, 1.0).log10();
    let mean = |rgb: [f64; 3]| (rgb[0] + rgb[1] + rgb[2]) / 3.0;

    let probe: Vec<_> = (0..=256)
        .rev()
        .map(|i| {
            let code = f64::from(i) / 256.0;
            (density(mean(cube.trilinear([code; 3]))), code)
        })
        .collect();
    let sweeps = [0.2, 0.4, 0.6, 0.8, 1.0]
        .into_iter()
        .map(|pin_density| {
            // The neutral output is approximately monotone. Invert
            // that curve to pin each sweep at the requested density.
            let upper = probe.partition_point(|&(d, _)| d <= pin_density);
            let code = match upper {
                0 => probe[0].1,
                i if i == probe.len() => probe[i - 1].1,
                i => {
                    let (low, a) = probe[i - 1];
                    let (high, b) = probe[i];
                    a + (b - a) * (pin_density - low) / (high - low)
                }
            };
            let pin = f64::from(code as f32);
            let channels = std::array::from_fn(|axis| {
                std::array::from_fn(|i| {
                    let mut sample = [pin; 3];
                    sample[axis] = i as f64 / (SWEEP_SAMPLES - 1) as f64;
                    cube.trilinear(sample).map(density)
                })
            });
            DensitySweep {
                pin_density,
                channels,
            }
        })
        .collect();

    let middle = cube.size / 2;
    let gamma = (cube.size >= 3).then(|| {
        let input = |i| (1e-6 + i as f64 * ((1.0 - 1e-6) / (cube.size - 1) as f64)).log10();
        let output = |i| -density(mean(cube.get([i; 3])));
        (output(middle + 1) - output(middle - 1)) / (input(middle + 1) - input(middle - 1))
    });
    let spread = (0..cube.size)
        .map(|i| {
            let values = cube.get([i; 3]).map(density);
            let min = values.into_iter().fold(f64::INFINITY, f64::min);
            let max = values.into_iter().fold(0.0_f64, f64::max);
            max - min
        })
        .fold(0.0_f64, f64::max);
    Characteristic {
        system_gamma_at_mid: gamma,
        max_channel_density_spread: spread,
        neutral: std::array::from_fn(|i| {
            cube.trilinear([i as f64 / (SWEEP_SAMPLES - 1) as f64; 3])
                .map(density)
        }),
        sweeps,
    }
}

#[derive(Serialize)]
pub(super) struct Planckian {
    max_bend_angle_deg: f64,
    cct_range_k: &'static str,
    passed: bool,
    #[serde(skip)]
    pub(super) output_xy: [[f64; 2]; WHITE_SAMPLES],
}

pub(super) fn planckian(cube: &Cube, frame: Frame) -> Planckian {
    let from_xyz = from_xyz_matrix(frame.input);
    let output_xy: [[f64; 2]; WHITE_SAMPLES] = std::array::from_fn(|i| {
        let cct = 2700.0 + i as f64 * ((10_000.0 - 2700.0) / (WHITE_SAMPLES - 1) as f64);
        let [x, y] = white_xy(cct);
        let rgb = mat_vec(from_xyz, [x / y, 1.0, (1.0 - x - y) / y]);
        let peak = rgb.into_iter().fold(1e-6_f64, f64::max);
        let code = rgb
            .map(|v| f64::from(frame.input.encode((v / peak).clamp(0.0, 1.0) / frame.gain) as f32));
        chromaticity(to_xyz_qa(cube.trilinear(code), frame.output))
    });
    let max_bend_angle_deg = output_xy
        .windows(3)
        .map(|points| {
            let [a, b, c] = [points[0], points[1], points[2]];
            let [u, v] = [b[0] - a[0], b[1] - a[1]];
            let [w, z] = [c[0] - b[0], c[1] - b[1]];
            let denom = (u.hypot(v) + 1e-12) * (w.hypot(z) + 1e-12);
            ((u * w + v * z) / denom)
                .clamp(-1.0, 1.0)
                .acos()
                .to_degrees()
        })
        .fold(0.0_f64, f64::max);
    Planckian {
        max_bend_angle_deg,
        cct_range_k: "2700-10000",
        passed: max_bend_angle_deg <= 30.0,
        output_xy,
    }
}

// Kang 2002 for 2700–3999 K and CIE Illuminant D above, matching
// the upstream daylight sweep's two chromaticity models.
fn white_xy(temperature: f64) -> [f64; 2] {
    let t2 = temperature.powi(2);
    let t3 = temperature.powi(3);
    if temperature < 4000.0 {
        let x = -0.2661239e9 / t3 - 0.2343589e6 / t2 + 0.8776956e3 / temperature + 0.179910;
        [
            x,
            -0.9549476 * x.powi(3) - 1.37418593 * x.powi(2) + 2.09137015 * x - 0.16748867,
        ]
    } else {
        let x = if temperature <= 7000.0 {
            -4.607e9 / t3 + 2.9678e6 / t2 + 0.09911e3 / temperature + 0.244063
        } else {
            -2.0064e9 / t3 + 1.9018e6 / t2 + 0.24748e3 / temperature + 0.23704
        };
        [x, -3.0 * x.powi(2) + 2.87 * x - 0.275]
    }
}

pub(super) fn chromaticity([x, y, z]: [f64; 3]) -> [f64; 2] {
    let sum = x + y + z;
    if sum == 0.0 {
        [0.0; 2]
    } else {
        [x / sum, y / sum]
    }
}
