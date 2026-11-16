use std::{cmp::Reverse, sync::OnceLock};

use anyhow::{Context, Result, ensure};
use lensfun::{Camera, Database, Lens, LensType, Modifier};
use rawler::{decoders::RawDecodeParams, formats::tiff::Rational, rawsource::RawSource};
use rayon::prelude::*;
use spektrafilm_math::{
    image::ImageBuf,
    precision::{Scalar, from_f64, to_f64},
};

static DATABASE: OnceLock<Result<Database, lensfun::Error>> = OnceLock::new();

pub(crate) fn apply(source: &RawSource, image: &mut ImageBuf) -> Result<()> {
    let Ok(metadata) = rawler::get_decoder(source)
        .and_then(|decoder| decoder.raw_metadata(source, &RawDecodeParams::default()))
    else {
        return Ok(());
    };
    let exif = &metadata.exif;
    let (Some(model), Some(focal)) = (
        exif.lens_model
            .as_deref()
            .map(str::trim)
            .filter(|model| !model.is_empty()),
        positive(exif.focal_length),
    ) else {
        return Ok(());
    };
    let aperture = positive(exif.fnumber).unwrap_or(0.0);
    if metadata.make.trim().is_empty() || metadata.model.trim().is_empty() {
        return Ok(());
    }
    let database = DATABASE
        .get_or_init(Database::load_bundled)
        .as_ref()
        .context("loading the bundled Lensfun database")?;
    let cameras = database.find_cameras(Some(metadata.make.trim()), metadata.model.trim());
    let Some(camera) = cameras.first() else {
        return Ok(());
    };
    let Some(lens) = select_lens(
        database,
        camera,
        model,
        exif.lens_make.as_deref(),
        focal,
        aperture,
    ) else {
        return Ok(());
    };
    correct(image, camera.crop_factor, lens, focal, aperture)
}

fn positive(value: Option<Rational>) -> Option<f32> {
    let value = value?;
    (value.n > 0 && value.d > 0).then(|| value.n as f32 / value.d as f32)
}

fn select_lens<'a>(
    database: &'a Database,
    camera: &Camera,
    model: &str,
    maker: Option<&str>,
    focal: f32,
    aperture: f32,
) -> Option<&'a Lens> {
    let matched = database.find_lenses(Some(camera), model);
    matched
        .into_iter()
        .enumerate()
        .max_by_key(|(index, lens)| {
            (
                (lens.focal_min..=lens.focal_max).contains(&focal),
                lens.model.eq_ignore_ascii_case(model),
                maker.is_some_and(|make| lens.maker.eq_ignore_ascii_case(make.trim())),
                (lens.aperture_min..=lens.aperture_max).contains(&aperture),
                Reverse(*index),
            )
        })
        .map(|(_, lens)| lens)
}

fn correct(image: &mut ImageBuf, crop: f32, lens: &Lens, focal: f32, aperture: f32) -> Result<()> {
    ensure!(
        image.width > 0 && image.height > 0,
        "empty image for lens correction"
    );
    ensure!(
        lens.lens_type == LensType::Rectilinear,
        "Lensfun cannot convert {:?} projection for {}",
        lens.lens_type,
        lens.model
    );
    let width = image.width as usize;
    let mut modifier = Modifier::new(lens, focal, crop, image.width, image.height, false);
    let distortion = modifier.enable_distortion_correction(lens);
    let tca = modifier.enable_tca_correction(lens);
    // The reference uses the Lensfun default subject distance of 1000 m.
    if aperture > 0.0 && modifier.enable_vignetting_correction(lens, aperture, 1000.0) {
        image
            .data
            .par_chunks_exact_mut(3 * width)
            .enumerate()
            .for_each_init(
                || vec![1.0; width],
                |gains, (y, row)| {
                    gains.fill(1.0);
                    modifier.apply_color_modification_f32(gains, 0.0, y as f32, width, 1, 1);
                    for (pixel, gain) in row.chunks_exact_mut(3).zip(gains) {
                        for component in pixel {
                            *component = from_f64(to_f64(*component) * f64::from(*gain));
                        }
                    }
                },
            );
    }
    if !distortion && !tca {
        return Ok(());
    }

    let half_width = (image.width - 1) as f32 * 0.5;
    let half_height = (image.height - 1) as f32 * 0.5;
    let short_half = half_width.min(half_height);
    let center = [
        half_width + short_half * lens.center_x,
        half_height + short_half * lens.center_y,
    ];
    let mut scale = if tca { 1.001 } else { 1.0 };
    if distortion {
        let mut inverse = Modifier::new(lens, focal, crop, image.width, image.height, true);
        if inverse.enable_distortion_correction(lens) {
            scale *= auto_scale(&inverse, center, [half_width, half_height]);
        }
    }
    let mut corrected = vec![from_f64(0.0); image.data.len()];
    corrected
        .par_chunks_exact_mut(3 * width)
        .enumerate()
        .for_each(|(y, row)| {
            for (x, pixel) in row.chunks_exact_mut(3).enumerate() {
                let [mut sx, mut sy] = [
                    center[0] + (x as f32 - center[0]) / scale,
                    center[1] + (y as f32 - center[1]) / scale,
                ];
                if distortion {
                    let mut geometry = [0.0; 2];
                    modifier.apply_geometry_distortion(sx, sy, 1, 1, &mut geometry);
                    [sx, sy] = geometry;
                }
                let mut coords = [sx, sy, sx, sy, sx, sy];
                if tca {
                    modifier.apply_subpixel_distortion(sx, sy, 1, 1, &mut coords);
                }
                for (channel, point) in coords.chunks_exact(2).enumerate() {
                    pixel[channel] = sample(image, point[0], point[1], channel);
                }
            }
        });
    image.data = corrected;
    Ok(())
}

fn auto_scale(inverse: &Modifier, center: [f32; 2], half: [f32; 2]) -> f32 {
    // Use the same edge midpoints and corners as Lensfun's automatic scaling.
    // Inverting the radial map gives the input position for each sensor edge.
    let mut scale = 0.01_f32;
    for x in [-half[0], 0.0, half[0]] {
        for y in [-half[1], 0.0, half[1]] {
            let mut point = [0.0; 2];
            inverse.apply_geometry_distortion(center[0] + x, center[1] + y, 1, 1, &mut point);
            let distance = (point[0] - center[0]).hypot(point[1] - center[1]);
            if distance > 0.0 {
                scale = scale.max(x.hypot(y) / distance);
            }
        }
    }
    scale * 1.001
}

fn sample(image: &ImageBuf, x: f32, y: f32, channel: usize) -> Scalar {
    let x = f64::from(x).clamp(0.0, f64::from(image.width - 1));
    let y = f64::from(y).clamp(0.0, f64::from(image.height - 1));
    let x0 = x.floor() as u32;
    let y0 = y.floor() as u32;
    let x1 = (x0 + 1).min(image.width - 1);
    let y1 = (y0 + 1).min(image.height - 1);
    let dx = x - f64::from(x0);
    let dy = y - f64::from(y0);
    let value =
        |x, y| to_f64(image.data[(y as usize * image.width as usize + x as usize) * 3 + channel]);
    let top = value(x0, y0) * (1.0 - dx) + value(x1, y0) * dx;
    let bottom = value(x0, y1) * (1.0 - dx) + value(x1, y1) * dx;
    from_f64(top * (1.0 - dy) + bottom * dy)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fuji_rectilinear_correction_matches_python_reference() -> Result<()> {
        let db = Database::load_bundled()?;
        let camera = db.find_cameras(Some("Fujifilm"), "X-T2")[0];
        let lens = select_lens(&db, camera, "XF35mmF1.4 R", Some("Fujifilm"), 35.0, 2.8)
            .context("XF35mmF1.4 R calibration")?;
        let (width, height) = (1200, 800);
        let data = (0..width * height)
            .flat_map(|p| {
                let p = f64::from(p) / f64::from(width * height - 1);
                [p * 0.65 + 0.02, p * 0.45 + 0.2, (1.0 - p) * 0.7 + 0.25].map(from_f64)
            })
            .collect();
        let mut image = ImageBuf::from_data(width, height, data);
        correct(&mut image, camera.crop_factor, lens, 35.0, 2.8)?;
        for (x, y, expected) in [
            (0, 0, [0.03196634, 0.30142206, 1.42268515]),
            (1199, 799, [1.00331688, 0.97369766, 0.37717301]),
            (400, 400, [0.35496479, 0.43712243, 0.61653692]),
        ] {
            let i = (y * width as usize + x) * 3;
            for (actual, expected) in image.data[i..i + 3].iter().zip(expected) {
                // The Rust crate uses newer Lensfun coordinate normalization.
                assert!(
                    (to_f64(*actual) - expected).abs() < 0.003,
                    "({x}, {y}) {actual} vs {expected}"
                );
            }
        }
        Ok(())
    }
}
