use anyhow::{Result, ensure};
use image::{ExtendedColorType, ImageEncoder, codecs::png::PngEncoder};
use serde::Serialize;
use spektrafilm_core::pipeline::LutPipeline;
use spektrafilm_math::{
    colorspace::{chromatic_adaptation_matrix_cat16_f64, mat3_mul3_f64},
    image::ImageBuf,
    precision::{from_f32, to_f64},
};

use super::{Encoding, Frame, from_xyz_matrix, mat_vec, to_xyz_matrix};

pub(super) const WIDTH: u32 = 768;
pub(super) const HEIGHT: u32 = 256;

#[derive(Serialize)]
#[serde(untagged)]
pub(super) enum Outcome {
    Panels(Vec<Panel>),
    Error { error: String },
}

#[derive(Serialize)]
pub(super) struct Panel {
    pub(super) target: &'static str,
    pub(super) oog_fraction_total: f64,
    pub(super) oog_fraction_saturated_row: f64,
    #[serde(skip)]
    pub(super) image_png: Vec<u8>,
}

struct Target {
    name: &'static str,
    to_xyz: [[f64; 3]; 3],
    white: [f64; 3],
    gamma: f64,
}

pub(super) fn evaluate(frame: Frame, pipeline: &LutPipeline) -> Result<Vec<Panel>> {
    let display_matrix = display_matrix(frame.output);
    let target = |encoding: Encoding| Target {
        name: encoding.name(),
        to_xyz: to_xyz_matrix(encoding),
        white: encoding.space().white_xyz(),
        gamma: 2.4,
    };
    let targets = [
        target(Encoding::Rec709),
        Target {
            gamma: 2.6,
            ..target(Encoding::DciP3)
        },
        target(Encoding::Rec2020),
    ];
    targets
        .into_iter()
        .map(|target| panel(target, frame.input, pipeline, display_matrix))
        .collect()
}

pub(super) fn display_matrix(output: Encoding) -> [[f64; 3]; 3] {
    let srgb = Encoding::Srgb;
    rgb_matrix(
        to_xyz_matrix(output),
        output.space().white_xyz(),
        from_xyz_matrix(srgb),
        srgb.space().white_xyz(),
    )
}

pub(super) fn encode_display(matrix: [[f64; 3]; 3], linear: [f64; 3]) -> [u8; 3] {
    Encoding::Srgb
        .encode(mat_vec(matrix, linear).map(|value| value.clamp(0.0, 1.0)))
        .map(|code| (code * 255.0).round() as u8)
}

pub(super) fn encode_png(pixels: &[u8], width: u32, height: u32) -> Result<Vec<u8>> {
    let mut png = Vec::new();
    PngEncoder::new(&mut png).write_image(pixels, width, height, ExtendedColorType::Rgb8)?;
    Ok(png)
}

fn rgb_matrix(
    to_xyz: [[f64; 3]; 3],
    src_white: [f64; 3],
    from_xyz: [[f64; 3]; 3],
    dst_white: [f64; 3],
) -> [[f64; 3]; 3] {
    if src_white == dst_white {
        return mat3_mul3_f64(&from_xyz, &to_xyz);
    }
    let cat = chromatic_adaptation_matrix_cat16_f64(src_white, dst_white);
    mat3_mul3_f64(&mat3_mul3_f64(&from_xyz, &cat), &to_xyz)
}

fn panel(
    target: Target,
    encoding: Encoding,
    pipeline: &LutPipeline,
    display_matrix: [[f64; 3]; 3],
) -> Result<Panel> {
    let input_matrix = rgb_matrix(
        target.to_xyz,
        target.white,
        from_xyz_matrix(encoding),
        encoding.space().white_xyz(),
    );
    let saturated: Vec<_> = (0..WIDTH)
        .map(|x| {
            let t = 6.0 * f64::from(x) / f64::from(WIDTH);
            let f = t.fract();
            match t as u8 {
                0 => [1.0, f, 0.0],
                1 => [1.0 - f, 1.0, 0.0],
                2 => [0.0, 1.0, f],
                3 => [0.0, 1.0 - f, 1.0],
                4 => [f, 0.0, 1.0],
                _ => [1.0, 0.0, 1.0 - f],
            }
        })
        .collect();
    let mid = HEIGHT / 2;
    let half_band = HEIGHT / 16;
    let band = mid - half_band..mid + half_band;
    let mut input = Vec::with_capacity((WIDTH * HEIGHT * 3) as usize);
    let mut oog_total = 0;
    let mut oog_band = 0;
    for y in 0..HEIGHT {
        let v = f64::from(y) / f64::from(HEIGHT - 1);
        let white = (1.0 - 2.0 * v).max(0.0);
        let weight = 1.0 - (2.0 * v - 1.0).abs();
        for hue in &saturated {
            let target_rgb = hue.map(|c| (white + weight * c).powf(target.gamma));
            let rgb = mat_vec(input_matrix, target_rgb);
            let oog = rgb.iter().any(|c| !(0.0..=1.0).contains(c));
            oog_total += u32::from(oog);
            oog_band += u32::from(oog && band.contains(&y));
            // Match the float32 linear input of the Python chart. The chart
            // bypasses headroom and the CUBE, so it can probe out-of-gamut inputs.
            input.extend(rgb.map(|c| from_f32(c as f32)));
        }
    }
    let output = pipeline.process(&ImageBuf::from_data(WIDTH * HEIGHT, 1, input))?;
    let mut pixels = Vec::with_capacity((WIDTH * HEIGHT * 3) as usize);
    for pixel in output.pixels() {
        let linear = [to_f64(pixel[0]), to_f64(pixel[1]), to_f64(pixel[2])];
        ensure!(
            linear.iter().all(|c| c.is_finite()),
            "non-finite stress chart for {}",
            target.name
        );
        pixels.extend(encode_display(display_matrix, linear));
    }
    let image_png = encode_png(&pixels, WIDTH, HEIGHT)?;
    Ok(Panel {
        target: target.name,
        oog_fraction_total: f64::from(oog_total) / f64::from(WIDTH * HEIGHT),
        oog_fraction_saturated_row: f64::from(oog_band) / f64::from(WIDTH * (2 * half_band)),
        image_png,
    })
}
