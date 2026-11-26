//! Pluggable [`TextObjectResolver`] abstraction and composable built-in text objects:
//!
//! - [`BuiltinObjects`]: standard Vim words (`w`/`W`), quotes (`"`/`'`/`` ` ``), balanced
//!   delimiters (`()`/`[]`/`{}`/`<>`), paragraphs (`p`), sentences (`s`), and lines (`l`).
//! - [`DelimSegmentObject`]: general separator-delimited segment object (e.g. `i|`/`a|` for
//!   shell pipeline stages, `i/`/`a/` for path segments, `i,`/`a,` for comma arguments!).
//! - [`ChainObjects`]: zero-cost static composition of custom domain resolvers (AST, shell,
//!   JSON, etc.) with [`BuiltinObjects`].

use crate::buffer::{
    CharClass, char_at, clamp_boundary, classify, line_bounds, next_char, prev_char,
};
use crate::{MotionKind, Span};

/// Inner (`i`) vs Around (`a`) scope of a Vim text object.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ObjectScope {
    Inner,
    Around,
}

/// Pluggable text-object resolver.
///
/// Implement this trait for any structured domain (AST functions `f`, blocks `b`, shell
/// pipelines `|`, markdown sections, table cells) and compose it with [`BuiltinObjects`]
/// via [`ChainObjects`].
pub trait TextObjectResolver {
    fn resolve(
        &self,
        text: &str,
        cursor: usize,
        scope: ObjectScope,
        key: char,
        count: usize,
    ) -> Option<(Span, MotionKind)>;
}

/// Compose a domain-specific `Primary` resolver in front of a `Fallback` resolver.
#[derive(Clone, Copy, Debug, Default)]
pub struct ChainObjects<Primary, Fallback> {
    pub primary: Primary,
    pub fallback: Fallback,
}

impl<P, F> TextObjectResolver for ChainObjects<P, F>
where
    P: TextObjectResolver,
    F: TextObjectResolver,
{
    #[inline]
    fn resolve(
        &self,
        text: &str,
        cursor: usize,
        scope: ObjectScope,
        key: char,
        count: usize,
    ) -> Option<(Span, MotionKind)> {
        self.primary
            .resolve(text, cursor, scope, key, count)
            .or_else(|| self.fallback.resolve(text, cursor, scope, key, count))
    }
}

/// Standard Vim text objects (`w`, `W`, `"`, `'`, `` ` ``, `(`, `)`, `b`, `[`, `]`, `{`, `}`, `B`, `<`, `>`, `p`, `s`, `l`).
#[derive(Clone, Copy, Debug, Default)]
pub struct BuiltinObjects;

impl TextObjectResolver for BuiltinObjects {
    fn resolve(
        &self,
        text: &str,
        cursor: usize,
        scope: ObjectScope,
        key: char,
        count: usize,
    ) -> Option<(Span, MotionKind)> {
        if text.is_empty() {
            return None;
        }
        let n = count.max(1);
        match key {
            'w' => word_object(text, cursor, scope, false, n).map(|sp| (sp, MotionKind::Charwise)),
            'W' => word_object(text, cursor, scope, true, n).map(|sp| (sp, MotionKind::Charwise)),
            '"' | '\'' | '`' => {
                quote_object(text, cursor, scope, key).map(|sp| (sp, MotionKind::Charwise))
            }
            '(' | ')' | 'b' => {
                pair_object(text, cursor, scope, '(', ')', n).map(|sp| (sp, MotionKind::Charwise))
            }
            '[' | ']' => {
                pair_object(text, cursor, scope, '[', ']', n).map(|sp| (sp, MotionKind::Charwise))
            }
            '{' | '}' | 'B' => {
                pair_object(text, cursor, scope, '{', '}', n).map(|sp| (sp, MotionKind::Charwise))
            }
            '<' | '>' => {
                pair_object(text, cursor, scope, '<', '>', n).map(|sp| (sp, MotionKind::Charwise))
            }
            'p' => paragraph_object(text, cursor, scope, n).map(|sp| (sp, MotionKind::Linewise)),
            's' => sentence_object(text, cursor, scope).map(|sp| (sp, MotionKind::Charwise)),
            'l' => line_object(text, cursor, scope),
            _ => None,
        }
    }
}

/// General separator-delimited segment text object (e.g. `|` for pipeline stages, `/` for path
/// segments, `,` for comma-separated arguments).
///
/// - `Inner` (`i<key>`): trimmed segment between adjacent delimiters containing `cursor`.
/// - `Around` (`a<key>`): segment plus one adjacent delimiter (and surrounding spaces for `|`/`,`).
#[derive(Clone, Copy, Debug)]
pub struct DelimSegmentObject {
    pub key: char,
    pub delim: char,
    pub trim_ws: bool,
}

impl TextObjectResolver for DelimSegmentObject {
    fn resolve(
        &self,
        text: &str,
        cursor: usize,
        scope: ObjectScope,
        key: char,
        _count: usize,
    ) -> Option<(Span, MotionKind)> {
        if key != self.key || text.is_empty() {
            return None;
        }
        let lb = line_bounds(text, cursor);
        let cur = clamp_boundary(text, cursor).clamp(lb.start, lb.end);
        let seg = segment_around_delim(text, lb, cur, self.delim, scope, self.trim_ws)?;
        Some((seg, MotionKind::Charwise))
    }
}

/// Shell-oriented text objects layered on top of [`BuiltinObjects`]:
/// - `i|` / `a|`: shell pipeline stage (`where size > 1KB`)
/// - `i/` / `a/`: path segment (`blaze-sh` in `crates/blaze-sh/src`)
/// - `i,` / `a,`: comma-separated list/arg item
/// - All standard [`BuiltinObjects`] (`iw`, `i"`, `i(`, `i{`, `ip`, etc.).
#[derive(Clone, Copy, Debug, Default)]
pub struct ShellObjects;

impl TextObjectResolver for ShellObjects {
    fn resolve(
        &self,
        text: &str,
        cursor: usize,
        scope: ObjectScope,
        key: char,
        count: usize,
    ) -> Option<(Span, MotionKind)> {
        const PIPE: DelimSegmentObject = DelimSegmentObject { key: '|', delim: '|', trim_ws: true };
        const PATH: DelimSegmentObject = DelimSegmentObject { key: '/', delim: '/', trim_ws: false };
        const ARG: DelimSegmentObject = DelimSegmentObject { key: ',', delim: ',', trim_ws: true };

        PIPE.resolve(text, cursor, scope, key, count)
            .or_else(|| PATH.resolve(text, cursor, scope, key, count))
            .or_else(|| ARG.resolve(text, cursor, scope, key, count))
            .or_else(|| BuiltinObjects.resolve(text, cursor, scope, key, count))
    }
}

// =============================================================================
// Text-object geometry helpers
// =============================================================================

fn word_object(
    text: &str,
    cursor: usize,
    scope: ObjectScope,
    big_word: bool,
    count: usize,
) -> Option<Span> {
    let cur = clamp_boundary(text, cursor);
    let idx = if cur >= text.len() { prev_char(text, text.len()) } else { cur };
    let cls = classify(char_at(text, idx)?, big_word);

    let mut start = idx;
    while start > 0 {
        let p = prev_char(text, start);
        if char_at(text, p).map(|c| classify(c, big_word)) != Some(cls) {
            break;
        }
        start = p;
    }

    let mut end = next_char(text, idx);
    while end < text.len() && char_at(text, end).map(|c| classify(c, big_word)) == Some(cls) {
        end = next_char(text, end);
    }

    // Extend for `count > 1`.
    for _ in 1..count {
        if end >= text.len() {
            break;
        }
        let next_cls = classify(char_at(text, end).unwrap(), big_word);
        while end < text.len() && char_at(text, end).map(|c| classify(c, big_word)) == Some(next_cls) {
            end = next_char(text, end);
        }
    }

    if scope == ObjectScope::Around && cls != CharClass::Whitespace {
        let mut after = end;
        while after < text.len()
            && matches!(char_at(text, after), Some(' ' | '\t'))
        {
            after = next_char(text, after);
        }
        if after > end {
            end = after;
        } else {
            // No trailing whitespace: include leading whitespace on the same line instead.
            while start > 0 && matches!(char_at(text, prev_char(text, start)), Some(' ' | '\t')) {
                start = prev_char(text, start);
            }
        }
    }

    Some(Span::new(start, end))
}

fn quote_object(text: &str, cursor: usize, scope: ObjectScope, quote: char) -> Option<Span> {
    let lb = line_bounds(text, cursor);
    let cur = clamp_boundary(text, cursor).clamp(lb.start, lb.end);
    let line = &text[lb.start..lb.end];
    let rel_cur = cur - lb.start;

    // Collect unescaped quote byte offsets on the current line.
    let mut positions = [0usize; 64];
    let mut npos = 0usize;
    let mut escaped = false;
    for (i, ch) in line.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        if ch == '\\' && quote != '\'' {
            escaped = true;
            continue;
        }
        if ch == quote && npos < positions.len() {
            positions[npos] = i;
            npos += 1;
        }
    }
    if npos < 2 {
        return None;
    }

    // Prefer a pair `[q0, q1]` enclosing `rel_cur`; otherwise Vim searches forward on the line.
    let mut chosen: Option<(usize, usize)> = None;
    let mut k = 0;
    while k + 1 < npos {
        let q0 = positions[k];
        let q1 = positions[k + 1];
        if rel_cur >= q0 && rel_cur <= q1 {
            chosen = Some((q0, q1));
            break;
        }
        if q0 > rel_cur && chosen.is_none() {
            chosen = Some((q0, q1));
            break;
        }
        k += 2;
    }
    let (q0, q1) = chosen?;
    let q_len = quote.len_utf8();
    let abs0 = lb.start + q0;
    let abs1 = lb.start + q1;
    match scope {
        ObjectScope::Inner => Some(Span::new(abs0 + q_len, abs1)),
        ObjectScope::Around => Some(Span::new(abs0, abs1 + q_len)),
    }
}

fn pair_object(
    text: &str,
    cursor: usize,
    scope: ObjectScope,
    open: char,
    close: char,
    count: usize,
) -> Option<Span> {
    let cur = clamp_boundary(text, cursor);
    let cur_on_open = char_at(text, cur) == Some(open);

    // Search backward for the `count`-th enclosing unmatched `open` delimiter.
    let mut remaining = count;
    let mut depth = 0usize;
    let mut open_pos = None;
    let mut p = if cur_on_open { next_char(text, cur) } else { cur };

    if cur_on_open {
        open_pos = Some(cur);
        remaining -= 1;
        p = cur;
    }

    while remaining > 0 && p > 0 {
        p = prev_char(text, p);
        if let Some(ch) = char_at(text, p) {
            if ch == close {
                depth += 1;
            } else if ch == open {
                if depth == 0 {
                    open_pos = Some(p);
                    remaining -= 1;
                } else {
                    depth -= 1;
                }
            }
        }
    }

    // If not inside a pair, look forward on the current line for the next `open` (standard Vim).
    let open_idx = match open_pos {
        Some(idx) if remaining == 0 => idx,
        _ => {
            let lb = line_bounds(text, cur);
            let mut q = cur;
            let mut found = None;
            while q < lb.end {
                if char_at(text, q) == Some(open) {
                    found = Some(q);
                    break;
                }
                q = next_char(text, q);
            }
            found?
        }
    };

    // Match forward from `open_idx` to its balancing `close`.
    let mut fwd_depth = 1usize;
    let mut q = next_char(text, open_idx);
    let mut close_idx = None;
    while q < text.len() {
        if let Some(ch) = char_at(text, q) {
            if ch == open {
                fwd_depth += 1;
            } else if ch == close {
                fwd_depth -= 1;
                if fwd_depth == 0 {
                    close_idx = Some(q);
                    break;
                }
            }
        }
        q = next_char(text, q);
    }
    let close_idx = close_idx?;

    match scope {
        ObjectScope::Inner => Some(Span::new(open_idx + open.len_utf8(), close_idx)),
        ObjectScope::Around => Some(Span::new(open_idx, close_idx + close.len_utf8())),
    }
}

fn paragraph_object(text: &str, cursor: usize, scope: ObjectScope, count: usize) -> Option<Span> {
    let lb = line_bounds(text, cursor);
    let on_blank = text[lb.start..lb.end].trim().is_empty();

    let mut start = lb.start;
    while start > 0 {
        let prev = line_bounds(text, start - 1);
        let prev_blank = text[prev.start..prev.end].trim().is_empty();
        if prev_blank != on_blank {
            break;
        }
        start = prev.start;
    }

    let mut end = lb.end;
    let mut cur_blank = on_blank;
    let mut groups = 0usize;
    while end < text.len() {
        let next = line_bounds(text, end + 1);
        let nb = text[next.start..next.end].trim().is_empty();
        if nb != cur_blank {
            groups += 1;
            let target_groups = match scope {
                ObjectScope::Inner => count,
                ObjectScope::Around => count * 2,
            };
            if groups >= target_groups {
                break;
            }
            cur_blank = nb;
        }
        end = next.end;
    }
    if end < text.len() && text.as_bytes()[end] == b'\n' {
        end += 1;
    }
    Some(Span::new(start, end))
}

fn sentence_object(text: &str, cursor: usize, scope: ObjectScope) -> Option<Span> {
    let cur = clamp_boundary(text, cursor);
    let mut start = cur;
    while start > 0 {
        let p = prev_char(text, start);
        if matches!(char_at(text, p), Some('.' | '!' | '?' | '\n')) {
            break;
        }
        start = p;
    }
    while start < text.len() && matches!(char_at(text, start), Some(' ' | '\t')) {
        start = next_char(text, start);
    }

    let mut end = cur;
    while end < text.len() {
        let ch = char_at(text, end).unwrap();
        end = next_char(text, end);
        if matches!(ch, '.' | '!' | '?') {
            break;
        }
        if ch == '\n' {
            end = prev_char(text, end);
            break;
        }
    }
    if scope == ObjectScope::Around {
        while end < text.len() && matches!(char_at(text, end), Some(' ' | '\t')) {
            end = next_char(text, end);
        }
    }
    Some(Span::new(start, end))
}

fn line_object(text: &str, cursor: usize, scope: ObjectScope) -> Option<(Span, MotionKind)> {
    let lb = line_bounds(text, cursor);
    match scope {
        ObjectScope::Inner => {
            let mut s = lb.start;
            let mut e = lb.end;
            while s < e && matches!(char_at(text, s), Some(' ' | '\t')) {
                s = next_char(text, s);
            }
            while e > s && matches!(char_at(text, prev_char(text, e)), Some(' ' | '\t')) {
                e = prev_char(text, e);
            }
            Some((Span::new(s, e), MotionKind::Charwise))
        }
        ObjectScope::Around => {
            let end = if lb.end < text.len() && text.as_bytes()[lb.end] == b'\n' {
                lb.end + 1
            } else {
                lb.end
            };
            Some((Span::new(lb.start, end), MotionKind::Linewise))
        }
    }
}

fn segment_around_delim(
    text: &str,
    lb: Span,
    cur: usize,
    delim: char,
    scope: ObjectScope,
    trim_ws: bool,
) -> Option<Span> {
    let line = &text[lb.start..lb.end];
    let rel_cur = cur.saturating_sub(lb.start);
    let d_len = delim.len_utf8();

    // Find top-level (outside quotes) delimiter positions on the line.
    let mut delims = [0usize; 64];
    let mut nd = 0usize;
    let mut in_quote: Option<char> = None;
    let mut escaped = false;
    for (i, ch) in line.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        if let Some(q) = in_quote {
            if ch == '\\' && q == '"' {
                escaped = true;
            } else if ch == q {
                in_quote = None;
            }
            continue;
        }
        if matches!(ch, '"' | '\'' | '`') {
            in_quote = Some(ch);
            continue;
        }
        if ch == delim && nd < delims.len() {
            delims[nd] = i;
            nd += 1;
        }
    }
    if nd == 0 {
        return None;
    }

    // Locate the segment `[seg_s, seg_e)` containing `rel_cur`.
    let mut seg_s = 0usize;
    let mut seg_e = line.len();
    let mut left_delim: Option<usize> = None;
    let mut right_delim: Option<usize> = None;

    for &d in &delims[..nd] {
        if d + d_len <= rel_cur {
            seg_s = d + d_len;
            left_delim = Some(d);
        } else {
            seg_e = d;
            right_delim = Some(d);
            break;
        }
    }

    match scope {
        ObjectScope::Inner => {
            let mut s = lb.start + seg_s;
            let mut e = lb.start + seg_e;
            if trim_ws {
                while s < e && matches!(char_at(text, s), Some(' ' | '\t')) {
                    s = next_char(text, s);
                }
                while e > s && matches!(char_at(text, prev_char(text, e)), Some(' ' | '\t')) {
                    e = prev_char(text, e);
                }
            }
            Some(Span::new(s, e))
        }
        ObjectScope::Around => {
            // Include the following delimiter (and its trailing space if `trim_ws`), or the
            // preceding delimiter when on the last segment.
            let (s, e) = if let Some(rd) = right_delim {
                let mut start = lb.start + seg_s;
                if trim_ws && left_delim.is_some() {
                    while start < lb.start + rd
                        && matches!(char_at(text, start), Some(' ' | '\t'))
                    {
                        start = next_char(text, start);
                    }
                }
                let mut end = lb.start + rd + d_len;
                if trim_ws {
                    while end < lb.end && matches!(char_at(text, end), Some(' ' | '\t')) {
                        end = next_char(text, end);
                    }
                }
                (start, end)
            } else if let Some(ld) = left_delim {
                let mut start = lb.start + ld;
                if trim_ws {
                    while start > lb.start
                        && matches!(char_at(text, prev_char(text, start)), Some(' ' | '\t'))
                    {
                        start = prev_char(text, start);
                    }
                }
                (start, lb.start + seg_e)
            } else {
                (lb.start + seg_s, lb.start + seg_e)
            };
            Some(Span::new(s, e))
        }
    }
}
