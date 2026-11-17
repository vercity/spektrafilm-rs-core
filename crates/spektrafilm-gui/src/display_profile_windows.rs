use std::{
    ffi::OsString,
    fs,
    os::windows::ffi::OsStringExt,
    path::PathBuf,
    ptr,
    time::{Duration, Instant, SystemTime},
};

use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use spektrafilm_io::display_icc::DisplayIcc;
use windows_sys::Win32::{
    Foundation::{HWND, RECT},
    Graphics::Gdi::{
        CreateDCW, DeleteDC, GetMonitorInfoW, HDC, MONITOR_DEFAULTTONEAREST, MONITORINFO,
        MONITORINFOEXW, MonitorFromWindow,
    },
    UI::ColorSystem::GetICMProfileW,
};

const REFRESH_INTERVAL: Duration = Duration::from_secs(1);

#[derive(Eq, PartialEq)]
struct ProfileFile {
    path: PathBuf,
    modification: Option<SystemTime>,
    bytes: u64,
}

#[derive(Default)]
pub(super) struct DisplayProfile {
    last_check: Option<Instant>,
    file: Option<ProfileFile>,
    conversion: Option<DisplayIcc>,
}

impl DisplayProfile {
    pub fn refresh(&mut self, ctx: &eframe::egui::Context, window: &impl HasWindowHandle) -> bool {
        let now = Instant::now();
        if let Some(last_check) = self.last_check {
            let remaining = REFRESH_INTERVAL.saturating_sub(now - last_check);
            if !remaining.is_zero() {
                ctx.request_repaint_after(remaining);
                return false;
            }
        }
        self.last_check = Some(now);
        ctx.request_repaint_after(REFRESH_INTERVAL);

        let file = profile_path(window).and_then(|path| {
            let metadata = fs::metadata(&path).ok()?;
            Some(ProfileFile {
                path,
                modification: metadata.modified().ok(),
                bytes: metadata.len(),
            })
        });
        if self.file == file {
            return false;
        }
        let was_available = self.available();
        self.conversion = file.as_ref().and_then(|file| match fs::read(&file.path) {
            Ok(profile) => match DisplayIcc::from_bytes(&profile) {
                Ok(conversion) => Some(conversion),
                Err(error) => {
                    tracing::warn!(%error, path = %file.path.display(), "could not load monitor ICC");
                    None
                }
            },
            Err(error) => {
                tracing::warn!(%error, path = %file.path.display(), "could not read monitor ICC");
                None
            }
        });
        self.file = file;
        was_available || self.available()
    }

    pub fn available(&self) -> bool {
        self.conversion.is_some()
    }

    pub fn convert_rgba(&self, rgba: &[u8]) -> Option<Vec<u8>> {
        match self.conversion.as_ref()?.transform_rgba(rgba) {
            Ok(converted) => Some(converted),
            Err(error) => {
                tracing::warn!(%error, "could not convert displayed pixels");
                None
            }
        }
    }
}

struct MonitorDc(HDC);

impl Drop for MonitorDc {
    fn drop(&mut self) {
        // SAFETY: This non-null DC was created on this thread and is owned here.
        unsafe { DeleteDC(self.0) };
    }
}

fn profile_path(window: &impl HasWindowHandle) -> Option<PathBuf> {
    let window = window.window_handle().ok()?;
    let RawWindowHandle::Win32(handle) = window.as_raw() else {
        return None;
    };
    let hwnd = handle.hwnd.get() as HWND;
    // SAFETY: The HWND is valid for the lifetime of the borrowed window handle.
    let monitor = unsafe { MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST) };
    if monitor.is_null() {
        return None;
    }
    let empty = RECT {
        left: 0,
        top: 0,
        right: 0,
        bottom: 0,
    };
    let mut info = MONITORINFOEXW {
        monitorInfo: MONITORINFO {
            cbSize: size_of::<MONITORINFOEXW>() as u32,
            rcMonitor: empty,
            rcWork: empty,
            dwFlags: 0,
        },
        szDevice: [0; 32],
    };
    // SAFETY: monitor is non-null and info is the writable MONITORINFOEXW
    // allocation identified by the embedded base structure's cbSize.
    if unsafe { GetMonitorInfoW(monitor, ptr::from_mut(&mut info).cast()) } == 0 {
        return None;
    }
    if !info.szDevice.contains(&0) || info.szDevice[0] == 0 {
        return None;
    }
    let driver = [b'D', b'I', b'S', b'P', b'L', b'A', b'Y', 0].map(u16::from);
    // SAFETY: Both supplied UTF-16 strings are NUL terminated. The port and
    // DEVMODE are intentionally null to select the current monitor settings.
    let dc = unsafe {
        CreateDCW(
            driver.as_ptr(),
            info.szDevice.as_ptr(),
            ptr::null(),
            ptr::null(),
        )
    };
    if dc.is_null() {
        return None;
    }
    let dc = MonitorDc(dc);

    let mut count = 0;
    // SAFETY: The driver fills count only when the filename pointer is null.
    if unsafe { GetICMProfileW(dc.0, &mut count, ptr::null_mut()) } == 0 || count <= 1 {
        return None;
    }
    let mut name = vec![0u16; count as usize];
    // SAFETY: name has exactly count writable UTF-16 code units, and the owned
    // DC remains valid for both calls.
    if unsafe { GetICMProfileW(dc.0, &mut count, name.as_mut_ptr()) } == 0 {
        return None;
    }
    let end = name.iter().position(|&c| c == 0)?;
    (end > 0).then(|| PathBuf::from(OsString::from_wide(&name[..end])))
}
