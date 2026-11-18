use std::{
    ffi::{CString, c_char, c_int, c_void},
    path::Path,
    ptr::NonNull,
};

use anyhow::{Context, Result, ensure};

unsafe extern "C" {
    fn spektrafilm_read_maker_note(path: *const c_char) -> *mut c_void;
    fn spektrafilm_write_maker_note(path: *const c_char, note: *const c_void) -> c_int;
    fn spektrafilm_free_maker_note(note: *mut c_void);
}

#[derive(Debug)]
pub(super) struct MakerNote(NonNull<c_void>);

// SAFETY: The owned ExifData has no thread affinity. The bridge clones the
// captured data into each destination under a mutex, and Drop needs ownership.
unsafe impl Send for MakerNote {}
// SAFETY: Sharing MakerNote only exposes immutable, serialized native reads.
unsafe impl Sync for MakerNote {}

impl MakerNote {
    pub fn read(path: &Path) -> Option<Self> {
        let path = path.canonicalize().ok()?;
        let path = CString::new(path.as_os_str().as_encoded_bytes()).ok()?;
        // SAFETY: The string is null-terminated and lives for the entire call.
        // A non-null result is an exclusively owned ExifData allocated by the bridge.
        NonNull::new(unsafe { spektrafilm_read_maker_note(path.as_ptr()) }).map(Self)
    }

    pub fn write(&self, path: &Path) -> Result<()> {
        let path = path
            .canonicalize()
            .context("resolve the encoded output path")?;
        let path =
            CString::new(path.as_os_str().as_encoded_bytes()).context("invalid output path")?;
        // SAFETY: The path and captured note outlive the call. The bridge does
        // not retain the output path or modify the source note.
        let status = unsafe { spektrafilm_write_maker_note(path.as_ptr(), self.0.as_ptr()) };
        ensure!(status == 0, "Exiv2 could not write the camera MakerNote");
        Ok(())
    }
}

impl Drop for MakerNote {
    fn drop(&mut self) {
        // SAFETY: The matching native deallocator releases this owned note once.
        unsafe { spektrafilm_free_maker_note(self.0.as_ptr()) };
    }
}
