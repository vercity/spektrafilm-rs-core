use std::fs;
use std::path::Path;
use std::process::{Command, Output};

use serde_json::Value;

fn export(topology: &str, output: &Path, args: &[&str]) -> Output {
    let data = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../data");
    Command::new(env!("CARGO_BIN_EXE_spektrafilm"))
        .args([
            "export-lut-bundle",
            "--film",
            "kodak_portra_400",
            "--paper",
            "kodak_portra_endura",
            "--paper",
            "kodak_2383",
            "--size",
            "5",
            "--topology",
            topology,
            "--output",
        ])
        .arg(output)
        .arg("--data-dir")
        .arg(data)
        .args(args)
        .output()
        .unwrap()
}

fn cube(path: &Path) -> Vec<[f64; 3]> {
    let text = fs::read_to_string(path).unwrap();
    assert!(text.lines().any(|line| line == "LUT_3D_SIZE 5"));
    let samples: Vec<_> = text
        .lines()
        .skip_while(|line| !line.starts_with("LUT_3D_SIZE"))
        .skip(1)
        .map(|line| {
            let rgb = line
                .split_whitespace()
                .map(|x| x.parse::<f64>().unwrap())
                .collect::<Vec<_>>();
            assert!(rgb.iter().all(|x| (0.0..=1.0).contains(x)));
            <[f64; 3]>::try_from(rgb).unwrap()
        })
        .collect();
    assert_eq!(samples.len(), 125);
    samples
}

#[test]
fn multi_print_cube_topologies_reproduce_the_shared_wire_contract() {
    let temp = tempfile::tempdir().unwrap();
    let mut shared_combination = None;
    let mut complete_combination = None;
    for (topology, canonical_count, all_count, complete_role) in [
        ("2lut", 3, 5, "subchain_12"),
        ("3lut", 4, 9, "subchain_123"),
        ("4lut", 6, 17, "subchain_1234"),
    ] {
        let output = temp.path().join(topology);
        let response = export(topology, &output, &["--combinations"]);
        assert!(
            response.status.success(),
            "{}",
            String::from_utf8_lossy(&response.stderr)
        );
        let metadata: Value =
            serde_json::from_slice(&fs::read(output.join("bundle.json")).unwrap()).unwrap();
        assert_eq!(metadata["schema_version"], 1);
        assert_eq!(metadata["topology"], topology);
        assert_eq!(metadata["color_spaces"]["input"]["name"], "sRGB");
        assert_eq!(metadata["input_exposure"]["stops_above_midgray"], 4.0);
        assert_eq!(metadata["input_exposure"]["gain"], 2.88);
        let wires = &metadata["wires"];
        assert_eq!(
            wires["cmy_film"]["d_min"],
            serde_json::json!([-0.2, -0.2, -0.2])
        );
        // The values are from the upstream's 9³ probing grid. Check that
        // topology and delivery grid size do not affect the wire endpoints.
        assert_eq!(
            wires["cmy_film"]["d_max"],
            serde_json::json!([1.586, 1.6045, 2.0476])
        );
        assert_eq!(wires["log_e_film"].is_object(), topology != "2lut");
        assert_eq!(wires["log_e_print"].is_object(), topology == "4lut");
        let files = metadata["luts"].as_array().unwrap();
        assert_eq!(files.len(), all_count);
        assert_eq!(
            files
                .iter()
                .filter(|file| !file["role"].as_str().unwrap().starts_with("subchain_"))
                .count(),
            canonical_count
        );
        let mut complete_outputs = Vec::new();
        for file in files {
            let samples = cube(&output.join(file["path"].as_str().unwrap()));
            if file["role"] == "film"
                || (file["role"] == "subchain_12" && file["print_profile"].is_null())
            {
                if let Some(expected) = &shared_combination {
                    assert_eq!(expected, &samples);
                } else {
                    shared_combination = Some(samples);
                    continue;
                }
            }
            if file["role"] == complete_role {
                complete_outputs.push(samples);
            }
        }
        assert_eq!(complete_outputs.len(), 2);
        assert_ne!(complete_outputs[0], complete_outputs[1]);
        if let Some(expected) = &complete_combination {
            assert_eq!(expected, &complete_outputs);
        } else {
            complete_combination = Some(complete_outputs);
        }
        assert!(output.join("README.md").is_file());
        assert!(output.join("SPEKTRAFILM_LICENSE.txt").is_file());
    }
}

#[test]
fn numeric_exposure_and_destination_failures_are_explicit() {
    let temp = tempfile::tempdir().unwrap();
    let output = temp.path().join("lut");
    let response = export(
        "1lut",
        &output,
        &[
            "--stops-above-midgray",
            "6",
            "--input-encoding",
            "vlog",
            "--output-encoding",
            "rec2100-pq",
        ],
    );
    assert!(
        response.status.success(),
        "{}",
        String::from_utf8_lossy(&response.stderr)
    );
    let metadata: Value =
        serde_json::from_slice(&fs::read(output.join("bundle.json")).unwrap()).unwrap();
    assert_eq!(metadata["color_spaces"]["output"]["name"], "Rec.2100 PQ");
    assert_eq!(metadata["input_exposure"]["stops_above_midgray"], 6.0);
    let expected_gain = 0.18 * 64.0 / 46.085_528;
    let actual = metadata["input_exposure"]["gain"].as_f64().unwrap();
    assert!((actual - expected_gain).abs() < 1e-8);

    let marker = output.join("keep");
    fs::write(&marker, b"existing bundle").unwrap();
    assert!(!export("2lut", &output, &[]).status.success());
    assert_eq!(fs::read(marker).unwrap(), b"existing bundle");

    let failed = temp.path().join("failed");
    assert!(
        !export("2lut", &failed, &["--stops-above-midgray", "nan"])
            .status
            .success()
    );
    assert!(!failed.exists());
}

#[test]
fn qa_can_select_a_single_paper_without_removing_the_other_paper_from_the_bundle() {
    let temp = tempfile::tempdir().unwrap();
    let output = temp.path().join("selected");
    let response = export("2lut", &output, &["--qa", "--qa-print-index", "1"]);
    assert!(
        response.status.success(),
        "{}",
        String::from_utf8_lossy(&response.stderr)
    );
    let read =
        |name| -> Value { serde_json::from_slice(&fs::read(output.join(name)).unwrap()).unwrap() };
    let bundle = read("bundle.json");
    assert_eq!(bundle["stocks"]["prints"].as_array().unwrap().len(), 2);
    let qa = read("qa.json");
    let prints = qa["prints"].as_object().unwrap();
    assert_eq!(prints.len(), 1);
    for kind in ["sequential", "resampled"] {
        let method = &prints["kodak_2383"][kind]["trilinear"];
        assert!(method["delta_e00_p95"].is_number());
        assert!(method["delta_itp_p99"].is_number());
    }

    let diagnostics = &prints["kodak_2383"]["diagnostics"];
    assert!(diagnostics["monotonicity"]["centerline_violations"].is_number());
    assert!(diagnostics["jacobian_condition"]["p95"].is_number());
    assert!(diagnostics["total_variation"]["axial_highband_ratio_mean"].is_number());
    assert_eq!(diagnostics["gamut_face_folds"]["triangles"], 96);
    assert!(
        diagnostics["noise_sensitivity"]["input_samples"]
            .as_u64()
            .unwrap()
            > 0
    );
    let charts = prints["kodak_2383"]["output_gamut_edge_stress"]
        .as_array()
        .unwrap();
    assert_eq!(charts.len(), 3);
    assert_eq!(charts[1]["target"], "DCI-P3");
    assert!(charts[1]["oog_fraction_saturated_row"].as_f64().unwrap() > 0.0);
    assert!(charts[1].get("image_png").is_none());
    let reports: Vec<_> = fs::read_dir(output.join("qa")).unwrap().collect();
    assert_eq!(reports.len(), 1);
    let html = fs::read_to_string(reports[0].as_ref().unwrap().path().join("report.html")).unwrap();
    assert!(html.contains("kodak_2383"));
    let qa_report = &prints["kodak_2383"];
    assert!(qa_report["dynamic_range_usage"]["midgray_offset_stops"].is_number());
    assert!(qa_report["hue_twist_oklab"]["max_hue_rotation_deg"].is_number());
    assert_eq!(qa_report["rg_plane_slices"]["n_slices"], 5);
    assert!(qa_report["characteristic_curve"]["system_gamma_at_mid"].is_number());
    assert_eq!(qa_report["planckian_sweep"]["cct_range_k"], "2700-10000");
    assert_eq!(qa_report["spectral_locus_envelope"]["cube_cells"], 125);
    assert!(qa_report["spectral_locus_envelope"]["near_locus_fraction"].is_number());
    assert_eq!(html.matches("data:image/png;base64,").count(), 9);
    assert!(html.contains("Noise sensitivity"));
    assert!(html.contains("Dynamic range usage"));
    assert!(html.contains("Red–green plane slices"));

    let invalid = temp.path().join("invalid-index");
    let rejected = export("2lut", &invalid, &["--qa", "--qa-print-index", "2"]);
    assert!(!rejected.status.success());
    assert!(String::from_utf8_lossy(&rejected.stderr).contains("QA print index 2 is out of range"));
    assert!(!invalid.exists());
}
