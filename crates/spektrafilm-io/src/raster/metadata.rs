use std::{
    borrow::Cow,
    fs::File,
    io::{BufReader, Cursor, Read, Seek, SeekFrom, Write},
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
const IPTC_NAA_TAG: u16 = 33723;
const JPEG_THUMB_OFFSET: u16 = 513;
const JPEG_THUMB_SIZE: u16 = 514;

type Tags = Vec<(u16, Value)>;

#[derive(Clone, Debug, Default)]
pub struct RasterMetadata {
    root: Tags,
    exif: Tags,
    gps: Tags,
    jpeg_thumbnail: Option<Vec<u8>>,
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
        metadata.photoshop = match format {
            Some(ImageFormat::Jpeg) => decoder.iptc_metadata().ok().flatten(),
            Some(ImageFormat::Png) => decoder
                .iptc_metadata()
                .ok()
                .flatten()
                .as_deref()
                .and_then(decode_png_iptc),
            _ => metadata.photoshop,
        };
        metadata
    }

    fn from_tiff(reader: &mut (impl Read + Seek)) -> Option<Self> {
        let decoder = GenericTiffReader::new(reader, 0, 0, Some(2), &[EXIF_IFD, GPS_IFD])
            .or_else(|_| GenericTiffReader::new(reader, 0, 0, Some(1), &[EXIF_IFD, GPS_IFD]))
            .ok()?;
        let root = decoder.chains().first()?;
        let packet = |tag| match &root.entries().get(&tag)?.value {
            Value::Byte(bytes) | Value::Undefined(bytes) => Some(bytes.clone()),
            _ => None,
        };
        let mut photoshop = packet(PHOTOSHOP_TAG);
        if photoshop.as_deref().and_then(iptc_resource).is_none() {
            if let Some(value) = root.entries().get(&IPTC_NAA_TAG) {
                let iim = match &value.value {
                    // TIFF stores IIM in a LONG array. Restore the bytes in
                    // the file's order without reversing individual words.
                    Value::Long(words) => Some(
                        words
                            .iter()
                            .flat_map(|&word| {
                                if decoder.little_endian() {
                                    word.to_le_bytes()
                                } else {
                                    word.to_be_bytes()
                                }
                            })
                            .collect::<Vec<_>>(),
                    ),
                    Value::Byte(bytes) | Value::Undefined(bytes) => Some(bytes.clone()),
                    _ => None,
                };
                if let Some(resource) = iim.as_deref().and_then(wrap_iim) {
                    photoshop.get_or_insert_with(Vec::new).extend(resource);
                }
            }
        }
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
            jpeg_thumbnail: decoder
                .chains()
                .get(1)
                .and_then(|ifd| read_thumbnail(reader, ifd)),
            xmp: packet(XMP_TAG),
            icc: packet(34675),
            photoshop,
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
        include_thumbnail: bool,
    ) -> Result<Vec<u8>> {
        let mut bytes = Cursor::new(Vec::new());
        let mut encoder = TiffEncoder::new(&mut bytes)?;
        let directories = self.write_subdirectories(&mut encoder, width, height, color)?;
        let mut root = encoder.image_directory()?;
        self.write_root(&mut root, directories)?;
        root.finish()?;
        if include_thumbnail {
            self.write_thumbnail(&mut encoder)?;
        }
        Ok(bytes.into_inner())
    }

    pub(super) fn write_thumbnail<W: Write + Seek>(
        &self,
        encoder: &mut TiffEncoder<W>,
    ) -> Result<()> {
        if let Some(bytes) = &self.jpeg_thumbnail {
            let mut directory = encoder.image_directory()?;
            let offset = u32::try_from(directory.write_data(bytes.as_slice())?)?;
            directory.write_tag(Tag::Compression, 6u16)?;
            directory.write_tag(Tag::Unknown(JPEG_THUMB_OFFSET), offset)?;
            directory.write_tag(Tag::Unknown(JPEG_THUMB_SIZE), u32::try_from(bytes.len())?)?;
            directory.finish()?;
        }
        Ok(())
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

    pub(super) fn png_iptc_text(&self) -> Option<String> {
        use std::fmt::Write as _;

        let data = iptc_resource(self.photoshop.as_deref()?)?;
        let mut text = format!("\niptc\n{:>8}\n", data.len());
        for line in data.chunks(36) {
            for byte in line {
                write!(&mut text, "{byte:02x}").ok()?;
            }
            text.push('\n');
        }
        Some(text)
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

fn read_thumbnail(reader: &mut (impl Read + Seek), directory: &IFD) -> Option<Vec<u8>> {
    let long = |tag| match &directory.entries().get(&tag)?.value {
        Value::Long(values) => values.first().copied().map(u64::from),
        Value::Short(values) => values.first().copied().map(u64::from),
        _ => None,
    };
    let offset = long(JPEG_THUMB_OFFSET)?;
    let length = long(JPEG_THUMB_SIZE)?;
    if length < 4 || offset.checked_add(length)? > reader.seek(SeekFrom::End(0)).ok()? {
        return None;
    }
    reader.seek(SeekFrom::Start(offset)).ok()?;
    let mut data = Vec::new();
    reader.take(length).read_to_end(&mut data).ok()?;
    (data.len() as u64 == length && data.starts_with(&[0xff, 0xd8])).then_some(data)
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

// Adobe image resource 0x0404 wraps the IPTC Information Interchange Model.
fn wrap_iim(data: &[u8]) -> Option<Vec<u8>> {
    if !data.starts_with(&[0x1c]) {
        return None;
    }
    let size = u32::try_from(data.len()).ok()?;
    let mut resource = b"8BIM\x04\x04\0\0".to_vec();
    resource.extend_from_slice(&size.to_be_bytes());
    resource.extend_from_slice(data);
    if size % 2 != 0 {
        resource.push(0);
    }
    Some(resource)
}

fn iptc_resource(mut resources: &[u8]) -> Option<&[u8]> {
    while resources.starts_with(b"8BIM") {
        // The Pascal name and payload each occupy an even number of bytes.
        let name_bytes = (1 + usize::from(*resources.get(6)?)).next_multiple_of(2);
        let length_start = 6 + name_bytes;
        let size = u32::from_be_bytes(
            resources
                .get(length_start..length_start + 4)?
                .try_into()
                .ok()?,
        );
        let end = (length_start + 4)
            .checked_add(usize::try_from(size).ok()?)?
            .checked_add(usize::from(size % 2 != 0))?;
        if resources.get(4..6)? == b"\x04\x04" {
            return resources.get(..end);
        }
        resources = resources.get(end..)?;
    }
    None
}

fn decode_png_iptc(text: &[u8]) -> Option<Vec<u8>> {
    // Exiv2 and ImageMagick store IPTC as a hex-encoded PNG text profile.
    let mut tokens = std::str::from_utf8(text).ok()?.split_ascii_whitespace();
    if !matches!(tokens.next()?, "iptc" | "8bim") {
        return None;
    }
    let len: usize = tokens.next()?.parse().ok()?;
    if len > text.len() / 2 {
        return None;
    }
    let mut bytes = Vec::with_capacity(len);
    for line in tokens {
        for pair in line.as_bytes().chunks(2) {
            if pair.len() != 2 {
                return None;
            }
            bytes.push(u8::from_str_radix(std::str::from_utf8(pair).ok()?, 16).ok()?);
        }
    }
    if bytes.len() != len {
        return None;
    }
    if iptc_resource(&bytes).is_some() {
        Some(bytes)
    } else {
        wrap_iim(&bytes)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jpeg_thumbnail_offset_is_relocated_in_the_new_exif_payload() -> Result<()> {
        let bytes = vec![0xff, 0xd8, 0xff, 0xd9];
        let input = RasterMetadata {
            jpeg_thumbnail: Some(bytes.clone()),
            ..RasterMetadata::default()
        };
        let included = input.exif_bytes(8, 6, None, true)?;
        let output = RasterMetadata::from_tiff(&mut Cursor::new(included)).unwrap();
        assert_eq!(output.jpeg_thumbnail, Some(bytes));

        let omitted = input.exif_bytes(8, 6, None, false)?;
        let output = RasterMetadata::from_tiff(&mut Cursor::new(omitted)).unwrap();
        assert!(output.jpeg_thumbnail.is_none());
        Ok(())
    }

    #[test]
    fn retains_long_iptc_in_tiff_and_png_text_profile() -> Result<()> {
        let mut bytes = Cursor::new(Vec::new());
        let mut encoder = TiffEncoder::new(&mut bytes)?;
        let mut frame = encoder.new_image::<tiff::encoder::colortype::RGB8>(1, 1)?;
        // IIM caption dataset (2:120), three bytes of text. The tag
        // stores the eight bytes as native-endian 32-bit words.
        let iim = b"\x1c\x02\x78\x00\x03abc";
        let words = [
            u32::from_ne_bytes([iim[0], iim[1], iim[2], iim[3]]),
            u32::from_ne_bytes([iim[4], iim[5], iim[6], iim[7]]),
        ];
        frame
            .encoder()
            .write_tag(Tag::Unknown(IPTC_NAA_TAG), &words[..])?;
        frame.write_data(&[0, 0, 0])?;
        bytes.set_position(0);

        let metadata = RasterMetadata::from_tiff(&mut bytes).unwrap();
        let resource = wrap_iim(iim).unwrap();
        assert_eq!(metadata.photoshop.as_deref(), Some(resource.as_slice()));
        let png_text = metadata.png_iptc_text().unwrap();
        assert_eq!(decode_png_iptc(png_text.as_bytes()), Some(resource));

        let mut jpeg_packet = Vec::new();
        metadata.write_jpeg_packets(&mut jpeg_packet)?;
        assert_eq!(&jpeg_packet[..2], &[0xff, 0xed]);
        assert_eq!(&jpeg_packet[4..18], b"Photoshop 3.0\0");
        assert_eq!(&jpeg_packet[30..], iim);
        Ok(())
    }
}
