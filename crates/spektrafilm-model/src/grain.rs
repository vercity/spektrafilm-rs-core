// Stochastic grain generation.
// Poisson-binomial particle model with dye cloud blur and lognormal micro-structure.

use rayon::prelude::*;
use spektrafilm_gpu::ComputeBackend;
use spektrafilm_math::gaussian::gaussian_blur_channel_in_place;
use spektrafilm_math::image::ImageBuf;
use spektrafilm_math::numpy_rng::{GaussRng, rk_binomial, rk_double, rk_poisson};
use spektrafilm_math::precision::{Scalar, ZERO, from_f64, to_f64};
use std::time::Instant;

fn stage_timings_enabled() -> bool {
    std::env::var_os("SPEKTRAFILM_STAGE_TIMINGS").is_some()
}

fn print_stage_timing(enabled: bool, stage: &str, start: Instant) {
    if enabled {
        eprintln!("stage {stage}: {} ms", start.elapsed().as_millis());
    }
}

/// Apply the Poisson-binomial grain particle model to a single-channel density image.
///
/// Port of Python `layer_particle_model`. Density values flow through in `Scalar`
/// (full f64 precision in `precision-f64` mode); RNG sampling uses f64 directly.
pub fn layer_particle_model(
    density: &[Scalar],
    width: u32,
    height: u32,
    density_max: f64,
    n_particles_per_pixel: f64,
    grain_uniformity: f64,
    seed: u64,
    blur_particle: f32,
) -> Vec<Scalar> {
    let layer = ParticleLayer {
        density_max,
        n_particles_per_pixel,
        uniformity: grain_uniformity,
    };
    let probabilities: Vec<_> = density
        .iter()
        .map(|&d| layer.probability(to_f64(d)))
        .collect();
    let mut grain = layer.sample(&probabilities, seed as u32, false);

    if blur_particle > 0.4 {
        let sigma = (f64::from(blur_particle) * layer.particle_density().sqrt()) as f32;
        if sigma > 0.1 {
            gaussian_blur_channel_in_place(&mut grain, width, height, from_f64(f64::from(sigma)));
        }
    }

    grain
}

#[derive(Clone, Copy)]
struct ParticleLayer {
    density_max: f64,
    n_particles_per_pixel: f64,
    uniformity: f64,
}

impl ParticleLayer {
    fn particle_density(self) -> f64 {
        self.density_max / self.n_particles_per_pixel
    }

    fn probability(self, density: f64) -> f64 {
        (density / self.density_max).clamp(1e-6, 1.0 - 1e-6)
    }

    fn saturation(self, p: f64) -> f64 {
        1.0 - p * self.uniformity * (1.0 - 1e-6)
    }

    fn sample(self, probabilities: &[f64], seed: u32, use_fast_stats: bool) -> Vec<Scalar> {
        let mut rng = GaussRng::new(seed);
        // NumPy draws every Poisson sample before the first binomial draw.
        let seeds: Vec<_> = probabilities
            .iter()
            .map(|&p| {
                let lambda = self.n_particles_per_pixel / self.saturation(p);
                if use_fast_stats && lambda >= 30.0 {
                    (lambda + lambda.sqrt() * rng.gauss())
                        .round_ties_even()
                        .max(0.0) as u64
                } else {
                    rk_poisson(&mut rng.rng, lambda)
                }
            })
            .collect();
        let particle_density = self.particle_density();
        seeds
            .into_iter()
            .zip(probabilities)
            .map(|(n, &p)| {
                let developed = if use_fast_stats {
                    fast_binomial(&mut rng, n, p)
                } else {
                    rk_binomial(&mut rng.rng, n, p)
                };
                from_f64(developed as f64 * particle_density * self.saturation(p))
            })
            .collect()
    }
}

fn fast_binomial(rng: &mut GaussRng, n: u64, p: f64) -> u64 {
    if n < 25 {
        return (0..n).filter(|_| rk_double(&mut rng.rng) < p).count() as u64;
    }
    let mean = n as f64 * p;
    let variance = mean * (1.0 - p);
    if variance > 10.0 {
        return (mean + variance.sqrt() * rng.gauss())
            .round_ties_even()
            .clamp(0.0, n as f64) as u64;
    }
    // NumPy's exact sampler handles p near 1 without underflow in (1 - p)^n.
    rk_binomial(&mut rng.rng, n, p)
}

/// Film-grain settings used by the three-emulsion model.
pub struct LayeredGrainParams {
    pub pixel_size_um: f64,
    pub particle_area_um2: f64,
    pub particle_scale: [f64; 3],
    pub particle_scale_layers: [f64; 3],
    pub density_min: [f64; 3],
    pub uniformity: [f64; 3],
    pub blur_px: f64,
    pub blur_dye_clouds_um: f64,
    /// Clumping blur in µm and linear-space deviation in nm.
    pub micro_structure: [f64; 2],
    pub positive_film: bool,
    pub monochrome: bool,
    pub use_fast_stats: bool,
}

/// Apply the profiled emulsion grain model. An incompatible profile can use
/// the aggregate model when its layer curves are absent or unusable.
pub fn apply_grain_to_density_layers(
    density: &ImageBuf,
    density_curves: &[[f64; 3]],
    density_curves_layers: &[Vec<Vec<f64>>],
    params: &LayeredGrainParams,
) -> Option<ImageBuf> {
    let n_channels = if params.monochrome { 1 } else { 3 };
    if density_curves.len() < 2
        || density_curves_layers.len() != density_curves.len()
        || density_curves_layers
            .iter()
            .any(|row| row.len() != 3 || row.iter().any(|layer| layer.len() != n_channels))
    {
        return None;
    }

    let direction = if params.positive_film { -1.0 } else { 1.0 };
    let curves: Vec<_> = (0..n_channels)
        .map(|ch| LayerCurves::new(density_curves, density_curves_layers, ch, direction))
        .collect::<Option<_>>()?;
    let width = density.width;
    let height = density.height;
    let pixel_area = params.pixel_size_um * params.pixel_size_um;
    let mut out = ImageBuf::new(width, height);

    // Keep only a channel accumulator and one layer's samples in memory.
    for (ch, curves) in curves.iter().enumerate() {
        let mut sum = vec![ZERO; density.pixel_count()];
        let total_max: f64 = curves.maxima.iter().sum();
        let density_min = params.density_min[ch];
        for (sl, &maximum) in curves.maxima.iter().enumerate() {
            let fraction = maximum / total_max;
            let layer_floor = fraction * density_min;
            let particle_area = params.particle_area_um2
                * params.particle_scale[ch]
                * params.particle_scale_layers[sl];
            let layer = ParticleLayer {
                density_max: maximum + layer_floor,
                n_particles_per_pixel: pixel_area * fraction / particle_area,
                uniformity: params.uniformity[ch],
            };
            let probabilities: Vec<_> = density
                .data
                .par_chunks_exact(3)
                .map(|px| {
                    layer.probability(curves.evaluate(direction * to_f64(px[ch]), sl) + layer_floor)
                })
                .collect();
            let seed = (ch + sl * 10) as u32;
            let mut grain = layer.sample(&probabilities, seed, params.use_fast_stats);
            drop(probabilities);

            if params.blur_dye_clouds_um > 0.0 {
                let sigma = params.blur_dye_clouds_um * layer.particle_density().sqrt();
                gaussian_blur_channel_in_place(&mut grain, width, height, from_f64(sigma));
            }
            for (sum, grain) in sum.iter_mut().zip(grain) {
                *sum += grain;
            }
        }

        add_micro_structure(&mut sum, width, height, params, ch);
        let density_min = from_f64(density_min);
        for value in &mut sum {
            *value -= density_min;
        }
        if params.blur_px > 0.0 {
            gaussian_blur_channel_in_place(&mut sum, width, height, from_f64(params.blur_px));
        }
        if params.monochrome {
            for (pixel, value) in out.pixels_mut().zip(sum) {
                pixel.fill(value);
            }
        } else {
            out.write_channel(ch, &sum);
        }
    }
    Some(out)
}

struct LayerCurves {
    axis: Vec<f64>,
    inverse_delta: Vec<f64>,
    values: Vec<[f64; 3]>,
    maxima: [f64; 3],
}

impl LayerCurves {
    fn new(
        density_curves: &[[f64; 3]],
        density_curves_layers: &[Vec<Vec<f64>>],
        ch: usize,
        direction: f64,
    ) -> Option<Self> {
        let axis: Vec<_> = density_curves
            .iter()
            .map(|row| row[ch] * direction)
            .collect();
        if axis[0] >= axis[axis.len() - 1] || axis.iter().any(|x| !x.is_finite()) {
            return None;
        }
        let mut maxima = [0.0_f64; 3];
        let mut values = Vec::with_capacity(axis.len());
        for row in density_curves_layers {
            let layers = [row[0][ch], row[1][ch], row[2][ch]];
            for (max, &value) in maxima.iter_mut().zip(&layers) {
                if !value.is_finite() {
                    return None;
                }
                *max = max.max(value);
            }
            values.push(layers);
        }
        if maxima.contains(&0.0) || !maxima.iter().sum::<f64>().is_finite() {
            return None;
        }
        let inverse_delta = axis
            .windows(2)
            .map(|pair| {
                let dx = pair[1] - pair[0];
                if dx == 0.0 { 0.0 } else { 1.0 / dx }
            })
            .collect();
        Some(Self {
            axis,
            inverse_delta,
            values,
            maxima,
        })
    }

    fn evaluate(&self, density: f64, layer: usize) -> f64 {
        let last = self.axis.len() - 1;
        if density <= self.axis[0] || density.is_nan() {
            return self.values[0][layer];
        }
        if density >= self.axis[last] {
            return self.values[last][layer];
        }

        // Match Python's right-biased binary search, including the search
        // order when measured stock curves have small reversals at the ends.
        let mut left = 0;
        let mut right = self.axis.len();
        while left < right {
            let mid = left + (right - left) / 2;
            if density < self.axis[mid] {
                right = mid;
            } else {
                left = mid + 1;
            }
        }
        let lo = left - 1;
        let t = (density - self.axis[lo]) * self.inverse_delta[lo];
        self.values[lo][layer] + t * (self.values[left][layer] - self.values[lo][layer])
    }
}

fn add_micro_structure(
    channel: &mut [Scalar],
    width: u32,
    height: u32,
    params: &LayeredGrainParams,
    ch: usize,
) {
    let [blur_um, deviation_nm] = params.micro_structure;
    let deviation = deviation_nm * 0.001 / params.pixel_size_um;
    if deviation <= 0.05 {
        return;
    }
    // Convert linear mean 1 and standard deviation to log-normal parameters.
    let variance_log = (deviation * deviation).ln_1p();
    let sigma = variance_log.sqrt();
    let mu = -variance_log / 2.0;
    // Python's Numba sampler owns an independent RNG. Use separate stable
    // seeds for each dye channel so CPU renders are reproducible.
    let mut rng = GaussRng::new(30 + ch as u32);
    let mut clumping: Vec<_> = (0..channel.len())
        .map(|_| from_f64((mu + sigma * rng.gauss()).exp()))
        .collect();
    let blur_px = blur_um / params.pixel_size_um;
    if blur_px > 0.4 {
        gaussian_blur_channel_in_place(&mut clumping, width, height, from_f64(blur_px));
    }
    for (density, clump) in channel.iter_mut().zip(clumping) {
        *density *= clump;
    }
}

/// Apply grain to a CMY density image.
///
/// Port of Python `apply_grain_to_density`.
#[allow(clippy::too_many_arguments)]
pub fn apply_grain_to_density(
    density_cmy: &ImageBuf,
    pixel_size_um: f32,
    agx_particle_area_um2: f64,
    agx_particle_scale: [f64; 3],
    density_min: [f64; 3],
    density_max_curves: [f64; 3],
    grain_uniformity: [f64; 3],
    grain_blur: f32,
    n_sub_layers: u32,
    monochrome: bool,
    backend: &dyn ComputeBackend,
) -> ImageBuf {
    let stage_timings = stage_timings_enabled();
    let w = density_cmy.width;
    let h = density_cmy.height;
    let pixel_area = (pixel_size_um as f64) * (pixel_size_um as f64);
    let density_max: [f64; 3] = [
        density_max_curves[0] + density_min[0],
        density_max_curves[1] + density_min[1],
        density_max_curves[2] + density_min[2],
    ];

    let channels: Vec<(usize, Vec<Scalar>)> = (0..3)
        .into_par_iter()
        .map(|ch| {
            let t_ch = Instant::now();
            let particle_area = agx_particle_area_um2 * agx_particle_scale[ch];
            let mut n_particles = pixel_area / particle_area;
            if n_sub_layers > 1 {
                n_particles /= n_sub_layers as f64;
            }

            // Add density_min to input (kept in Scalar precision)
            let dmin_s = from_f64(density_min[ch]);
            let t = Instant::now();
            let density_ch: Vec<Scalar> = density_cmy.pixels().map(|px| px[ch] + dmin_s).collect();
            print_stage_timing(stage_timings, "grain.extract_channel", t);

            let mut grain_sum = vec![ZERO; density_ch.len()];

            for sl in 0..n_sub_layers {
                let t = Instant::now();
                // Monochrome (B&W): one shared noise field — every lane draws
                // from channel 0's RNG stream (upstream n_channels==1 has a
                // single emulsion, seed ch=0).
                let seed_ch = if monochrome { 0 } else { ch as u64 };
                let seed = seed_ch + (sl as u64) * 10;
                let g = layer_particle_model(
                    &density_ch,
                    w,
                    h,
                    density_max[ch],
                    n_particles,
                    grain_uniformity[ch],
                    seed,
                    0.0,
                );
                print_stage_timing(stage_timings, "grain.layer_particle_model", t);
                for (s, &v) in grain_sum.iter_mut().zip(g.iter()) {
                    *s += v;
                }
            }

            // Average sub-layers
            let scale = from_f64(1.0 / n_sub_layers as f64);
            for v in &mut grain_sum {
                *v = *v * scale - dmin_s;
            }

            print_stage_timing(stage_timings, "grain.channel_total", t_ch);
            (ch, grain_sum)
        })
        .collect();

    let mut out = ImageBuf::new(w, h);
    for (ch, grain_sum) in channels {
        out.write_channel(ch, &grain_sum);
    }

    // Final blur — typically a few px sigma at 1–6 MP, big enough that the
    // GPU separable kernel wins.
    if grain_blur > 0.4 {
        let t = Instant::now();
        out = backend.gaussian_blur(&out, grain_blur);
        print_stage_timing(stage_timings, "grain.final_blur", t);
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const CURVE: [[f64; 3]; 4] = [[0.0; 3], [0.5; 3], [0.5; 3], [2.0; 3]];

    fn emulsion_layers() -> Vec<Vec<Vec<f64>>> {
        [
            [0.0, 0.0, 0.0],
            [0.4, 0.09, 0.01],
            [0.2, 0.2, 0.1],
            [0.3, 0.7, 1.0],
        ]
        .map(|row| row.map(|v| vec![v; 3]).to_vec())
        .to_vec()
    }

    #[test]
    fn grain_density_mapping_matches_negative_and_positive_curves() {
        let layers = emulsion_layers();
        let negative = LayerCurves::new(&CURVE, &layers, 0, 1.0).unwrap();
        assert_eq!(negative.evaluate(-0.25, 2), 0.0);
        assert_eq!(negative.evaluate(0.5, 0), 0.2);
        assert!((negative.evaluate(1.25, 2) - 0.55).abs() < 1e-12);
        assert_eq!(negative.evaluate(2.25, 2), 1.0);

        // An exact interior match selects the rightmost equal density in
        // exposure order. Reversing a duplicate reverses which layer wins.
        let mut positive_axis = CURVE;
        let mut positive_layers = layers;
        positive_axis.reverse();
        positive_layers.reverse();
        let positive = LayerCurves::new(&positive_axis, &positive_layers, 0, -1.0).unwrap();
        assert_eq!(positive.evaluate(-2.25, 2), 1.0);
        assert!((positive.evaluate(-1.25, 2) - 0.55).abs() < 1e-12);
        assert_eq!(positive.evaluate(-0.5, 0), 0.4);
        assert_eq!(positive.evaluate(0.25, 2), 0.0);
    }

    #[test]
    fn grain_layer_controls_preserve_tone_and_change_texture() {
        let density = ImageBuf::from_data(80, 60, vec![from_f64(1.25); 80 * 60 * 3]);
        let layers = emulsion_layers();
        let mut params = LayeredGrainParams {
            pixel_size_um: 0.5,
            particle_area_um2: 0.2,
            particle_scale: [1.6, 1.6, 3.2],
            particle_scale_layers: [2.0, 1.0, 0.5],
            density_min: [0.03; 3],
            uniformity: [0.97, 0.99, 0.97],
            blur_px: 0.0,
            blur_dye_clouds_um: 0.0,
            micro_structure: [0.0; 2],
            positive_film: false,
            monochrome: false,
            use_fast_stats: false,
        };
        let render = |p: &LayeredGrainParams| {
            apply_grain_to_density_layers(&density, &CURVE, &layers, p).unwrap()
        };
        let stats = |img: &ImageBuf| {
            let n = img.pixel_count() as f64;
            let mean = img.pixels().map(|px| to_f64(px[0])).sum::<f64>() / n;
            let rms = (img
                .pixels()
                .map(|px| (to_f64(px[0]) - mean).powi(2))
                .sum::<f64>()
                / n)
                .sqrt();
            (mean, rms)
        };

        let baseline = render(&params);
        let (mean, rms) = stats(&baseline);
        assert!((mean - 1.25).abs() < 0.03);
        assert_eq!(baseline.data, render(&params).data);
        // Python 0.3.4 apply_grain_to_density_layers on this constant patch.
        let expected = [
            [1.8526582069440007, -0.03, -0.03],
            [1.6105464490560006, 1.194769501632, 1.995987426816001],
            [1.6105464490560006, -0.03, 1.2251054712960003],
            [0.5975527356480002, 1.5600504995520001, -0.03],
        ];
        for (pixel, reference) in baseline.pixels().zip(expected) {
            for (&actual, expected) in pixel.iter().zip(reference) {
                assert!((to_f64(actual) - expected).abs() < 1e-6);
            }
        }
        params.particle_scale_layers[0] *= 3.0;
        assert!(stats(&render(&params)).1 > rms);
        params.particle_scale_layers[0] /= 3.0;

        params.blur_dye_clouds_um = 1.0;
        assert!(stats(&render(&params)).1 < rms * 0.75);
        params.blur_dye_clouds_um = 0.0;
        params.blur_px = 0.3;
        assert!(stats(&render(&params)).1 < rms);
        params.blur_px = 0.0;

        params.micro_structure = [0.0, 300.0];
        let clumped = render(&params);
        assert!(stats(&clumped).1 > rms);
        params.micro_structure[0] = 0.8;
        assert!(stats(&render(&params)).1 < stats(&clumped).1);

        params.pixel_size_um = 2.0;
        params.micro_structure = [0.8, 30.0];
        let fine = render(&params);
        assert!(stats(&fine).1 < rms * 0.4);
        params.micro_structure = [0.0; 2];
        assert_eq!(fine.data, render(&params).data);
        params.use_fast_stats = true;
        let fast = render(&params);
        assert_ne!(fine.data, fast.data);
        let (fast_mean, fast_rms) = stats(&fast);
        assert!((fast_mean - 1.25).abs() < 0.03);
        assert!((fast_rms / stats(&fine).1 - 1.0).abs() < 0.1);
    }
}
