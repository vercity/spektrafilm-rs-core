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

#[test]
fn calibrated_srgb_cube_matches_decoded_photo_samples_in_red_fastest_order() {
    use std::fs::File;
    use tiff::encoder::{TiffEncoder, colortype::RGB32Float};

    let fixture = ProcessFixture::new();
    let data_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../data");
    let command = || {
        let mut command = Command::new(env!("CARGO_BIN_EXE_spektrafilm"));
        command.current_dir(&fixture.0);
        command.env("SPEKTRAFILM_BACKEND", "cpu");
        command
    };
    let invalid = command()
        .args([
            "export-lut",
            "--film",
            "kodak_gold_200",
            "--size",
            "1",
            "-o",
            "invalid.cube",
        ])
        .output()
        .unwrap();
    assert!(!invalid.status.success());
    assert!(!fixture.0.join("invalid.cube").exists());

    let profiles = ["--film", "kodak_gold_200", "--paper", "kodak_portra_endura"];
    let cube = command()
        .args([
            "export-lut",
            "--input-encoding",
            "srgb",
            "--size",
            "3",
            "-o",
            "gold.cube",
        ])
        .args(profiles)
        .arg("--data-dir")
        .arg(&data_dir)
        .output()
        .unwrap();
    assert!(cube.status.success(), "{cube:?}");

    let table = fs::read_to_string(fixture.0.join("gold.cube")).unwrap();
    assert!(table.contains("LUT_3D_SIZE 3\n"));
    let samples: Vec<f64> = table
        .split_once("DOMAIN_MAX 1.0 1.0 1.0\n")
        .expect("normalized cube domain")
        .1
        .split_whitespace()
        .map(|sample| sample.parse().unwrap())
        .collect();
    assert_eq!(samples.len(), 27 * 3);
    assert!(samples.iter().all(|sample| (0.0..=1.0).contains(sample)));

    // Run a linear TIFF through the ordinary photo entry point. Reverse
    // the grid to check that the transform does not depend on pixel order.
    let linear_grid = [1.0_f32, 0.21404114, 0.0];
    let mut pixels = Vec::with_capacity(27 * 3);
    for b in linear_grid {
        for g in linear_grid {
            for r in linear_grid {
                pixels.extend([r, g, b]);
            }
        }
    }
    TiffEncoder::new(File::create(fixture.0.join("grid.tiff")).unwrap())
        .unwrap()
        .write_image::<RGB32Float>(9, 3, &pixels)
        .unwrap();

    let params = json!({
        "io": {"input_color_space": "sRGB", "input_cctf_decoding": false},
        "camera": {"auto_exposure": false},
        "enlarger": {"print_exposure_compensation": false},
        "film_render": {
            "halation": {"active": false, "boost_ev": 0.0},
            "dir_couplers": {"diffusion_size_um": 0.0, "diffusion_tail_um": 0.0},
            "grain": {"active": false},
        },
        "print_render": {"glare": {"active": false}},
        "scanner": {"unsharp_mask": [0.0, 0.0]},
    });
    fs::write(fixture.0.join("params.json"), params.to_string()).unwrap();

    let photo = command()
        .args([
            "process",
            "grid.tiff",
            "-o",
            "grid.png",
            "--raw-out",
            "grid.bin",
            "--params",
            "params.json",
        ])
        .args(profiles)
        .arg("--data-dir")
        .arg(data_dir)
        .output()
        .unwrap();
    assert!(photo.status.success(), "{photo:?}");
    let bytes = fs::read(fixture.0.join("grid.bin")).unwrap();
    let photo_samples: Vec<f64> = bytes
        .chunks_exact(8)
        .map(|chunk| f64::from_ne_bytes(chunk.try_into().unwrap()))
        .collect();
    assert_eq!(photo_samples.len(), samples.len());
    for (actual, expected) in samples
        .chunks_exact(3)
        .zip(photo_samples.chunks_exact(3).rev())
    {
        for (actual, expected) in actual.iter().zip(expected) {
            assert!(
                (actual - expected.clamp(0.0, 1.0)).abs() < 2e-6,
                "{actual} != {expected}"
            );
        }
    }
}
