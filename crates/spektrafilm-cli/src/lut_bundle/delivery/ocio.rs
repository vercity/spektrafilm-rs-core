//! OCIO 2.4 configuration for the canonical LUT paths.

use std::{collections::BTreeSet, fmt::Write as _};

use anyhow::{Context, Result, bail, ensure};

use super::{BundleMeta, ColorSpace, LutMeta};
use crate::lut_bundle::stock_tag;

const REFERENCE: &str = "ACES2065-1";
const XYZ_D65: &str = "UTILITY - ACES-AP0_to_CIE-XYZ-D65_BFD";

struct OcioSpace {
    encoding: &'static str,
    alias: &'static str,
    builtin: &'static str,
    display: bool,
}

fn supported_space(space: &ColorSpace) -> Result<OcioSpace> {
    let (encoding, alias, builtin, display) = match (space.name.as_str(), space.cctf) {
        (REFERENCE, false) => ("scene-linear", "lin_ap0", "", false),
        ("sRGB", true) => (
            "sdr-video",
            "sRGB - Display",
            "DISPLAY - CIE-XYZ-D65_to_sRGB",
            true,
        ),
        ("Rec.709", true) => (
            "sdr-video",
            "Rec.1886 Rec.709 - Display",
            "DISPLAY - CIE-XYZ-D65_to_REC.1886-REC.709",
            true,
        ),
        ("Rec.2020", true) => (
            "sdr-video",
            "Rec.1886 Rec.2020 - Display",
            "DISPLAY - CIE-XYZ-D65_to_REC.1886-REC.2020",
            true,
        ),
        ("Display P3", true) => (
            "sdr-video",
            "Display P3 - Display",
            "DISPLAY - CIE-XYZ-D65_to_DisplayP3",
            true,
        ),
        ("Rec.2100 PQ", true) => (
            "log",
            "Rec.2100-PQ - Display",
            "DISPLAY - CIE-XYZ-D65_to_REC.2100-PQ",
            true,
        ),
        ("Rec.2100 HLG", true) => (
            "log",
            "Rec.2100-HLG - Display",
            "DISPLAY - CIE-XYZ-D65_to_REC.2100-HLG-1000nit",
            true,
        ),
        ("Panasonic V-Log", true) => (
            "log",
            "Panasonic V-Log - V-Gamut",
            "PANASONIC_VLOG-VGAMUT_to_ACES2065-1",
            false,
        ),
        _ => bail!("OpenColorIO has no supported mapping for {}", space.name),
    };
    Ok(OcioSpace {
        encoding,
        alias,
        builtin,
        display,
    })
}

fn yaml(value: &str) -> String {
    // JSON strings are valid YAML double-quoted scalars, including control characters.
    serde_json::to_string(value).expect("serializing a string cannot fail")
}

fn chain_for_print<'meta>(
    meta: &'meta BundleMeta,
    print: &str,
    expected: usize,
) -> Result<Vec<&'meta LutMeta>> {
    let mut chain = Vec::with_capacity(expected);
    let mut domain = "input_rgb";
    for lut in &meta.luts {
        if lut.role.starts_with("subchain_")
            || lut
                .print_profile
                .as_deref()
                .is_some_and(|profile| profile != print)
        {
            continue;
        }
        ensure!(
            lut.domain == domain,
            "OpenColorIO chain for {print} expects {domain}, got {}",
            lut.domain
        );
        chain.push(lut);
        domain = &lut.range;
    }
    ensure!(
        chain.len() == expected && domain == "output_rgb",
        "invalid canonical OpenColorIO chain for {print}"
    );
    Ok(chain)
}

fn write_colorspace(
    out: &mut String,
    name: &str,
    family: &str,
    encoding: &str,
    description: &str,
) -> Result<()> {
    writeln!(
        out,
        "  - !<ColorSpace>\n    name: {}\n    family: {}\n    encoding: {encoding}\n    isdata: false",
        yaml(name),
        yaml(family),
    )?;
    if !description.is_empty() {
        writeln!(out, "    description: {}", yaml(description))?;
    }
    Ok(())
}

fn write_endpoint(
    out: &mut String,
    spec: &ColorSpace,
    mapping: &OcioSpace,
    family: &str,
) -> Result<()> {
    if spec.name == REFERENCE {
        return Ok(());
    }
    write_colorspace(out, &spec.name, family, mapping.encoding, "")?;
    writeln!(out, "    aliases: [{}]", yaml(mapping.alias))?;
    writeln!(
        out,
        "    from_scene_reference: !<GroupTransform>\n      children:"
    )?;
    if mapping.display {
        writeln!(
            out,
            "        - !<BuiltinTransform> {{style: {}}}",
            yaml(XYZ_D65)
        )?;
        writeln!(
            out,
            "        - !<BuiltinTransform> {{style: {}}}\n",
            yaml(mapping.builtin)
        )?;
    } else {
        writeln!(
            out,
            "        - !<BuiltinTransform> {{style: {}, direction: inverse}}\n",
            yaml(mapping.builtin),
        )?;
    }
    Ok(())
}

fn write_chain(out: &mut String, input: &ColorSpace, chain: &[&LutMeta]) -> Result<()> {
    writeln!(
        out,
        "    from_scene_reference: !<GroupTransform>\n      children:"
    )?;
    if input.name != REFERENCE {
        writeln!(
            out,
            "        - !<ColorSpaceTransform> {{src: \"{REFERENCE}\", dst: {}}}",
            yaml(&input.name),
        )?;
    }
    for lut in chain {
        writeln!(
            out,
            "        - !<FileTransform> {{src: {}, interpolation: tetrahedral}}",
            yaml(&lut.path),
        )?;
    }
    writeln!(out)?;
    Ok(())
}

fn wire_description(meta: &BundleMeta, tap: &str) -> Result<String> {
    let wire = meta
        .wires
        .get(tap)
        .and_then(Option::as_ref)
        .with_context(|| format!("OpenColorIO configuration needs constants for {tap}"))?;
    if tap == "cmy_film" {
        Ok(format!(
            "Normalized CMY film density. d_min: {}; d_max: {}. Decode with code * (d_max - d_min) + d_min.",
            wire["d_min"], wire["d_max"],
        ))
    } else {
        Ok(format!(
            "Normalized log10(exposure). min: {}; max: {}. Decode with code * (max - min) + min.",
            wire["min"], wire["max"],
        ))
    }
}

pub(super) fn generate(meta: &BundleMeta) -> Result<String> {
    let input = &meta.color_spaces.input;
    let output = &meta.color_spaces.output;
    let input_mapping = supported_space(input)?;
    let output_mapping = supported_space(output)?;
    let stages = match meta.topology.as_str() {
        "1lut" => 1,
        "2lut" => 2,
        "3lut" => 3,
        "4lut" => 4,
        other => bail!("unsupported OpenColorIO topology {other}"),
    };
    let film = stock_tag(&meta.stocks.film);
    let mut out = String::new();
    writeln!(
        out,
        "ocio_profile_version: 2.4\nname: {}\nsearch_path: .\nfamily_separator: /\n\nroles:",
        yaml(&meta.name),
    )?;
    for role in [
        "aces_interchange",
        "color_timing",
        "compositing_log",
        "default",
        "scene_linear",
    ] {
        writeln!(out, "  {role}: {REFERENCE}")?;
    }

    let output_name = yaml(&output.name);
    writeln!(out, "\ndisplays:\n  {output_name}:")?;
    let mut views = Vec::new();
    let mut names = BTreeSet::new();
    let mut prints = Vec::new();
    for print in &meta.stocks.prints {
        let tag = stock_tag(print);
        let name = format!("spektrafilm_{film}_{tag}");
        ensure!(
            names.insert(name.clone()),
            "duplicate OpenColorIO color space {name}"
        );
        if stages == 1 {
            let view = yaml(&format!(
                "Spektrafilm {} / {}",
                meta.stocks.film.replace('_', " "),
                print.replace('_', " "),
            ));
            writeln!(
                out,
                "    - !<View> {{name: {view}, colorspace: {}}}",
                yaml(&name)
            )?;
            views.push(view);
        }
        prints.push((tag, name, chain_for_print(meta, print, stages)?));
    }
    writeln!(
        out,
        "    - !<View> {{name: Raw, colorspace: {output_name}}}"
    )?;
    views.push(yaml("Raw"));
    writeln!(
        out,
        "\nactive_displays: [{output_name}]\nactive_views: [{}]",
        views.join(", ")
    )?;

    writeln!(out, "\ncolorspaces:")?;
    write_colorspace(
        &mut out,
        REFERENCE,
        "ACES",
        "scene-linear",
        "Scene-reference AP0 primaries.",
    )?;
    writeln!(out, "    aliases: [lin_ap0]\n")?;
    write_endpoint(&mut out, input, &input_mapping, "Input")?;
    if output.name != input.name {
        write_endpoint(&mut out, output, &output_mapping, "Output")?;
    }

    for (print, name, chain) in &prints {
        for (index, lut) in chain.iter().take(stages - 1).enumerate() {
            let suffix = if lut.range == "log_e_print" {
                format!("{film}_{print}")
            } else {
                film.clone()
            };
            let intermediate = format!("{}_{suffix}", lut.range);
            if names.insert(intermediate.clone()) {
                write_colorspace(
                    &mut out,
                    &intermediate,
                    "spektrafilm/intermediates",
                    "log",
                    &wire_description(meta, &lut.range)?,
                )?;
                write_chain(&mut out, input, &chain[..=index])?;
            }
        }
        write_colorspace(
            &mut out,
            name,
            &format!("spektrafilm/{film}/{print}"),
            output_mapping.encoding,
            &format!(
                "Spektrafilm {} canonical chain to {}.",
                meta.topology, output.name
            ),
        )?;
        write_chain(&mut out, input, chain)?;
    }
    Ok(out)
}
