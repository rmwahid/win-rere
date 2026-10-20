//! The single WebView2 view that draws the widget and the offline page.
//!
//! The widget chrome is a local HTML document loaded with `NavigateToString`.
//! The archived page is a sandboxed iframe placed inside that document. Its
//! files are served by WebView2's virtual host mapping, which works for iframes
//! (including out-of-process ones) without opening a socket.
//!
//! Scripts are enabled for the chrome, which needs them for IPC. They cannot run
//! inside the archived page: the iframe has `sandbox` with no `allow-scripts`,
//! the archived document carries `script-src 'none'`, and the sanitizer removed
//! every script element before the file was written.

use std::cell::RefCell;
use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;
use std::path::Path;
use std::rc::Rc;

use url::Url;
use webview2_com::Microsoft::Web::WebView2::Win32::*;
use webview2_com::{
    CoTaskMemPWSTR, CreateCoreWebView2ControllerCompletedHandler,
    CreateCoreWebView2EnvironmentCompletedHandler, DownloadStartingEventHandler,
    NavigationCompletedEventHandler, NavigationStartingEventHandler, NewWindowRequestedEventHandler,
    PermissionRequestedEventHandler, ProcessFailedEventHandler, TrySuspendCompletedHandler,
    WebMessageReceivedEventHandler,
};
use windows::core::{w, BOOL, Interface, PCWSTR, PWSTR};
use windows::Win32::Foundation::{HWND, RECT};

use crate::logging;
use crate::model::{UiCommand, UiEvent};
use crate::notify::{CommandInbox, Notifier};
use crate::paths::Paths;

pub const CONTENT_HOST: &str = "win-rere-content.example";
pub const CONTENT_ORIGIN: &str = "https://win-rere-content.example";

/// Identifies one content navigation so its completion can be matched.
type ContentNavigation = Rc<RefCell<Option<(u64, String)>>>;
type ActiveId = Rc<RefCell<Option<String>>>;

pub struct WebView {
    #[allow(dead_code)]
    environment: ICoreWebView2Environment,
    controller: ICoreWebView2Controller,
    core: ICoreWebView2,
    webview3: ICoreWebView2_3,
    memory: Option<ICoreWebView2_19>,
}

impl WebView {
    pub fn create(
        hwnd: HWND,
        paths: &Paths,
        active: ActiveId,
        notifier: &Notifier,
        inbox: &CommandInbox,
    ) -> Result<Self, String> {
        let environment = create_environment(&paths.webview_dir)?;
        let controller = create_controller(&environment, hwnd)?;
        // SAFETY: called on the UI thread that owns the controller.
        let core = unsafe { controller.CoreWebView2() }
            .map_err(|error| format!("WebView2 did not expose its core: {error}"))?;
        let webview3: ICoreWebView2_3 = core
            .cast()
            .map_err(|error| format!("WebView2 is too old for virtual host mapping: {error}"))?;
        let memory: Option<ICoreWebView2_19> = core.cast().ok();

        let version = browser_version(&environment);
        logging::line(&format!("WebView2 runtime {version}"));

        configure(&core, &controller)?;
        install_handlers(&core, active, notifier, inbox)?;

        if let Ok(controller2) = controller.cast::<ICoreWebView2Controller2>() {
            // SAFETY: plain property write on the controller.
            let _ = unsafe {
                controller2.SetDefaultBackgroundColor(COREWEBVIEW2_COLOR {
                    A: 255,
                    R: 16,
                    G: 16,
                    B: 18,
                })
            };
        }

        Ok(Self {
            environment,
            controller,
            core,
            webview3,
            memory,
        })
    }

    pub fn navigate_ui(&self, html: &str) -> Result<(), String> {
        let preview: String = html.chars().take(60).collect();
        logging::line(&format!(
            "navigating UI: {} chars, starts {preview:?}",
            html.chars().count()
        ));
        let owned = CoTaskMemPWSTR::from(html);
        // SAFETY: the COM-allocated string lives until after the call returns.
        unsafe { self.core.NavigateToString(*owned.as_ref().as_pcwstr()) }
            .map_err(|error| format!("Could not load the widget interface: {error}"))
    }

    pub fn focus(&self) {
        // SAFETY: moving focus inside this webview.
        let _ = unsafe {
            self.controller
                .MoveFocus(COREWEBVIEW2_MOVE_FOCUS_REASON_PROGRAMMATIC)
        };
    }

    pub fn set_bounds(&self, rect: RECT) {
        // SAFETY: plain bounds property on the controller.
        let _ = unsafe { self.controller.SetBounds(rect) };
    }

    pub fn set_visible(&self, visible: bool) {
        // SAFETY: plain visibility property on the controller.
        let _ = unsafe { self.controller.SetIsVisible(visible) };
    }

    /// Points the content host at one archive folder.
    ///
    /// The mapping is changed only when a different page is opened. Because the
    /// iframe URL carries `?v=<id>`, changing the mapping also forces a reload.
    pub fn show_archive(&self, id: &str, folder: &Path) -> Result<(), String> {
        if !folder.is_dir() {
            return Err("The offline copy is missing from disk.".to_string());
        }
        if !crate::state::valid_id(id) {
            return Err("That bookmark id is not valid.".to_string());
        }
        logging::line(&format!(
            "serving bookmark {id} from {}",
            folder.display()
        ));
        let folder = wide_os(folder.as_os_str());
        // SAFETY: strings live until the call returns; the COM method copies them.
        unsafe {
            let _ = self
                .webview3
                .ClearVirtualHostNameToFolderMapping(w!("win-rere-content.example"));
            self.webview3
                .SetVirtualHostNameToFolderMapping(
                    w!("win-rere-content.example"),
                    PCWSTR(folder.as_ptr()),
                    COREWEBVIEW2_HOST_RESOURCE_ACCESS_KIND_ALLOW,
                )
                .map_err(|error| format!("Could not serve the offline copy: {error}"))?;
        }
        Ok(())
    }

    pub fn clear_archive_mapping(&self) {
        // SAFETY: the mapping name is a static string.
        let _ = unsafe {
            self.webview3
                .ClearVirtualHostNameToFolderMapping(w!("win-rere-content.example"))
        };
    }

    /// Lowers the memory target while the window is minimized. Best effort: an
    /// older runtime simply ignores the cast.
    pub fn set_memory_low(&self, low: bool) {
        let Some(memory) = self.memory.as_ref() else {
            return;
        };
        let level = if low {
            COREWEBVIEW2_MEMORY_USAGE_TARGET_LEVEL_LOW
        } else {
            COREWEBVIEW2_MEMORY_USAGE_TARGET_LEVEL_NORMAL
        };
        // SAFETY: plain property write on the core webview.
        let _ = unsafe { memory.SetMemoryUsageTargetLevel(level) };
    }

    pub fn suspend(&self) {
        let handler = TrySuspendCompletedHandler::create(Box::new(|_result, _success| Ok(())));
        // SAFETY: the controller is invisible when this is called.
        let _ = unsafe { self.webview3.TrySuspend(&handler) };
    }

    pub fn resume(&self) {
        // SAFETY: plain method on the core webview.
        let _ = unsafe { self.webview3.Resume() };
    }

    pub fn post_event(&self, event: &UiEvent) {
        let Ok(json) = serde_json::to_string(event) else {
            logging::line("could not encode a UI event");
            return;
        };
        let script = format!("window.__winRere&&window.__winRere.onEvent({json});");
        self.execute(&script);
    }

    fn execute(&self, script: &str) {
        let wide = wide(script);
        // SAFETY: the script pointer is valid for the call.
        let _ = unsafe {
            self.core.ExecuteScript(
                PCWSTR(wide.as_ptr()),
                None::<&ICoreWebView2ExecuteScriptCompletedHandler>,
            )
        };
    }

    pub fn close(&self) {
        // SAFETY: closing this webview's controller; the COM object stays alive
        // until the app is dropped.
        let _ = unsafe { self.controller.Close() };
    }
}

fn create_environment(folder: &Path) -> Result<ICoreWebView2Environment, String> {
    let folder = wide_os(folder.as_os_str());
    let slot: Rc<RefCell<Option<ICoreWebView2Environment>>> = Rc::new(RefCell::new(None));
    let slot_callback = slot.clone();

    CreateCoreWebView2EnvironmentCompletedHandler::wait_for_async_operation(
        Box::new(move |handler| {
            // SAFETY: the user data folder pointer lives until the call returns.
            unsafe {
                CreateCoreWebView2EnvironmentWithOptions(
                    PCWSTR::null(),
                    PCWSTR(folder.as_ptr()),
                    None::<&ICoreWebView2EnvironmentOptions>,
                    &handler,
                )
                .map_err(webview2_com::Error::WindowsError)
            }
        }),
        Box::new(move |result, environment| {
            result?;
            *slot_callback.borrow_mut() = environment;
            Ok(())
        }),
    )
    .map_err(|error| {
        format!(
            "Could not start WebView2 ({error:?}). Install the Microsoft Edge WebView2 Runtime and try again."
        )
    })?;

    slot.borrow_mut()
        .take()
        .ok_or_else(|| "WebView2 did not return an environment.".to_string())
}

fn create_controller(
    environment: &ICoreWebView2Environment,
    hwnd: HWND,
) -> Result<ICoreWebView2Controller, String> {
    let slot: Rc<RefCell<Option<ICoreWebView2Controller>>> = Rc::new(RefCell::new(None));
    let slot_callback = slot.clone();
    let environment_for_call = environment.clone();

    CreateCoreWebView2ControllerCompletedHandler::wait_for_async_operation(
        Box::new(move |handler| {
            // SAFETY: the parent window belongs to this thread.
            unsafe {
                environment_for_call
                    .CreateCoreWebView2Controller(hwnd, &handler)
                    .map_err(webview2_com::Error::WindowsError)
            }
        }),
        Box::new(move |result, controller| {
            result?;
            *slot_callback.borrow_mut() = controller;
            Ok(())
        }),
    )
    .map_err(|error| format!("Could not create the WebView2 view ({error:?})."))?;

    slot.borrow_mut()
        .take()
        .ok_or_else(|| "WebView2 did not return a controller.".to_string())
}

fn browser_version(environment: &ICoreWebView2Environment) -> String {
    let mut value = PWSTR::null();
    // SAFETY: the environment owns the returned COM string.
    if unsafe { environment.BrowserVersionString(&mut value) }.is_err() {
        return "unknown".to_string();
    }
    CoTaskMemPWSTR::from(value).to_string()
}

fn configure(core: &ICoreWebView2, controller: &ICoreWebView2Controller) -> Result<(), String> {
    // SAFETY: every call is a property write on this webview.
    unsafe {
        let settings = core
            .Settings()
            .map_err(|error| format!("Could not read WebView2 settings: {error}"))?;
        let _ = settings.SetAreDefaultContextMenusEnabled(false);
        let _ = settings.SetAreDevToolsEnabled(false);
        let _ = settings.SetIsStatusBarEnabled(false);
        let _ = settings.SetIsZoomControlEnabled(false);
        let _ = settings.SetIsBuiltInErrorPageEnabled(false);
        let _ = settings.SetAreHostObjectsAllowed(false);
        let _ = settings.SetIsWebMessageEnabled(true);
        let _ = settings.SetIsScriptEnabled(true);

        if let Ok(settings3) = settings.cast::<ICoreWebView2Settings3>() {
            let _ = settings3.SetAreBrowserAcceleratorKeysEnabled(false);
        }
        if let Ok(settings4) = settings.cast::<ICoreWebView2Settings4>() {
            let _ = settings4.SetIsGeneralAutofillEnabled(false);
            let _ = settings4.SetIsPasswordAutosaveEnabled(false);
        }
        if let Ok(settings6) = settings.cast::<ICoreWebView2Settings6>() {
            let _ = settings6.SetIsSwipeNavigationEnabled(false);
        }
        let _ = controller.SetZoomFactor(1.0);
    }
    Ok(())
}

fn make_start_handler(
    active: ActiveId,
    notifier: Notifier,
    tracking: ContentNavigation,
    allow_ui_data: bool,
) -> ICoreWebView2NavigationStartingEventHandler {
    NavigationStartingEventHandler::create(Box::new(move |_sender, args| {
        let Some(args) = args else {
            return Ok(());
        };
        let uri = navigation_uri(&args);
        let preview: String = uri.chars().take(120).collect();
        let current = active.borrow().clone();
        // `NavigateToString` is implemented by WebView2 as a navigation to a
        // base64 `data:text/html` URL. It is the widget itself, and only
        // happens while no archive is open.
        if allow_ui_data && current.is_none() && uri.starts_with("data:text/html") {
            return Ok(());
        }
        if let Some(id) = content_navigation_id(&uri) {
            if current.as_deref() == Some(id.as_str()) {
                let mut navigation_id = 0u64;
                let _ = unsafe { args.NavigationId(&mut navigation_id) };
                *tracking.borrow_mut() = Some((navigation_id, id.clone()));
                logging::line(&format!("content navigation started for {id}"));
                return Ok(());
            }
        } else if is_blank(&uri) {
            return Ok(());
        }

        let _ = unsafe { args.SetCancel(true) };
        if uri.starts_with("http://") || uri.starts_with("https://") {
            logging::line(&format!("blocked navigation: {preview}"));
            notifier.send(UiEvent::LinkBlocked);
        }
        Ok(())
    }))
}

fn make_complete_handler(
    active: ActiveId,
    notifier: Notifier,
    tracking: ContentNavigation,
    top_level: bool,
) -> ICoreWebView2NavigationCompletedEventHandler {
    NavigationCompletedEventHandler::create(Box::new(move |_sender, args| {
        let Some(args) = args else {
            return Ok(());
        };
        let mut navigation_id = 0u64;
        if unsafe { args.NavigationId(&mut navigation_id) }.is_err() {
            return Ok(());
        }
        let tracked = tracking.borrow().clone();
        let Some((expected_id, bookmark_id)) = tracked else {
            if top_level {
                logging::line("widget interface loaded");
            }
            return Ok(());
        };
        if expected_id != navigation_id {
            return Ok(());
        }
        if active.borrow().as_deref() != Some(bookmark_id.as_str()) {
            return Ok(());
        }
        let mut success = BOOL(0);
        let _ = unsafe { args.IsSuccess(&mut success) };
        if success.as_bool() {
            logging::line(&format!("content displayed for {bookmark_id}"));
            notifier.send(UiEvent::Opened { id: bookmark_id });
        } else {
            let mut status = COREWEBVIEW2_WEB_ERROR_STATUS_UNKNOWN;
            let _ = unsafe { args.WebErrorStatus(&mut status) };
            logging::line(&format!(
                "content navigation failed for {bookmark_id}: {status:?}"
            ));
            notifier.send(UiEvent::OpenError {
                message: "The offline copy could not be displayed.".to_string(),
            });
        }
        Ok(())
    }))
}

fn install_handlers(
    core: &ICoreWebView2,
    active: ActiveId,
    notifier: &Notifier,
    inbox: &CommandInbox,
) -> Result<(), String> {
    let content_navigation: ContentNavigation = Rc::new(RefCell::new(None));
    let mut token = 0i64;

    let navigation_started = make_start_handler(
        active.clone(),
        notifier.clone(),
        content_navigation.clone(),
        true,
    );
    // SAFETY: registering a local COM handler; WebView2 keeps its own reference.
    unsafe { core.add_NavigationStarting(&navigation_started, &mut token) }
        .map_err(|error| format!("Could not watch navigation: {error}"))?;

    // WebView2 reports iframe navigations through the frame variants of the
    // same events, not through the top-level ones.
    let frame_started = make_start_handler(
        active.clone(),
        notifier.clone(),
        content_navigation.clone(),
        false,
    );
    // SAFETY: see above.
    unsafe { core.add_FrameNavigationStarting(&frame_started, &mut token) }
        .map_err(|error| format!("Could not watch frame navigation: {error}"))?;

    let navigation_completed = make_complete_handler(
        active.clone(),
        notifier.clone(),
        content_navigation.clone(),
        true,
    );
    // SAFETY: see above.
    unsafe { core.add_NavigationCompleted(&navigation_completed, &mut token) }
        .map_err(|error| format!("Could not watch navigation: {error}"))?;

    let frame_completed = make_complete_handler(
        active.clone(),
        notifier.clone(),
        content_navigation.clone(),
        false,
    );
    // SAFETY: see above.
    unsafe { core.add_FrameNavigationCompleted(&frame_completed, &mut token) }
        .map_err(|error| format!("Could not watch frame navigation: {error}"))?;

    let new_window = {
        let notifier = notifier.clone();
        NewWindowRequestedEventHandler::create(Box::new(move |_sender, args| {
            if let Some(args) = args {
                let _ = unsafe { args.SetHandled(true) };
                let mut value = PWSTR::null();
                if unsafe { args.Uri(&mut value) }.is_ok() {
                    let uri = CoTaskMemPWSTR::from(value).to_string();
                    if uri.starts_with("http://") || uri.starts_with("https://") {
                        notifier.send(UiEvent::LinkBlocked);
                    }
                }
            }
            Ok(())
        }))
    };
    // SAFETY: see above.
    unsafe { core.add_NewWindowRequested(&new_window, &mut token) }
        .map_err(|error| format!("Could not block popups: {error}"))?;

    let permission = PermissionRequestedEventHandler::create(Box::new(move |_sender, args| {
        if let Some(args) = args {
            let _ = unsafe { args.SetState(COREWEBVIEW2_PERMISSION_STATE_DENY) };
        }
        Ok(())
    }));
    // SAFETY: see above.
    unsafe { core.add_PermissionRequested(&permission, &mut token) }
        .map_err(|error| format!("Could not configure permissions: {error}"))?;

    let messages = {
        let inbox = inbox.clone();
        WebMessageReceivedEventHandler::create(Box::new(move |_sender, args| {
            let Some(args) = args else {
                return Ok(());
            };
            let mut json = PWSTR::null();
            if unsafe { args.WebMessageAsJson(&mut json) }.is_err() {
                return Ok(());
            }
            let text = CoTaskMemPWSTR::from(json).to_string();
            match serde_json::from_str::<UiCommand>(&text) {
                Ok(command) => inbox.push(command),
                Err(error) => logging::line(&format!("ignored a UI message: {error}")),
            }
            Ok(())
        }))
    };
    // SAFETY: see above.
    unsafe { core.add_WebMessageReceived(&messages, &mut token) }
        .map_err(|error| format!("Could not receive UI messages: {error}"))?;

    if let Ok(core4) = core.cast::<ICoreWebView2_4>() {
        let downloads = DownloadStartingEventHandler::create(Box::new(move |_sender, args| {
            if let Some(args) = args {
                let _ = unsafe { args.SetCancel(true) };
            }
            Ok(())
        }));
        // SAFETY: see above.
        let _ = unsafe { core4.add_DownloadStarting(&downloads, &mut token) };
    }

    let process = ProcessFailedEventHandler::create(Box::new(move |_sender, args| {
        if let Some(args) = args {
            let mut kind = COREWEBVIEW2_PROCESS_FAILED_KIND_BROWSER_PROCESS_EXITED;
            let _ = unsafe { args.ProcessFailedKind(&mut kind) };
            logging::line(&format!("WebView2 process failed: {kind:?}"));
        }
        Ok(())
    }));
    // SAFETY: see above.
    unsafe { core.add_ProcessFailed(&process, &mut token) }
        .map_err(|error| format!("Could not watch the WebView2 process: {error}"))?;

    Ok(())
}

fn navigation_uri(args: &ICoreWebView2NavigationStartingEventArgs) -> String {
    let mut value = PWSTR::null();
    // SAFETY: the arguments own the returned COM string.
    if unsafe { args.Uri(&mut value) }.is_err() {
        return String::new();
    }
    CoTaskMemPWSTR::from(value).to_string()
}

fn is_blank(uri: &str) -> bool {
    uri.is_empty() || uri == "about:blank" || uri.starts_with("about:")
}

/// Returns the bookmark id carried by a content navigation, if the URL really
/// points at the current offline page.
fn content_navigation_id(uri: &str) -> Option<String> {
    let url = Url::parse(uri).ok()?;
    if url.scheme() != "https" || url.host_str() != Some(CONTENT_HOST) {
        return None;
    }
    if url.path() != "/index.html" {
        return None;
    }
    url.query_pairs().find_map(|(key, value)| {
        if key == "v" {
            let value = value.into_owned();
            if crate::state::valid_id(&value) {
                Some(value)
            } else {
                None
            }
        } else {
            None
        }
    })
}

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

fn wide_os(value: &OsStr) -> Vec<u16> {
    value.encode_wide().chain(std::iter::once(0)).collect()
}

/// Kept for the content URL built by the app.
pub fn content_url(id: &str) -> String {
    format!("{CONTENT_ORIGIN}/index.html?v={id}")
}
