// Write the pixels shared by the GUI and CLI to a linear ACES2065-1 float TIFF.

use std::path::Path;

use anyhow::{Result, anyhow};
use spektrafilm_io::{
    raster::{RasterWriteOptions, read_source_metadata, write_raster},
    raw,
};

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
    eprintln!(
        "[decode_raw_gui] decoded {}×{} → linear {}",
        image.width,
        image.height,
        raw::INPUT_COLOR_SPACE
    );

    write_raster(
        output,
        &image,
        &RasterWriteOptions {
            bit_depth: Some(32),
            color_space: Some(raw::INPUT_COLOR_SPACE),
            cctf_encoding: false,
            source_metadata: Some(&read_source_metadata(input)),
        },
    )?;
    eprintln!("[decode_raw_gui] wrote {}", output.display());
    Ok(())
}
