use anyhow::Result;
use serde::Serialize;
use spektrafilm_core::input_gamut::spectral_locus_xy;

use super::{Cube, Frame, curves::chromaticity, gamut_edge::encode_png, to_xyz_qa};

pub(super) const SIDE: u32 = 512;

#[derive(Serialize)]
pub(super) struct Envelope {
    cube_resolution: usize,
    cube_cells: usize,
    valid_cells: usize,
    inside_output_gamut_fraction: f64,
    near_locus_fraction: f64,
    #[serde(skip)]
    pub(super) locus: &'static [[f64; 2]],
    #[serde(skip)]
    pub(super) primaries: [[f64; 2]; 3],
    #[serde(skip)]
    pub(super) white: [f64; 2],
    #[serde(skip)]
    pub(super) png: Vec<u8>,
}

pub(super) fn project([x, y]: [f64; 2]) -> [f64; 2] {
    let side = f64::from(SIDE);
    [(x + 0.05) / 0.90 * side, (0.95 - y) * side]
}

pub(super) fn inside_polygon([x, y]: [f64; 2], polygon: &[[f64; 2]]) -> bool {
    let mut inside = false;
    for (i, &[xx, yy]) in polygon.iter().enumerate() {
        let [xn, yn] = polygon[(i + 1) % polygon.len()];
        // Count intersections of a ray toward positive x. This uses the
        // half-open interval convention of the report's Python path.
        if (yy >= y) != (yn >= y) && x <= xx + (xn - xx) * (y - yy) / (yn - yy) {
            inside = !inside;
        }
    }
    inside
}

pub(super) fn evaluate(cube: &Cube, frame: Frame) -> Result<Envelope> {
    // Python spectral_locus_xy uses 380 to 700 nm at 5 nm. The end of
    // the purple line belongs to the drawn polygon but the metric only
    // measures distance to the sampled loci.
    let locus = spectral_locus_xy();
    let space = frame.output.space();
    let primaries = space.primaries();
    let white = space.white_xy();
    let mut pixels = vec![10_u8; (SIDE * SIDE * 3) as usize];
    let mut valid_cells = 0;
    let mut inside = 0;
    let mut near = 0;
    for &encoded in &cube.data {
        let xyz = to_xyz_qa(encoded, frame.output);
        let [x, y] = chromaticity(xyz);
        if xyz[1] <= 1e-4 || !x.is_finite() || !y.is_finite() {
            continue;
        }
        valid_cells += 1;
        inside += usize::from(inside_polygon([x, y], &primaries));
        near += usize::from(locus.iter().any(|&[lx, ly]| (lx - x).hypot(ly - y) < 0.02));

        let projected = project([x, y]);
        let margin = -1.0..=f64::from(SIDE);
        if !projected.iter().all(|v| margin.contains(v)) {
            continue;
        }
        let [px, py] = projected.map(|v| v.round() as i32);
        for dy in -1..=1 {
            for dx in -1..=1 {
                let [xx, yy] = [px + dx, py + dy];
                if !(0..SIDE as i32).contains(&xx) || !(0..SIDE as i32).contains(&yy) {
                    continue;
                }
                let offset = (yy as usize * SIDE as usize + xx as usize) * 3;
                let alpha = if dx == 0 && dy == 0 { 0.55 } else { 0.10 };
                for (pixel, code) in pixels[offset..offset + 3].iter_mut().zip(encoded) {
                    let v =
                        (1.0 - alpha) * f64::from(*pixel) + alpha * code.clamp(0.0, 1.0) * 255.0;
                    *pixel = v.round() as u8;
                }
            }
        }
    }
    Ok(Envelope {
        cube_resolution: cube.size,
        cube_cells: cube.data.len(),
        valid_cells,
        inside_output_gamut_fraction: inside as f64 / valid_cells.max(1) as f64,
        near_locus_fraction: near as f64 / valid_cells.max(1) as f64,
        locus,
        primaries,
        white,
        png: encode_png(&pixels, SIDE, SIDE)?,
    })
}
