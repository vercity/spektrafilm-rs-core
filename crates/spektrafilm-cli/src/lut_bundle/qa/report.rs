use std::{fmt::Write, fs, path::Path};

use anyhow::{Context, Result};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::Value;

use super::{InterpolationMetrics, Metrics, gamut_edge};
use crate::lut_bundle::stock_tag;

pub(super) fn write_html(bundle: &Path, film: &str, paper: &str, metrics: &Metrics) -> Result<()> {
    let directory = bundle
        .join("qa")
        .join(format!("{}_{}", stock_tag(film), stock_tag(paper)));
    fs::create_dir_all(&directory).with_context(|| format!("creating {}", directory.display()))?;
    let film = escape(film);
    let paper = escape(paper);
    let mut html = String::new();
    write!(
        html,
        r#"<!doctype html>
<html lang="en">
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>LUT QA · {film} / {paper}</title>
<style>
:root{{color-scheme:light dark;font:16px/1.5 system-ui,sans-serif}}
body{{margin:2rem auto;padding:0 1rem;max-width:55rem}}
table{{border-collapse:collapse;width:100%;font-variant-numeric:tabular-nums}}
th,td{{padding:.35em .55em;text-align:left;border-bottom:1px solid GrayText}}
.table{{overflow-x:auto}}figure{{margin:1.5rem 0}}img{{width:100%;height:auto}}
figcaption{{font-size:.92em}}section+section{{margin-top:2.5rem}}
.panels{{display:grid;grid-template-columns:repeat(auto-fit,minmax(min(100%,12rem),1fr));gap:1rem}}
.panels figure{{margin:.5rem 0}}svg{{width:100%;height:auto}}
</style>
<main>
<h1>LUT QA</h1>
<p>{film} / {paper}</p>
<section>
<h2>Color error against live simulation</h2>
<p>50,000 PCG64 random samples. The sequential measurements apply the delivered CUBEs in host order. The resampled measurements use a single effective LUT baked on the original grid.</p>
<div class="table"><table>
<thead><tr><th scope="col">Method</th><th scope="col">Interpolation</th><th scope="col">Metric</th><th scope="col">Mean</th><th scope="col">p50</th><th scope="col">p95</th><th scope="col">p99</th><th scope="col">Max</th></tr></thead>
<tbody>"#
    )?;
    error_rows(&mut html, "Sequential", &metrics.sequential)?;
    error_rows(&mut html, "Resampled", &metrics.resampled)?;
    html.push_str(
        "</tbody></table></div></section>\n<section><h2>Effective LUT diagnostics</h2>\n",
    );
    let value = serde_json::to_value(&metrics.diagnostics)?;
    for (key, title, description) in [
        (
            "monotonicity",
            "Monotonicity",
            "Negative same-channel changes between neighboring nodes and along the 18% gray centerlines.",
        ),
        (
            "jacobian_condition",
            "Jacobian condition",
            "Log₁₀ of the local Jacobian condition number. Higher values indicate stronger directional imbalance.",
        ),
        (
            "total_variation",
            "Total variation and spectral smoothness",
            "Mean absolute changes between neighboring nodes, and the high-frequency fraction across all lines parallel to each axis.",
        ),
        (
            "gamut_face_folds",
            "Gamut-face folds",
            "Flipped projected triangles on the six faces of the input cube.",
        ),
        (
            "noise_sensitivity",
            "Noise sensitivity",
            "Local output OkLab response to encoded RGB noise at in-gamut OkLCh samples near midgray.",
        ),
    ] {
        write!(html, "<h3>{title}</h3><p>{description}</p>")?;
        metric_table(&mut html, &value[key])?;
        match key {
            "monotonicity" => centerlines(&mut html, &metrics.diagnostics.centerlines)?,
            "jacobian_condition" => {
                condition_planes(&mut html, &metrics.diagnostics.condition_planes)?
            }
            _ => {}
        }
    }
    html.push_str("</section>\n");
    if let Some(outcome) = &metrics.output_gamut_edge_stress {
        html.push_str("<section><h2>Output gamut edge stress</h2>\n");
        match outcome {
            gamut_edge::Outcome::Panels(panels) => {
                html.push_str("<p>Live simulation renders a white → saturated edge → black gradient for each target. The preview uses sRGB. The percentages measure how much of the target gradient falls outside the bundle input gamut.</p>\n");
                for panel in panels {
                    write!(
                        html,
                        r#"<figure>
<img width="{}" height="{}" alt="{} white-to-black gradient with a saturated rainbow center" src="data:image/png;base64,{}">
<figcaption><strong>{}</strong> · {:.2}% outside input gamut overall · {:.2}% in the saturated band</figcaption>
</figure>"#,
                        gamut_edge::WIDTH,
                        gamut_edge::HEIGHT,
                        panel.target,
                        STANDARD.encode(&panel.image_png),
                        panel.target,
                        panel.oog_fraction_total * 100.0,
                        panel.oog_fraction_saturated_row * 100.0
                    )?;
                }
            }
            gamut_edge::Outcome::Error { error } => {
                write!(html, "<p>{}</p>", escape(error))?;
            }
        }
        html.push_str("</section>\n");
    }
    model_usage(&mut html, metrics)?;
    html.push_str("</main></html>\n");
    fs::write(directory.join("report.html"), html)
        .with_context(|| format!("writing QA report in {}", directory.display()))
}

fn model_usage(html: &mut String, metrics: &Metrics) -> Result<()> {
    html.push_str("<section><h2>Rendering diagnostics</h2><h3>Dynamic range usage</h3><p>Gray ramp anchored at the input encoding’s native 18% gray. Active stops have a density slope above 0.10 per stop.</p>");
    metric_table(html, &serde_json::to_value(&metrics.dynamic_range_usage)?)?;
    let ramp = &metrics.dynamic_range_usage.ramp;
    let first = ramp[0].stops;
    let last = ramp[ramp.len() - 1].stops;
    let project_stops = |stops: f64| (stops - first) / (last - first) * 478.0 + 1.0;
    let gray = project_stops(0.0);
    write!(
        html,
        r#"<figure><svg viewBox="0 0 480 144" role="img" aria-label="Output density across exposure stops"><path d="M1 1 V129 H479 M{gray:.2} 1 V129" fill="none" stroke="currentColor" opacity=".4"/><polyline fill="none" stroke="currentColor" points=""#
    )?;
    let min_y = ramp.iter().map(|s| s.y.log10()).fold(0.0_f64, f64::min);
    let max_y = ramp.iter().map(|s| s.y.log10()).fold(0.0_f64, f64::max);
    let y_scale = 120.0 / (max_y - min_y).max(1e-3);
    let density_screen = |value: f64| 4.0 + (value.log10() - min_y) * y_scale;
    for sample in ramp {
        write!(
            html,
            "{:.2} {:.2} ",
            project_stops(sample.stops),
            density_screen(sample.y)
        )?;
    }
    html.push_str(r#""/>"#);
    for sample in ramp.iter().filter(|s| s.clipped).step_by(4) {
        write!(
            html,
            r##"<circle cx="{:.2}" cy="{:.2}" r="2" fill="#da514b"/>"##,
            project_stops(sample.stops),
            density_screen(sample.y)
        )?;
    }
    html.push_str("</svg><figcaption>Higher density is higher on the chart. The vertical line marks 0 EV and red dots mark input encoding clipping.</figcaption></figure>");
    html.push_str("<h3>OkLab hue twist</h3><p>Largest input-to-output hue changes in four input chroma bands. Inputs above 0.6 OkLab chroma are outside the test’s visible-color envelope.</p>");
    metric_table(html, &serde_json::to_value(&metrics.hue_twist_oklab)?)?;
    html.push_str("</section>\n<section><h2>Red–green plane slices</h2><p>Red increases left to right and green bottom to top. Each panel fixes the input blue value and shows the output on an sRGB display.</p>");
    html.push_str(r#"<div class="panels">"#);
    let size = metrics.rg_plane_slices.cube_resolution;
    for plane in &metrics.rg_plane_slices.images {
        let blue = plane.blue;
        write!(
            html,
            r#"<figure><img width="{size}" height="{size}" alt="Red–green plane at input blue {blue:.2}" src="data:image/png;base64,{}"><figcaption>B = {blue:.2}</figcaption></figure>"#,
            STANDARD.encode(&plane.png)
        )?;
    }
    html.push_str("</div></section>");
    Ok(())
}

fn error_rows(html: &mut String, method: &str, metrics: &InterpolationMetrics) -> Result<()> {
    for (interpolation, m) in [
        ("Trilinear", metrics.trilinear),
        ("Tetrahedral", metrics.tetrahedral),
    ] {
        for (name, values) in [
            (
                "ΔE₀₀",
                [
                    m.delta_e00_mean,
                    m.delta_e00_p50,
                    m.delta_e00_p95,
                    m.delta_e00_p99,
                    m.delta_e00_max,
                ],
            ),
            (
                "ΔITP",
                [
                    m.delta_itp_mean,
                    m.delta_itp_p50,
                    m.delta_itp_p95,
                    m.delta_itp_p99,
                    m.delta_itp_max,
                ],
            ),
        ] {
            write!(
                html,
                "<tr><td>{method}</td><td>{interpolation}</td><th scope=\"row\">{name}</th>"
            )?;
            for value in values {
                write!(html, "<td>{value:.5}</td>")?;
            }
            html.push_str("</tr>\n");
        }
    }
    Ok(())
}

fn centerlines(html: &mut String, centerlines: &[[f64; 65]; 3]) -> Result<()> {
    html.push_str(r#"<div class="panels">"#);
    for (line, channel) in centerlines.iter().zip(["Red", "Green", "Blue"]) {
        write!(
            html,
            r#"<figure><svg viewBox="0 0 130 130" role="img" aria-label="{channel} output vs input on the gray centerline"><path d="M1 0 V128 H129" fill="none" stroke="currentColor" opacity=".6"/><polyline fill="none" stroke="currentColor" points=""#
        )?;
        for (i, &value) in line.iter().enumerate() {
            write!(html, "{} {:.2} ", 1 + 2 * i, (1.0 - value) * 126.0 + 1.0)?;
        }
        html.push_str(r#""/>"#);
        for (i, pair) in line.windows(2).enumerate() {
            if pair[1] < pair[0] {
                write!(
                    html,
                    r##"<circle cx="{}" cy="{:.2}" r="2" fill="#da514b"/>"##,
                    1 + 2 * (i + 1),
                    (1.0 - pair[1]) * 126.0 + 1.0
                )?;
            }
        }
        write!(html, "</svg><figcaption>{channel}</figcaption></figure>")?;
    }
    html.push_str("</div><p>Gray centerline sweeps. Red dots mark negative steps.</p>");
    Ok(())
}

fn condition_planes(html: &mut String, planes: &[Vec<f64>; 3]) -> Result<()> {
    if planes[0].is_empty() {
        return Ok(());
    }
    let size = planes[0].len().isqrt();
    let peak = planes.iter().flatten().copied().fold(1.0_f64, f64::max);
    html.push_str(r#"<div class="panels">"#);
    for (plane, name) in planes
        .iter()
        .zip(["Green / Blue", "Red / Blue", "Red / Green"])
    {
        write!(
            html,
            r#"<figure><svg viewBox="0 0 {size} {size}" role="img" aria-label="{name} maximum Jacobian condition projection" shape-rendering="crispEdges">"#
        )?;
        for (i, value) in plane.iter().enumerate() {
            let fraction = value / peak;
            let r = 30.0 + 224.0 * fraction;
            let g = 36.0 + 160.0 * fraction;
            let b = 68.0 + 70.0 * (1.0 - fraction);
            write!(
                html,
                r#"<rect x="{}" y="{}" width="1" height="1" fill="rgb({r:.0},{g:.0},{b:.0})"/>"#,
                i % size,
                size - 1 - i / size
            )?;
        }
        write!(html, "</svg><figcaption>{name}</figcaption></figure>")?;
    }
    write!(
        html,
        "</div><p>Maximum across the omitted input axis. The shared color scale runs from dark (0) to light ({peak:.2}) log₁₀ condition.</p>"
    )?;
    Ok(())
}

fn metric_table(html: &mut String, value: &Value) -> Result<()> {
    let Some(values) = value.as_object() else {
        html.push_str("<p>Unavailable at this sampling resolution or input gamut.</p>\n");
        return Ok(());
    };
    html.push_str("<table><tbody>\n");
    for (key, value) in values {
        write!(
            html,
            "<tr><th scope=\"row\">{}</th><td>",
            escape(&key.replace('_', " "))
        )?;
        if value.is_object() {
            metric_table(html, value)?;
        } else {
            write!(html, "{}", escape(&value.to_string()))?;
        }
        html.push_str("</td></tr>\n");
    }
    html.push_str("</tbody></table>\n");
    Ok(())
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}
