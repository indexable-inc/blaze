//! A row's path as DATA, apart from a name shown in a message (rule 30: one concept, one home;
//! rule 54: the class "a display string serving as data").
//!
//! The class: a fn row's path was made by the message printers (check.rs `item_shown`, reach.rs
//! `fn_shown`, lazy fill.rs `shown`), whose fallback for a key with no name in the names library
//! was the key's `Debug`. Every item declared in a BODY has no name there, so its row's path was
//! `ItemKey(<38 digits>)`, and the symbol code read the path's last segment as data: a
//! `#[no_mangle]` fn declared in a body was exported as "ItemKey(..)" (hello runner 9's stop,
//! fixed for that one symbol by hello-entry-shape-1). This file makes the two concepts two types:
//!
//! - [`RowPath`]: what a row carries as data: the item's key and the names of its segments, from
//!   the names the program declares (the names library's for a module-level item, the body's
//!   own local rows for an item declared in a block: its identifier is read where it is
//!   declared, front2 locals.rs). It is built by [`RowPath::of`] or refused by name
//!   ([`NoRowPath`]); there is no fallback text, so no display string can become a row's path.
//!   Code that needs the item's own identifier as data (a foreign fn's symbol, an intrinsic's
//!   table name) reads [`RowPath::ident`], never a split of a printed string.
//! - [`super::item_shown::ItemShown`]: an item in a message, with its key; it may say that no
//!   path is known. [`shown_name`] is the one printer of a fn's name in a message: its row path
//!   when one is built, else its `ItemShown`.
//!
//! A row path keeps the form rows always had (a module-level item's bare name; a member's
//! `Container::name`) and adds, for an item declared in a body, the path of the item whose body
//! declares it: `launcher::bstd_hosted_start`. An impl, which has no name, is its header as
//! written (`<impl Default for Stat>`). Built by a bounded loop up the owners, never recursion.

use core::fmt;
use std::boxed::Box;
use std::format;
use std::string::{String, ToString};
use std::vec::Vec;

use blaze_typeck::ItemKey;

use super::check::Records;
use super::fill::FillError;
use super::heads::{HeadError, Store};
use super::item_shown::{ItemShown, head_error_shown, impl_shown};
use super::keys::{Keys, MemberKind, Origin};
use super::locals::LocalKind;
use super::source::NamesSource;
use super::world_items::Gap;

/// The most owners a row path walks up (a member's container, a local item's enclosing body):
/// past it the path is refused by name ([`NoRowPath`]), never cut.
const MOST_OWNERS: usize = 64;

/// A fn row's path as data: its key, the path text, and where the item's own identifier
/// starts in it. Made only by [`RowPath::of`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RowPath {
    key: ItemKey,
    text: Box<str>,
    /// The byte offset of the item's own segment in `text` (after the last owner's `::`).
    own: usize,
}

/// No row path can be built for the item: one segment of it (`at`: the item itself or an
/// owner) is named by nothing the program declares and is of no kind a segment is written
/// for. Refused by name (rule 3), never printed from the key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NoRowPath {
    /// The row's item.
    pub item: ItemShown,
    /// The item whose segment has no name.
    pub at: ItemShown,
    pub why: &'static str,
}

impl fmt::Display for NoRowPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "front2: no row path is built for {}: its segment for {}: {} (front2 row_path.rs)", self.item, self.at, self.why)
    }
}

impl RowPath {
    /// The row path of checker key `k`: its own name, then its owners' up to a module-level
    /// item, each from what declares it. `Err`: a segment no declaration names.
    pub fn of<'n, S: NamesSource<'n>>(names: S, keys: &Keys, store: &Store<'n>, k: ItemKey) -> Result<RowPath, NoRowPath> {
        let refuse_at = |at: ItemKey, why: &'static str| NoRowPath { item: ItemShown::of(names, keys, k), at: ItemShown::of(names, keys, at), why };
        // The segments from the item outward.
        let mut segments: Vec<String> = Vec::new();
        let mut at = k;
        for _ in 0..MOST_OWNERS {
            // A member (an associated fn, const or type, a struct's ctor): its name, then its
            // container's segment.
            if let Some(Origin::Member { container, kind, name }) = keys.origin(at) {
                if *kind == MemberKind::Anon {
                    return Err(refuse_at(at, "an anonymous constant is no row: it is named by its text, not a path"));
                }
                segments.push(String::from(&**name));
                at = *container;
                continue;
            }
            // An item declared in a body: the name its declaration binds (the body's local
            // rows), then the path of the item whose body declares it.
            if let Some(local) = store.locals.head(at) {
                let declared = store.locals.body(local.enclosing).and_then(|b| b.rows.iter().find(|r| r.key == local.lib).map(|r| r.name));
                match declared {
                    Some(name) if !name.is_empty() => segments.push(String::from(name)),
                    // An impl declared in a body binds no name: its header as written.
                    _ if local.kind == LocalKind::Impl => segments.push(local_impl_segment(local.text).ok_or_else(|| refuse_at(at, "an impl declared in a body whose text has no `impl` header"))?),
                    // A nameless item declared in a body (`const _`): its kind, `{const}`.
                    _ => segments.push(format!("{{{}}}", local_kind_word(local.kind))),
                }
                at = local.enclosing;
                continue;
            }
            // A module-level item: its name in the names library, or an impl's header.
            let lib = keys.library_in(names, at).ok_or_else(|| refuse_at(at, "the key map and the names source hold no item under its key"))?;
            if let Some(name) = names.name_of(lib) {
                segments.push(String::from(name));
                return Ok(RowPath::joined(k, &segments));
            }
            // A member the key map has not filed (no item row; the member owners table names
            // it): its name, then its trait's or impl's segment.
            if let Some((owner, name)) = names.member_owner(lib) {
                segments.push(String::from(name));
                at = super::keys::checker_of(owner);
                continue;
            }
            // A variant (no item row; the variant table names it): its name, then its enum's.
            if let Some((enum_, index)) = names.variant_of(lib) {
                let name = names
                    .variants(enum_)
                    .and_then(|vs| vs.get(index as usize))
                    .map(|v| v.name)
                    .ok_or_else(|| refuse_at(at, "a variant whose enum's variant list the names source does not give at its index"))?;
                segments.push(String::from(name));
                at = super::keys::checker_of(enum_);
                continue;
            }
            match names.signature_text_of(lib).and_then(|t| t.impl_header) {
                // An impl: its header as written.
                Some(header) => segments.push(impl_shown(header)),
                // A nameless item that is no impl (`const _: () = { .. }`, an extern block):
                // its kind as its row says it, `{const}`.
                None => {
                    let kind = names
                        .kind_of_item(lib)
                        .ok_or_else(|| refuse_at(at, "a key in neither the items table, the variant table nor the member owners table of the names source"))?;
                    segments.push(format!("{{{kind}}}"));
                }
            }
            return Ok(RowPath::joined(k, &segments));
        }
        Err(refuse_at(at, "its owners are nested deeper than front2 row_path.rs `MOST_OWNERS`"))
    }

    /// `segments` (the item's own first) joined outermost first.
    fn joined(key: ItemKey, segments: &[String]) -> RowPath {
        let mut text = String::new();
        for (i, s) in segments.iter().rev().enumerate() {
            if i > 0 {
                text.push_str("::");
            }
            text.push_str(s);
        }
        let own = text.len() - segments.first().map_or(0, String::len);
        RowPath { key, text: text.into_boxed_str(), own }
    }

    pub fn key(&self) -> ItemKey {
        self.key
    }

    /// The whole path, as a row holds it and a message prints it.
    pub fn text(&self) -> &str {
        &self.text
    }

    /// The item's own identifier as its declaration writes it: the data a foreign fn's symbol
    /// and an intrinsic's table name are read from.
    pub fn ident(&self) -> &str {
        &self.text[self.own..]
    }

    pub fn into_text(self) -> Box<str> {
        self.text
    }
}

/// A body-local impl's segment (the caller has its kind: [`LocalKind::Impl`]): its text from
/// the keyword `impl` up to its body's `{`, as written (`<impl Tr for S>`), outer attributes
/// left out; `None`: no `impl` word stands before the `{`.
fn local_impl_segment(text: &str) -> Option<String> {
    let head = text.split('{').next()?;
    let at = head.match_indices("impl").map(|(i, _)| i).find(|&i| {
        let before = head[..i].chars().next_back();
        let after = head[i + 4..].chars().next();
        before.is_none_or(|c| !(c.is_alphanumeric() || c == '_')) && after.is_none_or(|c| !(c.is_alphanumeric() || c == '_'))
    })?;
    Some(impl_shown(&head[at..]))
}

/// The word a nameless body-local item's segment is written with (`{const}`), as blaze-surface
/// `ItemKind` words a module-level one.
fn local_kind_word(kind: LocalKind) -> &'static str {
    match kind {
        LocalKind::Fn => "function",
        LocalKind::Const => "const",
        LocalKind::Static => "static",
        LocalKind::Struct => "struct",
        LocalKind::Enum => "enum",
        LocalKind::Union => "union",
        LocalKind::Trait => "trait",
        LocalKind::TypeAlias => "type alias",
        LocalKind::Impl => "impl",
        LocalKind::Use => "use",
        LocalKind::Mod => "module",
        LocalKind::ExternBlock => "extern block",
        LocalKind::ExternCrate => "extern crate",
        LocalKind::MacroRules => "macro_rules",
        LocalKind::MacroDef => "macro",
        LocalKind::Other => "unhandled item",
    }
}

/// [`RowPath::of`] over a body's records, as `Front2Program::make` asks it for each row.
pub(crate) fn row_path_of<'n, S: NamesSource<'n>>(r: &Records<'_, 'n, S>, k: ItemKey) -> Result<RowPath, NoRowPath> {
    RowPath::of(r.names, &*r.keys, &*r.store, k)
}

/// THE printer of a fn's name in a message (check.rs `item_shown`, reach.rs `fn_shown` and lazy
/// fill.rs `shown` call it): its row path when one is built, else its [`ItemShown`] (its path
/// or that none is known, and its key in hex). Never a key's `Debug`.
pub fn shown_name<'n, S: NamesSource<'n>>(names: S, keys: &Keys, store: &Store<'n>, k: ItemKey) -> String {
    match RowPath::of(names, keys, store, k) {
        Ok(p) => p.into_text().into_string(),
        Err(no) => no.item.to_string(),
    }
}

/// The item's own name in a message (a type's spelling, `Result<Out, Missing>`): its row path's
/// identifier, else its [`ItemShown`].
pub fn shown_ident<'n, S: NamesSource<'n>>(names: S, keys: &Keys, store: &Store<'n>, k: ItemKey) -> String {
    match RowPath::of(names, keys, store, k) {
        Ok(p) => p.ident().to_string(),
        Err(no) => no.item.to_string(),
    }
}

/// [`FillError`]'s text with each item by its path ([`ItemShown`]); its `Display` holds no
/// names and prints keys.
pub fn fill_error_shown<'n, S: NamesSource<'n>>(names: S, keys: &Keys, e: &FillError) -> String {
    let key = |k: ItemKey| ItemShown::of(names, keys, k);
    let lib = |k: blaze_id::ItemKey| ItemShown::of_lib(names, k);
    match e {
        FillError::UnknownKey(k) => format!("{} is in no row of the front end's key map, and its crate's records hold no item under it (gap Fill/UnknownKey)", key(*k)),
        FillError::NoDetail(k) => format!("the names library holds no detail for item {}", lib(*k)),
        FillError::NotAHead(q) => format!("question {q:?} is not answered from an item's head"),
        FillError::NoHead(k, kind) => format!("the item {} is of kind `{kind}`, which has no head: no want makes its record (front2 by_origin, rule 3)", lib(*k)),
        FillError::VariantOfNoEnum(k) => format!("the item {} is a variant by its outline kind, and no enum's table holds it (front2 by_origin)", lib(*k)),
        FillError::AnonConst(k) => format!("the anonymous constant {} is filed with its head where a head writes it (front2 anon::file_asked); no want makes it", key(*k)),
        FillError::MemberOfMember { member, container, class } => format!(
            "the member {} is filed under {}, which is itself {class} (a member of a member): no want makes it (front2 by_origin)",
            key(*member),
            key(*container)
        ),
        FillError::NotAContainer { member, container, class } => format!(
            "the member {} is filed under {}, which is {class}, not a trait, an impl or an ADT (front2 by_origin)",
            key(*member),
            key(*container)
        ),
        FillError::LocalWithoutHead { lib: l, form } => format!(
            "the item {} is {form}: no head is made from its key, so no signature, predicates or record is asked of it (front2 by_origin kind_of_local)",
            lib(*l)
        ),
    }
}

/// A [`Gap`]'s text with the items it names by path: `WhereConst`, `CtorFn` and `Fill`, whose
/// `Display` prints bare keys; every other gap as its `Display` says it.
pub fn gap_shown<'n, S: NamesSource<'n>>(r: &Records<'_, 'n, S>, g: &Gap) -> String {
    match g {
        Gap::WhereConst(k) => format!("item {} has a `where const` condition, not yet a checker clause", ItemShown::of(r.names, &*r.keys, *k)),
        Gap::CtorFn(k) => format!("the constructor fn {} is in no variant of its type's head", ItemShown::of(r.names, &*r.keys, *k)),
        Gap::Fill(e) => fill_error_shown(r.names, &*r.keys, e),
        Gap::RefusedImpl { impl_, place, why } => {
            format!("the impl {} at {place}, a candidate of this question, has a refused header: {why}", ItemShown::of_lib(r.names, *impl_))
        }
        Gap::UnreadImpl(k, why) => format!(
            "the impl {}, a candidate filed without its trait or self type (unread as written), has no header: {why}",
            ItemShown::of_lib(r.names, *k)
        ),
        Gap::FnHead(k, why) => format!("the member fn {} has no signature: {why}", shown_name(r.names, &*r.keys, &*r.store, *k)),
        other => format!("{other}"),
    }
}

/// What splits a gap's census class (`check gap: RefusedImpl`) into the causes behind it
/// (program/left_out.rs `LeftOut::split`): the item the gap is about, by [`ItemShown`] or
/// [`shown_name`], and why; never the left-out fn's own name, a count, or a refused impl's
/// `file:line` and question (each splits one cause into many). A const term with no value by
/// its site and why (not its range or body); every other gap by [`gap_shown`].
pub fn gap_split<'n, S: NamesSource<'n>>(r: &Records<'_, 'n, S>, g: &Gap) -> String {
    match g {
        Gap::RefusedImpl { impl_, why, .. } => format!("the impl {}: {why}", ItemShown::of_lib(r.names, *impl_)),
        Gap::UnreadImpl(k, why) => format!("the impl {} (unread as written): {why}", ItemShown::of_lib(r.names, *k)),
        Gap::FnHead(k, why) => format!("the member fn {}: {why}", shown_name(r.names, &*r.keys, &*r.store, *k)),
        Gap::BodyConst { gap, .. } => format!("{:?}: {:?}", gap.site, gap.why),
        other => gap_shown(r, other),
    }
}

/// `CheckFail::Head`'s text with the body that needed the head and every item by path:
/// `checking <owner>: <the head error, by paths>` (its `Display` prints both by key).
pub fn head_fail_shown<'n, S: NamesSource<'n>>(r: &Records<'_, 'n, S>, owner: ItemKey, e: &HeadError) -> String {
    format!("checking {}: {}", shown_name(r.names, &*r.keys, &*r.store, owner), head_error_shown(r.names, e))
}
