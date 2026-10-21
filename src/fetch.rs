//! The only HTTP client in the program.
//!
//! Two properties matter here. First, every connection must go to a public
//! address: a custom DNS resolver filters the addresses the machine resolved,
//! and the redirect policy re-validates each hop. Second, no response may grow
//! without a bound: callers pass a byte limit and reads stop one byte past it.
//!
//! Proxies are disabled on purpose. Allowing an environment proxy would move
//! DNS resolution to another process, outside the address filter.

use std::error::Error;
use std::fmt;
use std::io::Read;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use reqwest::blocking::Client;
use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use reqwest::redirect::Policy;
use reqwest::header::{ACCEPT, CONTENT_TYPE, USER_AGENT};
use url::Url;

use crate::security;

const MAX_REDIRECTS: usize = 5;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(8);

/// A response that fits inside the caller's limit.
pub struct Fetched {
    pub bytes: Vec<u8>,
    pub content_type: String,
    pub final_url: Url,
}

/// The HTTP client for one archive job.
#[derive(Clone)]
pub struct Fetcher {
    client: Client,
    deadline: Instant,
}

impl Fetcher {
    pub fn new(deadline: Instant) -> Result<Self, String> {
        let client = Client::builder()
            .user_agent("win-rere/0.1 (offline bookmark widget)")
            .timeout(REQUEST_TIMEOUT)
            .connect_timeout(CONNECT_TIMEOUT)
            .redirect(Policy::custom(refuse_bad_redirect))
            .no_proxy()
            .dns_resolver(Arc::new(PublicResolver))
            .build()
            .map_err(|error| format!("Could not create the HTTP client: {error}"))?;
        Ok(Self { client, deadline })
    }

    /// Whether the whole job has run out of time.
    pub fn expired(&self) -> bool {
        Instant::now() >= self.deadline
    }

    /// Fetches `url`, refusing anything that is not a public web page and
    /// anything larger than `limit` bytes after decompression. A per-request
    /// user agent lets the caller ask the server for its mobile document.
    pub fn get_with_user_agent(
        &self,
        url: &Url,
        limit: u64,
        accept: &str,
        user_agent: Option<&str>,
    ) -> Result<Fetched, String> {
        security::validate_url(url)?;
        if self.expired() {
            return Err("The download took too long and was stopped.".to_string());
        }

        let mut request = self.client.get(url.clone()).header(ACCEPT, accept);
        if let Some(user_agent) = user_agent {
            request = request.header(USER_AGENT, user_agent);
        }
        let response = request
            .send()
            .map_err(|error| friendly_request_error(&error))?;

        let status = response.status().as_u16();
        let final_url = response.url().clone();
        security::validate_url(&final_url)?;

        if let Some(address) = response.remote_addr() {
            if !security::is_public_ip(address.ip()) {
                return Err(
                    "The connection ended at a private address and was refused.".to_string(),
                );
            }
        }

        if let Some(length) = response.content_length() {
            if length > limit {
                return Err(format!(
                    "The response is too large to save (limit {}).",
                    human_size(limit)
                ));
            }
        }

        let content_type = response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("")
            .to_string();

        if !(200..=299).contains(&status) {
            return Err(format!("The server answered HTTP {status}."));
        }

        let mut bytes = Vec::new();
        let read = response
            .take(limit + 1)
            .read_to_end(&mut bytes)
            .map_err(|error| format!("The response could not be read: {error}"))?;
        if read as u64 > limit {
            return Err(format!(
                "The response is too large to save (limit {}).",
                human_size(limit)
            ));
        }

        Ok(Fetched {
            bytes,
            content_type,
            final_url,
        })
    }
}

/// Resolves a hostname and keeps only public addresses.
struct PublicResolver;

impl Resolve for PublicResolver {
    fn resolve(&self, name: Name) -> Resolving {
        let host = name.as_str().to_ascii_lowercase();
        Box::pin(async move {
            if security::is_forbidden_hostname(&host) {
                return Err(boxed("that host is local or reserved".to_string()));
            }
            let resolved = tokio::net::lookup_host((host.as_str(), 0_u16))
                .await
                .map_err(|error| boxed(format!("DNS lookup for {host} failed: {error}")))?;
            let public: Vec<SocketAddr> = resolved
                .filter(|address| security::is_public_ip(address.ip()))
                .collect();
            if public.is_empty() {
                return Err(boxed(format!(
                    "{host} does not resolve to a public address"
                )));
            }
            Ok(Box::new(public.into_iter()) as Addrs)
        })
    }
}

fn boxed(message: String) -> Box<dyn Error + Send + Sync> {
    Box::new(ResolverError(message))
}

#[derive(Debug)]
struct ResolverError(String);

impl fmt::Display for ResolverError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl Error for ResolverError {}

#[derive(Debug)]
struct RedirectRefused(String);

impl fmt::Display for RedirectRefused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl Error for RedirectRefused {}

fn refuse_bad_redirect(attempt: reqwest::redirect::Attempt<'_>) -> reqwest::redirect::Action {
    if attempt.previous().len() >= MAX_REDIRECTS {
        return attempt.error(RedirectRefused("too many redirects".to_string()));
    }
    if let Err(reason) = security::validate_url(attempt.url()) {
        return attempt.error(RedirectRefused(reason));
    }
    if let Some(previous) = attempt.previous().last() {
        if previous.scheme() == "https" && attempt.url().scheme() == "http" {
            return attempt.error(RedirectRefused(
                "a secure page tried to redirect to plain HTTP".to_string(),
            ));
        }
    }
    attempt.follow()
}

fn friendly_request_error(error: &reqwest::Error) -> String {
    if error.is_timeout() {
        "The server took too long to answer.".to_string()
    } else if error.is_redirect() {
        "The page redirected too many times.".to_string()
    } else if error.is_connect() {
        "Could not connect to the server.".to_string()
    } else if error.is_body() || error.is_decode() {
        "The server response could not be read.".to_string()
    } else {
        "The page could not be downloaded.".to_string()
    }
}

/// Formats a byte count for an error message.
pub fn human_size(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    let value = bytes as f64;
    if value >= MB {
        format!("{:.1} MB", value / MB)
    } else if value >= KB {
        format!("{:.0} KB", value / KB)
    } else {
        format!("{bytes} B")
    }
}

/// Trims a `Content-Type` string to its media type, lowercased.
pub fn media_type(content_type: &str) -> String {
    content_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase()
}

/// Extracts a charset parameter from a `Content-Type` string.
pub fn charset(content_type: &str) -> Option<String> {
    for part in content_type.split(';').skip(1) {
        let part = part.trim();
        if let Some(value) = part.strip_prefix("charset=") {
            let value = value.trim_matches('"').trim();
            if !value.is_empty() {
                return Some(value.to_ascii_lowercase());
            }
        }
    }
    None
}
