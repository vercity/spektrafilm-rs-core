use anyhow::Result;
use serde::Serialize;

use super::{
    Cube, Encoding, Frame, Summary,
    diagnostics::from_oklab,
    from_xyz_matrix,
    gamut_edge::{encode_display, encode_png},
    mat_vec,
    rng::{Normal, Pcg64},
    to_xyz_qa,
};

pub(super) const MIDGRAY_OKLAB_L: f64 = 0.564_622_597_143_569_8;
pub(super) const SIDE: u32 = 512;

#[derive(Serialize)]
pub(super) struct Gradient {
    image_side: u32,
    sigma_in_encoded: f64,
    #[serde(rename = "mid_L")]
    mid_l: f64,
    rim_scale: f64,
    input_in_gamut_fraction: f64,
    mean_abs_output_delta: f64,
    p99_abs_output_delta: f64,
    #[serde(skip)]
    pub(super) clean: Vec<u8>,
    #[serde(skip)]
    pub(super) noisy: Vec<u8>,
}

pub(super) fn evaluate(cube: &Cube, frame: Frame) -> Result<Gradient> {
    const RIM: f64 = 0.96;
    const SIGMA: f64 = 0.02;
    let input_matrix = from_xyz_matrix(frame.input);
    let srgb = from_xyz_matrix(Encoding::Srgb);
    let mut rng = Pcg64::for_seed_zero();
    let normal = Normal::new();
    let directions: Vec<_> = (0..SIDE)
        .map(|x| {
            let t = f64::from(x) * (std::f64::consts::TAU / f64::from(SIDE));
            [t.cos(), t.sin()]
        })
        .collect();
    let mut in_gamut_count = 0;
    let mut differences = Vec::with_capacity((SIDE * SIDE * 3) as usize);
    let mut clean = vec![0_u8; (SIDE * SIDE * 3) as usize];
    let mut noisy = vec![0_u8; (SIDE * SIDE * 3) as usize];
    for y in 0..SIDE {
        // Keep the seed sequence in Python's bottom-up order and invert
        // the raster rows separately for display.
        let v = f64::from(y) * (1.0 / f64::from(SIDE - 1));
        let lightness = if v <= 0.5 {
            (v / 0.5) * MIDGRAY_OKLAB_L
        } else {
            MIDGRAY_OKLAB_L + ((v - 0.5) / 0.5) * (1.0 - MIDGRAY_OKLAB_L)
        };
        let weight = 1.0 - (2.0 * v - 1.0).abs();
        for (x, &[a, b]) in directions.iter().enumerate() {
            let hue = [a, b];
            let chroma = RIM * weight * max_chroma(input_matrix, lightness, hue);
            let linear = mat_vec(
                input_matrix,
                from_oklab([lightness, chroma * a, chroma * b]),
            );
            in_gamut_count += u32::from(in_gamut(linear));
            let original = frame
                .input
                .encode(linear.map(|value| value.clamp(0.0, 1.0) / frame.gain));
            let perturbed = original.map(|c| (c + SIGMA * normal.sample(&mut rng)).clamp(0.0, 1.0));
            let unperturbed_lut = cube.trilinear(original);
            let perturbed_lut = cube.trilinear(perturbed);
            differences.extend(
                unperturbed_lut
                    .into_iter()
                    .zip(perturbed_lut)
                    .map(|(a, b)| (a - b).abs()),
            );
            let i = ((SIDE - 1 - y) as usize * SIDE as usize + x) * 3;
            clean[i..i + 3].copy_from_slice(&encode_display(
                srgb,
                to_xyz_qa(unperturbed_lut, frame.output),
            ));
            noisy[i..i + 3].copy_from_slice(&encode_display(
                srgb,
                to_xyz_qa(perturbed_lut, frame.output),
            ));
        }
    }
    let summary = Summary::new(differences);
    Ok(Gradient {
        image_side: SIDE,
        sigma_in_encoded: SIGMA,
        mid_l: MIDGRAY_OKLAB_L,
        rim_scale: RIM,
        input_in_gamut_fraction: f64::from(in_gamut_count) / f64::from(SIDE * SIDE),
        mean_abs_output_delta: summary.mean,
        p99_abs_output_delta: summary.p99,
        clean: encode_png(&clean, SIDE, SIDE)?,
        noisy: encode_png(&noisy, SIDE, SIDE)?,
    })
}

/// Chroma up to the linear RGB gamut boundary in a constant-lightness slice.
pub(super) fn max_chroma(matrix: [[f64; 3]; 3], lightness: f64, [a, b]: [f64; 2]) -> f64 {
    let contains = |c| in_gamut(mat_vec(matrix, from_oklab([lightness, c * a, c * b])));
    let mut lower = 0.0;
    let mut upper = 0.45;
    for _ in 0..3 {
        if !contains(upper) {
            break;
        }
        upper *= 1.5;
    }
    for _ in 0..18 {
        let midpoint = 0.5 * (lower + upper);
        if contains(midpoint) {
            lower = midpoint;
        } else {
            upper = midpoint;
        }
    }
    lower
}

pub(super) fn in_gamut(linear: [f64; 3]) -> bool {
    linear.into_iter().all(|v| (0.0..=1.0).contains(&v))
}
