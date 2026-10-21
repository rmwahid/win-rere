//! Where the widget keeps its data.

use std::env;
use std::fs;
use std::path::PathBuf;

/// The two roots the widget uses.
///
/// Bookmarks are user data and live in Roaming AppData. The WebView2 profile is
/// a cache and lives in Local AppData, which keeps it off the roaming profile.
#[derive(Clone, Debug)]
pub struct Paths {
    pub bookmarks_dir: PathBuf,
    pub index_file: PathBuf,
    pub window_file: PathBuf,
    pub webview_dir: PathBuf,
    pub log_file: PathBuf,
}

impl Paths {
    pub fn discover() -> Result<Self, String> {
        let roaming = env::var_os("APPDATA")
            .map(PathBuf::from)
            .ok_or("APPDATA is not set, so there is nowhere to keep bookmarks.")?;
        let local = env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|| roaming.clone());

        let data_dir = roaming.join("win-rere");
        let bookmarks_dir = data_dir.join("bookmarks");
        let local_dir = local.join("win-rere");
        let webview_dir = local_dir.join("WebView2");
        let log_file = local_dir.join("win-rere.log");

        fs::create_dir_all(&bookmarks_dir)
            .map_err(|error| format!("Could not create the bookmark folder: {error}"))?;
        fs::create_dir_all(&webview_dir)
            .map_err(|error| format!("Could not create the WebView2 profile folder: {error}"))?;

        Ok(Self {
            index_file: data_dir.join("bookmarks.json"),
            window_file: data_dir.join("window.json"),
            bookmarks_dir,
            webview_dir,
            log_file,
        })
    }

    /// The folder holding one archive.
    pub fn archive_dir(&self, id: &str) -> PathBuf {
        self.bookmarks_dir.join(id)
    }

    /// The file a `https://win-rere-content.example` URL resolves to.
    pub fn archive_html(&self, id: &str) -> PathBuf {
        self.archive_dir(id).join("index.html")
    }

    /// The mobile document captured with a mobile user agent, when available.
    pub fn archive_mobile(&self, id: &str) -> PathBuf {
        self.archive_dir(id).join("mobile.html")
    }

    /// Metadata lives beside the archive folder, never inside it. The folder is
    /// handed to WebView2 as the content root, so nothing but the page and its
    /// assets may live there.
    pub fn meta_file(&self, id: &str) -> PathBuf {
        self.bookmarks_dir.join(format!("{id}.json"))
    }

    /// A marker written while a multi-file bookmark change is in progress.
    /// It sits in the bookmarks root, not in any mapped archive folder.
    pub fn dirty_file(&self) -> PathBuf {
        self.bookmarks_dir.join(".dirty")
    }
}
