use anyhow::Result;
use exr::{
    meta::{
        attribute::{AttributeValue, Chromaticities, Text},
        header::{ImageAttributes, LayerAttributes},
    },
    prelude::Vec2,
};
use moxcms::{ColorProfile, ProfileText, ToneReprCurve};
use spektrafilm_math::{
    colorspace::Cctf,
    precision::{from_f32, to_f32},
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RasterColor {
    pub color_space: &'static str,
    pub cctf_encoding: bool,
}

impl RasterColor {
    pub fn new(name: &str, encoded: bool) -> Option<Self> {
        let (color_space, cctf_encoding) = match name {
            "sRGB" => ("sRGB", encoded),
            "ProPhoto RGB" => ("ProPhoto RGB", encoded),
            "Rec. 2020" | "Rec2020" | "ITU-R BT.2020" => ("ITU-R BT.2020", encoded),
            "ACES2065-1" => ("ACES2065-1", false),
            _ => return None,
        };
        Some(Self {
            color_space,
            cctf_encoding,
        })
    }
}

const SPACES: [&str; 4] = ["sRGB", "ProPhoto RGB", "ITU-R BT.2020", "ACES2065-1"];

fn profile(name: &str) -> ColorProfile {
    match name {
        "sRGB" => ColorProfile::new_srgb(),
        "ProPhoto RGB" => ColorProfile::new_pro_photo_rgb(),
        "ITU-R BT.2020" => ColorProfile::new_bt2020(),
        "ACES2065-1" => ColorProfile::new_aces_aces_2065_1_linear(),
        _ => unreachable!("known RGB space"),
    }
}

pub(super) fn make_icc(color: RasterColor) -> Result<Vec<u8>> {
    let mut profile = profile(color.color_space);
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
    Ok(profile.encode()?)
}

pub(super) fn identify_icc(bytes: &[u8]) -> Option<RasterColor> {
    let embedded = ColorProfile::new_from_slice(bytes).ok()?;
    let matrix = embedded.rgb_to_xyz_matrix().v;
    let curves = [&embedded.red_trc, &embedded.green_trc, &embedded.blue_trc]
        .map(|curve| curve.as_ref()?.make_linear_evaluator().ok());
    let [Some(red), Some(green), Some(blue)] = curves else {
        return None;
    };
    for name in SPACES {
        let known = profile(name).rgb_to_xyz_matrix().v;
        if matrix
            .iter()
            .flatten()
            .zip(known.iter().flatten())
            .any(|(&a, &b)| (a - b).abs() > 0.002)
        {
            continue;
        }
        let cctf = Cctf::for_rgb_space(name)?;
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
                return RasterColor::new(name, transfer != Cctf::Linear);
            }
        }
    }
    None
}

fn chromaticities(name: &str) -> Chromaticities {
    let (red, green, blue, white) = match name {
        "sRGB" => ((0.64, 0.33), (0.30, 0.60), (0.15, 0.06), (0.3127, 0.3290)),
        "ProPhoto RGB" => (
            (0.7347, 0.2653),
            (0.1596, 0.8404),
            (0.0366, 0.0001),
            (0.3457, 0.3585),
        ),
        "ITU-R BT.2020" => (
            (0.708, 0.292),
            (0.170, 0.797),
            (0.131, 0.046),
            (0.3127, 0.3290),
        ),
        "ACES2065-1" => (
            (0.7347, 0.2653),
            (0.0, 1.0),
            (0.0001, -0.0770),
            (0.32168, 0.33767),
        ),
        _ => unreachable!("known RGB space"),
    };
    Chromaticities {
        red: red.into(),
        green: green.into(),
        blue: blue.into(),
        white: white.into(),
    }
}

const EXR_TRANSFER: &str = "spektrafilmTransfer";

pub(super) fn set_exr_color(attributes: &mut ImageAttributes, color: Option<RasterColor>) {
    if let Some(color) = color {
        attributes.chromaticities = Some(chromaticities(color.color_space));
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
    for name in SPACES {
        let expected = chromaticities(name);
        if near(actual.red, expected.red)
            && near(actual.green, expected.green)
            && near(actual.blue, expected.blue)
            && near(actual.white, expected.white)
        {
            return RasterColor::new(name, encoded);
        }
    }
    None
}
