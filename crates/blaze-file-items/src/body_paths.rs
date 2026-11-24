//! The paths a body's text writes, read by tokens for D189's surface pass (blaze-scope
//! `surface`): a macro call's path ([`call_path`]), what its input holds ([`input_mark`]), a
//! block `use`'s or `extern crate`'s bindings ([`use_bindings`]), a block item's name
//! ([`item_name`]) and a block `macro_rules!`'s name and rules ([`macro_rules_parts`]). No tree,
//! no lowering: each is one read of the item's or call's tokens, and a form it does not read is
//! answered `None` (or `false`), never guessed (rule 3): the surface pass then takes the call to
//! be able to do anything.

use blaze_lex::{Cpu, FrontmatterAllowed, TokenKind, tokenize};
use blaze_own::{AVec, Alloc};
use blaze_surface::{PathKind, Range};

use crate::nested::{ITEM_WORDS, NestedCall, NestedKind, inert_attr, not_a_macro};

/// The most segments a path read here holds (rule 18): a longer path is not read.
pub const MAX_SEGMENTS: usize = 16;

/// A path as written: where its first segment is read ([`PathKind`]: never `DollarCrate`, which
/// no hand-written text holds) and its segments after `crate`, `self`, the `super`s or `::`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WrittenPath<'t> {
    pub kind: PathKind,
    segments: [&'t str; MAX_SEGMENTS],
    len: usize,
}

impl<'t> WrittenPath<'t> {
    /// No segment yet, read from `kind`.
    pub const fn empty(kind: PathKind) -> WrittenPath<'t> {
        WrittenPath { kind, segments: [""; MAX_SEGMENTS], len: 0 }
    }

    pub fn segments(&self) -> &[&'t str] {
        &self.segments[..self.len]
    }

    /// Appends `segment`; `false` (nothing changed) past [`MAX_SEGMENTS`].
    #[must_use]
    pub fn push(&mut self, segment: &'t str) -> bool {
        match self.segments.get_mut(self.len) {
            Some(slot) => {
                *slot = segment;
                self.len += 1;
                true
            }
            None => false,
        }
    }

    /// This path with its first segment replaced by `by`'s whole path (a block `use` that binds
    /// it): `None` past [`MAX_SEGMENTS`].
    pub fn through(&self, by: &WrittenPath<'t>) -> Option<WrittenPath<'t>> {
        let mut out = *by;
        for &s in self.segments().get(1..).unwrap_or(&[]) {
            if !out.push(s) {
                return None;
            }
        }
        Some(out)
    }
}

/// A significant token of a text: its kind and its text.
#[derive(Clone, Copy)]
struct Tok<'t> {
    kind: TokenKind,
    text: &'t str,
    /// Its offset in the text read.
    at: u32,
}

fn tokens<'t, 'a>(text: &'t str, alloc: Alloc<'a>) -> AVec<'a, Tok<'t>> {
    let mut out: AVec<'a, Tok<'t>> = AVec::new_in(alloc);
    let mut at = 0usize;
    for token in tokenize(text, FrontmatterAllowed::No, Cpu::BASELINE) {
        let start = at;
        at += token.len as usize;
        if matches!(token.kind, TokenKind::Whitespace | TokenKind::LineComment { .. } | TokenKind::BlockComment { .. }) {
            continue;
        }
        let Some(t) = text.get(start..at) else { break };
        out.push(Tok { kind: token.kind, text: t, at: u32::try_from(start).unwrap_or(u32::MAX) });
    }
    out
}

fn is_word(t: Option<&Tok<'_>>) -> bool {
    t.is_some_and(|t| matches!(t.kind, TokenKind::Ident | TokenKind::RawIdent))
}

/// `t` when it is a word.
fn word<'a, 't>(t: Option<&'a Tok<'t>>) -> Option<&'a Tok<'t>> {
    t.filter(|w| matches!(w.kind, TokenKind::Ident | TokenKind::RawIdent))
}

fn word_is(t: Option<&Tok<'_>>, w: &str) -> bool {
    t.is_some_and(|t| t.kind == TokenKind::Ident && t.text == w)
}

fn kind_is(t: Option<&Tok<'_>>, k: TokenKind) -> bool {
    t.is_some_and(|t| t.kind == k)
}

/// Whether a `::` starts at `at`.
fn sep_at(t: &[Tok<'_>], at: usize) -> bool {
    kind_is(t.get(at), TokenKind::Colon) && kind_is(t.get(at + 1), TokenKind::Colon)
}

/// The index after the bracket group that opens at `at` (`t[at]` an open bracket); `None` when
/// it does not close.
fn group_end(t: &[Tok<'_>], at: usize) -> Option<usize> {
    let mut depth = 0u32;
    for (i, tok) in t.iter().enumerate().skip(at) {
        match tok.kind {
            TokenKind::OpenParen | TokenKind::OpenBracket | TokenKind::OpenBrace => depth += 1,
            TokenKind::CloseParen | TokenKind::CloseBracket | TokenKind::CloseBrace => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    return Some(i + 1);
                }
            }
            _ => {}
        }
    }
    None
}

/// The outer attributes at `at`: the index after them, and whether each is inert
/// (`nested::INERT_ATTRS`, a tool's); `None` for a `#` that does not start a whole `#[..]`.
fn skip_attrs(t: &[Tok<'_>], mut at: usize) -> Option<(usize, bool)> {
    let mut inert = true;
    while kind_is(t.get(at), TokenKind::Pound) {
        if !kind_is(t.get(at + 1), TokenKind::OpenBracket) {
            return None;
        }
        let end = group_end(t, at + 1)?;
        let first = word(t.get(at + 2))?;
        let mut segments = 1u32;
        let mut i = at + 3;
        while sep_at(t, i) && is_word(t.get(i + 2)) {
            segments += 1;
            i += 3;
        }
        inert &= inert_attr(first.text, segments);
        at = end;
    }
    Some((at, inert))
}

/// The index after a visibility at `at` (`pub`, `pub(crate)`, `pub(in a::b)`), or `at`.
fn skip_vis(t: &[Tok<'_>], at: usize) -> Option<usize> {
    if !word_is(t.get(at), "pub") {
        return Some(at);
    }
    if kind_is(t.get(at + 1), TokenKind::OpenParen) { group_end(t, at + 1) } else { Some(at + 1) }
}

/// A path at `at`: `::`, then `crate`, `self` or `super`s, then words joined by `::`. Returns
/// it and the index after it; `None` for a form that is no path read here.
fn path_at<'t>(t: &[Tok<'t>], mut at: usize) -> Option<(WrittenPath<'t>, usize)> {
    let mut path = WrittenPath::empty(PathKind::Plain);
    if sep_at(t, at) {
        path.kind = PathKind::Abs;
        at += 2;
    }
    loop {
        let tok = word(t.get(at))?;
        at += 1;
        let head = path.len == 0;
        match (tok.text, path.kind) {
            ("crate", PathKind::Plain) if head => path.kind = PathKind::Crate,
            ("self", PathKind::Plain) if head => path.kind = PathKind::SelfMod,
            ("super", PathKind::Plain) if head => path.kind = PathKind::Super(1),
            ("super", PathKind::Super(n)) if head => path.kind = PathKind::Super(n.checked_add(1)?),
            ("crate" | "self" | "super" | "Self", _) => return None,
            (w, _) => {
                if !path.push(w) {
                    return None;
                }
            }
        }
        if !sep_at(t, at) {
            return (path.len > 0).then_some((path, at));
        }
        at += 2;
    }
}

/// The path of `call`, a call [`crate::nested::scan`] listed in `text`, after the outer
/// attributes written before it, and whether those are all inert (`#[cfg(..)]`, `#[allow(..)]`:
/// an attribute macro on a statement call is not). `None` for a path not read here (a `$m`, a
/// path longer than [`MAX_SEGMENTS`], a `Self::m`).
pub fn call_path<'t>(text: &'t str, call: &NestedCall, alloc: Alloc<'_>) -> Option<(WrittenPath<'t>, bool)> {
    let own = text.get(call.range.start as usize..call.path.end as usize)?;
    let t = tokens(own, alloc);
    let (at, inert) = skip_attrs(&t, 0)?;
    let (path, end) = path_at(&t, at)?;
    (end == t.len()).then_some((path, inert))
}

/// What in a call's input may make the body it is in a surface body (D189, point 4): the
/// caller's tokens a transcriber may place as items. A superset: never `None` for an input
/// that holds one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InputMark<'t> {
    /// A token that begins an item (`nested::ITEM_WORDS`; `union` only before a name).
    ItemWord(&'t str),
    /// A `#`: an attribute (inert or not: a transcriber may place it on an item it writes).
    Attribute,
    /// A call in the input that `nested` did not prove safe, at its range in the text.
    Call(Range),
    /// A form not read: a `$`, or a call path not read here.
    Unread,
}

/// The first [`InputMark`] of `call`'s input (the text after its path through its range's
/// end), `None` when it holds none. Each nested call's path is handed to `nested` with its
/// range in `text`; `nested` answers whether the call is proven safe (its definition neither
/// emits surface nor binds at top). A nested call's own input is read in the same pass of the
/// tokens: an item word in it is the outer input's too.
pub fn input_mark<'t>(text: &'t str, call: &NestedCall, nested: &mut dyn FnMut(WrittenPath<'t>, Range) -> bool, alloc: Alloc<'_>) -> Option<InputMark<'t>> {
    let Some(own) = text.get(call.path.end as usize..call.range.end as usize) else { return Some(InputMark::Unread) };
    let base = call.path.end;
    let t = tokens(own, alloc);
    // `t[0]` is the call's `!`; its input starts after it.
    if !kind_is(t.first(), TokenKind::Bang) {
        return Some(InputMark::Unread);
    }
    let end_of = |e: usize| -> u32 { t.get(e.wrapping_sub(1)).map_or(u32::try_from(own.len()).unwrap_or(u32::MAX), |c| c.at.saturating_add(1)) };
    let mut i = 1usize;
    while let Some(tok) = t.get(i) {
        match tok.kind {
            TokenKind::Pound => return Some(InputMark::Attribute),
            TokenKind::Dollar => return Some(InputMark::Unread),
            TokenKind::Ident | TokenKind::RawIdent => {
                let w = tok.text;
                if w == "union" {
                    if is_word(t.get(i + 1)) {
                        return Some(InputMark::ItemWord(w));
                    }
                } else if ITEM_WORDS.contains(&w) {
                    return Some(InputMark::ItemWord(w));
                }
                // A path's first word (not after a `.`: a method or field is no macro's path):
                // read it whole, and a `!` and a bracket after it make it a call.
                let after_dot = i > 0 && kind_is(t.get(i - 1), TokenKind::Dot);
                let after_sep = i >= 2 && sep_at(&t, i - 2);
                if !after_dot && !after_sep && !not_a_macro(w) {
                    if let Some((path, end)) = path_at(&t, i)
                        && kind_is(t.get(end), TokenKind::Bang)
                        && t.get(end + 1).is_some_and(|b| matches!(b.kind, TokenKind::OpenParen | TokenKind::OpenBracket | TokenKind::OpenBrace))
                    {
                        let close = group_end(&t, end + 1).map_or(u32::try_from(own.len()).unwrap_or(u32::MAX), end_of);
                        let range = Range::new(base.saturating_add(tok.at), base.saturating_add(close));
                        if !nested(path, range) {
                            return Some(InputMark::Call(range));
                        }
                        // The input's own tokens are read on, in this pass.
                        i = end + 2;
                        continue;
                    }
                }
            }
            TokenKind::Colon if sep_at(&t, i) && !is_word(t.get(i.wrapping_sub(1))) => {
                // `::a::m!(..)`: an absolute path.
                if let Some((path, end)) = path_at(&t, i)
                    && kind_is(t.get(end), TokenKind::Bang)
                    && t.get(end + 1).is_some_and(|b| matches!(b.kind, TokenKind::OpenParen | TokenKind::OpenBracket | TokenKind::OpenBrace))
                {
                    let close = group_end(&t, end + 1).map_or(u32::try_from(own.len()).unwrap_or(u32::MAX), end_of);
                    let range = Range::new(base.saturating_add(tok.at), base.saturating_add(close));
                    if !nested(path, range) {
                        return Some(InputMark::Call(range));
                    }
                    i = end + 2;
                    continue;
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

/// One name a block `use` or `extern crate` binds: the path it binds and the name (`None`: a
/// glob, which binds every public name of its path's module, below the block's own names).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UseBinding<'t> {
    pub path: WrittenPath<'t>,
    pub binds: Option<&'t str>,
}

/// Each name the item statement `item` binds when it is a `use` or an `extern crate` (the
/// Reference, "Use declarations", "Extern crate declarations"), handed to `each`: `use a::b;`
/// binds `b` to `a::b`, `use a::b as c;` binds `c`, `use a::{self, d};` binds `a` and `d`,
/// `use a::*;` is a glob, `extern crate a as b;` binds `b` to `::a` and `extern crate self as
/// s;` binds `s` to `crate`. `false` when the item is not read whole (the caller then takes
/// any name to be bound).
pub fn use_bindings<'t>(item: &'t str, kind: NestedKind, alloc: Alloc<'_>, each: &mut dyn FnMut(UseBinding<'t>)) -> bool {
    let t = tokens(item, alloc);
    let Some((at, _)) = skip_attrs(&t, 0) else { return false };
    let Some(at) = skip_vis(&t, at) else { return false };
    match kind {
        NestedKind::Use => {
            if !word_is(t.get(at), "use") {
                return false;
            }
            let mut i = at + 1;
            tree(&t, &mut i, WrittenPath::empty(PathKind::Plain), each).is_some() && kind_is(t.get(i), TokenKind::Semi)
        }
        NestedKind::ExternCrate => {
            if !word_is(t.get(at), "extern") || !word_is(t.get(at + 1), "crate") {
                return false;
            }
            let Some(name) = word(t.get(at + 2)) else { return false };
            let (binds, end) = if word_is(t.get(at + 3), "as") {
                match word(t.get(at + 4)) {
                    Some(alias) => (alias.text, at + 5),
                    None => return false,
                }
            } else {
                (name.text, at + 3)
            };
            let path = if name.text == "self" {
                WrittenPath::empty(PathKind::Crate)
            } else {
                let mut p = WrittenPath::empty(PathKind::Abs);
                if !p.push(name.text) {
                    return false;
                }
                p
            };
            each(UseBinding { path, binds: Some(binds) });
            kind_is(t.get(end), TokenKind::Semi)
        }
        _ => false,
    }
}

/// How many use-tree groups deep [`tree`] reads (rule 18).
const MAX_TREE_DEPTH: usize = 16;

/// One use tree at `*at` under `prefix`.
fn tree<'t>(t: &[Tok<'t>], at: &mut usize, prefix: WrittenPath<'t>, each: &mut dyn FnMut(UseBinding<'t>)) -> Option<()> {
    let mut stack = [WrittenPath::empty(PathKind::Plain); MAX_TREE_DEPTH];
    let mut depth: usize = 0;
    let mut path = prefix;
    'child: loop {
        if path.len == 0 && path.kind == PathKind::Plain && sep_at(t, *at) {
            path.kind = PathKind::Abs;
            *at += 2;
        }
        loop {
            let tok = t.get(*at)?;
            match tok.kind {
                TokenKind::Star => {
                    *at += 1;
                    each(UseBinding { path, binds: None });
                    break;
                }
                TokenKind::OpenBrace => {
                    *at += 1;
                    if kind_is(t.get(*at), TokenKind::CloseBrace) {
                        *at += 1;
                        break;
                    }
                    let Some(slot) = stack.get_mut(depth) else {
                        return None;
                    };
                    *slot = path;
                    depth += 1;
                    continue 'child;
                }
                TokenKind::Ident | TokenKind::RawIdent => {
                    *at += 1;
                    let head = path.len == 0;
                    let mut last = None;
                    match (tok.text, path.kind) {
                        ("crate", PathKind::Plain) if head => path.kind = PathKind::Crate,
                        ("self", PathKind::Plain) if head && sep_at(t, *at) => path.kind = PathKind::SelfMod,
                        ("super", PathKind::Plain) if head => path.kind = PathKind::Super(1),
                        ("super", PathKind::Super(n)) if head => path.kind = PathKind::Super(n.checked_add(1)?),
                        // `a::{self}`: binds the prefix's own last name.
                        ("self", _) if !head => last = Some(*path.segments().last()?),
                        ("self" | "crate" | "super", _) => return None,
                        (w, _) => {
                            if !path.push(w) {
                                return None;
                            }
                            last = Some(w);
                        }
                    }
                    if last.is_none() || sep_at(t, *at) {
                        if !sep_at(t, *at) {
                            return None;
                        }
                        *at += 2;
                        continue;
                    }
                    let binds = if word_is(t.get(*at), "as") {
                        let alias = word(t.get(*at + 1))?;
                        *at += 2;
                        alias.text
                    } else {
                        last?
                    };
                    each(UseBinding { path, binds: Some(binds) });
                    break;
                }
                _ => return None,
            }
        }
        loop {
            if depth == 0 {
                return Some(());
            }
            if kind_is(t.get(*at), TokenKind::Comma) {
                *at += 1;
                if kind_is(t.get(*at), TokenKind::CloseBrace) {
                    *at += 1;
                    depth -= 1;
                    continue;
                }
                path = *stack.get(depth - 1)?;
                continue 'child;
            }
            if kind_is(t.get(*at), TokenKind::CloseBrace) {
                *at += 1;
                depth -= 1;
                continue;
            }
            return None;
        }
    }
}

/// The name a block item statement declares, after its attributes, visibility and qualifiers
/// (`unsafe`, `async`, `const fn`, `extern "C" fn`, ...): the first word after its kind's
/// keyword. `None` for a kind with no name (`impl`, an extern block, a `use`) or a text not read.
pub fn item_name<'t>(item: &'t str, kind: NestedKind, alloc: Alloc<'_>) -> Option<&'t str> {
    let keyword = match kind {
        NestedKind::Fn => "fn",
        NestedKind::Const => "const",
        NestedKind::Static => "static",
        NestedKind::Struct => "struct",
        NestedKind::Enum => "enum",
        NestedKind::Union => "union",
        NestedKind::Trait => "trait",
        NestedKind::TypeAlias => "type",
        NestedKind::Mod => "mod",
        NestedKind::Macro => "macro",
        NestedKind::MacroRules => return macro_rules_parts(item, alloc).map(|(name, _)| name),
        NestedKind::Impl | NestedKind::Use | NestedKind::ExternCrate | NestedKind::ExternBlock => return None,
    };
    let t = tokens(item, alloc);
    let (at, _) = skip_attrs(&t, 0)?;
    let at = skip_vis(&t, at)?;
    // The keyword is the first word that is the kind's own (`const fn f`: `fn`'s, then `f`).
    let mut i = at;
    while let Some(tok) = t.get(i) {
        i += 1;
        if tok.kind == TokenKind::Ident && tok.text == keyword {
            // `const fn`: the fn's keyword is the kind's when the kind is a fn.
            if kind == NestedKind::Const && word_is(t.get(i), "fn") {
                return None;
            }
            // `static mut X`.
            if word_is(t.get(i), "mut") {
                i += 1;
            }
            return word(t.get(i)).map(|w| w.text);
        }
        if !matches!(tok.kind, TokenKind::Ident | TokenKind::Literal { .. }) {
            return None;
        }
    }
    None
}

/// A `macro_rules!` item statement's name and its rules (the text from the bracket after the
/// name to the item's end), after its attributes; `None` for a text not of that form.
pub fn macro_rules_parts<'t>(item: &'t str, alloc: Alloc<'_>) -> Option<(&'t str, &'t str)> {
    let t = tokens(item, alloc);
    let (at, _) = skip_attrs(&t, 0)?;
    if !word_is(t.get(at), "macro_rules") || !kind_is(t.get(at + 1), TokenKind::Bang) {
        return None;
    }
    let name = word(t.get(at + 2))?;
    let open = t.get(at + 3).filter(|b| matches!(b.kind, TokenKind::OpenParen | TokenKind::OpenBracket | TokenKind::OpenBrace))?;
    Some((name.text, item.get(open.at as usize..)?))
}

#[cfg(test)]
mod tests {
    use std::vec::Vec;

    use blaze_surface::PathKind;

    use super::{InputMark, UseBinding, call_path, input_mark, item_name, macro_rules_parts, use_bindings};
    use crate::nested::{NestedKind, scan};

    fn alloc() -> blaze_own::Alloc<'static> {
        blaze_own::testing::heap().alloc()
    }

    type Row<'t> = (PathKind, Vec<&'t str>, Option<&'t str>);

    fn bindings(item: &str, kind: NestedKind) -> Option<Vec<Row<'_>>> {
        let mut out = Vec::new();
        let whole = use_bindings(item, kind, alloc(), &mut |b: UseBinding<'_>| out.push((b.path.kind, b.path.segments().iter().copied().collect(), b.binds)));
        whole.then_some(out)
    }

    fn row<'t>(kind: PathKind, segments: &[&'t str], binds: Option<&'t str>) -> Row<'t> {
        (kind, segments.iter().copied().collect(), binds)
    }

    /// Each form of a block `use` binds what the Reference says; a form not read is `None`
    /// (the twin: a trailing token after the tree).
    #[test]
    fn a_block_use_binds_its_names() {
        assert_eq!(bindings("use dep::emits_impl as format;", NestedKind::Use), Some(std::vec![row(PathKind::Plain, &["dep", "emits_impl"], Some("format"))]));
        assert_eq!(
            bindings("pub(crate) use ::a::{self, b::c as d, e::*};", NestedKind::Use),
            Some(std::vec![row(PathKind::Abs, &["a"], Some("a")), row(PathKind::Abs, &["a", "b", "c"], Some("d")), row(PathKind::Abs, &["a", "e"], None)])
        );
        assert_eq!(bindings("use super::super::m;", NestedKind::Use), Some(std::vec![row(PathKind::Super(2), &["m"], Some("m"))]));
        assert_eq!(bindings("extern crate serde as _serde;", NestedKind::ExternCrate), Some(std::vec![row(PathKind::Abs, &["serde"], Some("_serde"))]));
        assert_eq!(bindings("use a::b c;", NestedKind::Use), None, "the twin: a form not read");
    }

    /// A call's path after its attributes, and whether they are inert; its input's first mark,
    /// with a nested call proven safe read through and one not proven named by its range.
    #[test]
    fn a_call_s_path_and_input() {
        let text = "fn f() { #[cfg(x)] a::m!(1); n!(g!(2) + 3); p!(impl X for Y {}); q!(h!(4)); r!(x.union(y)); }";
        let heap = alloc();
        let nested = scan(text, heap);
        let calls: Vec<_> = nested.calls.iter().copied().collect();
        let (path, inert) = call_path(text, &calls[0], heap).expect("a path");
        assert_eq!((path.kind, path.segments()), (PathKind::Plain, &["a", "m"][..]));
        assert!(inert, "a cfg is inert");
        let safe = &mut |p: super::WrittenPath<'_>, _: blaze_surface::Range| p.segments() != ["h"];
        assert_eq!(input_mark(text, &calls[1], safe, heap), None, "g! is proven safe");
        assert_eq!(input_mark(text, &calls[2], safe, heap), Some(InputMark::ItemWord("impl")));
        assert!(matches!(input_mark(text, &calls[3], safe, heap), Some(InputMark::Call(_))), "h! is not proven");
        assert_eq!(input_mark(text, &calls[4], safe, heap), None, "a method named union is no item");
    }

    #[test]
    fn a_block_item_s_name() {
        let heap = alloc();
        assert_eq!(item_name("#[derive(Clone)] pub(crate) struct S { a: u8 }", NestedKind::Struct, heap), Some("S"));
        assert_eq!(item_name("pub const unsafe fn g() {}", NestedKind::Fn, heap), Some("g"));
        assert_eq!(item_name("static mut N: u8 = 0;", NestedKind::Static, heap), Some("N"));
        assert_eq!(macro_rules_parts("macro_rules! local { () => {} }", heap), Some(("local", "{ () => {} }")));
    }
}
