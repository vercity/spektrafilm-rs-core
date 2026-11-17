use std::{
    ffi::{c_int, c_void},
    path::Path,
    ptr::{self, NonNull},
};

use anyhow::{Context, Result, bail, ensure};
use spektrafilm_math::{image::ImageBuf, precision::from_f32};

#[repr(C)]
struct DevelopedImage {
    handle: *mut c_void,
    samples: *const u8,
    length: usize,
    width: u32,
    height: u32,
}

unsafe extern "C" {
    fn spektrafilm_develop_raw(
        file: *mut u8,
        length: usize,
        camera_wb: c_int,
        output: *mut DevelopedImage,
    ) -> c_int;
    fn spektrafilm_free_raw(image: *mut c_void);
}

struct Allocation(NonNull<c_void>);

impl Drop for Allocation {
    fn drop(&mut self) {
        // SAFETY: This is the exclusively owned handle from the bridge and is
        // released exactly once after the output bytes have been copied.
        unsafe { spektrafilm_free_raw(self.0.as_ptr()) };
    }
}

pub(super) fn develop(path: &Path, as_shot: bool) -> Result<Option<ImageBuf>> {
    let mut file =
        std::fs::read(path).with_context(|| format!("reading RAW image {}", path.display()))?;
    ensure!(!file.is_empty(), "RAW image {} is empty", path.display());
    let mut output = DevelopedImage {
        handle: ptr::null_mut(),
        samples: ptr::null(),
        length: 0,
        width: 0,
        height: 0,
    };

    // SAFETY: The bridge receives a live, exclusively owned input buffer and
    // output struct. It finishes reading the input before returning and the
    // only allocation transferred on success is the processed image handle.
    let status = unsafe {
        spektrafilm_develop_raw(
            file.as_mut_ptr(),
            file.len(),
            c_int::from(as_shot),
            &mut output,
        )
    };
    match status {
        0 => {}
        1 => return Ok(None),
        -10001 => bail!("LibRaw binary and header versions differ"),
        -10002 => bail!("LibRaw did not return a linear 16-bit RGB image"),
        status => bail!("LibRaw error {status} while developing {}", path.display()),
    }
    let _allocation = Allocation(NonNull::new(output.handle).context("missing LibRaw image")?);
    let expected = (output.width as usize)
        .checked_mul(output.height as usize)
        .and_then(|n| n.checked_mul(6));
    ensure!(
        !output.samples.is_null()
            && output.length > 0
            && output.length <= isize::MAX as usize
            && expected == Some(output.length),
        "invalid LibRaw RGB dimensions"
    );
    // SAFETY: The bridge verified that this many initialized bytes form the
    // interleaved RGB image. The guard above keeps the allocation alive and we
    // only borrow it immutably here.
    let bytes = unsafe { std::slice::from_raw_parts(output.samples, output.length) };
    let data = bytes
        .chunks_exact(2)
        .map(|pair| from_f32(f32::from(u16::from_ne_bytes([pair[0], pair[1]])) / 65535.0))
        .collect();
    Ok(Some(ImageBuf::from_data(output.width, output.height, data)))
}
