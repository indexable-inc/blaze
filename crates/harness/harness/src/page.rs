//! A web page's card for the app's hover over a `<Link>` chip (`page_peek`): its OpenGraph title, description, site
//! name and image, fetched once per URL and kept for the daemon's life (a failure for [`FAILED_TTL`] seconds).
//!
//! The core ([`meta`], [`icons`], [`ico_png`]) reads a page's `<head>` and an icon file with no IO; the driver ([`peek`])
//! fetches with `curl` (http and https only, redirects too, bounded in time and size) and saves the `og:image` and the
//! favicon under `$HARNESS_HOME/peek/` so the app reads them as files. A favicon is kept only as a PNG or JPEG, the
//! pictures the app draws: an ICO gives up its PNG image, or on macOS `sips` converts it; an SVG one is passed over.
//! What could not be fetched comes back with `error` naming the URL and why; a page with no favicon the app can draw
//! has an empty `icon` (the app's globe).
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
const ICON_MAX: &str = "1000000";
/// The most of a page's icons tried before it has none.
const ICONS_TRIED: usize = 4;
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
        icon: String::new(),
        error: String::new(),
    }
}

/// The icons of page `url` (its HTML `html`) in the order worth trying, each absolute: `apple-touch-icon` links (PNGs,
/// large), then `icon` links that are PNGs (largest `sizes` first), then other `icon` links that are not SVG (an ICO),
/// then the site's `/favicon.ico`. SVG and mask icons are left out: the app draws PNG and JPEG.
#[must_use]
pub fn icons(url: &str, html: &str) -> Vec<String> {
    let head = &html[..html.find("</head>").unwrap_or(html.len())];
    let lower = head.to_ascii_lowercase();
    let mut found: Vec<(u8, i64, usize, String)> = Vec::new();
    let mut from = 0;
    while let Some(i) = lower[from..].find("<link").map(|i| from + i) {
        let end = lower[i..].find('>').map_or(lower.len(), |e| i + e);
        let tag = &head[i..end];
        from = end;
        let (Some(rel), Some(href)) = (attr(tag, "rel"), attr(tag, "href")) else { continue };
        let rel = rel.to_ascii_lowercase();
        let ty = attr(tag, "type").unwrap_or_default().to_ascii_lowercase();
        let path = href.split(['?', '#']).next().unwrap_or(&href).to_ascii_lowercase();
        let svg = ty.contains("svg") || path.ends_with(".svg");
        let png = ty.contains("png") || path.ends_with(".png");
        let tokens: Vec<&str> = rel.split_whitespace().collect();
        let rank = if tokens.iter().any(|t| t.starts_with("apple-touch-icon")) {
            0
        } else if tokens.contains(&"icon") && png {
            1
        } else if tokens.contains(&"icon") && !svg {
            2
        } else {
            continue;
        };
        // The largest of `sizes` ("32x32 64x64", "any": none), larger first.
        let size = attr(tag, "sizes")
            .unwrap_or_default()
            .split_whitespace()
            .filter_map(|s| s.split_once(['x', 'X']).and_then(|(w, _)| w.parse::<i64>().ok()))
            .max()
            .unwrap_or(0);
        if let Some(abs) = absolute(url, &entities(&href)) {
            found.push((rank, -size, found.len(), abs));
        }
    }
    found.sort();
    let mut out: Vec<String> = Vec::new();
    for (.., u) in found {
        if !out.contains(&u) {
            out.push(u);
        }
    }
    if let Some(scheme) = url.split_once("://").map(|(s, _)| s) {
        let ico = format!("{scheme}://{}/favicon.ico", host(url));
        if web(&ico) && !out.contains(&ico) {
            out.push(ico);
        }
    }
    out
}

/// The largest PNG image inside ICO file `bytes` (a modern favicon.ico keeps its big sizes as PNGs); None when it is
/// not an ICO or holds only bitmaps.
#[must_use]
pub fn ico_png(bytes: &[u8]) -> Option<&[u8]> {
    let le16 = |at: usize| bytes.get(at..at + 2).map(|b| u16::from_le_bytes([b[0], b[1]]) as usize);
    let le32 = |at: usize| bytes.get(at..at + 4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as usize);
    if le16(0)? != 0 || le16(2)? != 1 {
        return None;
    }
    let count = le16(4)?.min(64);
    let mut best: Option<(usize, &[u8])> = None;
    for k in 0..count {
        let e = 6 + 16 * k;
        let width = match *bytes.get(e)? {
            0 => 256,
            w => usize::from(w),
        };
        let (size, offset) = (le32(e + 8)?, le32(e + 12)?);
        let Some(data) = offset.checked_add(size).and_then(|end| bytes.get(offset..end)) else { continue };
        if data.starts_with(b"\x89PNG\r\n\x1a\n") && best.is_none_or(|(w, _)| width > w) {
            best = Some((width, data));
        }
    }
    best.map(|(_, d)| d)
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
    let html = String::from_utf8_lossy(&ran.stdout);
    let mut m = meta(url, &html);
    if !m.image.is_empty() {
        // The image as a file the app reads (it draws pictures from paths); none when it could not be fetched.
        m.image = save_image(&m.image).unwrap_or_default();
    }
    // The first of its icons that comes as a picture the app draws; none: the app's globe.
    m.icon = icons(url, &html).iter().take(ICONS_TRIED).find_map(|i| save_icon(i)).unwrap_or_default();
    m
}

/// Icon `src` saved under `$HARNESS_HOME/peek/` as a PNG or JPEG, by a hash of its URL (pages of one site share it), its
/// path; None when it could not be fetched or is no picture the app draws. A failure is kept [`FAILED_TTL`] seconds.
fn save_icon(src: &str) -> Option<String> {
    use std::hash::{Hash, Hasher};
    static FAILED: Mutex<BTreeMap<String, f64>> = Mutex::new(BTreeMap::new());
    if FAILED.lock().get(src).is_some_and(|at| now() - at < FAILED_TTL) {
        return None;
    }
    let mut h = std::collections::hash_map::DefaultHasher::new();
    ("icon", src).hash(&mut h);
    let dir = crate::paths::home().join("peek");
    let path = dir.join(format!("{:016x}.icon", h.finish()));
    if path.exists() {
        return Some(path.display().to_string());
    }
    let saved = (|| {
        std::fs::create_dir_all(&dir).ok()?;
        let ran = run_bounded(curl(src, ICON_MAX), &[], FETCH_WAIT, "curl (icon)").ok()?;
        if ran.code != Some(0) {
            return None;
        }
        let bytes = ran.stdout;
        let picture = bytes.starts_with(b"\x89PNG") || bytes.starts_with(&[0xFF, 0xD8, 0xFF]);
        let tmp = path.with_extension("part");
        if picture {
            std::fs::write(&tmp, &bytes).ok()?;
        } else if let Some(png) = ico_png(&bytes) {
            std::fs::write(&tmp, png).ok()?;
        } else if bytes.starts_with(&[0, 0, 1, 0]) {
            sips_png(&bytes, &tmp)?;
        } else {
            return None;
        }
        std::fs::rename(&tmp, &path).ok()?;
        Some(path.display().to_string())
    })();
    if saved.is_none() {
        FAILED.lock().insert(src.to_owned(), now());
    }
    saved
}

/// ICO `bytes` of bitmaps converted to a PNG at `out` by macOS's `sips` (ImageIO reads ICO); None where it cannot.
#[cfg(target_os = "macos")]
fn sips_png(bytes: &[u8], out: &std::path::Path) -> Option<()> {
    let ico = out.with_extension("ico");
    std::fs::write(&ico, bytes).ok()?;
    let mut c = Command::new("/usr/bin/sips");
    c.args(["-s", "format", "png"]).arg(&ico).arg("--out").arg(out);
    let ran = run_bounded(c, &[], FETCH_WAIT, "sips (icon)");
    let _ = std::fs::remove_file(&ico);
    let ok = ran.ok()?.code == Some(0) && std::fs::read(out).ok()?.starts_with(b"\x89PNG");
    ok.then_some(())
}

#[cfg(not(target_os = "macos"))]
fn sips_png(_: &[u8], _: &std::path::Path) -> Option<()> {
    None
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

    /// A page's icons: touch icons, then PNG icons largest first, then an ICO, then the site's favicon.ico; never SVG.
    #[test]
    fn finds_the_icons() {
        let html = r#"<head>
            <link rel="icon" type="image/svg+xml" href="/f.svg">
            <link rel="shortcut icon" href="/favicon.ico?v=2">
            <link rel="alternate icon" type="image/png" sizes="16x16" href="/f16.png">
            <link rel="icon" sizes="32x32" href="https://cdn.x/f32.png">
            <link rel="apple-touch-icon" href="/touch.png">
            <link rel="mask-icon" href="/mask.svg">
            <link rel="stylesheet" href="/s.css">
            </head>"#;
        assert_eq!(
            icons("https://www.x.dev/a/b", html),
            [
                "https://www.x.dev/touch.png",
                "https://cdn.x/f32.png",
                "https://www.x.dev/f16.png",
                "https://www.x.dev/favicon.ico?v=2",
                "https://www.x.dev/favicon.ico",
            ]
        );
        assert_eq!(icons("https://a.b/c", "<head></head>"), ["https://a.b/favicon.ico"]);
    }

    /// An ICO's largest PNG image; an ICO of bitmaps or what is no ICO gives none.
    #[test]
    fn takes_the_png_out_of_an_ico() {
        let png = |n: u8| [b"\x89PNG\r\n\x1a\n".as_slice(), &[n; 4][..]].concat();
        let entry = |w: u8, size: usize, offset: usize| {
            let mut e = vec![w, w, 0, 0, 1, 0, 32, 0];
            e.extend((size as u32).to_le_bytes());
            e.extend((offset as u32).to_le_bytes());
            e
        };
        let (small, big, bmp) = (png(1), png(2), vec![40u8, 0, 0, 0, 9, 9]);
        let base = 6 + 16 * 3;
        let mut ico = vec![0, 0, 1, 0, 3, 0];
        ico.extend(entry(16, small.len(), base));
        ico.extend(entry(0, big.len(), base + small.len()));
        ico.extend(entry(48, bmp.len(), base + small.len() + big.len()));
        ico.extend([small.clone(), big.clone(), bmp].concat());
        assert_eq!(ico_png(&ico), Some(big.as_slice()));
        assert_eq!(ico_png(&ico[..base + small.len()]), Some(small.as_slice()), "a cut entry is passed over");
        assert_eq!(ico_png(b"\x89PNG\r\n\x1a\n"), None);
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
