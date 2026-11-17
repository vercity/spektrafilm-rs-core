use std::collections::BTreeMap;

use serde::Serialize;

use super::{Cube, Frame, diagnostics::to_oklab, to_xyz_qa};

#[derive(Serialize)]
pub(super) struct DynamicRange {
    encoded_range_stops: f64,
    active_range_stops: f64,
    toe_collapsed_stops: f64,
    shoulder_collapsed_stops: f64,
    midgray_output_y: f64,
    midgray_offset_stops: f64,
    #[serde(skip)]
    pub(super) ramp: Vec<RampSample>,
}

pub(super) struct RampSample {
    pub(super) stops: f64,
    pub(super) y: f64,
    pub(super) clipped: bool,
}

pub(super) fn dynamic_range(cube: &Cube, frame: Frame) -> DynamicRange {
    // For absolute-luminance inputs, the encoding's 18% reference
    // is in nits. Other encodings use a reflectance-scale gray.
    let gray = 0.18 * frame.input.output_gain();
    let code = |stops: f64| frame.input.encode([gray * stops.exp2(); 3])[0];
    let stop_lo = -8.0;
    let stop_hi = (0..=3200)
        .rev()
        .map(|i| -12.0 + f64::from(i) * 0.01)
        .find(|&stops| code(stops) <= 1.0)
        .unwrap_or(0.0)
        + 0.5;
    let step = (stop_hi - stop_lo) / 256.0;
    let ramp: Vec<_> = (0..=256)
        .map(|index| {
            let stops = stop_lo + f64::from(index) * step;
            let encoded = code(stops);
            let clamped = f64::from(encoded.clamp(0.0, 1.0) as f32);
            let y = to_xyz_qa(cube.trilinear([clamped; 3]), frame.output)[1].max(1e-6);
            RampSample {
                stops,
                y,
                clipped: !(0.0..=1.0).contains(&encoded),
            }
        })
        .collect();
    let center = ramp.partition_point(|sample| sample.stops < 0.0);
    let a = &ramp[center - 1];
    let b = &ramp[center];
    let midgray = a.y + (b.y - a.y) * (-a.stops / (b.stops - a.stops));
    let mut report = DynamicRange {
        encoded_range_stops: 0.0,
        active_range_stops: 0.0,
        toe_collapsed_stops: 0.0,
        shoulder_collapsed_stops: 0.0,
        midgray_output_y: midgray,
        midgray_offset_stops: (midgray / 0.18).log2(),
        ramp,
    };
    let ramp = &report.ramp;
    if let (Some(lo), Some(hi)) = (
        ramp.iter().position(|sample| !sample.clipped),
        ramp.iter().rposition(|sample| !sample.clipped),
    ) {
        report.encoded_range_stops = ramp[hi].stops - ramp[lo].stops;
        let active: Vec<_> = ramp[lo..=hi]
            .windows(2)
            .map(|pair| {
                let lo = pair[0].y.max(1e-4).log10();
                let hi = pair[1].y.max(1e-4).log10();
                (lo - hi).abs() / step > 0.10
            })
            .collect();
        report.active_range_stops = active.iter().filter(|&&sample| sample).count() as f64 * step;
        report.toe_collapsed_stops =
            active.iter().take_while(|&&sample| !sample).count() as f64 * step;
        report.shoulder_collapsed_stops =
            active.iter().rev().take_while(|&&sample| !sample).count() as f64 * step;
    }
    report
}

#[derive(Serialize)]
pub(super) struct HueTwist {
    #[serde(flatten)]
    bands: BTreeMap<String, f64>,
    max_hue_rotation_deg: f64,
    samples_in_locus: usize,
    samples_filtered_out_of_locus: usize,
}

pub(super) fn hue_twist(cube: &Cube, frame: Frame) -> HueTwist {
    let mut samples = Vec::new();
    let last = cube.size - 1;
    let step = 1.0 / last as f64;
    let code = |i| if i == last { 1.0 } else { i as f64 * step };
    let hue = |lab: [f64; 3]| lab[2].atan2(lab[1]).to_degrees();
    for b in 0..=last {
        for g in 0..=last {
            for r in 0..=last {
                let input = to_oklab(to_xyz_qa([code(r), code(g), code(b)], frame.input));
                let chroma = input[1].hypot(input[2]);
                if chroma <= 0.6 {
                    let output = to_oklab(to_xyz_qa(cube.get([r, g, b]), frame.output));
                    let angle =
                        ((hue(output) - hue(input) + 180.0).rem_euclid(360.0) - 180.0).abs();
                    samples.push((chroma, angle));
                }
            }
        }
    }
    let c_max = samples.iter().map(|&(c, _)| c).fold(0.0_f64, f64::max);
    let mut bands = BTreeMap::new();
    for (lo, hi) in [(0.20, 0.35), (0.35, 0.50), (0.50, 0.70), (0.70, 1.001)] {
        let band = (lo * c_max)..(hi * c_max);
        if let Some(max) = samples
            .iter()
            .filter_map(|(c, d)| band.contains(c).then_some(*d))
            .reduce(f64::max)
        {
            bands.insert(format!("max_rotation_chroma_{lo:.2}_{hi:.2}"), max);
        }
    }
    HueTwist {
        max_hue_rotation_deg: bands.values().copied().fold(0.0_f64, f64::max),
        bands,
        samples_in_locus: samples.len(),
        samples_filtered_out_of_locus: cube.data.len() - samples.len(),
    }
}

#[derive(Serialize)]
pub(super) struct RgPlanes {
    pub(super) cube_resolution: usize,
    n_slices: usize,
    #[serde(skip)]
    pub(super) images: Vec<RgPlane>,
}

pub(super) struct RgPlane {
    pub(super) blue: f64,
    pub(super) png: Vec<u8>,
}

pub(super) fn rg_planes(cube: &Cube, frame: Frame) -> anyhow::Result<RgPlanes> {
    use super::gamut_edge::{display_matrix, encode_display, encode_png};

    let matrix = display_matrix(frame.output);
    let gain = frame.output.output_gain();
    let n = cube.size;
    let n_slices = n.min(9);
    let mut images = Vec::with_capacity(n_slices);
    for i in 0..n_slices {
        let b = i * (n - 1) / (n_slices - 1);
        let mut pixels = Vec::with_capacity(n * n * 3);
        for g in (0..n).rev() {
            for r in 0..n {
                let linear = frame
                    .output
                    .decode(cube.get([r, g, b]))
                    .map(|value| value / gain);
                pixels.extend(encode_display(matrix, linear));
            }
        }
        images.push(RgPlane {
            blue: b as f64 / (n - 1) as f64,
            png: encode_png(&pixels, n as u32, n as u32)?,
        });
    }
    Ok(RgPlanes {
        cube_resolution: n,
        n_slices,
        images,
    })
}
