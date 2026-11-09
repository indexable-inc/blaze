//! Inline markdown (`MarkdownText`): a paragraph's, heading's or list item's words with their markers taken out and the
//! faces they asked for as runs: `**bold**` / `__bold__`, `*italic*` / `_italic_`, `***both***`, `` `code` `` (and
//! ``` ``code with ` in it`` ```), `[words](url)` as its words (in the link ink), `~~struck~~` and `~struck~`
//! (GitHub's strikethrough: a range the painter draws a hairline through), autolinks (`<https://...>`, `<someone@host>`)
//! and bare `https://`, `http://` and `www.` addresses as links, a newline after two spaces or a backslash as a hard line
//! break (any other newline a space), `\*` as a plain star. Blocks (fences, lists, quotes) are the host's rows, not this;
//! [`table`] reads a GitHub table's lines into its cells for the host's table row. Inline components
//! (`<Path loc="...">blaze</Path>`, harness-native-ui-inline's grammar) become their label after a hole the painter draws
//! the component's icon in, on a chip; so does a markdown link or a bare address that is
//! one (`[the fix](https://github.com/o/r/pull/7)` is that pull request's chip, harness-native-ui-inline's `from_url`).
//! LaTeX math (`$…$`, `\(…\)`, and `$$…$$`, `\[…\]` in display style) is typeset (math.rs) and leaves a hole as wide and
//! tall as the formula, which the painter draws it in; one that does not typeset is its source as written, as inline code.
//! Ranges are UTF-16 units of the plain text, as layouts count.
use std::sync::Arc;

use harness_native_ui_look_api::MathLook;

use crate::math::Typeset;
use crate::text::{Face, Hole, Run};

/// Inline markdown set: the words shown, the faces over them, the code spans (each drawn on a chip), the links (their
/// words' range and where they go) and the struck-through stretches.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Inline {
    pub text: String,
    pub runs: Vec<Run>,
    pub code: Vec<(usize, usize)>,
    pub links: Vec<(usize, usize, String)>,
    /// The struck-through stretches (`~~words~~`), sorted; one may hold code, links and emphasis.
    pub strike: Vec<(usize, usize)>,
    /// The inline components: each one's range (its icon's hole first, then its label) and its tag.
    pub tags: Vec<InlineTag>,
    /// The formulas, in order: each stands in one hole ([`crate::text::HOLE`]) the painter draws it in.
    pub math: Vec<InlineMath>,
}

/// A formula in the words: the hole it stands in (one unit at `at`), what was written (delimiters and all: what copying it
/// whole gives), whether it is in display style (`$$…$$`, `\[…\]`) and how it typeset.
#[derive(Clone, Debug, PartialEq)]
pub struct InlineMath {
    pub at: usize,
    pub source: String,
    pub display: bool,
    pub set: Arc<Typeset>,
}

impl InlineMath {
    /// Points to the formula's em for words set at `size`: the look's scale for inline or displayed math times their size.
    #[must_use]
    pub fn em(&self, size: f32, look: &MathLook) -> f32 {
        size * if self.display { look.display_scale } else { look.inline_scale }
    }

    /// The room it takes in its line for words set at `size`: as wide as it is, as far above and below the baseline as it
    /// reaches, with the look's pad clear of the line's edges.
    #[must_use]
    pub fn hole(&self, size: f32, look: &MathLook) -> Hole {
        let em = self.em(size, look);
        let s = &self.set;
        let pad = if self.display { look.display_pad } else { look.inline_pad };
        Hole { start: self.at, end: self.at + 1, width: s.width * em, ascent: s.height * em, descent: s.depth * em, pad }
    }
}

/// One inline component in the words: `start` is its icon's hole (one unit, [`crate::text::HOLE`]), its label runs to
/// `end`. The painter draws its chip and icon from the scene's `ui.inline.<name>`; a press opens its target there. Its
/// `meta` (a link's host) follows the label after a no-break space, `meta` its range, drawn quiet. A tag whose form has a
/// mark (`tag.mark`) ends in a second hole, `end - 1`, the mark its live state picks is drawn in.
#[derive(Clone, Debug, PartialEq)]
pub struct InlineTag {
    pub start: usize,
    pub end: usize,
    pub meta: Option<(usize, usize)>,
    pub tag: harness_native_ui_inline::Tag,
}

impl InlineTag {
    /// The mark's hole, when it has one.
    #[must_use]
    pub fn mark(&self) -> Option<usize> {
        self.tag.mark.then(|| self.end - 1)
    }

    /// The label's range: after the icon's hole, before the meta and the mark.
    #[must_use]
    pub fn label(&self) -> (usize, usize) {
        let end = self.meta.map_or_else(|| self.mark().unwrap_or(self.end), |(a, _)| a - 1);
        (self.start + 1, end)
    }
}

/// Inline component `tag` set into `out` at `at`: its icon's hole, its label, its meta, its mark's hole, in the chip's
/// face.
fn emit_tag(out: &mut Inline, at: &mut usize, tag: harness_native_ui_inline::Tag) {
    fn push(out: &mut Inline, at: &mut usize, c: char) {
        out.text.push(c);
        *at += c.len_utf16();
    }
    let start = *at;
    push(out, at, crate::text::HOLE);
    for ch in tag.label.chars() {
        push(out, at, ch);
    }
    let meta = if tag.meta.is_empty() {
        None
    } else {
        push(out, at, '\u{a0}');
        let from = *at;
        for ch in tag.meta.chars() {
            push(out, at, ch);
        }
        Some((from, *at))
    };
    if tag.mark {
        push(out, at, crate::text::HOLE);
    }
    // A cited phrase keeps the face of the words around it; a chip has its own.
    if !tag.plain {
        out.runs.push(Run { start, end: *at, face: Face::InlineRef });
    }
    out.tags.push(InlineTag { start, end: *at, meta, tag });
}

impl Inline {
    /// The holes in the words, in order, for words set at `size`: the components' icons [`REF_ICON_EM`] wide (none wide
    /// for a cited phrase, which has no icon), as tall as
    /// the words, and after a label with a mark, [`MARK_EM`] wide; each formula as wide and tall as `look` sets it
    /// ([`InlineMath::hole`]).
    #[must_use]
    pub fn holes(&self, size: f32, look: &MathLook) -> Vec<Hole> {
        let hole = |at: usize, em: f32| Hole { start: at, end: at + 1, width: size * em, ascent: 0.0, descent: 0.0, pad: 0.0 };
        let mut out: Vec<Hole> = self
            .tags
            .iter()
            .flat_map(|t| std::iter::once(hole(t.start, if t.tag.plain { 0.0 } else { REF_ICON_EM })).chain(t.mark().map(|m| hole(m, MARK_EM))))
            .chain(self.math.iter().map(|f| f.hole(size, look)))
            .collect();
        out.sort_by_key(|h| h.start);
        out
    }
}

/// The longest a formula may be written, in bytes: a delimiter that is never closed does not swallow the message.
const MATH_MAX: usize = 4000;

/// A formula found at `chars[i]`: its LaTeX's range (between the delimiters), where it ends (past its closer) and whether it
/// is displayed. `$…$` and `\(…\)` are inline, `$$…$$` and `\[…\]` displayed; a backslash before a `$` is no delimiter (an
/// escape: a dollar). A formula has something in it, and stops short of a blank line, of the next code span and of
/// [`MATH_MAX`] bytes, so a delimiter never closed does not swallow the words.
fn math_at(chars: &[char], i: usize) -> Option<(std::ops::Range<usize>, usize, bool)> {
    let (open, close, display) = match (chars[i], chars.get(i + 1)) {
        ('\\', Some('(')) => (2, close_of(chars, i + 2, &['\\', ')']), false),
        ('\\', Some('[')) => (2, close_of(chars, i + 2, &['\\', ']']), true),
        ('$', Some('$')) => (2, close_of(chars, i + 2, &['$', '$']), true),
        ('$', _) => (1, dollar_close(chars, i), false),
        _ => return None,
    };
    let c = close?;
    let tex = i + open..c;
    (!chars[tex.clone()].iter().all(|c| c.is_whitespace())).then_some((tex, c + open, display))
}

/// Whether the newline at `chars[j]` starts a blank line (the end of a paragraph).
fn blank_after(chars: &[char], j: usize) -> bool {
    chars[j + 1..].iter().find(|&&c| c != ' ' && c != '\t') == Some(&'\n')
}

/// Where `close` is from `from`, unless a blank line, a code span or the cap comes first. A backslash takes the character
/// after it with it, so `\$` and `\\` are no delimiters (a `close` that begins with a backslash is looked for first).
fn close_of(chars: &[char], from: usize, close: &[char]) -> Option<usize> {
    let (mut j, mut bytes) = (from, 0usize);
    while j < chars.len() && bytes <= MATH_MAX {
        if chars[j..].starts_with(close) {
            return Some(j);
        }
        match chars[j] {
            '\\' => {
                bytes += 1;
                j += 1;
            }
            '\n' if blank_after(chars, j) => return None,
            '`' if code_span(chars, j).is_some() => return None,
            _ => {}
        }
        bytes += chars.get(j).map_or(0, |c| c.len_utf8());
        j += 1;
    }
    None
}

/// The `$` that closes the inline math `chars[i]` opens (`$x$`), if it is math: the opener has no space after it, and the
/// closer none before it and no digit after it, so "costs $5 and $10" is a price list. Another `$` that cannot close it
/// ends the attempt (and is looked at as an opener itself: "$5 or $x$" is the formula `x`).
fn dollar_close(chars: &[char], i: usize) -> Option<usize> {
    let next = *chars.get(i + 1)?;
    if next.is_whitespace() || next == '$' {
        return None;
    }
    let (mut j, mut bytes) = (i + 1, 0usize);
    while j < chars.len() && bytes <= MATH_MAX {
        match chars[j] {
            '\\' => {
                bytes += 1;
                j += 1;
            }
            '\n' if blank_after(chars, j) => return None,
            '`' if code_span(chars, j).is_some() => return None,
            '$' => return (!chars[j - 1].is_whitespace() && !chars.get(j + 1).is_some_and(char::is_ascii_digit)).then_some(j),
            _ => {}
        }
        bytes += chars.get(j).map_or(0, |c| c.len_utf8());
        j += 1;
    }
    None
}

/// The formula `chars[i..next]` (its LaTeX `tex`) set into `out` at `at`: a hole it is drawn in when it typesets, else
/// its source as written, as inline code.
fn emit_math(out: &mut Inline, at: &mut usize, chars: &[char], i: usize, tex: std::ops::Range<usize>, next: usize, display: bool) {
    let source: String = chars[i..next].iter().collect();
    let latex: String = chars[tex].iter().collect();
    let start = *at;
    match crate::math::typeset(&latex, display) {
        Ok(set) => {
            out.text.push(crate::text::HOLE);
            *at += crate::text::HOLE.len_utf16();
            out.math.push(InlineMath { at: start, source, display, set });
        }
        Err(_) => {
            for ch in source.chars() {
                out.text.push(ch);
                *at += ch.len_utf16();
            }
            out.runs.push(Run { start, end: *at, face: Face::InlineCode });
            out.code.push((start, *at));
        }
    }
}

/// An inline component's icon hole, in ems of its words: the icon ([`REF_ICON_SIZE_EM`]) and the room before its label.
pub const REF_ICON_EM: f32 = 1.3;
/// Its icon's size, in ems of the words.
pub const REF_ICON_SIZE_EM: f32 = 0.9;
/// A file chip's drawing's box, in ems of the words, centred where the icon goes: IntelliJ's icons keep a unit of their
/// 16 clear round their ink, so their box is a little larger than a glyph's for the ink to read at the glyph's size.
pub const REF_DRAWING_EM: f32 = 1.05;
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
    // The UTF-16 length of what is out so far, and where each open emphasis started (in it). An open strike also keeps
    // how many tildes opened it: only as many close it.
    let mut at = 0usize;
    let (mut bold, mut italic): (Option<usize>, Option<usize>) = (None, None);
    let mut strike: Option<(usize, usize)> = None;
    let mut spans: Vec<(usize, usize, u8)> = Vec::new();
    let push = |out: &mut Inline, at: &mut usize, c: char| {
        out.text.push(c);
        *at += c.len_utf16();
    };
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        // A formula, before an escape takes its `\(` for a bracket or emphasis its `_` and `*`.
        if let Some((tex, next, display)) = math_at(&chars, i) {
            emit_math(&mut out, &mut at, &chars, i, tex, next, display);
            i = next;
            continue;
        }
        // A `$$` that opens no formula is two dollars, neither of them an opener.
        if c == '$' && chars.get(i + 1) == Some(&'$') {
            push(&mut out, &mut at, '$');
            push(&mut out, &mut at, '$');
            i += 2;
            continue;
        }
        match c {
            '\\' if chars.get(i + 1).is_some_and(|n| n.is_ascii_punctuation()) => {
                push(&mut out, &mut at, chars[i + 1]);
                i += 2;
            }
            // A backslash before a newline: a hard line break.
            '\\' if chars.get(i + 1) == Some(&'\n') => {
                push(&mut out, &mut at, '\n');
                i = past_indent(&chars, i + 2);
            }
            '\n' => {
                // Two spaces or more before it: a hard line break; else a soft one, a space. The spaces go either way,
                // and so does the next line's indent; never a space a code span or a chip ends in.
                let floor = settled(&out);
                let spaces = out.text.chars().rev().take_while(|&s| s == ' ').count().min(at - floor);
                for _ in 0..spaces {
                    out.text.pop();
                }
                at -= spaces;
                push(&mut out, &mut at, if spaces >= 2 { '\n' } else { ' ' });
                i = past_indent(&chars, i + 1);
            }
            '`' => {
                let ticks = run(&chars, i, '`');
                match code_span(&chars, i) {
                    Some((inner, next)) => {
                        let start = at;
                        for ch in inner.chars() {
                            push(&mut out, &mut at, ch);
                        }
                        out.runs.push(Run { start, end: at, face: Face::InlineCode });
                        out.code.push((start, at));
                        i = next;
                    }
                    None => {
                        for _ in 0..ticks {
                            push(&mut out, &mut at, '`');
                        }
                        i += ticks;
                    }
                }
            }
            '~' => {
                // GitHub's strikethrough: one or two tildes open where they are left-flanking and the same number
                // closes where right-flanking; three or more, and any without a partner, are tildes.
                let n = run(&chars, i, '~');
                let (left, right) = flanking(&chars, i, n);
                match strike {
                    Some((s, k)) if k == n && right => {
                        out.strike.push((s, at));
                        strike = None;
                    }
                    None if n <= 2 && left && has_closer(&chars, i + n, '~', n) => strike = Some((at, n)),
                    _ => {
                        for _ in 0..n {
                            push(&mut out, &mut at, '~');
                        }
                    }
                }
                i += n;
            }
            '*' | '_' => {
                let n = run(&chars, i, c);
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
            '<' => {
                if let Some((shown, href, next)) = autolink(&chars, i) {
                    // <https://...> and <someone@host>: the address as written, a link to it.
                    let start = at;
                    for &ch in &chars[shown] {
                        push(&mut out, &mut at, ch);
                    }
                    out.links.push((start, at, href));
                    i = next;
                } else if let Some((tag, next)) = harness_native_ui_inline::tag(&chars, i) {
                    // An inline component: a hole for its icon, then its label (plain words), all in the chip's face.
                    emit_tag(&mut out, &mut at, tag);
                    i = next;
                } else {
                    push(&mut out, &mut at, c);
                    i += 1;
                }
            }
            '[' => match link(&chars, i) {
                // [words](url): the words, the url dropped. A link holds no link (GitHub's rule): an address in its
                // words is words.
                Some((words, url, next)) => {
                    let inner = inline(&chars[words].iter().collect::<String>());
                    // A link to what a component is (a pull request) is that component's chip, its words kept.
                    let href: String = chars[url.clone()].iter().collect();
                    if let Some(tag) = harness_native_ui_inline::from_url(href.trim(), Some(&inner.text)) {
                        emit_tag(&mut out, &mut at, tag);
                        i = next;
                        continue;
                    }
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
                    for (a, b) in inner.strike {
                        out.strike.push((a + base, b + base));
                    }
                    for f in inner.math {
                        out.math.push(InlineMath { at: f.at + base, ..f });
                    }
                    out.links.push((base, at, chars[url].iter().collect()));
                    i = next;
                }
                None => {
                    push(&mut out, &mut at, c);
                    i += 1;
                }
            },
            _ => match bare_url(&chars, i) {
                // A bare https://, http:// or www. address: its words as written, a link to it.
                Some((next, href)) => {
                    // An address of what a component is (a pull request): its chip.
                    if let Some(tag) = harness_native_ui_inline::from_url(&href, None) {
                        emit_tag(&mut out, &mut at, tag);
                        i = next;
                        continue;
                    }
                    let start = at;
                    for &ch in &chars[i..next] {
                        push(&mut out, &mut at, ch);
                    }
                    out.links.push((start, at, href));
                    i = next;
                }
                None => {
                    push(&mut out, &mut at, c);
                    i += 1;
                }
            },
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
    out.strike.sort_unstable();
    out
}

/// How many `c`s run from `i`.
fn run(chars: &[char], i: usize, c: char) -> usize {
    chars[i..].iter().take_while(|&&t| t == c).count()
}

/// Past the spaces and tabs from `i` (a continuation line's indent).
fn past_indent(chars: &[char], i: usize) -> usize {
    i + chars[i..].iter().take_while(|&&t| t == ' ' || t == '\t').count()
}

/// Where the last code span, chip or link of `out` ends: a line break's trailing spaces are never taken from before it.
fn settled(out: &Inline) -> usize {
    let runs = out.runs.iter().map(|r| r.end);
    runs.chain(out.links.iter().map(|l| l.1)).max().unwrap_or(0)
}

/// CommonMark's punctuation, for flanking: ASCII punctuation and any other symbol that is no letter, digit or space.
fn punct(c: char) -> bool {
    c.is_ascii_punctuation() || (!c.is_ascii() && !c.is_alphanumeric() && !c.is_whitespace())
}

/// Whether the run of `n` delimiters at `i` is left-flanking (it may open) and right-flanking (it may close), as
/// CommonMark has it: not before white space, and if before punctuation, only after white space or punctuation; and the
/// mirror. The text's edges count as white space.
fn flanking(chars: &[char], i: usize, n: usize) -> (bool, bool) {
    let before = i.checked_sub(1).map(|k| chars[k]);
    let after = chars.get(i + n).copied();
    let space = |ch: Option<char>| ch.is_none_or(char::is_whitespace);
    let loose = |ch: Option<char>| space(ch) || ch.is_some_and(punct);
    let left = !space(after) && (!after.is_some_and(punct) || loose(before));
    let right = !space(before) && (!before.is_some_and(punct) || loose(after));
    (left, right)
}

/// Whether a run of `want` `c`s closes later in `chars` (from `from`), so an opener is one: never one inside a code
/// span, link, component or address ([`atom`]), which the reading takes whole. A tilde's closer is a right-flanking run
/// of exactly as many; emphasis's any run of as many or more after something other than white space.
fn has_closer(chars: &[char], from: usize, c: char, want: usize) -> bool {
    let mut j = from;
    while j < chars.len() {
        if let Some(next) = atom(chars, j) {
            j = next;
            continue;
        }
        if chars[j] == c {
            let n = run(chars, j, c);
            let closes = if c == '~' { n == want && flanking(chars, j, n).1 } else { n >= want && j > 0 && !chars[j - 1].is_whitespace() };
            if closes {
                return true;
            }
            j += n;
            continue;
        }
        j += 1;
    }
    false
}

/// Where a construct [`inline`] reads whole ends, when one starts at `i`: a formula, an escape, a code span (or an unclosed run of
/// ticks, which stays ticks), a link, an inline component, an autolink or a bare address.
fn atom(chars: &[char], i: usize) -> Option<usize> {
    if let Some((_, next, _)) = math_at(chars, i) {
        return Some(next);
    }
    match chars[i] {
        '\\' => chars.get(i + 1).is_some_and(|n| n.is_ascii_punctuation()).then_some(i + 2),
        '`' => Some(code_span(chars, i).map_or(i + run(chars, i, '`'), |(_, next)| next)),
        '[' => link(chars, i).map(|(_, _, next)| next),
        '<' => autolink(chars, i).map(|(_, _, next)| next).or_else(|| harness_native_ui_inline::tag(chars, i).map(|(_, next)| next)),
        _ => bare_url(chars, i).map(|(next, _)| next),
    }
}

/// The code span the ticks at `i` open: its words (a line break in them a space; one space each side taken off when both
/// are there and it is not all spaces) and where it ends. None when no run of as many ticks closes it.
fn code_span(chars: &[char], i: usize) -> Option<(String, usize)> {
    let ticks = run(chars, i, '`');
    // The closing run of exactly as many ticks.
    let j = (i + ticks..chars.len()).find(|&j| run(chars, j, '`') == ticks && chars[j - 1] != '`')?;
    let inner: String = chars[i + ticks..j].iter().map(|&c| if c == '\n' { ' ' } else { c }).collect();
    let inner = if inner.starts_with(' ') && inner.ends_with(' ') && inner.trim() != "" { inner[1..inner.len() - 1].to_owned() } else { inner };
    Some((inner, j + ticks))
}

/// The link `[words](url)` at `i`: its words' and url's ranges in `chars`, and where it ends.
fn link(chars: &[char], i: usize) -> Option<(std::ops::Range<usize>, std::ops::Range<usize>, usize)> {
    let j = i + 1 + chars[i + 1..].iter().position(|&t| t == ']')?;
    if chars.get(j + 1) != Some(&'(') {
        return None;
    }
    let k = j + 2 + chars[j + 2..].iter().position(|&t| t == ')')?;
    Some((i + 1..j, j + 2..k, k + 1))
}

/// The autolink at `i` (CommonMark's): `<scheme:anything>` (a scheme of 2 to 32 letters, digits, `+`, `.` or `-` after a
/// letter; no space, `<` or `>` in it), a link to itself, or `<someone@host>`, a `mailto:` link. Its words (the address,
/// without the brackets), where it goes and where it ends.
fn autolink(chars: &[char], i: usize) -> Option<(std::ops::Range<usize>, String, usize)> {
    let end = i + 1 + chars[i + 1..].iter().position(|&t| t == '>' || t == '<' || t.is_whitespace() || t.is_control())?;
    if chars[end] != '>' || end == i + 1 {
        return None;
    }
    let inner = &chars[i + 1..end];
    let address: String = inner.iter().collect();
    let scheme = inner.iter().position(|&t| t == ':').is_some_and(|n| {
        (2..=32).contains(&n)
            && inner[0].is_ascii_alphabetic()
            && inner[..n].iter().all(|&t| t.is_ascii_alphanumeric() || matches!(t, '+' | '.' | '-'))
    });
    if scheme {
        return Some((i + 1..end, address, end + 1));
    }
    let at = inner.iter().position(|&t| t == '@')?;
    let (local, host) = (&inner[..at], &inner[at + 1..]);
    let local_ok = !local.is_empty() && local.iter().all(|&t| t.is_ascii_alphanumeric() || ".!#$%&'*+/=?^_`{|}~-".contains(t));
    let label_ok = |l: &[char]| {
        (1..=63).contains(&l.len()) && l.iter().all(|&t| t.is_ascii_alphanumeric() || t == '-') && l[0] != '-' && l[l.len() - 1] != '-'
    };
    let host_ok = !host.is_empty() && host.split(|&t| t == '.').all(label_ok);
    (local_ok && host_ok).then(|| (i + 1..end, format!("mailto:{address}"), end + 1))
}

/// GitHub's extended autolink at `i`: `https://`, `http://` or `www.` where a word may start (the text's start, after
/// white space or one of `*_~(`), a domain (letters, digits, `-`, `_` in labels split by dots; `www.` needs a dot after
/// it, and no `_` in its last two labels), then anything up to white space or `<`, less trailing punctuation (`?!.,:*_~'"`,
/// a `)` it does not balance, an entity's `&name;`). Where it ends and where it goes (`http://` before a `www.`).
fn bare_url(chars: &[char], i: usize) -> Option<(usize, String)> {
    if !(i == 0 || chars[i - 1].is_whitespace() || matches!(chars[i - 1], '*' | '_' | '~' | '(')) {
        return None;
    }
    let starts = |s: &str| s.chars().enumerate().all(|(k, c)| chars.get(i + k) == Some(&c));
    let (domain_at, www) = if starts("https://") {
        (i + 8, false)
    } else if starts("http://") {
        (i + 7, false)
    } else if starts("www.") {
        (i, true)
    } else {
        return None;
    };
    let domain_end = domain_at + chars[domain_at..].iter().take_while(|&&t| t.is_alphanumeric() || matches!(t, '-' | '_' | '.')).count();
    let domain = &chars[domain_at..domain_end];
    let labels: Vec<&[char]> = domain.split(|&t| t == '.').filter(|l| !l.is_empty()).collect();
    // `www.` and at least one label after it; a scheme's domain may be one label (localhost).
    if labels.len() < if www { 2 } else { 1 } || labels.iter().rev().take(2).any(|l| l.contains(&'_')) {
        return None;
    }
    let mut end = domain_end + chars[domain_end..].iter().take_while(|&&t| !t.is_whitespace() && t != '<').count();
    while end > domain_at {
        let last = chars[end - 1];
        if "?!.,:*_~'\"".contains(last) {
            end -= 1;
        } else if last == ')' {
            let open = chars[i..end].iter().filter(|&&t| t == '(').count();
            let close = chars[i..end].iter().filter(|&&t| t == ')').count();
            if close <= open {
                break;
            }
            end -= 1;
        } else if last == ';' {
            // `&amp;` at the end is an entity, not part of the address.
            let name = chars[domain_at..end - 1].iter().rev().take_while(|t| t.is_ascii_alphanumeric()).count();
            if name == 0 || end - 1 - name <= domain_at || chars[end - 2 - name] != '&' {
                break;
            }
            end -= name + 2;
        } else {
            break;
        }
    }
    // Trimmed into the domain (`www.` then only dots): no address.
    if end <= domain_at + usize::from(www) * 4 {
        return None;
    }
    let url: String = chars[i..end].iter().collect();
    Some((end, if www { format!("http://{url}") } else { url }))
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

    /// Each formula's source as written and whether it is displayed.
    fn maths(m: &Inline) -> Vec<(&str, bool)> {
        m.math.iter().map(|f| (f.source.as_str(), f.display)).collect()
    }

    /// A formula leaves one hole where it stands, whichever delimiters it has; what markdown would take for emphasis or
    /// an escape stays in it; what is in code is no math.
    #[test]
    fn math_leaves_a_hole_whichever_delimiters_it_has() {
        let m = inline("The root is $x^2$.");
        assert_eq!((m.text.as_str(), maths(&m)), ("The root is \u{a0}.", vec![("$x^2$", false)]));
        assert_eq!(m.math[0].at, 12);
        assert_eq!(maths(&inline("so \\(a+b\\) ok")), [("\\(a+b\\)", false)]);
        assert_eq!(maths(&inline("so $$\\frac{a}{b}$$ and \\[x\\]")), [("$$\\frac{a}{b}$$", true), ("\\[x\\]", true)]);
        let m = inline("$a_1*b_2$ and **bold**");
        assert_eq!(maths(&m), [("$a_1*b_2$", false)]);
        let units: Vec<u16> = m.text.encode_utf16().collect();
        let faces: Vec<(String, Face)> = m.runs.iter().map(|r| (String::from_utf16_lossy(&units[r.start..r.end]), r.face)).collect();
        assert_eq!(faces, [("bold".to_owned(), Face::Bold)], "no emphasis was made of it");
        let m = inline("`$x$` then");
        assert!(m.math.is_empty() && m.text == "$x$ then", "{m:?}");
        // Across a soft break, inside a link's words.
        assert_eq!(maths(&inline("a $x +\ny$ b")), [("$x +\ny$", false)]);
        let m = inline("[see $x$](https://x.dev)");
        assert_eq!((maths(&m), m.math[0].at), (vec![("$x$", false)], 4));
    }

    /// Prices are not math: an opening `$` has no space after it, a closing one no space before it and no digit after it,
    /// and a backslash escapes either.
    #[test]
    fn currency_is_not_math() {
        for text in ["It costs $5 and $10 today.", "From $5 to $10.", "$ 5 and 6 $", "a $x$5 b", "It was \\$5 and \\$6", "just a $ sign", "$$ never closed", "\\( never closed"] {
            assert!(inline(text).math.is_empty(), "{text:?} is not math: {:?}", inline(text).math);
        }
        assert_eq!(inline("It was \\$5 and \\$6").text, "It was $5 and $6", "an escaped dollar is a dollar");
        assert_eq!(maths(&inline("costs $5 and $10, so $x+1$ more")), [("$x+1$", false)], "a price before real math does not swallow it");
        assert_eq!(maths(&inline("$5 or $x$")), [("$x$", false)]);
        let long = format!("${}$", "x".repeat(MATH_MAX + 1));
        assert!(inline(&long).math.is_empty(), "past the cap it is words");
    }

    /// Math that does not typeset is its source as written, as inline code, and no formula.
    #[test]
    fn math_that_does_not_typeset_is_its_source_as_inline_code() {
        let m = inline("see $\\frac{1}{$ ok");
        assert!(m.math.is_empty());
        assert_eq!(m.text, "see $\\frac{1}{$ ok");
        assert_eq!(m.code, [(4, 15)]);
    }

    /// A formula's hole is as wide and tall as the look sets it: its ems times the words' size and the look's scale, a
    /// displayed one at the display scale; the holes come in order with the components'.
    #[test]
    fn a_formulas_hole_is_its_typeset_size() {
        let look = MathLook { inline_scale: 1.1, display_scale: 1.25, inline_pad: 2.0, display_pad: 10.0, stroke: 0.0375 };
        let m = inline("a $x^2$ b $$\\sum_i i$$");
        let holes = m.holes(14.0, &look);
        assert_eq!(holes.len(), 2);
        for (h, f) in holes.iter().zip(&m.math) {
            let em = 14.0 * if f.display { 1.25 } else { 1.1 };
            assert_eq!((h.start, h.end), (f.at, f.at + 1));
            assert!(h.width > 0.0 && (h.width - f.set.width * em).abs() < 1e-4, "{h:?} for {f:?}");
            assert!((h.ascent - f.set.height * em).abs() < 1e-4 && (h.descent - f.set.depth * em).abs() < 1e-4);
            assert_eq!(h.pad, if f.display { 10.0 } else { 2.0 });
        }
        assert!(holes[1].ascent + holes[1].descent > holes[0].ascent + holes[0].descent, "the displayed sum stands taller");
    }

    #[test]
    fn links_are_ranged() {
        let m = inline("see [the **docs**](https://x.dev/a) now");
        assert_eq!(m.text, "see the docs now");
        assert_eq!(m.links, vec![(4, 12, "https://x.dev/a".to_owned())]);
    }

    /// A markdown link or a bare address that a component's `href` fits is that component's chip (its words kept, the
    /// form's label for a bare one); other links and addresses stay links; a link's host follows its words.
    #[test]
    fn links_to_pull_requests_are_chips() {
        use harness_native_ui_inline::{Form, install};
        let pr = Form { label: "#{n}".to_owned(), mark: true, href: "https://github.com/{repo}/pull/{n}".to_owned(), url: String::new(), plain: false };
        let link = Form { url: "href".to_owned(), ..Form::default() };
        install(std::collections::BTreeMap::from([("PR".to_owned(), pr), ("Link".to_owned(), link)]));
        let url = "https://github.com/indexable-inc/blaze/pull/1770";
        let m = inline(&format!("- [charts]({url}) and {url}. Also [docs](https://x.dev/a), https://x.dev/b"));
        let tags: Vec<(&str, &str)> = m.tags.iter().map(|t| (t.tag.name.as_str(), t.tag.label.as_str())).collect();
        assert_eq!(tags, [("PR", "charts"), ("PR", "#1770")]);
        assert!(m.text.ends_with(". Also docs, https://x.dev/b"), "{:?}", m.text);
        // The docs link and the bare address (a link too, GitHub's autolink rule).
        assert_eq!(m.links.len(), 2);
        let m = inline(r#"<Link href="https://www.x.dev/a">the doc</Link>"#);
        let t = &m.tags[0];
        assert_eq!(t.meta.map(|(a, b)| m.text.encode_utf16().collect::<Vec<_>>()[a..b].to_vec()), Some("x.dev".encode_utf16().collect()));
        assert_eq!(t.label(), (1, 8));
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

    /// Each link's words as shown and where it goes.
    fn links(m: &Inline) -> Vec<(&str, &str)> {
        m.links.iter().map(|l| (&m.text[l.0..l.1], l.2.as_str())).collect()
    }

    /// Each struck stretch's words as shown.
    fn struck(m: &Inline) -> Vec<&str> {
        m.strike.iter().map(|s| &m.text[s.0..s.1]).collect()
    }

    #[test]
    fn strike_is_ranged() {
        let m = inline("a ~~gone~~ b ~one~ c a~~mid~~d");
        assert_eq!(m.text, "a gone b one c amidd");
        assert_eq!(struck(&m), ["gone", "one", "mid"]);
        assert!(m.runs.is_empty());
    }

    /// Strikethrough holds emphasis, links and code, and a link's words may hold it.
    #[test]
    fn strike_nests() {
        let m = inline("~~**bold** struck~~ and ~~[x](u) `c` *i*~~ [a ~~b~~](v)");
        assert_eq!(m.text, "bold struck and x c i a b");
        assert_eq!(struck(&m), ["bold struck", "x c i", "b"]);
        assert_eq!(m.runs.iter().find(|r| r.start == 0).map(|r| (r.end, r.face)), Some((4, Face::Bold)));
        assert!(m.runs.iter().any(|r| r.face == Face::Italic && &m.text[r.start..r.end] == "i"));
        assert_eq!(links(&m), [("x", "u"), ("a b", "v")]);
        assert_eq!(m.code, [(18, 19)]);
        // Across a soft break.
        assert_eq!(struck(&inline("~~a\nb~~")), ["a b"]);
    }

    /// Tildes without a partner of the same count, three or more, or not flanking their words stay tildes.
    #[test]
    fn strike_unclosed_stays_tildes() {
        for src in ["~~open", "~~a~", "~a~~", "~~~x~~~", "a ~~ b ~~", "~~a [b~~](u)"] {
            let m = inline(src);
            assert!(m.strike.is_empty(), "{src}: {:?}", m.strike);
        }
        assert_eq!(inline("~~open").text, "~~open");
        assert_eq!(inline("~~~x~~~").text, "~~~x~~~");
        assert_eq!(inline("~~a [b~~](u)").text, "~~a b~~");
    }

    #[test]
    fn strike_escaped() {
        let m = inline(r"\~\~a\~\~ \~~b~~");
        assert_eq!(m.text, "~~a~~ ~~b~~");
        assert!(m.strike.is_empty());
        // An escaped tilde next to a real pair: the pair still strikes.
        let m = inline(r"~~a\~~~");
        assert_eq!(m.text, "a~");
        assert_eq!(struck(&m), ["a~"]);
    }

    /// Code spans are literal: no strike inside, and a strike over one skips the ticks' tildes.
    #[test]
    fn strike_not_in_code() {
        let m = inline("`~~x~~` and ``a ~~ b``");
        assert_eq!(m.text, "~~x~~ and a ~~ b");
        assert!(m.strike.is_empty());
        let m = inline("~~a `~~` b~~");
        assert_eq!(m.text, "a ~~ b");
        assert_eq!(struck(&m), ["a ~~ b"]);
        assert_eq!(m.code, [(2, 4)]);
    }

    /// `<scheme:...>` and `<someone@host>` are links to themselves; other angle brackets are words.
    #[test]
    fn autolinks() {
        let m = inline("see <https://x.dev/a?b=1> or <me@x.dev>, not <a b> or <nope> or 1 < 2");
        assert_eq!(m.text, "see https://x.dev/a?b=1 or me@x.dev, not <a b> or <nope> or 1 < 2");
        assert_eq!(links(&m), [("https://x.dev/a?b=1", "https://x.dev/a?b=1"), ("me@x.dev", "mailto:me@x.dev")]);
        // The address is not markdown: its underscores and stars are its own.
        let m = inline("<https://x.dev/a_b_*c*>");
        assert_eq!(m.text, "https://x.dev/a_b_*c*");
        assert!(m.runs.is_empty());
    }

    /// Bare `https://`, `http://` and `www.` addresses are links, less the punctuation that ends their sentence.
    #[test]
    fn bare_urls() {
        let m = inline("go to https://x.dev/a_b. (see https://y.dev/(p)) www.z.dev, http://localhost:3000/x! a&amp; https://q.io/?a=1&amp;");
        assert_eq!(
            links(&m),
            [
                ("https://x.dev/a_b", "https://x.dev/a_b"),
                ("https://y.dev/(p)", "https://y.dev/(p)"),
                ("www.z.dev", "http://www.z.dev"),
                ("http://localhost:3000/x", "http://localhost:3000/x"),
                ("https://q.io/?a=1", "https://q.io/?a=1"),
            ]
        );
        assert_eq!(m.text, "go to https://x.dev/a_b. (see https://y.dev/(p)) www.z.dev, http://localhost:3000/x! a&amp; https://q.io/?a=1&amp;");
        // Only where a word starts, never mid-word; `www.` needs a domain; emphasis around one still holds.
        for src in ["xhttps://a.dev", "www.", "https://", "www.a_b.c_d"] {
            assert!(inline(src).links.is_empty(), "{src}");
        }
        let m = inline("*see https://x.dev/a_b_c*");
        assert_eq!(m.text, "see https://x.dev/a_b_c");
        assert_eq!(m.runs.iter().map(|r| (r.start, r.end, r.face)).collect::<Vec<_>>(), [(0, 23, Face::Italic)]);
        assert_eq!(links(&m), [("https://x.dev/a_b_c", "https://x.dev/a_b_c")]);
        // A link holds no link: an address in its words is words.
        assert_eq!(links(&inline("[https://x.dev](https://y.dev)")), [("https://x.dev", "https://y.dev")]);
    }

    /// Two trailing spaces or a backslash before a newline break the line; any other newline is a space, and the next
    /// line's indent goes. A code span keeps its own spaces and reads a newline as a space.
    #[test]
    fn hard_breaks() {
        assert_eq!(inline("a  \nb").text, "a\nb");
        assert_eq!(inline("a   \n  b").text, "a\nb");
        assert_eq!(inline("a\\\nb").text, "a\nb");
        assert_eq!(inline("a \nb").text, "a b");
        assert_eq!(inline("a\n   b").text, "a b");
        assert_eq!(inline("**a**  \nb").text, "a\nb");
        let m = inline("`a  `\nb");
        assert_eq!((m.text.as_str(), m.code.as_slice()), ("a   b", &[(0, 3)][..]));
        assert_eq!(inline("`a\nb`").text, "a b");
        // An escaped backslash before a newline is a backslash, then a soft break.
        assert_eq!(inline("a\\\\\nb").text, "a\\ b");
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
