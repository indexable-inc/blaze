//! A local file as a page the pane shows (`App::open_target`): markdown rendered as a document, any other text as its
//! numbered lines, with the part a chip or a citation points at (its line) marked and scrolled into view. The page is
//! written under `$HARNESS_HOME/view/`, one file a source path (written again at each open, so it shows the file as it
//! is now), and opened as a `file://` page: Chromium on its own shows a markdown file as its source and has no line to
//! go to.
//!
//! A markdown file's front matter and its comments (a generated file's banner) are not shown.
//!
//! A folder, a binary file, a web page's own formats (html, svg, pdf, a picture) or a file too large to read whole stay
//! what the pane showed before: the file itself.
use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd, html};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

/// The most of a file a page is made of.
const MOST: u64 = 2 << 20;
/// What Chromium shows well itself.
const NATIVE: [&str; 12] = ["html", "htm", "svg", "pdf", "png", "jpg", "jpeg", "gif", "webp", "mp4", "mov", "webm"];

/// The page for `path`, its `line` (1-based) marked: the page's own path, or None when the file is shown as it is.
pub fn page(path: &str, line: Option<u32>) -> Option<PathBuf> {
    let ext = Path::new(path).extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
    let meta = std::fs::metadata(path).ok()?;
    if !meta.is_file() || meta.len() > MOST || NATIVE.contains(&ext.as_str()) {
        return None;
    }
    let bytes = std::fs::read(path).ok()?;
    if bytes.iter().take(4096).any(|&b| b == 0) {
        return None;
    }
    let text = String::from_utf8_lossy(&bytes);
    let body = if matches!(ext.as_str(), "md" | "mdx" | "markdown") { markdown(&text, line) } else { lines(&text, line) };
    let html = format!(
        "<!doctype html><meta charset=utf-8><title>{}</title><style>{CSS}</style>{body}<script>{JS}</script>",
        escape(path)
    );
    let dir = crate::link::home().join("view");
    std::fs::create_dir_all(&dir).ok()?;
    // One page a source path: its name the path's hash, so opening the same file again replaces its page.
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    path.hash(&mut h);
    let out = dir.join(format!("{:016x}.html", h.finish()));
    std::fs::write(&out, html).ok()?;
    Some(out)
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

/// A text file as numbered lines, `line` marked.
fn lines(text: &str, line: Option<u32>) -> String {
    let mut out = String::from("<pre class=code>"); // alloc: the page, built once an open
    for (i, l) in text.lines().enumerate() {
        let n = i + 1;
        let cited = if line == Some(n as u32) { " class=cited id=cite" } else { "" };
        let _ = write!(out, "<div{cited}><span class=n>{n}</span>{}</div>", escape(l));
    }
    out.push_str("</pre>");
    out
}

/// A markdown file as a document: each top-level block its own element, the one `line` falls in marked.
fn markdown(text: &str, line: Option<u32>) -> String {
    // The byte each line starts at, to find the block a line is in.
    let at = line.and_then(|n| text.split_inclusive('\n').take(n.saturating_sub(1) as usize).map(str::len).sum::<usize>().into());
    let opts = Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS | Options::ENABLE_YAML_STYLE_METADATA_BLOCKS;
    let mut out = String::from("<article>"); // alloc: the page, built once an open
    let (mut depth, mut block, mut from) = (0usize, Vec::new(), 0usize);
    for (ev, range) in Parser::new_ext(text, opts).into_offset_iter() {
        let (opens, closes) = (matches!(ev, Event::Start(_)), matches!(ev, Event::End(_)));
        if depth == 0 {
            from = range.start;
        }
        depth += usize::from(opens);
        // A raw HTML comment (a generated file's banner) is shown as nothing.
        let ev = match ev {
            Event::Html(h) | Event::InlineHtml(h) if h.trim_start().starts_with("<!--") => continue,
            // A list is one block; its items are cited one by one below.
            ev => ev,
        };
        block.push((ev, range.clone()));
        depth -= usize::from(closes);
        if depth == 0 {
            // Front matter (a skill's name and description) is the file's own bookkeeping: not shown.
            if matches!(block.first(), Some((Event::Start(Tag::MetadataBlock(_)), _))) {
                block.clear();
                continue;
            }
            let end = block.last().map_or(from, |(_, r)| r.end);
            let cited = at.is_some_and(|a| a >= from && a < end.max(from + 1));
            // In a cited list, the item the line is in is the part cited; the list around it is not.
            let item = cited.then(|| {
                block.iter().find_map(|(e, r)| (matches!(e, Event::Start(Tag::Item)) && at.is_some_and(|a| r.contains(&a))).then(|| r.clone()))
            });
            let mut inner = String::new();
            match item.flatten() {
                Some(item) => {
                    let mut marked = Vec::with_capacity(block.len() + 2); // alloc: one block's events
                    for (e, r) in block.drain(..) {
                        let (open, close) = (matches!(e, Event::Start(Tag::Item)) && r == item, matches!(e, Event::End(TagEnd::Item)) && r == item);
                        if open {
                            marked.push(Event::Html("<li class=cited id=cite>".into()));
                        } else if close {
                            marked.push(Event::Html("</li>".into()));
                        } else {
                            marked.push(e);
                        }
                    }
                    html::push_html(&mut inner, marked.into_iter());
                    out.push_str(&inner);
                }
                None => {
                    html::push_html(&mut inner, block.drain(..).map(|(e, _)| e));
                    if cited {
                        let _ = write!(out, "<div class=cited id=cite>{inner}</div>");
                    } else {
                        out.push_str(&inner);
                    }
                }
            }
        }
    }
    out.push_str("</article>");
    out
}

/// The page's look: the app's ground and inks in both appearances (scene/00-tokens.flecs's, as a page), one measure,
/// the cited part a wash with a bar at its side.
const CSS: &str = "
:root{color-scheme:dark light;--bg:#0a0b0d;--ink:#c2c5cc;--strong:#edeff2;--muted:#8b9099;--faint:#5a5f69;--line:#1a1c21;--well:#070809;--wash:rgba(255,255,255,.06);--link:#7aa7ff}
@media(prefers-color-scheme:light){:root{--bg:#f2f1ee;--ink:#46464a;--strong:#161617;--muted:#6b6a6e;--faint:#a2a19d;--line:#dcdbd5;--well:#ebeae5;--wash:rgba(22,22,23,.06);--link:#2675bf}}
html{background:var(--bg);color:var(--ink);font:14px/1.5 Inter,-apple-system,system-ui,sans-serif;-webkit-font-smoothing:antialiased}
body{margin:0;padding:28px 32px 64px}
article{max-width:46em;margin:0 auto}
h1,h2,h3,h4{color:var(--strong);font-weight:600;line-height:1.25;margin:1.4em 0 .4em}
h1{font-size:21px}h2{font-size:18px}h3,h4{font-size:15.5px}
p,ul,ol,pre,table,blockquote{margin:.6em 0}
ul,ol{padding-left:1.5em}li{margin:.2em 0}li::marker{color:var(--faint)}
strong{color:var(--strong);font-weight:600}
a{color:var(--link);text-decoration:none}a:hover{text-decoration:underline}
code{font:12.5px/1.4 'Berkeley Mono',ui-monospace,Menlo,monospace;background:var(--wash);border-radius:5px;padding:.1em .35em}
pre{background:var(--well);border:1px solid var(--line);border-radius:8px;padding:12px 14px;overflow:auto}
pre code{background:none;padding:0}
blockquote{border-left:2px solid var(--line);margin-left:0;padding-left:1em;color:var(--muted)}
table{border-collapse:collapse}th,td{border-bottom:1px solid var(--line);padding:4px 12px 4px 0;text-align:left}th{color:var(--muted);font-weight:500}
hr{border:0;border-top:1px solid var(--line);margin:1.5em 0}
.cited{background:var(--wash);box-shadow:-3px 0 0 var(--strong);border-radius:0 6px 6px 0;margin-left:-10px;padding-left:10px}
li.cited{margin-left:-1.5em;padding-left:1.5em;list-style-position:inside}
pre.code{background:none;border:0;padding:0;margin:0;font:12.5px/1.5 'Berkeley Mono',ui-monospace,Menlo,monospace;white-space:pre}
pre.code .n{display:inline-block;width:4ch;margin-right:2ch;text-align:right;color:var(--faint);user-select:none}
pre.code .cited{margin-left:-10px;padding-left:10px}
";

/// The cited part into the middle of the view.
const JS: &str = "document.getElementById('cite')?.scrollIntoView({block:'center'})";

#[cfg(test)]
mod tests {
    use super::*;

    /// The block a line is in is the one marked: a paragraph whole, in a list the one item; a banner comment is not shown.
    #[test]
    fn the_cited_part_is_marked() {
        let md = "---\nname: x\n---\n<!-- generated -->\n# Title\n\nFirst.\n\n1. **one**: a\n2. **two**: b\n   more of two\n3. **three**: c\n";
        let page = markdown(md, Some(11));
        assert!(!page.contains("generated") && !page.contains("name: x"), "{page}");
        assert_eq!(page.matches("id=cite").count(), 1, "{page}");
        assert!(page.contains("<li class=cited id=cite><strong>two</strong>"), "{page}");
        let para = markdown(md, Some(7));
        assert!(para.contains("<div class=cited id=cite><p>First.</p>"), "{para}");
        assert_eq!(markdown(md, None).matches("cited").count(), 0);
        let code = lines("a\n<b>\nc\n", Some(2));
        assert!(code.contains("<div class=cited id=cite><span class=n>2</span>&lt;b&gt;</div>"), "{code}");
    }
}
