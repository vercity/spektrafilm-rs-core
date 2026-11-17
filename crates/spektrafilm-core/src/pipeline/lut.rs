use std::path::Path;

use spektrafilm_gpu::cpu_backend::CpuBackend;
use spektrafilm_math::image::ImageBuf;
use thiserror::Error;

use super::Pipeline;
use crate::color_reference::ColorReference;
use crate::params::RuntimeParams;
use crate::profile::Profile;
use crate::stages::{filming, printing, scanning};

/// Invalid input buffer or a non-finite result from a LUT stage.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum LutSampleError {
    #[error("expected a non-empty {width}×{height}×3 sample buffer, got {samples} values")]
    InvalidShape {
        width: u32,
        height: u32,
        samples: usize,
    },
    #[error("non-finite LUT sample at channel index {0}")]
    NonFiniteSample(usize),
}

fn validate_samples(image: &ImageBuf) -> Result<(), LutSampleError> {
    let expected = (image.width as usize)
        .checked_mul(image.height as usize)
        .and_then(|pixels| pixels.checked_mul(3));
    if image.width == 0 || image.height == 0 || expected != Some(image.data.len()) {
        return Err(LutSampleError::InvalidShape {
            width: image.width,
            height: image.height,
            samples: image.data.len(),
        });
    }
    if let Some(index) = image.data.iter().position(|value| !value.is_finite()) {
        return Err(LutSampleError::NonFiniteSample(index));
    }
    Ok(())
}

/// Developed film CMY densities relative to base plus fog.
///
/// Values are physical densities with signed headroom. The LUT transport
/// decides how to normalize them for a cube file.
#[derive(Clone, Debug)]
pub struct FilmCmy(ImageBuf);

impl FilmCmy {
    pub fn new(density: ImageBuf) -> Result<Self, LutSampleError> {
        validate_samples(&density)?;
        Ok(Self(density))
    }

    pub fn as_image(&self) -> &ImageBuf {
        &self.0
    }
}

/// Calibrated CPU pipeline for deterministic LUT sampling.
///
/// RGB samples use the configured input and output spaces and transfer flags.
/// For physical linear RGB taps, disable both CCTF flags and handle transport
/// encoding and input gain outside this pipeline.
pub struct LutPipeline(Pipeline);

impl LutPipeline {
    pub fn new(
        film: Profile,
        print: Profile,
        mut params: RuntimeParams,
        data_dir: &Path,
    ) -> Result<Self, String> {
        // Python's LUT mode keeps the pointwise DIR transform and calibrated
        // print reference, including preflash. Image-wide and spatial effects
        // would change the meaning of a sample when reframed or reordered.
        params.io.crop = false;
        params.io.upscale_factor = 1.0;
        params.settings.preview_mode = false;
        params.camera.auto_exposure = false;
        params.camera.exposure_compensation_ev = 0.0;
        params.camera.lens_blur_um = 0.0;
        params.camera.diffusion_filter.active = false;
        params.enlarger.print_exposure = 1.0;
        params.enlarger.print_exposure_compensation = false;
        params.enlarger.lens_blur = 0.0;
        params.enlarger.diffusion_filter.active = false;
        params.film_render.halation.active = false;
        params.film_render.halation.boost_ev = 0.0;
        params.film_render.grain.active = false;
        params.film_render.glare.active = false;
        params.film_render.dir_couplers.diffusion_size_um = 0.0;
        params.film_render.dir_couplers.diffusion_tail_um = 0.0;
        params.print_render.glare.active = false;
        params.scanner.white_correction = false;
        params.scanner.black_correction = false;
        params.scanner.lens_blur = 0.0;
        params.scanner.unsharp_mask = [0.0, 0.0];
        Pipeline::new_with_spectral(film, print, params, data_dir).map(Self)
    }

    /// Collect `cmy_film` from `rgb_in`, before exposing the print.
    pub fn collect_film_cmy(&self, rgb: &ImageBuf) -> Result<FilmCmy, LutSampleError> {
        validate_samples(rgb)?;
        let pipeline = &self.0;
        let log_raw = filming::expose(
            rgb,
            &pipeline.film,
            &pipeline.params,
            &CpuBackend,
            pipeline.tc_lut(),
            pipeline.mallett_core.as_ref(),
            filming::select_illuminant(&pipeline.front_illuminant),
            1.0,
        );
        FilmCmy::new(filming::develop(
            &log_raw,
            &pipeline.film,
            &pipeline.params,
            &CpuBackend,
        ))
    }

    /// Inject physical `cmy_film` and collect `rgb_out` with no file clipping.
    pub fn inject_film_cmy(&self, cmy: &FilmCmy) -> Result<ImageBuf, LutSampleError> {
        let pipeline = &self.0;
        let color_ref = ColorReference::identity();
        let scan = |density, profile| {
            scanning::process(
                density,
                profile,
                &pipeline.params,
                &CpuBackend,
                &color_ref,
                &pipeline.output_gamut,
            )
        };
        let rgb = if pipeline.params.io.scan_film {
            scan(cmy.as_image(), &pipeline.film)
        } else {
            let paper_cmy = printing::process_with_calibration(
                cmy.as_image(),
                &pipeline.film,
                &pipeline.print,
                &pipeline.params,
                &CpuBackend,
                pipeline.print_illuminant_slice(),
                pipeline.print_exposure_factor(),
                pipeline.preflash_raw(),
                color_ref.printing_exposure_correction,
            );
            scan(&paper_cmy, &pipeline.print)
        };
        validate_samples(&rgb)?;
        Ok(rgb)
    }

    /// Run the same two stages as one RGB-to-RGB transform.
    pub fn process(&self, rgb: &ImageBuf) -> Result<ImageBuf, LutSampleError> {
        self.inject_film_cmy(&self.collect_film_cmy(rgb)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profile::load_profile_by_name;
    use spektrafilm_math::precision::{from_f64, to_f64};

    fn image<const N: usize>(width: u32, height: u32, samples: [[f64; 3]; N]) -> ImageBuf {
        let data = samples.into_iter().flatten().map(from_f64).collect();
        ImageBuf::from_data(width, height, data)
    }

    fn compare_samples<const N: usize>(actual: &ImageBuf, expected: [[f64; 3]; N], stage: &str) {
        assert_eq!(actual.pixel_count(), N);
        let max_error = actual
            .data
            .iter()
            .map(|&sample| to_f64(sample))
            .zip(expected.into_iter().flatten())
            .map(|(actual, expected)| (actual - expected).abs())
            .fold(0.0_f64, f64::max);
        eprintln!("{stage} maximum absolute error vs Python: {max_error:.3e}");
        assert!(
            max_error < 1e-6,
            "{stage} differs from Python by {max_error}"
        );
    }

    #[test]
    fn two_lut_stages_are_pointwise_and_match_python() {
        let data_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../data");
        let film = load_profile_by_name(&data_dir, "kodak_portra_400").unwrap();
        let mut params = RuntimeParams::default();
        params.io.input_color_space = "sRGB".into();
        params.io.output_cctf_encoding = false;
        params.enlarger.preflash_exposure = 0.0625;
        params.enlarger.preflash_y_filter_shift = 4.0;
        params.enlarger.preflash_m_filter_shift = 8.0;
        // The LUT constructor overrides photo framing and exposure settings.
        params.io.crop = true;
        params.io.upscale_factor = 0.5;
        params.camera.exposure_compensation_ev = 2.0;
        params.scanner.black_correction = true;
        params.scanner.white_correction = true;

        let make_pipeline = |paper| {
            let print = load_profile_by_name(&data_dir, paper).unwrap();
            LutPipeline::new(film.clone(), print, params.clone(), &data_dir).unwrap()
        };
        let primary = make_pipeline("kodak_portra_endura");
        let alternate = make_pipeline("kodak_2393");
        let rgb = image(
            3,
            2,
            [
                [0.1875, 0.1875, 0.1875],
                [0.75, 0.125, 0.015625],
                [-0.03125, 0.25, 0.5],
                [1.5, 1.0, 0.5],
                [0.0, 0.0, 0.0],
                [0.015625, 0.5, 0.75],
            ],
        );

        // Physical stages from the local Python 0.3.4 fixture with
        // debug.lut_mode, pointwise DIR and no input/output CCTF.
        let cmy = primary.collect_film_cmy(&rgb).unwrap();
        assert_eq!(
            (cmy.as_image().width, cmy.as_image().height),
            (rgb.width, rgb.height)
        );
        compare_samples(
            cmy.as_image(),
            [
                [0.696720400075, 0.708674929194, 0.837112651845],
                [1.100508627286, 0.572811710920, 0.196181495342],
                [0.426654590298, 0.858630093787, 1.254948864566],
                [1.208818269087, 1.058360212950, 0.971615535788],
                [0.0, 0.0, 0.0],
                [0.633008374377, 1.004543173955, 1.312694599567],
            ],
            "film CMY",
        );
        assert_eq!(
            cmy.as_image().data,
            alternate.collect_film_cmy(&rgb).unwrap().as_image().data
        );

        let primary_rgb = primary.inject_film_cmy(&cmy).unwrap();
        let alternate_rgb = alternate.inject_film_cmy(&cmy).unwrap();
        compare_samples(
            &primary_rgb,
            [
                [0.063979779895, 0.066785832268, 0.060266380281],
                [0.204279664396, 0.033268102820, 0.007876702387],
                [0.006956154210, 0.102606715641, 0.158670626369],
                [0.330796114214, 0.252173440676, 0.129240829279],
                [0.003738237342, 0.004088777651, 0.005425813155],
                [0.027159811943, 0.184774561454, 0.233422453029],
            ],
            "Portra Endura RGB",
        );
        compare_samples(
            &alternate_rgb,
            [
                [0.099312411299, 0.114005708251, 0.109552557770],
                [0.296501490449, 0.049833154648, 0.003164821970],
                [0.006272914286, 0.156398989171, 0.224476449785],
                [0.623218293537, 0.458765653056, 0.242714863512],
                [0.000063829040, 0.000058805063, 0.000016635461],
                [0.043470105791, 0.325583553368, 0.388335009430],
            ],
            "2393 RGB",
        );
        assert_eq!(primary.process(&rgb).unwrap().data, primary_rgb.data);
        assert_eq!(
            primary.0.process(rgb.clone(), &CpuBackend).data,
            primary_rgb.data,
            "LUT split must use the same calibrated math as the full photo path"
        );

        let reversed = ImageBuf::from_data(
            rgb.width,
            rgb.height,
            rgb.data.chunks_exact(3).rev().flatten().copied().collect(),
        );
        let reversed_rgb = primary.process(&reversed).unwrap();
        for ((input, batch), reversed) in rgb
            .pixels()
            .zip(primary_rgb.pixels())
            .zip(reversed_rgb.data.chunks_exact(3).rev())
        {
            let single = primary
                .process(&ImageBuf::from_data(1, 1, input.to_vec()))
                .unwrap();
            for (actual, expected) in single
                .data
                .iter()
                .zip(batch)
                .chain(reversed.iter().zip(batch))
            {
                assert!((to_f64(*actual) - to_f64(*expected)).abs() < 1e-6);
            }
        }

        let signed = FilmCmy::new(image(1, 1, [[-0.1875, -0.0625, 0.25]])).unwrap();
        compare_samples(
            &primary.inject_film_cmy(&signed).unwrap(),
            [[0.002972186270, 0.003843016747, 0.005848866145]],
            "signed CMY headroom",
        );
    }

    #[test]
    fn invalid_cmy_buffers_fail_before_stage_dispatch() {
        for (width, height) in [(0, 2), (2, 2), (u32::MAX, u32::MAX)] {
            let density = ImageBuf {
                width,
                height,
                data: vec![],
            };
            assert_eq!(
                FilmCmy::new(density).unwrap_err(),
                LutSampleError::InvalidShape {
                    width,
                    height,
                    samples: 0
                }
            );
        }
        let density = image(1, 1, [[0.0, f64::NAN, 1.0]]);
        assert_eq!(
            FilmCmy::new(density).unwrap_err(),
            LutSampleError::NonFiniteSample(1)
        );
    }
}
