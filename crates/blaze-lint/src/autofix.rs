//! Ergonomic auto-fixable lints and capability-narrowing checks:
//!
//! - [`Rule::IoRef`] (`blaze::io_ref`): rejects borrowed `&Io` / `&mut Io` / `&bstd::io::Io`
//!   parameters and fields so code asks only for the narrow capability it uses (`&Out`, `&Fs`,
//!   `&Clock`, `&Host`, `&Args`, `CxSpawn`, ...).
//! - [`Rule::UnusedImport`] (`blaze::unused_import`): detects unreferenced private `use` bindings
//!   and emits mechanical fixes that delete dead `use` lines, prune unused items from braced
//!   groups, and collapse single-item braces (`use a::{X, Y};` -> `use a::Y;`, `use a::{X};` ->
//!   `use a::X;`, `use a::{self, X};` -> `use a;`).
//! - [`Rule::UnusedMut`] (`blaze::unused_mut`): detects `let mut x = ...;` and `fn f(mut x: T)`
//!   bindings that are never mutated in their scope and emits a mechanical fix removing `mut `.

use crate::engine::{Cand, K, Pass, Plan, Tok};
use crate::{Arg, Rule, Run, Span, Template};

/// Maximum entries in one single-level braced `use` group analyzed without heap allocation.
const MAX_USE_GROUP: usize = 64;

#[derive(Clone, Copy)]
struct UseEntry {
    /// Trimmed span of the entry (`Missing`, `Missing as M`, or `self`).
    text: Span,
    /// Span of the bound identifier in `text` (used for diagnostics and matching).
    name: Span,
    /// Entry is `self` (binds the module segment right before `::{`).
    is_self: bool,
    /// Entry is `... as _` (anonymous trait-in-scope import; never unused).
    is_anon: bool,
    /// Whether the bound name is referenced in the enclosing scope outside this `use`.
    used: bool,
}

impl UseEntry {
    const EMPTY: UseEntry = UseEntry {
        text: Span { start: 0, end: 0 },
        name: Span { start: 0, end: 0 },
        is_self: false,
        is_anon: false,
        used: false,
    };
}

/// Safe text slice helper (no panics / unchecked indexing).
#[inline]
fn text_slice(text: &str, span: Span) -> &str {
    let start = usize::try_from(span.start).unwrap_or(usize::MAX);
    let end = usize::try_from(span.end).unwrap_or(usize::MAX);
    text.get(start..end).unwrap_or("")
}

#[inline]
fn tok_str<'u>(text: &'u str, tok: Tok) -> &'u str {
    text_slice(text, Span::new(tok.start, tok.end))
}

#[inline]
fn is_ident(text: &str, tok: Tok, want: &str) -> bool {
    tok.k == K::Ident && tok_str(text, tok) == want
}

#[inline]
fn is_punct(tok: Tok, ch: u8) -> bool {
    tok.k == K::P(ch)
}

#[inline]
fn is_double_colon(toks: &[Tok], at: usize) -> bool {
    match (toks.get(at), toks.get(at + 1)) {
        (Some(a), Some(b)) => is_punct(*a, b':') && is_punct(*b, b':') && a.end == b.start,
        _ => false,
    }
}

/// Returns `true` when the item/file declares a local `struct Io`, `enum Io`, `trait Io`, or `type Io`.
fn declares_local_io(text: &str, toks: &[Tok]) -> bool {
    let mut i = 0usize;
    // Bounded: advances by 1 each iteration up to `toks.len()`.
    while i + 1 < toks.len() {
        if let (Some(a), Some(b)) = (toks.get(i), toks.get(i + 1)) {
            if a.k == K::Ident && matches!(tok_str(text, *a), "struct" | "enum" | "trait" | "type") && is_ident(text, *b, "Io") {
                return true;
            }
        }
        i += 1;
    }
    false
}

/// Skips balanced brackets starting at `open` (`[`, `(`, or `{`); returns the index of the
/// matching closer, or `None` if unbalanced.
fn matching_close(toks: &[Tok], open: usize, open_ch: u8, close_ch: u8) -> Option<usize> {
    let mut depth = 0u32;
    let mut i = open;
    // Bounded: advances by 1 up to `toks.len()`.
    while let Some(t) = toks.get(i) {
        if is_punct(*t, open_ch) {
            depth = depth.saturating_add(1);
        } else if is_punct(*t, close_ch) {
            depth = depth.saturating_sub(1);
            if depth == 0 {
                return Some(i);
            }
        }
        i += 1;
    }
    None
}

/// Checks if the token at `start` is immediately preceded by any attribute `#[...]` containing
/// one of `skip_words` (e.g. `cfg`, `allow`, `expect`).
fn has_leading_attr(text: &str, toks: &[Tok], start: usize, skip_words: &[&str]) -> bool {
    let mut cur = start;
    // Walk backwards over contiguous outer attributes `#[...]`.
    // Bounded: `cur` strictly decreases on every iteration.
    while cur > 0 {
        let prev = cur - 1;
        let Some(t) = toks.get(prev) else { break };
        if !is_punct(*t, b']') {
            break;
        }
        // Find matching `[` backwards.
        let mut depth = 0u32;
        let mut open = prev;
        let mut found_open = None;
        loop {
            if let Some(tok) = toks.get(open) {
                if is_punct(*tok, b']') {
                    depth = depth.saturating_add(1);
                } else if is_punct(*tok, b'[') {
                    depth = depth.saturating_sub(1);
                    if depth == 0 {
                        found_open = Some(open);
                        break;
                    }
                }
            }
            if open == 0 {
                break;
            }
            open -= 1;
        }
        let Some(open_idx) = found_open else { break };
        // Must be preceded by `#` (and not `#!`).
        let is_outer_attr = open_idx > 0 && toks.get(open_idx - 1).is_some_and(|p| is_punct(*p, b'#'));
        if !is_outer_attr {
            break;
        }
        // Scan inside `[...]` for any skip word.
        let mut k = open_idx + 1;
        while k < prev {
            if let Some(tok) = toks.get(k) {
                if tok.k == K::Ident {
                    let word = tok_str(text, *tok);
                    if skip_words.contains(&word) {
                        return true;
                    }
                }
            }
            k += 1;
        }
        cur = open_idx - 1;
    }
    false
}

/// Computes a clean line deletion span for a statement spanning `stmt_start..stmt_end`:
/// if only horizontal whitespace precedes `stmt_start` on its line and only horizontal whitespace
/// plus `\n` (or `\r\n`) follows `stmt_end`, deletes the whole line including its newline.
fn line_delete_span(bytes: &[u8], stmt_start: u32, stmt_end: u32) -> Span {
    let start = usize::try_from(stmt_start).unwrap_or(0);
    let end = usize::try_from(stmt_end).unwrap_or(0);
    // Scan backwards to start of line.
    let mut ls = start;
    while ls > 0 {
        let b = bytes.get(ls - 1).copied().unwrap_or(b'\n');
        if b == b' ' || b == b'\t' {
            ls -= 1;
        } else {
            break;
        }
    }
    let at_line_start = ls == 0 || bytes.get(ls - 1).copied() == Some(b'\n');
    // Scan forwards to end of line.
    let mut le = end;
    while let Some(&b) = bytes.get(le) {
        if b == b' ' || b == b'\t' {
            le += 1;
        } else {
            break;
        }
    }
    let at_line_end = match bytes.get(le).copied() {
        Some(b'\n') => {
            le += 1;
            true
        }
        Some(b'\r') if bytes.get(le + 1).copied() == Some(b'\n') => {
            le += 2;
            true
        }
        None => true,
        _ => false,
    };
    if at_line_start && at_line_end {
        let s = u32::try_from(ls).unwrap_or(stmt_start);
        let e = u32::try_from(le).unwrap_or(stmt_end);
        Span::new(s, e)
    } else {
        Span::new(stmt_start, stmt_end)
    }
}

/// Checks if `name` appears as an identifier anywhere in `toks[scope_start..scope_end]`
/// outside the excluded range `exclude_start..=exclude_end`, skipping the leaf names of other
/// `use` statements (so duplicate `use a::Foo; use a::Foo;` does not count as a use of `Foo`,
/// while `use foo::bar; use bar::Baz;` still sees `bar` as the root segment of the second `use`).
fn ident_used_in_scope(
    text: &str,
    toks: &[Tok],
    scope_start: usize,
    scope_end: usize,
    exclude_start: usize,
    exclude_end: usize,
    name: &str,
) -> bool {
    let mut i = scope_start;
    let end = scope_end.min(toks.len());
    let mut in_other_use = false;
    let mut use_first_seg = false;
    // Bounded: advances by at least 1 each iteration.
    while i < end {
        if i >= exclude_start && i <= exclude_end {
            i = exclude_end.saturating_add(1);
            in_other_use = false;
            use_first_seg = false;
            continue;
        }
        if let Some(t) = toks.get(i) {
            if !in_other_use && is_ident(text, *t, "use") {
                in_other_use = true;
                use_first_seg = true;
                i += 1;
                continue;
            }
            if in_other_use {
                if is_punct(*t, b';') {
                    in_other_use = false;
                    use_first_seg = false;
                    i += 1;
                    continue;
                }
                if use_first_seg && matches!(t.k, K::Ident | K::Raw) {
                    use_first_seg = false;
                    let raw = tok_str(text, *t);
                    let clean = raw.strip_prefix("r#").unwrap_or(raw);
                    if clean == name {
                        return true;
                    }
                } else if !is_punct(*t, b':') {
                    use_first_seg = false;
                }
                i += 1;
                continue;
            }
            if matches!(t.k, K::Ident | K::Raw) {
                let raw = tok_str(text, *t);
                let clean = raw.strip_prefix("r#").unwrap_or(raw);
                if clean == name {
                    return true;
                }
            }
        }
        i += 1;
    }
    false
}

/// Returns `true` if `toks[from..to]` contains any `K::Hole` token.
fn has_hole(toks: &[Tok], from: usize, to: usize) -> bool {
    let mut i = from;
    let end = to.min(toks.len());
    while i < end {
        if toks.get(i).is_some_and(|t| t.k == K::Hole) {
            return true;
        }
        i += 1;
    }
    false
}

/// Finds the enclosing `{ ... }` block token range `[open+1, close)` containing `pos` when
/// `brace_depth > 0`, or `(0, toks.len())` when `brace_depth == 0`.
fn enclosing_brace_scope(toks: &[Tok], pos: usize) -> Option<(usize, usize)> {
    // Walk backwards to find the innermost unclosed `{`.
    let mut depth = 0i32;
    let mut i = pos;
    let mut open_brace = None;
    while i > 0 {
        i -= 1;
        let Some(t) = toks.get(i) else { break };
        if is_punct(*t, b'}') {
            depth += 1;
        } else if is_punct(*t, b'{') {
            if depth == 0 {
                open_brace = Some(i);
                break;
            }
            depth -= 1;
        }
    }
    match open_brace {
        None => Some((0, toks.len())),
        Some(open) => {
            let close = matching_close(toks, open, b'{', b'}')?;
            Some((open + 1, close))
        }
    }
}

/// Checks if a `use` at `u` is preceded by `pub` or `pub(...)`.
fn is_pub_use(text: &str, toks: &[Tok], u: usize) -> bool {
    if u == 0 {
        return false;
    }
    let Some(prev) = toks.get(u - 1) else { return false };
    if is_ident(text, *prev, "pub") {
        return true;
    }
    // `pub(crate) use`, `pub(super) use`, `pub(in ...) use`
    if is_punct(*prev, b')') {
        let mut depth = 0u32;
        let mut k = u - 1;
        loop {
            if let Some(t) = toks.get(k) {
                if is_punct(*t, b')') {
                    depth = depth.saturating_add(1);
                } else if is_punct(*t, b'(') {
                    depth = depth.saturating_sub(1);
                    if depth == 0 {
                        return k > 0 && toks.get(k - 1).is_some_and(|p| is_ident(text, *p, "pub"));
                    }
                }
            }
            if k == 0 {
                break;
            }
            k -= 1;
        }
    }
    false
}

/// Checks whether a token immediately following an identifier is an assignment or compound
/// assignment (`=`, `+=`, `-=`, `*=`, `/=`, `%=`, `&=`, `|=`, `^=`, `<<=`, `>>=`), excluding
/// `==` and `=>`.
fn is_assign_after(toks: &[Tok], ident_idx: usize) -> bool {
    let Some(t1) = toks.get(ident_idx + 1) else { return false };
    if is_punct(*t1, b'=') {
        // Reject `==` and `=>`.
        let next_same_op = toks
            .get(ident_idx + 2)
            .is_some_and(|t2| t1.end == t2.start && (is_punct(*t2, b'=') || is_punct(*t2, b'>')));
        return !next_same_op;
    }
    // Single-char compound ops: `+=`, `-=`, `*=`, `/=`, `%=`, `&=`, `|=`, `^=`.
    if let K::P(ch) = t1.k {
        if matches!(ch, b'+' | b'-' | b'*' | b'/' | b'%' | b'&' | b'|' | b'^') {
            if let Some(t2) = toks.get(ident_idx + 2) {
                return t1.end == t2.start && is_punct(*t2, b'=');
            }
        }
        // Shift compound ops: `<<=`, `>>=`.
        if matches!(ch, b'<' | b'>') {
            if let (Some(t2), Some(t3)) = (toks.get(ident_idx + 2), toks.get(ident_idx + 3)) {
                return t1.end == t2.start && t2.end == t3.start && is_punct(*t2, ch) && is_punct(*t3, b'=');
            }
        }
    }
    false
}

/// Scans `toks[scan_start..scan_end]` to see if local variable `var_name` may be mutated.
/// Conservative: returns `true` on any `&mut`, `*`, assignment/compound-assignment, method/field
/// access (`var.`), indexing (`var[`), or macro invocation argument containing `var`.
fn var_may_be_mutated(text: &str, toks: &[Tok], scan_start: usize, scan_end: usize, var_name: &str) -> bool {
    let mut i = scan_start;
    let mut end = scan_end.min(toks.len());
    let mut brace_depth = 0u32;
    let mut paren_depth = 0u32;
    let mut bracket_depth = 0u32;
    // Track macro call delimiter depth so any occurrence of `var_name` inside `foo!(...)`
    // is conservatively treated as potentially mutated.
    let mut macro_until: Option<usize> = None;

    // Bounded: `i` advances by at least 1 each iteration.
    while i < end {
        if let Some(until) = macro_until {
            if i > until {
                macro_until = None;
            }
        }
        let Some(tok) = toks.get(i).copied() else { break };

        // Detect shadowing `let [mut] <var_name>` at depth 0: finish scanning its initializer
        // (which can still mutate the outer `var_name`), then stop!
        if brace_depth == 0 && paren_depth == 0 && bracket_depth == 0 && is_ident(text, tok, "let") {
            let mut pat = i + 1;
            if toks.get(pat).is_some_and(|t| is_ident(text, *t, "mut")) {
                pat += 1;
            }
            if toks.get(pat).is_some_and(|t| is_ident(text, *t, var_name))
                && toks.get(pat + 1).is_some_and(|t| is_punct(*t, b'=') || is_punct(*t, b':'))
            {
                // Find the `;` ending this shadowing `let`'s initializer.
                let mut k = pat + 1;
                let mut d_b = 0u32;
                let mut d_p = 0u32;
                let mut d_k = 0u32;
                let mut semi = end;
                while k < end {
                    if let Some(tk) = toks.get(k) {
                        match tk.k {
                            K::P(b'{') => d_b = d_b.saturating_add(1),
                            K::P(b'}') => d_b = d_b.saturating_sub(1),
                            K::P(b'(') => d_p = d_p.saturating_add(1),
                            K::P(b')') => d_p = d_p.saturating_sub(1),
                            K::P(b'[') => d_k = d_k.saturating_add(1),
                            K::P(b']') => d_k = d_k.saturating_sub(1),
                            K::P(b';') if d_b == 0 && d_p == 0 && d_k == 0 => {
                                semi = k;
                                break;
                            }
                            _ => {}
                        }
                    }
                    k += 1;
                }
                // Scan the initializer expression (after `=`) up to `semi`, then stop.
                let mut eq = pat + 1;
                while eq < semi && !toks.get(eq).is_some_and(|t| is_punct(*t, b'=')) {
                    eq += 1;
                }
                if eq < semi {
                    i = eq + 1;
                    end = semi;
                    continue;
                }
                return false;
            }
        }

        // Detect macro invocation `ident ! ( ... )` / `[ ... ]` / `{ ... }`.
        if macro_until.is_none()
            && tok.k == K::Ident
            && toks.get(i + 1).is_some_and(|t| is_punct(*t, b'!'))
            && let Some(open) = toks.get(i + 2)
        {
            let close = match open.k {
                K::P(b'(') => matching_close(toks, i + 2, b'(', b')'),
                K::P(b'[') => matching_close(toks, i + 2, b'[', b']'),
                K::P(b'{') => matching_close(toks, i + 2, b'{', b'}'),
                _ => None,
            };
            if let Some(c) = close {
                macro_until = Some(c);
            }
        }

        match tok.k {
            K::P(b'{') => brace_depth = brace_depth.saturating_add(1),
            K::P(b'}') => brace_depth = brace_depth.saturating_sub(1),
            K::P(b'(') => paren_depth = paren_depth.saturating_add(1),
            K::P(b')') => paren_depth = paren_depth.saturating_sub(1),
            K::P(b'[') => bracket_depth = bracket_depth.saturating_add(1),
            K::P(b']') => bracket_depth = bracket_depth.saturating_sub(1),
            K::Ident if tok_str(text, tok) == var_name => {
                // Skip field/method names (`.x`) and path segments (`a::x`) and struct field labels (`x:`).
                let after_dot = i > scan_start && toks.get(i - 1).is_some_and(|p| is_punct(*p, b'.'));
                let after_colons = i >= scan_start + 2 && is_double_colon(toks, i - 2);
                let field_label = toks.get(i + 1).is_some_and(|n| is_punct(*n, b':'))
                    && !is_double_colon(toks, i + 1);
                if !after_dot && !after_colons && !field_label {
                    // Inside a macro call -> conservatively assume macro could mutate `var_name`.
                    if macro_until.is_some() {
                        return true;
                    }
                    // Preceded by `mut` (`&mut x`, `ref mut x`) or `*` (`*x = 1`, `DerefMut`).
                    if i > scan_start {
                        if let Some(prev) = toks.get(i - 1) {
                            if is_ident(text, *prev, "mut") || is_punct(*prev, b'*') {
                                return true;
                            }
                        }
                    }
                    // Followed by `=`, compound assignment, `.` (method/field), or `[` (IndexMut).
                    if is_assign_after(toks, i) {
                        return true;
                    }
                    if let Some(next) = toks.get(i + 1) {
                        if is_punct(*next, b'.') || is_punct(*next, b'[') {
                            return true;
                        }
                    }
                }
            }
            _ => {}
        }
        i += 1;
    }
    false
}

impl<'u, 's, 'a> Pass<'u, 's, 'a> {
    /// Appends `s` to `self.fix_buf` and returns its [`Run`].
    fn push_fix_str(&mut self, s: &str) -> Run {
        let from = u32::try_from(self.fix_buf.len()).unwrap_or(u32::MAX);
        self.fix_buf.extend_from_slice(s.as_bytes());
        let to = u32::try_from(self.fix_buf.len()).unwrap_or(u32::MAX);
        Run { from, to }
    }

    /// Rule [`Rule::IoRef`]: flags borrowed `&Io` / `&mut Io` / `&bstd::io::Io` outside tests.
    pub(crate) fn io_ref_pass(&mut self) {
        let local_io = declares_local_io(self.text, self.toks);
        let len = self.toks.len();
        let mut i = 0usize;
        let mut in_use = false;
        // Bounded: `i` increases by at least 1 each iteration.
        while i < len {
            let Some(tok) = self.toks.get(i).copied() else { break };
            if is_ident(self.text, tok, "use") {
                in_use = true;
                i += 1;
                continue;
            }
            if in_use {
                if is_punct(tok, b';') {
                    in_use = false;
                }
                i += 1;
                continue;
            }
            // Skip attributes `#[...]` / `#![...]`.
            if is_punct(tok, b'#') {
                let open = if self.toks.get(i + 1).is_some_and(|t| is_punct(*t, b'!')) { i + 2 } else { i + 1 };
                if self.toks.get(open).is_some_and(|t| is_punct(*t, b'[')) {
                    if let Some(close) = matching_close(self.toks, open, b'[', b']') {
                        i = close + 1;
                        continue;
                    }
                }
            }
            if is_punct(tok, b'&') {
                // Skip `&&` (logical AND).
                if self.toks.get(i + 1).is_some_and(|n| is_punct(*n, b'&') && tok.end == n.start) {
                    i += 2;
                    continue;
                }
                let mut j = i + 1;
                let mut has_life_or_mut = false;
                if self.toks.get(j).is_some_and(|t| t.k == K::Life) {
                    has_life_or_mut = true;
                    j += 1;
                }
                if self.toks.get(j).is_some_and(|t| is_ident(self.text, *t, "mut")) {
                    has_life_or_mut = true;
                    j += 1;
                }
                // Guard against bitwise AND (`x & Io`): require type-position token before `&`
                // unless `'a` or `mut` unambiguously marks a reference.
                let type_before = has_life_or_mut
                    || i == 0
                    || self.toks.get(i - 1).is_some_and(|p| {
                        matches!(p.k, K::P(b':' | b'>' | b'(' | b',' | b'<' | b'[' | b'=' | b'|'))
                    });
                if type_before {
                    // Check `&Io`, `&bstd::Io`, `&bstd::io::Io`.
                    let hit_end = if !local_io
                        && self.toks.get(j).is_some_and(|t| is_ident(self.text, *t, "Io"))
                        && !is_double_colon(self.toks, j + 1)
                    {
                        self.toks.get(j).map(|t| t.end)
                    } else if self.toks.get(j).is_some_and(|t| is_ident(self.text, *t, "bstd"))
                        && is_double_colon(self.toks, j + 1)
                    {
                        let seg1 = j + 3;
                        if self.toks.get(seg1).is_some_and(|t| is_ident(self.text, *t, "Io"))
                            && !is_double_colon(self.toks, seg1 + 1)
                        {
                            self.toks.get(seg1).map(|t| t.end)
                        } else if self.toks.get(seg1).is_some_and(|t| is_ident(self.text, *t, "io"))
                            && is_double_colon(self.toks, seg1 + 1)
                            && self.toks.get(seg1 + 3).is_some_and(|t| is_ident(self.text, *t, "Io"))
                            && !is_double_colon(self.toks, seg1 + 4)
                        {
                            self.toks.get(seg1 + 3).map(|t| t.end)
                        } else {
                            None
                        }
                    } else {
                        None
                    };
                    if let Some(end) = hit_end {
                        self.cost.add(Rule::IoRef);
                        self.cands.push(Cand {
                            template: Template::IoRef,
                            range: Span::new(tok.start, end),
                            arg: None,
                            plan: Plan::None,
                        });
                    }
                }
            }
            i += 1;
        }
    }

    /// Rule [`Rule::UnusedImport`]: flags unused private `use` bindings and minimizes braced
    /// import lists with mechanical fixes.
    ///
    /// When `top_level` is `false` (per-item `engine::run_with`), only block-local `use` items
    /// (`brace_depth > 0`) are checked so item-local linting stays independent of sibling items.
    /// When `top_level` is `true` (whole-file `lint_file_imports`), file-level `brace_depth == 0`
    /// `use` statements are checked against the entire file.
    pub(crate) fn unused_import_pass(&mut self, top_level: bool) {
        let len = self.toks.len();
        let mut i = 0usize;
        let mut cur_brace_depth = 0u32;
        // Bounded: `i` increases by at least 1 each iteration.
        while i < len {
            let Some(u_tok) = self.toks.get(i).copied() else { break };
            if is_punct(u_tok, b'{') {
                cur_brace_depth = cur_brace_depth.saturating_add(1);
                i += 1;
                continue;
            }
            if is_punct(u_tok, b'}') {
                cur_brace_depth = cur_brace_depth.saturating_sub(1);
                i += 1;
                continue;
            }
            // Skip attributes `#[...]` / `#![...]`.
            if is_punct(u_tok, b'#') {
                let open = if self.toks.get(i + 1).is_some_and(|t| is_punct(*t, b'!')) { i + 2 } else { i + 1 };
                if self.toks.get(open).is_some_and(|t| is_punct(*t, b'[')) {
                    if let Some(close) = matching_close(self.toks, open, b'[', b']') {
                        i = close + 1;
                        continue;
                    }
                }
            }
            if !is_ident(self.text, u_tok, "use") {
                i += 1;
                continue;
            }
            // Match scope level: block-local (`cur_brace_depth > 0`) vs top-level (`cur_brace_depth == 0`).
            if top_level != (cur_brace_depth == 0) {
                i += 1;
                continue;
            }
            // Skip path segments (`a::use`), `pub use`, and `#[cfg]`/`#[allow]`-guarded imports.
            if (i >= 2 && is_double_colon(self.toks, i - 2))
                || is_pub_use(self.text, self.toks, i)
                || has_leading_attr(self.text, self.toks, i, &["cfg", "cfg_attr", "allow", "expect", "macro_use"])
            {
                i += 1;
                continue;
            }
            // Find the terminating `;` at matching brace depth.
            let mut semi = None;
            let mut b_depth = 0u32;
            let mut k = i + 1;
            while k < len {
                let Some(tk) = self.toks.get(k) else { break };
                if is_punct(*tk, b'{') {
                    b_depth = b_depth.saturating_add(1);
                } else if is_punct(*tk, b'}') {
                    b_depth = b_depth.saturating_sub(1);
                } else if is_punct(*tk, b';') && b_depth == 0 {
                    semi = Some(k);
                    break;
                }
                k += 1;
            }
            let Some(semi_idx) = semi else {
                i += 1;
                continue;
            };
            let Some(semi_tok) = self.toks.get(semi_idx).copied() else {
                i = semi_idx + 1;
                continue;
            };

            // Determine enclosing scope; skip if scope contains holes (child items omitted).
            let Some((scope_start, scope_end)) = enclosing_brace_scope(self.toks, i) else {
                i = semi_idx + 1;
                continue;
            };
            if has_hole(self.toks, scope_start, scope_end) {
                i = semi_idx + 1;
                continue;
            }

            self.cost.add(Rule::UnusedImport);
            self.check_one_use(i, u_tok, semi_idx, semi_tok, scope_start, scope_end);
            i = semi_idx + 1;
        }
    }

    fn check_one_use(
        &mut self,
        u_idx: usize,
        u_tok: Tok,
        semi_idx: usize,
        semi_tok: Tok,
        scope_start: usize,
        scope_end: usize,
    ) {
        // Bail on glob imports `*`.
        let mut k = u_idx + 1;
        let mut first_lbrace = None;
        while k < semi_idx {
            if let Some(tk) = self.toks.get(k) {
                if is_punct(*tk, b'*') {
                    return;
                }
                if is_punct(*tk, b'{') && first_lbrace.is_none() {
                    first_lbrace = Some(k);
                }
            }
            k += 1;
        }

        match first_lbrace {
            None => {
                // Simple single import: `use a::b::Name;` or `use a::b::Name as Alias;`
                let Some(last) = self.toks.get(semi_idx - 1).copied() else { return };
                // `use a::b::Name as _;` -> anonymous trait import, always used.
                if is_ident(self.text, last, "_")
                    && semi_idx >= 2
                    && self.toks.get(semi_idx - 2).is_some_and(|t| is_ident(self.text, *t, "as"))
                {
                    return;
                }
                // `use a::b::self;` -> binds `b`; `use self;` -> skip.
                let bound_tok = if is_ident(self.text, last, "self") {
                    if semi_idx >= 4 && is_double_colon(self.toks, semi_idx - 3) {
                        match self.toks.get(semi_idx - 4).copied() {
                            Some(seg) if matches!(seg.k, K::Ident | K::Raw) => seg,
                            _ => return,
                        }
                    } else {
                        return;
                    }
                } else if matches!(last.k, K::Ident | K::Raw) {
                    last
                } else {
                    return;
                };
                let raw = tok_str(self.text, bound_tok);
                let clean = raw.strip_prefix("r#").unwrap_or(raw);
                if clean.starts_with('_') {
                    return;
                }
                if !ident_used_in_scope(self.text, self.toks, scope_start, scope_end, u_idx, semi_idx, clean) {
                    let del = line_delete_span(self.bytes, u_tok.start, semi_tok.end);
                    let name_span = Span::new(bound_tok.start, bound_tok.end);
                    self.cands.push(Cand {
                        template: Template::UnusedImport,
                        range: name_span,
                        arg: Some(Arg::Span(name_span)),
                        plan: Plan::ReplaceBuf { range: del, text: Run::default() },
                    });
                }
            }
            Some(lbrace_idx) => {
                // Single-level braced import: `use prefix::{A, B, ...};`
                let Some(rbrace_idx) = matching_close(self.toks, lbrace_idx, b'{', b'}') else { return };
                // Only handle top-level `{...}` right before `;` preceded by `::`.
                if rbrace_idx + 1 != semi_idx || lbrace_idx < u_idx + 3 || !is_double_colon(self.toks, lbrace_idx - 2) {
                    return;
                }
                let Some(lbrace_tok) = self.toks.get(lbrace_idx).copied() else { return };
                let Some(rbrace_tok) = self.toks.get(rbrace_idx).copied() else { return };
                let Some(colon1_tok) = self.toks.get(lbrace_idx - 2).copied() else { return };
                let Some(mod_seg_tok) = self.toks.get(lbrace_idx - 3).copied() else { return };

                // Parse comma-separated simple entries inside `{ ... }`.
                let mut entries = [UseEntry::EMPTY; MAX_USE_GROUP];
                let mut count = 0usize;
                let mut cur = lbrace_idx + 1;
                while cur < rbrace_idx {
                    // Find next `,` or `rbrace_idx`.
                    let mut end = cur;
                    while end < rbrace_idx {
                        let Some(tk) = self.toks.get(end) else { return };
                        if is_punct(*tk, b',') {
                            break;
                        }
                        // Bail on nested braces or nested paths (`a::B`) inside `{...}`.
                        if is_punct(*tk, b'{') || is_punct(*tk, b':') {
                            return;
                        }
                        end += 1;
                    }
                    if end > cur {
                        if count >= MAX_USE_GROUP {
                            return;
                        }
                        let Some(first) = self.toks.get(cur).copied() else { return };
                        let Some(last) = self.toks.get(end - 1).copied() else { return };
                        let entry_len = end - cur;
                        let entry = if entry_len == 1 && is_ident(self.text, first, "self") {
                            let used = matches!(mod_seg_tok.k, K::Ident | K::Raw) && {
                                let raw = tok_str(self.text, mod_seg_tok);
                                let clean = raw.strip_prefix("r#").unwrap_or(raw);
                                ident_used_in_scope(self.text, self.toks, scope_start, scope_end, u_idx, semi_idx, clean)
                            };
                            UseEntry {
                                text: Span::new(first.start, first.end),
                                name: Span::new(first.start, first.end),
                                is_self: true,
                                is_anon: false,
                                used,
                            }
                        } else if entry_len == 1 && matches!(first.k, K::Ident | K::Raw) {
                            let raw = tok_str(self.text, first);
                            let clean = raw.strip_prefix("r#").unwrap_or(raw);
                            let used = clean.starts_with('_')
                                || ident_used_in_scope(self.text, self.toks, scope_start, scope_end, u_idx, semi_idx, clean);
                            UseEntry {
                                text: Span::new(first.start, first.end),
                                name: Span::new(first.start, first.end),
                                is_self: false,
                                is_anon: false,
                                used,
                            }
                        } else if entry_len == 3
                            && matches!(first.k, K::Ident | K::Raw)
                            && self.toks.get(cur + 1).is_some_and(|t| is_ident(self.text, *t, "as"))
                            && matches!(last.k, K::Ident | K::Raw)
                        {
                            let raw = tok_str(self.text, last);
                            let clean = raw.strip_prefix("r#").unwrap_or(raw);
                            let is_anon = clean == "_";
                            let used = is_anon
                                || clean.starts_with('_')
                                || ident_used_in_scope(self.text, self.toks, scope_start, scope_end, u_idx, semi_idx, clean);
                            UseEntry {
                                text: Span::new(first.start, last.end),
                                name: Span::new(last.start, last.end),
                                is_self: false,
                                is_anon,
                                used,
                            }
                        } else {
                            // Unrecognized entry shape; bail conservatively.
                            return;
                        };
                        if let Some(slot) = entries.get_mut(count) {
                            *slot = entry;
                        }
                        count += 1;
                    }
                    cur = end + 1;
                }

                if count == 0 {
                    return;
                }

                let mut kept = 0usize;
                let mut first_unused = None;
                let mut only_kept = None;
                let mut idx = 0usize;
                while idx < count {
                    if let Some(e) = entries.get(idx) {
                        if e.used {
                            kept += 1;
                            only_kept = Some(*e);
                        } else if first_unused.is_none() {
                            first_unused = Some(*e);
                        }
                    }
                    idx += 1;
                }

                // Case A: All entries used, but redundant single-item braces `use a::{X};` or `use a::{self};`!
                if kept == count {
                    if count == 1 && let Some(e) = only_kept && !e.is_anon {
                        let (replace_range, replace_run) = if e.is_self {
                            (Span::new(colon1_tok.start, rbrace_tok.end), Run::default())
                        } else {
                            let s = text_slice(self.text, e.text);
                            let run = self.push_fix_str(s);
                            (Span::new(lbrace_tok.start, rbrace_tok.end), run)
                        };
                        self.cands.push(Cand {
                            template: Template::UnusedImport,
                            range: Span::new(lbrace_tok.start, rbrace_tok.end),
                            arg: Some(Arg::Span(e.name)),
                            plan: Plan::ReplaceBuf { range: replace_range, text: replace_run },
                        });
                    }
                    return;
                }

                // Case B: Zero entries used -> delete entire `use` line!
                if kept == 0 {
                    let del = line_delete_span(self.bytes, u_tok.start, semi_tok.end);
                    let report = first_unused.map_or(Span::new(u_tok.start, semi_tok.end), |e| e.name);
                    self.cands.push(Cand {
                        template: Template::UnusedImport,
                        range: report,
                        arg: Some(Arg::Span(report)),
                        plan: Plan::ReplaceBuf { range: del, text: Run::default() },
                    });
                    return;
                }

                // Case C: Exactly 1 entry kept -> collapse braces to minimal single import!
                if kept == 1 && let Some(e) = only_kept {
                    let report = first_unused.map_or(Span::new(lbrace_tok.start, rbrace_tok.end), |u| u.name);
                    let (replace_range, replace_run) = if e.is_self {
                        // `use a::b::{self, Unused};` -> `use a::b;`
                        (Span::new(colon1_tok.start, rbrace_tok.end), Run::default())
                    } else {
                        // `use bstd::io::{Io, Missing};` -> `use bstd::io::Missing;`
                        let s = text_slice(self.text, e.text);
                        let run = self.push_fix_str(s);
                        (Span::new(lbrace_tok.start, rbrace_tok.end), run)
                    };
                    self.cands.push(Cand {
                        template: Template::UnusedImport,
                        range: report,
                        arg: Some(Arg::Span(report)),
                        plan: Plan::ReplaceBuf { range: replace_range, text: replace_run },
                    });
                    return;
                }

                // Case D: 2+ entries kept -> rebuild minimal `{A, C}`!
                let report = first_unused.map_or(Span::new(lbrace_tok.start, rbrace_tok.end), |u| u.name);
                let from = u32::try_from(self.fix_buf.len()).unwrap_or(u32::MAX);
                self.fix_buf.push(b'{');
                let mut wrote = 0usize;
                let mut j = 0usize;
                while j < count {
                    if let Some(e) = entries.get(j) && e.used {
                        if wrote > 0 {
                            self.fix_buf.extend_from_slice(b", ");
                        }
                        self.fix_buf.extend_from_slice(text_slice(self.text, e.text).as_bytes());
                        wrote += 1;
                    }
                    j += 1;
                }
                self.fix_buf.push(b'}');
                let to = u32::try_from(self.fix_buf.len()).unwrap_or(u32::MAX);
                self.cands.push(Cand {
                    template: Template::UnusedImport,
                    range: report,
                    arg: Some(Arg::Span(report)),
                    plan: Plan::ReplaceBuf {
                        range: Span::new(lbrace_tok.start, rbrace_tok.end),
                        text: Run { from, to },
                    },
                });
            }
        }
    }

    /// Rule [`Rule::UnusedMut`]: flags `let mut x = ...;` and `fn f(mut x: T)` bindings that
    /// are never mutated, emitting a mechanical fix removing `mut `.
    pub(crate) fn unused_mut_pass(&mut self) {
        let len = self.toks.len();
        let mut i = 0usize;
        let mut brace_depth = 0u32;
        // Bounded: `i` increases by at least 1 each iteration.
        while i < len {
            let Some(tok) = self.toks.get(i).copied() else { break };
            if is_punct(tok, b'{') {
                brace_depth = brace_depth.saturating_add(1);
                i += 1;
                continue;
            }
            if is_punct(tok, b'}') {
                brace_depth = brace_depth.saturating_sub(1);
                i += 1;
                continue;
            }

            // Case 1: `let mut <ident> [: Ty] = <expr>;` inside a block.
            if brace_depth > 0 && is_ident(self.text, tok, "let") {
                if let (Some(mut_tok), Some(var_tok), Some(after_var)) =
                    (self.toks.get(i + 1).copied(), self.toks.get(i + 2).copied(), self.toks.get(i + 3).copied())
                {
                    if is_ident(self.text, mut_tok, "mut")
                        && var_tok.k == K::Ident
                        && (is_punct(after_var, b'=') || is_punct(after_var, b':'))
                    {
                        let var_name = tok_str(self.text, var_tok);
                        if !var_name.starts_with('_')
                            && var_name != "ref"
                            && !has_leading_attr(self.text, self.toks, i, &["allow", "expect", "cfg"])
                        {
                            // Find the `;` ending this `let` statement (and confirm it has `=`).
                            let mut k = i + 3;
                            let mut d_b = 0u32;
                            let mut d_p = 0u32;
                            let mut d_k = 0u32;
                            let mut saw_eq = false;
                            let mut semi_idx = None;
                            while k < len {
                                let Some(tk) = self.toks.get(k) else { break };
                                match tk.k {
                                    K::P(b'{') => d_b = d_b.saturating_add(1),
                                    K::P(b'}') => {
                                        if d_b == 0 {
                                            break;
                                        }
                                        d_b = d_b.saturating_sub(1);
                                    }
                                    K::P(b'(') => d_p = d_p.saturating_add(1),
                                    K::P(b')') => d_p = d_p.saturating_sub(1),
                                    K::P(b'[') => d_k = d_k.saturating_add(1),
                                    K::P(b']') => d_k = d_k.saturating_sub(1),
                                    K::P(b'=') if d_b == 0 && d_p == 0 && d_k == 0 => saw_eq = true,
                                    K::P(b';') if d_b == 0 && d_p == 0 && d_k == 0 => {
                                        semi_idx = Some(k);
                                        break;
                                    }
                                    _ => {}
                                }
                                k += 1;
                            }
                            if saw_eq && let Some(semi) = semi_idx {
                                if let Some((_, scope_end)) = enclosing_brace_scope(self.toks, i) {
                                    self.cost.add(Rule::UnusedMut);
                                    if !var_may_be_mutated(self.text, self.toks, semi + 1, scope_end, var_name) {
                                        self.cands.push(Cand {
                                            template: Template::UnusedMut,
                                            range: Span::new(mut_tok.start, mut_tok.end),
                                            arg: Some(Arg::Span(Span::new(var_tok.start, var_tok.end))),
                                            plan: Plan::ReplaceBuf {
                                                range: Span::new(mut_tok.start, var_tok.start),
                                                text: Run::default(),
                                            },
                                        });
                                    }
                                }
                            }
                        }
                    }
                }
            }

            // Case 2: `fn name(... mut param: Ty ...) { ... }` (functions with bodies).
            if is_ident(self.text, tok, "fn") && !has_leading_attr(self.text, self.toks, i, &["allow", "expect"]) {
                // Find the parameter list `(...)` and the body `{...}` (skipping `fn` signatures ending in `;`).
                let mut p = i + 1;
                // Skip fn name and optional generic `<...>`.
                while p < len {
                    let Some(tk) = self.toks.get(p) else { break };
                    if is_punct(*tk, b'(') || is_punct(*tk, b';') || is_punct(*tk, b'{') {
                        break;
                    }
                    p += 1;
                }
                if self.toks.get(p).is_some_and(|t| is_punct(*t, b'('))
                    && let Some(rparen) = matching_close(self.toks, p, b'(', b')')
                {
                    // Find body `{` before any `;`.
                    let mut b = rparen + 1;
                    let mut body_open = None;
                    while b < len {
                        let Some(tk) = self.toks.get(b) else { break };
                        if is_punct(*tk, b';') {
                            break;
                        }
                        if is_punct(*tk, b'{') {
                            body_open = Some(b);
                            break;
                        }
                        b += 1;
                    }
                    if let Some(open_b) = body_open && let Some(close_b) = matching_close(self.toks, open_b, b'{', b'}') {
                        // Scan top-level parameters inside `(p+1 .. rparen)` for `mut <ident> :`.
                        let mut q = p + 1;
                        let mut pd = 0u32;
                        let mut bd = 0u32;
                        let mut ad = 0u32;
                        let mut at_param_start = true;
                        while q < rparen {
                            let Some(tk) = self.toks.get(q).copied() else { break };
                            if pd == 0 && bd == 0 && ad == 0 && at_param_start && is_ident(self.text, tk, "mut") {
                                if let (Some(var_tok), Some(colon)) =
                                    (self.toks.get(q + 1).copied(), self.toks.get(q + 2).copied())
                                {
                                    if var_tok.k == K::Ident && is_punct(colon, b':') && !is_double_colon(self.toks, q + 2) {
                                        let var_name = tok_str(self.text, var_tok);
                                        if !var_name.starts_with('_') && var_name != "self" {
                                            self.cost.add(Rule::UnusedMut);
                                            if !var_may_be_mutated(self.text, self.toks, open_b + 1, close_b, var_name) {
                                                self.cands.push(Cand {
                                                    template: Template::UnusedMut,
                                                    range: Span::new(tk.start, tk.end),
                                                    arg: Some(Arg::Span(Span::new(var_tok.start, var_tok.end))),
                                                    plan: Plan::ReplaceBuf {
                                                        range: Span::new(tk.start, var_tok.start),
                                                        text: Run::default(),
                                                    },
                                                });
                                            }
                                        }
                                    }
                                }
                                at_param_start = false;
                            } else {
                                match tk.k {
                                    K::P(b'(') => pd = pd.saturating_add(1),
                                    K::P(b')') => pd = pd.saturating_sub(1),
                                    K::P(b'[') => bd = bd.saturating_add(1),
                                    K::P(b']') => bd = bd.saturating_sub(1),
                                    K::P(b'<') => ad = ad.saturating_add(1),
                                    K::P(b'>') => ad = ad.saturating_sub(1),
                                    K::P(b',') if pd == 0 && bd == 0 && ad == 0 => at_param_start = true,
                                    _ => at_param_start = false,
                                }
                            }
                            q += 1;
                        }
                    }
                }
            }

            i += 1;
        }
    }

    /// Rule [`Rule::HeapLeak`]: flags `.leak()` and `Box::leak(` / `Vec::leak(` outside tests
    /// (allowing `Root::leak(` for process-root initialization).
    pub(crate) fn heap_leak_pass(&mut self) {
        let len = self.toks.len();
        let mut i = 0usize;
        // Bounded: `i` increases by 1 each iteration.
        while i < len {
            let Some(tok) = self.toks.get(i).copied() else { break };
            if is_ident(self.text, tok, "leak")
                && self.toks.get(i + 1).is_some_and(|n| is_punct(*n, b'('))
                && i > 0
            {
                let hit_start = if self.toks.get(i - 1).is_some_and(|p| is_punct(*p, b'.')) {
                    self.toks.get(i - 1).map(|p| p.start)
                } else if i >= 2 && is_double_colon(self.toks, i - 2) {
                    // Allow `Root::leak(` (intentional process-lifetime root allocator).
                    let is_root = i >= 3 && self.toks.get(i - 3).is_some_and(|p| is_ident(self.text, *p, "Root"));
                    if is_root {
                        None
                    } else {
                        self.toks.get(i - 3).map_or(Some(tok.start), |p| Some(p.start))
                    }
                } else {
                    None
                };
                if let Some(start) = hit_start {
                    self.cost.add(Rule::HeapLeak);
                    self.cands.push(Cand {
                        template: Template::HeapLeak,
                        range: Span::new(start, tok.end),
                        arg: None,
                        plan: Plan::None,
                    });
                }
            }
            i += 1;
        }
    }
}
