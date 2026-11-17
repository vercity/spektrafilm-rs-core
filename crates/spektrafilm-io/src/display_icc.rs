use std::sync::Arc;

use anyhow::{Context, Result, ensure};
use moxcms::{ColorProfile, Layout, RenderingIntent, Transform8BitExecutor, TransformOptions};

/// Convert display-only sRGB RGBA pixels using a monitor's ICC profile.
/// Scene-referred and saved image buffers retain their own color spaces.
pub struct DisplayIcc {
    conversion: Arc<Transform8BitExecutor>,
}

impl DisplayIcc {
    pub fn from_bytes(profile: &[u8]) -> Result<Self> {
        let monitor = ColorProfile::new_from_slice(profile).context("reading display ICC")?;
        let conversion = ColorProfile::new_srgb()
            .create_transform_8bit(
                Layout::Rgba,
                &monitor,
                Layout::Rgba,
                TransformOptions {
                    rendering_intent: RenderingIntent::Perceptual,
                    allow_use_cicp_transfer: false,
                    prefer_fixed_point: false,
                    ..Default::default()
                },
            )
            .context("building sRGB to display ICC transform")?;
        Ok(Self { conversion })
    }

    pub fn transform_rgba(&self, source: &[u8]) -> Result<Vec<u8>> {
        ensure!(source.len() % 4 == 0, "display pixels must be RGBA");
        let mut destination = vec![0; source.len()];
        self.conversion
            .transform(source, &mut destination)
            .context("converting pixels to the display ICC")?;
        // Device profiles describe RGB components. Preserve the alpha channel
        // even for transform pipelines that treat the fourth channel as padding.
        for (source, destination) in source.chunks_exact(4).zip(destination.chunks_exact_mut(4)) {
            destination[3] = source[3];
        }
        Ok(destination)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profiles_transform_display_codes_and_leave_source_and_alpha_intact() {
        let bytes = ColorProfile::new_display_p3().encode().unwrap();
        let display = DisplayIcc::from_bytes(&bytes).unwrap();
        let original = [255, 0, 0, 10, 0, 128, 255, 25, 192, 192, 192, 250];
        let converted = display.transform_rgba(&original).unwrap();
        let expected = [234, 51, 35, 10, 55, 126, 246, 25, 192, 192, 192, 250];
        for (sample, expected) in converted.iter().zip(expected) {
            assert!(sample.abs_diff(expected) <= 1, "{sample} vs {expected}");
        }
        assert_ne!(converted, original);
        assert_eq!(
            original,
            [255, 0, 0, 10, 0, 128, 255, 25, 192, 192, 192, 250]
        );
        assert!(display.transform_rgba(&[1, 2, 3]).is_err());
        assert!(DisplayIcc::from_bytes(b"not an ICC").is_err());
    }
}
