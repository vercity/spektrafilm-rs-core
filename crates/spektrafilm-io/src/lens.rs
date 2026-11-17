use std::{cmp::Reverse, sync::OnceLock};

use anyhow::{Context, Result};
use lensfun::{Camera, Database, Lens};
use rawler::{decoders::RawDecodeParams, formats::tiff::Rational, rawsource::RawSource};
use spektrafilm_math::{
    image::ImageBuf,
    precision::{Scalar, from_f64, to_f64},
};

mod correction;
mod projection;

use correction::Correction;

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
    Correction::new(image.width, image.height, crop, lens, focal, aperture)?.apply(image);
    Ok(())
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
    const FIXTURE_SIZE: (u32, u32) = (97, 51);

    fn fixture_lens(projection: &str) -> Result<Lens> {
        let calibration = if matches!(projection, "equisolid" | "fisheye") {
            r#"<calibration>
              <distortion model="ptlens" focal="12" a="0.013" b="-0.037" c="0.018" real-focal="12.45"/>
              <tca model="poly3" focal="12" br="0.004" cr="-0.002" vr="1.002" bb="-0.0025" cb="0.003" vb="0.997"/>
              <vignetting model="pa" focal="12" aperture="4" distance="1000" k1="-0.15" k2="0.04" k3="-0.01"/>
              </calibration>"#
        } else {
            ""
        };
        let mut db = Database::new();
        db.load_str(&format!(
            r#"<lensdatabase version="2">
              <mount><name>Fixture mount</name></mount>
              <lens>
              <maker>Test</maker><model>Fixture 12mm f/4</model>
              <mount>Fixture mount</mount><cropfactor>1.2</cropfactor>
              <aspect-ratio>3:2</aspect-ratio><center x="0.047" y="-0.031"/>
              <type>{projection}</type>{calibration}
              </lens>
              </lensdatabase>"#
        ))?;
        Ok(db.lenses.remove(0))
    }

    fn fixture_image(width: u32, height: u32) -> ImageBuf {
        let data = (0..height)
            .flat_map(|y| {
                (0..width).flat_map(move |x| {
                    let (x, y) = (f64::from(x), f64::from(y));
                    [
                        0.06 + x * 0.006 + y * 0.001,
                        0.25 + x * 0.003 + y * 0.004,
                        0.9 - x * 0.004 + y * 0.002,
                    ]
                    .map(|v| from_f64(f64::from(v as f32)))
                })
            })
            .collect();
        ImageBuf::from_data(width, height, data)
    }

    #[test]
    fn non_rectilinear_projections_match_native_lensfun_in_both_orientations() -> Result<()> {
        // Top-left green sample from lensfunpy 1.18 / Lensfun 0.3.4 with
        // Modifier.initialize(12, 4, flags=ALL, pixel_format=float32).
        for (kind, landscape, portrait) in [
            (
                "fisheye",
                [11.16416454, 5.49978209],
                [5.79552078, 10.45628929],
            ),
            (
                "panoramic",
                [11.85065460, 7.87217808],
                [2.48436308, 6.42691135],
            ),
            (
                "equirectangular",
                [9.38152122, 7.83896399],
                [0.26008373, 12.26835155],
            ),
            (
                "orthographic",
                [15.83284092, 7.79970551],
                [8.24428749, 14.87436199],
            ),
            (
                "stereographic",
                [9.09546947, 4.48068571],
                [4.71423626, 8.50543880],
            ),
            (
                "equisolid",
                [12.18648243, 6.00340700],
                [6.33075905, 11.42196751],
            ),
            (
                "fisheye_thoby",
                [12.84396267, 6.32729816],
                [6.67382669, 12.04092789],
            ),
        ] {
            let lens = fixture_lens(kind)?;
            for (width, height, expected) in [(97, 51, landscape), (51, 97, portrait)] {
                let correction = Correction::new(width, height, 1.5, &lens, 12.0, 4.0)?;
                let green = correction.sample_at(width, height, 0, 0)[1];
                for (actual, expected) in green.into_iter().map(f64::from).zip(expected) {
                    assert!(
                        (actual - expected).abs() < 0.0005,
                        "{kind}, {width}×{height}: {actual} vs {expected}"
                    );
                }
            }
        }
        Ok(())
    }

    #[test]
    fn fisheye_remapping_composes_projection_distortion_tca_and_vignetting() -> Result<()> {
        let (width, height) = FIXTURE_SIZE;
        for (kind, expected_coords, expected_pixels) in [
            (
                "fisheye",
                [
                    [19.27515030, 35.16736603],
                    [19.36962509, 35.13278961],
                    [19.43671799, 35.10823822],
                ],
                [
                    [0.13875002, 0.32269055, 0.91452539],
                    [0.61416233, 0.55735010, 0.59714407],
                    [0.65473086, 0.72258097, 0.68110830],
                    [0.21764626, 0.46308479, 0.92108196],
                ],
            ),
            (
                "equisolid",
                [
                    [19.54416275, 35.06891632],
                    [19.63673210, 35.03503799],
                    [19.70295334, 35.01080322],
                ],
                [
                    [0.14544910, 0.32716617, 0.90882909],
                    [0.60719180, 0.55506778, 0.60058957],
                    [0.64642406, 0.71555239, 0.68225354],
                    [0.21909003, 0.46325535, 0.91928136],
                ],
            ),
        ] {
            let lens = fixture_lens(kind)?;
            let correction = Correction::new(width, height, 1.5, &lens, 12.0, 4.0)?;
            let sampled = correction.sample_at(width, height, 17, 36);
            for (actual, expected) in sampled
                .into_iter()
                .flatten()
                .map(f64::from)
                .zip(expected_coords.into_iter().flatten())
            {
                assert!(
                    (actual - expected).abs() < 0.0005,
                    "{kind}: {actual} vs {expected}"
                );
            }
            let mut image = fixture_image(width, height);
            correction.apply(&mut image);
            for ((x, y), expected) in [(0, 0), (96, 0), (96, 50), (17, 36)]
                .into_iter()
                .zip(expected_pixels)
            {
                let i = (y * width as usize + x) * 3;
                for (actual, expected) in image.data[i..i + 3]
                    .iter()
                    .copied()
                    .map(to_f64)
                    .zip(expected)
                {
                    assert!(
                        (actual - expected).abs() < 0.000004,
                        "{kind} ({x}, {y}): {actual} vs {expected}"
                    );
                }
            }
        }
        Ok(())
    }
}
