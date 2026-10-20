//! Cross-thread handoff into the UI thread.
//!
//! WebView2 calls its event handlers on the UI thread, but the archive worker is
//! a plain thread. Both push into a queue and post a private window message; the
//! window procedure drains the queue. No locks are held while doing UI work.

use std::collections::VecDeque;
use std::ffi::c_void;
use std::sync::{Arc, Mutex};

use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{PostMessageW, WM_APP};

use crate::model::{UiCommand, UiEvent};

const WM_APP_COMMAND: u32 = WM_APP + 1;
const WM_APP_EVENT: u32 = WM_APP + 2;

pub const COMMAND_MESSAGE: u32 = WM_APP_COMMAND;
pub const EVENT_MESSAGE: u32 = WM_APP_EVENT;

#[derive(Clone)]
pub struct Notifier {
    hwnd: isize,
    queue: Arc<Mutex<VecDeque<UiEvent>>>,
}

impl Notifier {
    pub fn new(hwnd: HWND) -> Self {
        Self {
            hwnd: hwnd.0 as isize,
            queue: Arc::new(Mutex::new(VecDeque::new())),
        }
    }

    pub fn send(&self, event: UiEvent) {
        if let Ok(mut queue) = self.queue.lock() {
            queue.push_back(event);
        }
        // SAFETY: posting to a window that belongs to this process; the message
        // only carries a wake-up signal, never a pointer.
        unsafe {
            let _ = PostMessageW(
                Some(HWND(self.hwnd as *mut c_void)),
                EVENT_MESSAGE,
                WPARAM(0),
                LPARAM(0),
            );
        }
    }

    pub fn drain(&self) -> Vec<UiEvent> {
        let Ok(mut queue) = self.queue.lock() else {
            return Vec::new();
        };
        queue.drain(..).collect()
    }
}

#[derive(Clone)]
pub struct CommandInbox {
    hwnd: isize,
    queue: Arc<Mutex<VecDeque<UiCommand>>>,
}

impl CommandInbox {
    pub fn new(hwnd: HWND) -> Self {
        Self {
            hwnd: hwnd.0 as isize,
            queue: Arc::new(Mutex::new(VecDeque::new())),
        }
    }

    pub fn push(&self, command: UiCommand) {
        if let Ok(mut queue) = self.queue.lock() {
            queue.push_back(command);
        }
        // SAFETY: see `Notifier::send`.
        unsafe {
            let _ = PostMessageW(
                Some(HWND(self.hwnd as *mut c_void)),
                COMMAND_MESSAGE,
                WPARAM(0),
                LPARAM(0),
            );
        }
    }

    pub fn drain(&self) -> Vec<UiCommand> {
        let Ok(mut queue) = self.queue.lock() else {
            return Vec::new();
        };
        queue.drain(..).collect()
    }
}
