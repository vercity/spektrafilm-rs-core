use crate::image::ImageBuf;
use crate::precision::{Scalar, ZERO, from_f32, from_f64};
use rayon::prelude::*;

/// Single-channel Gaussian blur with replicated edges.
pub fn gaussian_blur_channel(data: &[Scalar], w: u32, h: u32, sigma: f32) -> Vec<Scalar> {
    let mut channel = data.to_vec();
    gaussian_blur_channel_in_place(&mut channel, w, h, from_f32(sigma));
    channel
}

/// Blur a single channel using one scratch buffer.
pub fn gaussian_blur_channel_in_place(data: &mut [Scalar], w: u32, h: u32, sigma: Scalar) {
    blur_channel::<false>(data, w, h, sigma);
}

/// Match Python's reflected FIR edges and replicated IIR edges.
pub fn gaussian_blur_channel_reflect_in_place(data: &mut [Scalar], w: u32, h: u32, sigma: Scalar) {
    blur_channel::<true>(data, w, h, sigma);
}

fn blur_channel<const REFLECT: bool>(data: &mut [Scalar], w: u32, h: u32, sigma: Scalar) {
    let wu = w as usize;
    let hu = h as usize;
    assert_eq!(data.len(), wu * hu);
    // A truncated FIR kernel with radius zero is an identity.
    if data.is_empty() || (from_f64(3.0) * sigma + from_f64(0.5)) < from_f64(1.0) {
        return;
    }
    let mut tmp = vec![ZERO; wu * hu];
    // Python's FIR starts with the vertical pass.
    let horizontal_first = !REFLECT || sigma >= from_f64(3.0);
    blur_1d_parallel::<REFLECT>(data, &mut tmp, wu, hu, sigma, horizontal_first);
    blur_1d_parallel::<REFLECT>(&tmp, data, wu, hu, sigma, !horizontal_first);
}

/// Sum-of-3-Gaussians approximation of an isotropic 2D exponential PSF
/// `exp(-r / decay) / (2π · decay²)` with replicated edges.
pub fn exponential_filter_channel(
    data: &[Scalar],
    w: u32,
    h: u32,
    decay_constant: f32,
) -> Vec<Scalar> {
    exponential_filter::<false>(data, w, h, f64::from(decay_constant))
}

/// Python's scatter-tail filter, with reflected edges for small radii.
pub fn exponential_filter_channel_reflect(
    data: &[Scalar],
    w: u32,
    h: u32,
    decay_constant: f64,
) -> Vec<Scalar> {
    exponential_filter::<true>(data, w, h, decay_constant)
}

fn exponential_filter<const REFLECT: bool>(
    data: &[Scalar],
    w: u32,
    h: u32,
    decay_constant: f64,
) -> Vec<Scalar> {
    // Python `_EXPONENTIAL_GAUSSIAN_FITS[3]` — amplitude, σ / decay.
    const FIT: [(f64, f64); 3] = [(0.1633, 0.5360), (0.6496, 1.5236), (0.1870, 2.7684)];
    let n = data.len();
    let mut result = vec![ZERO; n];
    let mut component = vec![ZERO; n];
    for &(amp, ratio) in &FIT {
        let sigma = ratio * decay_constant;
        let sigma = if REFLECT {
            from_f64(sigma)
        } else {
            from_f32(sigma as f32)
        };
        component.copy_from_slice(data);
        blur_channel::<REFLECT>(&mut component, w, h, sigma);
        let amp_s = from_f64(amp);
        for (r, &v) in result.iter_mut().zip(component.iter()) {
            *r += amp_s * v;
        }
    }
    result
}

/// Separable Gaussian blur with each image channel processed independently.
pub fn gaussian_blur(img: &ImageBuf, sigma: f32) -> ImageBuf {
    gaussian_blur_image::<false>(img, from_f32(sigma))
}

/// Gaussian blur with reflected FIR edges and replicated IIR edges.
pub fn gaussian_blur_reflect(img: &ImageBuf, sigma: Scalar) -> ImageBuf {
    gaussian_blur_image::<true>(img, sigma)
}

fn gaussian_blur_image<const REFLECT: bool>(img: &ImageBuf, sigma: Scalar) -> ImageBuf {
    if sigma <= ZERO || img.data.is_empty() {
        return img.clone();
    }

    // Process each channel separately for cache-friendly access.
    let mut channels = [0, 1, 2].map(|c| img.extract_channel(c));
    channels.par_iter_mut().for_each(|channel| {
        blur_channel::<REFLECT>(channel, img.width, img.height, sigma);
    });

    let mut out = ImageBuf::new(img.width, img.height);
    for c in 0..3 {
        out.write_channel(c, &channels[c]);
    }
    out
}

/// 1D blur pass — either horizontal (along rows) or vertical (along columns).
fn blur_1d_parallel<const REFLECT: bool>(
    src: &[Scalar],
    dst: &mut [Scalar],
    w: usize,
    h: usize,
    sigma: Scalar,
    horizontal: bool,
) {
    // Python `_dispatch_2d`: `sigma >= SMALL_SIGMA_MAX (=3.0) → IIR`,
    // strictly less → FIR. We use the same `>=` boundary so σ exactly
    // at 3.0 doesn't pick a different branch than Python.
    if sigma < from_f64(3.0) {
        fir_blur_1d::<REFLECT>(src, dst, w, h, sigma, horizontal);
    } else {
        iir_blur_1d(src, dst, w, h, sigma, horizontal);
    }
}

/// FIR Gaussian blur (truncated kernel). Good for small sigma.
fn fir_blur_1d<const REFLECT: bool>(
    src: &[Scalar],
    dst: &mut [Scalar],
    w: usize,
    h: usize,
    sigma: Scalar,
    horizontal: bool,
) {
    // Python `_gaussian_kernel_1d`: `radius = int(truncate * sigma + 0.5)`
    // with `truncate=3.0`. `int(x)` on a non-negative float truncates
    // toward zero, which for `truncate·sigma + 0.5` is the standard
    // "round half up" rule. `(x + 0.5) as usize` produces the same
    // value as Python's `int(x + 0.5)` for all positive inputs we
    // care about. Rust used to use `.ceil()` here, which biases the
    // radius up by 1 when `truncate·sigma` lands at a half-integer
    // and changes the FIR kernel size — measurable parity drift.
    let radius = (from_f64(3.0) * sigma + from_f64(0.5)) as usize;
    let kernel = make_gaussian_kernel(sigma, radius);

    if horizontal {
        dst.par_chunks_exact_mut(w)
            .enumerate()
            .for_each(|(y, row_dst)| {
                let row_src = &src[y * w..(y + 1) * w];
                for x in 0..w {
                    let mut sum = ZERO;
                    for (ki, &kv) in kernel.iter().enumerate() {
                        let sx =
                            edge_index::<REFLECT>(x as isize + ki as isize - radius as isize, w);
                        sum += row_src[sx] * kv;
                    }
                    row_dst[x] = sum;
                }
            });
    } else {
        dst.par_chunks_exact_mut(w)
            .enumerate()
            .for_each(|(y, row_dst)| {
                for x in 0..w {
                    let mut sum = ZERO;
                    for (ki, &kv) in kernel.iter().enumerate() {
                        let sy =
                            edge_index::<REFLECT>(y as isize + ki as isize - radius as isize, h);
                        sum += src[sy * w + x] * kv;
                    }
                    row_dst[x] = sum;
                }
            });
    }
}

#[inline]
fn edge_index<const REFLECT: bool>(index: isize, len: usize) -> usize {
    if !REFLECT {
        return index.clamp(0, len as isize - 1) as usize;
    }
    if (index as usize) < len {
        return index as usize;
    }
    let wrapped = index.rem_euclid(2 * len as isize) as usize;
    wrapped.min(2 * len - 1 - wrapped)
}

/// Young-van Vliet recursive IIR Gaussian blur. Good for large sigma.
///
/// Reference: I.T. Young, L.J. van Vliet, "Recursive implementation of the
/// Gaussian filter", Signal Processing 44 (1995) 139-151.
fn iir_blur_1d(
    src: &[Scalar],
    dst: &mut [Scalar],
    w: usize,
    h: usize,
    sigma: Scalar,
    horizontal: bool,
) {
    // Compute IIR coefficients (Young-van Vliet, 3rd order)
    let q = if sigma >= from_f64(2.5) {
        from_f64(0.98711) * sigma - from_f64(0.96330)
    } else {
        from_f64(3.97156) - from_f64(4.14554) * (from_f64(1.0) - from_f64(0.26891) * sigma).sqrt()
    };

    let q2 = q * q;
    let q3 = q2 * q;

    let b0 =
        from_f64(1.57825) + from_f64(2.44413) * q + from_f64(1.4281) * q2 + from_f64(0.422205) * q3;
    let b1 = (from_f64(2.44413) * q + from_f64(2.85619) * q2 + from_f64(1.26661) * q3) / b0;
    let b2 = -(from_f64(1.4281) * q2 + from_f64(1.26661) * q3) / b0;
    let b3 = (from_f64(0.422205) * q3) / b0;
    let a = from_f64(1.0) - b1 - b2 - b3;

    if horizontal {
        dst.par_chunks_exact_mut(w)
            .enumerate()
            .for_each(|(y, row_dst)| {
                let row_src = &src[y * w..(y + 1) * w];
                iir_filter_row(row_src, row_dst, a, b1, b2, b3);
            });
    } else {
        // Vertical recursive filtering has a true dependency down each column,
        // so columns are independent work items. Store them column-major first
        // to keep parallel writes contiguous, then transpose back to row-major.
        let mut col_major = vec![ZERO; w * h];
        col_major
            .par_chunks_exact_mut(h)
            .enumerate()
            .for_each(|(x, col_dst)| {
                let col_src: Vec<Scalar> = (0..h).map(|y| src[y * w + x]).collect();
                iir_filter_row(&col_src, col_dst, a, b1, b2, b3);
            });
        dst.par_chunks_exact_mut(w)
            .enumerate()
            .for_each(|(y, row_dst)| {
                for x in 0..w {
                    row_dst[x] = col_major[x * h + y];
                }
            });
    }
}

/// Apply Young-van Vliet IIR filter to a 1D signal (forward + backward pass).
fn iir_filter_row(
    src: &[Scalar],
    dst: &mut [Scalar],
    a: Scalar,
    b1: Scalar,
    b2: Scalar,
    b3: Scalar,
) {
    let n = src.len();
    if n == 0 {
        return;
    }

    // Python parity (`_iir_horizontal` in `fast_gaussian_filter.py`):
    //
    //   forward:
    //     w1 = w2 = w3 = src[0]              (sample-replication border)
    //     for j in 0..n:
    //         w = a*src[j] + b1*w1 + b2*w2 + b3*w3
    //         dst[j] = w
    //         w3 = w2; w2 = w1; w1 = w
    //   backward:
    //     w1 = w2 = w3 = dst[n-1]            (sample-replication border)
    //     for j in (0..n).rev():
    //         w = a*dst[j] + b1*w1 + b2*w2 + b3*w3
    //         dst[j] = w
    //         w3 = w2; w2 = w1; w1 = w
    //
    // Crucially, the state is initialized to the FIRST sample (not zero),
    // and the iteration covers EVERY index in both directions (not
    // n-3..0). The previous Rust code zeroed the state and skipped the
    // last three positions in the backward pass — which produced a
    // small but visible edge bias vs. Python at every halation σ.
    let x0 = src[0];
    let mut w1 = x0;
    let mut w2 = x0;
    let mut w3 = x0;
    for j in 0..n {
        let w = a * src[j] + b1 * w1 + b2 * w2 + b3 * w3;
        dst[j] = w;
        w3 = w2;
        w2 = w1;
        w1 = w;
    }
    let xn = dst[n - 1];
    w1 = xn;
    w2 = xn;
    w3 = xn;
    for j in (0..n).rev() {
        let w = a * dst[j] + b1 * w1 + b2 * w2 + b3 * w3;
        dst[j] = w;
        w3 = w2;
        w2 = w1;
        w1 = w;
    }
}

/// Create normalized Gaussian kernel of given radius.
fn make_gaussian_kernel(sigma: Scalar, radius: usize) -> Vec<Scalar> {
    let size = 2 * radius + 1;
    let mut kernel = Vec::with_capacity(size);
    let s2 = from_f64(2.0) * sigma * sigma;

    for i in 0..size {
        let x = from_f64(i as f64) - from_f64(radius as f64);
        kernel.push((-x * x / s2).exp());
    }

    let sum: Scalar = kernel.iter().sum();
    for v in &mut kernel {
        *v /= sum;
    }
    kernel
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_gaussian_kernel_normalized() {
        let k = make_gaussian_kernel(from_f64(1.0), 3);
        let sum: Scalar = k.iter().sum();
        assert!((sum - from_f64(1.0)).abs() < from_f64(1e-6));
    }

    #[test]
    fn test_gaussian_blur_identity_zero_sigma() {
        let img = ImageBuf::from_data(4, 4, vec![from_f64(0.5); 4 * 4 * 3]);
        let out = gaussian_blur(&img, 0.0);
        assert_eq!(img.data, out.data);
    }

    #[test]
    fn single_channel_fir_reflects_samples_across_image_edges() {
        let sigma = from_f64(1.0);
        let mut pixels = [ZERO, from_f64(1.0), ZERO];
        let replicated = gaussian_blur_channel(&pixels, 3, 1, 1.0);
        gaussian_blur_channel_reflect_in_place(&mut pixels, 3, 1, sigma);

        let kernel = make_gaussian_kernel(sigma, 3);
        let tolerance = from_f64(1e-6);
        assert!((replicated[0] - kernel[4]).abs() < tolerance);
        assert!((pixels[0] - (kernel[1] + kernel[4])).abs() < tolerance);
        assert!((pixels[0] - pixels[2]).abs() < tolerance);
    }

    #[test]
    fn test_gaussian_blur_uniform_image() {
        // Blurring a uniform image should return the same image
        let img = ImageBuf::from_data(8, 8, vec![from_f64(0.42); 8 * 8 * 3]);
        let out = gaussian_blur(&img, 2.0);
        for (a, b) in img.data.iter().zip(out.data.iter()) {
            assert!((a - b).abs() < from_f64(1e-4), "expected {a}, got {b}");
        }
    }
}
