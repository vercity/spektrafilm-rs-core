/// 3-stage simulation pipeline: Filming → Printing → Scanning.
///
/// Full calibration chain:
///   1. Load spectra LUT → compute TC LUT for film sensitivity
///   2. Process virtual gray card through filming to get midgray spectral density
///   3. Compute print exposure normalization factor from midgray spectral density
///   4. Pass all calibration data to the pipeline stages
use std::borrow::Cow;
use std::path::Path;
use std::time::Instant;

use spektrafilm_gpu::ComputeBackend;
use spektrafilm_math::image::ImageBuf;
use spektrafilm_math::spectral::TcLut;

mod lut;
mod runtime_taps;
pub use lut::{FilmCmy, FilmLogExposure, LutPipeline, LutSampleError, PrintLogExposure};
pub use runtime_taps::{RenderTap, TapError, TappedImage};

fn stage_timings_enabled() -> bool {
    std::env::var_os("SPEKTRAFILM_STAGE_TIMINGS").is_some()
}

fn print_stage_timing(enabled: bool, stage: &str, start: Instant) {
    if enabled {
        eprintln!("stage {stage}: {} ms", start.elapsed().as_millis());
    }
}

fn crop_working(image: ImageBuf, center: [f32; 2], size: [f32; 2]) -> ImageBuf {
    if image.pixel_count() == 0 {
        return image;
    }
    let long_side = image.width.max(image.height) as f64;
    let axis = |extent: u32, center: f32, size: f32| {
        let length = ((long_side * size as f64).round_ties_even() as u32).clamp(1, extent);
        let midpoint = (extent as f64 * center as f64).round_ties_even();
        let start = (midpoint - length as f64 / 2.0).round_ties_even() as u32;
        (start.min(extent - length), length)
    };
    let (x, width) = axis(image.width, center[0], size[0]);
    let (y, height) = axis(image.height, center[1], size[1]);
    if width == image.width && height == image.height {
        return image;
    }
    let start = x as usize * 3;
    let end = start + width as usize * 3;
    let mut pixels = Vec::with_capacity(width as usize * height as usize * 3);
    for row in image.rows().skip(y as usize).take(height as usize) {
        pixels.extend_from_slice(&row[start..end]);
    }
    ImageBuf::from_data(width, height, pixels)
}

use crate::enlarger;
use crate::params::RuntimeParams;
use crate::profile::Profile;
use crate::spectral_service;
use crate::stages::{self, filming::select_illuminant};

fn apply_film_specific_params(film: &Profile, params: &mut RuntimeParams) {
    // Monochrome grain is derived from the film, never user-set — clear it
    // for colour so a stray params file can't correlate the colour channels'
    // noise.
    params.film_render.grain.monochrome = film.is_bw();
    if film.is_bw() {
        // Match the grain and halation settings to the emulsion's single layer.
        let g = &mut params.film_render.grain;
        g.agx_particle_scale = [g.agx_particle_scale[0]; 3];
        g.density_min = [g.density_min[0]; 3];
        g.uniformity = [g.uniformity[0]; 3];
        let h = &mut params.film_render.halation;
        h.scatter_core_um = [h.scatter_core_um[0]; 3];
        h.scatter_tail_um = [h.scatter_tail_um[0]; 3];
        h.scatter_tail_weight = [h.scatter_tail_weight[0]; 3];
    }
}

#[derive(Clone)]
pub struct Pipeline {
    pub film: Profile,
    pub print: Profile,
    pub params: RuntimeParams,
    tc_lut: Option<TcLut>,
    /// Mallett2019 reflectance-basis core matrix (linear-sRGB → raw), when that
    /// upsampler is selected instead of the hanatos2025 tc LUT. Mutually
    /// exclusive with `tc_lut`.
    mallett_core: Option<[[f64; 3]; 3]>,
    /// Illuminant used by the RGB→tc projection before TC LUT lookup.
    /// Most methods use the film reference illuminant; arctic2026alpha02
    /// recovers reflectance in D65 coordinates and relights separately.
    front_illuminant: String,
    /// Print exposure normalization factor (1/geomean of midgray raw through enlarger).
    print_exposure_factor: f64,
    /// Filtered enlarger illuminant for printing stage (f64 for Python parity).
    print_illuminant: Vec<f64>,
    /// Constant preflash raw 3-vector added to the print exposure before the
    /// inner log10. `[0; 3]` unless the enlarger preflash is active.
    preflash_raw: [f64; 3],
    /// Output gamut compressor (built once; identity unless oklch is enabled).
    output_gamut: crate::gamut_compression::OutputGamutCompress,
}

impl Pipeline {
    /// Accessor for the pre-computed TC LUT (used by parity tests).
    pub fn tc_lut(&self) -> Option<&TcLut> {
        self.tc_lut.as_ref()
    }
    /// Accessor for the print exposure factor (parity tests).
    pub fn print_exposure_factor(&self) -> f64 {
        self.print_exposure_factor
    }
    /// Accessor for the filtered print illuminant (parity tests).
    pub fn print_illuminant_slice(&self) -> &[f64] {
        &self.print_illuminant
    }
    /// Accessor for the constant preflash raw vector (parity tests / debug).
    pub fn preflash_raw(&self) -> [f64; 3] {
        self.preflash_raw
    }

    /// Return a copy of this calibrated pipeline with updated runtime params.
    /// Callers must only use this when the calibration-affecting inputs match
    /// the pipeline that was originally built.
    pub fn with_params(mut self, params: RuntimeParams) -> Self {
        let mut params = params;
        apply_film_specific_params(&self.film, &mut params);
        self.output_gamut = crate::gamut_compression::OutputGamutCompress::build(
            &params.io.output_gamut_compress,
            &params.io.output_color_space,
        );
        self.params = params;
        self
    }
}

impl Pipeline {
    /// Create pipeline without spectral LUT (simplified path).
    pub fn new(film: Profile, print: Profile, params: RuntimeParams) -> Self {
        // B&W profiles must be resolved on every construction path —
        // an unresolved family would read development-time columns as
        // R/G/B channels.
        let film = crate::profile::resolve_for_render(film, params.film_render.development_time);
        let print = crate::profile::resolve_for_render(print, params.print_render.development_time);
        let print_illuminant = enlarger::enlarger_filtered_illuminant_f64(
            &params.enlarger.illuminant,
            params.enlarger.c_filter_neutral as f64,
            (params.enlarger.m_filter_neutral + params.enlarger.m_filter_shift) as f64,
            (params.enlarger.y_filter_neutral + params.enlarger.y_filter_shift) as f64,
        );
        let output_gamut = crate::gamut_compression::OutputGamutCompress::build(
            &params.io.output_gamut_compress,
            &params.io.output_color_space,
        );
        let front_illuminant = film.info.reference_illuminant.clone();
        Self {
            film,
            print,
            params,
            tc_lut: None,
            mallett_core: None,
            front_illuminant,
            print_exposure_factor: 1.0,
            print_illuminant,
            preflash_raw: [0.0; 3],
            output_gamut,
        }
    }

    /// Create pipeline with full Hanatos2025 spectral upsampling and calibration.
    pub fn new_with_spectral(
        film: Profile,
        print: Profile,
        mut params: RuntimeParams,
        data_dir: &Path,
    ) -> Result<Self, String> {
        // B&W profiles: collapse the development-time family to the selected
        // time and broadcast the single channel onto the 3-channel engine
        // layout. Must happen before anything reads the profile data.
        let film = crate::profile::resolve_for_render(film, params.film_render.development_time);
        let print = crate::profile::resolve_for_render(print, params.print_render.development_time);

        apply_film_specific_params(&film, &mut params);

        // Python parity: look up per-(print, illuminant, film) neutral filter values from
        // the JSON database — matches `apply_database_neutral_print_filters`. Defaults to
        // params.enlarger.{c,m,y}_filter_neutral when the combo isn't in the database.
        // Keep the f64 lookup values around (params is f32) — narrowing to
        // f32 here costs ~4e-8 precision through the `10^(-cc/100)` step.
        let mut neutral_cmy_f64: Option<[f64; 3]> = None;
        if params.settings.neutral_print_filters_from_database {
            let db = crate::neutral_filters::NeutralFilters::load(data_dir);
            let print_stock = print.info.stock.as_deref().unwrap_or("");
            let film_stock = film.info.stock.as_deref().unwrap_or("");
            if let Some([c, m, y]) = db.lookup(print_stock, &params.enlarger.illuminant, film_stock)
            {
                params.enlarger.c_filter_neutral = c as f32;
                params.enlarger.m_filter_neutral = m as f32;
                params.enlarger.y_filter_neutral = y as f32;
                neutral_cmy_f64 = Some([c, m, y]);
            }
        }
        let cmy_f64 = neutral_cmy_f64.unwrap_or([
            params.enlarger.c_filter_neutral as f64,
            params.enlarger.m_filter_neutral as f64,
            params.enlarger.y_filter_neutral as f64,
        ]);
        let (c_neutral_f64, m_neutral_f64, y_neutral_f64) = (cmy_f64[0], cmy_f64[1], cmy_f64[2]);

        // Film sensitivity: Python `sensitivity = np.nan_to_num(10 ** log_sensitivity)` — f64.
        let log_sens = film.log_sensitivity_f64();
        let mut sensitivity: Vec<[f64; 3]> = log_sens
            .iter()
            .map(|row| {
                let mut out = [0.0f64; 3];
                for c in 0..3 {
                    let v = 10.0f64.powf(row[c]);
                    out[c] = if v.is_nan() { 0.0 } else { v };
                }
                out
            })
            .collect();

        let ref_illuminant = select_illuminant(&film.info.reference_illuminant);
        let ref_illuminant_f64 = select_illuminant_f64(&film.info.reference_illuminant);
        spectral_service::apply_camera_bandpass(
            &mut sensitivity,
            ref_illuminant_f64,
            params.camera.filter_uv,
            params.camera.filter_ir,
        );
        let mut front_illuminant = film.info.reference_illuminant.clone();

        // RGB → film raw upsampler. Default `hanatos2025` builds the spectral tc
        // LUT; `mallett2019` builds a 3×3 reflectance-basis matrix instead (no
        // LUT). Dispatch mirrors Python `_rgb_to_film_raw`.
        let (tc_lut, mallett_core): (Option<TcLut>, Option<[[f64; 3]; 3]>) =
            match params.settings.rgb_to_raw_method.as_str() {
                "hanatos2025" => {
                    let spectra_lut = spectral_service::load_spectra_lut(data_dir)?;
                    let window_params = &film.data.hanatos2025_adaptation_window_params;
                    let blur_sigma = f64::from(params.settings.spectral_gaussian_blur);
                    let mut tc_lut = if params.settings.apply_hanatos2025_adaptation_window
                        && window_params.len() >= 4
                    {
                        spectral_service::windowed_tc_lut(
                            &spectra_lut,
                            &sensitivity,
                            window_params,
                            ref_illuminant_f64,
                            blur_sigma,
                        )
                    } else if blur_sigma > 0.0 {
                        let mut blurred = sensitivity.clone();
                        spectral_service::blur_spectral_sensitivity(&mut blurred, blur_sigma);
                        spectral_service::compute_tc_lut(&spectra_lut, &blurred)
                    } else {
                        spectral_service::compute_tc_lut(&spectra_lut, &sensitivity)
                    };
                    let white_xy = spektrafilm_math::spectral::illuminant_to_xy(ref_illuminant);
                    if params.settings.apply_hanatos2025_adaptation_surface {
                        spectral_service::apply_exposure_surface(
                            &mut tc_lut,
                            &film.data.hanatos2025_adaptation_surface_params,
                            white_xy,
                        )?;
                    }

                    // Bake input gamut compression into the LUT so the per-pixel
                    // path stays compression-agnostic. The radial mode uses the
                    // film reference illuminant as its achromatic axis.
                    let input_gamut = crate::input_gamut::InputGamutCompress::build(
                        params.io.input_gamut_compress.effective_algorithm(),
                        params.io.input_gamut_compress.knee,
                    )?;
                    let tc_lut = if input_gamut.is_active() {
                        let (rx, ry) = white_xy;
                        input_gamut.remap(&tc_lut, [rx, ry])
                    } else {
                        tc_lut
                    };
                    (Some(tc_lut), None)
                }
                "arctic2026alpha02" => {
                    front_illuminant = "D65".into();
                    let reflectance_lut = spectral_service::load_arctic2026alpha02_lut(data_dir)?;
                    let tc_lut = spectral_service::compute_reflectance_tc_lut(
                        &reflectance_lut,
                        &sensitivity,
                        ref_illuminant_f64,
                    );
                    let input_gamut = crate::input_gamut::InputGamutCompress::build(
                        params.io.input_gamut_compress.effective_algorithm(),
                        params.io.input_gamut_compress.knee,
                    )?;
                    let tc_lut = if input_gamut.is_active() {
                        let (rx, ry) = spektrafilm_math::spectral::illuminant_to_xy(
                            select_illuminant(&front_illuminant),
                        );
                        input_gamut.remap(&tc_lut, [rx, ry])
                    } else {
                        tc_lut
                    };
                    (Some(tc_lut), None)
                }
                "mallett2019" => (
                    None,
                    Some(crate::mallett::compute_core_matrix(
                        &sensitivity,
                        ref_illuminant_f64,
                    )),
                ),
                other => {
                    return Err(format!("unsupported rgb_to_raw_method: {other:?}"));
                }
            };

        // Python parity: sensitivities are pre-balanced in the profile so midgray ≈ 1.0.
        // The TC LUT is used unnormalized — Python's `rgb_to_raw_hanatos2025` does NOT scale it.
        // See `spektrafilm/utils/spectral_upsampling.py:rgb_to_raw_hanatos2025` (comment line 373).

        // Compute enlarger illuminant with dichroic filters — f64 for Python parity.
        // c/m/y come from the f64 lookup, shift values are f32 in params.
        let print_illuminant = enlarger::enlarger_filtered_illuminant_f64(
            &params.enlarger.illuminant,
            c_neutral_f64,
            m_neutral_f64 + params.enlarger.m_filter_shift as f64,
            y_neutral_f64 + params.enlarger.y_filter_shift as f64,
        );

        // Midgray sensor raw for the print-exposure normalization — always in
        // sRGB (Python `_rgb_to_film_raw` default), through whichever upsampler
        // is active. `scale` folds the EV compensation for the `_comp` branch.
        let midgray_raw = |scale: f64| -> [f64; 3] {
            if let Some(lut) = &tc_lut {
                enlarger::midgray_raw_hanatos(
                    lut,
                    select_illuminant(&front_illuminant),
                    params.settings.use_cat16,
                    scale,
                )
            } else {
                let core = mallett_core
                    .as_ref()
                    .expect("one upsampler is always built");
                let m = crate::mallett::film_matrix(core, "sRGB");
                let g = 0.184 * scale;
                crate::mallett::apply(&m, [g, g, g])
            }
        };

        // Compute midgray spectral density (gray card through full filming path)
        let density_spectral_midgray =
            enlarger::midgray_density_spectral_from_raw(midgray_raw(1.0), &film, &params);

        // Print sensitivity: Python `sensitivity = np.nan_to_num(10 ** log_sensitivity)` — f64 with NaN→0.
        let print_log_sens = print.log_sensitivity_f64();
        let print_sensitivity: Vec<[f64; 3]> = print_log_sens
            .iter()
            .map(|row| {
                let mut out = [0.0f64; 3];
                for c in 0..3 {
                    let v = 10.0f64.powf(row[c]);
                    out[c] = if v.is_nan() { 0.0 } else { v };
                }
                out
            })
            .collect();

        // Constant enlarger preflash exposure (Python `_compute_raw_preflash`):
        // film base density lit by the preflash-filtered illuminant, integrated
        // against the print sensitivity. Off (→ [0; 3]) unless preflash_exposure > 0.
        let preflash_raw = enlarger::compute_preflash_raw(
            &params.enlarger,
            c_neutral_f64,
            m_neutral_f64,
            y_neutral_f64,
            &film.data.base_density,
            &print_sensitivity,
        );

        // Print exposure normalization — mirror Python's
        // `_compute_exposure_factor_midgray` in
        // `spektrafilm/runtime/stages/printing.py`. There are two
        // candidate factors:
        //   * `factor_midgray`     = 1 / geomean(raw_midgray)
        //   * `factor_midgray_comp`= 1 / geomean(raw_midgray_with_neg_EV)
        // and four flag combinations of
        // `enlarger.normalize_print_exposure` × `enlarger.print_exposure_compensation`.
        //
        // With both flags ON (defaults) and no EV compensation,
        // Python returns `factor_midgray_comp == 1.0`. Rust used to
        // unconditionally apply `factor_midgray` here, which biased
        // the print exposure by ~3% on every render and was the root
        // cause of the residual Python-parity drift in the print stage.
        let factor_midgray = enlarger::compute_exposure_factor(
            &density_spectral_midgray,
            &print_illuminant,
            &print_sensitivity,
        );
        // Python builds `density_spectral_midgray_comp` whenever
        // `print_exposure_compensation` is on — even when EV == 0, in
        // which case `rgb_midgray_comp = rgb_midgray * 2^0` and so
        // `factor_midgray_comp == factor_midgray`. We have to mirror
        // that (NOT short-circuit to 1.0) because the
        // `factor_midgray_comp` branch is what gets returned by default.
        let factor_midgray_comp = if !params.enlarger.print_exposure_compensation {
            1.0
        } else if params.camera.exposure_compensation_ev == 0.0 {
            factor_midgray
        } else {
            // Python: `rgb_midgray_comp = rgb_midgray * 2 ** exposure_compensation_ev`.
            let scale = 2.0f64.powf(params.camera.exposure_compensation_ev as f64);
            let density_spectral_midgray_comp =
                enlarger::midgray_density_spectral_from_raw(midgray_raw(scale), &film, &params);
            enlarger::compute_exposure_factor(
                &density_spectral_midgray_comp,
                &print_illuminant,
                &print_sensitivity,
            )
        };
        let print_exposure_factor = match (
            params.enlarger.normalize_print_exposure,
            params.enlarger.print_exposure_compensation,
        ) {
            (true, true) => factor_midgray_comp,
            (true, false) => factor_midgray,
            (false, true) => factor_midgray_comp / factor_midgray,
            (false, false) => 1.0,
        };

        tracing::info!(
            method = params.settings.rgb_to_raw_method,
            lut_size = tc_lut.as_ref().map_or(0, |l| l.size),
            film = film.info.stock.as_deref().unwrap_or("unknown"),
            print_exposure_factor = print_exposure_factor,
            "pipeline calibrated"
        );

        let output_gamut = crate::gamut_compression::OutputGamutCompress::build(
            &params.io.output_gamut_compress,
            &params.io.output_color_space,
        );
        Ok(Self {
            film,
            print,
            params,
            tc_lut,
            mallett_core,
            front_illuminant,
            print_exposure_factor,
            print_illuminant,
            preflash_raw,
            output_gamut,
        })
    }

    pub fn process(&self, image: ImageBuf, backend: &dyn ComputeBackend) -> ImageBuf {
        let stage_timings = stage_timings_enabled();
        let t = Instant::now();
        let (image, pix_um, exposure_scale) = self.prepare_image(image);
        print_stage_timing(stage_timings, "preprocess", t);
        tracing::info!(backend = backend.name(), "pipeline: start");

        // Scanner B&W/slide exposure correction (no-op unless scanner
        // white/black correction is on for a slide or print scan). Computed
        // up front so both the GPU-resident and per-stage paths share it.
        let t = Instant::now();
        let color_ref = self.color_reference();
        print_stage_timing(stage_timings, "color_reference", t);

        // GPU fast path: dispatch the whole filming→printing→scanning chain
        // (or filming→scanning when scan_film) as a single GPU command
        // buffer — one upload + one readback total. CUDA and WGSL run the
        // extended resident chain, including camera/scanner lens blur,
        // highlight boost, and enlarger diffusion. Backends that do not
        // implement the resident chain return `None` and fall through.
        if self.can_render_resident(backend)
            && let Some(out) =
                self.try_gpu_resident(&image, backend, &color_ref, pix_um, exposure_scale)
        {
            tracing::info!("pipeline: gpu-resident fast path complete");
            return self.finish_resident_scan(out, backend);
        }

        self.process_staged(
            image,
            &self.render_taps()[2..],
            backend,
            &color_ref,
            pix_um,
            exposure_scale,
        )
    }

    /// Run the GPU-resident path directly on the original image when no crop
    /// or rescale is needed. This lets the live preview skip an input clone.
    pub fn process_resident_borrowed(
        &self,
        image: &ImageBuf,
        backend: &dyn ComputeBackend,
    ) -> Option<ImageBuf> {
        if self.params.io.crop || self.working_scale() != 1.0 || !self.can_render_resident(backend)
        {
            return None;
        }
        tracing::info!(
            backend = backend.name(),
            "pipeline: borrowed resident start"
        );
        let color_ref = crate::color_reference::ColorReference::compute_with_preflash(
            &self.film,
            &self.print,
            &self.params,
            &self.print_illuminant,
            self.print_exposure_factor,
            self.preflash_raw,
        );
        let out = self.try_gpu_resident(
            image,
            backend,
            &color_ref,
            self.pixel_size_um(image),
            stages::filming::autoexposure_scale(image, &self.params),
        )?;
        Some(self.finish_resident_scan(out, backend))
    }

    fn can_render_resident(&self, backend: &dyn ComputeBackend) -> bool {
        // The resident path runs direct spectral integration. Staged
        // rendering handles the internal print and scan LUTs.
        backend.is_gpu()
            && (self.tc_lut.is_some() || self.mallett_core.is_some())
            && self
                .output_gamut
                .gpu_params()
                .is_none_or(|gamut| backend.supports_resident_gamut(&gamut))
            && !self.params.settings.use_scanner_lut
            && (self.params.io.scan_film || !self.params.settings.use_enlarger_lut)
    }

    fn resident_halation_params(&self, pix_um: f32) -> Option<spektrafilm_gpu::HalationGpuParams> {
        let h = &self.params.film_render.halation;
        if !h.active {
            return None;
        }
        let (strength, sigma_um) = h.strength_and_sigma(&self.film);
        let a_tot = strength.map(|s| s * h.halation_amount);
        let px = |radius: [f64; 3], scale: f64| radius.map(|r| r * scale / f64::from(pix_um));
        let halation_sigma = px(sigma_um, h.halation_spatial_scale);
        let halation_n_bounces =
            if halation_sigma.iter().any(|&s| s > 0.0) && a_tot.iter().any(|&a| a > 0.0) {
                h.halation_n_bounces
            } else {
                0
            };
        let scatter_core_px = px(h.scatter_core_um, h.scatter_spatial_scale);
        let scatter_tail_px = px(h.scatter_tail_um, h.scatter_spatial_scale);
        let scatter_amount = if scatter_core_px
            .iter()
            .chain(&scatter_tail_px)
            .any(|&s| s > 0.0)
        {
            h.scatter_amount.max(0.0)
        } else {
            0.0
        };
        if scatter_amount == 0.0 && halation_n_bounces == 0 {
            return None;
        }
        Some(spektrafilm_gpu::HalationGpuParams {
            scatter_amount: scatter_amount as f32,
            scatter_core_px,
            scatter_tail_px,
            scatter_tail_weight: h.scatter_tail_weight.map(|w| w as f32),
            halation_a_tot: a_tot.map(|a| a as f32),
            halation_first_sigma_px: halation_sigma,
            halation_n_bounces,
            halation_bounce_decay: h.halation_bounce_decay,
            halation_renormalize: h.halation_renormalize,
        })
    }

    fn pixel_size_um(&self, image: &ImageBuf) -> f32 {
        stages::filming::pixel_size_um(self.params.camera.film_format_mm, image.width, image.height)
    }

    fn working_scale(&self) -> f32 {
        let scale = self.params.io.upscale_factor;
        if !scale.is_finite() || scale <= 0.0 || (scale - 1.0).abs() < 1e-6 {
            1.0
        } else {
            scale
        }
    }

    fn rescale_working(image: ImageBuf, scale: f32) -> ImageBuf {
        use spektrafilm_math::resize::{Interpolation, resize, scaled_dimensions};

        if scale == 1.0 || image.data.is_empty() {
            return image;
        }
        let [new_w, new_h] = scaled_dimensions(image.width, image.height, f64::from(scale));
        if [new_w, new_h] == [image.width, image.height] {
            return image;
        }
        tracing::info!(
            from = format!("{}x{}", image.width, image.height),
            to = format!("{new_w}x{new_h}"),
            "pipeline: rescaled working image"
        );
        resize(&image, [new_w, new_h], Interpolation::CubicSpline)
    }

    /// Try the GPU-resident fast path. Builds all the per-stage data and
    /// hands it to the backend's `try_run_film_chain`. Device sRGB
    /// encoding is optional. Other output encodings run on the host.
    fn try_gpu_resident(
        &self,
        image: &ImageBuf,
        backend: &dyn ComputeBackend,
        color_ref: &crate::color_reference::ColorReference,
        pix_um: f32,
        autoexposure_scale: f64,
    ) -> Option<ImageBuf> {
        // Match staged rendering for encoded raster inputs. The metered
        // factor applies before decoding, then manual EV applies in linear RGB.
        let input_cctf = self.params.io.input_cctf();
        let (image, mut exposure_scale_f64) =
            if input_cctf == spektrafilm_math::colorspace::Cctf::Linear {
                (Cow::Borrowed(image), autoexposure_scale)
            } else {
                use rayon::prelude::*;
                use spektrafilm_math::precision::from_f64;

                let scale = from_f64(autoexposure_scale);
                let linear = image
                    .data
                    .par_iter()
                    .map(|&value| input_cctf.decode(value * scale))
                    .collect();
                (
                    Cow::Owned(ImageBuf::from_data(image.width, image.height, linear)),
                    1.0,
                )
            };

        // Bake the remaining exposure scale
        // into the front-pass matrix. Both upsamplers (hanatos and mallett)
        // are homogeneous in the input RGB, so scaling the matrix is
        // equivalent to scaling the input — saves a separate "scale" compute
        // pass at the head of the chain. Auto-exposure metering itself stays
        // on CPU (~30 ms at 6 MP after the per-row rayon parallelization);
        // the result is a single float that's cheap to roll into the matrix.
        if self.params.camera.exposure_compensation_ev != 0.0 {
            exposure_scale_f64 *= 2.0f64.powf(self.params.camera.exposure_compensation_ev as f64);
        }
        // B&W/slide filming exposure correction is a linear scale on the raw
        // film exposure (CPU: `raw *= factor` before log10). Since both
        // upsamplers are homogeneous in the input RGB, folding it into the
        // exposure scale is equivalent. 1.0 (no-op) on every path except a
        // corrected slide scan.
        exposure_scale_f64 *= color_ref.filming_exposure_correction;
        let fold_exposure = |m: &mut [[f64; 3]; 3]| {
            if (exposure_scale_f64 - 1.0).abs() > 1e-9 {
                for row in m.iter_mut() {
                    for v in row.iter_mut() {
                        *v *= exposure_scale_f64;
                    }
                }
            }
        };

        // Front pass: hanatos TC LUT lookup, or the mallett 3×3 matmul
        // (`core · M_cs`, same fold as the CPU `expose` dispatch).
        let front = if let Some(tc_lut) = self.tc_lut.as_ref() {
            let ref_illuminant = select_illuminant(&self.front_illuminant);
            let mut rgb_to_adapted = spektrafilm_math::spectral::build_rgb_to_adapted_xyz(
                &self.params.io.input_color_space,
                ref_illuminant,
                self.params.settings.use_cat16,
            );
            fold_exposure(&mut rgb_to_adapted);
            spektrafilm_gpu::FrontPass::Hanatos2025 {
                tc_lut,
                rgb_to_adapted_xyz: rgb_to_adapted,
            }
        } else {
            let core = self.mallett_core.as_ref()?;
            let mut matrix = crate::mallett::film_matrix(core, &self.params.io.input_color_space);
            fold_exposure(&mut matrix);
            spektrafilm_gpu::FrontPass::Mallett2019 { matrix }
        };

        // Film density curves: normalized (filming.develop subtracts nanmin).
        let film_log_exp = self.film.log_exposure_f64();
        let film_curves = self.film.density_curves_f64();
        let film_curves_norm =
            spektrafilm_model::density_curves::normalize_density_curves_f64(&film_curves);
        let film_channel_density: Vec<[f64; 3]> = self
            .film
            .data
            .channel_density
            .iter()
            .map(|r| {
                [
                    r.first().copied().unwrap_or(0.0),
                    r.get(1).copied().unwrap_or(0.0),
                    r.get(2).copied().unwrap_or(0.0),
                ]
            })
            .collect();
        let film_base_density = self.film.data.base_density.clone();

        // Print sensitivity (10**log_sensitivity, NaN→0).
        let print_sens: Vec<[f64; 3]> = self
            .print
            .log_sensitivity_f64()
            .iter()
            .map(|row| {
                let mut o = [0.0; 3];
                for c in 0..3 {
                    let v = 10f64.powf(row[c]);
                    o[c] = if v.is_nan() { 0.0 } else { v };
                }
                o
            })
            .collect();
        let print_log_exp = self.print.log_exposure_f64();
        // Print density curves: model-evaluated at gamma 1 whenever the
        // profile carries a fitted `density_curves_model` (identity when the
        // morph is inactive, morphed when active — matching the CPU
        // `develop_print_morph` path); stored RAW curves at the configured
        // gamma only for model-less profiles. Computed once on CPU here so
        // the resident print density pass needs no shader change.
        let morph = &self.params.print_render.density_curves_morph;
        let (print_curves, print_gamma_eff) = match self.print.data.density_curves_model.as_ref() {
            Some(model) => match crate::print_morph::morph_density_curves(
                &print_log_exp,
                model,
                morph,
                self.print.is_positive(),
            ) {
                Ok(curves) => (curves, 1.0),
                Err(e) => {
                    tracing::error!(error = %e, "print-curve model eval failed; using stored curves");
                    (
                        self.print.density_curves_f64(),
                        self.params.print_render.density_curve_gamma as f64,
                    )
                }
            },
            None => (
                self.print.density_curves_f64(),
                self.params.print_render.density_curve_gamma as f64,
            ),
        };
        let print_channel_density: Vec<[f64; 3]> = self
            .print
            .data
            .channel_density
            .iter()
            .map(|r| {
                [
                    r.first().copied().unwrap_or(0.0),
                    r.get(1).copied().unwrap_or(0.0),
                    r.get(2).copied().unwrap_or(0.0),
                ]
            })
            .collect();
        let print_base_density = self.print.data.base_density.clone();

        // Scanning: viewing illuminant + normalization + combined XYZ→RGB
        // matrix. For scan_film we scan the developed film directly, so the
        // viewing illuminant and dye-density wavelength count come from the
        // film, not the print (mirrors the CPU scanning stage, which is
        // handed `self.film` as its profile when scan_film).
        let scan_profile = if self.params.io.scan_film {
            &self.film
        } else {
            &self.print
        };
        let viewing_illum =
            stages::scanning::viewing_illuminant(&scan_profile.info.viewing_illuminant);
        let viewing_illu: Vec<f64> = viewing_illum.samples.iter().map(|&v| v as f64).collect();
        let n_wl = if self.params.io.scan_film {
            film_channel_density.len()
        } else {
            print_channel_density.len()
        };
        let scan_norm: f64 = (0..n_wl)
            .map(|i| viewing_illu[i] * spektrafilm_math::spectral::CMF_Y[i] as f64)
            .sum();
        let viewing_white = spektrafilm_math::spectral::illuminant_xyz_f64(viewing_illum.samples);
        let output_white = spektrafilm_math::spectral::colorspace_white_xyz_f64(
            &self.params.io.output_color_space,
        );
        let adapt = spektrafilm_math::colorspace::chromatic_adaptation_matrix_f64(
            viewing_white,
            output_white,
        );
        let base_xyz_to_rgb =
            stages::scanning::output_colorspace_from_xyz_f64(&self.params.io.output_color_space);
        let mut scan_xyz_to_rgb = [[0.0f64; 3]; 3];
        for i in 0..3 {
            for j in 0..3 {
                scan_xyz_to_rgb[i][j] = base_xyz_to_rgb[i][0] * adapt[0][j]
                    + base_xyz_to_rgb[i][1] * adapt[1][j]
                    + base_xyz_to_rgb[i][2] * adapt[2][j];
            }
        }

        // print_exposure_factor × print_exposure, with the B&W printing
        // exposure correction folded in (CPU applies it as a further raw
        // multiply; print_spectral applies the whole product as one
        // normalization). 1.0 except on a corrected print scan.
        let print_exposure_scale =
            self.params.enlarger.print_exposure as f64 * color_ref.printing_exposure_correction;
        let print_norm_factor = self.print_exposure_factor * print_exposure_scale;

        let halation = self.resident_halation_params(pix_um);

        // DIR couplers in the resident chain. Mirrors CPU
        // `apply_density_correction`: build the scaled couplers matrix and
        // pre-compute the "density curves before DIR" once. The shader
        // re-interpolates these against `log_raw - correction`.
        // Held in this binding so `&density_curves_0_f64` outlives the
        // backend call.
        let dir_inputs = if self.params.film_render.dir_couplers.active {
            let dir = &self.params.film_render.dir_couplers;
            let mut matrix_scaled = stages::filming::dir_couplers_matrix(&self.film, dir);
            for row in &mut matrix_scaled {
                for v in row.iter_mut() {
                    *v *= dir.amount;
                }
            }
            let film_curves_f32 = self.film.density_curves_f32();
            let film_log_exp_f32 = self.film.log_exposure_f32();
            let norm_curves_f32 =
                spektrafilm_model::density_curves::normalize_density_curves(&film_curves_f32);
            // Narrow matrix to f32 just for the compute_curves_before_dir helper
            // (which still operates on f32 density curves from the profile).
            let matrix_scaled_f32: [[f32; 3]; 3] = [
                [
                    matrix_scaled[0][0] as f32,
                    matrix_scaled[0][1] as f32,
                    matrix_scaled[0][2] as f32,
                ],
                [
                    matrix_scaled[1][0] as f32,
                    matrix_scaled[1][1] as f32,
                    matrix_scaled[1][2] as f32,
                ],
                [
                    matrix_scaled[2][0] as f32,
                    matrix_scaled[2][1] as f32,
                    matrix_scaled[2][2] as f32,
                ],
            ];
            let density_curves_0_f32 = spektrafilm_model::couplers::compute_curves_before_dir(
                &norm_curves_f32,
                &film_log_exp_f32,
                &matrix_scaled_f32,
                self.film.is_positive(),
            );
            let density_curves_0_f64: Vec<[f64; 3]> = density_curves_0_f32
                .iter()
                .map(|row| [row[0] as f64, row[1] as f64, row[2] as f64])
                .collect();
            let density_max_f32 = spektrafilm_model::density_curves::max_density(&norm_curves_f32);
            Some((
                density_curves_0_f64,
                matrix_scaled,
                density_max_f32,
                pix_um,
                dir.diffusion_size_um,
                dir.diffusion_tail_um,
                dir.diffusion_tail_weight,
                self.film.is_positive(),
                self.params.film_render.density_curve_gamma as f64,
            ))
        } else {
            None
        };
        let dir_couplers = dir_inputs.as_ref().map(|d| {
            // GPU shader path is f32 — narrow at the boundary.
            let matrix_f32: [[f32; 3]; 3] = [
                [d.1[0][0] as f32, d.1[0][1] as f32, d.1[0][2] as f32],
                [d.1[1][0] as f32, d.1[1][1] as f32, d.1[1][2] as f32],
                [d.1[2][0] as f32, d.1[2][1] as f32, d.1[2][2] as f32],
            ];
            spektrafilm_gpu::DirCouplersGpuParams {
                couplers_matrix_scaled: matrix_f32,
                density_max: d.2,
                is_positive: d.7,
                diffusion_size_px: (d.4 / d.3 as f64) as f32,
                diffusion_tail_px: (d.5 / d.3 as f64) as f32,
                diffusion_tail_weight: d.6 as f32,
                density_curves_0: &d.0,
                log_exposure: &film_log_exp,
                gamma_factor: d.8,
            }
        });

        let grain_params = &self.params.film_render.grain;
        let layered_grain = if grain_params.active && grain_params.sublayers_active {
            spektrafilm_model::grain::layered_grain_gpu_params(
                &film_curves_norm,
                &self.film.data.density_curves_layers,
                &stages::filming::layered_grain_params(&self.params, &self.film, pix_um),
            )
        } else {
            None
        };
        if layered_grain.is_some() {
            tracing::debug!("pipeline: using profiled resident grain");
        }
        let grain = if grain_params.active && layered_grain.is_none() {
            let g = &self.params.film_render.grain;
            let pixel_area = pix_um * pix_um;
            let n_sub = g.n_sub_layers.max(1);
            // GPU shaders are f32 — narrow the f64 grain params at the boundary.
            let mut npp = [0.0f32; 3];
            for c in 0..3 {
                let particle_area = g.agx_particle_area_um2 * g.agx_particle_scale[c];
                npp[c] = ((pixel_area as f64 / particle_area) / n_sub as f64) as f32;
            }
            let film_curves_f32 = self.film.density_curves_f32();
            let norm_curves_f32 =
                spektrafilm_model::density_curves::normalize_density_curves(&film_curves_f32);
            let dmax_curves = spektrafilm_model::density_curves::max_density(&norm_curves_f32);
            let mut density_max = [0.0f32; 3];
            for c in 0..3 {
                density_max[c] = dmax_curves[c] + g.density_min[c] as f32;
            }
            Some(spektrafilm_gpu::GrainGpuParams {
                density_min: [
                    g.density_min[0] as f32,
                    g.density_min[1] as f32,
                    g.density_min[2] as f32,
                ],
                density_max,
                n_particles_per_pixel: npp,
                grain_uniformity: [
                    g.uniformity[0] as f32,
                    g.uniformity[1] as f32,
                    g.uniformity[2] as f32,
                ],
                n_sub_layers: n_sub,
                base_seed: 0,
                grain_blur: g.blur,
                monochrome: g.monochrome,
            })
        } else {
            None
        };

        // Viewing glare is only applied to prints in the reference pipeline.
        let glare_params = &self.params.print_render.glare;
        let glare =
            if !self.params.io.scan_film && glare_params.active && glare_params.percent > 0.0 {
                let g = glare_params;
                // LogNormal parameters (same derivation as `compute_random_glare_amount`).
                let m = g.percent as f64;
                let s = (g.roughness * g.percent) as f64;
                let sigma2 = (1.0 + (s * s) / (m * m)).ln();
                let sigma = sigma2.sqrt();
                let mu = m.ln() - sigma2 / 2.0;
                // glare_rgb_offset = (XYZ→RGB) · illuminant_xyz / 100.
                let mut illu_xyz = [0.0f64; 3];
                for i in 0..n_wl {
                    illu_xyz[0] += viewing_illu[i] * spektrafilm_math::spectral::CMF_X[i] as f64;
                    illu_xyz[1] += viewing_illu[i] * spektrafilm_math::spectral::CMF_Y[i] as f64;
                    illu_xyz[2] += viewing_illu[i] * spektrafilm_math::spectral::CMF_Z[i] as f64;
                }
                for c in 0..3 {
                    illu_xyz[c] /= scan_norm;
                }
                let mut offset_rgb = [0.0f32; 3];
                for i in 0..3 {
                    let v = scan_xyz_to_rgb[i][0] * illu_xyz[0]
                        + scan_xyz_to_rgb[i][1] * illu_xyz[1]
                        + scan_xyz_to_rgb[i][2] * illu_xyz[2];
                    offset_rgb[i] = (v / 100.0) as f32;
                }
                Some(spektrafilm_gpu::GlareGpuParams {
                    mu: mu as f32,
                    sigma: sigma as f32,
                    blur_px: g.blur,
                    base_seed: 42,
                    rgb_offset: offset_rgb,
                })
            } else {
                None
            };

        // Unsharp mask: scanner.unsharp_mask = [sigma, amount].
        let [usm_sigma, usm_amount] = self.params.scanner.unsharp_mask;
        let unsharp = if usm_sigma > 0.0 && usm_amount > 0.0 {
            Some(spektrafilm_gpu::UnsharpGpuParams {
                sigma_px: usm_sigma,
                amount: usm_amount,
            })
        } else {
            None
        };

        let camera_lens_blur_px = if self.params.camera.lens_blur_um > 0.0 {
            Some(self.params.camera.lens_blur_um / pix_um)
        } else {
            None
        };

        let scanner_lens_blur_px = if self.params.scanner.lens_blur > 0.0 {
            Some(self.params.scanner.lens_blur)
        } else {
            None
        };

        let hboost = &self.params.film_render.halation;
        let highlight_boost = if hboost.boost_ev != 0.0 {
            Some(spektrafilm_gpu::HighlightBoostGpuParams {
                boost_ev: hboost.boost_ev as f32,
                boost_range: hboost.boost_range as f32,
                protect_ev: hboost.protect_ev as f32,
            })
        } else {
            None
        };

        // Camera diffusion filter plan (downsampled sum-of-Gaussians) for the
        // resident chain. `None` when the filter is a no-op.
        let diffusion = if self.params.camera.diffusion_filter.active {
            spektrafilm_model::diffusion::diffusion_gpu_plan(
                &self.params.camera.diffusion_filter.to_model(),
                pix_um as f64,
                image.width,
                image.height,
            )
        } else {
            None
        };

        let enlarger_diffusion =
            if !self.params.io.scan_film && self.params.enlarger.diffusion_filter.active {
                spektrafilm_model::diffusion::diffusion_gpu_plan(
                    &self.params.enlarger.diffusion_filter.to_model(),
                    pix_um as f64,
                    image.width,
                    image.height,
                )
            } else {
                None
            };

        let params = spektrafilm_gpu::FilmChainParams {
            image: &image,
            front,
            film_log_exposure: &film_log_exp,
            film_density_curves_normalized: &film_curves_norm,
            film_gamma: self.params.film_render.density_curve_gamma as f64,
            film_channel_density: &film_channel_density,
            film_base_density: &film_base_density,
            print_illuminant: &self.print_illuminant,
            print_sensitivity: &print_sens,
            print_normalization_factor: print_norm_factor,
            print_log_exposure: &print_log_exp,
            print_density_curves: &print_curves,
            print_gamma: print_gamma_eff,
            print_channel_density: &print_channel_density,
            print_base_density: &print_base_density,
            preflash: self.preflash_raw,
            viewing_illuminant: &viewing_illu,
            scan_normalization: scan_norm,
            scan_xyz_to_rgb: &scan_xyz_to_rgb,
            bw_xyz_remap: color_ref.xyz_remap(),
            scan_film: self.params.io.scan_film,
            halation,
            dir_couplers,
            grain,
            layered_grain,
            glare,
            gamut: self.output_gamut.gpu_params(),
            unsharp,
            camera_lens_blur_px,
            scanner_lens_blur_px,
            highlight_boost,
            diffusion,
            enlarger_diffusion,
            print_exposure_scale,
            device_srgb_matrix: self.device_encodes_srgb(backend).then(|| {
                stages::scanning::rgb_to_rgb_identity_matrix(&self.params.io.output_color_space)
            }),
        };
        backend.try_run_film_chain(&params)
    }

    fn device_encodes_srgb(&self, backend: &dyn ComputeBackend) -> bool {
        self.params.io.output_cctf() == spektrafilm_math::colorspace::Cctf::Srgb
            && backend.resident_chain_applies_post_scan()
    }

    fn finish_resident_scan(&self, mut rgb: ImageBuf, backend: &dyn ComputeBackend) -> ImageBuf {
        if !self.device_encodes_srgb(backend) {
            stages::scanning::apply_output_cctf(&mut rgb, &self.params.io);
        }
        rgb
    }
}

fn select_illuminant_f64(name: &str) -> &'static [f64] {
    use spektrafilm_math::spectral;
    spectral::named_illuminant(name)
        .map_or(&spectral::ILLUMINANT_D55_F64, |illum| illum.samples_f64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use spektrafilm_gpu::cpu_backend::CpuBackend;
    use spektrafilm_math::precision::from_f64;

    fn data_dir() -> std::path::PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../data")
    }

    #[test]
    #[ignore = "requires a native wgpu adapter"]
    fn resident_gamut_matches_cpu_for_wide_spaces_and_jzazbz() {
        use spektrafilm_gpu::wgpu_backend::WgpuBackend;

        let backend = WgpuBackend::new().expect("native wgpu adapter");
        let dir = data_dir();
        let film = crate::profile::load_profile_by_name(&dir, "kodak_ektar_100").unwrap();
        let paper = crate::profile::load_profile_by_name(&dir, "kodak_ultra_endura").unwrap();
        let samples = [
            [0.0, 0.0, 0.0],
            [0.0001, 0.0004, 0.0002],
            [0.18, 0.18, 0.18],
            [1.0, 1.0, 1.0],
            [1.0, 0.0, 0.0],
            [0.0, 1.0, 0.0],
            [0.0, 0.0, 1.0],
            [2.5, 1.0, 0.2],
            [0.06, 0.6, 2.3],
        ];
        let data = samples.into_iter().flatten().map(from_f64).collect();
        let image = ImageBuf::from_data(samples.len() as u32, 1, data);
        let mut params = RuntimeParams::default();
        params.io.input_cctf_decoding = false;
        params.camera.auto_exposure = false;
        params.film_render.halation.active = false;
        params.film_render.halation.boost_ev = 0.0;
        params.film_render.dir_couplers.active = false;
        params.film_render.grain.active = false;
        params.print_render.glare.active = false;
        params.scanner.unsharp_mask = [0.0; 2];

        for (space, mode, encoded, tolerance) in [
            ("Display P3", "cam16ucs", true, 5e-5),
            ("DCI-P3", "cam16ucs", false, 5e-5),
            ("Adobe RGB (1998)", "oklrab", true, 5e-5),
            ("ProPhoto RGB", "oklch", false, 5e-5),
            ("sRGB", "jzazbz", true, 4e-4),
            ("ITU-R BT.2020", "jzazbz", false, 4e-4),
        ] {
            params.io.output_color_space = space.into();
            params.io.output_cctf_encoding = encoded;
            params.io.output_gamut_compress.algorithm = mode.into();
            let pipeline =
                Pipeline::new_with_spectral(film.clone(), paper.clone(), params.clone(), &dir)
                    .unwrap();
            let gpu = pipeline
                .process_resident_borrowed(&image, &backend)
                .unwrap_or_else(|| panic!("{space} / {mode} missed the resident path"));
            let cpu = pipeline.process(image.clone(), &CpuBackend);
            for (channel, (&gpu, &cpu)) in gpu.data.iter().zip(&cpu.data).enumerate() {
                let delta = (gpu as f64 - cpu as f64).abs();
                assert!(
                    delta < tolerance,
                    "{space} / {mode}, channel {channel}: GPU {gpu}, CPU {cpu}, delta {delta}"
                );
            }
        }
    }

    #[test]
    #[ignore = "requires a native wgpu adapter"]
    fn resident_optics_matches_cpu_for_mixed_radii_and_preflash() {
        let backend =
            spektrafilm_gpu::wgpu_backend::WgpuBackend::new().expect("native wgpu adapter");
        let dir = data_dir();
        let film = crate::profile::load_profile_by_name(&dir, "kodak_ektar_100").unwrap();
        let paper = crate::profile::load_profile_by_name(&dir, "kodak_ultra_endura").unwrap();
        let mut params = RuntimeParams::default();
        params.camera.film_format_mm = 0.021;
        params.camera.auto_exposure = false;
        params.io.input_cctf_decoding = false;
        params.io.output_cctf_encoding = false;
        params.io.output_gamut_compress.algorithm = "off".into();
        params.enlarger.preflash_exposure = 0.03125;
        params.film_render.dir_couplers.active = false;
        params.film_render.grain.active = false;
        params.print_render.glare.active = false;
        params.scanner.unsharp_mask = [0.0; 2];

        let optics = &mut params.film_render.halation;
        optics.scatter_amount = 0.63;
        optics.scatter_core_um = [1.1, 3.5, 0.0];
        optics.scatter_tail_um = [6.5, 2.1, 0.0];
        optics.scatter_tail_weight = [0.2, 0.61, 0.86];
        optics.halation_strength = Some([0.15, 0.09, 0.13]);
        optics.halation_first_sigma_um = Some([2.0, 0.0, 79.0]);
        optics.halation_n_bounces = 3;
        optics.halation_bounce_decay = 0.38;
        let mut image =
            ImageBuf::from_data(21, 12, [0.15, 0.11, 0.06].map(from_f64).repeat(21 * 12));
        image.set(0, 0, [3.1, 1.0, 0.4].map(from_f64));
        image.set(20, 0, [0.1, 0.3, 2.8].map(from_f64));
        image.set(10, 6, [2.8, 1.3, 0.9].map(from_f64));
        for renormalize in [false, true] {
            params.film_render.halation.halation_renormalize = renormalize;
            let pipeline =
                Pipeline::new_with_spectral(film.clone(), paper.clone(), params.clone(), &dir)
                    .unwrap();
            let gpu = pipeline
                .process_resident_borrowed(&image, &backend)
                .expect("optical passes should stay resident");
            let cpu = pipeline.process(image.clone(), &CpuBackend);
            for (px, (gpu, cpu)) in gpu.data.into_iter().zip(cpu.data).enumerate() {
                let delta = (gpu as f64 - cpu as f64).abs();
                assert!(delta < 2e-5, "channel sample {px}: {gpu}, {cpu}, {delta}");
            }
        }
    }

    #[test]
    #[ignore = "requires a native wgpu adapter"]
    fn resident_profiled_grain_preserves_moments_and_aggregate_fallback() {
        let backend =
            spektrafilm_gpu::wgpu_backend::WgpuBackend::new().expect("native wgpu adapter");
        let dir = data_dir();
        let paper = crate::profile::load_profile_by_name(&dir, "kodak_portra_endura").unwrap();
        let mut params = RuntimeParams::default();
        params.camera.auto_exposure = false;
        params.camera.film_format_mm = 0.5;
        params.io.scan_film = true;
        params.io.input_cctf_decoding = false;
        params.io.output_cctf_encoding = false;
        params.io.output_gamut_compress.algorithm = "off".into();
        params.film_render.halation.active = false;
        params.film_render.dir_couplers.active = true;
        params.film_render.grain.blur = 0.0;
        params.film_render.grain.micro_structure = [0.0, 0.0];
        params.print_render.glare.active = false;
        params.scanner.unsharp_mask = [0.0; 2];
        let (width, height) = (229, 223);
        let image = ImageBuf::from_data(
            width,
            height,
            [0.24, 0.18, 0.12]
                .map(from_f64)
                .repeat((width * height) as usize),
        );
        let render = |pipeline: &Pipeline| {
            pipeline
                .process_resident_borrowed(&image, &backend)
                .expect("grain should stay resident")
        };
        let moments = |img: &ImageBuf, channel| {
            let n = img.pixel_count() as f64;
            let pixels = || img.pixels().map(move |px| px[channel] as f64);
            let mean = pixels().sum::<f64>() / n;
            let var = pixels().map(|value| (value - mean).powi(2)).sum::<f64>() / n;
            (mean, var)
        };

        for stock in ["kodak_portra_400", "fujifilm_velvia_100"] {
            let film = crate::profile::load_profile_by_name(&dir, stock).unwrap();
            let pipeline =
                Pipeline::new_with_spectral(film, paper.clone(), params.clone(), &dir).unwrap();
            let mut unblurred = pipeline.params.clone();
            unblurred.film_render.grain.blur_dye_clouds_um = 0.0;
            let raw = render(&pipeline.clone().with_params(unblurred));
            let mut blurred = pipeline.params.clone();
            blurred.film_render.grain.blur_dye_clouds_um = 3.0;
            let blurred = pipeline.clone().with_params(blurred);
            let gpu = render(&blurred);
            let cpu = blurred.process(image.clone(), &CpuBackend);
            for channel in 0..3 {
                let (gpu_mean, gpu_var) = moments(&gpu, channel);
                let (cpu_mean, cpu_var) = moments(&cpu, channel);
                let (_, raw_var) = moments(&raw, channel);
                assert!(
                    (gpu_mean - cpu_mean).abs() < 0.003,
                    "{stock} mean {channel}"
                );
                assert!(
                    (gpu_var / cpu_var - 1.0).abs() < 0.15,
                    "{stock} variance {channel}: {gpu_var} vs {cpu_var}"
                );
                assert!(gpu_var < 0.65 * raw_var, "{stock} dye blur {channel}");
            }

            let mut aggregate = pipeline.params.clone();
            aggregate.film_render.grain.sublayers_active = false;
            let aggregate = render(&pipeline.clone().with_params(aggregate));
            let mut fallback = pipeline;
            fallback.film.data.density_curves_layers[0][0].clear();
            assert_eq!(
                render(&fallback).data,
                aggregate.data,
                "{stock} malformed layers"
            );
            fallback.film.data.density_curves_layers.clear();
            assert_eq!(
                render(&fallback).data,
                aggregate.data,
                "{stock} missing layers"
            );
        }
    }

    #[test]
    fn cached_pipeline_uses_stock_halation_and_explicit_overrides() {
        let data = data_dir();
        let print = crate::profile::load_profile_by_name(&data, "kodak_portra_endura").unwrap();
        let mut input = ImageBuf::new(7, 7);
        input.set(3, 3, [from_f64(1.0); 3]);

        for (name, strength, sigma) in [
            ("kodak_gold_200", [0.08, 0.02, 0.0], 65.0),
            ("kodak_vision3_500t", [0.015, 0.005, 0.0], 50.0),
        ] {
            let film = crate::profile::load_profile_by_name(&data, name).unwrap();
            let mut params = RuntimeParams::default();
            params.camera.auto_exposure = false;
            params.camera.film_format_mm = 0.35;
            params.film_render.halation.scatter_amount = 0.0;

            let cached = Pipeline::new(film.clone(), print.clone(), params.clone());
            let pipeline = cached.clone().with_params(params);
            let gpu = pipeline.resident_halation_params(50.0).unwrap();
            assert_eq!(gpu.halation_a_tot, strength.map(|s| s as f32));
            assert_eq!(gpu.halation_first_sigma_px, [sigma / 50.0; 3]);

            let expose = |pipeline: &Pipeline| {
                stages::filming::expose(
                    &input,
                    &film,
                    &pipeline.params,
                    &CpuBackend,
                    None,
                    None,
                    &[],
                    1.0,
                )
            };
            let mut explicit = pipeline.params.clone();
            explicit.film_render.halation.halation_strength = Some(strength);
            explicit.film_render.halation.halation_first_sigma_um = Some([sigma; 3]);
            assert_eq!(
                expose(&pipeline).data,
                expose(&cached.clone().with_params(explicit)).data
            );

            let mut overridden = pipeline.params.clone();
            overridden.film_render.halation = serde_json::from_value(serde_json::json!({
                "scatter_amount": 0.0,
                "halation_strength": [0.05, 0.015, 0.0],
            }))
            .unwrap();
            let overridden = cached.clone().with_params(overridden);
            let gpu = overridden.resident_halation_params(50.0).unwrap();
            assert_eq!(gpu.halation_a_tot, [0.05, 0.015, 0.0]);
            assert_eq!(gpu.halation_first_sigma_px, [sigma / 50.0; 3]);
            assert_ne!(expose(&pipeline).data, expose(&overridden).data);

            let mut disabled = pipeline.params.clone();
            disabled.film_render.halation = serde_json::from_value(serde_json::json!({
                "scatter_amount": 0.0,
                "halation_first_sigma_um": [0.0, 0.0, 0.0],
            }))
            .unwrap();
            let no_radius = cached.clone().with_params(disabled.clone());
            assert!(no_radius.resident_halation_params(50.0).is_none());
            disabled.film_render.halation.active = false;
            assert_eq!(
                expose(&no_radius).data,
                expose(&cached.clone().with_params(disabled)).data
            );

            let mut scatter = no_radius.params.clone();
            scatter.film_render.halation.scatter_amount = 1.0;
            let gpu = cached
                .with_params(scatter)
                .resident_halation_params(50.0)
                .unwrap();
            assert_eq!(gpu.halation_n_bounces, 0);
            assert_eq!(gpu.scatter_amount, 1.0);
        }
    }

    #[test]
    fn calibrated_hanatos_and_mallett_use_filtered_sensitivity() {
        let data = data_dir();
        let film = crate::profile::load_profile_by_name(&data, "kodak_gold_200").unwrap();
        let print = crate::profile::load_profile_by_name(&data, "kodak_portra_endura").unwrap();
        let input = ImageBuf::from_data(1, 1, [0.02, 0.03, 0.8].map(from_f64).to_vec());
        let mut params = RuntimeParams::default();
        params.camera.auto_exposure = false;
        params.film_render.halation.active = false;
        params.enlarger.print_exposure_compensation = false;

        for method in ["hanatos2025", "mallett2019"] {
            params.settings.rgb_to_raw_method = method.into();
            let build = |params| {
                Pipeline::new_with_spectral(film.clone(), print.clone(), params, &data).unwrap()
            };
            let expose = |pipeline: &Pipeline| {
                stages::filming::expose(
                    &input,
                    &pipeline.film,
                    &pipeline.params,
                    &CpuBackend,
                    pipeline.tc_lut(),
                    pipeline.mallett_core.as_ref(),
                    select_illuminant(&pipeline.front_illuminant),
                    1.0,
                )
            };
            let baseline = build(params.clone());

            let mut filters = params.clone();
            filters.camera.filter_uv = [0.0, 460.0, 8.0];
            filters.camera.filter_ir = [0.0, 605.0, 15.0];
            let inactive = build(filters.clone());
            assert_eq!(expose(&baseline).data, expose(&inactive).data, "{method}");
            assert_eq!(
                baseline.print_exposure_factor(),
                inactive.print_exposure_factor(),
                "{method}"
            );

            filters.camera.filter_uv[0] = 0.9;
            filters.camera.filter_ir[0] = 0.8;
            let filtered = build(filters);
            if method == "hanatos2025" {
                assert!(
                    (baseline.print_exposure_factor() - filtered.print_exposure_factor()).abs()
                        > 1e-6
                );
            }
            let baseline_raw = expose(&baseline);
            let filtered_raw = expose(&filtered);
            assert!(
                baseline_raw
                    .data
                    .iter()
                    .zip(filtered_raw.data)
                    .any(|(&a, b)| (a - b).abs() > from_f64(1e-3)),
                "{method}"
            );
        }
    }
}
