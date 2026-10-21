//! Bookmark index, archive folders and window placement.
//!
//! `bookmarks.json` is a cache, not the only copy of the truth: every archive
//! folder also holds `meta.json`. If the index is lost or damaged, startup
//! scans the folders and rebuilds it. Writes go through a temporary file so a
//! crash never leaves half a JSON document behind.

use std::fs;
use std::io;
use std::path::Path;

use crate::logging;
use crate::model::{Bookmark, WindowState};
use crate::paths::Paths;

/// Reads the index, repairing it from `meta.json` files when needed.
///
/// The index is the fast path: startup reads one file and does not touch the
/// per-bookmark metadata. A dirty marker, a missing/corrupt index, or a count
/// mismatch triggers the metadata scan instead.
pub fn load_bookmarks(paths: &Paths) -> Vec<Bookmark> {
    let mut repaired = false;
    let mut bookmarks = match fs::read_to_string(&paths.index_file) {
        Ok(text) => match serde_json::from_str::<Vec<Bookmark>>(&text) {
            Ok(items) => items,
            Err(error) => {
                logging::line(&format!("bookmark index is unreadable: {error}"));
                repaired = true;
                Vec::new()
            }
        },
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            repaired = true;
            Vec::new()
        }
        Err(error) => {
            logging::line(&format!("bookmark index could not be read: {error}"));
            repaired = true;
            Vec::new()
        }
    };

    let index_count = bookmarks.len();
    let dirty = is_dirty(paths);
    let count_mismatch = count_meta_files(paths).is_some_and(|count| count != index_count);
    let scan_meta = repaired || dirty || count_mismatch;

    if scan_meta {
        // Metadata files are the durable copy. Merge them only when the fast
        // path cannot be trusted.
        for candidate in scan_meta_files(paths) {
            if let Some(existing) = bookmarks.iter_mut().find(|bookmark| bookmark.id == candidate.id)
            {
                if *existing != candidate {
                    *existing = candidate;
                }
            } else {
                bookmarks.push(candidate);
            }
        }
        repaired = true;
    } else {
        logging::line(&format!(
            "bookmark index fast path ({} items)",
            bookmarks.len()
        ));
    }

    let before = bookmarks.len();
    bookmarks.retain_mut(|bookmark| normalize(bookmark, paths, scan_meta));
    bookmarks.sort_by(|a, b| b.saved_at.cmp(&a.saved_at).then_with(|| a.title.cmp(&b.title)));
    bookmarks.dedup_by(|a, b| a.id == b.id);
    if bookmarks.len() != before {
        repaired = true;
    }

    if repaired {
        match write_bookmarks(paths, &bookmarks) {
            Ok(()) => clear_dirty(paths),
            Err(error) => logging::line(&format!("could not rewrite the bookmark index: {error}")),
        }
    }
    bookmarks
}

pub fn write_bookmarks(paths: &Paths, bookmarks: &[Bookmark]) -> Result<(), String> {
    // Compact JSON: this file is machine-read, and a smaller index parses faster.
    let bytes = serde_json::to_vec(bookmarks)
        .map_err(|error| format!("could not encode the bookmark index: {error}"))?;
    write_atomic(&paths.index_file, &bytes)
}

/// Removes one archive folder.
pub fn delete_archive(paths: &Paths, id: &str) -> Result<(), String> {
    if !valid_id(id) {
        return Err("That bookmark id is not valid.".to_string());
    }
    let directory = paths.archive_dir(id);
    if directory.exists() {
        fs::remove_dir_all(&directory)
            .map_err(|error| format!("Could not delete the offline copy: {error}"))?;
    }
    let metadata = paths.meta_file(id);
    if metadata.exists() {
        let _ = fs::remove_file(metadata);
    }
    Ok(())
}

/// Removes half-written archives left behind by a crash.
pub fn cleanup_temp(paths: &Paths) {
    for stale in [
        paths.index_file.with_extension("tmp"),
        paths.window_file.with_extension("tmp"),
    ] {
        let _ = fs::remove_file(stale);
    }
    let Ok(entries) = fs::read_dir(&paths.bookmarks_dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with(".tmp-") {
            let path = entry.path();
            if path.is_dir() {
                let _ = fs::remove_dir_all(&path);
            }
        }
    }
}

pub fn read_window_state(paths: &Paths) -> WindowState {
    let Ok(text) = fs::read_to_string(&paths.window_file) else {
        return WindowState::default();
    };
    let Ok(mut state) = serde_json::from_str::<WindowState>(&text) else {
        logging::line("window state is unreadable; using the default placement");
        return WindowState::default();
    };
    let defaults = WindowState::default();
    // Version 2 was the 920-wide default. The current default is the minimum
    // width; keep the height the user already has.
    if state.version < 2 {
        state.width = defaults.width;
        state.height = defaults.height;
        state.version = 3;
    } else if state.version < 3 {
        state.width = defaults.width;
        state.version = 3;
    }
    if state.width < 320 || state.height < 240 {
        state.width = defaults.width;
        state.height = defaults.height;
    }
    if state.version < 4 {
        // Older files do not have the mobile preference, which serde already
        // defaulted to off.
        state.version = 4;
    }
    state
}

pub fn write_window_state(paths: &Paths, state: &WindowState) {
    let Ok(bytes) = serde_json::to_vec_pretty(state) else {
        return;
    };
    if let Err(error) = write_atomic(&paths.window_file, &bytes) {
        logging::line(&format!("could not save the window state: {error}"));
    }
}

fn count_meta_files(paths: &Paths) -> Option<usize> {
    let entries = fs::read_dir(&paths.bookmarks_dir).ok()?;
    Some(
        entries
            .flatten()
            .filter(|entry| {
                entry.path().extension().and_then(|value| value.to_str()) == Some("json")
            })
            .count(),
    )
}

fn is_dirty(paths: &Paths) -> bool {
    paths.dirty_file().is_file()
}

/// Marks a multi-file bookmark change in progress. If the process dies before
/// the index is written, the next startup scans the metadata instead of
/// trusting a stale index.
pub fn mark_dirty(paths: &Paths) {
    if let Err(error) = fs::write(paths.dirty_file(), b"1") {
        logging::line(&format!("could not write the dirty marker: {error}"));
    }
}

pub fn clear_dirty(paths: &Paths) {
    let _ = fs::remove_file(paths.dirty_file());
}

fn scan_meta_files(paths: &Paths) -> Vec<Bookmark> {
    let Ok(entries) = fs::read_dir(&paths.bookmarks_dir) else {
        return Vec::new();
    };
    let mut bookmarks = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|value| value.to_str()) != Some("json") {
            continue;
        }
        let Ok(text) = fs::read_to_string(&path) else {
            continue;
        };
        let Ok(mut bookmark) = serde_json::from_str::<Bookmark>(&text) else {
            logging::line(&format!("ignoring unreadable metadata at {}", path.display()));
            continue;
        };
        if normalize(&mut bookmark, paths, true) {
            bookmarks.push(bookmark);
        }
    }
    logging::line(&format!(
        "scanned {} metadata file(s)",
        bookmarks.len()
    ));
    bookmarks
}

/// Trims untrusted metadata to something safe to display. Archive folder checks
/// are only done on the recovery path; the fast path trusts the index and lets
/// opening a page report a missing archive.
fn normalize(bookmark: &mut Bookmark, paths: &Paths, check_archive: bool) -> bool {
    if !bookmark.has_valid_id() {
        return false;
    }
    if check_archive && !paths.archive_html(&bookmark.id).is_file() {
        return false;
    }
    if check_archive && bookmark.has_mobile && !paths.archive_mobile(&bookmark.id).is_file() {
        bookmark.has_mobile = false;
    }
    let url_ok = url::Url::parse(&bookmark.url)
        .map(|url| matches!(url.scheme(), "http" | "https"))
        .unwrap_or(false);
    if !url_ok {
        return false;
    }
    if url::Url::parse(&bookmark.final_url).is_err() {
        bookmark.final_url = bookmark.url.clone();
    }
    bookmark.title = clean_text(&bookmark.title, 300);
    if bookmark.title.is_empty() {
        bookmark.title = clean_text(&bookmark.host, 120);
    }
    if bookmark.title.is_empty() {
        bookmark.title = "Untitled page".to_string();
    }
    bookmark.host = clean_text(&bookmark.host, 255);
    bookmark.description = clean_text(&bookmark.description, 600);
    bookmark.saved_at = clean_text(&bookmark.saved_at, 40);
    if bookmark.saved_at.is_empty() {
        bookmark.saved_at = logging::now_iso8601();
    }
    true
}

/// Cleans a user supplied bookmark title. Titles are the only editable part of
/// a saved bookmark.
pub fn clean_title(value: &str) -> String {
    clean_text(value, 200)
}

fn clean_text(value: &str, limit: usize) -> String {
    let mut out = String::with_capacity(value.len().min(limit));
    let mut last_space = false;
    for character in value.chars() {
        let character = if character.is_control() {
            ' '
        } else {
            character
        };
        if character.is_whitespace() {
            if !last_space && !out.is_empty() {
                out.push(' ');
            }
            last_space = true;
        } else {
            out.push(character);
            last_space = false;
        }
        if out.len() >= limit {
            break;
        }
    }
    out.trim().to_string()
}

pub fn valid_id(id: &str) -> bool {
    id.len() == 32
        && id
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let temporary = path.with_extension("tmp");
    fs::write(&temporary, bytes)
        .map_err(|error| format!("could not write {}: {error}", temporary.display()))?;
    match fs::rename(&temporary, path) {
        Ok(()) => Ok(()),
        Err(_) => {
            // Windows refuses to replace an existing file in some states.
            let _ = fs::remove_file(path);
            fs::rename(&temporary, path)
                .map_err(|error| format!("could not replace {}: {error}", path.display()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_strict_lowercase_hex() {
        assert!(valid_id("0123456789abcdef0123456789abcdef"));
        assert!(!valid_id("0123456789ABCDEF0123456789abcdef"));
        assert!(!valid_id("../0123456789abcdef0123456789ab"));
        assert!(!valid_id("short"));
    }

    #[test]
    fn cleaning_strips_controls_and_limits_length() {
        assert_eq!(clean_text("a\u{0}b\nc", 20), "a b c");
        assert_eq!(clean_text("   lots   of   space   ", 100), "lots of space");
        assert_eq!(clean_text("abcdef", 3), "abc");
    }

    fn sample(id: &str, title: &str) -> Bookmark {
        Bookmark {
            id: id.to_string(),
            url: "https://example.com/".to_string(),
            final_url: "https://example.com/".to_string(),
            title: title.to_string(),
            host: "example.com".to_string(),
            description: String::new(),
            saved_at: "2026-01-01T00:00:00Z".to_string(),
            bytes: 0,
            assets: 0,
            has_mobile: false,
            insecure: false,
        }
    }

    fn temp_paths() -> Paths {
        let root = std::env::temp_dir().join(format!(
            "win-rere-state-{}",
            uuid::Uuid::new_v4().simple()
        ));
        let bookmarks_dir = root.join("bookmarks");
        fs::create_dir_all(&bookmarks_dir).unwrap();
        Paths {
            bookmarks_dir,
            index_file: root.join("bookmarks.json"),
            window_file: root.join("window.json"),
            webview_dir: root.join("WebView2"),
            log_file: root.join("win-rere.log"),
        }
    }

    fn write_archive(paths: &Paths, bookmark: &Bookmark) {
        let directory = paths.archive_dir(&bookmark.id);
        fs::create_dir_all(&directory).unwrap();
        fs::write(directory.join("index.html"), b"<html></html>").unwrap();
        fs::write(
            paths.meta_file(&bookmark.id),
            serde_json::to_vec(bookmark).unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn fast_path_trusts_the_index_without_reading_metadata() {
        let paths = temp_paths();
        let id = "0123456789abcdef0123456789abcdef";
        write_archive(&paths, &sample(id, "Meta title"));
        fs::write(
            &paths.index_file,
            serde_json::to_vec(&[sample(id, "Indexed title")]).unwrap(),
        )
        .unwrap();

        let items = load_bookmarks(&paths);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].title, "Indexed title");
        assert!(!paths.dirty_file().exists());

        let root = paths.index_file.parent().unwrap().to_path_buf();
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn dirty_marker_forces_the_metadata_to_win() {
        let paths = temp_paths();
        let id = "fedcba9876543210fedcba9876543210";
        write_archive(&paths, &sample(id, "From metadata"));
        fs::write(
            &paths.index_file,
            serde_json::to_vec(&[sample(id, "Old index title")]).unwrap(),
        )
        .unwrap();
        mark_dirty(&paths);

        let items = load_bookmarks(&paths);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].title, "From metadata");
        assert!(!paths.dirty_file().exists());

        let root = paths.index_file.parent().unwrap().to_path_buf();
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn mobile_view_is_off_for_older_window_state() {
        let paths = temp_paths();
        fs::write(
            &paths.window_file,
            r#"{"version":3,"x":0,"y":0,"width":736,"height":640,"topmost":true,"sidebar_visible":true}"#,
        )
        .unwrap();

        let state = read_window_state(&paths);
        assert_eq!(state.version, 4);
        assert!(!state.mobile_view);

        let root = paths.window_file.parent().unwrap().to_path_buf();
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn missing_mobile_file_is_dropped_on_recovery() {
        let paths = temp_paths();
        let id = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let mut bookmark = sample(id, "Desktop only after recovery");
        bookmark.has_mobile = true;
        write_archive(&paths, &bookmark);
        mark_dirty(&paths);

        let items = load_bookmarks(&paths);
        assert_eq!(items.len(), 1);
        assert!(!items[0].has_mobile);

        let root = paths.index_file.parent().unwrap().to_path_buf();
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn mobile_view_round_trips_through_the_window_file() {
        let paths = temp_paths();
        let mut state = WindowState::default();
        state.mobile_view = true;
        write_window_state(&paths, &state);

        let loaded = read_window_state(&paths);
        assert_eq!(loaded.version, 4);
        assert!(loaded.mobile_view);

        let root = paths.window_file.parent().unwrap().to_path_buf();
        let _ = fs::remove_dir_all(root);
    }
}
