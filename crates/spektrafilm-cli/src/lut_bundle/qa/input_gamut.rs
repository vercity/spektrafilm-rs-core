use anyhow::{Result, anyhow};
use serde::Serialize;
use spektrafilm_core::{
    input_gamut::{InputGamutCompress, spectral_locus_xy},
    pipeline::LutPipeline,
};
use spektrafilm_math::colorspace::{
    chromatic_adaptation_matrix_cat16_f64, mat3_mul3_f64, xy_to_xyz_f64,
};

use super::{Frame, Summary, mat_vec, spectral_envelope::inside_polygon, to_xyz_matrix};

#[derive(Serialize)]
pub(super) struct Preview {
    #[serde(flatten)]
    spec: Settings,
    oog_fraction: f64,
    n_oog_samples: usize,
    n_oog_bright: usize,
    bright_oog_displacement: Option<Summary>,
    #[serde(skip)]
    pub(super) samples: Vec<PreviewSample>,
}

#[derive(Clone, Copy)]
pub(super) struct PreviewSample {
    pub(super) original: [f64; 2],
    pub(super) compressed: [f64; 2],
    pub(super) in_locus: bool,
}

#[derive(Serialize)]
pub(super) struct Smoothness {
    #[serde(flatten)]
    spec: Settings,
    probe_radius: f64,
    probe_samples: usize,
    worst_step: f64,
    median_step: f64,
    worst_over_median_step: f64,
    #[serde(skip)]
    pub(super) original: Vec<[f64; 2]>,
    #[serde(skip)]
    pub(super) compressed: Vec<[f64; 2]>,
}

#[derive(Clone, Serialize)]
struct Settings {
    active: bool,
    algorithm: String,
    knee_threshold: f64,
    knee_limit: f64,
    knee_power: f64,
    reference_illuminant: String,
}

#[derive(Serialize)]
#[serde(untagged)]
pub(super) enum Outcome {
    Reports(Box<Reports>),
    Error { error: String },
}

#[derive(Serialize)]
pub(super) struct Reports {
    #[serde(rename = "input_gamut_compression_preview")]
    pub(super) preview: Preview,
    #[serde(rename = "input_gamut_compression_smoothness")]
    pub(super) smoothness: Smoothness,
    #[serde(skip)]
    pub(super) white: [f64; 2],
    #[serde(skip)]
    pub(super) primaries: [[f64; 2]; 3],
}

pub(super) fn evaluate(size: usize, frame: Frame, pipeline: &LutPipeline) -> Result<Reports> {
    let (params, illuminant) = pipeline.input_gamut_qa();
    let white = match illuminant {
        "D50" => [0.3457, 0.3585],
        "" | "D55" => [0.33243, 0.34744],
        "D65" => [0.3127, 0.3290],
        "A" => [0.44758, 0.40745],
        "E" => [1.0 / 3.0; 2],
        _ => {
            return Err(anyhow!(
                "No CIE xy reference for film illuminant {illuminant}"
            ));
        }
    };
    let algorithm = if params.active {
        params.algorithm.as_str()
    } else {
        "off"
    };
    let compressor =
        InputGamutCompress::build(algorithm, params.knee).map_err(anyhow::Error::msg)?;
    let spec = Settings {
        active: params.active,
        algorithm: params.algorithm.clone(),
        knee_threshold: f64::from(params.knee[0]),
        knee_limit: f64::from(params.knee[1]),
        knee_power: f64::from(params.knee[2]),
        reference_illuminant: if illuminant.is_empty() {
            "D55"
        } else {
            illuminant
        }
        .to_owned(),
    };
    let input = frame.input.space();
    let adaptation =
        chromatic_adaptation_matrix_cat16_f64(input.white_xyz(), xy_to_xyz_f64(white[0], white[1]));
    let matrix = mat3_mul3_f64(&adaptation, &to_xyz_matrix(frame.input));
    let locus = spectral_locus_xy();
    let step = 1.0 / (size - 1) as f64;
    let levels: Vec<_> = (0..size)
        .map(|i| if i == size - 1 { 1.0 } else { i as f64 * step })
        .collect();
    let mut validity = 0;
    let mut oog = 0;
    let mut bright_oog = Vec::new();
    let mut samples = Vec::new();
    // Sample the plotted dots and displacement vectors for readability.
    // The reported statistics still use the full cube.
    let stride = (size.pow(3) / 2500).max(1);
    let mut index = 0;
    for &b in &levels {
        for &g in &levels {
            for &r in &levels {
                let plot = index % stride == 0;
                index += 1;
                let [x, y, z] = mat_vec(matrix, frame.input.decode([r, g, b]));
                let brightness = x + y + z;
                if brightness <= 1e-4 {
                    continue;
                }
                validity += 1;
                let original = [x / brightness, y / brightness];
                let in_locus = inside_polygon(original, locus);
                oog += usize::from(!in_locus);
                let bright = brightness > 1e-2;
                if !in_locus && bright || plot {
                    let compressed = compressor.compress_xy(original, white);
                    if !in_locus && bright {
                        bright_oog.push(distance(original, compressed));
                    }
                    if plot {
                        samples.push(PreviewSample {
                            original,
                            compressed: if bright { compressed } else { original },
                            in_locus,
                        });
                    }
                }
            }
        }
    }
    let preview = Preview {
        spec: spec.clone(),
        oog_fraction: oog as f64 / validity.max(1) as f64,
        n_oog_samples: oog,
        n_oog_bright: bright_oog.len(),
        bright_oog_displacement: (params.active && !bright_oog.is_empty())
            .then(|| Summary::new(bright_oog)),
        samples,
    };

    let radius = 0.30;
    let probe_samples = 720;
    let original: Vec<_> = (0..probe_samples)
        .map(|i| {
            let theta = i as f64 * (std::f64::consts::TAU / probe_samples as f64);
            [
                white[0] + theta.cos() * radius,
                white[1] + theta.sin() * radius,
            ]
        })
        .collect();
    let compressed: Vec<_> = original
        .iter()
        .map(|&xy| compressor.compress_xy(xy, white))
        .collect();
    let differences = Summary::new(
        compressed
            .windows(2)
            .map(|p| distance(p[0], p[1]))
            .collect(),
    );
    let smoothness = Smoothness {
        spec,
        probe_radius: radius,
        probe_samples,
        worst_step: differences.max,
        median_step: differences.p50,
        worst_over_median_step: differences.max / differences.p50.max(1e-9),
        original,
        compressed,
    };
    Ok(Reports {
        preview,
        smoothness,
        white,
        primaries: input.primaries(),
    })
}

fn distance(a: [f64; 2], b: [f64; 2]) -> f64 {
    (a[0] - b[0]).hypot(a[1] - b[1])
}
