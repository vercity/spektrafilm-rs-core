use std::sync::Arc;

use spektrafilm_io::{
    raster::{ColorTransform, RasterColor},
    raw,
};
use spektrafilm_math::{
    image::ImageBuf,
    resize::{Interpolation, resize},
};

pub(super) struct PreviewSource {
    original: Arc<ImageBuf>,
    cap: u32,
    raw_destination: Option<RasterColor>,
    image: Arc<ImageBuf>,
}

impl PreviewSource {
    pub fn prepare(
        cache: &mut Option<Self>,
        original: Arc<ImageBuf>,
        cap: u32,
        raw_destination: Option<RasterColor>,
    ) -> Arc<ImageBuf> {
        if let Some(hit) = cache.as_ref().filter(|hit| {
            Arc::ptr_eq(&original, &hit.original)
                && hit.cap == cap
                && hit.raw_destination == raw_destination
        }) {
            return Arc::clone(&hit.image);
        }

        let image = interpret_input(Arc::clone(&original), raw_destination);
        let longest = image.width.max(image.height);
        let cap = cap.max(1);
        let image = if longest > cap {
            // The Python preview controller truncates both scaled dimensions.
            let scale = cap as f64 / longest as f64;
            let size =
                [image.width, image.height].map(|axis| ((axis as f64 * scale) as u32).max(1));
            Arc::new(resize(&image, size, Interpolation::Bilinear))
        } else {
            image
        };
        *cache = Some(Self {
            original,
            cap,
            raw_destination,
            image: Arc::clone(&image),
        });
        image
    }

    pub fn belongs_to(&self, original: &Arc<ImageBuf>) -> bool {
        Arc::ptr_eq(&self.original, original)
    }
}

pub(super) fn interpret_input(
    image: Arc<ImageBuf>,
    raw_destination: Option<RasterColor>,
) -> Arc<ImageBuf> {
    let raw_color = RasterColor::new(raw::INPUT_COLOR_SPACE, false).unwrap();
    if let Some(destination) = raw_destination.filter(|&color| color != raw_color) {
        let mut converted = (*image).clone();
        ColorTransform::new(raw_color, destination).apply_image(&mut converted);
        Arc::new(converted)
    } else {
        image
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use spektrafilm_math::precision::{from_f64, to_f64};

    #[test]
    fn matches_python_antialiased_preview_with_truncated_height() {
        let pixels = (0..9)
            .flat_map(|y| {
                (0..13).flat_map(move |x| {
                    [
                        (x - 3 * y) as f64 * 0.0625,
                        ((x + y) % 3) as f64 * 0.3,
                        (y % 2) as f64 * 1.25 + x as f64 * 0.03125,
                    ]
                    .map(from_f64)
                })
            })
            .collect();
        let original = Arc::new(ImageBuf::from_data(13, 9, pixels));
        let mut cache = None;
        let actual = PreviewSource::prepare(&mut cache, Arc::clone(&original), 7, None);
        assert_eq!((actual.width, actual.height), (7, 4));
        for ([x, y], expected) in [
            (
                [0, 0],
                [-0.11315485449136703, 0.3168255328479695, 0.6858936330127783],
            ),
            (
                [2, 1],
                [-0.28013422540287625, 0.3020200844622247, 0.8907378226288285],
            ),
            (
                [6, 3],
                [-0.6368451455086329, 0.28317446715203043, 1.0299553915969168],
            ),
        ] {
            for (got, expected) in actual.get(x, y).into_iter().map(to_f64).zip(expected) {
                assert!((got - expected).abs() < 2e-7);
            }
        }
        let reused = PreviewSource::prepare(&mut cache, Arc::clone(&original), 7, None);
        assert!(Arc::ptr_eq(&actual, &reused));
        let changed = PreviewSource::prepare(&mut cache, Arc::clone(&original), 6, None);
        assert_eq!((changed.width, changed.height), (6, 4));
        assert!(!Arc::ptr_eq(&actual, &changed));
        let unscaled = PreviewSource::prepare(&mut cache, Arc::clone(&original), 13, None);
        assert!(Arc::ptr_eq(&original, &unscaled));
    }
}
