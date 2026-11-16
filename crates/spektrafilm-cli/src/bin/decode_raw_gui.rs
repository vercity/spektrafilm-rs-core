// Write the pixels shared by the GUI and CLI to a linear ACES2065-1 float TIFF.

use std::fs::File;
use std::io::BufWriter;
use std::path::Path;

use anyhow::{Context, Result, anyhow};
use spektrafilm_io::raw;
use spektrafilm_math::precision::to_f32;
use tiff::encoder::{TiffEncoder, colortype::RGB32Float};

fn main() -> Result<()> {
    let mut args = std::env::args_os().skip(1);
    let input = args
        .next()
        .ok_or_else(|| anyhow!("usage: decode_raw_gui <input.raw> <output.tif>"))?;
    let output = args
        .next()
        .ok_or_else(|| anyhow!("usage: decode_raw_gui <input.raw> <output.tif>"))?;
    let input = Path::new(&input);
    let output = Path::new(&output);

    let image = raw::decode(input)?;
    let floats: Vec<f32> = image.data.into_iter().map(to_f32).collect();
    eprintln!(
        "[decode_raw_gui] decoded {}×{} → linear {}",
        image.width,
        image.height,
        raw::INPUT_COLOR_SPACE
    );

    let f = File::create(output).with_context(|| format!("creating {}", output.display()))?;
    let mut encoder = TiffEncoder::new(BufWriter::new(f)).context("init TiffEncoder")?;
    encoder
        .write_image::<RGB32Float>(image.width, image.height, &floats)
        .context("write TIFF image")?;
    eprintln!("[decode_raw_gui] wrote {}", output.display());
    Ok(())
}
