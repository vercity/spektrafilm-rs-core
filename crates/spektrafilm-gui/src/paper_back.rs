use std::sync::LazyLock;

use crate::PreviewTextureData;

const LONG_EDGE: f64 = 1024.0;
const STAMP: &[u8] = include_bytes!("assets/paper_watermark_stamp.png");

// Derived from Python 0.3.4's spektrafilm_gui/assets/virtual_paper_watermark.png
// with prepare_tile_stamp(0.22 * 1024 / max(original dimensions), 16°).
static LOGO: LazyLock<image::ImageBuffer<image::Luma<u16>, Vec<u16>>> = LazyLock::new(|| {
    image::load_from_memory_with_format(STAMP, image::ImageFormat::Png)
        .expect("bundled virtual-paper watermark")
        .into_luma16()
});

pub(super) fn dimensions(width: u32, height: u32) -> [usize; 2] {
    let scale = LONG_EDGE / f64::from(width.max(height).max(1));
    [width, height].map(|axis| (f64::from(axis.max(1)) * scale).round_ties_even().max(1.0) as usize)
}

pub(super) fn render(size: [usize; 2]) -> PreviewTextureData {
    let [width, height] = size;
    let logo = &*LOGO;
    let (stamp_w, stamp_h) = (logo.width() as usize, logo.height() as usize);
    let basis = rhombic_basis();
    let radius = (stamp_w as f64).hypot(stamp_h as f64) * 0.5;
    let extent = (width as f64).hypot(height as f64);
    let limit = ((extent + 2.0 * radius) / (LONG_EDGE * 0.25)).ceil() as i32 + 2;
    let mut centers = Vec::new();
    for i in -limit..=limit {
        for j in -limit..=limit {
            let center = [width, height].map(|axis| axis as f64 * 0.5);
            let x =
                center[0] + i as f64 * f64::from(basis[0][0]) + j as f64 * f64::from(basis[1][0]);
            let y =
                center[1] + i as f64 * f64::from(basis[0][1]) + j as f64 * f64::from(basis[1][1]);
            if (-radius..=width as f64 + radius).contains(&x)
                && (-radius..=height as f64 + radius).contains(&y)
            {
                centers.push((x, y));
            }
        }
    }
    centers.sort_by(|(x, y), (a, b)| x.total_cmp(a).then_with(|| y.total_cmp(b)));

    let mut transmission = vec![1.0f32; width * height];
    for (x, y) in centers {
        let left = (x - 0.5 * stamp_w as f64).round_ties_even() as isize;
        let top = (y - 0.5 * stamp_h as f64).round_ties_even() as isize;
        let start_x = left.max(0);
        let start_y = top.max(0);
        let end_x = (left + stamp_w as isize).min(width as isize);
        let end_y = (top + stamp_h as isize).min(height as isize);
        for dy in start_y..end_y {
            for dx in start_x..end_x {
                let alpha = logo.get_pixel((dx - left) as u32, (dy - top) as u32).0[0];
                transmission[dy as usize * width + dx as usize] *= 1.0 - f32::from(alpha) / 65535.0;
            }
        }
    }

    let mut noise = PaperNoise::default();
    let first_draws: Vec<_> = (0..transmission.len()).map(|_| noise.sample()).collect();
    let mut rgba = Vec::with_capacity(width * height * 4);
    for (transmittance, first) in transmission.into_iter().zip(first_draws) {
        let glare_map = 0.88 * first + 0.12 * noise.sample().powi(10);
        let paper = 0.7 * transmittance + 0.3;
        let variation = 1.0 + (2.0 * glare_map - 1.0) * paper * (0.07 * 0.25);
        let lift = glare_map.powi(6) * paper * (0.42 * 0.25);
        let color = (transmittance * (242.0 / 255.0 - 215.0 / 255.0) + 215.0 / 255.0) * variation;
        let sample =
            ((color + (1.0 - color) * lift).clamp(0.0, 1.0) * 255.0).round_ties_even() as u8;
        rgba.extend_from_slice(&[sample, sample, sample, 255]);
    }

    PreviewTextureData { size, rgba }
}

fn rhombic_basis() -> [[f32; 2]; 2] {
    let (sin, cos) = 36.0_f64.to_radians().sin_cos();
    let (s, c) = 16.0_f64.to_radians().sin_cos();
    let (x, y) = (
        (LONG_EDGE * 0.25 * cos) as f32,
        (LONG_EDGE * 0.25 * sin) as f32,
    );
    let (s, c) = (s as f32, c as f32);
    [y, -y].map(|y| [x * c + y * s, -x * s + y * c])
}

// NumPy PCG64 seed 0. float32 consumes the low uint32 then the high uint32.
struct PaperNoise {
    state: u128,
    high: Option<u32>,
}

impl Default for PaperNoise {
    fn default() -> Self {
        Self {
            state: 35399562948360463058890781895381311971,
            high: None,
        }
    }
}

impl PaperNoise {
    fn sample(&mut self) -> f32 {
        let word = if let Some(high) = self.high.take() {
            high
        } else {
            self.state = self
                .state
                .wrapping_mul(47026247687942121848144207491837523525)
                .wrapping_add(87136372517582989555478159403783844777);
            let raw =
                (((self.state >> 64) ^ self.state) as u64).rotate_right((self.state >> 122) as u32);
            self.high = Some((raw >> 32) as u32);
            raw as u32
        };
        (word >> 8) as f32 / 16_777_216.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paper_texture_matches_the_python_watermark_layout_and_glare() {
        let size = dimensions(3, 2);
        assert_eq!(size, [1024, 683]);
        assert_eq!(dimensions(1, 28_000), [1, 1024]);
        let pixels = render(size);
        for (y, x, expected) in [
            (0, 0, 244),
            (3, 5, 239),
            (90, 317, 242),
            (100, 100, 239),
            (341, 512, 239),
            (341, 514, 243),
            (515, 513, 241),
            (682, 1023, 241),
        ] {
            let pixel = &pixels.rgba[4 * (y * size[0] + x)..][..4];
            assert_eq!(pixel, &[expected, expected, expected, 255]);
        }
    }
}
