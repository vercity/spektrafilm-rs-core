use spektrafilm_math::{
    colorspace::{Cctf, RgbSpace, chromatic_adaptation_matrix_f64, mat3_mul3_f64},
    image::ImageBuf,
    precision::{Scalar, from_f64, to_f64},
};

use super::RasterColor;

/// RGB conversion for display and saving. The destination format owns clipping.
pub struct ColorTransform {
    decode: Cctf,
    matrix: Option<[[f64; 3]; 3]>,
    encode: Cctf,
}

impl ColorTransform {
    pub fn new(source: RasterColor, destination: RasterColor) -> Self {
        let source_space = RgbSpace::from_name(source.color_space).expect("known RGB space");
        let destination_space =
            RgbSpace::from_name(destination.color_space).expect("known RGB space");
        if source == destination {
            return Self {
                decode: Cctf::Linear,
                matrix: None,
                encode: Cctf::Linear,
            };
        }

        let matrix = (source_space != destination_space).then(|| {
            let source_xyz = source_space.to_xyz();
            let source_white = source_space.white_xyz();
            let destination_white = destination_space.white_xyz();
            let adapted_xyz = if source_white == destination_white {
                source_xyz
            } else {
                let adapt = chromatic_adaptation_matrix_f64(source_white, destination_white);
                mat3_mul3_f64(&adapt, &source_xyz)
            };
            mat3_mul3_f64(&destination_space.from_xyz(), &adapted_xyz)
        });

        Self {
            decode: if source.cctf_encoding {
                source_space.transfer()
            } else {
                Cctf::Linear
            },
            matrix,
            encode: if destination.cctf_encoding {
                destination_space.transfer()
            } else {
                Cctf::Linear
            },
        }
    }

    pub fn apply(&self, rgb: [Scalar; 3]) -> [Scalar; 3] {
        let linear = rgb.map(|value| self.decode.decode(value));
        let converted = match &self.matrix {
            Some(matrix) => matrix.map(|row| {
                from_f64(
                    row.into_iter()
                        .zip(linear)
                        .map(|(a, b)| a * to_f64(b))
                        .sum(),
                )
            }),
            None => linear,
        };
        converted.map(|value| self.encode.encode(value))
    }

    pub fn apply_image(&self, image: &mut ImageBuf) {
        use rayon::prelude::*;
        image.par_pixels_mut().for_each(|pixel| {
            pixel.copy_from_slice(&self.apply([pixel[0], pixel[1], pixel[2]]));
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prophoto_to_srgb_matches_colour_science_without_clipping() {
        let source = RasterColor::new("ProPhoto RGB", false).unwrap();
        let destination = RasterColor::new("sRGB", true).unwrap();
        let got = ColorTransform::new(source, destination).apply([0.71, 0.12, -0.03]);
        let expected = [
            1.1465450510674418,
            -0.17519956875231785,
            -0.7497337480799856,
        ];
        for (sample, expected) in got.into_iter().zip(expected) {
            assert!(
                (to_f64(sample) - expected).abs() < 5e-6,
                "{sample} vs {expected}"
            );
        }
    }
}
