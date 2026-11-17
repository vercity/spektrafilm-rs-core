//! Floating-point image resampling on the same pixel-centred grid as skimage.

use rayon::prelude::*;
use std::borrow::Cow;

use crate::image::ImageBuf;
use crate::precision::{from_f64, to_f64};

#[derive(Clone, Copy)]
pub enum Interpolation {
    Nearest,
    Bilinear,
    CubicSpline,
}

/// Round each output axis independently, retaining at least one sample.
pub fn scaled_dimensions(width: u32, height: u32, scale: f64) -> [u32; 2] {
    [width, height].map(|length| (length as f64 * scale).round_ties_even().max(1.0) as u32)
}

/// Resample a floating RGB image. Downsampling prefilters each shrinking axis
/// with a Gaussian, including for nearest neighbour, as skimage does for floats.
pub fn resize(image: &ImageBuf, size: [u32; 2], interpolation: Interpolation) -> ImageBuf {
    if size == [image.width, image.height] || image.data.is_empty() {
        return image.clone();
    }
    let [width, height] = size.map(|length| length.max(1));
    match interpolation {
        Interpolation::Nearest => gaussian_sample(image, width, height, width, height),
        Interpolation::Bilinear | Interpolation::CubicSpline => {
            let filtered = if width < image.width || height < image.height {
                Cow::Owned(gaussian_sample(
                    image,
                    width,
                    height,
                    image.width,
                    image.height,
                ))
            } else {
                Cow::Borrowed(image)
            };
            if matches!(interpolation, Interpolation::Bilinear) {
                return linear_resize(&filtered, width, height);
            }
            // skimage clips interpolation overshoot to the input range across
            // all three channels. The range may include negatives and >1.
            let (min, max) = image.data.iter().copied().fold(
                (f64::INFINITY, f64::NEG_INFINITY),
                |(min, max), value| {
                    let value = to_f64(value);
                    (min.min(value), max.max(value))
                },
            );
            let mut resized = cubic_resize(&filtered, width, height);
            if min <= max {
                resized
                    .data
                    .par_iter_mut()
                    .for_each(|value| *value = from_f64(to_f64(*value).clamp(min, max)));
            }
            resized
        }
    }
}

/// Mirror around each edge sample, as numpy.pad's reflect mode does.
fn mirror(index: isize, len: usize) -> usize {
    if (index as usize) < len {
        return index as usize;
    }
    if len <= 1 {
        return 0;
    }
    let period = 2 * (len - 1);
    let wrapped = index.rem_euclid(period as isize) as usize;
    wrapped.min(period - wrapped)
}

fn source_center(index: usize, input: usize, output: usize) -> f64 {
    (index as f64 + 0.5) * (input as f64 / output as f64) - 0.5
}

fn linear_resize(image: &ImageBuf, width: u32, height: u32) -> ImageBuf {
    let axis = |index, source, destination| {
        let coordinate = source_center(index, source, destination);
        let lower = coordinate.floor() as isize;
        (
            [
                mirror(lower, source) as u32,
                mirror(lower + 1, source) as u32,
            ],
            coordinate - lower as f64,
        )
    };
    let horizontal: Vec<_> = (0..width as usize)
        .map(|x| axis(x, image.width as usize, width as usize))
        .collect();
    let mut output = ImageBuf::new(width, height);
    output.par_rows_mut().enumerate().for_each(|(y, row)| {
        let ([top, bottom], ty) = axis(y, image.height as usize, height as usize);
        for (pixel, &([left, right], tx)) in row.chunks_exact_mut(3).zip(&horizontal) {
            let upper_left = image.get(left, top);
            let upper_right = image.get(right, top);
            let lower_left = image.get(left, bottom);
            let lower_right = image.get(right, bottom);
            for channel in 0..3 {
                let above =
                    to_f64(upper_left[channel]) * (1.0 - tx) + to_f64(upper_right[channel]) * tx;
                let below =
                    to_f64(lower_left[channel]) * (1.0 - tx) + to_f64(lower_right[channel]) * tx;
                pixel[channel] = from_f64(above * (1.0 - ty) + below * ty);
            }
        }
    });
    output
}

fn nearest_index(index: usize, input: usize, output: usize) -> usize {
    (source_center(index, input, output) + 0.5).floor() as usize
}

fn antialias_kernel(input: usize, output: usize) -> Vec<f64> {
    let sigma = (input as f64 / output as f64 - 1.0) * 0.5;
    let radius = (4.0 * sigma + 0.5).max(0.0) as isize;
    if radius == 0 {
        return vec![1.0];
    }
    let mut kernel: Vec<_> = (-radius..=radius)
        .map(|x| (-(x as f64).powi(2) / (2.0 * sigma * sigma)).exp())
        .collect();
    let sum: f64 = kernel.iter().sum();
    kernel.iter_mut().for_each(|weight| *weight /= sum);
    kernel
}

/// Apply the antialias filters for the requested resize. Sampling can happen
/// inside each axis pass for nearest neighbour, reducing metering work to
/// the source rows and columns that contribute to the small grid.
fn gaussian_sample(
    image: &ImageBuf,
    target_width: u32,
    target_height: u32,
    sampled_width: u32,
    sampled_height: u32,
) -> ImageBuf {
    let width = image.width as usize;
    let height = image.height as usize;
    let stride = width * 3;
    let y_kernel = antialias_kernel(height, target_height as usize);
    let x_kernel = antialias_kernel(width, target_width as usize);

    let mut vertical = ImageBuf::new(image.width, sampled_height);
    vertical
        .data
        .par_chunks_mut(stride)
        .enumerate()
        .for_each_init(
            || vec![0.0; stride],
            |sums, (y, row)| {
                let center = nearest_index(y, height, sampled_height as usize);
                if y_kernel.len() == 1 {
                    row.copy_from_slice(&image.data[center * stride..][..stride]);
                    return;
                }
                let start = center as isize - (y_kernel.len() / 2) as isize;
                sums.fill(0.0);
                for (tap, &weight) in y_kernel.iter().enumerate() {
                    let iy = mirror(start + tap as isize, height);
                    for (sum, &sample) in sums.iter_mut().zip(&image.data[iy * stride..][..stride])
                    {
                        *sum += weight * to_f64(sample);
                    }
                }
                for (sample, &sum) in row.iter_mut().zip(sums.iter()) {
                    *sample = from_f64(sum);
                }
            },
        );

    if x_kernel.len() == 1 && image.width == sampled_width {
        return vertical;
    }
    let centers: Vec<_> = (0..sampled_width as usize)
        .map(|x| nearest_index(x, width, sampled_width as usize))
        .collect();
    let mut out = ImageBuf::new(sampled_width, sampled_height);
    out.par_rows_mut()
        .zip(vertical.par_rows())
        .for_each(|(out, row)| {
            for (pixel, &center) in out.chunks_exact_mut(3).zip(&centers) {
                let start = center as isize - (x_kernel.len() / 2) as isize;
                let mut rgb = [0.0; 3];
                for (tap, &weight) in x_kernel.iter().enumerate() {
                    let ix = mirror(start + tap as isize, width);
                    for (sum, &sample) in rgb.iter_mut().zip(&row[ix * 3..][..3]) {
                        *sum += weight * to_f64(sample);
                    }
                }
                pixel.copy_from_slice(&rgb.map(from_f64));
            }
        });
    out
}

struct CubicAxis {
    inverse_diagonal: Vec<f64>,
    samples: Vec<([usize; 4], [f64; 4])>,
}

impl CubicAxis {
    fn new(input: usize, output: usize) -> Self {
        // At integer positions a cubic B-spline has weights 1/6, 4/6, 1/6.
        // Solve the resulting tridiagonal system for its coefficients. Mirror
        // boundaries double the sole neighbouring term at each endpoint.
        let mut inverse_diagonal = vec![0.25_f64; input];
        for i in 1..input {
            let lower = if i == input - 1 { 2.0 } else { 1.0 };
            let prev_upper = if i == 1 { 2.0 } else { 1.0 };
            inverse_diagonal[i] = (4.0 - lower * prev_upper * inverse_diagonal[i - 1]).recip();
        }

        let samples = (0..output)
            .map(|i| {
                let center = source_center(i, input, output);
                let knot = center.floor();
                let x = center - knot;
                let z = 1.0 - x;
                let weights = [
                    z.powi(3) / 6.0,
                    (4.0 + 3.0 * x * x * (x - 2.0)) / 6.0,
                    (4.0 + 3.0 * z * z * (z - 2.0)) / 6.0,
                    x.powi(3) / 6.0,
                ];
                let indices = [-1, 0, 1, 2].map(|offset| mirror(knot as isize + offset, input));
                (indices, weights)
            })
            .collect();

        Self {
            inverse_diagonal,
            samples,
        }
    }

    fn coefficients(&self, line: &mut [[f64; 3]]) {
        let len = line.len();
        if len <= 1 {
            return;
        }
        line[0] = line[0].map(|sample| sample * 1.5);
        for i in 1..len {
            let lower = if i == len - 1 { 2.0 } else { 1.0 };
            for c in 0..3 {
                line[i][c] = (6.0 * line[i][c] - lower * line[i - 1][c]) * self.inverse_diagonal[i];
            }
        }
        for i in (0..len - 1).rev() {
            let upper = if i == 0 { 2.0 } else { 1.0 };
            for c in 0..3 {
                line[i][c] -= upper * self.inverse_diagonal[i] * line[i + 1][c];
            }
        }
    }

    fn interpolate(&self, line: &[[f64; 3]], index: usize) -> [f64; 3] {
        let (indices, weights) = self.samples[index];
        let mut rgb = [0.0; 3];
        for (i, weight) in indices.into_iter().zip(weights) {
            for (sum, sample) in rgb.iter_mut().zip(line[i]) {
                *sum += weight * sample;
            }
        }
        rgb
    }
}

fn cubic_resize(image: &ImageBuf, width: u32, height: u32) -> ImageBuf {
    let input_width = image.width as usize;
    let input_height = image.height as usize;
    let output_height = height as usize;
    let y_axis = CubicAxis::new(input_height, output_height);
    let x_axis = CubicAxis::new(input_width, width as usize);

    // Interpolate each column, then each row. Store the intermediate in
    // column-major order so parallel column writes stay contiguous.
    let mut vertical = vec![[0.0; 3]; input_width * output_height];
    vertical
        .par_chunks_mut(output_height)
        .enumerate()
        .for_each_init(
            || vec![[0.0; 3]; input_height],
            |line, (x, output)| {
                for (y, sample) in line.iter_mut().enumerate() {
                    *sample = image.get(x as u32, y as u32).map(to_f64);
                }
                y_axis.coefficients(line);
                for (y, pixel) in output.iter_mut().enumerate() {
                    *pixel = y_axis.interpolate(line, y);
                }
            },
        );

    let mut out = ImageBuf::new(width, height);
    out.par_rows_mut().enumerate().for_each_init(
        || vec![[0.0; 3]; input_width],
        |line, (y, row)| {
            for (x, sample) in line.iter_mut().enumerate() {
                *sample = vertical[x * output_height + y];
            }
            x_axis.coefficients(line);
            for (x, pixel) in row.chunks_exact_mut(3).enumerate() {
                pixel.copy_from_slice(&x_axis.interpolate(line, x).map(from_f64));
            }
        },
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cubic_matches_skimage_at_an_odd_mirror_boundary() {
        let data = [
            -0.25, 3.0, 0.0, 0.0, 0.5, 1.0, 0.0, 1.5, 0.0, -0.2, 0.1, -0.1, 0.4, 0.0, 2.0,
        ];
        let image = ImageBuf::from_data(
            5,
            3,
            data.iter()
                .flat_map(|&v| [v, 0.5 * v, 0.1 - 0.2 * v].map(from_f64))
                .collect(),
        );

        // skimage float64 rescale(order=3, channel_axis=2).
        let down = resize(
            &image,
            scaled_dimensions(5, 3, 0.5),
            Interpolation::CubicSpline,
        );
        assert_eq!([down.width, down.height], [2, 2]);
        let expected = [
            1.43487953819735,
            0.14477598134993216,
            0.05860487004007655,
            0.6255631238968119,
        ];
        for (pixel, sample) in down.pixels().zip(expected) {
            for (&actual, expected) in pixel.iter().zip([sample, 0.5 * sample, 0.1 - 0.2 * sample])
            {
                assert!((to_f64(actual) - expected).abs() < 1e-6);
            }
        }

        let up = resize(
            &image,
            scaled_dimensions(5, 3, 1.5),
            Interpolation::CubicSpline,
        );
        assert_eq!([up.width, up.height], [8, 4]);
        assert!((to_f64(up.get(0, 0)[0]) - 0.06211584710649153).abs() < 1e-6);
        assert!((to_f64(up.get(7, 3)[0]) - 1.777006227735962).abs() < 1e-6);
        assert!(
            up.data
                .iter()
                .all(|&sample| (-0.5..=3.0).contains(&to_f64(sample)))
        );
    }

    #[test]
    fn cubic_keeps_signed_and_superwhite_values_with_a_single_source_column() {
        let data = [-0.25, 0.8, 2.0]
            .iter()
            .flat_map(|&v| [from_f64(v); 3])
            .collect();
        let image = ImageBuf::from_data(1, 3, data);
        let resized = resize(
            &image,
            scaled_dimensions(1, 3, 1.5),
            Interpolation::CubicSpline,
        );
        assert_eq!([resized.width, resized.height], [2, 4]);
        let expected = [
            -0.2279541015625,
            0.2205810546875,
            1.4268798828125,
            1.9715087890625,
        ];
        for (row, expected) in resized.rows().zip(expected) {
            assert!(
                row.iter()
                    .all(|&sample| (to_f64(sample) - expected).abs() < 1e-6)
            );
        }
        assert_eq!(scaled_dimensions(1, 3, 0.1), [1, 1]);
    }
}
