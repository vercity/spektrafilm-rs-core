use std::fs::{self, File};
use std::process::{Command, Output};

use serde_json::Value;

fn lut() -> Command {
    Command::new(env!("CARGO_BIN_EXE_spektrafilm-lut"))
}

fn stdout(output: Output) -> String {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

#[test]
fn registry_list_prints_public_roles_and_canonical_short_tags() {
    let input = stdout(lut().args(["list", "input"]).output().unwrap());
    let output = stdout(lut().args(["list", "output"]).output().unwrap());
    assert_eq!(input.lines().count(), 26);
    assert_eq!(output.lines().count(), 10);
    assert!(input.lines().any(|line| line.ends_with("  applelog")));
    assert!(!output.lines().any(|line| line.ends_with("  applelog")));
    assert!(output.lines().any(|line| line.ends_with("  rec2100hlg")));
    assert_eq!(
        stdout(lut().args(["list", "target"]).output().unwrap()),
        "lumix_realtime_vlog\n"
    );
    let film = stdout(lut().args(["list", "film"]).output().unwrap());
    let paper = stdout(lut().args(["list", "print"]).output().unwrap());
    assert!(film.lines().any(|stock| stock == "kodak_ektar_100"));
    assert!(paper.lines().any(|stock| stock == "kodak_ultra_endura"));
}

#[test]
fn toml_can_set_gamut_and_delivery_while_cli_replaces_prints() {
    let temp = tempfile::tempdir().unwrap();
    let recipe = temp.path().join("base.toml");
    fs::write(
        &recipe,
        r#"
name = "base"
film_profile = "kodak_ektar_100"
print_profiles = ["kodak_ultra_endura"]
input_color_space = "vlog"
output_color_space = "Rec.709"
topology = "2lut"
resolution = 4
container = "zip"
stops_above_midgray = 6
include_combinations = true
qa = true
qa_print_index = 0

[input_gamut_compress]
active = false
algorithm = "oklch"
knee = [0.2, 1.0, 4.0]

[output_gamut_compress]
algorithm = "jzazbz"
knee = [0.3, 1.0, 5.0]
lightness_compression = [0.75, 1.0, 2.5]
"#,
    )
    .unwrap();
    let out = temp.path().join("new").join("nested");
    let result = lut()
        .args(["build", "--from"])
        .arg(&recipe)
        .args([
            "--name",
            "override",
            "--print",
            "kodak_2383",
            "--print",
            "kodak_portra_endura",
            "--resolution",
            "3",
            "--qa-print-index",
            "1",
            "--ocio-config",
            "--out",
        ])
        .arg(&out)
        .output()
        .unwrap();
    assert!(stdout(result).contains("override.zip"));
    let bundle = out.join("override");
    let metadata: Value =
        serde_json::from_reader(File::open(bundle.join("bundle.json")).unwrap()).unwrap();
    assert_eq!(metadata["name"], "override");
    assert_eq!(metadata["resolution"], 3);
    assert_eq!(metadata["input_exposure"]["stops_above_midgray"], 6.0);
    assert_eq!(
        metadata["stocks"]["prints"],
        serde_json::json!(["kodak_2383", "kodak_portra_endura"])
    );
    let profile = &metadata["params_snapshot"]["kodak_2383"];
    assert_eq!(profile["input_gamut_compress"]["active"], false);
    assert_eq!(profile["output_gamut_compress"]["algorithm"], "jzazbz");
    assert_eq!(
        profile["output_gamut_compress"]["lightness_compression"][0],
        0.75
    );
    let qa: Value = serde_json::from_reader(File::open(bundle.join("qa.json")).unwrap()).unwrap();
    assert_eq!(qa["prints"].as_object().unwrap().len(), 1);
    assert!(qa["prints"]["kodak_portra_endura"].is_object());
    assert!(bundle.join("combinations").is_dir());
    let archive = zip::ZipArchive::new(File::open(out.join("override.zip")).unwrap()).unwrap();
    assert!(
        archive
            .file_names()
            .any(|name| name == "override/config.ocio")
    );
    assert!(archive.file_names().any(|name| name == "override/qa.json"));
}

#[test]
fn bare_build_uses_python_default_topology_and_versioned_name() {
    let temp = tempfile::tempdir().unwrap();
    stdout(
        lut()
            .args([
                "build",
                "--film",
                "kodak_ektar_100",
                "--print",
                "kodak_ultra_endura",
                "--input",
                "Apple Log",
                "--output",
                "srgb",
                "--resolution",
                "2",
                "--out",
            ])
            .arg(temp.path())
            .output()
            .unwrap(),
    );
    let directory = temp
        .path()
        .join("spektrafilm_v034_ektar100_ultraendura_1lut_applelog_srgb");
    let cube = fs::read_to_string(directory.join("lut_v034_ektar100_ultraendura.cube")).unwrap();
    assert!(cube.contains("TITLE \"v034_ektar100_ultraendura\""));
}
