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
        let space = RgbSpace::from_name(name).filter(|space| RgbSpace::ALL.contains(space))?;
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
        RgbSpace::Rec709 => ColorProfile::new_srgb(),
        RgbSpace::DisplayP3 => ColorProfile::new_display_p3(),
        RgbSpace::DciP3 => ColorProfile::new_dci_p3(),
        RgbSpace::AdobeRgb => ColorProfile::new_adobe_rgb(),
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
        _ => unreachable!("runtime-only RGB space cannot be a RasterColor"),
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
            "ITU-R BT.709" => Some(ToneReprCurve::Parametric(vec![2.4])),
            "Adobe RGB (1998)" => Some(ToneReprCurve::Parametric(vec![563.0 / 256.0])),
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
    if matches!(
        space,
        RgbSpace::Rec709 | RgbSpace::DciP3 | RgbSpace::AdobeRgb
    ) {
        profile.cicp = None;
        profile.description = Some(ProfileText::PlainString(format!(
            "{}{}",
            space.name(),
            if color.cctf_encoding { "" } else { " (linear)" },
        )));
    }
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
    let description = embedded.description.as_ref().and_then(|text| match text {
        ProfileText::PlainString(text) => Some(text.as_str()),
        ProfileText::Localizable(text) => text.first().map(|entry| entry.value.as_str()),
        ProfileText::Description(text) => Some(text.ascii_string.as_str()),
    });
    // Linear Rec.709 and linear sRGB share primaries and curves. Prefer the
    // named color space when its colorants and curves also match.
    let named_space = description
        .map(|text| text.strip_suffix(" (linear)").unwrap_or(text))
        .and_then(RgbSpace::from_name)
        .filter(|space| RgbSpace::ALL.contains(space));
    for space in named_space.into_iter().chain(RgbSpace::ALL) {
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
        attributes.other.insert(
            Text::from("spektrafilmRgbSpace"),
            AttributeValue::Text(Text::from(space.name())),
        );
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
    let encoding = attributes
        .other
        .get(&Text::from(EXR_TRANSFER))
        .or_else(|| layer.other.get(&Text::from(EXR_TRANSFER)));
    let encoded = matches!(
        encoding,
        Some(AttributeValue::Text(value)) if value.as_slice() == b"encoded"
    );
    let space_tag = Text::from("spektrafilmRgbSpace");
    let tagged_space = attributes
        .other
        .get(&space_tag)
        .or_else(|| layer.other.get(&space_tag))
        .and_then(|attribute| match attribute {
            AttributeValue::Text(value) => std::str::from_utf8(value.as_slice()).ok(),
            _ => None,
        })
        .and_then(RgbSpace::from_name);
    for space in tagged_space.into_iter().chain(RgbSpace::ALL) {
        let expected = chromaticities(space);
        if near(actual.red, expected.red)
            && near(actual.green, expected.green)
            && near(actual.blue, expected.blue)
            && near(actual.white, expected.white)
        {
            if let Some(color) = RasterColor::new(space.name(), encoded) {
                return Some(color);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn added_rgb_spaces_roundtrip_icc_metadata() -> Result<()> {
        for space in [
            RgbSpace::Rec709,
            RgbSpace::DisplayP3,
            RgbSpace::DciP3,
            RgbSpace::AdobeRgb,
        ] {
            for encoded in [true, false] {
                let color = RasterColor::new(space.name(), encoded).unwrap();
                let bytes = make_icc(color)?.unwrap();
                assert_eq!(identify_icc(&bytes), Some(color));
            }
        }
        Ok(())
    }

    #[test]
    fn exr_tags_disambiguate_srgb_and_rec709() {
        let mut attrs = ImageAttributes::with_size((1, 1));
        let color = RasterColor::new(RgbSpace::Rec709.name(), false);
        set_exr_color(&mut attrs, color);
        assert_eq!(identify_exr(&attrs, &LayerAttributes::default()), color);
    }
}
