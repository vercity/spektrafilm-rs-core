use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use spektrafilm_io::raster::{RasterWriteOptions, write_raster};
use spektrafilm_math::image::ImageBuf;

/// An output private to the current save or export until it is ready to publish.
pub(crate) struct StagedOutput<'a> {
    destination: &'a Path,
    path: PathBuf,
    _directory: tempfile::TempDir,
}

impl<'a> StagedOutput<'a> {
    pub(crate) fn new(destination: &'a Path) -> Result<Self> {
        let filename = destination
            .file_name()
            .with_context(|| format!("missing output filename in {}", destination.display()))?;
        let parent = destination
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        let directory =
            temporary_export_directory(parent, ".spektrafilm-output-").with_context(|| {
                format!("creating temporary output beside {}", destination.display())
            })?;
        // Keep the extension for the writer and let it create its usual file mode.
        let path = directory.path().join(filename);
        Ok(Self {
            destination,
            path,
            _directory: directory,
        })
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// Publish after the writer has closed the file. The directory is on the
    /// same filesystem, so rename replaces an existing file in one operation.
    pub(crate) fn publish(self) -> Result<()> {
        std::fs::rename(&self.path, self.destination)
            .with_context(|| format!("publishing output to {}", self.destination.display()))
    }
}

pub(crate) fn temporary_export_directory(
    parent: &Path,
    prefix: &str,
) -> std::io::Result<tempfile::TempDir> {
    let mut builder = tempfile::Builder::new();
    builder.prefix(prefix);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        builder.permissions(std::fs::Permissions::from_mode(0o700));
    }
    builder.tempdir_in(parent)
}

pub(crate) fn write_preview_output(
    destination: &Path,
    image: &ImageBuf,
    options: &RasterWriteOptions<'_>,
) -> Result<()> {
    let staged = StagedOutput::new(destination)?;
    write_raster(staged.path(), image, options)
        .with_context(|| format!("encoding output for {}", destination.display()))?;
    staged.publish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use spektrafilm_io::raster::{RasterColor, read_raster};
    use spektrafilm_math::precision::from_f32;
    use std::fs;

    #[test]
    fn preview_save_replaces_existing_tiff_after_encoding() {
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("saved frame.TIFF");
        fs::write(&destination, b"previous contents").unwrap();
        let samples = [-0.125, 0.25, 1.5].map(from_f32);
        let preview = ImageBuf::from_data(1, 1, samples.to_vec());
        write_preview_output(
            &destination,
            &preview,
            &RasterWriteOptions {
                bit_depth: Some(32),
                color_space: Some("ACES2065-1"),
                ..Default::default()
            },
        )
        .unwrap();

        let saved = read_raster(&destination).unwrap();
        assert_eq!(saved.image.data, samples);
        assert_eq!(saved.color, RasterColor::new("ACES2065-1", false));
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 1);
    }

    #[test]
    fn preview_encoding_failure_preserves_existing_and_absent_targets() {
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("failed.tiff");
        let preview = ImageBuf::new(1, 1);
        let options = RasterWriteOptions {
            bit_depth: Some(9),
            ..Default::default()
        };

        let error = write_preview_output(&destination, &preview, &options).unwrap_err();
        assert!(format!("{error:#}").contains("9-bit tiff output is unsupported"));
        assert!(!destination.exists());
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 0);

        fs::write(&destination, b"previous contents").unwrap();
        assert!(write_preview_output(&destination, &preview, &options).is_err());
        assert_eq!(fs::read(&destination).unwrap(), b"previous contents");
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 1);
    }
}
