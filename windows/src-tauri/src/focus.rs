// The ↗ "Show window" button: brings the agent's own window to the front.
//
// Every hook event carries the relay's ancestor PIDs, nearest first (see
// hook/src/win.rs). Somewhere in that chain is the process that owns the
// window the agent runs in — the Claude app, Codex, Windows Terminal. The first
// ancestor that owns a visible top-level window wins.

#[cfg(windows)]
pub fn focus_ancestor_window(pids: &[u32]) -> bool {
    use windows::core::BOOL;
    use windows::Win32::Foundation::{HWND, LPARAM};
    use windows::Win32::UI::WindowsAndMessaging::{
        EnumWindows, GetWindow, GetWindowTextLengthW, GetWindowThreadProcessId, IsIconic,
        IsWindowVisible, SetForegroundWindow, ShowWindow, GW_OWNER, SW_RESTORE,
    };

    // Visible, unowned, titled top-level windows with their owning PID.
    unsafe extern "system" fn collect(hwnd: HWND, lparam: LPARAM) -> BOOL {
        let out = &mut *(lparam.0 as *mut Vec<(HWND, u32)>);
        let owned = GetWindow(hwnd, GW_OWNER).map(|o| !o.is_invalid()).unwrap_or(false);
        if IsWindowVisible(hwnd).as_bool() && !owned && GetWindowTextLengthW(hwnd) > 0 {
            let mut pid = 0u32;
            GetWindowThreadProcessId(hwnd, Some(&mut pid));
            out.push((hwnd, pid));
        }
        BOOL(1)
    }

    let mut windows: Vec<(HWND, u32)> = Vec::new();
    unsafe {
        let _ = EnumWindows(Some(collect), LPARAM(&mut windows as *mut _ as isize));
    }
    let Some(&(hwnd, _)) = pids
        .iter()
        .find_map(|pid| windows.iter().find(|(_, owner)| owner == pid))
    else {
        return false;
    };
    unsafe {
        if IsIconic(hwnd).as_bool() {
            let _ = ShowWindow(hwnd, SW_RESTORE);
        }
        // Allowed: the click on the island made Boo the foreground process.
        SetForegroundWindow(hwnd).as_bool()
    }
}

#[cfg(not(windows))]
pub fn focus_ancestor_window(_pids: &[u32]) -> bool {
    false
}
