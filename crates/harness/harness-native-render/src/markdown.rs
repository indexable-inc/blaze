//! Inline markdown (`MarkdownText`): a paragraph's, heading's or list item's words with their markers taken out and the
//! faces they asked for as runs: `**bold**` / `__bold__`, `*italic*` / `_italic_`, `***both***`, `` `code` `` (and
//! ``` ``code with ` in it`` ```), `[words](url)` as its words (in the link ink), `\*` as a plain star. Blocks (fences,
//! lists, quotes) are the host's rows, not this; [`table`] reads a GitHub table's lines into its cells for the host's
//! table row. Inline components (`<Path loc="...">blaze</Path>`, harness-native-ui-inline's grammar) become their label
//! after a hole the painter draws the component's icon in, on a chip. Ranges are UTF-16 units of the plain text, as
//! layouts count.
use crate::text::{Face, Hole, Run};

/// Inline markdown set: the words shown, the faces over them, the code spans (each drawn on a chip) and the links (their
/// words' range and where they go).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Inline {
    pub text: String,
    pub runs: Vec<Run>,
    pub code: Vec<(usize, usize)>,
    pub links: Vec<(usize, usize, String)>,
    /// The inline components: each one's range (its icon's hole first, then its label) and its tag.
    pub tags: Vec<InlineTag>,
}

/// One inline component in the words: `start` is its icon's hole (one unit, [`crate::text::HOLE`]), its label runs to
/// `end`. The painter draws its chip and icon from the scene's `ui.inline.<name>`; a press opens its target there. A tag
/// whose form has a mark (`tag.mark`) ends in a second hole, `end - 1`, the mark its live state picks is drawn in.
#[derive(Clone, Debug, PartialEq)]
pub struct InlineTag {
    pub start: usize,
    pub end: usize,
    pub tag: harness_native_ui_inline::Tag,
}

impl InlineTag {
    /// The mark's hole, when it has one.
    #[must_use]
    pub fn mark(&self) -> Option<usize> {
        self.tag.mark.then(|| self.end - 1)
    }
}

impl Inline {
    /// The holes the components' icons stand in, for words set at `size`: [`REF_ICON_EM`] wide, as tall as the words; and
    /// after a label with a mark, [`MARK_EM`] wide.
    #[must_use]
    pub fn holes(&self, size: f32) -> Vec<Hole> {
        let hole = |at: usize, em: f32| Hole { start: at, end: at + 1, width: size * em, ascent: 0.0, descent: 0.0, pad: 0.0 };
        self.tags
            .iter()
            .flat_map(|t| std::iter::once(hole(t.start, REF_ICON_EM)).chain(t.mark().map(|m| hole(m, MARK_EM))))
            .collect()
    }
}

/// An inline component's icon hole, in ems of its words: the icon ([`REF_ICON_SIZE_EM`]) and the room before its label.
pub const REF_ICON_EM: f32 = 1.3;
/// Its icon's size, in ems of the words.
pub const REF_ICON_SIZE_EM: f32 = 0.9;
/// The mark's hole after a label (a pull request's CI check): the room before it and the mark ([`MARK_SIZE_EM`]).
pub const MARK_EM: f32 = 1.05;
/// The mark's size, in ems of the words.
pub const MARK_SIZE_EM: f32 = 0.72;

/// An inline code span's chip, in ems of its text's size: the plate reaches `CHIP_INSET_EM` past the span's glyphs each
/// side, and keeps `CHIP_GAP_EM` clear of a neighbour that is not white space (a word space is gap enough on its own). The
/// typesetter leaves that room ([`chip_room`]), so the plate never covers the words beside it.
pub const CHIP_INSET_EM: f32 = 0.3;
pub const CHIP_GAP_EM: f32 = 0.15;
/// The chip's corners, points.
pub const CHIP_RADIUS: f32 = 3.0;

/// The room (points) the typesetter leaves before and after the code span `lo..hi` of `units` (UTF-16) set at `size`: kerned
/// in after the unit before it and after its last unit. None before a span that starts the text (no unit to kern; its
/// plate hangs into the margin there). A caret between two units sits in the middle of the kern between them, so the
/// span's carets are half of each room outside its glyphs.
#[must_use]
pub fn chip_room(units: &[u16], lo: usize, hi: usize, size: f32) -> (f32, f32) {
    let spaced = |u: Option<&u16>| u.is_none_or(|&u| char::from_u32(u32::from(u)).is_some_and(char::is_whitespace));
    let side = |spaced: bool| size * (CHIP_INSET_EM + if spaced { 0.0 } else { CHIP_GAP_EM });
    let before = if lo == 0 { 0.0 } else { side(spaced(units.get(lo - 1))) };
    (before, side(spaced(units.get(hi))))
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
                        out.runs.push(Run { start, end: at, face: Face::InlineCode });
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
            '<' if chars.get(i + 1).is_some_and(char::is_ascii_uppercase) => match harness_native_ui_inline::tag(&chars, i) {
                // An inline component: a hole for its icon, then its label (plain words), all in the chip's face.
                Some((tag, next)) => {
                    let start = at;
                    push(&mut out, &mut at, crate::text::HOLE);
                    for ch in tag.label.chars() {
                        push(&mut out, &mut at, ch);
                    }
                    if tag.mark {
                        push(&mut out, &mut at, crate::text::HOLE);
                    }
                    out.runs.push(Run { start, end: at, face: Face::InlineRef });
                    out.tags.push(InlineTag { start, end: at, tag });
                    i = next;
                }
                None => {
                    push(&mut out, &mut at, c);
                    i += 1;
                }
            },
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
                            out.links.push((base, at, chars[j + 2..j + 2 + k].iter().collect()));
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
        let mut chips: Vec<(usize, usize)> = out.code.iter().copied().chain(out.tags.iter().map(|t| (t.start, t.end))).collect();
        chips.sort_unstable();
        for &(cs, ce) in chips.iter().filter(|(cs, ce)| *ce > a && *cs < b) {
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

/// A table column's alignment, from its delimiter cell: `:--` left, `:-:` centre, `--:` right, `---` unsaid.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Align {
    #[default]
    None,
    Left,
    Center,
    Right,
}

/// A GitHub table: its header's cells, each column's alignment, and its rows, each as many cells as the header (a short
/// row padded with empty cells, a long one cut, as GitHub does). Cells are inline markdown, trimmed, as written: an
/// escaped `\|` stays escaped outside code ([`inline`] shows it as `|`) and is a plain `|` inside a code span.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Table {
    pub head: Vec<String>,
    pub align: Vec<Align>,
    pub rows: Vec<Vec<String>>,
}

/// The cells of one table line: split at its pipes, never at an escaped `\|` or one inside a code span; the leading and
/// trailing pipes are optional.
#[must_use]
pub fn cells(line: &str) -> Vec<String> {
    let t = line.trim();
    let chars: Vec<char> = t.chars().collect();
    let mut out = vec![String::new()];
    let mut ended = false;
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        ended = false;
        let cur = out.last_mut().expect("one cell at least");
        match c {
            '\\' => {
                cur.push(c);
                if let Some(&n) = chars.get(i + 1) {
                    cur.push(n);
                }
                i += 2;
            }
            '`' => {
                let n = chars[i..].iter().take_while(|&&t| t == '`').count();
                let close = (i + n..chars.len())
                    .find(|&j| chars[j - 1] != '`' && chars[j..].iter().take_while(|&&t| t == '`').count() == n);
                let ticks = "`".repeat(n);
                match close {
                    Some(j) => {
                        let inner: String = chars[i + n..j].iter().collect();
                        cur.push_str(&ticks);
                        cur.push_str(&inner.replace("\\|", "|"));
                        cur.push_str(&ticks);
                        i = j + n;
                    }
                    None => {
                        cur.push_str(&ticks);
                        i += n;
                    }
                }
            }
            '|' => {
                out.push(String::new());
                ended = true;
                i += 1;
            }
            _ => {
                cur.push(c);
                i += 1;
            }
        }
    }
    if t.starts_with('|') {
        out.remove(0);
    }
    if ended && !out.is_empty() {
        out.pop();
    }
    out.into_iter().map(|c| c.trim().to_owned()).collect()
}

/// A table's delimiter line (`| :--- | ---: |`) as its columns' alignments; None for any other line.
#[must_use]
pub fn delimiter(line: &str) -> Option<Vec<Align>> {
    if !line.contains('-') || !line.contains('|') {
        return None;
    }
    let cs = cells(line);
    if cs.is_empty() {
        return None;
    }
    cs.iter()
        .map(|c| {
            let (l, r) = (c.starts_with(':'), c.len() > 1 && c.ends_with(':'));
            let mid = c.trim_start_matches(':').trim_end_matches(':');
            (!mid.is_empty() && mid.chars().all(|ch| ch == '-')).then_some(match (l, r) {
                (true, true) => Align::Center,
                (true, false) => Align::Left,
                (false, true) => Align::Right,
                (false, false) => Align::None,
            })
        })
        .collect()
}

/// The GitHub table `lines` start with, and how many lines it takes: a header line with a pipe, a delimiter line of as
/// many cells, then rows while lines have a pipe in them (a blank line or one without ends it). None when they do not
/// start one.
#[must_use]
pub fn table(lines: &[&str]) -> Option<(Table, usize)> {
    let (head, delim) = (lines.first()?, lines.get(1)?);
    if !head.contains('|') {
        return None;
    }
    let head = cells(head);
    let align = delimiter(delim)?;
    if align.len() != head.len() {
        return None;
    }
    let n = head.len();
    let mut rows = Vec::new();
    let mut used = 2;
    for l in &lines[2..] {
        if l.trim().is_empty() || !l.contains('|') {
            break;
        }
        let mut r = cells(l);
        r.resize(n, String::new());
        rows.push(r);
        used += 1;
    }
    Some((Table { head, align, rows }, used))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn links_are_ranged() {
        let m = inline("see [the **docs**](https://x.dev/a) now");
        assert_eq!(m.text, "see the docs now");
        assert_eq!(m.links, vec![(4, 12, "https://x.dev/a".to_owned())]);
    }

    /// A GitHub table's cells: pipes in code spans and escaped pipes do not split, ragged rows are evened to the header,
    /// alignment comes from the delimiter line.
    #[test]
    fn tables_split_into_cells() {
        assert_eq!(cells(r"| a | `x|y` | b \| c |"), ["a", "`x|y`", r"b \| c"]);
        assert_eq!(cells("a | b"), ["a", "b"]);
        assert_eq!(cells(r"| `a\|b` | ``c ` | d`` |"), ["`a|b`", "``c ` | d``"]);
        assert_eq!(cells("| unclosed `tick | x |"), ["unclosed `tick", "x"]);
        assert_eq!(cells("| a | |"), ["a", ""]);
        assert_eq!(delimiter("| :--- | :-: | --: | --- |"), Some(vec![Align::Left, Align::Center, Align::Right, Align::None]));
        assert_eq!(delimiter("|---|---|"), Some(vec![Align::None, Align::None]));
        assert_eq!(delimiter("| a | --- |"), None);
        assert_eq!(delimiter("- item"), None);
        let lines = ["| name | n |", "|:--|--:|", "| **a** | 1 |", "| b |", "| c | 3 | extra |", "", "after"];
        let (t, used) = table(&lines).unwrap();
        assert_eq!(used, 5);
        assert_eq!(t.head, ["name", "n"]);
        assert_eq!(t.align, [Align::Left, Align::Right]);
        assert_eq!(t.rows, [vec!["**a**", "1"], vec!["b", ""], vec!["c", "3"]]);
        // A header whose delimiter has another count is no table; nor is a line with no delimiter under it.
        assert!(table(&["| a | b |", "|---|"]).is_none());
        assert!(table(&["| a | b |", "| c | d |"]).is_none());
        // An escaped pipe shows as a pipe.
        assert_eq!(inline(&cells(r"| a \| b |")[0]).text, "a | b");
    }

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
        assert_eq!(face("co*de*"), Some(Face::InlineCode));
        assert_eq!(m.code.len(), 1);
        let both = inline("***x*** **a `b` c**");
        assert_eq!(both.text, "x a b c");
        assert_eq!(both.runs[0].face, Face::BoldItalic);
        assert!(both.runs.iter().any(|r| r.face == Face::InlineCode));
    }
}
