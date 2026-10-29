//! Inline markup in a text field (content/schema.flecs), as render/inline.ts: `code`, **strong**, *emphasis*,
//! [label](url), [[name]] and [label][name] for a reference under `refs`. Everything else is text, escaped. An
//! unknown reference or a link that is not http(s), / or # is an error.

use std::sync::LazyLock;

use regex_lite::Regex;

use crate::js;
use crate::render::Served;
use crate::world::{Id, World};

pub fn esc(t: &str) -> String {
    let mut out = String::with_capacity(t.len());
    for c in t.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            c => out.push(c),
        }
    }
    out
}

// A constant pattern, compiled once; not state that changes.
static TOKEN: LazyLock<Regex> = LazyLock::new(|| {
    let s = js::SPACE_CLASS;
    // JS `.` stops at any line terminator; JS `\w` is ASCII, as regex-lite's is.
    let dot = "[^\n\r\u{2028}\u{2029}]";
    Regex::new(&format!(
        r"`([^`]+)`|\*\*({dot}+?)\*\*|\*({dot}+?)\*|\[\[([\w-]+)\]\]|\[([^\]]+)\]\[([\w-]+)\]|\[([^\]]+)\]\(([^){s}]+)\)"
    ))
    .expect("TOKEN compiles")
});

fn safe_href(h: &str) -> bool {
    h.starts_with("http://")
        || h.starts_with("https://")
        || h.starts_with('/')
        || h.starts_with('#')
}

/// The references a page can name: `refs`'s children, by name.
pub struct Refs<'w> {
    pub world: &'w World,
    pub parent: Option<Id>,
    /// Where the site is served: a link to a site path (`/rfcs/x`) goes under its base.
    pub served: &'w Served,
}

impl Refs<'_> {
    pub fn get(&self, name: &str) -> Option<Id> {
        let p = self.parent?;
        self.world.get(p).children.iter().copied().find(|c| self.world.get(*c).name == name)
    }
}

/// `text` as HTML. `cited` collects the references it names, in order of first use.
pub fn inline(text: &str, refs: &Refs, wher: &str, cited: &mut Vec<Id>) -> Result<String, String> {
    let mut html = String::new();
    let mut last = 0;
    for m in TOKEN.captures_iter(text) {
        let whole = m.get(0).expect("group 0");
        html += &esc(&text[last..whole.start()]);
        last = whole.end();
        let g = |i: usize| m.get(i).map(|x| x.as_str());
        if let Some(code) = g(1) {
            html += &format!("<code>{}</code>", esc(code));
        } else if let Some(strong) = g(2) {
            html += &format!("<strong>{}</strong>", inline(strong, refs, wher, cited)?);
        } else if let Some(em) = g(3) {
            html += &format!("<em>{}</em>", inline(em, refs, wher, cited)?);
        } else if let Some(label) = g(7) {
            let href = g(8).expect("label has href");
            if !safe_href(href) {
                return Err(format!("{wher}: link {href} is not http(s), / or #"));
            }
            html += &format!(
                "<a href=\"{}\">{}</a>",
                esc(&refs.served.href(href)),
                inline(label, refs, wher, cited)?
            );
        } else {
            let name = g(4).or(g(6)).expect("a reference");
            let r = refs.get(name).ok_or_else(|| format!("{wher}: no reference refs.{name}"))?;
            if !cited.contains(&r) {
                cited.push(r);
            }
            let v = refs
                .world
                .get(r)
                .value("Ref")
                .ok_or_else(|| format!("{wher}: refs.{name} is not a Ref"))?;
            let shown = match g(5) {
                Some(label) => inline(label, refs, wher, cited)?,
                None => esc(&js::string(v.get("label"))),
            };
            html += &format!(
                "<a href=\"{}\">{shown}</a>",
                esc(&refs.served.href(&js::string(v.get("href"))))
            );
        }
    }
    Ok(html + &esc(&text[last..]))
}
