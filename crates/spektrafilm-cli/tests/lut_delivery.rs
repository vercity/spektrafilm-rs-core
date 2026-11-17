use std::{fs, io::Read, path::Path, process::Command};

fn bundle(directory: &Path, topology: &str, size: &str) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_spektrafilm"));
    command
        .args([
            "export-lut-bundle",
            "--film",
            "kodak_ektar_100",
            "--paper",
            "kodak_ultra_endura",
            "--paper",
            "kodak_portra_endura",
            "--input-encoding",
            "vlog",
            "--size",
            size,
            "--topology",
            topology,
            "--output",
        ])
        .arg(directory)
        .arg("--data-dir")
        .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../data"));
    command
}

fn success(command: &mut Command) {
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn zip_contains_canonical_combination_and_sidecar_files_and_ocio_uses_canonical_paths() {
    let temp = tempfile::tempdir().unwrap();
    let directory = temp.path().join("bundle");
    let zip_path = temp.path().join("bundle.zip");

    success(
        bundle(&directory, "4lut", "3")
            .args([
                "--combinations",
                "--ocio-config",
                "--qa",
                "--format",
                "3dl",
                "--zip",
            ])
            .arg(&zip_path),
    );

    let metadata: serde_json::Value =
        serde_json::from_slice(&fs::read(directory.join("bundle.json")).unwrap()).unwrap();
    let luts = metadata["luts"].as_array().unwrap();
    assert_eq!(luts.len(), 17);
    let config = fs::read_to_string(directory.join("config.ocio")).unwrap();
    assert!(config.contains("log_e_film_ektar100"));
    assert!(config.contains("log_e_print_ektar100_ultraendura"));
    assert!(config.contains("log_e_print_ektar100_portraendura"));
    assert!(!config.contains("combinations/"));
    let qa: serde_json::Value =
        serde_json::from_slice(&fs::read(directory.join("qa.json")).unwrap()).unwrap();
    assert_eq!(qa["samples_per_print"], 50_000);
    for paper in ["kodak_ultra_endura", "kodak_portra_endura"] {
        let report = &qa["prints"][paper];
        for method in ["sequential", "resampled"] {
            for interpolation in ["trilinear", "tetrahedral"] {
                assert!(report[method][interpolation]["delta_e00_max"].is_number());
            }
        }
        assert_ne!(report["sequential"], report["resampled"]);
    }
    for lut in luts {
        let path = lut["path"].as_str().unwrap();
        let sidecar = directory.join(path).with_extension("3dl");
        let data = fs::read_to_string(sidecar).unwrap();
        assert_eq!(data.lines().next().unwrap(), "0 512 1023");
        assert_eq!(data.lines().count(), 28);
        if !lut["role"].as_str().unwrap().starts_with("subchain_") {
            assert!(config.contains(path));
        }
    }

    let mut archive = zip::ZipArchive::new(fs::File::open(&zip_path).unwrap()).unwrap();
    assert_eq!(archive.len(), 41);
    for index in 0..archive.len() {
        let mut entry = archive.by_index(index).unwrap();
        let expected = fs::read(directory.join(entry.enclosed_name().unwrap())).unwrap();
        let mut bytes = Vec::new();
        entry.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, expected, "{}", entry.name());
    }

    let second_directory = temp.path().join("second");
    let response = bundle(&second_directory, "2lut", "3")
        .arg("--zip")
        .arg(&zip_path)
        .output()
        .unwrap();
    assert!(!response.status.success());
    assert!(!second_directory.exists());
}

#[test]
fn lumix_and_hald_preserve_device_header_and_readback_order() {
    let temp = tempfile::tempdir().unwrap();
    let directory = temp.path().join("camera");
    success(bundle(&directory, "1lut", "4").args([
        "--target",
        "lumix_realtime_vlog",
        "--output-encoding",
        "rec2020",
        "--format",
        "hald-png",
    ]));

    let metadata: serde_json::Value =
        serde_json::from_slice(&fs::read(directory.join("bundle.json")).unwrap()).unwrap();
    assert_eq!(metadata["target"], "lumix_realtime_vlog");
    let path = directory.join(metadata["luts"][0]["path"].as_str().unwrap());
    let text = fs::read_to_string(&path).unwrap();
    let mut lines = text.lines();
    assert!(lines.next().unwrap().starts_with("TITLE \""));
    assert_eq!(lines.next(), Some("#LUMIXPHOTOSTYLE VLOG"));
    assert_eq!(lines.next(), Some("LUT_3D_SIZE 4"));
    assert_eq!(lines.next(), Some("DOMAIN_MIN 0.000000 0.000000 0.000000"));
    assert_eq!(lines.next(), Some("DOMAIN_MAX 1.000000 1.000000 1.000000"));
    assert_eq!(lines.next(), Some(""));
    let expected: Vec<_> = lines
        .flat_map(str::split_whitespace)
        .map(|code| (code.parse::<f64>().unwrap() * 255.0).round_ties_even() as u8)
        .collect();
    let image = image::open(path.with_extension("png")).unwrap().to_rgb8();
    assert_eq!(image.dimensions(), (8, 8));
    assert_eq!(image.as_raw(), &expected);

    for (topology, size) in [("2lut", "4"), ("1lut", "5")] {
        let invalid = temp.path().join(format!("invalid-{topology}"));
        let result = bundle(&invalid, topology, size)
            .args(["--target", "lumix_realtime_vlog", "--format", "hald-png"])
            .output()
            .unwrap();
        assert!(!result.status.success());
        assert!(!invalid.exists());
    }
}
