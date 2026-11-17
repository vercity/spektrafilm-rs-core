use std::fs::File;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use tempfile::{Builder, TempDir};

/// Encoded source shared by preview and export. The f64 child needs the
/// original file format, pixel values and metadata.
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
