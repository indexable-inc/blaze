//! A UI view's navigation lock, sans IO (README.md, "UI views"): the origin a UI view was opened at, and whether a
//! top-level navigation stays inside it. A `file://` page's origin is its own file (the path, percent-escapes decoded,
//! the query and fragment ignored): one file of the app's may not walk to another file on disk. An `http`/`https`
//! page's is its scheme, host and port (the port the scheme implies when none is written). No other scheme opens a UI
//! view.

/// Where a UI view may be.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Origin {
    /// One file, its path decoded.
    File(Vec<u8>),
    Http { https: bool, host: String, port: u16 },
}

/// `s` with its `%XX` escapes decoded (a `%` not followed by two hex digits is kept as it is).
fn unescape(s: &str) -> Vec<u8> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len()); // alloc: one path per navigation of a UI view
    let mut i = 0;
    while i < b.len() {
        let hex = |c: u8| char::from(c).to_digit(16);
        if b[i] == b'%'
            && let (Some(hi), Some(lo)) = (b.get(i + 1).copied().and_then(hex), b.get(i + 2).copied().and_then(hex))
        {
            out.push((hi * 16 + lo) as u8);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    out
}

/// `url`'s scheme, lowercased, and the rest after `://`.
fn split(url: &str) -> Option<(String, &str)> {
    let (scheme, rest) = url.split_once("://")?;
    Some((scheme.to_ascii_lowercase(), rest))
}

impl Origin {
    /// The origin of `url`, the first url of a UI view.
    ///
    /// # Errors
    /// A url that is not `file://`, `http://` or `https://`, or one whose host or port cannot be read, naming it.
    pub fn of(url: &str) -> Result<Self, String> {
        let bad = |why: &str| format!("a UI view cannot open at {url}: {why}");
        let (scheme, rest) = split(url).ok_or_else(|| bad("it has no scheme (a UI view is file://, http:// or https://)"))?;
        let rest = rest.split(['?', '#']).next().unwrap_or("");
        match scheme.as_str() {
            "file" => {
                // file:///p, or file://localhost/p: no other host names a file here.
                let (host, path) = rest.find('/').map_or((rest, ""), |i| rest.split_at(i));
                if !(host.is_empty() || host.eq_ignore_ascii_case("localhost")) {
                    return Err(bad(&format!("a file url names the host {host}")));
                }
                if path.is_empty() {
                    return Err(bad("a file url with no path"));
                }
                Ok(Self::File(unescape(path)))
            }
            "http" | "https" => {
                let https = scheme == "https";
                let authority = rest.split('/').next().unwrap_or("");
                let hostport = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
                // A bracketed IPv6 host keeps its colons; the port is after the bracket.
                let (host, port) = match hostport.strip_prefix('[') {
                    Some(v6) => {
                        let (h, after) = v6.split_once(']').ok_or_else(|| bad("an IPv6 host with no closing bracket"))?;
                        (h, after.strip_prefix(':'))
                    }
                    None => match hostport.rsplit_once(':') {
                        Some((h, p)) => (h, Some(p)),
                        None => (hostport, None),
                    },
                };
                if host.is_empty() {
                    return Err(bad("it names no host"));
                }
                let port = match port.filter(|p| !p.is_empty()) {
                    Some(p) => p.parse::<u16>().map_err(|e| bad(&format!("its port {p:?}: {e}")))?,
                    None if https => 443,
                    None => 80,
                };
                Ok(Self::Http { https, host: host.to_ascii_lowercase(), port })
            }
            other => Err(bad(&format!("its scheme is {other} (a UI view is file://, http:// or https://)"))),
        }
    }

    /// Whether a top-level navigation to `url` stays in this origin.
    #[must_use]
    pub fn admits(&self, url: &str) -> bool {
        Self::of(url).is_ok_and(|o| o == *self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_file_page_is_its_own_file() {
        let o = Origin::of("file:///Users/a/web/dist/index.html").unwrap();
        assert!(o.admits("file:///Users/a/web/dist/index.html#lesson"));
        assert!(o.admits("file:///Users/a/web/dist/index.html?mock=start"));
        assert!(o.admits("file://localhost/Users/a/web/dist/index.html"));
        assert!(o.admits("FILE:///Users/a/web/dist/%69ndex.html"), "escapes are decoded, the scheme's case ignored");
        assert!(!o.admits("file:///Users/a/web/dist/other.html"));
        assert!(!o.admits("file:///etc/passwd"));
        assert!(!o.admits("https://example.com/"));
        assert!(!o.admits("about:blank"));
    }

    #[test]
    fn an_http_page_is_its_scheme_host_and_port() {
        let o = Origin::of("http://localhost:5173/?mock=start").unwrap();
        assert!(o.admits("http://localhost:5173/"));
        assert!(o.admits("http://LOCALHOST:5173/lesson/2#x"));
        assert!(!o.admits("http://localhost:5174/"));
        assert!(!o.admits("https://localhost:5173/"));
        assert!(!o.admits("http://127.0.0.1:5173/"));
        let d = Origin::of("https://example.com").unwrap();
        assert!(d.admits("https://example.com:443/a"), "the port the scheme implies");
        assert!(d.admits("https://user@example.com/"));
        assert!(!d.admits("http://example.com/"));
        let v6 = Origin::of("http://[::1]:8080/").unwrap();
        assert_eq!(v6, Origin::Http { https: false, host: "::1".into(), port: 8080 });
    }

    #[test]
    fn other_schemes_and_broken_urls_are_refused() {
        for url in ["about:blank", "chrome://extensions", "data:text/html,x", "file://host/p", "file://", "http://", "http://h:99999/", "http://[::1/"] {
            let e = Origin::of(url).unwrap_err();
            assert!(e.contains(url), "{e}");
        }
    }
}
