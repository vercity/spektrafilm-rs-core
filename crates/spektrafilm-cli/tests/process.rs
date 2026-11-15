use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::json;

const SLIDE: &str = "fujifilm_provia_100f";

struct ProcessFixture(PathBuf);

impl ProcessFixture {
    fn new() -> Self {
        let stamp = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
        let dir = std::env::temp_dir().join(format!(
            "spektrafilm-cli-process-{}-{}",
            std::process::id(),
            stamp.as_nanos()
        ));
        fs::create_dir(&dir).unwrap();
        let fixture = Self(dir);

        // This positive film has no target_print. Copy only the profile so
        // Hanatos initialization fails predictably when requested.
        let profiles = fixture.0.join("profiles");
        fs::create_dir(&profiles).unwrap();
        let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../data/profiles");
        let name = format!("{SLIDE}.json");
        fs::copy(source.join(&name), profiles.join(&name)).unwrap();

        image::RgbImage::from_pixel(2, 2, image::Rgb([128, 96, 64]))
            .save(fixture.0.join("input.png"))
            .unwrap();
        fixture
    }

    fn process(&self, scan_film_in_json: bool, method: &str, flags: &[&str]) -> Output {
        let params = json!({
            "io": { "scan_film": scan_film_in_json },
            "settings": { "rgb_to_raw_method": method },
            "camera": { "auto_exposure": false },
            "scanner": { "unsharp_mask": [0.0, 0.0] },
            "film_render": {
                "dir_couplers": { "active": false },
                "halation": { "active": false },
                "grain": { "active": false },
                "glare": { "active": false },
            },
        });
        fs::write(self.0.join("params.json"), params.to_string()).unwrap();

        Command::new(env!("CARGO_BIN_EXE_spektrafilm"))
            .current_dir(&self.0)
            .env("SPEKTRAFILM_BACKEND", "cpu")
            .args([
                "process",
                "input.png",
                "--output",
                "output.png",
                "--raw-out",
                "pixels.bin",
                "--film",
                SLIDE,
                "--params",
                "params.json",
                "--data-dir",
                ".",
            ])
            .args(flags)
            .output()
            .unwrap()
    }
}

impl Drop for ProcessFixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn scan_mode_is_resolved_before_requiring_a_paper() {
    let fixture = ProcessFixture::new();

    let print = fixture.process(false, "mallett2019", &[]);
    assert!(!print.status.success());
    assert!(String::from_utf8_lossy(&print.stderr).contains("film has no target_print"));

    let json_scan = fixture.process(true, "mallett2019", &[]);
    assert!(json_scan.status.success(), "{json_scan:?}");
    assert!(fixture.0.join("output.png").is_file());
    let json_pixels = fs::read(fixture.0.join("pixels.bin")).unwrap();
    assert_eq!(json_pixels.len(), 2 * 2 * 3 * size_of::<f64>());

    let flag_scan = fixture.process(false, "mallett2019", &["--scan-film"]);
    assert!(flag_scan.status.success(), "{flag_scan:?}");
    assert_eq!(json_pixels, fs::read(fixture.0.join("pixels.bin")).unwrap());
}

#[test]
fn spectral_initialization_failure_returns_an_error_without_outputs() {
    let fixture = ProcessFixture::new();
    for (method, cause) in [
        ("hanatos2025", "irradiance_xy_tc.npy"),
        ("not-an-upsampler", "unsupported rgb_to_raw_method"),
    ] {
        let result = fixture.process(true, method, &[]);
        let stderr = String::from_utf8_lossy(&result.stderr);
        assert!(!result.status.success(), "{method} unexpectedly succeeded");
        assert!(
            stderr.contains("initializing spectral pipeline"),
            "{stderr}"
        );
        assert!(stderr.contains(cause), "{stderr}");
        assert!(!fixture.0.join("output.png").exists());
        assert!(!fixture.0.join("pixels.bin").exists());
    }
}
