//! Downloads one page and writes a complete offline archive folder.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use url::Url;

use crate::fetch::{self, Fetcher};
use crate::logging;
use crate::model::Bookmark;
use crate::paths::Paths;
use crate::sanitize::{self, Progress};
use crate::security;

const MAX_HTML_BYTES: u64 = 5 * 1024 * 1024;
const JOB_DEADLINE: Duration = Duration::from_secs(120);

/// Downloads, sanitizes and stores one page. The folder only becomes visible
/// under its final id at the end; a failure removes everything.
pub fn save(paths: &Paths, input: &str, progress: Progress) -> Result<Bookmark, String> {
    let url = security::normalize_input(input)?;
    let id = uuid::Uuid::new_v4().simple().to_string();
    let temporary = paths.bookmarks_dir.join(format!(".tmp-{id}"));
    fs::create_dir_all(&temporary)
        .map_err(|error| format!("Could not create the archive folder: {error}"))?;
    let mut archive = TempArchive {
        path: temporary.clone(),
        keep: false,
    };

    let fetcher = Fetcher::new(Instant::now() + JOB_DEADLINE)?;
    progress("Downloading page".to_string());
    let fetched = fetcher.get(
        &url,
        MAX_HTML_BYTES,
        "text/html,application/xhtml+xml;q=0.9,*/*;q=0.1",
    )?;

    let media = fetch::media_type(&fetched.content_type);
    if !matches!(media.as_str(), "text/html" | "application/xhtml+xml") {
        return Err("That URL did not return an HTML page.".to_string());
    }
    if fetched.bytes.is_empty() {
        return Err("The page was empty.".to_string());
    }

    let final_url = fetched.final_url.clone();
    security::validate_url(&final_url)?;
    let host = final_url.host_str().unwrap_or("this page").to_string();
    let decoded = sanitize::decode_bytes(&fetched.bytes, &fetched.content_type);

    progress("Preparing offline copy".to_string());
    let rewritten = sanitize::rewrite_html(
        &decoded,
        &final_url,
        &temporary,
        &fetcher,
        progress.clone(),
    )?;

    let title = if rewritten.title.is_empty() {
        fallback_title(&final_url)
    } else {
        rewritten.title
    };
    let description = if rewritten.description.is_empty() {
        format!("Offline copy of {host}")
    } else {
        rewritten.description
    };

    progress("Writing archive".to_string());
    let html_bytes = rewritten.html.as_bytes().len() as u64;
    fs::write(temporary.join("index.html"), rewritten.html.as_bytes())
        .map_err(|error| format!("Could not write the offline page: {error}"))?;
    let assets_bytes = folder_size(&temporary.join("assets"))?;

    let bookmark = Bookmark {
        id: id.clone(),
        url: url.to_string(),
        final_url: final_url.to_string(),
        title: truncate(&title, 300),
        host: truncate(&host, 255),
        description: truncate(&description, 600),
        saved_at: logging::now_iso8601(),
        bytes: html_bytes + assets_bytes,
        assets: rewritten.assets,
        insecure: final_url.scheme() == "http",
    };

    let meta = serde_json::to_vec_pretty(&bookmark)
        .map_err(|error| format!("Could not encode the archive metadata: {error}"))?;

    let destination = paths.archive_dir(&id);
    if destination.exists() {
        return Err("An archive with this id already exists.".to_string());
    }
    fs::rename(&temporary, &destination)
        .map_err(|error| format!("Could not finish the archive: {error}"))?;
    archive.keep = true;

    let meta_path = paths.meta_file(&id);
    if let Err(error) = fs::write(&meta_path, meta) {
        let _ = fs::remove_file(&meta_path);
        let _ = fs::remove_dir_all(&destination);
        return Err(format!("Could not write the archive metadata: {error}"));
    }

    logging::line(&format!(
        "saved {} as {} ({} assets, {})",
        final_url,
        id,
        bookmark.assets,
        fetch::human_size(bookmark.bytes)
    ));
    Ok(bookmark)
}

struct TempArchive {
    path: PathBuf,
    keep: bool,
}

impl Drop for TempArchive {
    fn drop(&mut self) {
        if !self.keep {
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}

fn fallback_title(url: &Url) -> String {
    let host = url.host_str().unwrap_or("Untitled page");
    let path = url.path();
    if path.is_empty() || path == "/" {
        host.to_string()
    } else {
        format!("{host}{path}")
    }
}

fn truncate(value: &str, limit: usize) -> String {
    if value.len() <= limit {
        return value.to_string();
    }
    let mut end = limit;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_string()
}

fn folder_size(path: &Path) -> Result<u64, String> {
    let entries = match fs::read_dir(path) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(format!("Could not inspect the asset folder: {error}")),
    };
    let mut total = 0u64;
    for entry in entries {
        let entry = entry.map_err(|error| format!("Could not inspect the asset folder: {error}"))?;
        if let Ok(metadata) = entry.metadata() {
            if metadata.is_file() {
                total = total.saturating_add(metadata.len());
            }
        }
    }
    Ok(total)
}
