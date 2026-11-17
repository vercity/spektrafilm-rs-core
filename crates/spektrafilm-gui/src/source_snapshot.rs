use std::fs::File;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use tempfile::{Builder, TempDir};

/// Original encoded source shared by preview, re-development, and export.
/// For RAW export it supplies metadata for the already developed pixels.
pub(super) struct SourceSnapshot {
    original_path: PathBuf,
    path: PathBuf,
    _directory: TempDir,
}

impl SourceSnapshot {
    pub(super) fn capture(original_path: &Path) -> Result<Self> {
        let name = original_path
            .file_name()
            .context("source has no filename")?;
        let mut input = File::open(original_path).context("opening source image")?;
        ensure!(
            input.metadata()?.is_file(),
            "source image is not a regular file"
        );

        let mut builder = Builder::new();
        builder.prefix("spektrafilm-source-");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            builder.permissions(std::fs::Permissions::from_mode(0o700));
        }
        let directory = builder
            .tempdir()
            .context("creating source snapshot directory")?;
        let path = directory.path().join(name);
        {
            let mut encoded = File::create_new(&path).context("creating source snapshot")?;
            std::io::copy(&mut input, &mut encoded).context("copying source image for export")?;
        }
        Ok(Self {
            original_path: original_path.to_path_buf(),
            path,
            _directory: directory,
        })
    }

    pub(super) fn original_path(&self) -> &Path {
        &self.original_path
    }

    pub(super) fn path(&self) -> &Path {
        &self.path
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::App;
    use spektrafilm_gpu::cpu_backend::CpuBackend;
    use std::fs;
    use std::sync::Arc;

    fn write_png(path: &Path, red: u8) {
        image::RgbImage::from_pixel(2, 1, image::Rgb([red, 0, 0]))
            .save(path)
            .unwrap();
    }

    #[test]
    fn opened_source_survives_rename_delete_and_replacement_until_reopened() {
        let directory = tempfile::tempdir().unwrap();
        let original = directory.path().join("original.png");
        let renamed = directory.path().join("renamed.png");
        write_png(&original, 32);
        let encoded_a = fs::read(&original).unwrap();
        let mut app = App::new(Arc::new(CpuBackend), Some(original.clone()));
        let snapshot = app.source.as_ref().unwrap().path().to_path_buf();
        assert_eq!(snapshot.file_name(), original.file_name());

        fs::rename(&original, &renamed).unwrap();
        assert_eq!(fs::read(&snapshot).unwrap(), encoded_a);
        fs::remove_file(&renamed).unwrap();
        assert_eq!(fs::read(&snapshot).unwrap(), encoded_a);
        write_png(&original, 224);
        let encoded_b = fs::read(&original).unwrap();
        assert_ne!(encoded_b, encoded_a);
        assert_eq!(fs::read(&snapshot).unwrap(), encoded_a);

        app.load_image_from_path(&original);
        let reopened = app.source.as_ref().unwrap();
        assert_eq!(reopened.original_path(), original);
        assert_eq!(fs::read(reopened.path()).unwrap(), encoded_b);
        assert!(!snapshot.exists());
    }

    #[test]
    fn failed_capture_or_decode_keeps_the_previous_source() {
        let directory = tempfile::tempdir().unwrap();
        let original = directory.path().join("original.png");
        write_png(&original, 32);
        let mut app = App::new(Arc::new(CpuBackend), Some(original));
        let previous_source = app.source.as_ref().unwrap().clone();
        let previous_image = app.image.as_ref().unwrap().clone();
        let generation = app.render_generation;

        let invalid = directory.path().join("invalid.png");
        for contents in [None, Some(b"invalid PNG")] {
            if let Some(contents) = contents {
                fs::write(&invalid, contents).unwrap();
            }
            app.load_image_from_path(&invalid);
            assert!(app.status.starts_with("Load error:"));
            assert!(app.status.contains("invalid.png"));
            assert!(Arc::ptr_eq(app.source.as_ref().unwrap(), &previous_source));
            assert!(Arc::ptr_eq(app.image.as_ref().unwrap(), &previous_image));
            assert_eq!(app.render_generation, generation);
        }
    }

    #[cfg(unix)]
    #[test]
    fn raw_export_hands_off_the_rotated_pixels_without_a_second_raw_develop() {
        use std::{os::unix::fs::PermissionsExt, time::Duration};

        let directory = tempfile::tempdir().unwrap();
        let original = directory.path().join("camera.dng");
        let child = directory.path().join("export.sh");
        let output = directory.path().join("developed.tiff");
        fs::write(&original, "dummy original RAW metadata input").unwrap();
        fs::write(
            &child,
            r#"#!/bin/sh
set -eu
test "${2##*.}" = dng
target=$4
while [ "$#" -gt 0 ]; do
    case "$1" in
        --rotate-quarter-turns) exit 31 ;;
        --developed-raw) cp "$2" "$target"; exit ;;
    esac
    shift
done
exit 32
"#,
        )
        .unwrap();
        fs::set_permissions(&child, fs::Permissions::from_mode(0o700)).unwrap();
        let source = Arc::new(SourceSnapshot::capture(&original).unwrap());
        let mut app = App::new(Arc::new(CpuBackend), None);
        app.source = Some(Arc::clone(&source));
        app.image = Some(Arc::new(spektrafilm_math::image::ImageBuf::from_data(
            2,
            3,
            vec![
                0.1, 0.2, 0.3, 0.4, 0.5, 0.6, 0.7, 0.8, 0.9, 1.1, 1.2, 1.3, 1.4, 1.5, 1.6, -0.1,
                -0.2, -0.3,
            ],
        )));
        app.rotate_input(1);
        let image_at_export = app.image.clone().unwrap();

        let ctx = eframe::egui::Context::default();
        app.start_f64_export(&ctx, source, child, output.clone());
        app.rotate_input(1);
        let job = app.export_job.as_mut().unwrap();
        job.rx
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .unwrap();
        job.handle.take().unwrap().join().unwrap();
        let developed = spektrafilm_io::read_developed_raw(&output).unwrap();
        assert_eq!((developed.width, developed.height), (3, 2));
        assert_eq!(developed.data, image_at_export.data);
    }

    #[cfg(unix)]
    #[test]
    fn export_worker_retains_the_source_and_settings_at_dispatch() {
        use std::os::unix::fs::PermissionsExt;
        use std::time::Duration;

        use spektrafilm_core::params::RuntimeParams;

        let directory = tempfile::tempdir().unwrap();
        let original = directory.path().join("original.png");
        let output = directory.path().join("output.png");
        let child = directory.path().join("export.sh");
        write_png(&original, 32);
        let encoded_a = fs::read(&original).unwrap();
        fs::write(
            &child,
            r#"#!/bin/sh
set -eu
directory=${0%/*}
while [ ! -f "$directory/continue" ]; do
    [ -d "$directory" ] || exit 1
    sleep 0.01
done
printf '%s\n%s\n' "$6" "$8" > "$directory/stocks.txt"
cp "${10}" "$directory/params.json"
cp "$2" "$4"
"#,
        )
        .unwrap();
        fs::set_permissions(&child, fs::Permissions::from_mode(0o700)).unwrap();

        let ctx = eframe::egui::Context::default();
        let mut app = App::new(Arc::new(CpuBackend), Some(original.clone()));
        app.film_name = "film-A".into();
        app.print_name = "paper-A".into();
        app.params.camera.exposure_compensation_ev = 1.25;
        let source = app.source.as_ref().unwrap().clone();
        let first_snapshot = source.path().to_path_buf();
        app.start_f64_export(&ctx, source, child, output.clone());

        write_png(&original, 224);
        app.load_image_from_path(&original);
        app.film_name = "film-B".into();
        app.print_name = "paper-B".into();
        app.params.camera.exposure_compensation_ev = 2.5;
        assert!(first_snapshot.exists());
        fs::write(directory.path().join("continue"), b"").unwrap();

        let job = app.export_job.as_mut().unwrap();
        job.rx
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .unwrap();
        job.handle.take().unwrap().join().unwrap();
        assert_eq!(fs::read(&output).unwrap(), encoded_a);
        assert_eq!(
            fs::read_to_string(directory.path().join("stocks.txt")).unwrap(),
            "film-A\npaper-A\n"
        );
        let params: RuntimeParams =
            serde_json::from_reader(File::open(directory.path().join("params.json")).unwrap())
                .unwrap();
        assert_eq!(params.camera.exposure_compensation_ev, 1.25);
        assert!(!first_snapshot.exists());
    }
}
