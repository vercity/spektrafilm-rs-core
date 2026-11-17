use std::{
    fs::File,
    io::{BufReader, BufWriter, Read, Write},
    path::Path,
};

use anyhow::{Context, Result, ensure};
use spektrafilm_math::{
    image::ImageBuf,
    precision::{from_f64, to_f64},
};

const BYTES_PER_SAMPLE: usize = size_of::<f64>();
const HEADER_LEN: u64 = 8;
const BUFFER_LEN: usize = 16 * 1024;

/// Carry already developed pixels from the GUI to the f64 child without
/// quantizing the GUI's white balance, lens correction, or manual rotation.
pub fn write_developed_raw(path: &Path, image: &ImageBuf) -> Result<()> {
    let mut writer = BufWriter::new(File::create_new(path)?);
    writer.write_all(&image.width.to_le_bytes())?;
    writer.write_all(&image.height.to_le_bytes())?;
    let mut bytes = [0; BUFFER_LEN];
    for pixels in image.data.chunks(BUFFER_LEN / BYTES_PER_SAMPLE) {
        let prefix = &mut bytes[..pixels.len() * BYTES_PER_SAMPLE];
        for (pixel, sample) in prefix.chunks_exact_mut(BYTES_PER_SAMPLE).zip(pixels) {
            pixel.copy_from_slice(&to_f64(*sample).to_le_bytes());
        }
        writer.write_all(prefix)?;
    }
    writer.flush()?;
    Ok(())
}

/// Read the GUI's temporary linear ACES2065-1 buffer.
pub fn read_developed_raw(path: &Path) -> Result<ImageBuf> {
    let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let file_length = file.metadata()?.len();
    let mut reader = BufReader::new(file);
    let mut width_bytes = [0; 4];
    let mut height_bytes = [0; 4];
    reader.read_exact(&mut width_bytes)?;
    reader.read_exact(&mut height_bytes)?;
    let width = u32::from_le_bytes(width_bytes);
    let height = u32::from_le_bytes(height_bytes);
    let count = (width as usize)
        .checked_mul(height as usize)
        .and_then(|pixels| pixels.checked_mul(3))
        .context("developed RAW dimensions overflow")?;
    ensure!(count > 0, "developed RAW dimensions must be positive");
    let size = (count as u64)
        .checked_mul(BYTES_PER_SAMPLE as u64)
        .and_then(|size| size.checked_add(HEADER_LEN))
        .context("developed RAW size overflows")?;
    ensure!(file_length == size, "invalid developed RAW buffer length");
    let mut data = Vec::new();
    data.try_reserve_exact(count)
        .context("allocating developed RAW buffer")?;
    let mut bytes = [0; BUFFER_LEN];
    let mut remaining = count;
    while remaining > 0 {
        let chunk_samples = remaining.min(BUFFER_LEN / BYTES_PER_SAMPLE);
        let prefix = &mut bytes[..chunk_samples * BYTES_PER_SAMPLE];
        reader.read_exact(prefix)?;
        for sample in prefix.chunks_exact(BYTES_PER_SAMPLE) {
            let value = f64::from_le_bytes(sample.try_into().expect("fixed sample width"));
            data.push(from_f64(value));
        }
        remaining -= chunk_samples;
    }
    Ok(ImageBuf::from_data(width, height, data))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn developed_raw_roundtrips_dimensions_and_signed_samples() {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "spektrafilm-raw-{}-{stamp}.bin",
            std::process::id()
        ));
        let data = [
            -0.0039032678923,
            0.08291,
            1.2519257,
            0.0010818504317,
            0.0,
            -1.3,
        ]
        .map(from_f64)
        .to_vec();
        let image = ImageBuf::from_data(2, 1, data);
        write_developed_raw(&path, &image).unwrap();
        let read = read_developed_raw(&path).unwrap();
        assert_eq!((read.width, read.height), (image.width, image.height));
        assert_eq!(read.data, image.data);
        let handle = File::options().write(true).open(&path).unwrap();
        handle.set_len(HEADER_LEN).unwrap();
        assert!(read_developed_raw(&path).is_err());
        drop(handle);
        std::fs::remove_file(&path).unwrap();
    }
}
