use anyhow::Result;
use serde::Serialize;
use spektrafilm_core::pipeline::LutPipeline;
use spektrafilm_math::{
    image::ImageBuf,
    precision::{from_f32, to_f64},
};

use super::{
    Cube, Frame, curves::chromaticity, diagnostics::to_oklab, hull, mat_vec, rng::Pcg64,
    to_xyz_matrix, to_xyz_qa,
};

pub(super) const SEGMENT_SAMPLES: usize = 96;

#[derive(Serialize)]
pub(super) struct Compression {
    fold_triangles: usize,
    fold_fraction: f64,
    input_hull_volume: Option<f64>,
    output_hull_volume: Option<f64>,
    compression_ratio: Option<f64>,
    passed: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    #[serde(flatten)]
    pub(super) rim: Option<Rim>,
    #[serde(skip)]
    pub(super) output_oklab: Vec<[f64; 3]>,
}

#[derive(Serialize)]
pub(super) struct Rim {
    compression_algorithm: String,
    rim_oog_fraction: f64,
    rim_oog_samples: usize,
    rim_max_displacement: f64,
    rim_mean_displacement: f64,
    #[serde(skip)]
    pub(super) colors: Vec<[f64; 3]>,
    #[serde(skip)]
    pub(super) unbounded_oklab: Vec<[f64; 3]>,
    #[serde(skip)]
    pub(super) compressed_oklab: Vec<[f64; 3]>,
    #[serde(skip)]
    pub(super) unbounded_xy: Vec<[f64; 2]>,
    #[serde(skip)]
    pub(super) compressed_xy: Vec<[f64; 2]>,
}

impl Compression {
    pub(super) fn evaluate(
        cube: &Cube,
        frame: Frame,
        fold_triangles: usize,
        fold_fraction: f64,
    ) -> Self {
        let n = cube.size;
        let population = cube.data.len();
        let indices = Pcg64::for_seed_zero().choose(population, population.min(8000));
        let last = n - 1;
        let step = 1.0 / last as f64;
        let code = |i| if i == last { 1.0 } else { i as f64 * step };
        // Compare both sets in the *output* color space, as the Python
        // hull statistic does. This isolates LUT compression from the
        // selection of encoding on the input side.
        let input_oklab: Vec<_> = indices
            .iter()
            .map(|&i| {
                let input = [code(i % n), code(i / n % n), code(i / n / n)];
                to_oklab(to_xyz_qa(input, frame.output))
            })
            .collect();
        let output_oklab: Vec<_> = indices
            .iter()
            .map(|&i| to_oklab(to_xyz_qa(cube.data[i], frame.output)))
            .collect();
        let input_hull_volume = hull::volume(&input_oklab);
        let output_hull_volume = hull::volume(&output_oklab);
        let compression_ratio = input_hull_volume
            .zip(output_hull_volume)
            .map(|(input, output)| output / input.max(1e-12));
        let passed =
            compression_ratio.map(|ratio| fold_triangles == 0 && (0.05..=1.05).contains(&ratio));
        Self {
            fold_triangles,
            fold_fraction,
            input_hull_volume,
            output_hull_volume,
            compression_ratio,
            passed,
            error: compression_ratio
                .is_none()
                .then(|| "A three-dimensional gamut hull could not be computed".into()),
            rim: None,
            output_oklab,
        }
    }

    pub(super) fn evaluate_rim(&mut self, frame: Frame, pipeline: &LutPipeline) {
        match Rim::evaluate(frame, pipeline) {
            Ok(rim) => self.rim = Some(rim),
            Err(error) => {
                let details = format!("Output gamut rim: {error:#}");
                if let Some(message) = &mut self.error {
                    message.push_str(". ");
                    message.push_str(&details);
                } else {
                    self.error = Some(details);
                }
            }
        }
    }

    pub(super) fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }
}

impl Rim {
    fn evaluate(frame: Frame, pipeline: &LutPipeline) -> Result<Self> {
        let mut data = Vec::with_capacity(12 * SEGMENT_SAMPLES * 3);
        let mut colors = Vec::with_capacity(12 * SEGMENT_SAMPLES);
        for axis in 0..3 {
            let free_axes: Vec<_> = (0..3).filter(|&c| c != axis).collect();
            for a in [0.0, 1.0] {
                for b in [0.0, 1.0] {
                    for i in 0..SEGMENT_SAMPLES {
                        let mut rgb = [0.0; 3];
                        rgb[axis] = i as f64 / (SEGMENT_SAMPLES - 1) as f64;
                        rgb[free_axes[0]] = a;
                        rgb[free_axes[1]] = b;
                        colors.push(rgb);
                        // Match Python's rim. This native-code stress probe
                        // bypasses exposure scaling and the delivered LUT.
                        data.extend(frame.input.decode(rgb).map(|value| from_f32(value as f32)));
                    }
                }
            }
        }
        let image = ImageBuf::from_data((data.len() / 3) as u32, 1, data);
        let (reference, compressor, algorithm) = pipeline.output_gamut_qa(&image)?;
        let mut bright = 0;
        let mut oog = 0;
        let mut displacement_sum = 0.0;
        let mut displacement_max = 0.0_f64;
        let len = reference.pixel_count();
        let mut unbounded_oklab = Vec::with_capacity(len);
        let mut compressed_oklab = Vec::with_capacity(len);
        let mut unbounded_xy = Vec::with_capacity(len);
        let mut compressed_xy = Vec::with_capacity(len);
        let matrix = to_xyz_matrix(frame.output);
        for px in reference.pixels() {
            let unbounded = [to_f64(px[0]), to_f64(px[1]), to_f64(px[2])];
            let compressed = compressor.compress(unbounded);
            let ub_xyz = mat_vec(matrix, unbounded);
            let compressed_xyz = mat_vec(matrix, compressed);
            unbounded_oklab.push(to_oklab(ub_xyz));
            compressed_oklab.push(to_oklab(compressed_xyz));
            unbounded_xy.push(chromaticity(ub_xyz));
            compressed_xy.push(chromaticity(compressed_xyz));
            let ach = unbounded.into_iter().fold(0.0_f64, f64::max);
            if ach > 0.01 {
                bright += 1;
                if unbounded.iter().any(|&c| (ach - c) / ach > 1.0) {
                    oog += 1;
                    let displacement = unbounded
                        .into_iter()
                        .zip(compressed)
                        .map(|(a, b)| (a - b).powi(2))
                        .sum::<f64>()
                        .sqrt();
                    displacement_sum += displacement;
                    displacement_max = displacement_max.max(displacement);
                }
            }
        }
        Ok(Self {
            compression_algorithm: algorithm.to_owned(),
            rim_oog_fraction: oog as f64 / bright.max(1) as f64,
            rim_oog_samples: oog,
            rim_max_displacement: displacement_max,
            rim_mean_displacement: displacement_sum / oog.max(1) as f64,
            colors,
            unbounded_oklab,
            compressed_oklab,
            unbounded_xy,
            compressed_xy,
        })
    }
}
