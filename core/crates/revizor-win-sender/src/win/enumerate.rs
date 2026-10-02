use crate::sources::{Source, SourceKind};
use windows::core::PWSTR;
use windows::Win32::Foundation::{BOOL, HWND, LPARAM, RECT, TRUE};
use windows::Win32::Graphics::Gdi::{
    EnumDisplayMonitors, EnumDisplaySettingsW, GetMonitorInfoW, DEVMODEW, ENUM_CURRENT_SETTINGS, HDC, HMONITOR, MONITORINFO,
    MONITORINFOEXW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetWindowLongW, GetWindowRect, GetWindowTextLengthW, GetWindowTextW, IsIconic, IsWindowVisible, GWL_EXSTYLE,
    WS_EX_TOOLWINDOW,
};

pub fn list_sources() -> Vec<Source> {
    let mut out = Vec::new();
    unsafe {
        let mut monitors: Vec<HMONITOR> = Vec::new();
        let _ = EnumDisplayMonitors(None, None, Some(mon_cb), LPARAM(&mut monitors as *mut _ as isize));
        for (i, hm) in monitors.iter().enumerate() {
            let mut mi = MONITORINFOEXW::default();
            mi.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
            if !GetMonitorInfoW(*hm, &mut mi as *mut _ as *mut MONITORINFO).as_bool() {
                continue;
            }
            let r = mi.monitorInfo.rcMonitor;
            let mut dm = DEVMODEW { dmSize: std::mem::size_of::<DEVMODEW>() as u16, ..Default::default() };
            let hz = if EnumDisplaySettingsW(windows::core::PCWSTR(mi.szDevice.as_ptr()), ENUM_CURRENT_SETTINGS, &mut dm).as_bool() {
                dm.dmDisplayFrequency
            } else {
                0
            };
            let primary = mi.monitorInfo.dwFlags & 1 != 0;
            out.push(Source {
                id: format!("m:{}", hm.0 as isize),
                name: format!("Display {}{}", i + 1, if primary { " (main)" } else { "" }),
                kind: SourceKind::Monitor { index: i as u32 },
                width: (r.right - r.left).max(0) as u32,
                height: (r.bottom - r.top).max(0) as u32,
                refresh_hz: hz,
            });
        }
        let mut wins: Vec<HWND> = Vec::new();
        let _ = EnumWindows(Some(win_cb), LPARAM(&mut wins as *mut _ as isize));
        for hw in wins {
            let len = GetWindowTextLengthW(hw);
            if len <= 0 {
                continue;
            }
            let mut buf = vec![0u16; len as usize + 1];
            let n = GetWindowTextW(hw, &mut buf);
            let title = String::from_utf16_lossy(&buf[..n.max(0) as usize]);
            let mut rc = RECT::default();
            if GetWindowRect(hw, &mut rc).is_err() {
                continue;
            }
            let (w, h) = ((rc.right - rc.left).max(0) as u32, (rc.bottom - rc.top).max(0) as u32);
            if w < 64 || h < 64 {
                continue;
            }
            out.push(Source { id: format!("w:{}", hw.0 as isize), name: title, kind: SourceKind::Window, width: w, height: h, refresh_hz: 0 });
        }
    }
    let _ = PWSTR::null();
    out
}

unsafe extern "system" fn mon_cb(hm: HMONITOR, _dc: HDC, _rc: *mut RECT, lparam: LPARAM) -> BOOL {
    let v = &mut *(lparam.0 as *mut Vec<HMONITOR>);
    v.push(hm);
    TRUE
}

unsafe extern "system" fn win_cb(hw: HWND, lparam: LPARAM) -> BOOL {
    if IsWindowVisible(hw).as_bool() && !IsIconic(hw).as_bool() {
        let ex = GetWindowLongW(hw, GWL_EXSTYLE) as u32;
        if ex & WS_EX_TOOLWINDOW.0 == 0 {
            (&mut *(lparam.0 as *mut Vec<HWND>)).push(hw);
        }
    }
    TRUE
}
