//! Shared RGB image input and output for the CLI and GUI.

mod color;
mod metadata;

use std::{
    borrow::Cow,
    fs::File,
    io::{BufWriter, Write},
    path::Path,
};

use anyhow::{Context, Result, bail, ensure};
use exr::prelude::{
    Encoding, Image, IntoSample, SpecificChannels, Vec2, WritableImage, f16,
    read_first_rgba_layer_from_file,
};
use image::{DynamicImage, ImageDecoder, ImageEncoder, ImageFormat, ImageReader};
use spektrafilm_math::{
    image::ImageBuf,
    precision::{from_f32, from_f64, to_f32, to_f64},
};
use tiff::{
    encoder::{Compression, DeflateLevel, TiffEncoder, colortype},
    tags::Tag,
};

pub use color::RasterColor;
pub use metadata::RasterMetadata;

pub struct RasterImage {
    pub image: ImageBuf,
    pub metadata: RasterMetadata,
    /// Recognized file colorimetry and transfer. Callers can explicitly override it.
    pub color: Option<RasterColor>,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct RasterWriteOptions<'a> {
    /// Defaults to 16 for TIFF and EXR, 8 for PNG and JPEG.
    pub bit_depth: Option<u8>,
    pub color_space: Option<&'a str>,
    pub cctf_encoding: bool,
    pub source_metadata: Option<&'a RasterMetadata>,
}

/// Read samples as stored, normalize integer samples, and apply file orientation.
/// Transfer decoding belongs to the rendering pipeline.
pub fn read_raster(path: &Path) -> Result<RasterImage> {
    read_raster_inner(path).with_context(|| format!("reading raster {}", path.display()))
}

fn read_raster_inner(path: &Path) -> Result<RasterImage> {
    let mut reader = ImageReader::open(path)?.with_guessed_format()?;
    if reader.format() == Some(ImageFormat::OpenExr) {
        return read_exr(path);
    }

    let format = reader.format();
    // A normal 60 MP float RGB photo needs more than image's default limit.
    reader.no_limits();
    let mut decoder = reader.into_decoder()?;
    let metadata = RasterMetadata::from_decoder(path, format, &mut decoder);
    let decoder_icc = decoder.icc_profile().ok().flatten();
    let color = decoder_icc
        .as_deref()
        .or(metadata.icc.as_deref())
        .and_then(color::identify_icc);
    let orientation = decoder
        .orientation()
        .unwrap_or(image::metadata::Orientation::NoTransforms);
    let mut decoded = DynamicImage::from_decoder(decoder)?;
    decoded.apply_orientation(orientation);
    let width = decoded.width();
    let height = decoded.height();
    let data = match decoded {
        DynamicImage::ImageRgb8(rgb) => rgb
            .into_raw()
            .into_iter()
            .map(|v| from_f64(f64::from(v) / 255.0))
            .collect(),
        DynamicImage::ImageRgb16(rgb) => rgb
            .into_raw()
            .into_iter()
            .map(|v| from_f64(f64::from(v) / 65535.0))
            .collect(),
        _ => decoded
            .into_rgb32f()
            .into_raw()
            .into_iter()
            .map(from_f32)
            .collect(),
    };
    Ok(RasterImage {
        image: ImageBuf::from_data(width, height, data),
        metadata,
        color,
    })
}

fn read_exr(path: &Path) -> Result<RasterImage> {
    let decoded = read_first_rgba_layer_from_file(
        path,
        |size, _| ImageBuf::new(size.width() as u32, size.height() as u32),
        |pixels, position, (r, g, b, _): (f32, f32, f32, f32)| {
            let index = (position.y() * pixels.width as usize + position.x()) * 3;
            pixels.data[index..index + 3].copy_from_slice(&[r, g, b].map(from_f32));
        },
    )?;
    Ok(RasterImage {
        image: decoded.layer_data.channel_data.pixels,
        metadata: RasterMetadata::default(),
        color: color::identify_exr(&decoded.attributes, &decoded.layer_data.attributes),
    })
}

/// Read transferable metadata from a RAW or ordinary image on a best-effort basis.
pub fn read_source_metadata(path: &Path) -> RasterMetadata {
    metadata::read_source_metadata(path)
}

/// Write the rendering buffer in the requested format and attach its actual colorimetry.
pub fn write_raster(path: &Path, image: &ImageBuf, options: &RasterWriteOptions<'_>) -> Result<()> {
    write_raster_inner(path, image, options)
        .with_context(|| format!("writing raster {}", path.display()))
}

fn write_raster_inner(
    path: &Path,
    image: &ImageBuf,
    options: &RasterWriteOptions<'_>,
) -> Result<()> {
    ensure!(
        image.width != 0
            && image.height != 0
            && Some(image.data.len())
                == (image.width as usize)
                    .checked_mul(image.height as usize)
                    .and_then(|pixels| pixels.checked_mul(3)),
        "invalid RGB image dimensions"
    );

    let extension = path
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    let format = match extension.as_str() {
        "tif" | "tiff" => ImageFormat::Tiff,
        "png" => ImageFormat::Png,
        "jpg" | "jpeg" => ImageFormat::Jpeg,
        "exr" => ImageFormat::OpenExr,
        _ => bail!("unsupported output extension {extension:?} (expected TIFF, EXR, PNG or JPEG)"),
    };
    let depth = options.bit_depth.unwrap_or(match format {
        ImageFormat::Tiff | ImageFormat::OpenExr => 16,
        _ => 8,
    });
    let supported = match format {
        ImageFormat::Tiff => matches!(depth, 8 | 16 | 32),
        ImageFormat::OpenExr => matches!(depth, 16 | 32),
        ImageFormat::Png => matches!(depth, 8 | 16),
        _ => depth == 8,
    };
    ensure!(supported, "{depth}-bit {extension} output is unsupported");

    let color = options
        .color_space
        .and_then(|name| RasterColor::new(name, options.cctf_encoding));
    if format == ImageFormat::OpenExr {
        if depth == 16 {
            return write_exr(path, image, color, |v| f16::from_f64(to_f64(v)));
        }
        return write_exr(path, image, color, to_f32);
    }

    // Build metadata before creating the destination.
    let icc = color.map(color::make_icc).transpose()?.flatten();
    let empty = RasterMetadata::default();
    let metadata = options.source_metadata.unwrap_or(&empty);
    let file = BufWriter::new(File::create(path)?);

    match format {
        ImageFormat::Tiff => write_tiff(file, image, depth, icc.as_deref(), metadata, color),
        ImageFormat::Png => write_png(file, image, depth, icc, metadata, color),
        ImageFormat::Jpeg => write_jpeg(file, image, icc, metadata, color),
        _ => unreachable!("validated raster format"),
    }
}

fn quantize8(image: &ImageBuf) -> Vec<u8> {
    image
        .data
        .iter()
        .map(|&v| (to_f64(v).clamp(0.0, 1.0) * 255.0) as u8)
        .collect()
}

fn quantize16(image: &ImageBuf) -> impl Iterator<Item = u16> + '_ {
    image
        .data
        .iter()
        .map(|&v| (to_f64(v).clamp(0.0, 1.0) * 65535.0) as u16)
}

fn write_tiff(
    mut file: BufWriter<File>,
    image: &ImageBuf,
    depth: u8,
    icc: Option<&[u8]>,
    metadata: &RasterMetadata,
    color: Option<RasterColor>,
) -> Result<()> {
    let mut encoder =
        TiffEncoder::new(&mut file)?.with_compression(Compression::Deflate(DeflateLevel::Fast));
    let directory_tags =
        metadata.write_subdirectories(&mut encoder, image.width, image.height, color)?;

    macro_rules! write_samples {
        ($format:ty, $data:expr) => {{
            let mut output = encoder.new_image::<$format>(image.width, image.height)?;
            let directory = output.encoder();
            metadata.write_root(directory, directory_tags)?;
            if let Some(icc) = icc {
                directory.write_tag(Tag::IccProfile, icc)?;
            }
            metadata.write_tiff_packets(directory)?;
            output.write_data($data)?;
        }};
    }

    match depth {
        8 => write_samples!(colortype::RGB8, &quantize8(image)),
        16 => write_samples!(colortype::RGB16, &quantize16(image).collect::<Vec<_>>()),
        _ => write_samples!(
            colortype::RGB32Float,
            &image.data.iter().copied().map(to_f32).collect::<Vec<_>>()
        ),
    }
    file.flush()?;
    Ok(())
}

fn write_png(
    mut file: BufWriter<File>,
    image: &ImageBuf,
    depth: u8,
    icc: Option<Vec<u8>>,
    metadata: &RasterMetadata,
    color: Option<RasterColor>,
) -> Result<()> {
    let mut info = png::Info::with_size(image.width, image.height);
    info.color_type = png::ColorType::Rgb;
    info.bit_depth = if depth == 8 {
        png::BitDepth::Eight
    } else {
        png::BitDepth::Sixteen
    };
    info.icc_profile = icc.map(Cow::Owned);
    info.exif_metadata = Some(Cow::Owned(metadata.exif_bytes(
        image.width,
        image.height,
        color,
    )?));
    let mut encoder = png::Encoder::with_info(&mut file, info)?;
    if let Some(text) = metadata
        .xmp
        .as_deref()
        .and_then(|bytes| std::str::from_utf8(bytes).ok())
    {
        encoder.add_itxt_chunk("XML:com.adobe.xmp".into(), text.into())?;
    }
    if let Some(profile) = metadata.png_iptc_text() {
        encoder.add_ztxt_chunk("Raw profile type iptc".into(), profile)?;
    }
    let mut output = encoder.write_header()?;
    if depth == 8 {
        output.write_image_data(&quantize8(image))?;
    } else {
        let bytes = quantize16(image)
            .flat_map(u16::to_be_bytes)
            .collect::<Vec<_>>();
        output.write_image_data(&bytes)?;
    }
    output.finish()?;
    file.flush()?;
    Ok(())
}

fn write_jpeg(
    mut file: BufWriter<File>,
    image: &ImageBuf,
    icc: Option<Vec<u8>>,
    metadata: &RasterMetadata,
    color: Option<RasterColor>,
) -> Result<()> {
    let mut bytes = Vec::new();
    let mut encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut bytes, 95);
    if let Some(icc) = icc {
        encoder.set_icc_profile(icc)?;
    }
    encoder.set_exif_metadata(metadata.exif_bytes(image.width, image.height, color)?)?;
    encoder.write_image(
        &quantize8(image),
        image.width,
        image.height,
        image::ExtendedColorType::Rgb8,
    )?;

    // Preserve the first APP0 marker when the JPEG encoder has written a JFIF header.
    let insertion = if bytes.get(2..4) == Some(&[0xff, 0xe0]) {
        4 + usize::from(u16::from_be_bytes([bytes[4], bytes[5]]))
    } else {
        2
    };
    file.write_all(&bytes[..insertion])?;
    metadata.write_jpeg_packets(&mut file)?;
    file.write_all(&bytes[insertion..])?;
    file.flush()?;
    Ok(())
}

fn write_exr<T: IntoSample>(
    path: &Path,
    image: &ImageBuf,
    color: Option<RasterColor>,
    convert: impl Fn(spektrafilm_math::precision::Scalar) -> T + Sync,
) -> Result<()> {
    let channels = SpecificChannels::rgb(|Vec2(x, y): Vec2<usize>| {
        let index = (y * image.width as usize + x) * 3;
        (
            convert(image.data[index]),
            convert(image.data[index + 1]),
            convert(image.data[index + 2]),
        )
    });
    let mut output = Image::from_encoded_channels(
        (image.width as usize, image.height as usize),
        Encoding::FAST_LOSSLESS,
        channels,
    );
    color::set_exr_color(&mut output.attributes, color);
    output.write().to_file(path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn floating_formats_preserve_extended_values_and_linear_color() -> Result<()> {
        let data = [-0.125, 1.25, 0.5].map(from_f64);
        let image = ImageBuf::from_data(1, 1, data.to_vec());
        for (extension, depth) in [("tiff", 32), ("exr", 16), ("exr", 32)] {
            let path = std::env::temp_dir().join(format!(
                "spektrafilm-float-{}-{depth}.{extension}",
                std::process::id()
            ));
            write_raster(
                &path,
                &image,
                &RasterWriteOptions {
                    bit_depth: Some(depth),
                    color_space: Some("ACES2065-1"),
                    ..RasterWriteOptions::default()
                },
            )?;
            let decoded = read_raster(&path);
            std::fs::remove_file(path)?;
            let decoded = decoded?;
            assert_eq!(decoded.image.data, data);
            assert_eq!(decoded.color, RasterColor::new("ACES2065-1", false));
        }
        Ok(())
    }

    #[test]
    fn vgamut_tiff_and_vlog_exr_keep_their_transfer_metadata() -> Result<()> {
        let data = [-0.125, 1.25, 0.5].map(from_f64);
        let image = ImageBuf::from_data(1, 1, data.to_vec());
        for (extension, cctf_encoding) in [("tiff", false), ("exr", true)] {
            let path = std::env::temp_dir().join(format!(
                "spektrafilm-vgamut-{}.{extension}",
                std::process::id()
            ));
            write_raster(
                &path,
                &image,
                &RasterWriteOptions {
                    bit_depth: Some(32),
                    color_space: Some("V-Gamut"),
                    cctf_encoding,
                    ..RasterWriteOptions::default()
                },
            )?;
            let decoded = read_raster(&path);
            std::fs::remove_file(path)?;
            let decoded = decoded?;
            assert_eq!(decoded.image.data, data);
            assert_eq!(decoded.color, RasterColor::new("V-Gamut", cctf_encoding));
        }
        Ok(())
    }

    #[test]
    fn integer_formats_clip_and_truncate_at_the_file_boundary() -> Result<()> {
        let image = ImageBuf::from_data(1, 1, [-0.125, 1.25, 0.5].map(from_f64).to_vec());
        for (extension, depth, midpoint) in [
            ("png", 8, 127.0 / 255.0),
            ("tiff", 8, 127.0 / 255.0),
            ("png", 16, 32767.0 / 65535.0),
            ("tiff", 16, 32767.0 / 65535.0),
        ] {
            let path = std::env::temp_dir().join(format!(
                "spektrafilm-integer-{}-{depth}.{extension}",
                std::process::id()
            ));
            write_raster(
                &path,
                &image,
                &RasterWriteOptions {
                    bit_depth: Some(depth),
                    color_space: Some("sRGB"),
                    cctf_encoding: true,
                    ..RasterWriteOptions::default()
                },
            )?;
            let decoded = read_raster(&path);
            std::fs::remove_file(path)?;
            let decoded = decoded?;
            assert_eq!(decoded.image.data, [0.0, 1.0, midpoint].map(from_f64));
            assert_eq!(decoded.color, RasterColor::new("sRGB", true));
        }
        Ok(())
    }
}
