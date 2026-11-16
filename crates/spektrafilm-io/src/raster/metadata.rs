use std::{
    borrow::Cow,
    fs::File,
    io::{BufReader, Cursor, Read, Seek, Write},
    path::Path,
};

use anyhow::Result;
use image::{ImageDecoder, ImageFormat, ImageReader};
use rawler::{
    decoders::{RawDecodeParams, RawMetadata},
    formats::tiff::{
        IFD, Value,
        reader::{GenericTiffReader, TiffReader},
        writer::TiffWriter,
    },
    rawsource::RawSource,
};
use tiff::{
    encoder::{DirectoryEncoder, Rational, SRational, TiffEncoder, TiffKindStandard, TiffValue},
    tags::{Tag, Type},
};

use super::RasterColor;

const EXIF_IFD: u16 = 0x8769;
const GPS_IFD: u16 = 0x8825;
const XMP_TAG: u16 = 700;
const PHOTOSHOP_TAG: u16 = 34377;

type Tags = Vec<(u16, Value)>;

#[derive(Clone, Debug, Default)]
pub struct RasterMetadata {
    root: Tags,
    exif: Tags,
    gps: Tags,
    pub(super) xmp: Option<Vec<u8>>,
    pub(super) icc: Option<Vec<u8>>,
    // JPEG APP13 and TIFF tag 34377 both use Photoshop Image Resources.
    photoshop: Option<Vec<u8>>,
}

#[derive(Clone, Copy)]
pub(super) struct Directories {
    exif: u32,
    gps: Option<u32>,
}

impl RasterMetadata {
    pub(super) fn from_decoder(
        path: &Path,
        format: Option<ImageFormat>,
        decoder: &mut dyn ImageDecoder,
    ) -> Self {
        let mut metadata = if format == Some(ImageFormat::Tiff) {
            File::open(path)
                .ok()
                .and_then(|file| Self::from_tiff(&mut BufReader::new(file)))
                .unwrap_or_default()
        } else {
            decoder
                .exif_metadata()
                .ok()
                .flatten()
                .and_then(|data| {
                    let tiff = data.strip_prefix(b"Exif\0\0").unwrap_or(&data);
                    Self::from_tiff(&mut Cursor::new(tiff))
                })
                .unwrap_or_default()
        };

        if metadata.xmp.is_none() {
            metadata.xmp = decoder.xmp_metadata().ok().flatten();
        }
        if format == Some(ImageFormat::Jpeg) {
            metadata.photoshop = decoder.iptc_metadata().ok().flatten();
        }
        metadata
    }

    fn from_tiff(reader: &mut (impl Read + Seek)) -> Option<Self> {
        let decoder = GenericTiffReader::new(reader, 0, 0, Some(1), &[EXIF_IFD, GPS_IFD]).ok()?;
        let root = decoder.chains().first()?;
        let packet = |tag| match &root.entries().get(&tag)?.value {
            Value::Byte(bytes) | Value::Undefined(bytes) => Some(bytes.clone()),
            _ => None,
        };
        Some(Self {
            // Descriptive IFD0 entries only. File layout offsets belong to the
            // new encoder, as do orientation, software and pixel dimensions.
            root: copy_tags(Some(root), |tag| {
                matches!(tag, 270..=272 | 306 | 315 | 33432)
            }),
            exif: copy_tags(root.get_sub_ifd(EXIF_IFD), |tag| {
                // MakerNote and interoperability IFD references can contain
                // offsets into the original file and cannot be moved verbatim.
                !matches!(tag, 37500 | 40961..=40965)
            }),
            gps: copy_tags(root.get_sub_ifd(GPS_IFD), |_| true),
            xmp: packet(XMP_TAG),
            icc: packet(34675),
            photoshop: packet(PHOTOSHOP_TAG),
        })
    }

    fn from_raw(metadata: &RawMetadata, xmp: Option<Vec<u8>>) -> Result<Self> {
        // Rawler normalizes vendor-specific capture metadata into its EXIF struct.
        let mut bytes = Cursor::new(Vec::new());
        let mut writer = TiffWriter::new(&mut bytes)?;
        let mut root = writer.new_directory();
        let mut exif = writer.new_directory();
        if !metadata.make.is_empty() {
            root.add_tag(271u16, metadata.make.as_str());
        }
        if !metadata.model.is_empty() {
            root.add_tag(272u16, metadata.model.as_str());
        }
        metadata.write_exif_tags(&mut writer, &mut root, &mut exif)?;
        if exif.entry_count() != 0 {
            root.add_tag(EXIF_IFD, [exif.build(&mut writer)?]);
        }
        writer.build(root)?;
        bytes.set_position(0);
        let mut output = Self::from_tiff(&mut bytes).unwrap_or_default();
        output.xmp = xmp;
        Ok(output)
    }

    pub(super) fn write_subdirectories<W: Write + Seek>(
        &self,
        encoder: &mut TiffEncoder<W>,
        width: u32,
        height: u32,
        color: Option<RasterColor>,
    ) -> Result<Directories> {
        let srgb = color.is_some_and(|color| color.color_space == "sRGB" && color.cctf_encoding);
        let interoperability = if srgb {
            let mut directory = encoder.extra_directory()?;
            directory.write_tag(Tag::Unknown(1), "R98")?;
            Some(directory.finish_with_offsets()?.offset)
        } else {
            None
        };
        let mut exif = encoder.extra_directory()?;
        write_tags(&mut exif, &self.exif)?;
        exif.write_tag(Tag::Unknown(40961), if srgb { 1u16 } else { 65535u16 })?;
        exif.write_tag(Tag::Unknown(40962), width)?;
        exif.write_tag(Tag::Unknown(40963), height)?;
        if let Some(interoperability) = interoperability {
            exif.write_tag(Tag::Unknown(40965), interoperability)?;
        }
        let exif = exif.finish_with_offsets()?.offset;
        let gps = if self.gps.is_empty() {
            None
        } else {
            let mut gps = encoder.extra_directory()?;
            write_tags(&mut gps, &self.gps)?;
            Some(gps.finish_with_offsets()?.offset)
        };
        Ok(Directories { exif, gps })
    }

    pub(super) fn write_root<W: Write + Seek>(
        &self,
        root: &mut DirectoryEncoder<'_, W, TiffKindStandard>,
        directories: Directories,
    ) -> Result<()> {
        write_tags(root, &self.root)?;
        root.write_tag(Tag::Orientation, 1u16)?;
        root.write_tag(Tag::Software, "spektrafilm")?;
        root.write_tag(Tag::ExifDirectory, directories.exif)?;
        if let Some(gps) = directories.gps {
            root.write_tag(Tag::Unknown(GPS_IFD), gps)?;
        }
        Ok(())
    }

    pub(super) fn exif_bytes(
        &self,
        width: u32,
        height: u32,
        color: Option<RasterColor>,
    ) -> Result<Vec<u8>> {
        let mut bytes = Cursor::new(Vec::new());
        let mut encoder = TiffEncoder::new(&mut bytes)?;
        let directories = self.write_subdirectories(&mut encoder, width, height, color)?;
        let mut root = encoder.image_directory()?;
        self.write_root(&mut root, directories)?;
        root.finish()?;
        Ok(bytes.into_inner())
    }

    pub(super) fn write_tiff_packets<W: Write + Seek>(
        &self,
        directory: &mut DirectoryEncoder<'_, W, TiffKindStandard>,
    ) -> Result<()> {
        if let Some(bytes) = &self.xmp {
            directory.write_tag(Tag::Unknown(XMP_TAG), bytes.as_slice())?;
        }
        if let Some(bytes) = &self.photoshop {
            directory.write_tag(Tag::Unknown(PHOTOSHOP_TAG), bytes.as_slice())?;
        }
        Ok(())
    }

    pub(super) fn write_jpeg_packets(&self, file: &mut impl Write) -> Result<()> {
        if let Some(bytes) = &self.xmp {
            write_jpeg_packet(file, 0xe1, b"http://ns.adobe.com/xap/1.0/\0", bytes)?;
        }
        if let Some(bytes) = &self.photoshop {
            write_jpeg_packet(file, 0xed, b"Photoshop 3.0\0", bytes)?;
        }
        Ok(())
    }
}

pub(super) fn read_source_metadata(path: &Path) -> RasterMetadata {
    // The RAW source may itself be a TIFF container (including DNG).
    // Ask the RAW decoder first to obtain the camera-normalized metadata.
    let raw = || -> Option<RasterMetadata> {
        let source = RawSource::new(path).ok()?;
        let decoder = rawler::get_decoder(&source).ok()?;
        let params = RawDecodeParams::default();
        let metadata = decoder.raw_metadata(&source, &params).ok()?;
        let xmp = decoder.xpacket(&source, &params).ok().flatten();
        RasterMetadata::from_raw(&metadata, xmp).ok()
    };
    let raster = || -> Option<RasterMetadata> {
        let reader = ImageReader::open(path).ok()?.with_guessed_format().ok()?;
        let format = reader.format();
        let mut decoder = reader.into_decoder().ok()?;
        Some(RasterMetadata::from_decoder(path, format, &mut decoder))
    };
    raw().or_else(raster).unwrap_or_default()
}

fn copy_tags(ifd: Option<&IFD>, mut retain: impl FnMut(u16) -> bool) -> Tags {
    ifd.into_iter()
        .flat_map(|directory| directory.entries())
        .filter(|(tag, _)| retain(**tag))
        .map(|(&tag, entry)| (tag, entry.value.clone()))
        .collect()
}

fn write_tags<W: Write + Seek>(
    directory: &mut DirectoryEncoder<'_, W, TiffKindStandard>,
    values: &Tags,
) -> Result<()> {
    for (number, value) in values {
        let tag = Tag::from_u16_exhaustive(*number);
        match value {
            Value::Ascii(strings) => {
                // TIFF ASCII is plain ASCII with a trailing NUL.
                if let Some(text) = strings
                    .strings()
                    .first()
                    .filter(|text| text.is_ascii() && !text.contains('\0'))
                {
                    directory.write_tag(tag, text.as_str())?;
                }
            }
            Value::Byte(v) => directory.write_tag(tag, v.as_slice())?,
            Value::Short(v) => directory.write_tag(tag, v.as_slice())?,
            Value::Long(v) => directory.write_tag(tag, v.as_slice())?,
            Value::SByte(v) => directory.write_tag(tag, v.as_slice())?,
            Value::SShort(v) => directory.write_tag(tag, v.as_slice())?,
            Value::SLong(v) => directory.write_tag(tag, v.as_slice())?,
            Value::Float(v) => directory.write_tag(tag, v.as_slice())?,
            Value::Double(v) => directory.write_tag(tag, v.as_slice())?,
            Value::Undefined(v) => directory.write_tag(tag, Undefined(v))?,
            Value::Rational(v) => {
                let entries = v
                    .iter()
                    .map(|v| Rational { n: v.n, d: v.d })
                    .collect::<Vec<_>>();
                directory.write_tag(tag, entries.as_slice())?;
            }
            Value::SRational(v) => {
                let entries = v
                    .iter()
                    .map(|v| SRational { n: v.n, d: v.d })
                    .collect::<Vec<_>>();
                directory.write_tag(tag, entries.as_slice())?;
            }
            Value::Unknown(_, _) => {}
        }
    }
    Ok(())
}

struct Undefined<'a>(&'a [u8]);

impl TiffValue for Undefined<'_> {
    const BYTE_LEN: u8 = 1;
    const FIELD_TYPE: Type = Type::UNDEFINED;

    fn count(&self) -> usize {
        self.0.len()
    }

    fn data(&self) -> Cow<'_, [u8]> {
        Cow::Borrowed(self.0)
    }
}

fn write_jpeg_packet(file: &mut impl Write, marker: u8, prefix: &[u8], data: &[u8]) -> Result<()> {
    if let Ok(length) = u16::try_from(2 + prefix.len() + data.len()) {
        file.write_all(&[0xff, marker])?;
        file.write_all(&length.to_be_bytes())?;
        file.write_all(prefix)?;
        file.write_all(data)?;
    }
    Ok(())
}
