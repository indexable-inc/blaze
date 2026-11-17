//! An `extern` block declared inside a body: the functions and statics it declares.
//!
//! The Reference, "External blocks": [items.extern.intro] "External blocks provide
//! declarations of items that are not defined in the current crate and are the basis of Rust's
//! foreign function interface." [items.extern.namespace] "The external block defines its
//! functions and statics in the value namespace of the module or block where it is located."
//! [items.extern.fn.body] "Functions within external blocks are declared in the same way as
//! other Rust functions, with the exception that they must not have a body and are instead
//! terminated by a semicolon." [items.extern.fn.safety] "A function declared in an extern block
//! is implicitly unsafe unless the safe function qualifier is present." [items.extern.static]
//! "Statics within external blocks are declared in the same way as statics outside of external
//! blocks, except that they do not have an expression initializing their value."
//! "Statements", "Item declarations": "An item declaration statement has a syntactic form
//! identical to an item declaration within a module."
//!
//! The pinned core's `panic_fmt` (library/core/src/panicking.rs) is the case that asks:
//! `unsafe extern "Rust" { #[lang = "panic_impl"] fn panic_impl(pi: &PanicInfo<'_>) -> !; }`
//! written in its body, then `unsafe { panic_impl(&pi) }`.
//!
//! What is here: the block's text read by the ONE outline every module-level `extern` block is
//! read by (`blaze_file_items::outline_item`): its ABI as written and, per declared fn or
//! static, the name, the text (attributes included, as a module-level foreign item's text is)
//! and whether `safe` is written. [`super::locals`]'s `Locals::keep` files each as a row of its
//! block, of kind fn or static, so the body's path lookup and the store's head (`Store::step`,
//! from the text) are those of any local fn or static: no second path.

use blaze_expand::{CfgAttrError, CfgPredicateError, Delim, TokenKind};
use blaze_lower::{Cfg, CfgAnswer};
use blaze_own::{AVec, Alloc};
use blaze_surface::{AttrKind, Edition, ItemDetail, Range};

/// What a foreign item of a block is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ForeignKind {
    Fn,
    Static,
}

/// One fn or static an `extern` block declares, as ranges and slices of the block's text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Declared<'t> {
    pub kind: ForeignKind,
    /// Its name, without `r#`.
    pub name: &'t str,
    /// Its text: from its first outer attribute or doc comment to its `;`.
    pub text: &'t str,
    /// Where `text` is in the block's text.
    pub at: Range,
    /// The `safe` qualifier is written ("implicitly unsafe unless the safe function qualifier
    /// is present").
    pub safe: bool,
}

/// What a local foreign item's head and the code builder ask beyond its text: its block's ABI
/// and its safety.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LocalForeign<'n> {
    pub kind: ForeignKind,
    /// The block's ABI string as written, quotes included (`"Rust"`); `"C"` for a bare
    /// `extern` (the Reference, "External blocks", "ABI": "By default external blocks assume
    /// that the library they are calling uses the standard C ABI").
    pub abi: &'n str,
    pub safe: bool,
}

/// Why an `extern` block in a body was not read: each a form refused by name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refused {
    /// The outline reports a syntax error in the block's text.
    Syntax,
    /// The text is not exactly one `extern` block.
    NotOneBlock,
    /// A range of the outline lies outside the block's text.
    Range,
    /// A `#[cfg]` or `#[cfg_attr]` on a declaration whose tokens are no predicate (or no
    /// `cfg(..)` / `cfg_attr(..)` of one).
    CfgForm,
    /// A `#[cfg]` or `#[cfg_attr]` on a declaration asks an atom the crate's cfg set does not
    /// answer yet.
    CfgAtom,
    /// A comment between a declaration's qualifiers and its name: `safe` is read from the
    /// words there.
    Comment,
    /// A declaration with no name.
    Unnamed,
    /// A macro call in the block: what it declares is its expansion's.
    MacroCall,
    /// Anything else an `extern` block may hold (a foreign type).
    OtherItem,
}

impl Refused {
    /// The refusal's words, for `Why::Unsupported`.
    pub fn form(self) -> &'static str {
        match self {
            Refused::Syntax => "an extern block in a block whose text has a syntax error",
            Refused::NotOneBlock => "an extern block in a block whose text is not one extern block",
            Refused::Range => "an extern block in a block whose outline names a range outside its text",
            Refused::CfgForm => "a `cfg` or `cfg_attr` attribute that is no predicate, on a declaration of an extern block in a block",
            Refused::CfgAtom => "a `cfg` or `cfg_attr` attribute asking a cfg atom the crate's set does not answer, on a declaration of an extern block in a block",
            Refused::Comment => "a comment between the qualifiers and the name of a declaration of an extern block in a block",
            Refused::Unnamed => "a declaration without a name in an extern block in a block",
            Refused::MacroCall => "a macro call in an extern block in a block",
            Refused::OtherItem => "a declaration that is neither a fn nor a static in an extern block in a block",
        }
    }
}

impl core::fmt::Display for Refused {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.form())
    }
}

/// The ABI string of a bare `extern { .. }`.
const C_ABI: &str = "\"C\"";

fn slice(text: &str, r: Range) -> Result<&str, Refused> {
    text.get(r.start as usize..r.end as usize).ok_or(Refused::Range)
}

/// The ABI (as written, quotes included) and the declared fns and statics, in the order
/// written, of `block`: the whole text of one `extern` block item of `edition`, under the
/// crate's `cfg`. The Reference, "Conditional compilation": [cfg.attr.effect] "If any
/// predicate is false, the form is removed from the source code"; [cfg.cfg_attr.behavior]
/// "When the configuration predicate is true, cfg_attr expands out to the attributes listed
/// after the predicate." So a declaration a `#[cfg]` is false for, or a `#[cfg_attr]` expands
/// to a false `cfg` for, is not declared; the other attributes a `cfg_attr` leaves
/// (`link_name`, as the pinned library's `core::arch` writes it) stay in the declaration's
/// text for its signature's lowering, which applies them under the same cfg
/// (`blaze_lower::lower_signature`).
pub fn declared<'t, 'a>(block: &'t str, edition: Edition, cfg: &mut dyn Cfg, alloc: Alloc<'a>) -> Result<(&'t str, AVec<'a, Declared<'t>>), Refused> {
    let outline = blaze_file_items::outline_item(block, edition, alloc);
    if !outline.errors().is_empty() {
        return Err(Refused::Syntax);
    }
    let mut top = outline.children(None);
    let (Some((at, item)), None) = (top.next(), top.next()) else { return Err(Refused::NotOneBlock) };
    let ItemDetail::ExternBlock { abi } = item.detail else { return Err(Refused::NotOneBlock) };
    let abi = match abi {
        None => C_ABI,
        Some(r) => {
            let written = slice(block, Range::new(item.range.start + r.start, item.range.start + r.end))?.trim();
            written.strip_prefix("extern").unwrap_or(written).trim()
        }
    };
    let mut out = AVec::new_in(alloc);
    for (_, child) in outline.children(Some(at)) {
        // A form the reading refuses is refused only when the cfg keeps it ([cfg.attr.effect]:
        // a removed form is not there to refuse).
        let kind = match child.detail {
            ItemDetail::Fn { .. } => Ok(ForeignKind::Fn),
            ItemDetail::Static { .. } => Ok(ForeignKind::Static),
            ItemDetail::MacroCall { .. } => Err(Refused::MacroCall),
            _ => Err(Refused::OtherItem),
        };
        let text = slice(block, child.range)?;
        // Where its attributes and doc comments end: its qualifiers start there.
        let mut head = 0u32;
        // Whether every `cfg` on it (written, or left by a `cfg_attr`) holds; the first false
        // one removes it and the rest are not asked, as `all` asks no more after a false one.
        let mut kept = true;
        for a in outline.attrs(child) {
            if kept && !a.inner && matches!(a.kind, AttrKind::Cfg | AttrKind::CfgAttr) {
                kept = attr_keeps(slice(text, a.range)?, a.kind, edition, cfg, alloc)?;
            }
            if !a.inner {
                head = head.max(a.range.end);
            }
        }
        if !kept {
            continue;
        }
        let kind = kind?;
        let name = child.name.ok_or(Refused::Unnamed)?;
        if name.start < head {
            return Err(Refused::Range);
        }
        let quals = slice(text, Range::new(head, name.start))?;
        if quals.contains("//") || quals.contains("/*") {
            return Err(Refused::Comment);
        }
        let safe = quals.split_whitespace().any(|w| w == "safe");
        let name = slice(text, name)?;
        let name = name.strip_prefix("r#").unwrap_or(name);
        out.push(Declared { kind, name, text, at: child.range, safe });
    }
    Ok((abi, out))
}

/// Whether the declaration the outer attribute `attr` (its text from the `#`, of kind `kind`:
/// `Cfg` or `CfgAttr`) is written on stays under `cfg`: a `#[cfg(p)]` keeps it when `p` holds;
/// a `#[cfg_attr(p, ..)]` keeps it unless `p` holds and one of the attributes it expands to
/// (nested `cfg_attr`s applied: blaze-expand's `cfg_attr_metas`) is a `cfg(q)` with `q`
/// false. An atom the set does not answer, or tokens that are no such attribute, are refused
/// by name. The predicate's reading is blaze-expand's (`cfg_predicate_at`), the one every
/// `#[cfg]` and `cfg!` is read by.
fn attr_keeps(attr: &str, kind: AttrKind, edition: Edition, cfg: &mut dyn Cfg, alloc: Alloc<'_>) -> Result<bool, Refused> {
    let tokens = blaze_expand::lex(attr, edition, alloc).map_err(|_| Refused::CfgForm)?;
    let mut atom = |name: &str, value: Option<&str>| match cfg.atom(name, value) {
        CfgAnswer::Yes => Ok(true),
        CfgAnswer::No => Ok(false),
        CfgAnswer::Need => Err(()),
    };
    let predicate = |e: CfgPredicateError<()>| match e {
        CfgPredicateError::Atom(()) => Refused::CfgAtom,
        CfgPredicateError::Malformed { .. } | CfgPredicateError::NotTakesOne { .. } => Refused::CfgForm,
    };
    let list = tokens.tokens();
    let index = |at: usize| u32::try_from(at).map_err(|_| Refused::CfgForm);
    // The metas to test, as token ranges of `attr`: a `#[cfg(..)]`'s own (inside its
    // brackets), or what a `#[cfg_attr(..)]` leaves under `cfg`.
    let metas = match kind {
        AttrKind::Cfg => {
            let is_hash = list.first().is_some_and(|t| matches!(t.kind, TokenKind::Punct { char: '#', .. }));
            let is_bracket = list.get(1).is_some_and(|t| matches!(t.kind, TokenKind::Open(Delim::Bracket)));
            let last = list.len().checked_sub(1).ok_or(Refused::CfgForm)?;
            if !(is_hash && is_bracket && tokens.close_of(1) == Some(last)) {
                return Err(Refused::CfgForm);
            }
            let mut one = AVec::new_in(alloc);
            one.push(index(2)?..index(last)?);
            one
        }
        AttrKind::CfgAttr => blaze_expand::cfg_attr_metas(&tokens, alloc, &mut atom).map_err(|e| match e {
            CfgAttrError::Predicate(p) => predicate(p),
            CfgAttrError::NotCfgAttr | CfgAttrError::NoPredicate { .. } => Refused::CfgForm,
        })?,
        // Only the two kinds above are asked for (`declared`).
        _ => return Err(Refused::CfgForm),
    };
    for m in metas.iter() {
        let (start, end) = (m.start as usize, m.end as usize);
        let is_cfg = end >= start + 3
            && list.get(start).is_some_and(|t| matches!(t.kind, TokenKind::Ident { .. }))
            && tokens.text_at(start) == "cfg"
            && list.get(start + 1).is_some_and(|t| matches!(t.kind, TokenKind::Open(Delim::Paren)))
            && tokens.close_of(start + 1) == Some(end - 1);
        if !is_cfg {
            // A `#[cfg]` holds nothing else; any other attribute a `cfg_attr` leaves is the
            // signature's lowering's to apply.
            if kind == AttrKind::Cfg {
                return Err(Refused::CfgForm);
            }
            continue;
        }
        if !blaze_expand::cfg_predicate_at(&tokens, start + 2..end - 1, alloc, &mut atom).map_err(predicate)? {
            return Ok(false);
        }
    }
    Ok(true)
}
