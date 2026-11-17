//! Named boundaries in the ordinary spatial rendering pipeline.
use std::time::Instant;

use serde::{Deserialize, Serialize};
use spektrafilm_gpu::ComputeBackend;
use spektrafilm_math::image::ImageBuf;
use thiserror::Error;

use super::{Pipeline, crop_working, print_stage_timing, stage_timings_enabled};
use crate::color_reference::ColorReference;
use crate::stages::{dump_if_env, filming, printing, scanning};

/// Select a starting or stopping point for a normal render.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RenderTap {
    RgbIn,
    RgbPre,
    LogEFilm,
    CmyFilm,
    LogEPrint,
    CmyPrint,
    RgbOut,
}

/// Samples at the requested boundary with their physical working scale.
#[derive(Debug)]
pub struct TappedImage {
    pub image: ImageBuf,
    /// Pass this value when resuming at an intermediate boundary.
    pub pixel_size_um: f32,
}

#[derive(Debug, Error, PartialEq)]
pub enum TapError {
    #[error("no rendering path from {inject:?} to {collect:?}")]
    Unreachable {
        inject: RenderTap,
        collect: RenderTap,
    },
    #[error("expected a non-empty {width}×{height}×3 image, got {samples} values")]
    InvalidImage {
        width: u32,
        height: u32,
        samples: usize,
    },
    #[error("pixel size must be finite and positive, got {0} µm")]
    InvalidPixelSize(f32),
}

impl Pipeline {
    /// Render between named stages without disabling spatial or stochastic effects.
    ///
    /// Collecting `RgbPre` returns the metered, cropped and rescaled input in
    /// its original encoding. Later taps accept physical log exposure or dye
    /// density samples and skip stages before the injection point. Intermediate
    /// injections may supply the working pixel size from an earlier result;
    /// otherwise it is inferred from the supplied image and film format.
    ///
    /// For whole-image GPU rendering with a single upload, use [Self::process].
    pub fn process_with_taps(
        &self,
        image: ImageBuf,
        inject: RenderTap,
        collect: RenderTap,
        pixel_size_um: Option<f32>,
        backend: &dyn ComputeBackend,
    ) -> Result<TappedImage, TapError> {
        use RenderTap::{RgbIn, RgbPre};

        let path = self.render_taps();
        let start = path
            .iter()
            .position(|tap| *tap == inject)
            .ok_or(TapError::Unreachable { inject, collect })?;
        let remaining = &path[start..];
        let end = remaining
            .iter()
            .position(|tap| *tap == collect)
            .ok_or(TapError::Unreachable { inject, collect })?;
        let expected = (image.width as usize)
            .checked_mul(image.height as usize)
            .and_then(|pixels| pixels.checked_mul(3));
        if image.width == 0 || image.height == 0 || expected != Some(image.data.len()) {
            return Err(TapError::InvalidImage {
                width: image.width,
                height: image.height,
                samples: image.data.len(),
            });
        }

        let (mut image, pixel_size_um, exposure_scale, next) = if inject == RgbIn && end > 0 {
            let start = Instant::now();
            let (image, pix_um, exposure_scale) = self.prepare_image(image);
            print_stage_timing(stage_timings_enabled(), "preprocess", start);
            (image, pix_um, exposure_scale, 2)
        } else {
            let pix_um = pixel_size_um.unwrap_or_else(|| self.pixel_size_um(&image));
            (image, pix_um, 1.0, 1)
        };
        if !pixel_size_um.is_finite() || pixel_size_um <= 0.0 {
            return Err(TapError::InvalidPixelSize(pixel_size_um));
        }
        if end == 0 || collect == RgbPre {
            filming::apply_autoexposure_scale(&mut image, exposure_scale);
            return Ok(TappedImage {
                image,
                pixel_size_um,
            });
        }
        let image = self.process_staged(
            image,
            &remaining[next..=end],
            backend,
            &self.color_reference(),
            pixel_size_um,
            exposure_scale,
        );
        Ok(TappedImage {
            image,
            pixel_size_um,
        })
    }

    pub(super) fn render_taps(&self) -> &'static [RenderTap] {
        use RenderTap::*;
        const FILM: &[RenderTap] = &[RgbIn, RgbPre, LogEFilm, CmyFilm, RgbOut];
        const PRINT: &[RenderTap] = &[
            RgbIn, RgbPre, LogEFilm, CmyFilm, LogEPrint, CmyPrint, RgbOut,
        ];
        if self.params.io.scan_film {
            FILM
        } else {
            PRINT
        }
    }

    pub(super) fn prepare_image(&self, image: ImageBuf) -> (ImageBuf, f32, f64) {
        // Meter the original frame and keep its physical scale when cropping.
        let exposure_scale = filming::autoexposure_scale(&image, &self.params);
        let scale = self.working_scale();
        let pixel_size_um = self.pixel_size_um(&image) / scale;
        let image = if self.params.io.crop {
            crop_working(image, self.params.io.crop_center, self.params.io.crop_size)
        } else {
            image
        };
        (
            Self::rescale_working(image, scale),
            pixel_size_um,
            exposure_scale,
        )
    }

    pub(super) fn color_reference(&self) -> ColorReference {
        ColorReference::compute_with_preflash(
            &self.film,
            &self.print,
            &self.params,
            &self.print_illuminant,
            self.print_exposure_factor,
            self.preflash_raw,
        )
    }

    pub(super) fn process_staged(
        &self,
        mut image: ImageBuf,
        remaining: &[RenderTap],
        backend: &dyn ComputeBackend,
        color_ref: &ColorReference,
        pixel_size_um: f32,
        exposure_scale: f64,
    ) -> ImageBuf {
        let stage_timings = stage_timings_enabled();
        for &tap in remaining {
            let start = Instant::now();
            image = match tap {
                RenderTap::RgbIn | RenderTap::RgbPre => unreachable!("already preprocessed"),
                RenderTap::LogEFilm => {
                    let raw = filming::expose_working(
                        &image,
                        &self.film,
                        &self.params,
                        backend,
                        self.tc_lut.as_ref(),
                        self.mallett_core.as_ref(),
                        filming::select_illuminant(&self.front_illuminant),
                        color_ref.filming_exposure_correction,
                        pixel_size_um,
                        exposure_scale,
                    );
                    print_stage_timing(stage_timings, "filming_expose", start);
                    dump_if_env("SPEKTRAFILM_DUMP_FILM_LOG_RAW", &raw);
                    raw
                }
                RenderTap::CmyFilm => {
                    let density = filming::develop_with_pixel_size(
                        &image,
                        &self.film,
                        &self.params,
                        backend,
                        pixel_size_um,
                    );
                    print_stage_timing(stage_timings, "filming_develop", start);
                    tracing::info!("pipeline: filming complete");
                    dump_if_env("SPEKTRAFILM_DUMP_FILM_DENSITY", &density);
                    density
                }
                RenderTap::LogEPrint => {
                    let raw = printing::expose_with_pixel_size(
                        &image,
                        &self.film,
                        &self.print,
                        &self.params,
                        backend,
                        &self.print_illuminant,
                        self.print_exposure_factor,
                        self.preflash_raw,
                        color_ref.printing_exposure_correction,
                        pixel_size_um,
                    );
                    print_stage_timing(stage_timings, "printing_expose", start);
                    dump_if_env("SPEKTRAFILM_DUMP_PRINT_LOG_RAW", &raw);
                    raw
                }
                RenderTap::CmyPrint => {
                    let density = printing::develop(&image, &self.print, &self.params, backend);
                    print_stage_timing(stage_timings, "printing_develop", start);
                    tracing::info!("pipeline: printing complete");
                    dump_if_env("SPEKTRAFILM_DUMP_PRINT_DENSITY", &density);
                    density
                }
                RenderTap::RgbOut => {
                    let stock = if self.params.io.scan_film {
                        &self.film
                    } else {
                        &self.print
                    };
                    let rgb = scanning::process(
                        &image,
                        stock,
                        &self.params,
                        backend,
                        color_ref,
                        &self.output_gamut,
                    );
                    print_stage_timing(stage_timings, "scanning", start);
                    tracing::info!("pipeline: scanning complete");
                    rgb
                }
            };
        }
        image
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::{params::RuntimeParams, profile::load_profile_by_name};
    use spektrafilm_gpu::cpu_backend::CpuBackend;
    use spektrafilm_math::precision::from_f64;

    fn scene() -> ImageBuf {
        let image = (0..8)
            .flat_map(|y| {
                (0..12).flat_map(move |x| {
                    [
                        0.08 + f64::from(x) * 0.03,
                        0.10 + f64::from(y) * 0.035,
                        0.05 + f64::from((x + y) % 2) * 0.3,
                    ]
                    .map(from_f64)
                })
            })
            .collect();
        ImageBuf::from_data(12, 8, image)
    }

    fn params(scan_film: bool) -> RuntimeParams {
        let mut params = RuntimeParams::default();
        params.io.input_color_space = "sRGB".into();
        params.io.input_cctf_decoding = true;
        params.io.output_cctf_encoding = false;
        params.io.scan_film = scan_film;
        params.io.crop = true;
        params.io.crop_size = [0.75, 0.5];
        params.io.upscale_factor = 1.5;
        params.camera.auto_exposure = true;
        params.camera.film_format_mm = 0.24;
        params.camera.lens_blur_um = 15.0;
        params.enlarger.lens_blur = 8.0;
        params.enlarger.preflash_exposure = 0.03;
        params.film_render.grain.active = false;
        params.film_render.glare.active = false;
        params.print_render.glare.active = false;
        params.scanner.white_correction = true;
        params.scanner.black_correction = true;
        params
    }

    fn pipeline(scan_film: bool) -> Pipeline {
        let data = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../data");
        let stock = if scan_film {
            "fujifilm_provia_100f"
        } else {
            "kodak_portra_400"
        };
        Pipeline::new_with_spectral(
            load_profile_by_name(&data, stock).unwrap(),
            load_profile_by_name(&data, "kodak_portra_endura").unwrap(),
            params(scan_film),
            &data,
        )
        .unwrap()
    }

    #[test]
    fn ordinary_render_resumes_at_each_tap_with_original_working_scale() {
        use RenderTap::*;
        for scan_film in [false, true] {
            let pipeline = pipeline(scan_film);
            let image = scene();
            let full = pipeline.process(image.clone(), &CpuBackend);
            for &tap in &pipeline.render_taps()[1..] {
                let tapped = pipeline
                    .process_with_taps(image.clone(), RgbIn, tap, None, &CpuBackend)
                    .unwrap();
                assert_eq!((tapped.image.width, tapped.image.height), (14, 9));
                assert!((tapped.pixel_size_um - 13.333_333).abs() < 1e-5);
                let resumed = pipeline
                    .process_with_taps(
                        tapped.image,
                        tap,
                        RgbOut,
                        Some(tapped.pixel_size_um),
                        &CpuBackend,
                    )
                    .unwrap();
                assert_eq!(resumed.image.data, full.data, "{tap:?}, scan={scan_film}");
            }
            if scan_film {
                assert_eq!(
                    pipeline
                        .process_with_taps(image, RgbIn, CmyPrint, None, &CpuBackend)
                        .unwrap_err(),
                    TapError::Unreachable {
                        inject: RgbIn,
                        collect: CmyPrint,
                    }
                );
            }
        }
    }
}
