//! A web page's card for the app's hover over a `<Link>` chip (`page_peek`): its OpenGraph title, description, site
//! name and image, fetched once per URL and kept for the daemon's life (a failure for [`FAILED_TTL`] seconds).
//!
//! The core ([`meta`]) reads a page's `<head>` with no IO; the driver ([`peek`]) fetches with `curl` (http and https
//! only, redirects too, bounded in time and size) and saves the `og:image` under `$HARNESS_HOME/peek/` so the app reads
//! it as a file. What could not be fetched comes back with `error` naming the URL and why.
use crate::github::run_bounded;
use harness_proto::peek::PageMeta;
use parking_lot::Mutex;
use std::collections::BTreeMap;
use std::process::Command;
use std::time::{Duration, Instant};

/// How long a failure is kept before a hover tries again (seconds).
pub const FAILED_TTL: f64 = 60.0;
/// The most a page's HTML or its image may be, and how long either fetch may take.
const PAGE_MAX: &str = "2000000";
const IMAGE_MAX: &str = "5000000";
const FETCH_WAIT: Duration = Duration::from_secs(12);
/// The most text kept of a description.
const DESCRIPTION_MAX: usize = 400;

/// The card in `html`, a page fetched from `url`: `og:` properties first, then `twitter:` ones, then `<title>` and the
/// meta description; the image made absolute against `url`.
#[must_use]
pub fn meta(url: &str, html: &str) -> PageMeta {
    let head = &html[..html.find("</head>").unwrap_or(html.len()).min(html.len())];
    let mut props: BTreeMap<String, String> = BTreeMap::new();
    let mut rest = head;
    while let Some(i) = rest.find("<meta") {
        let tag_end = rest[i..].find('>').map_or(rest.len(), |e| i + e);
        let tag = &rest[i..tag_end];
        let key = attr(tag, "property").or_else(|| attr(tag, "name")).map(|k| k.to_ascii_lowercase());
        if let (Some(k), Some(v)) = (key, attr(tag, "content")) {
            props.entry(k).or_insert_with(|| entities(&v));
        }
        rest = &rest[tag_end.min(rest.len())..];
        if rest.is_empty() {
            break;
        }
    }
    let first = |keys: &[&str]| keys.iter().find_map(|k| props.get(*k).filter(|v| !v.trim().is_empty())).map(|v| v.trim().to_owned());
    let title = first(&["og:title", "twitter:title"]).or_else(|| {
        let lower = head.to_ascii_lowercase();
        let s = lower.find("<title")?;
        let open = s + lower[s..].find('>')? + 1;
        let close = open + lower[open..].find("</title")?;
        Some(entities(head[open..close].trim()))
    });
    let description = first(&["og:description", "twitter:description", "description"]).unwrap_or_default();
    let image = first(&["og:image", "og:image:url", "twitter:image"]).and_then(|i| absolute(url, &i)).unwrap_or_default();
    PageMeta {
        url: url.to_owned(),
        title: title.unwrap_or_default(),
        description: cut(&description, DESCRIPTION_MAX),
        site: first(&["og:site_name"]).unwrap_or_else(|| host(url).to_owned()),
        image,
        error: String::new(),
    }
}

/// Attribute `name`'s value in one tag (`"..."` or `'...'` quoted).
fn attr(tag: &str, name: &str) -> Option<String> {
    let lower = tag.to_ascii_lowercase();
    let mut from = 0;
    while let Some(i) = lower[from..].find(name).map(|i| from + i) {
        from = i + name.len();
        let before_ok = i == 0 || lower.as_bytes()[i - 1].is_ascii_whitespace();
        let after = lower[from..].trim_start();
        if !before_ok || !after.starts_with('=') {
            continue;
        }
        let at = tag.len() - after.len() + 1;
        let v = tag[at..].trim_start();
        let q = v.chars().next()?;
        if q != '"' && q != '\'' {
            return Some(v.split(|c: char| c.is_whitespace() || c == '>' || c == '/').next().unwrap_or_default().to_owned());
        }
        return v[1..].find(q).map(|e| v[1..1 + e].to_owned());
    }
    None
}

/// HTML's common character references decoded.
fn entities(s: &str) -> String {
    let mut out = s.replace("&quot;", "\"").replace("&#39;", "'").replace("&#x27;", "'").replace("&lt;", "<").replace("&gt;", ">");
    out = out.replace("&nbsp;", " ").replace("&amp;", "&");
    out
}

fn cut(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_owned();
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}

/// `url`'s host.
fn host(url: &str) -> &str {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    rest.split(['/', '?', '#']).next().unwrap_or(rest)
}

/// `href` made absolute against page `base`; None for what is not http(s) once made so.
fn absolute(base: &str, href: &str) -> Option<String> {
    let href = href.trim();
    let scheme = base.split_once("://")?.0;
    let abs = if href.starts_with("https://") || href.starts_with("http://") {
        href.to_owned()
    } else if let Some(r) = href.strip_prefix("//") {
        format!("{scheme}://{r}")
    } else if href.starts_with('/') {
        format!("{scheme}://{}{href}", host(base))
    } else {
        let dir = base.rsplit_once('/').map_or(base, |(d, _)| d);
        format!("{dir}/{href}")
    };
    web(&abs).then_some(abs)
}

/// Whether `url` is one this fetches: http or https, no white space or control characters, not an option to curl.
#[must_use]
pub fn web(url: &str) -> bool {
    (url.starts_with("https://") || url.starts_with("http://")) && url.len() <= 2048 && !url.chars().any(|c| c.is_whitespace() || c.is_control())
}

static KEPT: Mutex<BTreeMap<String, (f64, PageMeta)>> = Mutex::new(BTreeMap::new());

fn now() -> f64 {
    static START: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_secs_f64()
}

/// The card for `url`: kept, else fetched (and its image saved), never guessed.
pub fn peek(url: &str) -> PageMeta {
    let t = now();
    if let Some((at, m)) = KEPT.lock().get(url)
        && (m.error.is_empty() || t - at < FAILED_TTL)
    {
        return m.clone();
    }
    let m = fetch(url);
    KEPT.lock().insert(url.to_owned(), (now(), m.clone()));
    m
}

fn curl(url: &str, max: &str) -> Command {
    let mut c = Command::new("curl");
    c.args(["--silent", "--show-error", "--fail", "--location", "--max-redirs", "5", "--proto", "=http,https", "--proto-redir", "=http,https"]);
    c.args(["--max-time", "10", "--max-filesize", max, "--user-agent", "harness-peek/1 (link preview)", "--"]);
    c.arg(url);
    c
}

fn fetch(url: &str) -> PageMeta {
    let failed = |why: String| PageMeta { url: url.to_owned(), error: format!("{url}: {why}"), ..PageMeta::default() };
    if !web(url) {
        return failed("not an http or https link".to_owned());
    }
    let ran = match run_bounded(curl(url, PAGE_MAX), &[], FETCH_WAIT, "curl (page)") {
        Ok(r) => r,
        Err(e) => return failed(e),
    };
    if ran.code != Some(0) {
        let err = String::from_utf8_lossy(&ran.stderr);
        return failed(err.lines().next().unwrap_or("curl failed").trim().to_owned());
    }
    let mut m = meta(url, &String::from_utf8_lossy(&ran.stdout));
    if !m.image.is_empty() {
        // The image as a file the app reads (it draws pictures from paths); none when it could not be fetched.
        m.image = save_image(&m.image).unwrap_or_default();
    }
    m
}

/// `src` saved under `$HARNESS_HOME/peek/` by a hash of its URL, its path; None when it could not be fetched.
fn save_image(src: &str) -> Option<String> {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    src.hash(&mut h);
    let dir = crate::paths::home().join("peek");
    std::fs::create_dir_all(&dir).ok()?;
    let path = dir.join(format!("{:016x}.img", h.finish()));
    if path.exists() {
        return Some(path.display().to_string());
    }
    let ran = run_bounded(curl(src, IMAGE_MAX), &[], FETCH_WAIT, "curl (image)").ok()?;
    let png = ran.stdout.starts_with(b"\x89PNG");
    let jpeg = ran.stdout.starts_with(&[0xFF, 0xD8, 0xFF]);
    if ran.code != Some(0) || !(png || jpeg) {
        return None;
    }
    let tmp = path.with_extension("part");
    std::fs::write(&tmp, &ran.stdout).ok()?;
    std::fs::rename(&tmp, &path).ok()?;
    Some(path.display().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_card() {
        let html = r#"<html><head><title>Fallback &amp; more</title>
            <meta property="og:title" content="Builds &amp; caches">
            <meta name='description' content='Plain description'>
            <meta content="blaze" property="og:site_name" />
            <meta property="og:image" content="/img/card.png">
            </head><body><meta property="og:title" content="not this"></body></html>"#;
        let m = meta("https://ix.dev/blaze/learn/builds", html);
        assert_eq!(m.title, "Builds & caches");
        assert_eq!(m.description, "Plain description");
        assert_eq!(m.site, "blaze");
        assert_eq!(m.image, "https://ix.dev/img/card.png");
        let bare = meta("https://a.b/c", "<head><TITLE> Only </TITLE></head>");
        assert_eq!((bare.title.as_str(), bare.site.as_str(), bare.image.as_str()), ("Only", "a.b", ""));
    }

    #[test]
    fn only_web_links() {
        assert!(web("https://x.dev/a?b=c"));
        assert!(!web("file:///etc/passwd"));
        assert!(!web("-o/tmp/x"));
        assert!(!web("https://x.dev/a b"));
        assert_eq!(absolute("https://a.b/c/d", "//cdn.x/i.png").as_deref(), Some("https://cdn.x/i.png"));
        assert_eq!(absolute("https://a.b/c/d", "i.png").as_deref(), Some("https://a.b/c/i.png"));
        assert_eq!(absolute("https://a.b/c/d", "javascript:x"), Some("https://a.b/c/javascript:x".to_owned()));
    }
}
