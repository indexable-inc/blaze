//! Inline markdown (`MarkdownText`): a paragraph's, heading's or list item's words with their markers taken out and the
//! faces they asked for as runs: `**bold**` / `__bold__`, `*italic*` / `_italic_`, `***both***`, `` `code` `` (and
//! ``` ``code with ` in it`` ```), `[words](url)` as its words, `\*` as a plain star. Blocks (fences, lists, quotes) are
//! the host's rows, not this. Ranges are UTF-16 units of the plain text, as layouts count.
use crate::text::{Face, Run};

/// Inline markdown set: the words shown, the faces over them, and the code spans (each drawn on a chip).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Inline {
    pub text: String,
    pub runs: Vec<Run>,
    pub code: Vec<(usize, usize)>,
}

/// `src` read as inline markdown.
#[must_use]
pub fn inline(src: &str) -> Inline {
    let chars: Vec<char> = src.chars().collect();
    let mut out = Inline::default();
    // The UTF-16 length of what is out so far, and where each open emphasis started (in it).
    let mut at = 0usize;
    let (mut bold, mut italic): (Option<usize>, Option<usize>) = (None, None);
    let mut spans: Vec<(usize, usize, u8)> = Vec::new();
    let push = |out: &mut Inline, at: &mut usize, c: char| {
        out.text.push(c);
        *at += c.len_utf16();
    };
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        match c {
            '\\' if chars.get(i + 1).is_some_and(|n| n.is_ascii_punctuation()) => {
                push(&mut out, &mut at, chars[i + 1]);
                i += 2;
            }
            '`' => {
                let ticks = chars[i..].iter().take_while(|&&t| t == '`').count();
                // The closing run of as many ticks.
                let close = (i + ticks..chars.len()).find(|&j| {
                    chars[j..].iter().take_while(|&&t| t == '`').count() == ticks
                        && (j == 0 || chars[j - 1] != '`')
                });
                match close {
                    Some(j) => {
                        let inner: String = chars[i + ticks..j].iter().collect();
                        let inner = if inner.starts_with(' ') && inner.ends_with(' ') && inner.trim() != "" {
                            inner[1..inner.len() - 1].to_owned()
                        } else {
                            inner
                        };
                        let start = at;
                        for ch in inner.chars() {
                            push(&mut out, &mut at, ch);
                        }
                        out.runs.push(Run { start, end: at, face: Face::Code });
                        out.code.push((start, at));
                        i = j + ticks;
                    }
                    None => {
                        for _ in 0..ticks {
                            push(&mut out, &mut at, '`');
                        }
                        i += ticks;
                    }
                }
            }
            '*' | '_' => {
                let n = chars[i..].iter().take_while(|&&t| t == c).count();
                let before = i.checked_sub(1).map(|k| chars[k]);
                let after = chars.get(i + n).copied();
                let space = |ch: Option<char>| ch.is_none_or(char::is_whitespace);
                // `_` inside a word (snake_case) is a letter; `*` with spaces both sides is a star.
                let word = |ch: Option<char>| ch.is_some_and(char::is_alphanumeric);
                let opens = !space(after) && (c == '*' || !word(before));
                let closes = !space(before) && (c == '*' || !word(after));
                let mut used = 0;
                let take = |want: usize, slot: &mut Option<usize>, kind: u8, spans: &mut Vec<(usize, usize, u8)>, used: &mut usize| {
                    if n - *used < want {
                        return;
                    }
                    if let Some(s) = slot.filter(|_| closes) {
                        spans.push((s, at, kind));
                        *slot = None;
                        *used += want;
                    } else if opens && slot.is_none() && has_closer(&chars, i + n, c, want) {
                        *slot = Some(at);
                        *used += want;
                    }
                };
                if n >= 2 {
                    take(2, &mut bold, 2, &mut spans, &mut used);
                }
                take(1, &mut italic, 1, &mut spans, &mut used);
                if n >= 2 && used < 2 {
                    take(2, &mut bold, 2, &mut spans, &mut used);
                }
                for _ in used..n {
                    push(&mut out, &mut at, c);
                }
                i += n;
            }
            '[' => {
                // [words](url): the words, the url dropped.
                let close = chars[i + 1..].iter().position(|&t| t == ']').map(|k| i + 1 + k);
                match close.filter(|&j| chars.get(j + 1) == Some(&'(')) {
                    Some(j) => match chars[j + 2..].iter().position(|&t| t == ')') {
                        Some(k) => {
                            let words: String = chars[i + 1..j].iter().collect();
                            let inner = inline(&words);
                            let base = at;
                            for ch in inner.text.chars() {
                                push(&mut out, &mut at, ch);
                            }
                            for r in inner.runs {
                                out.runs.push(Run { start: r.start + base, end: r.end + base, face: r.face });
                            }
                            for (a, b) in inner.code {
                                out.code.push((a + base, b + base));
                            }
                            i = j + 2 + k + 1;
                        }
                        None => {
                            push(&mut out, &mut at, c);
                            i += 1;
                        }
                    },
                    None => {
                        push(&mut out, &mut at, c);
                        i += 1;
                    }
                }
            }
            _ => {
                push(&mut out, &mut at, c);
                i += 1;
            }
        }
    }
    // Emphasis as runs: each UTF-16 stretch between span edges gets the face of the spans over it; code keeps its face.
    let mut edges: Vec<usize> = spans.iter().flat_map(|&(a, b, _)| [a, b]).collect();
    edges.sort_unstable();
    edges.dedup();
    for w in edges.windows(2) {
        let (a, b) = (w[0], w[1]);
        let k = spans.iter().filter(|s| s.0 <= a && b <= s.1).fold(0u8, |k, s| k | s.2);
        let face = match k {
            3 => Face::BoldItalic,
            2 => Face::Bold,
            1 => Face::Italic,
            _ => continue,
        };
        // Cut around the code spans inside it.
        let mut from = a;
        for &(cs, ce) in out.code.iter().filter(|(cs, ce)| *ce > a && *cs < b) {
            if cs > from {
                out.runs.push(Run { start: from, end: cs, face });
            }
            from = ce.max(from);
        }
        if from < b {
            out.runs.push(Run { start: from, end: b, face });
        }
    }
    out.runs.sort_by_key(|r| r.start);
    out
}

/// Whether a run of `want` `c`s closes later in `chars` (from `from`), so an opener is one.
fn has_closer(chars: &[char], from: usize, c: char, want: usize) -> bool {
    let mut j = from;
    while j < chars.len() {
        if chars[j] == '`' {
            // Skip a code span.
            let n = chars[j..].iter().take_while(|&&t| t == '`').count();
            match (j + n..chars.len()).find(|&k| chars[k..].iter().take_while(|&&t| t == '`').count() == n) {
                Some(k) => j = k + n,
                None => j += n,
            }
            continue;
        }
        if chars[j] == c {
            let n = chars[j..].iter().take_while(|&&t| t == c).count();
            if n >= want && j > 0 && !chars[j - 1].is_whitespace() {
                return true;
            }
            j += n;
            continue;
        }
        j += 1;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markers_go_and_faces_come() {
        let m = inline("a **bold** and *it* with `co*de*` [link](http://x) 2 * 3 snake_case_name");
        assert_eq!(m.text, "a bold and it with co*de* link 2 * 3 snake_case_name");
        let face = |s: &str| {
            let at = m.text.find(s).unwrap();
            m.runs.iter().find(|r| r.start == at).map(|r| r.face)
        };
        assert_eq!(face("bold"), Some(Face::Bold));
        assert_eq!(face("it "), Some(Face::Italic));
        assert_eq!(face("co*de*"), Some(Face::Code));
        assert_eq!(m.code.len(), 1);
        let both = inline("***x*** **a `b` c**");
        assert_eq!(both.text, "x a b c");
        assert_eq!(both.runs[0].face, Face::BoldItalic);
        assert!(both.runs.iter().any(|r| r.face == Face::Code));
    }
}
