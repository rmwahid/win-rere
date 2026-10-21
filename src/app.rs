//! The widget window, its single message loop, and the bridge between the UI
//! and the archive worker.

use std::cell::RefCell;
use std::fs;
use std::rc::Rc;
use std::sync::Arc;

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{CreateSolidBrush, HBRUSH};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::GetDpiForSystem;
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, FindWindowW, GetMessageW,
    GetWindowLongPtrW, LoadCursorW, LoadIconW, MINMAXINFO, MSG, PostMessageW, PostQuitMessage,
    RegisterClassW, SetForegroundWindow, SetWindowLongPtrW, SetWindowPos, ShowWindow,
    TranslateMessage, GWLP_USERDATA, HTBOTTOM, HTBOTTOMLEFT, HTBOTTOMRIGHT, HTCLIENT, HTLEFT,
    HTRIGHT, HTTOP, HTTOPLEFT, HTTOPRIGHT, IDC_ARROW, SC_MAXIMIZE, SIZE_MINIMIZED, SWP_NOACTIVATE,
    SWP_NOZORDER, SW_RESTORE, SW_SHOW, SW_SHOWNORMAL, WA_INACTIVE, WM_ACTIVATE, WM_CLOSE, WM_DESTROY,
    WM_DPICHANGED, WM_EXITSIZEMOVE, WM_GETMINMAXINFO, WM_NCHITTEST, WM_NCLBUTTONDBLCLK, WM_SIZE,
    WM_SYSCOMMAND, WNDCLASSW, WS_CLIPCHILDREN, WS_CLIPSIBLINGS, WS_EX_APPWINDOW, WS_MINIMIZEBOX,
    WS_POPUP,
};
use windows::Win32::Foundation::POINT;

use crate::archive;
use crate::logging;
use crate::model::{Bookmark, UiCommand, UiEvent, WindowState};
use crate::notify::{CommandInbox, Notifier, COMMAND_MESSAGE, EVENT_MESSAGE};
use crate::paths::Paths;
use crate::security;
use crate::state;
use crate::ui;
use crate::webview::{self, WebView};
use crate::window;

const CLASS_NAME: PCWSTR = w!("win-rere-widget");
const WINDOW_TITLE: PCWSTR = w!("win-rere");

/// Starts the widget, or raises the running one and exits.
pub fn run(paths: Paths) -> Result<(), String> {
    // SAFETY: looking up a window with this widget's unique class and title.
    unsafe {
        if let Ok(existing) = FindWindowW(CLASS_NAME, WINDOW_TITLE) {
            if !existing.is_invalid() {
                let _ = ShowWindow(existing, SW_RESTORE);
                let _ = SetForegroundWindow(existing);
                return Ok(());
            }
        }
    }

    let mut app = App::launch(paths)?;
    app.pump();
    Ok(())
}

struct App {
    hwnd: HWND,
    paths: Paths,
    bookmarks: Vec<Bookmark>,
    window_state: WindowState,
    active: Rc<RefCell<Option<String>>>,
    notifier: Notifier,
    inbox: CommandInbox,
    webview: Option<WebView>,
    saving: bool,
}

impl App {
    fn launch(paths: Paths) -> Result<Box<Self>, String> {
        state::cleanup_temp(&paths);
        let mut window_state = state::read_window_state(&paths);
        let bookmarks = state::load_bookmarks(&paths);
        if let Some(id) = &window_state.last_open {
            if !bookmarks.iter().any(|bookmark| &bookmark.id == id) {
                window_state.last_open = None;
            }
        }
        logging::line(&format!("loaded {} bookmark(s)", bookmarks.len()));

        let system_scale = {
            // SAFETY: plain DPI query.
            let dpi = unsafe { GetDpiForSystem() };
            if dpi == 0 {
                1.0
            } else {
                dpi as f32 / 96.0
            }
        };
        let first_run = window_state.x == i32::MIN || window_state.y == i32::MIN;
        let initial_width = if first_run {
            (window_state.width as f32 * system_scale).round() as i32
        } else {
            window_state.width
        };
        let initial_height = if first_run {
            (window_state.height as f32 * system_scale).round() as i32
        } else {
            window_state.height
        };

        // SAFETY: registering a class and creating this thread's window.
        let hwnd = unsafe {
            let module = GetModuleHandleW(None)
                .map_err(|error| format!("Could not find the module handle: {error}"))?;
            let instance = HINSTANCE(module.0);
            let brush: HBRUSH = CreateSolidBrush(COLORREF(0x000e0c0c));
            // Resource id 1 is the icon compiled by build.rs.
            let icon = match LoadIconW(Some(instance), PCWSTR(1 as *const u16)) {
                Ok(icon) => {
                    logging::line("window icon loaded");
                    icon
                }
                Err(_) => {
                    logging::line("no window icon resource");
                    Default::default()
                }
            };
            let class = WNDCLASSW {
                lpfnWndProc: Some(window_procedure),
                hInstance: instance,
                hIcon: icon,
                hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
                hbrBackground: brush,
                lpszClassName: CLASS_NAME,
                ..Default::default()
            };
            if RegisterClassW(&class) == 0 {
                return Err("Could not register the widget window class.".to_string());
            }
            CreateWindowExW(
                WS_EX_APPWINDOW,
                CLASS_NAME,
                WINDOW_TITLE,
                WS_POPUP | WS_MINIMIZEBOX | WS_CLIPCHILDREN | WS_CLIPSIBLINGS,
                0,
                0,
                initial_width.max(320),
                initial_height.max(240),
                None,
                None,
                Some(instance),
                None,
            )
            .map_err(|error| format!("Could not create the widget window: {error}"))?
        };

        // SAFETY: placing and styling the window before it is shown.
        unsafe {
            let (x, y, width, height) = window::restore_placement(hwnd, &window_state);
            let _ = SetWindowPos(
                hwnd,
                None,
                x,
                y,
                width,
                height,
                SWP_NOZORDER | SWP_NOACTIVATE,
            );
        }
        window::style_window(hwnd);
        window::set_topmost(hwnd, window_state.topmost);

        let active = Rc::new(RefCell::new(None));
        let notifier = Notifier::new(hwnd);
        let inbox = CommandInbox::new(hwnd);
        let mut app = Box::new(Self {
            hwnd,
            paths,
            bookmarks,
            window_state,
            active: active.clone(),
            notifier: notifier.clone(),
            inbox: inbox.clone(),
            webview: None,
            saving: false,
        });

        // SAFETY: the Box keeps the address stable for as long as the window
        // exists. The window procedure only dereferences it on this thread.
        unsafe {
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, &mut *app as *mut Self as isize);
        }

        match WebView::create(hwnd, &app.paths, active, &notifier, &inbox) {
            Ok(webview) => {
                if let Some(rect) = window::webview_rect(hwnd) {
                    logging::line(&format!(
                        "webview bounds {}x{} at {},{}",
                        rect.right - rect.left,
                        rect.bottom - rect.top,
                        rect.left,
                        rect.top
                    ));
                    webview.set_bounds(rect);
                } else {
                    logging::line("webview bounds unavailable at startup");
                }
                webview.set_visible(true);
                webview.set_memory_low(false);
                let ui_html = ui::html(app.window_state.sidebar_visible);
                if let Err(error) = webview.navigate_ui(&ui_html) {
                    webview.close();
                    // SAFETY: this thread's own window.
                    unsafe {
                        let _ = DestroyWindow(hwnd);
                    }
                    return Err(error);
                }
                app.webview = Some(webview);
            }
            Err(error) => {
                // SAFETY: this thread's own window.
                unsafe {
                    let _ = DestroyWindow(hwnd);
                }
                return Err(error);
            }
        }

        // SAFETY: showing this thread's own window.
        unsafe {
            let _ = ShowWindow(hwnd, SW_SHOW);
            let _ = SetForegroundWindow(hwnd);
        }
        if let Some(webview) = &app.webview {
            webview.focus();
        }
        // Capture the real placement once. `open_bookmark` later writes
        // `last_open` into the same file, and it must not persist the sentinel
        // coordinates used to mean "never placed".
        app.save_placement();

        // The worker writes the index on the UI thread, so an old last_open is
        // only a hint; `open_bookmark` checks that it still exists.
        Ok(app)
    }

    fn pump(&mut self) {
        let mut message = MSG::default();
        loop {
            // SAFETY: the standard message loop for this thread's window.
            let result = unsafe { GetMessageW(&mut message, None, 0, 0).0 };
            if result == -1 || result == 0 {
                break;
            }
            // SAFETY: dispatching the message that was just retrieved.
            unsafe {
                let _ = TranslateMessage(&message);
                DispatchMessageW(&message);
            }
        }
    }

    fn resize_webview(&self) {
        if let (Some(webview), Some(rect)) = (&self.webview, window::webview_rect(self.hwnd)) {
            webview.set_bounds(rect);
        }
    }

    fn on_size(&mut self, minimized: bool) {
        let Some(webview) = &self.webview else {
            return;
        };
        if minimized {
            webview.set_visible(false);
            webview.set_memory_low(true);
            webview.suspend();
        } else {
            webview.set_memory_low(false);
            webview.resume();
            if let Some(rect) = window::webview_rect(self.hwnd) {
                webview.set_bounds(rect);
            }
            webview.set_visible(true);
        }
    }

    fn handle_command(&mut self, command: UiCommand) {
        match command {
            UiCommand::Ready => {
                logging::line("UI ready");
                self.send_library();
                if let Some(id) = self.window_state.last_open.clone() {
                    if self.bookmarks.iter().any(|bookmark| bookmark.id == id) {
                        self.open_bookmark(&id);
                    }
                }
            }
            UiCommand::Save { url } => self.start_save(&url),
            UiCommand::Open { id } => self.open_bookmark(&id),
            UiCommand::Delete { id } => self.delete_bookmark(&id),
            UiCommand::Rename { id, title } => self.rename_bookmark(&id, &title),
            UiCommand::Sidebar { visible } => self.set_sidebar(visible),
            UiCommand::MobileView { value } => self.set_mobile_view(value),
            UiCommand::OpenExternal { id } => self.open_external(&id),
            UiCommand::Drag => window::begin_drag(self.hwnd),
            UiCommand::Pin { value } => self.pin(value),
            UiCommand::Minimize => window::minimize(self.hwnd),
            UiCommand::Close => {
                // SAFETY: this thread's own window.
                unsafe {
                    let _ = PostMessageW(
                        Some(self.hwnd),
                        WM_CLOSE,
                        WPARAM(0),
                        LPARAM(0),
                    );
                }
            }
        }
    }

    fn start_save(&mut self, url: &str) {
        if self.saving {
            self.send_event(UiEvent::SaveError {
                message: "A page is already being saved.".to_string(),
            });
            return;
        }
        let normalized = match security::normalize_input(url) {
            Ok(url) => url,
            Err(message) => {
                self.send_event(UiEvent::SaveError { message });
                return;
            }
        };
        self.saving = true;
        logging::line(&format!("save requested: {normalized}"));
        state::mark_dirty(&self.paths);
        self.send_event(UiEvent::SaveStarted {
            url: normalized.to_string(),
        });

        let paths = self.paths.clone();
        let notifier = self.notifier.clone();
        let input = normalized.to_string();
        let spawned = std::thread::Builder::new()
            .name("win-rere-archive".to_string())
            .spawn(move || {
                let progress_notifier = notifier.clone();
                let progress: Arc<dyn Fn(String) + Send + Sync> = Arc::new(move |message| {
                    progress_notifier.send(UiEvent::SaveProgress { message });
                });
                let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    archive::save(&paths, &input, progress)
                }));
                match outcome {
                    Ok(Ok(bookmark)) => notifier.send(UiEvent::SaveDone { item: bookmark }),
                    Ok(Err(message)) => notifier.send(UiEvent::SaveError { message }),
                    Err(_) => {
                        logging::line("the archive worker panicked");
                        notifier.send(UiEvent::SaveError {
                            message: "The archiver stopped unexpectedly.".to_string(),
                        });
                    }
                }
            });

        if let Err(error) = spawned {
            self.saving = false;
            state::clear_dirty(&self.paths);
            self.send_event(UiEvent::SaveError {
                message: format!("Could not start the archiver: {error}"),
            });
        }
    }

    fn open_bookmark(&mut self, id: &str) {
        if !state::valid_id(id) {
            self.send_event(UiEvent::OpenError {
                message: "That bookmark id is not valid.".to_string(),
            });
            return;
        }
        let Some(bookmark) = self.bookmarks.iter().find(|bookmark| bookmark.id == id) else {
            self.send_event(UiEvent::OpenError {
                message: "That bookmark no longer exists.".to_string(),
            });
            return;
        };
        let use_mobile = self.window_state.mobile_view && bookmark.has_mobile;
        let Some(webview) = &self.webview else {
            self.send_event(UiEvent::OpenError {
                message: "The reader is not ready yet.".to_string(),
            });
            return;
        };
        if let Err(message) = webview.show_archive(id, &self.paths.archive_dir(id)) {
            self.send_event(UiEvent::OpenError { message });
            return;
        }

        *self.active.borrow_mut() = Some(id.to_string());
        self.window_state.last_open = Some(id.to_string());
        state::write_window_state(&self.paths, &self.window_state);
        logging::line(&format!(
            "opening bookmark {id} ({})",
            if use_mobile { "mobile" } else { "desktop" }
        ));
        self.send_event(UiEvent::Opening {
            id: id.to_string(),
            url: webview::content_url(id, use_mobile),
        });
    }

    fn clear_active(&mut self) {
        *self.active.borrow_mut() = None;
        self.window_state.last_open = None;
        state::write_window_state(&self.paths, &self.window_state);
        if let Some(webview) = &self.webview {
            webview.clear_archive_mapping();
        }
        self.send_event(UiEvent::Cleared);
    }

    fn delete_bookmark(&mut self, id: &str) {
        if !state::valid_id(id) {
            self.send_event(UiEvent::DeleteError {
                message: "That bookmark id is not valid.".to_string(),
            });
            return;
        }
        let Some(index) = self.bookmarks.iter().position(|bookmark| bookmark.id == id) else {
            self.send_event(UiEvent::DeleteError {
                message: "That bookmark no longer exists.".to_string(),
            });
            return;
        };
        state::mark_dirty(&self.paths);
        if let Err(message) = state::delete_archive(&self.paths, id) {
            self.send_event(UiEvent::DeleteError { message });
            return;
        }
        let removed = self.bookmarks.remove(index);
        match state::write_bookmarks(&self.paths, &self.bookmarks) {
            Ok(()) => state::clear_dirty(&self.paths),
            Err(error) => logging::line(&error),
        }
        if self.active.borrow().as_deref() == Some(id) {
            self.clear_active();
        }
        self.send_library();
        self.send_event(UiEvent::Deleted {
            id: id.to_string(),
        });
        self.send_event(UiEvent::Status {
            message: format!("Deleted {}.", removed.title),
        });
    }

    /// Changes only the bookmark title. The URL, page and assets are immutable.
    fn rename_bookmark(&mut self, id: &str, title: &str) {
        if !state::valid_id(id) {
            self.send_event(UiEvent::RenameError {
                message: "That bookmark id is not valid.".to_string(),
            });
            return;
        }
        let title = state::clean_title(title);
        if title.is_empty() {
            self.send_event(UiEvent::RenameError {
                message: "A title is required.".to_string(),
            });
            return;
        }
        let Some(index) = self
            .bookmarks
            .iter()
            .position(|bookmark| bookmark.id == id)
        else {
            self.send_event(UiEvent::RenameError {
                message: "That bookmark no longer exists.".to_string(),
            });
            return;
        };

        let previous = self.bookmarks[index].title.clone();
        self.bookmarks[index].title = title.clone();
        let meta = match serde_json::to_vec_pretty(&self.bookmarks[index]) {
            Ok(meta) => meta,
            Err(error) => {
                self.bookmarks[index].title = previous;
                self.send_event(UiEvent::RenameError {
                    message: format!("Could not encode the new title: {error}"),
                });
                return;
            }
        };
        state::mark_dirty(&self.paths);
        if let Err(error) = fs::write(self.paths.meta_file(id), meta) {
            self.bookmarks[index].title = previous;
            self.send_event(UiEvent::RenameError {
                message: format!("Could not save the new title: {error}"),
            });
            return;
        }
        match state::write_bookmarks(&self.paths, &self.bookmarks) {
            Ok(()) => state::clear_dirty(&self.paths),
            Err(error) => logging::line(&format!(
                "could not write the bookmark index after a rename: {error}"
            )),
        }
        logging::line(&format!("renamed bookmark {id}"));
        self.send_library();
        self.send_event(UiEvent::Renamed {
            id: id.to_string(),
            title,
        });
    }

    fn set_sidebar(&mut self, visible: bool) {
        self.window_state.sidebar_visible = visible;
        state::write_window_state(&self.paths, &self.window_state);
    }

    fn set_mobile_view(&mut self, value: bool) {
        self.window_state.mobile_view = value;
        state::write_window_state(&self.paths, &self.window_state);
    }

    fn open_external(&mut self, id: &str) {
        let Some(bookmark) = self.bookmarks.iter().find(|bookmark| bookmark.id == id) else {
            self.send_event(UiEvent::Status {
                message: "That bookmark no longer exists.".to_string(),
            });
            return;
        };
        let Ok(url) = url::Url::parse(&bookmark.url) else {
            return;
        };
        if !matches!(url.scheme(), "http" | "https") {
            return;
        }
        let operation = w!("open");
        let target = window::wide(bookmark.url.as_str());
        // SAFETY: a plain shell invocation with valid, NUL terminated strings.
        let result = unsafe {
            ShellExecuteW(
                Some(self.hwnd),
                operation,
                PCWSTR(target.as_ptr()),
                PCWSTR::null(),
                PCWSTR::null(),
                SW_SHOWNORMAL,
            )
        };
        if result.0 as isize <= 32 {
            logging::line(&format!("ShellExecuteW failed with {:?}", result.0));
            self.send_event(UiEvent::Status {
                message: "Could not open the page in the default browser.".to_string(),
            });
        }
    }

    fn pin(&mut self, value: bool) {
        self.window_state.topmost = value;
        window::set_topmost(self.hwnd, value);
        state::write_window_state(&self.paths, &self.window_state);
        self.send_event(UiEvent::Pinned { value });
    }

    fn handle_event(&mut self, event: UiEvent) {
        match event {
            UiEvent::SaveDone { item } => {
                self.saving = false;
                if let Some(existing) = self
                    .bookmarks
                    .iter_mut()
                    .find(|bookmark| bookmark.id == item.id)
                {
                    *existing = item.clone();
                } else {
                    self.bookmarks.push(item.clone());
                }
                self.sort_bookmarks();
                match state::write_bookmarks(&self.paths, &self.bookmarks) {
                    Ok(()) => state::clear_dirty(&self.paths),
                    Err(error) => logging::line(&error),
                }
                logging::line(&format!("bookmark {} saved and indexed", item.id));
                self.send_library();
                self.send_event(UiEvent::SaveDone { item: item.clone() });
                self.open_bookmark(&item.id);
            }
            UiEvent::SaveError { message } => {
                self.saving = false;
                state::clear_dirty(&self.paths);
                self.send_event(UiEvent::SaveError { message });
            }
            UiEvent::OpenError { message } => {
                self.clear_active();
                self.send_event(UiEvent::OpenError { message });
            }
            other => self.send_event(other),
        }
    }

    fn sort_bookmarks(&mut self) {
        self.bookmarks.sort_by(|a, b| {
            b.saved_at
                .cmp(&a.saved_at)
                .then_with(|| a.title.cmp(&b.title))
        });
    }

    fn send_library(&self) {
        self.send_event(UiEvent::Library {
            items: self.bookmarks.clone(),
            topmost: self.window_state.topmost,
            active: self.active.borrow().clone(),
            mobile_view: self.window_state.mobile_view,
        });
    }

    fn send_event(&self, event: UiEvent) {
        if let Some(webview) = &self.webview {
            webview.post_event(&event);
        }
    }

    fn save_placement(&mut self) {
        let Some(rect) = window::current_rect(self.hwnd) else {
            return;
        };
        self.window_state.x = rect.left;
        self.window_state.y = rect.top;
        self.window_state.width = rect.right - rect.left;
        self.window_state.height = rect.bottom - rect.top;
        state::write_window_state(&self.paths, &self.window_state);
    }

    fn shutdown(&mut self) {
        self.save_placement();
        if let Some(webview) = self.webview.take() {
            webview.close();
        }
    }
}

/// Handles input messages for the whole widget.
///
/// # Safety
///
/// Called by Windows on the thread that created the window.
unsafe extern "system" fn window_procedure(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    let pointer = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *mut App;
    let app = if pointer.is_null() {
        None
    } else {
        // SAFETY: the pointer comes from the stable Box created in `launch`,
        // and Windows calls this procedure on the owning thread only.
        Some(unsafe { &mut *pointer })
    };

    match message {
        WM_GETMINMAXINFO => {
            // SAFETY: Windows passes a MINMAXINFO pointer for this message.
            unsafe {
                let info = lparam.0 as *mut MINMAXINFO;
                if !info.is_null() {
                    let (min_width, min_height, max_width, max_height) = window::size_limits(hwnd);
                    (*info).ptMinTrackSize = POINT {
                        x: min_width,
                        y: min_height,
                    };
                    (*info).ptMaxTrackSize = POINT {
                        x: max_width,
                        y: max_height,
                    };
                }
            }
            LRESULT(0)
        }
        WM_NCHITTEST => LRESULT(hit_test(hwnd, lparam) as isize),
        WM_ACTIVATE => {
            // A widget spends most of its life unfocused. Dropping the WebView2
            // memory target lets the engine release caches while it is idle,
            // and returning focus restores the normal target.
            if let Some(app) = app {
                if let Some(webview) = &app.webview {
                    let active = (wparam.0 & 0xffff) != WA_INACTIVE as usize;
                    webview.set_memory_low(!active);
                }
            }
            unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
        }
        WM_SIZE => {
            if let Some(app) = app {
                app.on_size(wparam.0 == SIZE_MINIMIZED as usize);
            }
            LRESULT(0)
        }
        WM_DPICHANGED => {
            // SAFETY: Windows passes the suggested rectangle in lparam.
            unsafe {
                let suggested = lparam.0 as *const RECT;
                if !suggested.is_null() {
                    let rect = *suggested;
                    let _ = SetWindowPos(
                        hwnd,
                        None,
                        rect.left,
                        rect.top,
                        rect.right - rect.left,
                        rect.bottom - rect.top,
                        SWP_NOZORDER | SWP_NOACTIVATE,
                    );
                }
            }
            if let Some(app) = app {
                app.resize_webview();
            }
            LRESULT(0)
        }
        WM_EXITSIZEMOVE => {
            if let Some(app) = app {
                app.save_placement();
            }
            LRESULT(0)
        }
        WM_SYSCOMMAND => {
            if (wparam.0 & 0xfff0) == SC_MAXIMIZE as usize {
                return LRESULT(0);
            }
            unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
        }
        WM_NCLBUTTONDBLCLK => LRESULT(0),
        WM_CLOSE => {
            if let Some(app) = app {
                app.save_placement();
            }
            // SAFETY: destroying this thread's own window.
            unsafe {
                let _ = DestroyWindow(hwnd);
            }
            LRESULT(0)
        }
        WM_DESTROY => {
            if let Some(app) = app {
                app.shutdown();
            }
            // SAFETY: ending this thread's message loop.
            unsafe {
                PostQuitMessage(0);
            }
            LRESULT(0)
        }
        COMMAND_MESSAGE => {
            if let Some(app) = app {
                for command in app.inbox.drain() {
                    app.handle_command(command);
                }
            }
            LRESULT(0)
        }
        EVENT_MESSAGE => {
            if let Some(app) = app {
                for event in app.notifier.drain() {
                    app.handle_event(event);
                }
            }
            LRESULT(0)
        }
        _ => unsafe { DefWindowProcW(hwnd, message, wparam, lparam) },
    }
}

/// The resize border lives in the eight pixels of window that the WebView does
/// not cover, so the parent receives these hit tests.
fn hit_test(hwnd: HWND, lparam: LPARAM) -> u32 {
    let x = (lparam.0 & 0xffff) as u16 as i16 as i32;
    let y = ((lparam.0 >> 16) & 0xffff) as u16 as i16 as i32;
    let point = window::client_point_from_screen(hwnd, x, y);
    let client = window::client_rect(hwnd);
    if client.right <= 0 || client.bottom <= 0 {
        return HTCLIENT;
    }
    let border = window::edge(hwnd);
    let left = point.x < border;
    let right = point.x >= client.right - border;
    let top = point.y < border;
    let bottom = point.y >= client.bottom - border;

    match (left, right, top, bottom) {
        (true, _, true, _) => HTTOPLEFT,
        (_, true, true, _) => HTTOPRIGHT,
        (true, _, _, true) => HTBOTTOMLEFT,
        (_, true, _, true) => HTBOTTOMRIGHT,
        (true, _, _, _) => HTLEFT,
        (_, true, _, _) => HTRIGHT,
        (_, _, true, _) => HTTOP,
        (_, _, _, true) => HTBOTTOM,
        _ => HTCLIENT,
    }
}
