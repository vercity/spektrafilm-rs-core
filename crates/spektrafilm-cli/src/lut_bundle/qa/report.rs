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
            "noise_sensitivity" => {
                if let Some(noise) = &metrics.diagnostics.noise_sensitivity {
                    noise_field(&mut html, &noise.field)?;
                }
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
    spectral_envelope(&mut html, metrics)?;
    input_gamut(&mut html, metrics)?;
    output_gamut(&mut html, metrics)?;
    noise_gradient(&mut html, metrics)?;
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
    model_curves(html, metrics)?;
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

fn model_curves(html: &mut String, metrics: &Metrics) -> Result<()> {
    use spektrafilm_math::spectral::{CMF_X_F64, CMF_Y_F64, CMF_Z_F64};

    let characteristic = &metrics.characteristic_curve;
    html.push_str("<h3>Characteristic curves</h3><p>Encoded density across each input channel at five fixed neutral densities (0.2–1.0). The neutral sweep varies all three input channels together.</p>");
    metric_table(html, &serde_json::to_value(characteristic)?)?;
    html.push_str(r#"<div class="panels">"#);
    for (axis, title) in ["Red input", "Green input", "Blue input"]
        .into_iter()
        .enumerate()
    {
        figure_density_curve_open(html, title)?;
        for (sweep, opacity) in characteristic
            .sweeps
            .iter()
            .zip([0.32, 0.50, 1.0, 0.65, 0.40])
        {
            figure_density_lines(
                html,
                &sweep.channels[axis],
                opacity,
                Some(sweep.pin_density),
            )?;
        }
        write!(html, "</svg><figcaption>{title}</figcaption></figure>")?;
    }
    figure_density_curve_open(html, "Neutral input")?;
    figure_density_lines(html, &characteristic.neutral, 1.0, None)?;
    html.push_str("</svg><figcaption>Neutral input</figcaption></figure></div>");

    let white = &metrics.planckian_sweep;
    html.push_str("<h3>Daylight and Planckian white sweep</h3><p>Chromaticity after the LUT. Bends over 30° between successive temperatures are flagged.</p>");
    metric_table(html, &serde_json::to_value(white)?)?;
    let project = |[x, y]: [f64; 2]| (x * 220.0 + 2.0, 186.0 - y * 208.0);
    html.push_str(r#"<figure><svg viewBox="0 0 190 190" role="img" aria-label="White output chromaticity across daylight temperatures and the CIE locus" style="max-width:23rem"><polygon fill="none" stroke="currentColor" opacity=".35" points=""#);
    for ((x, y), z) in CMF_X_F64.into_iter().zip(CMF_Y_F64).zip(CMF_Z_F64) {
        let (x, y) = project(super::curves::chromaticity([x, y, z]));
        write!(html, "{x:.3} {y:.3} ")?;
    }
    html.push_str(r#""/><polyline fill="none" stroke="currentColor" points=""#);
    for xy in white.output_xy {
        let (x, y) = project(xy);
        write!(html, "{x:.3} {y:.3} ")?;
    }
    html.push_str(r#""/>"#);
    for (i, xy) in white.output_xy.into_iter().enumerate() {
        let (x, y) = project(xy);
        let temperature = 2700.0 + i as f64 * 7300.0 / (white.output_xy.len() - 1) as f64;
        write!(
            html,
            r#"<circle cx="{x:.3}" cy="{y:.3}" r="1" fill="currentColor"><title>{temperature:.0} K</title></circle>"#
        )?;
    }
    html.push_str("</svg><figcaption>Output CIE xy over the spectral locus. Each point shows its temperature on hover.</figcaption></figure>");
    Ok(())
}

fn figure_density_curve_open(html: &mut String, title: &str) -> Result<()> {
    write!(
        html,
        r#"<figure><svg viewBox="0 0 129 129" role="img" aria-label="{title} versus encoded output density"><path d="M1 1 V128 H128" fill="none" stroke="currentColor" opacity=".5"/>"#
    )?;
    Ok(())
}

fn figure_density_lines(
    html: &mut String,
    line: &[[f64; 3]],
    opacity: f64,
    pin: Option<f64>,
) -> Result<()> {
    for (channel, (label, color)) in [
        ("Red", "#dc5252"),
        ("Green", "#32a86a"),
        ("Blue", "#5580d8"),
    ]
    .into_iter()
    .enumerate()
    {
        write!(
            html,
            r#"<polyline fill="none" stroke="{color}" opacity="{opacity}" points=""#
        )?;
        for (i, value) in line.iter().enumerate() {
            let x = 1.0 + i as f64 * 126.0 / (line.len() - 1) as f64;
            write!(html, "{x:.2} {:.2} ", 127.0 - value[channel] * 31.5)?;
        }
        write!(html, r#""><title>{label} output"#)?;
        if let Some(pin) = pin {
            write!(html, " at density {pin:.1}")?;
        }
        html.push_str("</title></polyline>");
    }
    Ok(())
}

fn spectral_envelope(html: &mut String, metrics: &Metrics) -> Result<()> {
    use super::spectral_envelope::{SIDE, project};

    let envelope = &metrics.spectral_locus_envelope;
    html.push_str("<section><h2>Spectral locus envelope</h2><p>All valid LUT grid samples in CIE xy. The output gamut triangle and the visible spectral locus are drawn over the distribution.</p>");
    metric_table(html, &serde_json::to_value(envelope)?)?;
    write!(
        html,
        r##"<figure><svg viewBox="0 0 {SIDE} {SIDE}" role="img" aria-label="Output chromaticity map with reference gamut and spectral locus" style="max-width:32rem"><image width="{SIDE}" height="{SIDE}" href="data:image/png;base64,{}"/><polygon fill="none" stroke="#aaa" opacity=".6" points=""##,
        STANDARD.encode(&envelope.png)
    )?;
    for &xy in envelope.locus {
        let [x, y] = project(xy);
        write!(html, "{x:.2} {y:.2} ")?;
    }
    html.push_str(r##""/><polygon fill="none" stroke="#eee" points=""##);
    for xy in envelope.primaries {
        let [x, y] = project(xy);
        write!(html, "{x:.2} {y:.2} ")?;
    }
    html.push_str(r#""/>"#);
    let [x, y] = project(envelope.white);
    write!(
        html,
        r##"<circle cx="{x:.2}" cy="{y:.2}" r="2.5" fill="#eee"><title>Output white point</title></circle></svg><figcaption>The white triangle marks the output gamut. The marked point is its reference white.</figcaption></figure></section>"##
    )?;
    Ok(())
}

fn input_gamut(html: &mut String, metrics: &Metrics) -> Result<()> {
    use super::{
        input_gamut::Outcome,
        spectral_envelope::{SIDE, project},
    };
    use spektrafilm_core::input_gamut::spectral_locus_xy;

    let Some(outcome) = &metrics.input_gamut_compression else {
        return Ok(());
    };
    html.push_str("<section><h2>Input gamut compression</h2>");
    let reports = match outcome {
        Outcome::Reports(reports) => reports,
        Outcome::Error { error } => {
            write!(html, "<p>{}</p></section>", escape(error))?;
            return Ok(());
        }
    };

    let open_plot = |html: &mut String, label: &str| -> Result<()> {
        write!(
            html,
            r##"<figure><svg viewBox="0 0 {SIDE} {SIDE}" role="img" aria-label="{label}" style="max-width:32rem;background:#0a0a0a">"##
        )?;
        xy_shape(html, spectral_locus_xy(), "polygon", "#bbb", 0.7)?;
        xy_shape(html, &reports.primaries, "polygon", "#ffef68", 0.7)?;
        let [x, y] = project(reports.white);
        write!(
            html,
            r##"<circle cx="{x:.2}" cy="{y:.2}" r="3" fill="#ddd"><title>Film reference illuminant</title></circle>"##
        )?;
        Ok(())
    };
    html.push_str("<h3>Compression preview</h3><p>Sampled LUT inputs projected into the film reference frame. Red dots are outside the visible spectral locus. Orange lines connect a selection of moved points to their cyan destinations.</p>");
    metric_table(html, &serde_json::to_value(&reports.preview)?)?;
    open_plot(
        html,
        "Original and compressed locations of LUT inputs in CIE xy",
    )?;
    for sample in &reports.preview.samples {
        let [x, y] = project(sample.original);
        let [cx, cy] = project(sample.compressed);
        let dot = if sample.in_locus {
            "#60dd9a"
        } else {
            "#ff6262"
        };
        let alpha = if sample.in_locus { 0.32 } else { 0.65 };
        write!(
            html,
            r#"<circle cx="{x:.2}" cy="{y:.2}" r="1.4" fill="{dot}" opacity="{alpha}"/>"#
        )?;
        if (cx - x).hypot(cy - y) > 0.55 {
            write!(
                html,
                r##"<path d="M{x:.2} {y:.2} L{cx:.2} {cy:.2}" stroke="#fa5" opacity=".34"/><circle cx="{cx:.2}" cy="{cy:.2}" r="1.4" fill="#6df" opacity=".8"/>"##
            )?;
        }
    }
    html.push_str("</svg></figure>");

    html.push_str("<h3>Circumferential smoothness</h3><p>The hue-colored points show the compressed path of a ring about the film reference white. Closely or unevenly spaced points show where compression changes speed.</p>");
    metric_table(html, &serde_json::to_value(&reports.smoothness)?)?;
    open_plot(
        html,
        "Chromaticity ring before and after input gamut compression",
    )?;
    xy_shape(html, &reports.smoothness.original, "polygon", "#888", 0.6)?;
    for (i, &xy) in reports.smoothness.compressed.iter().enumerate() {
        let [x, y] = project(xy);
        let hue = i as f64 * 360.0 / reports.smoothness.compressed.len() as f64;
        write!(
            html,
            r#"<circle cx="{x:.2}" cy="{y:.2}" r="1.7" fill="hsl({hue:.1} 100% 65%)"/>"#
        )?;
    }
    html.push_str("</svg></figure></section>");
    Ok(())
}

fn xy_shape(
    html: &mut String,
    points: &[[f64; 2]],
    element: &str,
    stroke: &str,
    opacity: f64,
) -> Result<()> {
    write!(
        html,
        r#"<{element} fill="none" stroke="{stroke}" opacity="{opacity}" points=""#
    )?;
    for &xy in points {
        let [x, y] = super::spectral_envelope::project(xy);
        write!(html, "{x:.2} {y:.2} ")?;
    }
    html.push_str(r#""/>"#);
    Ok(())
}

fn output_gamut(html: &mut String, metrics: &Metrics) -> Result<()> {
    use super::{
        output_gamut::SEGMENT_SAMPLES,
        spectral_envelope::{SIDE, project},
    };
    use spektrafilm_core::input_gamut::spectral_locus_xy;

    let diagnostic = &metrics.output_gamut_compression;
    html.push_str("<section><h2>Output gamut compression</h2><p>Convex-hull volume in output-space OkLab and face folds measure the effective LUT. The rim compares the live model’s linear RGB before and after output compression.</p>");
    metric_table(html, &serde_json::to_value(diagnostic)?)?;
    if let Some(error) = diagnostic.error() {
        write!(html, "<p>{}</p>", escape(error))?;
    }
    if let Some(rim) = &diagnostic.rim {
        let project_lab =
            |[l, a, b]: [f64; 3]| [0.65 * a - 0.32 * b, 0.48 * l + 0.22 * a + 0.25 * b];
        let projected: Vec<_> = rim
            .unbounded_oklab
            .iter()
            .chain(&rim.compressed_oklab)
            .chain(&diagnostic.output_oklab)
            .map(|&point| project_lab(point))
            .collect();
        let side = f64::from(SIDE);
        let bounds: [[f64; 2]; 2] = std::array::from_fn(|axis| {
            projected
                .iter()
                .fold([f64::INFINITY, f64::NEG_INFINITY], |[lo, hi], p| {
                    [lo.min(p[axis]), hi.max(p[axis])]
                })
        });
        let lab_screen = |p| {
            let [x, y] = project_lab(p);
            let tx =
                12.0 + (side - 24.0) * (x - bounds[0][0]) / (bounds[0][1] - bounds[0][0]).max(1e-3);
            let ty =
                12.0 + (side - 24.0) * (y - bounds[1][0]) / (bounds[1][1] - bounds[1][0]).max(1e-3);
            [tx, side - ty]
        };
        let open = |html: &mut String, label: &str| -> Result<()> {
            write!(
                html,
                r##"<figure><svg viewBox="0 0 {SIDE} {SIDE}" role="img" aria-label="{label}" style="background:#0a0a0a">"##
            )?;
            Ok(())
        };
        html.push_str(r#"<div class="panels">"#);
        open(html, "Output rim and LUT gamut in OkLab")?;
        let skip = (diagnostic.output_oklab.len() / 1600).max(1);
        for &p in diagnostic.output_oklab.iter().step_by(skip) {
            let [x, y] = lab_screen(p);
            write!(
                html,
                r##"<circle cx="{x:.2}" cy="{y:.2}" r="1.4" fill="#b9d8ef" opacity=".15"/>"##
            )?;
        }
        let draw = |html: &mut String, i, color: [f64; 3], points: &[[f64; 2]]| -> Result<()> {
            let [r, g, b] = color.map(|v| (0.26 + 0.74 * v) * 255.0);
            let dash = if i == 0 {
                ""
            } else {
                r#" stroke-dasharray="3 2""#
            };
            for segment in points.chunks_exact(SEGMENT_SAMPLES) {
                write!(
                    html,
                    r#"<polyline fill="none" stroke="rgb({r:.0},{g:.0},{b:.0})" stroke-width="2.2"{dash} points=""#
                )?;
                for [x, y] in segment {
                    write!(html, "{x:.2} {y:.2} ")?;
                }
                html.push_str(r#""/>"#);
            }
            Ok(())
        };
        for (mode, points) in [&rim.unbounded_oklab, &rim.compressed_oklab]
            .into_iter()
            .enumerate()
        {
            for (segment, colors) in points
                .chunks_exact(SEGMENT_SAMPLES)
                .zip(rim.colors.chunks_exact(SEGMENT_SAMPLES))
            {
                let projected: Vec<_> = segment.iter().map(|&p| lab_screen(p)).collect();
                draw(html, mode, colors[SEGMENT_SAMPLES / 2], &projected)?;
            }
        }
        html.push_str("</svg><figcaption>OkLab projection with the delivered LUT sample cloud</figcaption></figure>");
        open(html, "Output rim in CIE xy")?;
        xy_shape(html, spectral_locus_xy(), "polygon", "#bbb", 0.65)?;
        xy_shape(
            html,
            &metrics.spectral_locus_envelope.primaries,
            "polygon",
            "#eee",
            0.65,
        )?;
        for (mode, points) in [&rim.unbounded_xy, &rim.compressed_xy]
            .into_iter()
            .enumerate()
        {
            for (segment, colors) in points
                .chunks_exact(SEGMENT_SAMPLES)
                .zip(rim.colors.chunks_exact(SEGMENT_SAMPLES))
            {
                let projected: Vec<_> = segment.iter().map(|&p| project(p)).collect();
                draw(html, mode, colors[SEGMENT_SAMPLES / 2], &projected)?;
            }
        }
        html.push_str("</svg><figcaption>CIE xy with output primaries and the spectral locus</figcaption></figure></div><p>Solid lines show the unbounded rim. Dashes show the compressed rim.</p>");
    }
    html.push_str("</section>");
    Ok(())
}

fn noise_gradient(html: &mut String, metrics: &Metrics) -> Result<()> {
    use super::noise_gradient::SIDE;

    let noise = &metrics.noise_gradient;
    html.push_str("<section><h2>Noise gradient</h2><p>The OkLab hue cycle runs left to right. Lightness rises from black at the bottom to white at the top, with peak chroma near 18% gray.</p>");
    metric_table(html, &serde_json::to_value(noise)?)?;
    html.push_str(r#"<div class="panels">"#);
    for (image, title) in [
        (&noise.clean, "Clean LUT output"),
        (
            &noise.noisy,
            "Same gradient with seeded encoded-input noise",
        ),
    ] {
        write!(
            html,
            r#"<figure><img width="{SIDE}" height="{SIDE}" alt="{title}" src="data:image/png;base64,{}"><figcaption>{title}</figcaption></figure>"#,
            STANDARD.encode(image)
        )?;
    }
    html.push_str("</div></section>");
    Ok(())
}

fn noise_field(html: &mut String, field: &super::noise_field::Field) -> Result<()> {
    use super::noise_field::HEATMAP_SIDE;

    const SIDE: f64 = 240.0;
    let half = SIDE * 0.5;
    let scale = half / field.panel_extent;
    let project = |[a, b]: [f64; 2]| [half + scale * a, half - scale * b];
    let has_rosette = field.samples.iter().any(|p| p.hue.is_some());
    let open = |html: &mut String, label: &str| -> Result<()> {
        write!(
            html,
            r##"<figure><svg viewBox="0 0 {SIDE} {SIDE}" role="img" aria-label="{label}" style="background:#111317">"##
        )?;
        Ok(())
    };
    let axes = |html: &mut String| -> Result<()> {
        write!(
            html,
            r##"<path d="M0 {half} H{SIDE} M{half} 0 V{SIDE}" stroke="#9b9fa8" opacity=".3"/>"##
        )?;
        Ok(())
    };
    let outline = |html: &mut String| -> Result<()> {
        for (line, style) in [
            (&field.input_outline, r##"stroke="#61a0f9""##),
            (
                &field.output_outline,
                r##"stroke="#f2cb58" stroke-dasharray="4 2""##,
            ),
        ] {
            write!(html, r#"<polygon {style} fill="none" points=""#)?;
            for &p in line {
                let [x, y] = project(p);
                write!(html, "{x:.2} {y:.2} ")?;
            }
            html.push_str(r#""/>"#);
        }
        Ok(())
    };
    let color = |gain: f64| {
        let f = (gain / field.color_limit).clamp(0.0, 1.0);
        let r = 26.0 + 229.0 * f;
        let g = 18.0 + 213.0 * f.powi(3);
        let b = 66.0 + 112.0 * f;
        format!("rgb({r:.0},{g:.0},{b:.0})")
    };

    html.push_str(r#"<div class="panels">"#);
    open(html, "OkLab covariance ellipses")?;
    axes(html)?;
    outline(html)?;
    for point in &field.samples {
        let [x0, y0] = project(point.input_ab);
        let [x, y] = project(point.output_ab);
        let [rx, ry] = point.ellipse_radii.map(|r| (scale * r).max(0.2));
        let angle = -point.ellipse_angle_deg;
        let edge = color(point.sigma1);
        let [r, g, b] = point.output_rgb.map(|c| c.clamp(0.0, 1.0) * 255.0);
        write!(
            html,
            r##"<path d="M{x0:.2} {y0:.2} L{x:.2} {y:.2}" stroke="#aaa" opacity=".45"/><ellipse cx="{x:.2}" cy="{y:.2}" rx="{rx:.2}" ry="{ry:.2}" transform="rotate({angle:.2} {x:.2} {y:.2})" fill="rgb({r:.0},{g:.0},{b:.0})" stroke="{edge}" opacity=".9"/>"##
        )?;
    }
    html.push_str("</svg><figcaption>OkLab a*/b* noise ellipses at 2σ (1.5× for display)</figcaption></figure>");

    open(
        html,
        "Worst-case noise gain heatmap across input OkLab chromaticity",
    )?;
    let width = 2.0 * scale * field.domain_extent / (HEATMAP_SIDE - 1) as f64;
    let edge = field.domain_extent;
    for (i, value) in field.gain.iter().enumerate() {
        if let Some(gain) = value {
            let [x, y] = project([
                -edge + (i % HEATMAP_SIDE) as f64 * width / scale,
                -edge + (i / HEATMAP_SIDE) as f64 * width / scale,
            ])
            .map(|v| v - width * 0.5);
            let fill = color(*gain);
            write!(
                html,
                r#"<rect x="{x:.2}" y="{y:.2}" width="{width:.2}" height="{width:.2}" fill="{fill}" shape-rendering="crispEdges"/>"#
            )?;
        }
    }
    axes(html)?;
    outline(html)?;
    write!(
        html,
        "</svg><figcaption>Worst-case gain σ₁(J). The color scale runs from 0 to {:.2}.</figcaption></figure>",
        field.color_limit
    )?;

    if has_rosette {
        open(
            html,
            "Hue rosette of luminance and chroma noise at mid chroma",
        )?;
        let peak = field
            .samples
            .iter()
            .filter(|p| p.hue.is_some())
            .map(|p| p.sigma_l.max(p.sigma_ab))
            .fold(1e-6_f64, f64::max);
        axes(html)?;
        for grid in [0.25, 0.5, 0.75, 1.0] {
            let radius = 0.43 * SIDE * grid;
            write!(
                html,
                r##"<circle cx="{half}" cy="{half}" r="{radius:.2}" stroke="#9b9fa8" fill="none" opacity=".3"/>"##
            )?;
        }
        for (chroma, color) in [(false, "#86cfff"), (true, "#ff88a8")] {
            write!(
                html,
                r#"<polygon stroke="{color}" stroke-width="2" fill="{color}" fill-opacity=".11" points=""#
            )?;
            for s in &field.samples {
                if let Some(angle) = s.hue {
                    let sigma = if chroma { s.sigma_ab } else { s.sigma_l };
                    let r = (0.43 * SIDE) * sigma / peak;
                    let [x, y] = [half + r * angle.cos(), half - r * angle.sin()];
                    write!(html, "{x:.2} {y:.2} ")?;
                }
            }
            html.push_str(r#""/>"#);
        }
        write!(
            html,
            "</svg><figcaption>Hue rosette. Blue is luminance noise and pink is chroma noise. Outer ring σ={peak:.4}.</figcaption></figure>"
        )?;
    }
    html.push_str("</div><p>Solid blue is the input gamut. Dashed gold is the output gamut.</p>");
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
