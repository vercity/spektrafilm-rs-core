use serde::Serialize;

use super::{
    Cube, Encoding, Frame,
    diagnostics::{from_oklab, singular_values, to_oklab},
    from_xyz_matrix, mat_vec,
    noise_gradient::{MIDGRAY_OKLAB_L, in_gamut, max_chroma},
    to_xyz_qa,
};

pub(super) const SIGMA: f64 = 0.005;
pub(super) const PAD: f64 = 0.006;
pub(super) const HEATMAP_SIDE: usize = 96;

pub(super) struct Sample {
    pub(super) input_ab: [f64; 2],
    pub(super) output_ab: [f64; 2],
    pub(super) output_rgb: [f64; 3],
    pub(super) hue: Option<f64>,
    pub(super) sigma1: f64,
    pub(super) sigma_l: f64,
    pub(super) sigma_ab: f64,
    pub(super) ellipse_radii: [f64; 2],
    pub(super) ellipse_angle_deg: f64,
}

impl Sample {
    pub(super) fn new(
        in_lab: [f64; 3],
        out_lab: [f64; 3],
        rgb: [f64; 3],
        hue: Option<f64>,
        sigma1: f64,
        jacobian: &[[f64; 3]; 3],
    ) -> Self {
        let cov = |x, y| SIGMA * SIGMA * jacobian.iter().map(|c| c[x] * c[y]).sum::<f64>();
        let aa = cov(1, 1);
        let bb = cov(2, 2);
        let ab = cov(1, 2);
        let mean = (aa + bb) / 2.0;
        let offset = ((aa - bb) / 2.0).hypot(ab);
        Self {
            input_ab: [in_lab[1], in_lab[2]],
            output_ab: [out_lab[1], out_lab[2]],
            output_rgb: rgb,
            hue,
            sigma1,
            sigma_l: cov(0, 0).max(0.0).sqrt(),
            sigma_ab: (aa + bb).max(0.0).sqrt(),
            // 2σ contours, drawn at 1.5× the actual radius.
            ellipse_radii: [
                3.0 * (mean + offset).max(0.0).sqrt(),
                3.0 * (mean - offset).max(0.0).sqrt(),
            ],
            ellipse_angle_deg: (2.0 * ab).atan2(aa - bb).to_degrees() / 2.0,
        }
    }
}

#[derive(Serialize)]
pub(super) struct Field {
    heatmap_side: usize,
    heatmap_valid_cells: usize,
    heatmap_max_sigma1: f64,
    rosette_samples: usize,
    ellipse_samples: usize,
    #[serde(skip)]
    pub(super) domain_extent: f64,
    #[serde(skip)]
    pub(super) panel_extent: f64,
    #[serde(skip)]
    pub(super) input_outline: Vec<[f64; 2]>,
    #[serde(skip)]
    pub(super) output_outline: Vec<[f64; 2]>,
    #[serde(skip)]
    pub(super) gain: Vec<Option<f64>>,
    #[serde(skip)]
    pub(super) color_limit: f64,
    #[serde(skip)]
    pub(super) samples: Vec<Sample>,
}

impl Field {
    pub(super) fn evaluate(cube: &Cube, frame: Frame, samples: Vec<Sample>) -> Self {
        let input_outline = outline(frame.input);
        let output_outline = outline(frame.output);
        let domain_extent =
            (max_extent(input_outline.iter().chain(&output_outline)) * 1.15).max(0.30);
        let panel_extent = domain_extent
            .max(1.15 * max_extent(samples.iter().flat_map(|s| [&s.input_ab, &s.output_ab])));
        let mut gain = Vec::with_capacity(HEATMAP_SIDE * HEATMAP_SIDE);
        let matrix = from_xyz_matrix(frame.input);
        let step = domain_extent * 2.0 / (HEATMAP_SIDE - 1) as f64;
        let origin = -domain_extent;
        let mut valid_cells = 0;
        let mut max_sigma1 = 0.0_f64;
        for row in 0..HEATMAP_SIDE {
            for col in 0..HEATMAP_SIDE {
                let a = origin + col as f64 * step;
                let b = origin + row as f64 * step;
                let linear = mat_vec(matrix, from_oklab([MIDGRAY_OKLAB_L, a, b]));
                let sample = in_gamut(linear).then(|| {
                    let code = frame
                        .input
                        .encode(linear.map(|value| value / frame.gain))
                        .map(|code| code.clamp(PAD, 1.0 - PAD));
                    let sigma1 = singular_values(local_jacobian(cube, code, frame.output))[2];
                    max_sigma1 = max_sigma1.max(sigma1);
                    valid_cells += 1;
                    sigma1
                });
                gain.push(sample);
            }
        }
        let mut sample_gains: Vec<_> = samples.iter().map(|s| s.sigma1).collect();
        sample_gains.sort_unstable_by(f64::total_cmp);
        let p = 0.98 * (sample_gains.len() - 1) as f64;
        let lo = sample_gains[p.floor() as usize];
        let hi = sample_gains[p.ceil() as usize];
        Self {
            heatmap_side: HEATMAP_SIDE,
            heatmap_valid_cells: valid_cells,
            heatmap_max_sigma1: max_sigma1,
            rosette_samples: samples.iter().filter(|s| s.hue.is_some()).count(),
            ellipse_samples: samples.len(),
            domain_extent,
            panel_extent,
            input_outline,
            output_outline,
            gain,
            color_limit: (lo + p.fract() * (hi - lo)).max(1e-6),
            samples,
        }
    }
}

pub(super) fn local_jacobian(cube: &Cube, code: [f64; 3], output: Encoding) -> [[f64; 3]; 3] {
    const EPS: f64 = 0.005;
    let lab = |code| to_oklab(to_xyz_qa(cube.trilinear(code), output));
    std::array::from_fn(|axis| {
        let mut hi = code;
        let mut lo = code;
        hi[axis] += EPS;
        lo[axis] -= EPS;
        let hi = lab(hi);
        let lo = lab(lo);
        std::array::from_fn(|k| (hi[k] - lo[k]) / (2.0 * EPS))
    })
}

fn outline(encoding: Encoding) -> Vec<[f64; 2]> {
    let matrix = from_xyz_matrix(encoding);
    (0..181)
        .map(|i| {
            let h = f64::from(i) * (std::f64::consts::TAU / 181.0);
            let direction = [h.cos(), h.sin()];
            let chroma = max_chroma(matrix, MIDGRAY_OKLAB_L, direction);
            direction.map(|v| v * chroma)
        })
        .collect()
}

fn max_extent<'a>(pts: impl Iterator<Item = &'a [f64; 2]>) -> f64 {
    pts.flat_map(|v| v.map(f64::abs)).fold(0.0, f64::max)
}
