//! Text helpers as render/tree.ts has them.

use crate::js;

/// A multiline string as written: drop the indent the script added and the blank edges.
pub fn dedent(text: &str) -> String {
    let text = text.replace('\r', "");
    let lines: Vec<&str> = text.split('\n').collect();
    let indent = lines[1..]
        .iter()
        .filter(|l| !js::trim(l).is_empty())
        .map(|l| l.bytes().take_while(|b| *b == b' ').count())
        .min();
    let mut body: Vec<&str> = std::iter::once(lines[0])
        .chain(lines[1..].iter().map(|l| {
            // `l.slice(indent)`: the indent is spaces, so a byte offset is a UTF-16 offset; past the end is empty.
            let i = indent.unwrap_or(0);
            match l.char_indices().nth(i) {
                Some((at, _)) => &l[at..],
                None => "",
            }
        }))
        .collect();
    while body.first().is_some_and(|l| js::trim(l).is_empty()) {
        body.remove(0);
    }
    while body.last().is_some_and(|l| js::trim(l).is_empty()) {
        body.pop();
    }
    body.join("\n")
}

/// Prose: one line, whatever the script's line breaks (`/\s*\n\s*/g` to a space, then trim).
pub fn prose(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.char_indices().peekable();
    let mut pending = String::new();
    while let Some((_, c)) = chars.next() {
        if js::is_space(c) {
            // A run of whitespace: one space if it holds a newline, else itself.
            pending.clear();
            pending.push(c);
            while let Some((_, n)) = chars.peek().copied() {
                if !js::is_space(n) {
                    break;
                }
                pending.push(n);
                chars.next();
            }
            if pending.contains('\n') {
                out.push(' ');
            } else {
                out.push_str(&pending);
            }
        } else {
            out.push(c);
        }
    }
    js::trim(&out).to_string()
}
