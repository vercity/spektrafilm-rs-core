use anyhow::Result;
use exr::{
    meta::{
        attribute::{AttributeValue, Chromaticities, Text},
        header::{ImageAttributes, LayerAttributes},
    },
    prelude::Vec2,
};
use moxcms::{ColorProfile, ProfileText, ToneReprCurve, Xyzd};
use spektrafilm_math::{
    colorspace::{Cctf, RgbSpace, mat3_mul3_f64},
    precision::{from_f32, to_f32},
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RasterColor {
    pub color_space: &'static str,
    pub cctf_encoding: bool,
}

impl RasterColor {
    pub fn new(name: &str, encoded: bool) -> Option<Self> {
        let space = RgbSpace::from_name(name)?;
        Some(Self {
            color_space: space.name(),
            cctf_encoding: encoded && space.transfer() != Cctf::Linear,
        })
    }
}

fn profile(space: RgbSpace) -> ColorProfile {
    match space {
        RgbSpace::Srgb => ColorProfile::new_srgb(),
        RgbSpace::ProPhoto => ColorProfile::new_pro_photo_rgb(),
        RgbSpace::Rec2020 => ColorProfile::new_bt2020(),
        RgbSpace::Aces2065 => ColorProfile::new_aces_aces_2065_1_linear(),
        RgbSpace::VGamut => {
            // Both spaces use D65, so reuse the same Bradford adaptation to
            // the ICC D50 connection space when building the colorants.
            let mut profile = ColorProfile::new_srgb();
            let to_d50 = profile
                .chromatic_adaptation
                .as_ref()
                .expect("the built-in sRGB profile includes D65-to-D50 adaptation");
            let matrix = mat3_mul3_f64(&to_d50.v, &space.to_xyz());
            let colorant = |i: usize| Xyzd::new(matrix[0][i], matrix[1][i], matrix[2][i]);
            profile.red_colorant = colorant(0);
            profile.green_colorant = colorant(1);
            profile.blue_colorant = colorant(2);
            profile
        }
    }
}

pub(super) fn make_icc(color: RasterColor) -> Result<Option<Vec<u8>>> {
    let space = RgbSpace::from_name(color.color_space).expect("known RGB space");
    // The normalized ICC curves here cannot carry V-Log's scene-linear
    // headroom. EXR can carry the primaries and identify the transfer separately.
    if space == RgbSpace::VGamut && color.cctf_encoding {
        return Ok(None);
    }
    let mut profile = profile(space);
    let curve = if color.cctf_encoding {
        match color.color_space {
            "ProPhoto RGB" => Some(ToneReprCurve::Parametric(vec![
                1.8,
                1.0,
                0.0,
                1.0 / 16.0,
                1.0 / 32.0,
            ])),
            "ITU-R BT.2020" => Some(ToneReprCurve::Parametric(vec![
                1.0 / 0.45,
                1.0 / 1.099,
                0.099 / 1.099,
                1.0 / 4.5,
                0.081247944,
            ])),
            _ => None,
        }
    } else {
        profile.cicp = None;
        profile.description = Some(ProfileText::PlainString(format!(
            "{} (linear)",
            color.color_space
        )));
        Some(ToneReprCurve::Lut(Vec::new()))
    };
    if let Some(curve) = curve {
        profile.red_trc = Some(curve.clone());
        profile.green_trc = Some(curve.clone());
        profile.blue_trc = Some(curve);
    }
    Ok(Some(profile.encode()?))
}

pub(super) fn identify_icc(bytes: &[u8]) -> Option<RasterColor> {
    let embedded = ColorProfile::new_from_slice(bytes).ok()?;
    let matrix = embedded.rgb_to_xyz_matrix().v;
    let curves = [&embedded.red_trc, &embedded.green_trc, &embedded.blue_trc]
        .map(|curve| curve.as_ref()?.make_linear_evaluator().ok());
    let [Some(red), Some(green), Some(blue)] = curves else {
        return None;
    };
    for space in RgbSpace::ALL {
        let known = profile(space).rgb_to_xyz_matrix().v;
        if matrix
            .iter()
            .flatten()
            .zip(known.iter().flatten())
            .any(|(&a, &b)| (a - b).abs() > 0.002)
        {
            continue;
        }
        let cctf = space.transfer();
        for transfer in [Cctf::Linear, cctf] {
            // Matrix alone cannot distinguish linear and encoded ICCs. Sample the
            // three curves, allowing for common ICC table and fixed-point rounding.
            let matches = [0.01_f32, 0.09, 0.25, 0.55, 0.9].into_iter().all(|v| {
                let decoded = to_f32(transfer.decode(from_f32(v)));
                [&red, &green, &blue]
                    .into_iter()
                    .all(|curve| (curve.evaluate_value(v) - decoded).abs() < 0.002)
            });
            if matches {
                return RasterColor::new(space.name(), transfer != Cctf::Linear);
            }
        }
    }
    None
}

fn chromaticities(space: RgbSpace) -> Chromaticities {
    let xy = |[x, y]: [f64; 2]| Vec2(x as f32, y as f32);
    let [red, green, blue] = space.primaries().map(xy);
    Chromaticities {
        red,
        green,
        blue,
        white: xy(space.white_xy()),
    }
}

const EXR_TRANSFER: &str = "spektrafilmTransfer";

pub(super) fn set_exr_color(attributes: &mut ImageAttributes, color: Option<RasterColor>) {
    if let Some(color) = color {
        let space = RgbSpace::from_name(color.color_space).expect("known RGB space");
        attributes.chromaticities = Some(chromaticities(space));
        let transfer = if color.cctf_encoding {
            "encoded"
        } else {
            "linear"
        };
        attributes.other.insert(
            Text::from(EXR_TRANSFER),
            AttributeValue::Text(Text::from(transfer)),
        );
    }
}

pub(super) fn identify_exr(
    attributes: &ImageAttributes,
    layer: &LayerAttributes,
) -> Option<RasterColor> {
    let Some(actual) = attributes.chromaticities else {
        // The OpenEXR default is linear Rec.709, whose RGB primaries are sRGB.
        return RasterColor::new("sRGB", false);
    };
    let near =
        |a: Vec2<f32>, b: Vec2<f32>| (a.0 - b.0).abs() < 0.0005 && (a.1 - b.1).abs() < 0.0005;
    let encoded = matches!(
        attributes.other.get(&Text::from(EXR_TRANSFER))
            .or_else(|| layer.other.get(&Text::from(EXR_TRANSFER))),
        Some(AttributeValue::Text(value)) if value.as_slice() == b"encoded"
    );
    for space in RgbSpace::ALL {
        let expected = chromaticities(space);
        if near(actual.red, expected.red)
            && near(actual.green, expected.green)
            && near(actual.blue, expected.blue)
            && near(actual.white, expected.white)
        {
            return RasterColor::new(space.name(), encoded);
        }
    }
    None
}
