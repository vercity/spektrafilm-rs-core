use std::path::Path;

use anyhow::{Context, Result, bail, ensure};
use rawler::{
    CFA, Orientation, RawImage, RawImageData,
    decoders::RawDecodeParams,
    imgop::{
        Dim2, Point, Rect,
        develop::{Intermediate, ProcessingStep, RawDevelop},
        matrix::{multiply, normalize, pseudo_inverse},
        raw::clip_euclidean_norm_avg,
        xyz::{Illuminant, SRGB_TO_XYZ_D65},
    },
    rawimage::RawPhotometricInterpretation,
    rawsource::RawSource,
};
use rayon::prelude::*;
use spektrafilm_math::{
    colorspace::{
        ACES_TO_XYZ_F64, D65_XYZ, XYZ_TO_ACES, XYZ_TO_ACES_F64, chromatic_adaptation_matrix_f64,
        mat3_mul, mat3_mul3_f64, rgb_to_rgb_matrix, xy_to_xyz, xy_to_xyz_f64,
    },
    image::ImageBuf,
    precision::{Scalar, from_f32, from_f64},
};

/// The shared RAW decoder returns linear pixels in ACES AP0.
pub const INPUT_COLOR_SPACE: &str = "ACES2065-1";

pub fn is_raw(path: &Path) -> bool {
    let Some(extension) = path.extension().and_then(|ext| ext.to_str()) else {
        return false;
    };
    rawler::decoders::supported_extensions()
        .iter()
        // Include the aliases already accepted by the GUI and CLI.
        .chain(["SRF", "SR2", "PTX", "BAY", "DRF", "K25"].iter())
        .any(|ext| ext.eq_ignore_ascii_case(extension))
}

#[derive(Clone, Copy, Debug, Default)]
pub enum WhiteBalance {
    #[default]
    AsShot,
    Daylight,
    Tungsten,
    Custom {
        kelvin: f64,
        tint: f64,
    },
}

#[derive(Clone, Copy, Debug, Default)]
pub struct DevelopOptions {
    pub white_balance: WhiteBalance,
    pub lens_correction: bool,
}

pub fn decode(path: &Path) -> Result<ImageBuf> {
    decode_with_options(path, DevelopOptions::default())
}

pub fn decode_with_options(path: &Path, options: DevelopOptions) -> Result<ImageBuf> {
    let white_balance = options.white_balance;
    let adaptation = white_balance.adaptation()?;
    let source =
        RawSource::new(path).with_context(|| format!("opening RAW image {}", path.display()))?;
    let params = RawDecodeParams::default();
    let mut raw = rawler::decode(&source, &params)
        .with_context(|| format!("decoding RAW image {}", path.display()))?;
    let metadata_orientation =
        if matches!(raw.orientation, Orientation::Normal | Orientation::Unknown) {
            // Several Rawler decoders leave RawImage::orientation at its default.
            rawler::get_decoder(&source)
                .and_then(|decoder| decoder.raw_metadata(&source, &params))
                .ok()
                .and_then(|metadata| metadata.exif.orientation)
        } else {
            None
        };
    let orientation = metadata_orientation
        .map(Orientation::from_u16)
        .unwrap_or(raw.orientation);

    let sensor = Rect::new(Point::new(0, 0), raw.dim());
    let area = if matches!(
        &raw.photometric,
        RawPhotometricInterpretation::Cfa(config) if config.cfa.width == 6 && config.cfa.height == 6
    ) {
        fuji_xtrans_active_area(source.buf(), sensor.d, raw.crop_area).or(raw.active_area)
    } else {
        raw.active_area
    }
    .unwrap_or(sensor);
    validate_area(area, sensor.d)?;
    if path
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("dng"))
        && matches!(raw.photometric, RawPhotometricInterpretation::Cfa(_))
    {
        adjust_dng_white_level(&mut raw, area);
    }
    let color_matrix = raw
        .color_matrix
        .get_key_value(&Illuminant::D65)
        .or_else(|| {
            raw.color_matrix
                .iter()
                .min_by_key(|(light, _)| **light as u32)
        });
    let is_cfa = matches!(raw.photometric, RawPhotometricInterpretation::Cfa(_));
    if raw.cpp > 1 || is_cfa {
        ensure!(
            color_matrix.is_some_and(|(_, matrix)| {
                matches!(matrix.len(), 9 | 12) && matrix.iter().all(|value| value.is_finite())
            }),
            "missing or invalid RAW color matrix for {}",
            path.display()
        );
    }

    let mut xyz_to_camera = [[0.0; 3]; 4];
    if let Some((&illuminant, matrix)) = color_matrix {
        for (row, values) in xyz_to_camera.iter_mut().zip(matrix.chunks_exact(3)) {
            row.copy_from_slice(values);
        }
        raw.wb_coeffs = white_balance_coefficients(&raw, &xyz_to_camera, white_balance);
        raw.color_matrix.retain(|light, _| *light == illuminant);
    }
    let mut image = match &raw.photometric {
        RawPhotometricInterpretation::Cfa(config)
            if config.cfa.width == 6 && config.cfa.height == 6 && config.cfa.is_rgb() =>
        {
            develop_xtrans(&raw, area, orientation, &config.cfa, &xyz_to_camera)
        }
        RawPhotometricInterpretation::BlackIsZero => {
            let samples = linear_sensor_samples(&raw)?;
            oriented_aces(area, raw.width, orientation, |i| [from_f32(samples[i]); 3])
        }
        _ => {
            if matches!(&raw.photometric, RawPhotometricInterpretation::Cfa(config) if config.cfa.is_rgb()) {
                ensure!(area.d.w >= 6 && area.d.h >= 6, "RAW active area is too small for demosaicing");
            }
            // LibRaw keeps the sensor's active rectangle on DNG, ignoring DefaultCrop.
            // Rawler applies it during CFA demosaicing only.
            let mut develop = RawDevelop::default();
            develop
                .steps
                .retain(|step| !matches!(step, ProcessingStep::CropDefault | ProcessingStep::SRgb));
            let intermediate = develop
                .develop_intermediate(&raw)
                .with_context(|| format!("developing RAW image {}", path.display()))?;
            let area = if is_cfa { None } else { Some(area) };
            to_aces(intermediate, area, orientation)
        }
    }
    .with_context(|| {
        format!(
            "converting RAW image {} to {INPUT_COLOR_SPACE}",
            path.display()
        )
    })?;
    if options.lens_correction {
        crate::lens::apply(&source, &mut image)
            .with_context(|| format!("correcting RAW lens for {}", path.display()))?;
    }
    if let Some(matrix) = adaptation {
        image.data.par_chunks_exact_mut(3).for_each(|rgb| {
            let input = [rgb[0] as f64, rgb[1] as f64, rgb[2] as f64];
            for (channel, row) in rgb.iter_mut().zip(&matrix) {
                *channel = from_f64(row.iter().zip(input).map(|(m, x)| m * x).sum());
            }
        });
    }
    Ok(image)
}

impl WhiteBalance {
    fn adaptation(self) -> Result<Option<[[f64; 3]; 3]>> {
        let (kelvin, tint) = match self {
            Self::AsShot | Self::Daylight => return Ok(None),
            Self::Tungsten => (2850.0, 1.0),
            Self::Custom { kelvin, tint } => (kelvin, tint),
        };
        ensure!(
            (1667.0..=25_000.0).contains(&kelvin),
            "RAW white balance temperature must be between 1667 K and 25000 K"
        );
        ensure!(
            tint.is_finite() && tint > 0.0,
            "RAW white balance tint must be finite and positive"
        );
        let source = whitepoint_from_cct(kelvin);
        let daylight = whitepoint_from_cct(6504.0);
        let cct = chromatic_adaptation_matrix_f64(source, daylight);
        let mut aces = mat3_mul3_f64(&XYZ_TO_ACES_F64, &mat3_mul3_f64(&cct, &ACES_TO_XYZ_F64));
        for value in &mut aces[1] {
            *value *= tint;
        }
        Ok(Some(aces))
    }
}

fn whitepoint_from_cct(t: f64) -> [f64; 3] {
    let t2 = t * t;
    let t3 = t2 * t;
    // Same CIE daylight and Kang (2002) Planckian loci as Python's RAW loader.
    let (x, y) = if t >= 4000.0 {
        let x = if t <= 7000.0 {
            -4.607e9 / t3 + 2.9678e6 / t2 + 0.09911e3 / t + 0.244063
        } else {
            -2.0064e9 / t3 + 1.9018e6 / t2 + 0.24748e3 / t + 0.23704
        };
        (x, -3.0 * x * x + 2.87 * x - 0.275)
    } else {
        let x = -0.2661239e9 / t3 - 0.2343589e6 / t2 + 0.8776956e3 / t + 0.179910;
        let y = if t <= 2222.0 {
            ((-1.1063814 * x - 1.34811020) * x + 2.18555832) * x - 0.20219683
        } else {
            ((-0.9549476 * x - 1.37418593) * x + 2.09137015) * x - 0.16748867
        };
        (x, y)
    };
    xy_to_xyz_f64(x, y)
}

fn validate_area(area: Rect, sensor: Dim2) -> Result<()> {
    ensure!(
        !area.is_empty()
            && area
                .p
                .x
                .checked_add(area.d.w)
                .is_some_and(|x| x <= sensor.w)
            && area
                .p
                .y
                .checked_add(area.d.h)
                .is_some_and(|y| y <= sensor.h),
        "invalid RAW active area"
    );
    Ok(())
}

fn fuji_xtrans_active_area(data: &[u8], sensor: Dim2, inset: Option<Rect>) -> Option<Rect> {
    // The X-T2 generation stores the full active dimensions in RAFData,
    // distinct from the camera's suggested crop and sensor padding.
    // LibRaw 0.22 identifies the 6048/6160-pixel sensor family at left=0,
    // then rounds the top margin up to the next six-row CFA boundary.
    if !data.starts_with(b"FUJIFILMCCD-RAW") || !matches!(sensor.w, 6048 | 6160) {
        return None;
    }
    let inset = inset?;
    let offset = u32::from_be_bytes(data.get(92..96)?.try_into().ok()?) as usize;
    let (count, mut fields) = data.get(offset..)?.split_first_chunk::<4>()?;
    let count = u32::from_be_bytes(*count);
    if count > 4000 {
        return None;
    }
    for _ in 0..count {
        let (&[tag_high, tag_low, len_high, len_low], tail) = fields.split_first_chunk::<4>()?;
        let len = usize::from(u16::from_be_bytes([len_high, len_low]));
        let (value, remaining) = tail.split_at_checked(len)?;
        fields = remaining;
        if [tag_high, tag_low] != [0xc0, 0x00]
            || !value.get(4..8).is_some_and(|magic| magic == b"WRTS")
        {
            continue;
        }
        let width = u32::from_le_bytes(value.get(8..12)?.try_into().ok()?) as usize;
        let height = u32::from_le_bytes(value.get(12..16)?.try_into().ok()?) as usize;
        let unaligned_top = sensor.h.checked_sub(height)? / 4 * 2;
        let top = unaligned_top.next_multiple_of(6);
        let height = height.checked_sub(top - unaligned_top)?;
        let area = Rect::new(Point::new(0, top), Dim2::new(width, height));
        validate_area(area, sensor).ok()?;
        let [_, inset_top, inset_right, inset_bottom] = inset.as_ltrb();
        // Keep the existing decoder's fallback for in-camera crops and
        // variants whose recommended rectangle does not describe a full shot.
        if inset_top < top
            || inset_right > width
            || inset_bottom > top + height
            || inset.d.w * 20 < width * 19
            || inset.d.h * 20 < height * 19
        {
            return None;
        }
        return Some(area);
    }
    None
}

fn adjust_dng_white_level(raw: &mut RawImage, area: Rect) {
    // LibRaw's DNG adjustment lowers an over-reported sensor white to the
    // observed, black-subtracted maximum when it is near saturation.
    // Apply this to integer mosaics with uniform levels.
    let RawImageData::Integer(samples) = &raw.data else {
        return;
    };
    let black = raw.blacklevel.as_vec();
    let Some(&black_level) = black.first() else {
        return;
    };
    if !black.iter().all(|value| *value == black_level) {
        return;
    }
    let [white_level] = raw.whitelevel.0.as_mut_slice() else {
        return;
    };
    let [left, top, right, bottom] = area.as_ltrb();
    let actual = (top..bottom)
        .into_par_iter()
        .map(|row| {
            let start = row * raw.width;
            samples[start + left..start + right]
                .iter()
                .copied()
                .max()
                .unwrap_or(0)
        })
        .max()
        .unwrap_or(0);
    let usable_range = *white_level as f32 - black_level;
    let observed_range = f32::from(actual) - black_level;
    if observed_range > 0.75 * usable_range && observed_range < usable_range {
        *white_level = u32::from(actual);
    }
}

fn white_balance_coefficients(
    raw: &RawImage,
    xyz_to_camera: &[[f32; 3]; 4],
    white_balance: WhiteBalance,
) -> [f32; 4] {
    let color_count = if raw.cpp == 4
        || matches!(
            &raw.photometric,
            RawPhotometricInterpretation::Cfa(config) if config.cfa.unique_colors() == 4
        ) {
        4
    } else {
        3
    };
    let coeffs = if matches!(white_balance, WhiteBalance::AsShot)
        && raw.wb_coeffs[..color_count]
            .iter()
            .all(|value| value.is_finite() && *value > 0.0)
    {
        raw.wb_coeffs
    } else {
        // The default daylight coefficients undo each camera row's response
        // to D65 before the normalized camera matrix operates on the samples.
        multiply(xyz_to_camera, &SRGB_TO_XYZ_D65).map(|row| row.iter().sum::<f32>().recip())
    };
    let mut coefficients = [1.0; 4];
    let min = coeffs[..color_count]
        .iter()
        .copied()
        .reduce(f32::min)
        .unwrap_or(1.0);
    for (output, value) in coefficients.iter_mut().zip(&coeffs[..color_count]) {
        if value.is_finite() && *value > 0.0 && min.is_finite() && min > 0.0 {
            *output = value / min;
        }
    }
    coefficients
}

fn srgb_to_aces() -> [[f32; 3]; 3] {
    rgb_to_rgb_matrix(
        &SRGB_TO_XYZ_D65,
        D65_XYZ,
        &XYZ_TO_ACES,
        xy_to_xyz(0.32168, 0.33767),
    )
}

fn linear_sensor_samples(raw: &RawImage) -> Result<Vec<f32>> {
    let repeat = Dim2::new(raw.blacklevel.width, raw.blacklevel.height);
    let black_levels = raw.blacklevel.as_vec();
    let white = raw.whitelevel.as_vec();
    ensure!(
        raw.cpp == 1
            && !repeat.is_empty()
            && repeat.w.checked_mul(repeat.h) == Some(black_levels.len())
            && !white.is_empty(),
        "invalid RAW sensor levels"
    );
    let level: Vec<_> = black_levels
        .iter()
        .enumerate()
        .map(|(i, &black)| {
            let white = if white.len() == black_levels.len() {
                white[i]
            } else {
                white[0]
            };
            (black, white - black)
        })
        .collect();
    ensure!(
        level
            .iter()
            .all(|&(offset, scale)| offset.is_finite() && scale.is_finite() && scale > 0.0),
        "invalid RAW black or white level"
    );
    let mut samples = raw.data.as_f32().into_owned();
    samples
        .par_chunks_exact_mut(raw.width)
        .enumerate()
        .for_each(|(y, row)| {
            let start = y % repeat.h * repeat.w;
            for (x, sample) in row.iter_mut().enumerate() {
                let (offset, scale) = level[start + x % repeat.w];
                *sample = ((*sample - offset) / scale).max(0.0);
            }
        });
    Ok(samples)
}

fn develop_xtrans(
    raw: &RawImage,
    area: Rect,
    orientation: Orientation,
    cfa: &CFA,
    xyz_to_camera: &[[f32; 3]; 4],
) -> Result<ImageBuf> {
    ensure!(
        area.d.w >= 6 && area.d.h >= 6,
        "X-Trans active area is smaller than one CFA pattern"
    );
    let samples = linear_sensor_samples(raw)?;
    let camera_to_srgb = pseudo_inverse(normalize(multiply(xyz_to_camera, &SRGB_TO_XYZ_D65)));
    let camera_to_srgb = camera_to_srgb.map(|[r, g, b, _]| [r, g, b]);
    ensure!(
        camera_to_srgb.iter().flatten().all(|v| v.is_finite()),
        "invalid X-Trans camera color matrix"
    );
    let to_aces = srgb_to_aces();
    let offsets = xtrans_offsets(cfa);
    oriented_aces(area, raw.width, orientation, |index| {
        let x = index % raw.width;
        let y = index / raw.width;
        let phase = y % 6 * 6 + x % 6;
        let camera = std::array::from_fn(|channel| {
            interpolate_xtrans(&samples, raw.width, area, x, y, &offsets[phase][channel])
                * raw.wb_coeffs[channel]
        });
        let rgb = clip_euclidean_norm_avg(&mat3_mul(&camera_to_srgb, camera));
        mat3_mul(&to_aces, rgb).map(from_f32)
    })
}

#[derive(Clone, Copy)]
struct Offset {
    dx: isize,
    dy: isize,
    distance_sq: isize,
}

fn xtrans_offsets(cfa: &CFA) -> [[Vec<Offset>; 3]; 36] {
    // Precompute nearest samples for each phase of the 6x6 pattern. The
    // wider search radius provides fallback neighbors at the active border.
    std::array::from_fn(|phase| {
        let x = (phase % 6) as isize;
        let y = (phase / 6) as isize;
        let mut channels: [Vec<Offset>; 3] = std::array::from_fn(|_| Vec::new());
        for dy in -3..=3 {
            for dx in -3..=3 {
                let color = cfa.color_at(
                    (y + dy).rem_euclid(6) as usize,
                    (x + dx).rem_euclid(6) as usize,
                );
                channels[color].push(Offset {
                    dx,
                    dy,
                    distance_sq: dx * dx + dy * dy,
                });
            }
        }
        for offsets in &mut channels {
            offsets.sort_by_key(|sample| sample.distance_sq);
        }
        channels
    })
}

fn interpolate_xtrans(
    samples: &[f32],
    width: usize,
    area: Rect,
    x: usize,
    y: usize,
    offsets: &[Offset],
) -> f32 {
    let [left, top, right, bottom] = area.as_ltrb();
    let mut closest = isize::MAX;
    let mut sum = 0.0;
    let mut count = 0;
    for &Offset {
        dx,
        dy,
        distance_sq,
    } in offsets
    {
        if distance_sq > closest {
            break;
        }
        let sx = (x as isize + dx) as usize;
        let sy = (y as isize + dy) as usize;
        if (left..right).contains(&sx) && (top..bottom).contains(&sy) {
            closest = distance_sq;
            sum += samples[sy * width + sx];
            count += 1;
        }
    }
    if count > 0 {
        sum / count as f32
    } else {
        samples[y * width + x]
    }
}

fn to_aces(
    intermediate: Intermediate,
    active_area: Option<Rect>,
    orientation: Orientation,
) -> Result<ImageBuf> {
    let dim = intermediate.dim();
    let area = active_area.unwrap_or_else(|| intermediate.rect());
    validate_area(area, dim)?;
    let matrix = srgb_to_aces();
    match intermediate {
        Intermediate::Monochrome(pixels) => oriented_aces(area, dim.w, orientation, |i| {
            let value = pixels.data[i].max(0.0);
            [from_f32(value); 3]
        }),
        Intermediate::ThreeColor(pixels) => oriented_aces(area, dim.w, orientation, |i| {
            let rgb = pixels.data[i].map(|value| value.max(0.0));
            mat3_mul(&matrix, rgb).map(from_f32)
        }),
        Intermediate::FourColor(_) => bail!("RAW camera color data was not calibrated"),
    }
}

fn oriented_aces(
    area: Rect,
    source_width: usize,
    orientation: Orientation,
    pixel: impl Fn(usize) -> [Scalar; 3] + Sync,
) -> Result<ImageBuf> {
    let (transpose, flip_x, flip_y) = orientation.to_flips();
    let (width, height) = if transpose {
        (area.d.h, area.d.w)
    } else {
        (area.d.w, area.d.h)
    };
    let width_u32 = u32::try_from(width).context("RAW image is too wide")?;
    let height_u32 = u32::try_from(height).context("RAW image is too tall")?;
    let count = width
        .checked_mul(height)
        .context("RAW image is too large")?;
    let mut data = vec![from_f32(0.0); count.checked_mul(3).context("RAW image is too large")?];
    data.par_chunks_exact_mut(width * 3)
        .enumerate()
        .for_each(|(row, pixels)| {
            for (col, rgb) in pixels.chunks_exact_mut(3).enumerate() {
                let (x, y) = if transpose { (row, col) } else { (col, row) };
                let x = if flip_x { area.d.w - 1 - x } else { x };
                let y = if flip_y { area.d.h - 1 - y } else { y };
                let i = (y + area.p.y) * source_width + x + area.p.x;
                rgb.copy_from_slice(&pixel(i));
            }
        });
    Ok(ImageBuf::from_data(width_u32, height_u32, data))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn applies_each_exif_orientation_after_active_crop() {
        let area = Rect::new(Point::new(1, 1), Dim2::new(3, 2));
        for (orientation, width, height, expected) in [
            (Orientation::Normal, 3, 2, [5, 6, 7, 9, 10, 11]),
            (Orientation::HorizontalFlip, 3, 2, [7, 6, 5, 11, 10, 9]),
            (Orientation::Rotate180, 3, 2, [11, 10, 9, 7, 6, 5]),
            (Orientation::VerticalFlip, 3, 2, [9, 10, 11, 5, 6, 7]),
            (Orientation::Transpose, 2, 3, [5, 9, 6, 10, 7, 11]),
            (Orientation::Rotate90, 2, 3, [9, 5, 10, 6, 11, 7]),
            (Orientation::Transverse, 2, 3, [11, 7, 10, 6, 9, 5]),
            (Orientation::Rotate270, 2, 3, [7, 11, 6, 10, 5, 9]),
        ] {
            let image = oriented_aces(area, 4, orientation, |i| [from_f32(i as f32); 3]).unwrap();
            assert_eq!((image.width, image.height), (width, height));
            assert_eq!(
                image.data,
                expected
                    .into_iter()
                    .flat_map(|i| [from_f32(i as f32); 3])
                    .collect::<Vec<_>>(),
                "{orientation:?}"
            );
        }
    }

    #[test]
    fn xtrans_interpolation_keeps_three_constant_planes_separate_at_edges() {
        let cfa = CFA::new("GRGGBGBGBRGRGRGGBGGBGGRGRGRBGBGBGGRG");
        let expect = [0.125, 0.5, 0.875];
        let mut samples = Vec::new();
        for y in 0..10 {
            for x in 0..13 {
                samples.push(expect[cfa.color_at(y, x)]);
            }
        }
        let area = Rect::new(Point::new(1, 2), Dim2::new(11, 7));
        let offsets = xtrans_offsets(&cfa);
        for y in 2..9 {
            for x in 1..12 {
                let phase = y % 6 * 6 + x % 6;
                let rgb = offsets[phase]
                    .each_ref()
                    .map(|neighbors| interpolate_xtrans(&samples, 13, area, x, y, neighbors));
                assert_eq!(rgb, expect);
            }
        }
    }

    #[test]
    fn fuji_wrts_rounds_the_active_area_to_xtrans_phase() {
        let mut raf = vec![0; 96];
        raf[..15].copy_from_slice(b"FUJIFILMCCD-RAW");
        raf[92..96].copy_from_slice(&96u32.to_be_bytes());
        raf.extend_from_slice(&1u32.to_be_bytes());
        raf.extend_from_slice(&[0xc0, 0x00, 0x00, 0x10]);
        raf.extend_from_slice(&0x0258_0000u32.to_le_bytes());
        raf.extend_from_slice(b"WRTS");
        raf.extend_from_slice(&6032u32.to_le_bytes());
        raf.extend_from_slice(&4032u32.to_le_bytes());
        let inset = Rect::new(Point::new(16, 21), Dim2::new(6000, 4000));
        let sensor = Dim2::new(6048, 4038);
        let active = fuji_xtrans_active_area(&raf, sensor, Some(inset)).unwrap();
        assert_eq!(active, Rect::new(Point::new(0, 6), Dim2::new(6032, 4028)));
        raf.pop();
        assert_eq!(fuji_xtrans_active_area(&raf, sensor, Some(inset)), None);
    }

    #[test]
    fn tungsten_and_tint_match_python_aces_white_balance() {
        // Golden rows from the reference RAW processor applied to RGB basis vectors.
        let expected = [
            [0.8177768, -0.10810402, 0.42100173],
            [-0.019335484, 1.11116278, 0.20303074],
            [0.01653093, 0.019360978, 2.96131921],
        ];
        let tungsten = WhiteBalance::Tungsten.adaptation().unwrap().unwrap();
        let custom = WhiteBalance::Custom {
            kelvin: 2850.0,
            tint: 1.2,
        }
        .adaptation()
        .unwrap()
        .unwrap();
        for (channel, row) in expected.iter().enumerate() {
            for (column, &value) in row.iter().enumerate() {
                assert!((tungsten[channel][column] - value).abs() < 1e-6);
                let tinted = if channel == 1 { 1.2 * value } else { value };
                assert!((custom[channel][column] - tinted).abs() < 1e-6);
            }
        }
    }
}
