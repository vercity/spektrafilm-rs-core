use std::path::Path;

use anyhow::Result;

use crate::{
    raster::{RasterColor, RasterImage, read_raster, read_source_metadata},
    raw::{self, WhiteBalance},
};

/// Load one frame and identify the color encoding the renderer should consume.
/// Untagged TIFF retains the caller's configured input color space.
pub fn read_image(path: &Path, raw_white_balance: WhiteBalance) -> Result<RasterImage> {
    if raw::is_raw(path) {
        return Ok(RasterImage {
            image: raw::decode_with_white_balance(path, raw_white_balance)?,
            metadata: read_source_metadata(path),
            color: Some(RasterColor {
                color_space: raw::INPUT_COLOR_SPACE,
                cctf_encoding: false,
            }),
        });
    }

    let mut loaded = read_raster(path)?;
    if loaded.color.is_none() {
        let extension = path
            .extension()
            .and_then(|ext| ext.to_str())
            .unwrap_or_default()
            .to_ascii_lowercase();
        loaded.color = match extension.as_str() {
            "png" | "jpg" | "jpeg" => Some(RasterColor {
                color_space: "sRGB",
                cctf_encoding: true,
            }),
            "exr" => Some(RasterColor {
                color_space: "sRGB",
                cctf_encoding: false,
            }),
            _ => None,
        };
    }
    Ok(loaded)
}
