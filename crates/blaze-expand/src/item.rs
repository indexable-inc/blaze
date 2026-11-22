//! Item position, the one home of what an item's outer attributes ask the expander for, in the
//! Reference's order ("Attributes", "Procedural macros"; rustc's `take_first_attr`): each outer
//! attribute in turn, a `#[cfg_attr(..)]` replaced in place by what it leaves under the unit's
//! cfg set and read again, a `#[cfg(..)]` removed when true and the item gone when false; the
//! first attribute whose path names an attribute macro takes the item without that attribute
//! ([`ItemAttrs::attr_call`]), later attributes as written; when none does, the derives in
//! order, each on the item as a derive sees it ([`ItemAttrs::derives`]).
//!
//! Which path names an attribute macro, and which definition a derive path names, is the
//! caller's to answer (it resolves names); everything else is here, for every caller: the names
//! library's fill and blaze-lower's `lower_file`. A cfg atom is asked of the caller's `atom`
//! (`Err` when the cfg set cannot answer it yet), each attribute read once, no recursion.

use core::{fmt, ops::Range};

use blaze_own::{AString, AVec, Alloc, String, Vec};
use parser::SyntaxKind;

use crate::{
    call::{AlsoCopy, Constness, DeriveFacts},
    cfg::{CfgAttrError, CfgPredicateError, cfg_attr_metas, cfg_predicate_at},
    derive::{attr_call, derive_meta},
    error::StreamError,
    extent::ExtentError,
    tokens::{Delim, TokenKind, TokenStream, index},
};

/// Why an item's attributes leave no answer.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ItemAttrsError<E> {
    /// A stream the walk builds passed its tables' limits.
    Stream(StreamError),
    /// The `#[cfg_attr(..)]` at this token of the item (as it stood then) has no answer.
    CfgAttr { token: u32, error: CfgAttrError<E> },
    /// The `#[cfg(..)]` at this token of the item (as it stood then) has no answer.
    Cfg { token: u32, error: CfgPredicateError<E> },
    /// The item a derive sees does not parse: the element and the token it stops at
    /// (extent.rs, the one reading of an element's extent).
    Syntax(ExtentError),
    /// An inner `#![cfg]` or `#![cfg_attr]` at this token, inside the item a derive sees:
    /// refused, not applied (what it would remove is the block around it, which rustc refuses
    /// in most positions too).
    InnerCfg { token: u32 },
}

impl<E> From<StreamError> for ItemAttrsError<E> {
    fn from(error: StreamError) -> Self {
        ItemAttrsError::Stream(error)
    }
}

impl<E: fmt::Debug> fmt::Display for ItemAttrsError<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ItemAttrsError::Stream(e) => write!(f, "an item's attributes: {e}"),
            ItemAttrsError::CfgAttr { token, error } => write!(f, "the `#[cfg_attr]` at token {token} of an item: {error:?}"),
            ItemAttrsError::Cfg { token, error } => write!(f, "the `#[cfg]` at token {token} of an item: {error}"),
            ItemAttrsError::Syntax(error) => write!(f, "the item a derive sees: {error}"),
            ItemAttrsError::InnerCfg { token } => {
                write!(f, "an inner `#![cfg]` or `#![cfg_attr]` at token {token} of the item a derive sees: not applied, refused")
            }
        }
    }
}

/// The next outer attribute of an item that may be an attribute macro.
#[derive(Debug)]
pub enum NextAttr<'a> {
    /// `#[path]` or `#[path(args)]` (any delimiter), the attribute the tokens `attr` of
    /// [`ItemAttrs::item`]: an attribute macro when `path` (as written, `::` kept, no spaces)
    /// resolves to one; then [`ItemAttrs::attr_call`] is its call.
    Call { path: AString<'a>, attr: Range<u32> },
    /// A `#[cfg(..)]` of the item is false: the item is gone, nothing of it expands.
    Removed,
    /// No outer attribute is left.
    End,
}

/// An attribute macro's call: its arguments (inside their delimiters) and the item without
/// the attribute and without the derives written before it, for [`crate::Call::attr`].
pub struct AttrCall<'a> {
    pub args: Option<TokenStream<'a>>,
    pub item: TokenStream<'a>,
    /// How many derive paths the `#[derive(..)]`/`#[derive_const(..)]`s cut from `item` hold:
    /// they are the first `derives_taken` of [`ItemAttrs::derives`]' uses on the same item, in
    /// order, and run before the macro (its caller runs them; the macro never sees them).
    pub derives_taken: usize,
}

/// One path of a `#[derive(..)]` or `#[derive_const(..)]`, in order.
#[derive(Debug)]
pub struct DeriveUse<'a> {
    /// As written, `::` kept, no spaces.
    pub path: AString<'a>,
    /// The path's tokens: what a call of this derive is found by ([`crate::same_path`]).
    pub tokens: TokenStream<'a>,
    pub constness: Constness,
}

/// An item's derives and the item as each sees it, for [`crate::Call::derive`].
pub struct Derives<'a> {
    pub uses: AVec<'a, DeriveUse<'a>>,
    /// The item as rustc hands it to a derive: every `#[derive]` gone, each `#[cfg_attr]`
    /// replaced by what it leaves, each `#[cfg]` gone, and a field or variant whose `#[cfg]` is
    /// false gone with its comma.
    pub input: TokenStream<'a>,
    copy: AlsoCopy,
}

impl Derives<'_> {
    /// The facts of the derive `u`: whether the item also derives `Copy`, and its constness.
    pub fn facts(&self, u: &DeriveUse<'_>) -> DeriveFacts {
        DeriveFacts { copy: self.copy, constness: u.constness }
    }
}

/// An item's tokens, walked attribute by attribute.
pub struct ItemAttrs<'a> {
    item: TokenStream<'a>,
    /// The token the next outer attribute starts at.
    at: usize,
}

impl<'a> ItemAttrs<'a> {
    /// The walk of `item`: an item's tokens from its first outer attribute.
    pub fn new(item: TokenStream<'a>) -> ItemAttrs<'a> {
        ItemAttrs { item, at: 0 }
    }

    /// The item as the walk has left it: the `#[cfg_attr]`s met applied, the true `#[cfg]`s
    /// gone.
    pub fn item(&self) -> &TokenStream<'a> {
        &self.item
    }

    /// The next outer attribute that may be an attribute macro, each `#[cfg_attr]` and
    /// `#[cfg]` before it applied. Derive attributes and `#[name = value]` ones (doc comments)
    /// are passed over.
    pub fn next<E>(&mut self, atom: &mut dyn FnMut(&str, Option<&str>) -> Result<bool, E>) -> Result<NextAttr<'a>, ItemAttrsError<E>> {
        let alloc = self.item.allocator();
        loop {
            let at = self.at;
            let Some(close) = outer_attr(&self.item, at) else { return Ok(NextAttr::End) };
            let meta = meta_at(&self.item, at + 1, close);
            match meta_name(&self.item, &meta) {
                "cfg_attr" => {
                    let attr = attr_copy(&self.item, at, close, meta)?;
                    let metas = cfg_attr_metas(&attr, alloc, atom).map_err(|error| ItemAttrsError::CfgAttr { token: index(at), error })?;
                    self.item = splice(&self.item, at..close + 1, &attr, &metas)?;
                }
                "cfg" => {
                    if !cfg_holds(&self.item, at, meta, alloc, atom)? {
                        return Ok(NextAttr::Removed);
                    }
                    self.item = splice(&self.item, at..close + 1, &self.item, &[])?;
                }
                _ => {
                    self.at = close + 1;
                    let attr = attr_copy(&self.item, at, close, meta)?;
                    if derive_meta(&attr, 2..index(attr.len() - 1), alloc).is_some() {
                        continue;
                    }
                    if let Some((path, _)) = attr_call(&attr) {
                        // The Reference, "Conditional compilation": `cfg_attr` expands to its
                        // attributes; an attribute macro receives the item with the remaining
                        // attributes, so each later `cfg_attr` is applied before it sees them
                        // (icu_collections: `#[zerovec::make_varule(..)] .. #[cfg_attr(p,
                        // zerovec::skip_derive(..))]`, a helper the macro consumes only as such).
                        self.apply_later_cfg_attrs(close + 1, atom)?;
                        return Ok(NextAttr::Call { path: path_text(&attr, path, alloc), attr: index(at)..index(close + 1) });
                    }
                }
            }
        }
    }

    /// The call of an attribute macro that no attribute in the item names (D104: a blaze
    /// edition binary's root `fn main` is `std::main`'s input unasked, blaze-scope
    /// `IMPLICIT_MAIN`). It stands outermost, before every written attribute, so, as for a
    /// written first attribute ([`ItemAttrs::next`]), each `#[cfg_attr]` is applied before the
    /// macro sees the item, and every other attribute (a `#[cfg]`, a doc, `#[inline]`) stays in
    /// it. No arguments, and no derive is taken (a derive on a fn is refused by the macro's own
    /// expansion, as written).
    pub fn implicit_call<E>(mut self, atom: &mut dyn FnMut(&str, Option<&str>) -> Result<bool, E>) -> Result<AttrCall<'a>, ItemAttrsError<E>> {
        self.apply_later_cfg_attrs(0, atom)?;
        Ok(AttrCall { args: None, item: self.item, derives_taken: 0 })
    }

    /// Each `#[cfg_attr(..)]` among the outer attributes from token `from` on replaced in place
    /// by what it leaves (nested ones applied by [`cfg_attr_metas`]); a loop, read once each.
    fn apply_later_cfg_attrs<E>(&mut self, from: usize, atom: &mut dyn FnMut(&str, Option<&str>) -> Result<bool, E>) -> Result<(), ItemAttrsError<E>> {
        let alloc = self.item.allocator();
        let mut at = from;
        while let Some(close) = outer_attr(&self.item, at) {
            let meta = meta_at(&self.item, at + 1, close);
            if meta_name(&self.item, &meta) != "cfg_attr" {
                at = close + 1;
                continue;
            }
            let attr = attr_copy(&self.item, at, close, meta)?;
            let metas = cfg_attr_metas(&attr, alloc, atom).map_err(|error| ItemAttrsError::CfgAttr { token: index(at), error })?;
            self.item = splice(&self.item, at..close + 1, &attr, &metas)?;
            // `at` now starts the first attribute it left (none: the next one); none of them is
            // a `cfg_attr`, so each is passed over once.
        }
        Ok(())
    }

    /// The call of the attribute macro at `attr` (a [`NextAttr::Call`]'s): its arguments, and
    /// the item without it and without each `#[derive(..)]`/`#[derive_const(..)]` written
    /// before it; every other attribute before it (a derive's helper, `#[inline]`, a doc) and
    /// everything after it stay as written. The Reference, "Attributes": "active attributes
    /// remove themselves from the form they are on while inert attributes stay on"; "Attribute
    /// macros": "The second TokenStream is the rest of the item, including other attributes on
    /// the item"; "Derive macro helper attributes": a helper is in scope "for attributes that
    /// are applied to that item and are applied lexically after the derive macro invocation".
    /// A derive written first is expanded first (rustc_expand `take_first_attr`), so it is not
    /// in the macro's input; [`AttrCall::derives_taken`] counts its paths for the caller.
    pub fn attr_call(&self, attr: Range<u32>) -> Result<AttrCall<'a>, StreamError> {
        let alloc = self.item.allocator();
        let (start, end) = (attr.start as usize, attr.end as usize);
        let tokens = attr_copy(&self.item, start, end - 1, meta_at(&self.item, start + 1, end - 1))?;
        let args = match attr_call(&tokens).and_then(|(_, args)| args) {
            Some(r) => Some(tokens.copy_in(r.start as usize..r.end as usize, alloc)?),
            None => None,
        };
        let mut item = TokenStream::new_in(alloc);
        let mut derives_taken = 0;
        let mut at = 0;
        // The outer attributes before the call, each read once: a derive is cut, the rest kept.
        while let Some(close) = outer_attr(&self.item, at).filter(|&close| close < start) {
            let written = attr_copy(&self.item, at, close, meta_at(&self.item, at + 1, close))?;
            match derive_meta(&written, 2..index(written.len() - 1), alloc) {
                Some((_, paths)) => derives_taken += paths.len(),
                None => {
                    for t in at..close + 1 {
                        item.push_from(&self.item, t)?;
                    }
                }
            }
            at = close + 1;
        }
        for t in (at..start).chain(end..self.item.len()) {
            item.push_from(&self.item, t)?;
        }
        Ok(AttrCall { args, item, derives_taken })
    }

    /// Whether the attribute `attr` (a [`NextAttr::Call`]'s, tokens of [`ItemAttrs::item`]) is
    /// written with the path `path` (a stream holding one path): equal word by word
    /// ([`crate::same_path`]), so an item of a file's text and one an expansion wrote are found
    /// alike, a `$crate` head held as one token, as a `$` then `crate`, or rendered `crate`.
    pub fn attr_is(&self, attr: Range<u32>, path: &TokenStream<'_>) -> bool {
        // `#`, `[`, then the path (inside the invisible groups that are all of a `$m:meta`).
        let meta = meta_at(&self.item, attr.start as usize + 1, attr.end as usize - 1);
        let start = meta.start;
        match crate::attr_path::path_end(&self.item, start) {
            Some(end) if end <= meta.end => crate::attr_path::same_path(&self.item, start..end, path, 0..path.len()),
            _ => false,
        }
    }

    /// When no attribute macro took the item: the rest of its outer attributes applied, then its
    /// derives in order and the item as they see it. `None` when a `#[cfg]` of the item is
    /// false.
    pub fn derives<E>(mut self, atom: &mut dyn FnMut(&str, Option<&str>) -> Result<bool, E>) -> Result<Option<Derives<'a>>, ItemAttrsError<E>> {
        loop {
            match self.next(atom)? {
                NextAttr::End => break,
                NextAttr::Removed => return Ok(None),
                NextAttr::Call { .. } => {}
            }
        }
        let alloc = self.item.allocator();
        let mut uses = Vec::new_in(alloc);
        let mut at = 0;
        while let Some(close) = outer_attr(&self.item, at) {
            let attr = attr_copy(&self.item, at, close, meta_at(&self.item, at + 1, close))?;
            if let Some((constness, paths)) = derive_meta(&attr, 2..index(attr.len() - 1), alloc) {
                for p in paths.iter() {
                    let tokens = attr.copy_in(p.start as usize..p.end as usize, alloc)?;
                    uses.push(DeriveUse { path: path_text(&attr, p.clone(), alloc), tokens, constness });
                }
            }
            at = close + 1;
        }
        let copy = match uses.iter().any(|u| u.path.rsplit(':').next() == Some("Copy")) {
            true => AlsoCopy::Yes,
            false => AlsoCopy::No,
        };
        let Some(input) = derive_input(&self.item, atom)? else { return Ok(None) };
        Ok(Some(Derives { uses, input, copy }))
    }
}

/// The `]` of the attribute `#[..]` at `at` of `item`, when one starts there.
fn outer_attr(item: &TokenStream<'_>, at: usize) -> Option<usize> {
    let list = item.tokens();
    let hash = matches!(list.get(at).map(|t| t.kind), Some(TokenKind::Punct { char: '#', .. }));
    let bracket = matches!(list.get(at + 1).map(|t| t.kind), Some(TokenKind::Open(Delim::Bracket)));
    if hash && bracket { item.close_of(at + 1) } else { None }
}

/// The meta of the attribute whose `[` is at `open` and `]` at `close` (`cfg(x)` of
/// `#[cfg(x)]`): its tokens without the invisible groups that are all of it, as `#[$attr]`
/// writes a `$attr:meta` fragment (libc's `s!`, pin-project-lite).
fn meta_at(item: &TokenStream<'_>, open: usize, close: usize) -> Range<usize> {
    let mut meta = open + 1..close;
    while meta.len() >= 2
        && item.tokens()[meta.start].kind == TokenKind::Open(Delim::Invisible)
        && item.close_of(meta.start) == Some(meta.end - 1)
    {
        meta = meta.start + 1..meta.end - 1;
    }
    meta
}

/// Whether the `#[cfg(pred)]` at `at` holds, `meta` its `cfg(pred)`.
fn cfg_holds<E>(
    item: &TokenStream<'_>,
    at: usize,
    meta: Range<usize>,
    alloc: Alloc<'_>,
    atom: &mut dyn FnMut(&str, Option<&str>) -> Result<bool, E>,
) -> Result<bool, ItemAttrsError<E>> {
    let open = meta.start + 1;
    let inside = match item.close_of(open) {
        Some(end) if end < meta.end && matches!(item.tokens()[open].kind, TokenKind::Open(Delim::Paren)) => open + 1..end,
        _ => open..open,
    };
    cfg_predicate_at(item, inside, alloc, atom).map_err(|error| ItemAttrsError::Cfg { token: index(at), error })
}

/// The first name of the meta `meta` (`cfg` of `cfg(..)`), or `""`.
fn meta_name<'s>(item: &'s TokenStream<'_>, meta: &Range<usize>) -> &'s str {
    match item.tokens().get(meta.start).map(|t| t.kind) {
        Some(TokenKind::Ident { .. }) if meta.start < meta.end => item.text_at(meta.start),
        _ => "",
    }
}

/// The attribute `#[meta]` at `at`..=`close` as a stream of its own, `meta` its meta's
/// tokens: the item's tokens copied, the invisible groups that were all of the meta (a
/// `$attr:meta` fragment's) left out, so cfg.rs reads `#[cfg_attr(..)]` as written.
fn attr_copy<'a>(item: &TokenStream<'a>, at: usize, close: usize, meta: Range<usize>) -> Result<TokenStream<'a>, StreamError> {
    let alloc = item.allocator();
    if meta.start == at + 2 && meta.end == close {
        return item.copy_in(at..close + 1, alloc);
    }
    let mut out = TokenStream::new_in(alloc);
    out.push_from(item, at)?;
    out.push_from(item, at + 1)?;
    for t in meta {
        out.push_from(item, t)?;
    }
    out.push_from(item, close)?;
    Ok(out)
}

/// A path's tokens as written: `::` kept, no spaces (a `$` then `crate` as `$crate`).
fn path_text<'a>(attr: &TokenStream<'_>, path: Range<u32>, alloc: Alloc<'a>) -> AString<'a> {
    let mut out = String::new_in(alloc);
    for at in path {
        match attr.tokens()[at as usize].kind {
            TokenKind::Punct { char, .. } => out.push(char),
            _ => out.push_str(attr.text_at(at as usize)),
        }
    }
    out
}

/// `item` with the tokens `cut` replaced by `#[meta]` for each meta range of `attr` (an
/// attribute's tokens, from its `#`).
fn splice<'a>(item: &TokenStream<'a>, cut: Range<usize>, attr: &TokenStream<'_>, metas: &[Range<u32>]) -> Result<TokenStream<'a>, StreamError> {
    let mut out = TokenStream::new_in(item.allocator());
    for t in 0..cut.start {
        out.push_from(item, t)?;
    }
    for meta in metas {
        out.push_from(attr, 0)?;
        out.push_from(attr, 1)?;
        for m in meta.clone() {
            out.push_from(attr, m as usize)?;
        }
        out.push_from(attr, attr.len() - 1)?;
    }
    for t in cut.end..item.len() {
        out.push_from(item, t)?;
    }
    Ok(out)
}

/// One change [`derive_input`] makes to the item: the tokens `cut` go, and in their place, when
/// `with` holds what a `#[cfg_attr]` leaves (its tokens from the `#`, and each meta it leaves
/// as a range of them), each meta written as `#[meta]`.
struct Edit<'a> {
    cut: Range<usize>,
    with: Option<(TokenStream<'a>, AVec<'a, Range<u32>>)>,
}

/// The item a derive sees ([`Derives::input`]), from `item` with its own outer attributes
/// applied; `None` when a false `cfg` of the item itself removes it.
///
/// The Reference, "Conditional compilation": a false `cfg` removes the thing it is on, and a
/// derive's input is the item after `cfg` and `cfg_attr` are applied (rustc's `cfg_eval`, every
/// node of it). The item is parsed once (extent.rs) and an attribute's element is the parser's
/// node it is an attribute of: a field, a variant, a generic parameter, a statement, an arm,
/// an expression. A false `cfg` removes that node whole, with the comma that ends it and WITH
/// EVERY ATTRIBUTE IT CARRIES, the ones written before the `cfg` too
/// (`#[command(flatten)] #[cfg(feature = "x")] f: F,`; a doc comment is a `#[doc]` of the same
/// field). Each attribute is read once, in order, and nothing of an element after its first
/// false `cfg` is asked. No comma or `<`/`>` is counted here: reth-ethereum-forks'
/// `ChainHardforks` (`map: BTreeMap<&'static str, ForkCondition>,` under a false `cfg`,
/// tempo-chainspec t1545) had a walk that did cut the field at its type's comma.
fn derive_input<'a, E>(
    item: &TokenStream<'a>,
    atom: &mut dyn FnMut(&str, Option<&str>) -> Result<bool, E>,
) -> Result<Option<TokenStream<'a>>, ItemAttrsError<E>> {
    let alloc = item.allocator();
    let tree = crate::extent::item(item, alloc).map_err(ItemAttrsError::Syntax)?;
    let list = item.tokens();
    let mut edits: AVec<'a, Edit<'a>> = Vec::new_in(alloc);
    let mut node = 0;
    while node < tree.len() {
        if tree.kind(node) != SyntaxKind::ATTR {
            node += 1;
            continue;
        }
        let this = node;
        let attr = tree.range(this);
        // From its `#` to its `]`: the range is the whole node, with an invisible group that
        // is all of the attribute.
        let at = tree.token(this);
        let mut close = attr.end - 1;
        while close > at && list[close].kind == TokenKind::Close(Delim::Invisible) {
            close -= 1;
        }
        node = tree.past(this);
        let meta = crate::extent::meta_of(item, &tree, this);
        if matches!(list.get(at + 1).map(|t| t.kind), Some(TokenKind::Punct { char: '!', .. })) {
            // An inner attribute (`{ #![cfg(x)] .. }` in a default's block).
            if matches!(meta_name(item, &meta), "cfg" | "cfg_attr") {
                return Err(ItemAttrsError::InnerCfg { token: index(at) });
            }
            continue;
        }
        // A derive by any path (libc writes `::core::prelude::v1::derive(..)`) goes.
        if derive_meta(item, index(meta.start)..index(meta.end), alloc).is_some() {
            edits.push(Edit { cut: attr, with: None });
            continue;
        }
        let owner_goes = match meta_name(item, &meta) {
            "cfg" => {
                let holds = cfg_holds(item, at, meta, alloc, atom)?;
                if holds {
                    edits.push(Edit { cut: attr, with: None });
                }
                !holds
            }
            "cfg_attr" => {
                let copy = attr_copy(item, at, close, meta)?;
                let metas = cfg_attr_metas(&copy, alloc, atom).map_err(|error| ItemAttrsError::CfgAttr { token: index(at), error })?;
                // What it leaves, with any derive it leaves gone and a `cfg` it leaves applied.
                let mut kept: AVec<'a, Range<u32>> = Vec::new_in(alloc);
                let mut removed = false;
                for meta in metas.iter() {
                    if derive_meta(&copy, meta.clone(), alloc).is_some() {
                        continue;
                    }
                    let is_cfg = matches!(copy.tokens()[meta.start as usize].kind, TokenKind::Ident { .. })
                        && copy.text_at(meta.start as usize) == "cfg"
                        && meta.end - meta.start >= 2;
                    if is_cfg {
                        let inside = meta.start as usize + 2..meta.end as usize - 1;
                        removed |= !cfg_predicate_at(&copy, inside, alloc, atom).map_err(|error| ItemAttrsError::Cfg { token: index(at), error })?;
                        if removed {
                            break;
                        }
                        continue;
                    }
                    kept.push(meta.clone());
                }
                if !removed {
                    edits.push(Edit { cut: attr, with: Some((copy, kept)) });
                }
                removed
            }
            _ => false,
        };
        if owner_goes {
            // The element the attribute is on; the item's own (node 0) removes the item.
            let owner = match tree.parent(this) {
                None | Some(0) => return Ok(None),
                Some(owner) => owner,
            };
            let cut = tree.removal(owner);
            // What was changed inside it goes with it: its attributes before this one too.
            edits.retain(|e| e.cut.start < cut.start);
            edits.push(Edit { cut, with: None });
            node = node.max(tree.past(owner));
        }
    }
    let mut out = TokenStream::new_in(alloc);
    let mut at = 0;
    for edit in edits.iter() {
        for t in at..edit.cut.start {
            out.push_from(item, t)?;
        }
        if let Some((attr, metas)) = &edit.with {
            for meta in metas.iter() {
                out.push_from(attr, 0)?;
                out.push_from(attr, 1)?;
                for m in meta.clone() {
                    out.push_from(attr, m as usize)?;
                }
                out.push_from(attr, attr.len() - 1)?;
            }
        }
        at = edit.cut.end;
    }
    for t in at..item.len() {
        out.push_from(item, t)?;
    }
    Ok(Some(out))
}
