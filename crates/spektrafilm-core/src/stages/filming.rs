/// Filming stage: expose the digital image onto virtual film.
///
/// Full Hanatos2025 path: RGB → XYZ → xy chromaticity → tc coordinates →
/// 2D LUT lookup (spectra × sensitivity) → per-channel film raw exposure.
use rayon::prelude::*;
use spektrafilm_gpu::ComputeBackend;
use spektrafilm_math::colorspace;
use spektrafilm_math::image::ImageBuf;
use spektrafilm_math::precision::{from_f32, from_f64, to_f64};
use spektrafilm_math::resize::{Interpolation, resize, scaled_dimensions};
use spektrafilm_math::spectral::{self, TcLut};
use spektrafilm_model::grain::{self, LayeredGrainParams};
use std::borrow::Cow;
use std::time::Instant;

use crate::params::{DirCouplersParams, RuntimeParams};
use crate::profile::Profile;

fn stage_timings_enabled() -> bool {
    std::env::var_os("SPEKTRAFILM_STAGE_TIMINGS").is_some()
}

fn print_stage_timing(enabled: bool, stage: &str, start: Instant) {
    if enabled {
        eprintln!("stage {stage}: {} ms", start.elapsed().as_millis());
    }
}

/// Compute pixel size in micrometers from film format and image dimensions.
pub fn pixel_size_um(film_format_mm: f32, width: u32, height: u32) -> f32 {
    film_format_mm * 1000.0 / width.max(height) as f32
}

/// Auto-exposure compensation, Python-compatible
/// (`spektrafilm/utils/autoexposure.py`).
///
/// Python samples a Gaussian-prefiltered image at up to 256 px on the long
/// edge, then decodes the sample values and meters luminance Y.
///
/// `method` selects the metering pattern: `average`, `median`,
/// `center_weighted`, `partial`, `matrix`, `multi_zone`,
/// `highlight_weighted`. Anything else meters a flat 1.0 (0 EV), matching
/// the Python `else` branch.
pub fn measure_autoexposure_ev(image: &ImageBuf, rgb_to_xyz: &[[f32; 3]; 3], method: &str) -> f32 {
    measure_autoexposure_ev_with_cctf(
        image,
        rgb_to_xyz[1].map(f64::from),
        method,
        colorspace::Cctf::Linear,
    ) as f32
}

fn measure_autoexposure_ev_with_cctf(
    image: &ImageBuf,
    [r, g, b]: [f64; 3],
    method: &str,
    cctf: colorspace::Cctf,
) -> f64 {
    const MAX_SIZE: u32 = 256;
    if image.data.is_empty() {
        return 0.0;
    }
    let max_dim = image.width.max(image.height);
    let small = if max_dim > MAX_SIZE {
        let scale = f64::from(MAX_SIZE) / f64::from(max_dim);
        let size = scaled_dimensions(image.width, image.height, scale);
        Cow::Owned(resize(image, size, Interpolation::Nearest))
    } else {
        Cow::Borrowed(image)
    };
    let (sw, sh) = (small.width as usize, small.height as usize);
    let lum: Vec<f64> = small
        .par_pixels()
        .map(|px| {
            r * to_f64(cctf.decode(px[0]))
                + g * to_f64(cctf.decode(px[1]))
                + b * to_f64(cctf.decode(px[2]))
        })
        .collect();

    let metered = meter_luminance(&lum, sw, sh, method);
    let exposure = metered / 0.184;
    if exposure <= 0.0 || exposure.is_infinite() {
        return 0.0;
    }
    let ev = -exposure.log2();
    tracing::info!(
        sw = sw,
        sh = sh,
        method = method,
        metered = metered,
        exposure_div_184 = exposure,
        ev = ev,
        "autoexposure"
    );
    ev
}

/// Normalized pixel coordinate along an axis: `(i/dim - 0.5) * (dim/maxdim)`,
/// so the long edge spans [-0.5, 0.5] (Python `_normalized_coords`).
fn norm_coord(i: usize, dim: usize, max_dim: usize) -> f64 {
    (i as f64 / dim as f64 - 0.5) * (dim as f64 / max_dim as f64)
}

/// Meter the downsampled luminance grid with the named pattern, returning the
/// mean luminance the autoexposure should map to mid-grey (0.184).
fn meter_luminance(lum: &[f64], sw: usize, sh: usize, method: &str) -> f64 {
    let max_dim = sw.max(sh);
    match method {
        "average" => lum.iter().sum::<f64>() / lum.len() as f64,

        "median" => {
            let mut sorted = lum.to_vec();
            sorted.sort_by(|a, b| a.total_cmp(b));
            let n = sorted.len();
            if n % 2 == 1 {
                sorted[n / 2]
            } else {
                0.5 * (sorted[n / 2 - 1] + sorted[n / 2])
            }
        }

        "center_weighted" => {
            let sigma = 0.2f64;
            let inv_2sigma2 = 1.0 / (2.0 * sigma * sigma);
            let mut weighted = 0.0;
            let mut total = 0.0;
            for y in 0..sh {
                let ny = norm_coord(y, sh, max_dim);
                let ny2 = ny * ny;
                for x in 0..sw {
                    let nx = norm_coord(x, sw, max_dim);
                    let w = (-(nx * nx + ny2) * inv_2sigma2).exp();
                    weighted += lum[y * sw + x] * w;
                    total += w;
                }
            }
            weighted / total
        }

        "partial" => {
            // Hard circular region, ~15% radius (Canon Partial).
            let mut sum = 0.0;
            let mut count = 0usize;
            for y in 0..sh {
                let ny = norm_coord(y, sh, max_dim);
                for x in 0..sw {
                    let nx = norm_coord(x, sw, max_dim);
                    if (nx * nx + ny * ny).sqrt() < 0.15 {
                        sum += lum[y * sw + x];
                        count += 1;
                    }
                }
            }
            if count == 0 {
                lum.iter().sum::<f64>() / lum.len() as f64
            } else {
                sum / count as f64
            }
        }

        "matrix" => {
            // 5×5 grid; each cell weighted by a raised-cosine of its distance
            // from centre so corner zones contribute less.
            let (n_rows, n_cols) = (5usize, 5usize);
            let cell_h = sh / n_rows;
            let cell_w = sw / n_cols;
            let mut means = Vec::with_capacity(n_rows * n_cols);
            let mut weights = Vec::with_capacity(n_rows * n_cols);
            for r in 0..n_rows {
                for c in 0..n_cols {
                    if cell_h == 0 || cell_w == 0 {
                        continue;
                    }
                    let mut sum = 0.0;
                    for yy in r * cell_h..(r + 1) * cell_h {
                        for xx in c * cell_w..(c + 1) * cell_w {
                            sum += lum[yy * sw + xx];
                        }
                    }
                    means.push(sum / (cell_h * cell_w) as f64);
                    let dy = (r as f64 - (n_rows - 1) as f64 / 2.0) / ((n_rows - 1) as f64 / 2.0);
                    let dx = (c as f64 - (n_cols - 1) as f64 / 2.0) / ((n_cols - 1) as f64 / 2.0);
                    let dist = (dx * dx + dy * dy).sqrt() / 2.0f64.sqrt();
                    weights.push(0.5 * (1.0 + (std::f64::consts::PI * dist).cos()));
                }
            }
            let wsum: f64 = weights.iter().sum();
            means.iter().zip(&weights).map(|(m, w)| m * w / wsum).sum()
        }

        "multi_zone" => {
            // Three concentric rings weighted 50/30/20.
            let rings = [(0.00, 0.05, 0.50), (0.05, 0.25, 0.30), (0.25, 0.50, 0.20)];
            let mut weighted_sum = 0.0;
            let mut weight_total = 0.0;
            for &(r_min, r_max, weight) in &rings {
                let mut sum = 0.0;
                let mut count = 0usize;
                for y in 0..sh {
                    let ny = norm_coord(y, sh, max_dim);
                    for x in 0..sw {
                        let nx = norm_coord(x, sw, max_dim);
                        let radius = (nx * nx + ny * ny).sqrt();
                        if radius >= r_min && radius < r_max {
                            sum += lum[y * sw + x];
                            count += 1;
                        }
                    }
                }
                if count == 0 {
                    continue;
                }
                weighted_sum += weight * (sum / count as f64);
                weight_total += weight;
            }
            if weight_total > 0.0 {
                weighted_sum / weight_total
            } else {
                lum.iter().sum::<f64>() / lum.len() as f64
            }
        }

        "highlight_weighted" => {
            // Bias toward bright pixels (weight = Y²) to protect highlights.
            let mut weighted = 0.0;
            let mut total = 0.0;
            for &y in lum {
                let w = y * y;
                weighted += y * w;
                total += w;
            }
            if total < 1e-12 {
                lum.iter().sum::<f64>() / lum.len() as f64
            } else {
                weighted / total
            }
        }

        // Python's `else` branch sets `exposure = 1.0` (already post-`/0.184`),
        // i.e. 0 EV. Returning the mid-grey target makes that division cancel.
        _ => 0.184,
    }
}

/// Per-pixel `raw = m · rgb` for the Mallett2019 path (f64 matmul, rayon-parallel).
fn apply_mallett_matrix(image: &ImageBuf, m: &[[f64; 3]; 3]) -> ImageBuf {
    let mut out = image.clone();
    out.data
        .par_chunks_exact_mut(3)
        .zip(image.data.par_chunks_exact(3))
        .for_each(|(dst, src)| {
            let rgb = [src[0] as f64, src[1] as f64, src[2] as f64];
            let v = crate::mallett::apply(m, rgb);
            dst[0] = from_f64(v[0]);
            dst[1] = from_f64(v[1]);
            dst[2] = from_f64(v[2]);
        });
    out
}

fn decode_input_in_place(image: &mut ImageBuf, io: &crate::params::IoParams) {
    let cctf = io.input_cctf();
    if cctf != colorspace::Cctf::Linear {
        image
            .data
            .par_iter_mut()
            .for_each(|value| *value = cctf.decode(*value));
    }
}

/// Expose: convert RGB to film raw exposure.
///
/// Dispatches on the configured upsampler: the Mallett2019 per-pixel matrix
/// (when `mallett_core` is set), the full Hanatos2025 spectral path (when a
/// TC LUT is provided), or a simplified RGB → log10 fallback.
#[allow(clippy::too_many_arguments)]
pub fn expose(
    image: &ImageBuf,
    film: &Profile,
    params: &RuntimeParams,
    backend: &dyn ComputeBackend,
    tc_lut: Option<&TcLut>,
    mallett_core: Option<&[[f64; 3]; 3]>,
    front_illuminant: &[f32],
    bw_filming_correction: f64,
) -> ImageBuf {
    let pix_um = pixel_size_um(params.camera.film_format_mm, image.width, image.height);
    expose_working(
        image,
        film,
        params,
        backend,
        tc_lut,
        mallett_core,
        front_illuminant,
        bw_filming_correction,
        pix_um,
        autoexposure_scale(image, params),
    )
}

pub(crate) fn autoexposure_scale(image: &ImageBuf, params: &RuntimeParams) -> f64 {
    if !params.camera.auto_exposure {
        return 1.0;
    }
    let rgb_to_xyz = spectral::colorspace_to_xyz_f64(&params.io.input_color_space);
    let ae_ev = measure_autoexposure_ev_with_cctf(
        image,
        rgb_to_xyz[1],
        &params.camera.auto_exposure_method,
        params.io.input_cctf(),
    );
    ae_ev.exp2()
}

pub(crate) fn apply_autoexposure_scale(image: &mut ImageBuf, scale: f64) {
    if scale != 1.0 {
        let scale = from_f64(scale);
        image.data.par_iter_mut().for_each(|value| *value *= scale);
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn expose_working(
    image: &ImageBuf,
    film: &Profile,
    params: &RuntimeParams,
    backend: &dyn ComputeBackend,
    tc_lut: Option<&TcLut>,
    mallett_core: Option<&[[f64; 3]; 3]>,
    front_illuminant: &[f32],
    bw_filming_correction: f64,
    pix_um: f32,
    autoexposure_scale: f64,
) -> ImageBuf {
    let mut rgb = image.clone();
    apply_autoexposure_scale(&mut rgb, autoexposure_scale); // Python meters decoded luminance and applies its scale before decoding
    // the resulting working pixels for exposure.
    decode_input_in_place(&mut rgb, &params.io);
    // Exposure compensation
    let exp_comp = from_f32(2.0f32.powf(params.camera.exposure_compensation_ev));
    rgb.data.par_iter_mut().for_each(|v| *v *= exp_comp);

    // RGB → film raw exposure
    let mut raw = if let Some(core) = mallett_core {
        // Mallett2019: per-pixel `raw = (core · M_cs) · rgb`, a single 3×3
        // matrix folding the input-colour-space → linear-sRGB conversion.
        let m = crate::mallett::film_matrix(core, &params.io.input_color_space);
        apply_mallett_matrix(&rgb, &m)
    } else if let Some(lut) = tc_lut {
        // Full Hanatos2025 spectral upsampling with CAT02 adaptation.
        backend.hanatos2025_rgb_to_raw(
            &rgb,
            lut,
            &params.io.input_color_space,
            front_illuminant,
            params.settings.use_cat16,
        )
    } else {
        // Simplified fallback: treat RGB values as proportional to raw exposure
        rgb.clone()
    };

    // Order mirrors Python filming: boost → diffusion → lens_blur → halation.

    // Highlight boost: reconstruct pre-clip highlight irradiance before the
    // optical-scatter effects. No-op when boost_ev == 0.
    let hal_boost = &params.film_render.halation;
    if hal_boost.boost_ev != 0.0 {
        raw = spektrafilm_model::diffusion::boost_highlights(
            &raw,
            hal_boost.boost_ev as f64,
            hal_boost.boost_range as f64,
            hal_boost.protect_ev as f64,
        );
    }

    // Diffusion filter (camera): lens diffusion-filter PSF on linear raw.
    // GPU uses a downsampled sum-of-Gaussians (fast preview); CPU keeps the
    // exact FFT convolution (export parity).
    let df = &params.camera.diffusion_filter;
    if df.active {
        let dm = df.to_model();
        raw = if backend.is_gpu() {
            spektrafilm_model::diffusion::apply_diffusion_filter_blur(
                &raw,
                &dm,
                pix_um as f64,
                backend,
            )
        } else {
            spektrafilm_model::diffusion::apply_diffusion_filter_um(&raw, &dm, pix_um as f64)
        };
    }

    // Lens blur
    if params.camera.lens_blur_um > 0.0 {
        raw = spektrafilm_model::diffusion::apply_gaussian_blur_um(
            &raw,
            params.camera.lens_blur_um,
            pix_um,
            backend,
        );
    }

    // Halation (on linear raw)
    let halation = &params.film_render.halation;
    if halation.active {
        let (strength, sigma_um) = halation.strength_and_sigma(film);
        raw = spektrafilm_model::diffusion::apply_halation_um(
            &raw,
            pix_um,
            halation.scatter_amount,
            halation.scatter_spatial_scale,
            halation.scatter_core_um,
            halation.scatter_tail_um,
            halation.scatter_tail_weight,
            halation.halation_amount,
            halation.halation_spatial_scale,
            strength,
            sigma_um,
            halation.halation_n_bounces,
            halation.halation_bounce_decay,
            halation.halation_renormalize,
            backend,
        );
    }

    // B&W / slide scanner exposure correction (Python applies this last,
    // before the log10). No-op at factor 1.0.
    if bw_filming_correction != 1.0 {
        let f = from_f64(bw_filming_correction);
        raw.data.par_iter_mut().for_each(|v| *v *= f);
    }

    // Convert to log10 exposure. Mirror Python's
    // `np.log10(np.fmax(raw, 0.0) + 1e-10)` exactly — adding 1e-10
    // after the floor-at-zero shifts every value by a constant in
    // log-space and is *not* the same as `log10(max(raw, 1e-10))`.
    let zero = from_f64(0.0);
    let eps = from_f64(1e-10);
    raw.data.par_iter_mut().for_each(|v| {
        *v = ((*v).max(zero) + eps).log10();
    });

    raw
}

pub(crate) fn layered_grain_params(
    params: &RuntimeParams,
    film: &Profile,
    pix_um: f32,
) -> LayeredGrainParams {
    let grain = &params.film_render.grain;
    LayeredGrainParams {
        pixel_size_um: f64::from(pix_um),
        particle_area_um2: grain.agx_particle_area_um2,
        particle_scale: grain.agx_particle_scale,
        particle_scale_layers: grain.agx_particle_scale_layers,
        density_min: grain.density_min,
        uniformity: grain.uniformity,
        blur_px: f64::from(grain.blur),
        blur_dye_clouds_um: f64::from(grain.blur_dye_clouds_um),
        micro_structure: grain.micro_structure.map(f64::from),
        positive_film: film.is_positive(),
        monochrome: film.is_bw(),
        use_fast_stats: params.settings.use_fast_stats,
    }
}

/// Develop: log_raw → density_cmy via density curves + DIR couplers + grain.
pub fn develop(
    log_raw: &ImageBuf,
    film: &Profile,
    params: &RuntimeParams,
    backend: &dyn ComputeBackend,
) -> ImageBuf {
    let pix_um = pixel_size_um(params.camera.film_format_mm, log_raw.width, log_raw.height);
    develop_with_pixel_size(log_raw, film, params, backend, pix_um)
}

pub(crate) fn develop_with_pixel_size(
    log_raw: &ImageBuf,
    film: &Profile,
    params: &RuntimeParams,
    backend: &dyn ComputeBackend,
    pix_um: f32,
) -> ImageBuf {
    let stage_timings = stage_timings_enabled();
    // f64 chain for Python parity — curves are f64 in the profile JSON.
    let log_exposure_f64 = film.log_exposure_f64();
    let density_curves_f64 = film.density_curves_f64();
    // f32 versions kept for DIR couplers (still f32 API).
    let log_exposure = &film.log_exposure_f32();
    let density_curves = &film.density_curves_f32();
    let gamma = params.film_render.density_curve_gamma;

    // Filming.develop uses NORMALIZED curves (Python `develop` subtracts nanmin).
    let norm_curves_f64 =
        spektrafilm_model::density_curves::normalize_density_curves_f64(&density_curves_f64);
    let t = Instant::now();
    let mut density_cmy =
        backend.density_curve_interp(log_raw, &log_exposure_f64, &norm_curves_f64, gamma as f64);
    print_stage_timing(stage_timings, "filming_develop.density_interp", t);

    // DIR couplers
    let dir = &params.film_render.dir_couplers;
    if dir.active {
        let t = Instant::now();
        let matrix = dir_couplers_matrix(film, dir);
        density_cmy = spektrafilm_model::couplers::apply_density_correction(
            &density_cmy,
            log_raw,
            pix_um,
            log_exposure,
            density_curves,
            &matrix,
            dir.amount,
            dir.diffusion_size_um,
            dir.diffusion_tail_um,
            dir.diffusion_tail_weight,
            film.is_positive(),
            gamma,
            backend,
        );
        print_stage_timing(stage_timings, "filming_develop.dir_couplers", t);
    }

    // Grain
    let grain = &params.film_render.grain;
    if grain.active {
        let t = Instant::now();
        let layered = if grain.sublayers_active {
            grain::apply_grain_to_density_layers(
                &density_cmy,
                &norm_curves_f64,
                &film.data.density_curves_layers,
                &layered_grain_params(params, film, pix_um),
            )
        } else {
            None
        };
        density_cmy = layered.unwrap_or_else(|| {
            let density_max = spektrafilm_model::density_curves::max_density_f64(&norm_curves_f64);
            grain::apply_grain_to_density(
                &density_cmy,
                pix_um,
                grain.agx_particle_area_um2,
                grain.agx_particle_scale,
                grain.density_min,
                density_max,
                grain.uniformity,
                grain.blur,
                grain.n_sub_layers,
                grain.monochrome,
                backend,
            )
        });
        print_stage_timing(stage_timings, "filming_develop.grain", t);
    }

    density_cmy
}

/// Resolve per-film DIR defaults before building the same matrix for CPU and GPU.
pub(crate) fn dir_couplers_matrix(film: &Profile, dir: &DirCouplersParams) -> [[f64; 3]; 3] {
    // Stock-specific defaults from Python 0.3.4 _apply_film_specifics().
    let (same, r_to_gb, g_to_rb, b_to_rg) = match film.info.stock.as_deref() {
        Some("fujifilm_velvia_100") => (
            [0.108, 0.072, 0.054],
            [0.108, 0.054],
            [0.072, 0.054],
            [0.054, 0.054],
        ),
        Some("fujifilm_provia_100f") => (
            [0.156, 0.104, 0.078],
            [0.156, 0.078],
            [0.104, 0.078],
            [0.078, 0.078],
        ),
        _ if film.is_positive() => ([0.12, 0.08, 0.06], [0.12, 0.06], [0.08, 0.06], [0.06, 0.06]),
        _ if film.is_negative() => (
            [0.336, 0.319, 0.273],
            [0.353, 0.302],
            [0.154, 0.353],
            [0.168, 0.226],
        ),
        _ => (
            [0.341, 0.324, 0.273],
            [0.355, 0.305],
            [0.154, 0.358],
            [0.171, 0.225],
        ),
    };
    let same = dir.gamma_samelayer_rgb.unwrap_or(same);
    let (same, r_to_gb, g_to_rb, b_to_rg) = if film.is_bw() {
        // Broadcast single-layer self-inhibition across the three-channel engine.
        ([same[0]; 3], [0.0; 2], [0.0; 2], [0.0; 2])
    } else {
        (
            same,
            dir.gamma_interlayer_r_to_gb.unwrap_or(r_to_gb),
            dir.gamma_interlayer_g_to_rb.unwrap_or(g_to_rb),
            dir.gamma_interlayer_b_to_rg.unwrap_or(b_to_rg),
        )
    };
    spektrafilm_model::couplers::compute_dir_couplers_matrix(
        same,
        r_to_gb,
        g_to_rb,
        b_to_rg,
        dir.inhibition_samelayer,
        dir.inhibition_interlayer,
    )
}

/// Full filming stage: expose + develop.
pub fn process(
    image: &ImageBuf,
    film: &Profile,
    params: &RuntimeParams,
    backend: &dyn ComputeBackend,
    tc_lut: Option<&TcLut>,
) -> ImageBuf {
    let ref_illuminant = select_illuminant(&film.info.reference_illuminant);
    let log_raw = expose(
        image,
        film,
        params,
        backend,
        tc_lut,
        None,
        ref_illuminant,
        1.0,
    );
    develop(&log_raw, film, params, backend)
}

pub fn input_colorspace_to_xyz(name: &str) -> [[f32; 3]; 3] {
    spectral::colorspace_to_xyz(name)
}

pub(crate) fn select_illuminant(name: &str) -> &'static [f32] {
    spectral::named_illuminant(name).map_or(&spectral::ILLUMINANT_D55, |illum| illum.samples)
}

#[cfg(test)]
mod tests {
    use super::*;
    use spektrafilm_math::precision::from_f64;

    #[test]
    fn develop_uses_profiled_grain_layers_with_aggregate_fallback() {
        use spektrafilm_gpu::cpu_backend::CpuBackend;

        let data_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../data");
        let log_raw = ImageBuf::from_data(16, 16, vec![from_f64(-0.5); 16 * 16 * 3]);
        let mut params = RuntimeParams::default();
        params.film_render.dir_couplers.active = false;
        for stock in ["kodak_portra_400", "fujifilm_velvia_100", "kodak_doublex"] {
            let film = crate::profile::load_profile_by_name(&data_dir, stock).unwrap();
            let mut film = crate::profile::resolve_for_render(film, None);
            params.film_render.grain.monochrome = film.is_bw();
            if film.is_bw() {
                let g = &mut params.film_render.grain;
                g.agx_particle_scale = [g.agx_particle_scale[0]; 3];
                g.uniformity = [g.uniformity[0]; 3];
            }
            let develop = |film: &Profile, p: &RuntimeParams| {
                develop_with_pixel_size(&log_raw, film, p, &CpuBackend, 2.0)
            };

            params.film_render.grain.sublayers_active = false;
            let aggregate = develop(&film, &params);
            params.film_render.grain.sublayers_active = true;
            let layered = develop(&film, &params);
            assert!(layered.data.iter().all(|value| value.is_finite()));
            assert_ne!(aggregate.data, layered.data, "stock {stock}");
            if film.is_bw() {
                assert!(layered.pixels().all(|px| px[0] == px[1] && px[0] == px[2]));
            }

            film.data.density_curves_layers[0][0].pop();
            assert_eq!(
                develop(&film, &params).data,
                aggregate.data,
                "stock {stock}"
            );
            film.data.density_curves_layers.clear();
            assert_eq!(
                develop(&film, &params).data,
                aggregate.data,
                "stock {stock}"
            );
        }
    }

    #[test]
    fn slide_dir_stock_defaults_preserve_partially_overridden_json() {
        let data_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../data");
        let provia =
            crate::profile::load_profile_by_name(&data_dir, "fujifilm_provia_100f").unwrap();
        let velvia =
            crate::profile::load_profile_by_name(&data_dir, "fujifilm_velvia_100").unwrap();

        let defaults = DirCouplersParams::default();
        assert_eq!(
            dir_couplers_matrix(&provia, &defaults),
            [
                [0.156, 0.156, 0.078],
                [0.104, 0.104, 0.078],
                [0.078, 0.078, 0.078],
            ]
        );
        assert_eq!(
            dir_couplers_matrix(&velvia, &defaults),
            [
                [0.108, 0.108, 0.054],
                [0.072, 0.072, 0.054],
                [0.054, 0.054, 0.054],
            ]
        );

        let overrides: DirCouplersParams =
            serde_json::from_str(r#"{"gamma_interlayer_r_to_gb":[0.355,0.305]}"#).unwrap();
        let encoded = serde_json::to_string(&overrides).unwrap();
        let roundtrip: DirCouplersParams = serde_json::from_str(&encoded).unwrap();
        assert_eq!(
            dir_couplers_matrix(&provia, &roundtrip),
            [
                [0.156, 0.355, 0.305],
                [0.104, 0.104, 0.078],
                [0.078, 0.078, 0.078],
            ]
        );
    }

    /// Deterministic synthetic image, mirrored bit-for-bit in the Python
    /// reference: a 300×200 gradient where each channel is
    /// `((x*7 + y*13 + c*29) % 100) / 100`.
    fn synthetic_image() -> ImageBuf {
        let (w, h) = (300usize, 200usize);
        let mut data = Vec::with_capacity(w * h * 3);
        for y in 0..h {
            for x in 0..w {
                for c in 0..3 {
                    let v = ((x * 7 + y * 13 + c * 29) % 100) as f64 / 100.0;
                    data.push(from_f64(v));
                }
            }
        }
        ImageBuf {
            width: w as u32,
            height: h as u32,
            data,
        }
    }

    /// Every metering pattern matches the upstream `measure_autoexposure_ev`
    /// (`spektrafilm/utils/autoexposure.py`) on the synthetic image, with the
    /// sRGB RGB→XYZ matrix and `apply_cctf_decoding=False`. The reference is
    /// metered on the ≤256 px `small_preview` downsample.
    #[test]
    fn autoexposure_methods_match_python_reference() {
        let img = synthetic_image();
        let rgb_to_xyz = input_colorspace_to_xyz("sRGB");
        let cases = [
            ("average", -1.427705567203),
            ("median", -1.308011314552),
            ("center_weighted", -1.427664142652),
            ("partial", -1.425832251352),
            ("matrix", -1.427803049578),
            ("multi_zone", -1.439398829551),
            ("highlight_weighted", -1.783882472180),
        ];
        for (method, expected) in cases {
            let ev = measure_autoexposure_ev(&img, &rgb_to_xyz, method);
            assert!(
                (ev as f64 - expected).abs() < 1e-5,
                "method {method}: got {ev}, expected {expected}",
            );
        }
    }

    #[test]
    fn metering_antialiases_float_samples_before_decoding() {
        let mut checker = ImageBuf::new(512, 384);
        for (y, row) in checker.data.chunks_exact_mut(512 * 3).enumerate() {
            for (x, pixel) in row.chunks_exact_mut(3).enumerate() {
                let level = if (x + y) % 2 == 0 { 0.02 } else { 0.8 };
                pixel.fill(from_f64(level));
            }
        }
        let luminance = spectral::colorspace_to_xyz_f64("sRGB")[1];
        // Python ResizingService.small_preview then measure_autoexposure_ev.
        for (cctf, expected) in [
            (colorspace::Cctf::Linear, -0.612999529688),
            (colorspace::Cctf::Srgb, 1.51519523618),
        ] {
            let ev =
                measure_autoexposure_ev_with_cctf(&checker, luminance, "center_weighted", cctf);
            assert!(
                (ev - expected).abs() < 1e-6,
                "got {ev}, expected {expected}"
            );
        }
    }

    /// An unknown method name meters a flat 1.0 → 0 EV, matching the Python
    /// `else` branch (`exposure = 1.0`).
    #[test]
    fn autoexposure_unknown_method_is_zero_ev() {
        let img = synthetic_image();
        let rgb_to_xyz = input_colorspace_to_xyz("sRGB");
        let ev = measure_autoexposure_ev(&img, &rgb_to_xyz, "bogus");
        assert_eq!(ev, 0.0);
    }
}
