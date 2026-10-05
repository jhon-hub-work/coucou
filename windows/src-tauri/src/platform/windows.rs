// Windows: Win32 for the island window and the cursor, %APPDATA% for files.

use std::os::windows::process::CommandExt;
use std::path::PathBuf;
use std::process::Command;

use tauri::WebviewWindow;

use ::windows::core::PWSTR;
use ::windows::Win32::Foundation::{CloseHandle, HANDLE, HLOCAL, HWND, LocalFree, POINT};
use ::windows::Win32::Security::Authorization::ConvertSidToStringSidW;
use ::windows::Win32::Security::{GetTokenInformation, TokenUser, TOKEN_QUERY, TOKEN_USER};
use ::windows::Win32::System::SystemInformation::GetLocalTime;
use ::windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
use ::windows::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_LBUTTON};
use ::windows::Win32::UI::WindowsAndMessaging::{
    GetCursorPos, GetWindowLongPtrW, SetWindowLongPtrW,
    GWL_EXSTYLE, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW,
};

use super::LocalTime;

/// File name of the Claude Code relay.
pub const HOOK_EXE: &str = "boo-hook.exe";

/// Environment variable holding the home directory.
pub const HOME_VAR: &str = "USERPROFILE";

/// Keeps spawned helpers from flashing a console window.
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

// ── Files ─────────────────────────────────────────────────────────────────────

/// %APPDATA%\Boo — preferences.
pub fn config_dir() -> PathBuf {
    let base = std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("Boo")
}

/// %LOCALAPPDATA%\Boo — where boo-hook.exe, the inbox and the log live.
pub fn local_dir() -> PathBuf {
    let base = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("Boo")
}

/// %APPDATA% and %LOCALAPPDATA% are already private to the user.
pub fn ensure_private_dir(dir: &std::path::Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)
}

/// Nothing to set up before the webview starts.
pub fn prepare_environment() {}

pub fn local_time() -> LocalTime {
    let t = unsafe { GetLocalTime() };
    LocalTime {
        year: t.wYear.into(),
        month: t.wMonth.into(),
        day: t.wDay.into(),
        hour: t.wHour.into(),
        minute: t.wMinute.into(),
        second: t.wSecond.into(),
    }
}

// ── Processes ─────────────────────────────────────────────────────────────────

/// Spawned helpers must never flash a console window.
pub fn no_console(cmd: &mut Command) -> &mut Command {
    cmd.creation_flags(CREATE_NO_WINDOW)
}

pub fn open_url(url: &str) {
    let _ = no_console(Command::new("rundll32.exe").args(["url.dll,FileProtocolHandler", url]))
        .spawn();
}

pub fn reveal_folder(path: &str) {
    let _ = Command::new("explorer").arg(path).spawn();
}

/// Our own `where`: walks %PATH% against %PATHEXT%, no shell involved.
/// Rust quotes arguments correctly for `.cmd`/`.bat` targets since 1.77, so
/// spawning `code.cmd` directly is safe.
pub fn find_on_path(stem: &str) -> Option<PathBuf> {
    let exts = std::env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".into());
    let dirs = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&dirs) {
        for ext in exts.split(';').filter(|e| !e.is_empty()) {
            let candidate = dir.join(format!("{stem}{}", ext.to_lowercase()));
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

// ── Who we are ────────────────────────────────────────────────────────────────
//
// Named pipes share one machine-wide namespace, so the SID in the name is what
// keeps two accounts on the same machine from ever meeting on `boo-*`.
// boo-hook computes the same string (hook/src/win.rs) and additionally checks
// that the process serving the pipe really is us.

/// The SID of the account this process runs as, as `S-1-5-21-…`.
pub fn current_user_sid() -> Option<String> {
    unsafe {
        let mut token = HANDLE::default();
        OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token).ok()?;

        // First call sizes the buffer, second fills it.
        let mut needed = 0u32;
        let _ = GetTokenInformation(token, TokenUser, None, 0, &mut needed);
        if needed == 0 {
            let _ = CloseHandle(token);
            return None;
        }
        let mut buf = vec![0u8; needed as usize];
        let ok = GetTokenInformation(
            token,
            TokenUser,
            Some(buf.as_mut_ptr().cast()),
            needed,
            &mut needed,
        )
        .is_ok();
        let _ = CloseHandle(token);
        if !ok {
            return None;
        }

        let user = &*(buf.as_ptr() as *const TOKEN_USER);
        let mut text = PWSTR::null();
        ConvertSidToStringSidW(user.User.Sid, &mut text).ok()?;
        let sid = text.to_string().ok();
        let _ = LocalFree(Some(HLOCAL(text.0 as *mut _)));
        sid
    }
}

// ── Cursor ────────────────────────────────────────────────────────────────────

/// The 60 Hz poll reads the cursor and flips click-through from it.
pub const CURSOR_POLL: bool = true;

/// Cursor position in physical screen pixels.
pub fn cursor_physical() -> Option<(f64, f64)> {
    let mut p = POINT::default();
    unsafe { GetCursorPos(&mut p).ok()? };
    Some((p.x as f64, p.y as f64))
}

/// True while the left mouse button is held — the only signal we get that a
/// drag might be in flight before it reaches the window.
pub fn left_button_down() -> bool {
    unsafe { (GetAsyncKeyState(VK_LBUTTON.0 as i32) as u16 & 0x8000) != 0 }
}

// ── Island window ─────────────────────────────────────────────────────────────

fn hwnd_of(win: &WebviewWindow) -> Option<HWND> {
    let raw = win.hwnd().ok()?.0 as isize;
    if raw == 0 {
        return None;
    }
    Some(HWND(raw as *mut _))
}

/// A half-resolution desktop image; Boo is excluded only during the blit.
pub fn glass_snapshot(win: &WebviewWindow) -> Result<serde_json::Value, String> {
    use std::ffi::c_void;
    use ::windows::Win32::UI::WindowsAndMessaging::{
        SetWindowDisplayAffinity, WDA_EXCLUDEFROMCAPTURE, WDA_NONE,
    };

    // GDI is linked directly to keep the existing Cargo feature set unchanged.
    #[link(name = "user32")]
    extern "system" {
        fn GetDC(hwnd: isize) -> isize;
        fn ReleaseDC(hwnd: isize, dc: isize) -> i32;
    }
    #[link(name = "gdi32")]
    extern "system" {
        fn CreateCompatibleDC(dc: isize) -> isize;
        fn DeleteDC(dc: isize) -> i32;
        fn CreateDIBSection(dc: isize, info: *const u32, usage: u32, bits: *mut *mut c_void, section: isize, offset: u32) -> isize;
        fn SelectObject(dc: isize, object: isize) -> isize;
        fn DeleteObject(object: isize) -> i32;
        fn SetStretchBltMode(dc: isize, mode: i32) -> i32;
        fn SetBrushOrgEx(dc: isize, x: i32, y: i32, old: *mut c_void) -> i32;
        fn StretchBlt(dst: isize, x: i32, y: i32, w: i32, h: i32, src: isize, sx: i32, sy: i32, sw: i32, sh: i32, rop: u32) -> i32;
        fn GdiFlush() -> i32;
    }
    #[link(name = "dwmapi")]
    extern "system" { fn DwmFlush() -> i32; }

    // ponytail: one-island lock; split by HWND if multiple islands are introduced.
    static CAPTURE: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _lock = CAPTURE.lock().map_err(|e| e.to_string())?;
    let hwnd = hwnd_of(win).ok_or("Missing island window")?;
    let pos = win.inner_position().map_err(|e| e.to_string())?;
    let size = win.inner_size().map_err(|e| e.to_string())?;
    let scale = win.scale_factor().map_err(|e| e.to_string())?;
    if size.height as f64 / scale <= 6.0 || size.width > 8192 || size.height > 4096 {
        return Err("Island is hidden or capture size is invalid".into());
    }
    let w = ((size.width as f64 / scale / 2.0).ceil() as i32).max(1);
    let h = ((size.height as f64 / scale / 2.0).ceil() as i32).max(1);
    let bytes = (w * h * 4) as usize;
    // BITMAPINFOHEADER: top-down, 32-bit BI_RGB, followed by an unused color slot.
    let info = [40u32, w as u32, (-h) as u32, 1 | (32 << 16), 0, bytes as u32, 0, 0, 0, 0, 0];
    struct Capture { hwnd: HWND, screen: isize, dc: isize, bitmap: isize, old: isize, excluded: bool }
    impl Drop for Capture {
        fn drop(&mut self) {
            unsafe {
                if self.excluded { let _ = SetWindowDisplayAffinity(self.hwnd, WDA_NONE); }
                if self.old != 0 && self.old != -1 { SelectObject(self.dc, self.old); }
                if self.bitmap != 0 { DeleteObject(self.bitmap); }
                if self.dc != 0 { DeleteDC(self.dc); }
                if self.screen != 0 { ReleaseDC(0, self.screen); }
            }
        }
    }
    let mut capture = Capture { hwnd, screen: 0, dc: 0, bitmap: 0, old: 0, excluded: false };
    let mut bits = std::ptr::null_mut();
    unsafe {
        capture.screen = GetDC(0);
        if capture.screen == 0 { return Err("Desktop DC unavailable".into()); }
        capture.dc = CreateCompatibleDC(capture.screen);
        if capture.dc == 0 { return Err("Capture DC unavailable".into()); }
        capture.bitmap = CreateDIBSection(capture.screen, info.as_ptr(), 0, &mut bits, 0, 0);
        if capture.bitmap == 0 || bits.is_null() { return Err("Capture bitmap unavailable".into()); }
        capture.old = SelectObject(capture.dc, capture.bitmap);
        if capture.old == 0 || capture.old == -1 { return Err("Cannot select capture bitmap".into()); }
        SetStretchBltMode(capture.dc, 4); // HALFTONE downsampling.
        SetBrushOrgEx(capture.dc, 0, 0, std::ptr::null_mut());
        SetWindowDisplayAffinity(hwnd, WDA_EXCLUDEFROMCAPTURE).map_err(|e| e.to_string())?;
        capture.excluded = true;
        if DwmFlush() < 0 { return Err("Compositor did not flush".into()); }
        let ok = StretchBlt(capture.dc, 0, 0, w, h, capture.screen,
            pos.x, pos.y, size.width as i32, size.height as i32, 0x00CC0020); // SRCCOPY
        let flushed = GdiFlush();
        SetWindowDisplayAffinity(hwnd, WDA_NONE).map_err(|e| e.to_string())?;
        capture.excluded = false;
        if ok == 0 || flushed == 0 { return Err("Desktop capture failed".into()); }
    }
    let mut bmp = Vec::with_capacity(54 + bytes);
    bmp.extend_from_slice(b"BM");
    bmp.extend_from_slice(&((54 + bytes) as u32).to_le_bytes());
    bmp.extend_from_slice(&[0; 4]);
    bmp.extend_from_slice(&54u32.to_le_bytes());
    for field in &info[..10] { bmp.extend_from_slice(&field.to_le_bytes()); }
    bmp.extend_from_slice(unsafe { std::slice::from_raw_parts(bits.cast::<u8>(), bytes) });
    // BI_RGB ignores alpha, but an opaque byte also handles decoders that read it.
    for pixel in bmp[54..].chunks_exact_mut(4) { pixel[3] = 255; }
    const BASE64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut data_url = String::with_capacity(22 + bmp.len().div_ceil(3) * 4);
    data_url.push_str("data:image/bmp;base64,");
    for chunk in bmp.chunks(3) {
        let n = ((chunk[0] as u32) << 16) | ((chunk.get(1).copied().unwrap_or(0) as u32) << 8)
            | chunk.get(2).copied().unwrap_or(0) as u32;
        data_url.push(BASE64[((n >> 18) & 63) as usize] as char);
        data_url.push(BASE64[((n >> 12) & 63) as usize] as char);
        data_url.push(if chunk.len() > 1 { BASE64[((n >> 6) & 63) as usize] as char } else { '=' });
        data_url.push(if chunk.len() > 2 { BASE64[(n & 63) as usize] as char } else { '=' });
    }
    Ok(serde_json::json!({ "dataUrl": data_url,
        "width": size.width as f64 / scale, "height": size.height as f64 / scale }))
}


/// WS_EX_NOACTIVATE keeps clicks from stealing focus; WS_EX_TOOLWINDOW keeps the
/// island out of Alt-Tab.
pub fn make_non_activating(win: &WebviewWindow) {
    let Some(hwnd) = hwnd_of(win) else { return };
    unsafe {
        let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
        let want = ex | WS_EX_NOACTIVATE.0 as isize | WS_EX_TOOLWINDOW.0 as isize;
        SetWindowLongPtrW(hwnd, GWL_EXSTYLE, want);
    }
}

/// Temporarily allow activation so a text field inside the island can be typed in.
pub fn set_activating(win: &WebviewWindow, activating: bool) {
    let Some(hwnd) = hwnd_of(win) else { return };
    unsafe {
        let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
        let want = if activating {
            ex & !(WS_EX_NOACTIVATE.0 as isize)
        } else {
            ex | WS_EX_NOACTIVATE.0 as isize
        };
        SetWindowLongPtrW(hwnd, GWL_EXSTYLE, want);
    }
}

/// Click-through here is the poll's WS_EX_TRANSPARENT toggle, not a region.
pub fn set_input_region(_win: &WebviewWindow, _rect: Option<(f64, f64, f64, f64)>) {}
