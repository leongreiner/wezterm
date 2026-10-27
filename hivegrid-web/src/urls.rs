//! Turn what a person (or a script) typed into a URL Chrome can navigate to, and refuse the rest.
//!
//! The allowlist is the same one the Lua and Python normalisers use (shared cases in
//! `tests/data/web_urls.json` of the main repo): the schemes `http`, `https`, `file` and `data`,
//! and exactly `about:blank`. Everything else (`javascript:`, `chrome:`, `devtools:`,
//! `about:flags`, `view-source:`, `blob:`, ...) is refused.

/// Normalise `input` for navigation; `None` when it is not an address this pane may open.
///
/// * an allowed scheme (`http://`, `https://`, `file:`, `data:`) or `about:blank` -> as typed
/// * `localhost`, `*.localhost`, IPv4 and `[IPv6]` hosts (with or without port) -> `http://`
/// * other host names with a dotted alphabetic TLD (`example.com`, `example.com:8080/a`) -> `https://`
/// * anything else (bare words, text with whitespace, paths, `?q`, `#a`, other schemes) -> `None`
pub fn normalize_url(input: &str) -> Option<String> {
    let s = input.trim();
    if s.is_empty() || s.chars().any(char::is_control) {
        return None;
    }
    // An explicit scheme: kept as typed when allowed (a data: URL may hold spaces).
    if let Some(scheme) = scheme_of(s) {
        if !has_port(s) {
            let allowed = matches!(
                scheme.to_ascii_lowercase().as_str(),
                "http" | "https" | "file" | "data"
            ) || s.eq_ignore_ascii_case("about:blank");
            return allowed.then(|| s.to_string());
        }
    }
    if s.chars().any(char::is_whitespace) {
        return None;
    }
    let authority_end = s.find(['/', '?', '#']).unwrap_or(s.len());
    let authority = &s[..authority_end];
    let host = strip_port(authority);
    let local = is_local_host(host);
    if local || is_dns_name(host) {
        let scheme = if local { "http" } else { "https" };
        return Some(format!("{scheme}://{s}"));
    }
    None
}

/// What a typed location stands for, as the web pane's address field and Lua's `normalize_url`
/// agree on it: a URL as [`normalize_url`] reads it, else a search for the words when `search`
/// (a template with `%s`) is given. An address with a scheme that is not allowed
/// (`javascript:`, `chrome:`, ...) is refused, never searched for.
pub fn resolve_location(input: &str, search: Option<&str>) -> Result<String, String> {
    let s = input.trim();
    if s.is_empty() {
        return Err("nothing to open".to_string());
    }
    if s.chars().any(char::is_control) {
        return Err("not a web address".to_string());
    }
    let spaced = s.chars().any(char::is_whitespace);
    if !spaced {
        if let Some(url) = normalize_url(s) {
            return Ok(url);
        }
        if let Some(scheme) = scheme_of(s) {
            if !has_port(s) {
                return Err(format!("cannot open a {scheme}: address in a web pane"));
            }
        }
    }
    match search {
        Some(template) => Ok(template.replace("%s", &percent_encode(s))),
        None => Err("not a web address, and no search engine is set (Settings, Web)".to_string()),
    }
}

/// Everything but unreserved characters (`A-Za-z0-9-_.~`) as `%XX` of its UTF-8 bytes.
fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// The leading `scheme` of `scheme:rest`.
fn scheme_of(s: &str) -> Option<&str> {
    let i = s.find(':')?;
    let scheme = &s[..i];
    let mut chars = scheme.chars();
    (matches!(chars.next(), Some(c) if c.is_ascii_alphabetic())
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.')))
    .then_some(scheme)
}

/// `host:1234`, `host:1234/path`, `[::1]:1234...`: a host with a numeric port, not a scheme.
fn has_port(s: &str) -> bool {
    let (host_ok, rest) = if let Some(r) = s.strip_prefix('[') {
        match r.split_once("]:") {
            Some((h, rest)) if !h.is_empty() && h.chars().all(|c| c.is_ascii_hexdigit() || matches!(c, ':' | '.')) => {
                (true, rest)
            }
            _ => (false, ""),
        }
    } else {
        match s.split_once(':') {
            Some((h, rest)) if !h.is_empty() && !h.contains(['/', '?', '#']) => (true, rest),
            _ => (false, ""),
        }
    };
    if !host_ok {
        return false;
    }
    let digits = rest.chars().take_while(char::is_ascii_digit).count();
    digits > 0 && matches!(rest[digits..].chars().next(), None | Some('/' | '?' | '#'))
}

/// The authority without a trailing `:digits`.
fn strip_port(authority: &str) -> &str {
    match authority.rfind(':') {
        Some(i) if i + 1 < authority.len() && authority[i + 1..].chars().all(|c| c.is_ascii_digit()) => {
            &authority[..i]
        }
        _ => authority,
    }
}

fn is_local_host(host: &str) -> bool {
    let h = host.to_ascii_lowercase();
    if h == "localhost" || h.ends_with(".localhost") {
        return true;
    }
    if h.len() >= 2 && h.starts_with('[') && h.ends_with(']') {
        return true;
    }
    let parts: Vec<&str> = h.split('.').collect();
    parts.len() == 4 && parts.iter().all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()))
}

/// `[A-Za-z0-9.-]+` ending in a dot and at least two letters.
fn is_dns_name(host: &str) -> bool {
    if host.is_empty() || !host.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-')) {
        return false;
    }
    match host.rsplit_once('.') {
        Some((_, tld)) => tld.len() >= 2 && tld.chars().all(|c| c.is_ascii_alphabetic()),
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn n(s: &str) -> Option<String> {
        normalize_url(s)
    }

    #[test]
    fn bare_hosts_get_https() {
        assert_eq!(n("example.com").unwrap(), "https://example.com");
        assert_eq!(n("  example.com/a/b?c=d#e  ").unwrap(), "https://example.com/a/b?c=d#e");
        assert_eq!(n("example.com:8080/a").unwrap(), "https://example.com:8080/a");
        assert_eq!(n("sub.example.co.uk").unwrap(), "https://sub.example.co.uk");
    }

    #[test]
    fn local_hosts_and_ips_get_http() {
        assert_eq!(n("localhost:3000").unwrap(), "http://localhost:3000");
        assert_eq!(n("localhost").unwrap(), "http://localhost");
        assert_eq!(n("app.localhost:8080/x").unwrap(), "http://app.localhost:8080/x");
        assert_eq!(n("127.0.0.1:8080/x").unwrap(), "http://127.0.0.1:8080/x");
        assert_eq!(n("192.168.1.5").unwrap(), "http://192.168.1.5");
        assert_eq!(n("[::1]:9000").unwrap(), "http://[::1]:9000");
        assert_eq!(n("[::1]").unwrap(), "http://[::1]");
    }

    #[test]
    fn allowed_schemes_pass_through() {
        for u in [
            "https://example.com",
            "HTTP://EXAMPLE.COM/",
            "file:///tmp/a.html",
            "file:/tmp/a.html",
            "about:blank",
            "data:text/html,<b>x</b>",
            "data:text/html;base64,PGI+eDwvYj4=",
            "data:text/html,<body style=\"margin:0\">x</body>",
        ] {
            assert_eq!(n(u).as_deref(), Some(u), "{u}");
        }
    }

    #[test]
    fn everything_else_is_refused() {
        for u in [
            "",
            "   \t",
            "javascript:alert(1)",
            "javascript://%0aalert(1)",
            "JavaScript:alert(1)",
            "chrome://settings",
            "chrome://version",
            "devtools://devtools/bundled/inspector.html",
            "about:flags",
            "about:blank#x",
            "about:",
            "view-source:https://example.com",
            "blob:https://example.com/uuid",
            "ftp://host/file",
            "mailto:a@b.co",
            "word",
            "two words",
            "/tmp/x",
            "?q=1",
            "#a",
            "//host/x",
            "http://a\u{1b}]52;c;x\u{7}.example",
        ] {
            assert_eq!(n(u), None, "{u:?}");
        }
    }

    const GOOGLE: &str = "https://www.google.com/search?q=%s";

    #[test]
    fn a_location_is_an_address_or_a_search() {
        let r = |s: &str| resolve_location(s, Some(GOOGLE));
        assert_eq!(r("example.com/a").unwrap(), "https://example.com/a");
        assert_eq!(r("localhost:3000").unwrap(), "http://localhost:3000");
        assert_eq!(r("about:blank").unwrap(), "about:blank");
        assert_eq!(r("rust lifetimes").unwrap(), "https://www.google.com/search?q=rust%20lifetimes");
        assert_eq!(r("word").unwrap(), "https://www.google.com/search?q=word");
        assert_eq!(r("  a&b=c/é  ").unwrap(), "https://www.google.com/search?q=a%26b%3Dc%2F%C3%A9");
        assert_eq!(r("data: hello world").unwrap(), "https://www.google.com/search?q=data%3A%20hello%20world");
    }

    #[test]
    fn a_refused_scheme_is_never_searched_for() {
        for u in ["javascript:alert(1)", "chrome://settings", "about:flags", "view-source:https://x.co", "mailto:a@b.co"] {
            let err = resolve_location(u, Some(GOOGLE)).unwrap_err();
            assert!(err.starts_with("cannot open a "), "{u}: {err}");
        }
    }

    #[test]
    fn without_a_search_engine_words_are_refused_and_empty_is_nothing() {
        assert!(resolve_location("two words", None).unwrap_err().contains("no search engine"));
        assert_eq!(resolve_location("example.com", None).unwrap(), "https://example.com");
        assert_eq!(resolve_location("  \t", Some(GOOGLE)).unwrap_err(), "nothing to open");
        assert!(resolve_location("a\u{1b}b", Some(GOOGLE)).is_err());
    }
}
