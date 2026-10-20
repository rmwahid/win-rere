//! URL and address policy for everything the archiver fetches.
//!
//! The widget downloads pages from the public internet and nothing else. These
//! checks are the first of two layers: they reject a bad URL before a socket is
//! opened, and the DNS resolver in `fetch` filters the addresses the machine
//! actually resolves. That second layer is what makes DNS rebinding hard: the
//! connection is made to an address this module has already classified.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use url::{Host, Url};

/// A URL longer than this is refused before it is parsed.
pub const MAX_URL_BYTES: usize = 2048;

/// Whether a resolved or literal address is a public unicast destination.
pub fn is_public_ip(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(ip) => is_public_ipv4(ip),
        IpAddr::V6(ip) => is_public_ipv6(ip),
    }
}

/// Whether a hostname is obviously local, reserved or otherwise not reachable
/// on the public internet.
///
/// Cloud metadata services and split-horizon DNS are covered by the address
/// check, not by this list. This only rejects names that must never be fetched.
pub fn is_forbidden_hostname(host: &str) -> bool {
    let host = host.trim_end_matches('.');
    if host.is_empty() {
        return true;
    }
    let lower = host.to_ascii_lowercase();
    if lower == "localhost" || lower.ends_with(".localhost") {
        return true;
    }
    // Single-label names belong to the local network, with no public TLD.
    if !lower.contains('.') {
        return true;
    }
    const BLOCKED_SUFFIXES: &[&str] = &[
        ".local",
        ".internal",
        ".home.arpa",
        ".arpa",
        ".test",
        ".invalid",
        ".example",
        ".onion",
    ];
    BLOCKED_SUFFIXES
        .iter()
        .any(|suffix| lower.ends_with(suffix))
}

/// Validates a URL the user asked to save.
///
/// Only `http` and `https`, only public destinations, no embedded credentials,
/// and only the standard web ports. The port rule keeps the fetcher away from
/// arbitrary public services while still allowing every normal web page.
pub fn validate_url(url: &Url) -> Result<(), String> {
    if url.as_str().len() > MAX_URL_BYTES {
        return Err("That URL is too long.".to_string());
    }
    match url.scheme() {
        "http" | "https" => {}
        other => {
            return Err(format!(
                "Only http and https pages can be saved offline, not {other}."
            ));
        }
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err("URLs with a username or password are not fetched.".to_string());
    }
    if let Some(port) = url.port() {
        if port != 80 && port != 443 {
            return Err("Only the standard web ports 80 and 443 are allowed.".to_string());
        }
    }
    let Some(host) = url.host() else {
        return Err("The URL has no host.".to_string());
    };
    match host {
        Host::Domain(name) => {
            if is_forbidden_hostname(name) {
                return Err(
                    "That host is local or reserved and cannot be fetched.".to_string(),
                );
            }
        }
        Host::Ipv4(ip) => {
            if !is_public_ipv4(ip) {
                return Err(
                    "That address is private or reserved and cannot be fetched.".to_string(),
                );
            }
        }
        Host::Ipv6(ip) => {
            if !is_public_ipv6(ip) {
                return Err(
                    "That address is private or reserved and cannot be fetched.".to_string(),
                );
            }
        }
    }
    Ok(())
}

/// Normalizes what the user typed into an absolute URL and validates it.
pub fn normalize_input(input: &str) -> Result<Url, String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err("Enter a page URL first.".to_string());
    }
    if trimmed.len() > MAX_URL_BYTES {
        return Err("That URL is too long.".to_string());
    }
    let candidate = if has_scheme(trimmed) {
        trimmed.to_string()
    } else {
        format!("https://{trimmed}")
    };
    let url = Url::parse(&candidate).map_err(|_| "That does not look like a URL.".to_string())?;
    validate_url(&url)?;
    Ok(url)
}

/// Whether the input starts with an explicit scheme (`https:`, `ftp:` ...).
///
/// `example.com:8080/path` is a host and a port, not a scheme, so a bare
/// host:port must not be mistaken for one. `://` is unambiguous; schemes
/// without slashes are matched against the small set the archiver may meet so
/// that `javascript:` is rejected explicitly instead of being turned into a
/// host.
fn has_scheme(input: &str) -> bool {
    if let Some(colon) = input.find("://") {
        return is_scheme_token(&input[..colon]);
    }
    let lower = input.to_ascii_lowercase();
    const SLASHLESS: &[&str] = &[
        "http:", "https:", "ftp:", "file:", "data:", "javascript:", "mailto:", "tel:", "about:",
    ];
    SLASHLESS.iter().any(|prefix| lower.starts_with(prefix))
}

fn is_scheme_token(token: &str) -> bool {
    let bytes = token.as_bytes();
    if bytes.is_empty() || !bytes[0].is_ascii_alphabetic() {
        return false;
    }
    bytes
        .iter()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'-' | b'.'))
}

fn is_public_ipv4(ip: Ipv4Addr) -> bool {
    let [a, b, c, _d] = ip.octets();
    !(a == 0
        || a == 10
        || a == 127
        || (a == 100 && (64..=127).contains(&b))
        || (a == 169 && b == 254)
        || (a == 172 && (16..=31).contains(&b))
        || (a == 192 && b == 0 && (c == 0 || c == 2))
        || (a == 192 && b == 88 && c == 99)
        || (a == 192 && b == 168)
        || (a == 198 && (b == 18 || b == 19))
        || (a == 198 && b == 51 && c == 100)
        || (a == 203 && b == 0 && c == 113)
        || a >= 224)
}

fn is_public_ipv6(ip: Ipv6Addr) -> bool {
    if let Some(mapped) = ip.to_ipv4_mapped() {
        return is_public_ipv4(mapped);
    }
    let s = ip.segments();
    let unspecified = s[..7].iter().all(|part| *part == 0) && s[7] == 0;
    let loopback = s[..7].iter().all(|part| *part == 0) && s[7] == 1;
    !(unspecified
        || loopback
        || (s[0] & 0xffc0) == 0xfe80
        || (s[0] & 0xfe00) == 0xfc00
        || (s[0] & 0xff00) == 0xff00
        || (s[0] == 0x0064 && s[1] == 0xff9b)
        || (s[0] == 0x0100 && s[1] == 0 && s[2] == 0 && s[3] == 0)
        || (s[0] == 0x2001 && s[1] == 0x0000)
        || (s[0] == 0x2001 && s[1] == 0x0db8)
        || s[0] == 0x2002)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check(url: &str) -> Result<Url, String> {
        let parsed = Url::parse(url).map_err(|error| error.to_string())?;
        validate_url(&parsed)?;
        Ok(parsed)
    }

    #[test]
    fn accepts_public_web_urls() {
        assert!(check("https://example.com/").is_ok());
        assert!(check("http://example.com/docs?q=1#part").is_ok());
        assert!(check("http://93.184.216.34/").is_ok());
        assert!(check("https://example.com:443/").is_ok());
    }

    #[test]
    fn rejects_other_schemes() {
        assert!(check("file:///C:/Windows/win.ini").is_err());
        assert!(check("ftp://example.com/file").is_err());
        assert!(check("javascript:alert(1)").is_err());
        assert!(check("data:text/html,hello").is_err());
    }

    #[test]
    fn rejects_local_and_private_addresses() {
        for url in [
            "http://localhost/",
            "http://localhost./",
            "http://foo.localhost/",
            "http://printer.local/",
            "http://metadata.google.internal/",
            "http://intranet/",
            "http://127.0.0.1/",
            "http://127.1/",
            "http://0.0.0.0/",
            "http://10.0.0.1/",
            "http://172.16.0.1/",
            "http://172.31.255.255/",
            "http://192.168.1.1/",
            "http://169.254.169.254/",
            "http://100.64.0.1/",
            "http://198.18.0.1/",
            "http://203.0.113.9/",
            "http://[::1]/",
            "http://[fe80::1]/",
            "http://[fc00::1]/",
            "http://[::ffff:127.0.0.1]/",
        ] {
            assert!(check(url).is_err(), "{url} should be rejected");
        }
    }

    #[test]
    fn rejects_credentials_and_odd_ports() {
        assert!(check("https://user:pass@example.com/").is_err());
        assert!(check("https://user@example.com/").is_err());
        assert!(check("https://example.com:8080/").is_err());
    }

    #[test]
    fn normalizes_bare_hosts_to_https() {
        let url = normalize_input("example.com/docs").unwrap();
        assert_eq!(url.scheme(), "https");
        assert_eq!(url.host_str(), Some("example.com"));
    }

    #[test]
    fn distinguishes_scheme_from_port() {
        assert!(!has_scheme("example.com:8080/path"));
        assert!(has_scheme("https://example.com"));
        assert!(has_scheme("HtTp://example.com"));
        assert!(has_scheme("ftp://example.com"));
        assert!(has_scheme("javascript:alert(1)"));
        assert!(normalize_input("example.com:8080/path").is_err());
    }

    #[test]
    fn classifies_more_addresses() {
        assert!(is_public_ip("1.1.1.1".parse().unwrap()));
        assert!(!is_public_ip("192.0.2.1".parse().unwrap()));
        assert!(!is_public_ip("255.255.255.255".parse().unwrap()));
        assert!(is_public_ip("2606:4700:4700::1111".parse().unwrap()));
        assert!(!is_public_ip("2001:db8::1".parse().unwrap()));
        assert!(!is_public_ip("2002::1".parse().unwrap()));
    }
}
