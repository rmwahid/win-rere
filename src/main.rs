#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod archive;
mod fetch;
mod logging;
mod model;
mod notify;
mod paths;
mod sanitize;
mod security;
mod state;
mod ui;
mod webview;
mod window;

use std::sync::Arc;

use windows::core::PCWSTR;
use windows::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED};
use windows::Win32::UI::HiDpi::{
    SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows::Win32::UI::WindowsAndMessaging::{MessageBoxW, MB_ICONERROR, MB_OK};

fn main() {
    // SAFETY: COM is initialized once, before WebView2 or the shell APIs are
    // used. Nothing else in this process has chosen an apartment yet.
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }

    let paths = match paths::Paths::discover() {
        Ok(paths) => paths,
        Err(message) => {
            show_error(&message);
            return;
        }
    };
    logging::init(paths.log_file.clone());

    let arguments: Vec<String> = std::env::args().collect();
    if arguments.get(1).map(String::as_str) == Some("--save") {
        let Some(url) = arguments.get(2) else {
            eprintln!("usage: win-rere --save <url>");
            return;
        };
        let progress: sanitize::Progress = Arc::new(|message| eprintln!("{message}"));
        match archive::save(&paths, url, progress) {
            Ok(bookmark) => {
                let mut bookmarks = state::load_bookmarks(&paths);
                bookmarks.retain(|item| item.id != bookmark.id);
                bookmarks.push(bookmark.clone());
                bookmarks.sort_by(|a, b| b.saved_at.cmp(&a.saved_at));
                if let Err(error) = state::write_bookmarks(&paths, &bookmarks) {
                    eprintln!("error: {error}");
                    logging::line(&error);
                }
                let mut window = state::read_window_state(&paths);
                window.last_open = Some(bookmark.id.clone());
                state::write_window_state(&paths, &window);
                println!(
                    "saved \"{}\" as {}\n  {}",
                    bookmark.title, bookmark.id, bookmark.final_url
                );
            }
            Err(message) => {
                eprintln!("error: {message}");
                logging::line(&format!("CLI save failed: {message}"));
            }
        }
        return;
    }

    if let Err(message) = app::run(paths) {
        logging::line(&format!("fatal: {message}"));
        show_error(&message);
    }
}

fn show_error(message: &str) {
    let text = window::wide(message);
    let title = window::wide("win-rere");
    // SAFETY: modal error box on this thread with valid strings.
    unsafe {
        let _ = MessageBoxW(
            None,
            PCWSTR(text.as_ptr()),
            PCWSTR(title.as_ptr()),
            MB_OK | MB_ICONERROR,
        );
    }
}
