//! Turns a downloaded page into a self-contained, script-free snapshot.
//!
//! Three things happen here. Dangerous elements and attributes are removed,
//! every URL that would reach the network is either downloaded next to the page
//! or dropped, and a strict CSP is injected. The content is then shown inside a
//! sandboxed iframe, so the sanitizer is one layer of three rather than the only
//! one.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;

use lol_html::html_content::ContentType;
use lol_html::{element, end_tag, text, HtmlRewriter, Settings};
use sha2::{Digest, Sha256};
use url::Url;

use crate::fetch::{self, Fetcher};
use crate::logging;
use crate::security;

/// Upper bounds for one page. They keep a hostile page from filling the disk or
/// keeping the worker busy forever.
const MAX_ASSETS: u32 = 250;
const MAX_ASSET_BYTES: u64 = 8 * 1024 * 1024;
const MAX_TOTAL_BYTES: u64 = 40 * 1024 * 1024;

const TRANSPARENT_GIF: &str =
    "data:image/gif;base64,R0lGODlhAQABAIAAAAAAAP///ywAAAAAAQABAAACAUwAOw==";

const HEAD_HTML: &str = concat!(
    "<meta charset=\"utf-8\">",
    "<meta name=\"referrer\" content=\"no-referrer\">",
    "<meta http-equiv=\"Content-Security-Policy\" content=\"",
    "default-src 'none'; ",
    "img-src https://win-rere-content.example data:; ",
    "style-src 'unsafe-inline' https://win-rere-content.example; ",
    "font-src https://win-rere-content.example data:; ",
    "script-src 'none'; object-src 'none'; connect-src 'none'; ",
    "form-action 'none'; base-uri 'none'; media-src 'none'\">",
    // A saved page should scroll in both directions even if its own stylesheet
    // hid the overflow. This is the one layout property the snapshot overrides.
    "<style>html{overflow:auto!important}body{overflow:visible!important}</style>"
);

/// A progress sink shared with the worker thread.
pub type Progress = Arc<dyn Fn(String) + Send + Sync>;

pub struct HtmlResult {
    pub html: String,
    pub title: String,
    pub description: String,
    pub assets: u32,
}

pub fn rewrite_html(
    html: &str,
    base: &Url,
    root: &Path,
    fetcher: &Fetcher,
    progress: Progress,
) -> Result<HtmlResult, String> {
    fs::create_dir_all(root.join("assets"))
        .map_err(|error| format!("Could not create the asset folder: {error}"))?;

    let store = Rc::new(RefCell::new(AssetStore::new(
        fetcher.clone(),
        root.to_path_buf(),
        progress,
    )));
    let title = Rc::new(RefCell::new(String::new()));
    let og_title = Rc::new(RefCell::new(String::new()));
    let description = Rc::new(RefCell::new(String::new()));
    let style_buffer: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(Vec::new()));
    let head_seen = Rc::new(Cell::new(false));

    let settings = Settings {
        element_content_handlers: vec![
            element!(
                "script, noscript, iframe, frame, frameset, object, embed, applet, template, \
                 video, audio, source, track, portal, fencedframe",
                |el| {
                    el.remove();
                    Ok(())
                }
            ),
            element!("base", |el| {
                el.remove();
                Ok(())
            }),
            element!("meta", {
                let description = description.clone();
                let og_title = og_title.clone();
                move |el| {
                    if attr(el, "http-equiv").is_some() {
                        el.remove();
                        return Ok(());
                    }
                    if attr(el, "charset").is_some() {
                        el.remove();
                        return Ok(());
                    }
                    let key = attr(el, "name")
                        .or_else(|| attr(el, "property"))
                        .unwrap_or_default()
                        .to_ascii_lowercase();
                    if let Some(content) = attr(el, "content") {
                        if (key == "description" || key == "og:description")
                            && description.borrow().is_empty()
                        {
                            *description.borrow_mut() = content;
                        } else if key == "og:title" && og_title.borrow().is_empty() {
                            *og_title.borrow_mut() = content;
                        }
                    }
                    Ok(())
                }
            }),
            element!("link", {
                let store = store.clone();
                let base = base.clone();
                move |el| {
                    let rel = attr(el, "rel")
                        .unwrap_or_default()
                        .to_ascii_lowercase();
                    if rel.split_whitespace().any(|part| part == "stylesheet") {
                        let Some(href) = attr(el, "href") else {
                            el.remove();
                            return Ok(());
                        };
                        match fetch_asset(&store, &href, Kind::Css, false, &base) {
                            Some(path) => {
                                let _ = el.set_attribute("href", &path);
                                el.remove_attribute("integrity");
                                el.remove_attribute("crossorigin");
                                el.remove_attribute("nonce");
                                el.remove_attribute("referrerpolicy");
                            }
                            None => el.remove(),
                        }
                    } else {
                        el.remove();
                    }
                    Ok(())
                }
            }),
            element!("style", {
                let buffer = style_buffer.clone();
                let store = store.clone();
                let base = base.clone();
                move |el| {
                    buffer.borrow_mut().push(String::new());
                    let buffer_end = buffer.clone();
                    let store_end = store.clone();
                    let base_end = base.clone();
                    el.on_end_tag(end_tag!(move |end| {
                        let css = buffer_end.borrow_mut().pop().unwrap_or_default();
                        let rewritten = rewrite_css_at(&store_end, &css, &base_end, false);
                        if !rewritten.trim().is_empty() {
                            end.before(&rewritten, ContentType::Text);
                        }
                        Ok(())
                    }))?;
                    Ok(())
                }
            }),
            element!("img", {
                let store = store.clone();
                let base = base.clone();
                move |el| {
                    let source = attr(el, "src")
                        .or_else(|| attr(el, "data-src"))
                        .or_else(|| attr(el, "data-original"))
                        .or_else(|| attr(el, "data-lazy-src"))
                        .or_else(|| first_srcset(&attr(el, "srcset").unwrap_or_default()));
                    for name in [
                        "srcset",
                        "sizes",
                        "data-src",
                        "data-original",
                        "data-lazy-src",
                        "lowsrc",
                        "dynsrc",
                    ] {
                        el.remove_attribute(name);
                    }
                    match source {
                        Some(source) if source.trim().starts_with("data:") => {
                            let _ = el.set_attribute("src", source.trim());
                        }
                        Some(source) if !source.trim().is_empty() => {
                            match fetch_asset(&store, &source, Kind::Image, false, &base) {
                                Some(path) => {
                                    let _ = el.set_attribute("src", &path);
                                }
                                None => {
                                    let _ = el.set_attribute("src", TRANSPARENT_GIF);
                                    let _ = el.set_attribute("data-offline-missing", "1");
                                }
                            }
                        }
                        _ => {
                            let _ = el.set_attribute("src", TRANSPARENT_GIF);
                            let _ = el.set_attribute("data-offline-missing", "1");
                        }
                    }
                    Ok(())
                }
            }),
            element!("form", |el| {
                el.remove_attribute("action");
                el.remove_attribute("method");
                el.remove_attribute("target");
                Ok(())
            }),
            text!("title", {
                let title = title.clone();
                move |chunk| {
                    let mut title = title.borrow_mut();
                    if title.len() < 2000 {
                        title.push_str(chunk.as_str());
                    }
                    Ok(())
                }
            }),
            text!("style", {
                let buffer = style_buffer.clone();
                move |chunk| {
                    if let Some(style) = buffer.borrow_mut().last_mut() {
                        style.push_str(chunk.as_str());
                        chunk.set_str(String::new());
                    }
                    Ok(())
                }
            }),
            element!("head", {
                let seen = head_seen.clone();
                move |el| {
                    if !seen.replace(true) {
                        el.prepend(HEAD_HTML, ContentType::Html);
                    }
                    Ok(())
                }
            }),
            element!("*", {
                let store = store.clone();
                let base = base.clone();
                move |el| {
                    let tag = el.tag_name_preserve_case().to_ascii_lowercase();
                    if matches!(
                        tag.as_str(),
                        "foreignobject" | "animate" | "animatetransform" | "animatemotion" | "set"
                    ) {
                        el.remove();
                        return Ok(());
                    }

                    let mut doomed = Vec::new();
                    for attribute in el.attributes() {
                        let name = attribute.name();
                        let lower = name.to_ascii_lowercase();
                        if lower.starts_with("on")
                            || matches!(
                                lower.as_str(),
                                "srcdoc"
                                    | "ping"
                                    | "nonce"
                                    | "integrity"
                                    | "crossorigin"
                                    | "formaction"
                                    | "autofocus"
                                    | "download"
                                    | "target"
                                    | "poster"
                                    | "lowsrc"
                                    | "dynsrc"
                            )
                        {
                            doomed.push(name);
                        }
                    }
                    for name in doomed {
                        el.remove_attribute(&name);
                    }

                    if let Some(style) = attr(el, "style") {
                        let rewritten = rewrite_css_at(&store, &style, &base, false);
                        let _ = el.set_attribute("style", &rewritten);
                    }

                    if let Some(background) = attr(el, "background") {
                        match fetch_asset(&store, &background, Kind::Image, false, &base) {
                            Some(path) => {
                                let _ = el.set_attribute("background", &path);
                            }
                            None => el.remove_attribute("background"),
                        }
                    }

                    match tag.as_str() {
                        "a" | "area" => rewrite_link(el, &base),
                        "use" => {
                            rewrite_fragment_only(el, "href");
                            rewrite_fragment_only(el, "xlink:href");
                        }
                        "image" => {
                            rewrite_svg_image(el, &store, &base);
                        }
                        "input" => {
                            if attr(el, "type")
                                .unwrap_or_default()
                                .eq_ignore_ascii_case("image")
                            {
                                if let Some(source) = attr(el, "src") {
                                    match fetch_asset(
                                        &store,
                                        &source,
                                        Kind::Image,
                                        false,
                                        &base,
                                    ) {
                                        Some(path) => {
                                            let _ = el.set_attribute("src", &path);
                                        }
                                        None => el.remove_attribute("src"),
                                    }
                                }
                            }
                        }
                        _ => {}
                    }

                    for name in ["srcset", "sizes"] {
                        el.remove_attribute(name);
                    }
                    Ok(())
                }
            }),
        ],
        ..Settings::default()
    };

    let mut output: Vec<u8> = Vec::with_capacity(html.len());
    let mut rewriter = HtmlRewriter::new(settings, |chunk: &[u8]| {
        output.extend_from_slice(chunk);
    });
    rewriter
        .write(html.as_bytes())
        .map_err(|error| format!("The page could not be parsed: {error}"))?;
    rewriter
        .end()
        .map_err(|error| format!("The page could not be parsed: {error}"))?;

    let rewritten = String::from_utf8(output)
        .map_err(|_| "The page produced invalid text while being saved.".to_string())?;
    let rewritten = if head_seen.get() {
        ensure_doctype(rewritten)
    } else {
        inject_head(&rewritten)
    };

    let mut title = decode_text(&title.borrow(), 200);
    if title.is_empty() {
        title = decode_text(&og_title.borrow(), 200);
    }
    let mut description = decode_text(&description.borrow(), 400);
    if description.is_empty() {
        description = collapse_text(&extract_text(&rewritten), 240);
    }

    let assets = store.borrow().count;
    Ok(HtmlResult {
        html: rewritten,
        title,
        description,
        assets,
    })
}

/// Resolves and fetches one asset, returning the path the page should use.
fn fetch_asset(
    store: &Rc<RefCell<AssetStore>>,
    raw: &str,
    kind: Kind,
    from_css: bool,
    base: &Url,
) -> Option<String> {
    let url = resolve_asset_url(base, raw)?;
    let key = url.as_str().to_string();

    {
        let store = store.borrow();
        if let Some(cached) = store.cache.get(&key) {
            return cached.clone();
        }
    }

    {
        let mut store = store.borrow_mut();
        if store.count >= MAX_ASSETS
            || store.total >= MAX_TOTAL_BYTES
            || store.fetcher.expired()
        {
            store.cache.insert(key.clone(), None);
            return None;
        }
        // Marked as in progress so a cycle in CSS imports terminates.
        store.cache.insert(key.clone(), None);
    }

    let fetched = match store
        .borrow()
        .fetcher
        .get(&url, MAX_ASSET_BYTES, accept_for(kind))
    {
        Ok(fetched) => fetched,
        Err(error) => {
            logging::line(&format!("asset skipped ({url}): {error}"));
            return None;
        }
    };

    let Some((extension, is_css)) = classify(&url, &fetched.content_type, kind, &fetched.bytes)
    else {
        logging::line(&format!(
            "asset skipped ({url}): content type {} is not allowed here",
            fetched.content_type
        ));
        return None;
    };

    let bytes = if is_css {
        let css = decode_bytes(&fetched.bytes, &fetched.content_type);
        rewrite_css_at(store, &css, &url, true).into_bytes()
    } else {
        fetched.bytes
    };

    let digest = Sha256::digest(&bytes);
    let filename = format!("{}.{extension}", hex(&digest));
    let path = store.borrow().assets_dir.join(&filename);
    if !path.exists() {
        if let Err(error) = fs::write(&path, &bytes) {
            logging::line(&format!("could not write asset {filename}: {error}"));
            return None;
        }
    }

    let returned = if from_css {
        filename.clone()
    } else {
        format!("assets/{filename}")
    };

    let mut store = store.borrow_mut();
    store.count += 1;
    store.total += bytes.len() as u64;
    store.cache.insert(key, Some(returned.clone()));
    if store.count % 5 == 0 {
        (store.progress)(format!("Downloading assets ({})", store.count));
    }
    Some(returned)
}

fn resolve_asset_url(base: &Url, raw: &str) -> Option<Url> {
    let raw = raw.trim();
    if raw.is_empty()
        || raw.starts_with('#')
        || raw.starts_with("data:")
        || raw.starts_with("about:")
        || raw.starts_with("blob:")
        || raw.starts_with("javascript:")
        || raw.starts_with("mailto:")
        || raw.starts_with("tel:")
    {
        return None;
    }
    let mut url = base.join(raw).ok()?;
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    security::validate_url(&url).ok()?;
    url.set_fragment(None);
    Some(url)
}

struct AssetStore {
    fetcher: Fetcher,
    assets_dir: PathBuf,
    cache: HashMap<String, Option<String>>,
    count: u32,
    total: u64,
    progress: Progress,
}

impl AssetStore {
    fn new(fetcher: Fetcher, root: PathBuf, progress: Progress) -> Self {
        Self {
            fetcher,
            assets_dir: root.join("assets"),
            cache: HashMap::new(),
            count: 0,
            total: 0,
            progress,
        }
    }
}

#[derive(Clone, Copy)]
enum Kind {
    Any,
    Image,
    Css,
}

fn accept_for(kind: Kind) -> &'static str {
    match kind {
        Kind::Css => "text/css,*/*;q=0.1",
        Kind::Image => {
            "image/avif,image/webp,image/png,image/svg+xml,image/*;q=0.9,*/*;q=0.1"
        }
        Kind::Any => concat!(
            "image/avif,image/webp,image/png,image/svg+xml,image/*;q=0.9,",
            "font/woff2,font/woff,font/ttf,font/otf,*/*;q=0.1"
        ),
    }
}

/// Returns the file extension and whether the bytes are CSS to rewrite.
fn classify(url: &Url, content_type: &str, kind: Kind, bytes: &[u8]) -> Option<(String, bool)> {
    let media = fetch::media_type(content_type);
    let extension = match media.as_str() {
        "text/css" if matches!(kind, Kind::Css) => "css".to_string(),
        "image/png" | "image/apng" if matches!(kind, Kind::Image | Kind::Any) => "png".to_string(),
        "image/jpeg" if matches!(kind, Kind::Image | Kind::Any) => "jpg".to_string(),
        "image/gif" if matches!(kind, Kind::Image | Kind::Any) => "gif".to_string(),
        "image/webp" if matches!(kind, Kind::Image | Kind::Any) => "webp".to_string(),
        "image/avif" if matches!(kind, Kind::Image | Kind::Any) => "avif".to_string(),
        "image/bmp" if matches!(kind, Kind::Image | Kind::Any) => "bmp".to_string(),
        "image/x-icon" | "image/vnd.microsoft.icon" if matches!(kind, Kind::Image | Kind::Any) => {
            "ico".to_string()
        }
        "image/svg+xml" if matches!(kind, Kind::Image | Kind::Any) => "svg".to_string(),
        "font/woff2" | "application/font-woff2" if matches!(kind, Kind::Any) => {
            "woff2".to_string()
        }
        "font/woff" | "application/font-woff" if matches!(kind, Kind::Any) => {
            "woff".to_string()
        }
        "font/ttf" | "application/x-font-ttf" if matches!(kind, Kind::Any) => {
            "ttf".to_string()
        }
        "font/otf" | "application/x-font-opentype" if matches!(kind, Kind::Any) => {
            "otf".to_string()
        }
        "" | "application/octet-stream" => {
            // A misconfigured server, or a CDN that does not label its files.
            // Fall back to the URL suffix, but only for this kind.
            let suffix = url
                .path()
                .rsplit('.')
                .next()
                .unwrap_or("")
                .to_ascii_lowercase();
            let allowed = match kind {
                Kind::Image => matches!(
                    suffix.as_str(),
                    "png" | "jpg" | "jpeg" | "gif" | "webp" | "avif" | "bmp" | "ico" | "svg"
                ),
                Kind::Css => suffix == "css",
                Kind::Any => matches!(
                    suffix.as_str(),
                    "png"
                        | "jpg"
                        | "jpeg"
                        | "gif"
                        | "webp"
                        | "avif"
                        | "bmp"
                        | "ico"
                        | "svg"
                        | "woff2"
                        | "woff"
                        | "ttf"
                        | "otf"
                ),
            };
            if !allowed {
                return None;
            }
            if suffix == "jpeg" {
                "jpg".to_string()
            } else {
                suffix
            }
        }
        _ => return None,
    };

    if extension != "css" && !bytes_match(&extension, bytes) {
        return None;
    }
    let is_css = extension == "css";
    Some((extension, is_css))
}

fn bytes_match(extension: &str, bytes: &[u8]) -> bool {
    match extension {
        "png" => bytes.starts_with(b"\x89PNG\r\n\x1a\n"),
        "jpg" => bytes.starts_with(&[0xff, 0xd8, 0xff]),
        "gif" => bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a"),
        "webp" => bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP",
        "avif" => bytes.len() >= 12 && &bytes[4..8] == b"ftyp",
        "bmp" => bytes.starts_with(b"BM"),
        "ico" => bytes.starts_with(&[0, 0, 1, 0]),
        "svg" => {
            let bytes = bytes.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(bytes);
            let start = bytes
                .iter()
                .position(|byte| !byte.is_ascii_whitespace())
                .unwrap_or(0);
            let rest = &bytes[start..];
            rest.starts_with(b"<?xml")
                || rest.starts_with(b"<svg")
                || rest.starts_with(b"<!--")
                || rest.starts_with(b"<!DOCTYPE svg")
        }
        "woff2" => bytes.starts_with(b"wOF2"),
        "woff" => bytes.starts_with(b"wOFF"),
        "ttf" => bytes.starts_with(&[0, 1, 0, 0]) || bytes.starts_with(b"true"),
        "otf" => bytes.starts_with(b"OTTO"),
        _ => true,
    }
}

fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

fn attr(el: &lol_html::html_content::Element<'_, '_>, name: &str) -> Option<String> {
    el.attributes()
        .iter()
        .find(|attribute| attribute.name().eq_ignore_ascii_case(name))
        .map(|attribute| attribute.value())
}

fn first_srcset(srcset: &str) -> Option<String> {
    srcset
        .split(',')
        .find_map(|candidate| candidate.split_whitespace().next())
        .map(|candidate| candidate.to_string())
}

fn rewrite_link(el: &mut lol_html::html_content::Element<'_, '_>, base: &Url) {
    let Some(raw) = attr(el, "href") else {
        return;
    };
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return;
    }
    if trimmed.starts_with('#') {
        return;
    }
    match base.join(trimmed) {
        Ok(url) if matches!(url.scheme(), "http" | "https") => {
            let _ = el.set_attribute("href", url.as_str());
        }
        _ => el.remove_attribute("href"),
    }
}

fn rewrite_fragment_only(el: &mut lol_html::html_content::Element<'_, '_>, name: &str) {
    if let Some(value) = attr(el, name) {
        if !value.trim_start().starts_with('#') {
            el.remove_attribute(name);
        }
    }
}

fn rewrite_svg_image(
    el: &mut lol_html::html_content::Element<'_, '_>,
    store: &Rc<RefCell<AssetStore>>,
    base: &Url,
) {
    for name in ["href", "xlink:href"] {
        let Some(value) = attr(el, name) else {
            continue;
        };
        let value = value.trim();
        if value.starts_with('#') || value.starts_with("data:") {
            continue;
        }
        match fetch_asset(store, value, Kind::Image, false, base) {
            Some(path) => {
                let _ = el.set_attribute(name, &path);
            }
            None => el.remove_attribute(name),
        }
    }
}

fn rewrite_css_at(
    store: &Rc<RefCell<AssetStore>>,
    css: &str,
    base: &Url,
    from_css: bool,
) -> String {
    rewrite_css_with(css, |raw, kind| {
        fetch_asset(store, raw, kind, from_css, base)
    })
}

/// Rewrites every URL in a stylesheet through `resolve`.
///
/// The scanner is deliberately small but handles the two constructs that
/// matter: comments and strings are copied verbatim, `url(...)` is replaced,
/// and `@import` can use either the string or the `url()` form.
fn rewrite_css_with<F>(css: &str, mut resolve: F) -> String
where
    F: FnMut(&str, Kind) -> Option<String>,
{
    let bytes = css.as_bytes();
    let mut out = String::with_capacity(css.len());
    let mut index = 0usize;

    while index < bytes.len() {
        if bytes[index] == b'/' && index + 1 < bytes.len() && bytes[index + 1] == b'*' {
            let end = find_bytes(css, index + 2, "*/")
                .map(|position| position + 2)
                .unwrap_or(bytes.len());
            out.push_str(&css[index..end]);
            index = end;
            continue;
        }

        if bytes[index] == b'"' || bytes[index] == b'\'' {
            let end = string_end(css, index);
            out.push_str(&css[index..end]);
            index = end;
            continue;
        }

        if starts_with_ci(css, index, "url(") && is_function_start(css, index) {
            let (url, end) = parse_url_token(css, index + 4);
            match url {
                Some(raw) => {
                    let trimmed = raw.trim();
                    if trimmed.is_empty() || is_kept_css_url(trimmed) {
                        out.push_str(&css[index..end]);
                    } else if let Some(path) = resolve(trimmed, Kind::Any) {
                        out.push_str("url(\"");
                        out.push_str(&path);
                        out.push_str("\")");
                    } else {
                        out.push_str("url(\"data:,\")");
                    }
                }
                None => out.push_str(&css[index..end]),
            }
            index = end;
            continue;
        }

        if starts_with_ci(css, index, "@charset") && is_at_rule_start(css, index) {
            // The saved file is always UTF-8, so the original declaration must
            // not be allowed to make the browser decode it as something else.
            out.push_str("@charset \"UTF-8\";");
            index = statement_end(css, index + 8);
            continue;
        }

        if starts_with_ci(css, index, "@import") && is_at_rule_start(css, index) {
            let mut cursor = index + 7;
            while cursor < bytes.len() && bytes[cursor].is_ascii_whitespace() {
                cursor += 1;
            }
            let parsed = if cursor < bytes.len()
                && (bytes[cursor] == b'"' || bytes[cursor] == b'\'')
            {
                let quote = bytes[cursor];
                let (content, end) = parse_css_string(css, cursor, quote);
                Some((content, end))
            } else if starts_with_ci(css, cursor, "url(") {
                let (content, end) = parse_url_token(css, cursor + 4);
                content.map(|content| (content, end))
            } else {
                None
            };

            match parsed {
                Some((raw, end)) => {
                    let trimmed = raw.trim();
                    if trimmed.is_empty() || is_kept_css_url(trimmed) {
                        out.push_str(&css[index..end]);
                    } else if let Some(path) = resolve(trimmed, Kind::Css) {
                        out.push_str("@import url(\"");
                        out.push_str(&path);
                        out.push_str("\")");
                    } else {
                        // Drop the whole at-rule, including its media query.
                        index = statement_end(css, end);
                        continue;
                    }
                    index = end;
                }
                None => {
                    out.push_str(&css[index..index + 7]);
                    index += 7;
                }
            }
            continue;
        }

        let character = css[index..].chars().next().unwrap_or('\u{fffd}');
        out.push(character);
        index += character.len_utf8();
    }
    out
}

fn is_kept_css_url(url: &str) -> bool {
    url.starts_with("data:") || url.starts_with('#') || url.starts_with("about:")
}

fn is_function_start(css: &str, index: usize) -> bool {
    if index == 0 {
        return true;
    }
    let previous = css.as_bytes()[index - 1] as char;
    !(previous.is_ascii_alphanumeric() || previous == '-' || previous == '_' || previous == '\\')
}

fn is_at_rule_start(css: &str, index: usize) -> bool {
    if index == 0 {
        return true;
    }
    let previous = css.as_bytes()[index - 1] as char;
    !(previous.is_ascii_alphanumeric() || previous == '-' || previous == '_')
}

fn starts_with_ci(css: &str, index: usize, needle: &str) -> bool {
    let end = index + needle.len();
    end <= css.len() && css.is_char_boundary(end) && css[index..end].eq_ignore_ascii_case(needle)
}

fn find_bytes(css: &str, from: usize, needle: &str) -> Option<usize> {
    css.get(from..)?.find(needle).map(|position| from + position)
}

fn string_end(css: &str, index: usize) -> usize {
    let bytes = css.as_bytes();
    let quote = bytes[index];
    let mut cursor = index + 1;
    while cursor < bytes.len() {
        match bytes[cursor] {
            b'\\' => cursor += 2,
            byte if byte == quote => return cursor + 1,
            _ => cursor += 1,
        }
    }
    bytes.len()
}

/// Parses a quoted string at `index` and returns its raw contents and the
/// position after the closing quote.
fn parse_css_string(css: &str, index: usize, quote: u8) -> (String, usize) {
    let bytes = css.as_bytes();
    let mut cursor = index + 1;
    let start = cursor;
    while cursor < bytes.len() {
        match bytes[cursor] {
            b'\\' => cursor += 2,
            byte if byte == quote => {
                return (css[start..cursor.min(bytes.len())].to_string(), cursor + 1);
            }
            _ => cursor += 1,
        }
    }
    (css[start..].to_string(), bytes.len())
}

/// Parses the content of `url(`. Returns the raw URL and the position after the
/// closing parenthesis.
fn parse_url_token(css: &str, content_start: usize) -> (Option<String>, usize) {
    let bytes = css.as_bytes();
    let mut cursor = content_start;
    while cursor < bytes.len() && bytes[cursor].is_ascii_whitespace() {
        cursor += 1;
    }
    if cursor >= bytes.len() {
        return (None, bytes.len());
    }
    if bytes[cursor] == b'"' || bytes[cursor] == b'\'' {
        let quote = bytes[cursor];
        let (content, after) = parse_css_string(css, cursor, quote);
        let mut close = after;
        while close < bytes.len() && bytes[close].is_ascii_whitespace() {
            close += 1;
        }
        if close < bytes.len() && bytes[close] == b')' {
            close += 1;
        }
        return (Some(content), close);
    }

    let start = cursor;
    while cursor < bytes.len() {
        match bytes[cursor] {
            b'\\' => cursor += 2,
            b')' => {
                return (Some(css[start..cursor].to_string()), cursor + 1);
            }
            _ => cursor += 1,
        }
    }
    (Some(css[start..].to_string()), bytes.len())
}

/// Returns the index just after the next semicolon, outside strings/comments.
fn statement_end(css: &str, from: usize) -> usize {
    let bytes = css.as_bytes();
    let mut cursor = from;
    while cursor < bytes.len() {
        match bytes[cursor] {
            b'/' if cursor + 1 < bytes.len() && bytes[cursor + 1] == b'*' => {
                cursor = find_bytes(css, cursor + 2, "*/")
                    .map(|position| position + 2)
                    .unwrap_or(bytes.len());
            }
            b'"' | b'\'' => cursor = string_end(css, cursor),
            b';' => return cursor + 1,
            _ => cursor += 1,
        }
    }
    bytes.len()
}

/// Decodes bytes with the header charset, a BOM, or UTF-8, in that order.
pub fn decode_bytes(bytes: &[u8], content_type: &str) -> String {
    if let Some(label) = fetch::charset(content_type) {
        if let Some(encoding) = encoding_rs::Encoding::for_label(label.as_bytes()) {
            return encoding.decode(bytes).0.into_owned();
        }
    }
    if bytes.starts_with(&[0xef, 0xbb, 0xbf]) {
        return String::from_utf8_lossy(&bytes[3..]).into_owned();
    }
    if bytes.starts_with(&[0xff, 0xfe]) {
        return encoding_rs::UTF_16LE.decode(&bytes[2..]).0.into_owned();
    }
    if bytes.starts_with(&[0xfe, 0xff]) {
        return encoding_rs::UTF_16BE.decode(&bytes[2..]).0.into_owned();
    }
    if let Some(label) = meta_charset(bytes) {
        if let Some(encoding) = encoding_rs::Encoding::for_label(label.as_bytes()) {
            return encoding.decode(bytes).0.into_owned();
        }
    }
    String::from_utf8_lossy(bytes).into_owned()
}

fn meta_charset(bytes: &[u8]) -> Option<String> {
    let head = &bytes[..bytes.len().min(1024)];
    let text = String::from_utf8_lossy(head).to_ascii_lowercase();
    let position = text.find("charset=")? + "charset=".len();
    let rest = &text[position..];
    let rest = rest.trim_start_matches(['"', '\'', ' ']);
    let end = rest
        .find(|character: char| {
            character.is_ascii_whitespace()
                || matches!(character, '"' | '\'' | ';' | '>' | '/')
        })
        .unwrap_or(rest.len());
    let label = rest[..end].trim();
    if label.is_empty() {
        None
    } else {
        Some(label.to_string())
    }
}

fn decode_text(raw: &str, limit: usize) -> String {
    let decoded = html_escape::decode_html_entities(raw);
    collapse_text(&decoded, limit)
}

fn collapse_text(text: &str, limit: usize) -> String {
    let mut out = String::with_capacity(text.len().min(limit));
    let mut last_space = false;
    for character in text.chars() {
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

fn extract_text(html: &str) -> String {
    let buffer = Rc::new(RefCell::new(String::new()));
    let settings = Settings {
        element_content_handlers: vec![
            element!("script, style, noscript, template, head, svg", |el| {
                el.remove();
                Ok(())
            }),
            text!("body", {
                let buffer = buffer.clone();
                move |chunk| {
                    let mut buffer = buffer.borrow_mut();
                    if buffer.len() < 4000 {
                        buffer.push_str(chunk.as_str());
                    }
                    Ok(())
                }
            }),
        ],
        ..Settings::default()
    };
    let mut sink = Vec::new();
    let mut rewriter = HtmlRewriter::new(settings, |chunk: &[u8]| {
        sink.extend_from_slice(chunk);
    });
    if rewriter.write(html.as_bytes()).is_err() || rewriter.end().is_err() {
        return String::new();
    }
    buffer.borrow().clone()
}

/// Inserts the charset/referrer/CSP block after the opening head tag, creating a
/// head when the document has none.
fn inject_head(html: &str) -> String {
    let mut output = html.to_string();
    if let Some(index) = find_ci(&output, "<head") {
        if let Some(close) = output[index..].find('>') {
            output.insert_str(index + close + 1, HEAD_HTML);
            return ensure_doctype(output);
        }
    }
    if let Some(index) = find_ci(&output, "<html") {
        if let Some(close) = output[index..].find('>') {
            output.insert_str(index + close + 1, &format!("<head>{HEAD_HTML}</head>"));
            return ensure_doctype(output);
        }
    }
    if let Some(index) = find_ci(&output, "<body") {
        output.insert_str(index, &format!("<head>{HEAD_HTML}</head>"));
        return ensure_doctype(output);
    }
    output.insert_str(0, &format!("<head>{HEAD_HTML}</head>"));
    ensure_doctype(output)
}

fn ensure_doctype(html: String) -> String {
    let trimmed = html.trim_start();
    if trimmed
        .get(..9)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("<!doctype"))
    {
        return html;
    }
    if trimmed.starts_with("<?xml") {
        return html;
    }
    format!("<!doctype html>{html}")
}

fn find_ci(haystack: &str, needle: &str) -> Option<usize> {
    haystack
        .as_bytes()
        .windows(needle.len())
        .position(|window| window.eq_ignore_ascii_case(needle.as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    fn css_urls_are_rewritten() {
        let output = rewrite_css_with(
            "body{background:url('pics/a.png')} @font-face{src:url(f.woff2)}",
            |raw, _kind| Some(format!("assets/{raw}")),
        );
        assert!(output.contains("url(\"assets/pics/a.png\")"));
        assert!(output.contains("url(\"assets/f.woff2\")"));
    }

    #[test]
    fn css_keeps_data_and_fragments() {
        let output = rewrite_css_with(
            ".a{fill:url(#g);background:url(data:image/png;base64,AAAA)}",
            |_, _| panic!("no URL should be resolved"),
        );
        assert!(output.contains("url(#g)"));
        assert!(output.contains("url(data:image/png;base64,AAAA)"));
    }

    #[test]
    fn css_ignores_comments_and_strings() {
        let source = "/* url(x.png) */ .a::before{content:\"url(y.png)\"}";
        let output = rewrite_css_with(source, |_, _| panic!("nothing to resolve"));
        assert_eq!(output, source);
    }

    #[test]
    fn css_import_is_rewritten_or_removed() {
        let output = rewrite_css_with(r#"@import "a.css" screen;"#, |raw, kind| {
            assert_eq!(raw, "a.css");
            assert!(matches!(kind, Kind::Css));
            Some("b.css".to_string())
        });
        assert_eq!(output, r#"@import url("b.css") screen;"#);

        let output = rewrite_css_with(r#"@import "gone.css" screen;"#, |_, _| None);
        assert_eq!(output, "");
    }

    #[test]
    fn css_charset_is_replaced_with_utf8() {
        let output = rewrite_css_with("@charset \"iso-8859-1\"; body{color:red}", |_, _| None);
        assert_eq!(output, "@charset \"UTF-8\"; body{color:red}");
    }

    #[test]
    fn css_failed_url_becomes_empty_data_uri() {
        let output = rewrite_css_with(".a{background:url(x.png)}", |_, _| None);
        assert!(output.contains("url(\"data:,\")"));
    }

    fn test_fetcher() -> Fetcher {
        Fetcher::new(Instant::now() + Duration::from_secs(30)).unwrap()
    }

    fn rewrite_page(html: &str) -> HtmlResult {
        let root = std::env::temp_dir().join(format!(
            "win-rere-sanitize-{}",
            uuid::Uuid::new_v4().simple()
        ));
        let base = Url::parse("https://example.com/docs/page").unwrap();
        let result = rewrite_html(
            html,
            &base,
            &root,
            &test_fetcher(),
            Arc::new(|_| {}),
        )
        .unwrap();
        let _ = std::fs::remove_dir_all(root);
        result
    }

    #[test]
    fn dangerous_markup_is_removed() {
        let result = rewrite_page(
            r#"<html><head><title>Hi &amp; bye</title><meta name="description" content="A page">
               <meta http-equiv="refresh" content="0;url=https://evil.example/">
               <script>alert(1)</script></head>
               <body onload="alert(1)"><iframe src="https://evil.example/"></iframe>
               <a href="javascript:alert(1)">bad</a>
               <a href="/relative">good</a>
               <img src="data:image/gif;base64,R0lGODlhAQABAIAAAAAAAP///ywAAAAAAQABAAACAUwAOw==">
               </body></html>"#,
        );
        assert!(!result.html.contains("<script"));
        assert!(!result.html.contains("onload"));
        assert!(!result.html.contains("<iframe"));
        assert!(!result.html.contains("javascript:"));
        assert!(!result.html.contains("http-equiv=\"refresh\""));
        assert!(result.html.contains("href=\"https://example.com/relative\""));
        assert!(result.html.contains("Content-Security-Policy"));
        assert!(result.html.contains("<meta charset=\"utf-8\">"));
        assert_eq!(result.title, "Hi & bye");
        assert_eq!(result.description, "A page");
    }

    #[test]
    fn csp_and_charset_are_injected_without_a_head() {
        let result = rewrite_page("<body><p>Hello</p></body>");
        assert!(result.html.starts_with("<!doctype html>"));
        assert!(result.html.contains("<head><meta charset=\"utf-8\">"));
        assert!(!result.html.contains("http-equiv=\"refresh\""));
    }

    #[test]
    fn css_is_rewritten_in_style_elements() {
        let result = rewrite_page(
            "<html><head><style>.a{fill:url(#g);background:url(data:image/png;base64,AAAA)}</style></head><body>x</body></html>",
        );
        assert!(result.html.contains("url(#g)"));
        assert!(result.html.contains("url(data:image/png;base64,AAAA)"));
        assert!(result.html.contains("<style>"));
    }

    #[test]
    fn inline_event_handlers_disappear() {
        let result = rewrite_page(r#"<p onclick="x()" onmouseover="y()">text</p>"#);
        assert!(!result.html.contains("onclick"));
        assert!(!result.html.contains("onmouseover"));
        assert!(result.html.contains("<p>text</p>"));
    }
}
