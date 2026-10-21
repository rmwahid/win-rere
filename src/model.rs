//! Data shared between the native side and the web UI.

use serde::{Deserialize, Serialize};

/// One saved page.
///
/// Everything except `id`, `url`, `final_url`, `saved_at` and the numbers is
/// untrusted text taken from the page. The UI only ever inserts it with
/// `textContent`, never as HTML.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Bookmark {
    pub id: String,
    pub url: String,
    pub final_url: String,
    pub title: String,
    pub host: String,
    #[serde(default)]
    pub description: String,
    pub saved_at: String,
    #[serde(default)]
    pub bytes: u64,
    #[serde(default)]
    pub assets: u32,
    /// True when the archive also contains a separately downloaded mobile page.
    #[serde(default)]
    pub has_mobile: bool,
    /// True when the page was fetched over plain HTTP.
    #[serde(default)]
    pub insecure: bool,
}

impl Bookmark {
    /// The id is the folder name, so it must be a plain UUID in simple form.
    pub fn has_valid_id(&self) -> bool {
        self.id.len() == 32
            && self
                .id
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    }
}

/// Window placement and the last page that was open.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WindowState {
    /// Bumped when a stored placement needs to be migrated. Version 3 is the
    /// minimum-width default and knows about the sidebar preference. Version 4
    /// adds the mobile reading preference.
    #[serde(default)]
    pub version: u32,
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
    pub topmost: bool,
    #[serde(default)]
    pub last_open: Option<String>,
    #[serde(default = "default_true")]
    pub sidebar_visible: bool,
    /// The reader constrains the archived page to a phone-width viewport.
    #[serde(default)]
    pub mobile_view: bool,
}

fn default_true() -> bool {
    true
}

impl Default for WindowState {
    fn default() -> Self {
        Self {
            version: 4,
            // A sentinel the window code replaces with the top right corner of
            // the work area on the first run.
            x: i32::MIN,
            y: i32::MIN,
            // The minimum width: 720 logical of WebView plus the 8 px resize
            // border on each side. The height was chosen by the user.
            width: 736,
            height: 640,
            topmost: true,
            last_open: None,
            sidebar_visible: true,
            mobile_view: false,
        }
    }
}

/// One message from the UI, decoded from JSON.
#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum UiCommand {
    Ready,
    Save { url: String },
    Open { id: String },
    Delete { id: String },
    Rename { id: String, title: String },
    Sidebar { visible: bool },
    MobileView { value: bool },
    OpenExternal { id: String },
    Drag,
    Pin { value: bool },
    Minimize,
    Close,
}

/// One message to the UI, serialized to JSON and handed to `onEvent`.
#[derive(Clone, Debug, Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum UiEvent {
    Library {
        items: Vec<Bookmark>,
        topmost: bool,
        active: Option<String>,
        mobile_view: bool,
    },
    SaveStarted {
        url: String,
    },
    SaveProgress {
        message: String,
    },
    SaveDone {
        item: Bookmark,
    },
    SaveError {
        message: String,
    },
    Opening {
        id: String,
        url: String,
    },
    Opened {
        id: String,
    },
    OpenError {
        message: String,
    },
    Cleared,
    Deleted {
        id: String,
    },
    DeleteError {
        message: String,
    },
    Renamed {
        id: String,
        title: String,
    },
    RenameError {
        message: String,
    },
    LinkBlocked,
    Pinned {
        value: bool,
    },
    Status {
        message: String,
    },
}
