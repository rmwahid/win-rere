//! Window geometry, DPI and the small amount of Win32 the widget needs.

use std::ffi::c_void;
use std::mem::size_of;

use windows::Win32::Foundation::{HWND, LPARAM, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Dwm::{
    DwmSetWindowAttribute, DWMWA_BORDER_COLOR, DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_ROUND,
};
use windows::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MonitorFromWindow, ScreenToClient, MONITORINFO, MONITOR_DEFAULTTONEAREST,
};
use windows::Win32::UI::HiDpi::{GetDpiForSystem, GetDpiForWindow};
use windows::Win32::UI::Input::KeyboardAndMouse::ReleaseCapture;
use windows::Win32::UI::WindowsAndMessaging::{
    GetClientRect, GetSystemMetrics, GetWindowRect, IsIconic, SendMessageW, SetWindowPos, ShowWindow,
    SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN, SWP_NOACTIVATE,
    SWP_NOMOVE, SWP_NOSIZE, SW_MINIMIZE, WM_NCLBUTTONDOWN, HTCAPTION, HWND_NOTOPMOST, HWND_TOPMOST,
};

/// The unused strip around the WebView that acts as the resize border.
pub const EDGE_LOGICAL: i32 = 8;
/// Minimum size of the usable content, in logical pixels.
pub const MIN_INNER_WIDTH: i32 = 720;
pub const MIN_INNER_HEIGHT: i32 = 480;
/// The widget may never cover more than this share of the monitor work area.
const MAX_WORK_PERCENT: i32 = 90;
/// Gap from the work area edge when the widget has never been placed.
const PLACE_GAP: i32 = 24;

pub fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

pub fn scale(hwnd: HWND) -> f32 {
    let dpi = unsafe {
        let value = GetDpiForWindow(hwnd);
        if value == 0 {
            GetDpiForSystem()
        } else {
            value
        }
    };
    if dpi == 0 {
        1.0
    } else {
        dpi as f32 / 96.0
    }
}

pub fn edge(hwnd: HWND) -> i32 {
    (EDGE_LOGICAL as f32 * scale(hwnd)).round() as i32
}

pub fn client_rect(hwnd: HWND) -> RECT {
    let mut rect = RECT::default();
    // SAFETY: reading this window's client rectangle.
    unsafe {
        let _ = GetClientRect(hwnd, &mut rect);
    }
    rect
}

/// The rectangle the WebView should occupy: the client area minus the resize
/// border. Returns `None` while the window is collapsed or minimized.
pub fn webview_rect(hwnd: HWND) -> Option<RECT> {
    let client = client_rect(hwnd);
    let border = edge(hwnd);
    let width = (client.right - client.left) - border * 2;
    let height = (client.bottom - client.top) - border * 2;
    if width <= 0 || height <= 0 {
        return None;
    }
    Some(RECT {
        left: border,
        top: border,
        right: border + width,
        bottom: border + height,
    })
}

pub fn work_area(hwnd: HWND) -> RECT {
    // SAFETY: querying the monitor nearest this window.
    unsafe {
        let monitor = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
        let mut info = MONITORINFO {
            cbSize: size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        if GetMonitorInfoW(monitor, &mut info).as_bool() {
            return info.rcWork;
        }
    }
    RECT {
        left: unsafe { GetSystemMetrics(SM_XVIRTUALSCREEN) },
        top: unsafe { GetSystemMetrics(SM_YVIRTUALSCREEN) },
        right: unsafe { GetSystemMetrics(SM_CXVIRTUALSCREEN) },
        bottom: unsafe { GetSystemMetrics(SM_CYVIRTUALSCREEN) },
    }
}

/// Minimum and maximum outer window sizes in physical pixels.
pub fn size_limits(hwnd: HWND) -> (i32, i32, i32, i32) {
    let scale = scale(hwnd);
    let border = (EDGE_LOGICAL as f32 * scale).round() as i32;
    let min_width = (MIN_INNER_WIDTH as f32 * scale).round() as i32 + border * 2;
    let min_height = (MIN_INNER_HEIGHT as f32 * scale).round() as i32 + border * 2;
    let work = work_area(hwnd);
    let max_width = ((work.right - work.left) * MAX_WORK_PERCENT / 100).max(min_width);
    let max_height = ((work.bottom - work.top) * MAX_WORK_PERCENT / 100).max(min_height);
    (min_width, min_height, max_width, max_height)
}

/// Clamps a requested outer size to the current monitor's limits.
pub fn clamp_size(hwnd: HWND, width: i32, height: i32) -> (i32, i32) {
    let (min_width, min_height, max_width, max_height) = size_limits(hwnd);
    (width.clamp(min_width, max_width), height.clamp(min_height, max_height))
}

/// Places the widget in the top right corner of the work area, clamped so the
/// title area is never off screen.
pub fn default_position(hwnd: HWND, width: i32, height: i32) -> (i32, i32) {
    let work = work_area(hwnd);
    let x = (work.right - width - PLACE_GAP).max(work.left);
    let y = (work.top + PLACE_GAP).min(work.bottom - height);
    (x, y)
}

/// Keeps a restored rectangle usable when monitors changed since last time.
pub fn restore_placement(hwnd: HWND, state: &crate::model::WindowState) -> (i32, i32, i32, i32) {
    let first_run = state.x == i32::MIN || state.y == i32::MIN;
    let scale = scale(hwnd);
    let (mut width, mut height) = if first_run {
        (
            (state.width as f32 * scale).round() as i32,
            (state.height as f32 * scale).round() as i32,
        )
    } else {
        (state.width, state.height)
    };
    let (clamped_width, clamped_height) = clamp_size(hwnd, width, height);
    width = clamped_width;
    height = clamped_height;

    if first_run {
        let (x, y) = default_position(hwnd, width, height);
        return (x, y, width, height);
    }

    let work = work_area(hwnd);
    let min_x = work.left - width + 120;
    let max_x = work.right - 120;
    let min_y = work.top;
    let max_y = work.bottom - 60;
    (
        state.x.clamp(min_x, max_x),
        state.y.clamp(min_y, max_y),
        width,
        height,
    )
}

/// Saves the outer rectangle, unless the window is minimized (where Windows
/// reports a sentinel position).
pub fn current_rect(hwnd: HWND) -> Option<RECT> {
    // SAFETY: reading this window's rectangle and iconic state.
    unsafe {
        if IsIconic(hwnd).as_bool() {
            return None;
        }
        let mut rect = RECT::default();
        if GetWindowRect(hwnd, &mut rect).is_ok() {
            Some(rect)
        } else {
            None
        }
    }
}

pub fn client_point_from_screen(hwnd: HWND, x: i32, y: i32) -> POINT {
    let mut point = POINT { x, y };
    // SAFETY: converting a screen point into this window's client space.
    unsafe {
        let _ = ScreenToClient(hwnd, &mut point);
    }
    point
}

pub fn begin_drag(hwnd: HWND) {
    // SAFETY: hands the drag to Windows' own caption loop.
    unsafe {
        let _ = ReleaseCapture();
        SendMessageW(
            hwnd,
            WM_NCLBUTTONDOWN,
            Some(WPARAM(HTCAPTION as usize)),
            Some(LPARAM(0)),
        );
    }
}

pub fn set_topmost(hwnd: HWND, topmost: bool) {
    let after = if topmost {
        HWND_TOPMOST
    } else {
        HWND_NOTOPMOST
    };
    // SAFETY: this thread's own window.
    unsafe {
        let _ = SetWindowPos(
            hwnd,
            Some(after),
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
        );
    }
}

pub fn minimize(hwnd: HWND) {
    // SAFETY: this thread's own window.
    unsafe {
        let _ = ShowWindow(hwnd, SW_MINIMIZE);
    }
}

/// Rounds the window on Windows 11 and tints its DWM border. Both calls fail
/// harmlessly on older builds.
pub fn style_window(hwnd: HWND) {
    let corner = DWMWCP_ROUND.0 as u32;
    let border: u32 = 0x002e2c2c;
    // SAFETY: passing a pointer to a local u32 with its exact size.
    unsafe {
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_WINDOW_CORNER_PREFERENCE,
            &corner as *const u32 as *const c_void,
            size_of::<u32>() as u32,
        );
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_BORDER_COLOR,
            &border as *const u32 as *const c_void,
            size_of::<u32>() as u32,
        );
    }
}

