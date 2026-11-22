//! The fns and statics EXPORTED from inside a body: an item declared in a block that carries
//! `#[no_mangle]` (`#[unsafe(no_mangle)]`) or `#[export_name = ".."]`.
//!
//! The Reference, "Items": "Items may be declared in a block"; "Statements", "Item
//! declarations": "An item declaration statement has a syntactic form identical to an item
//! declaration within a module". "The `no_mangle` attribute" ([abi.no_mangle]): "the symbol
//! name for the function or static ... will be the same as the item's name"; [abi.export_name]:
//! "specifies the name of the symbol that will be exported on a function or static". Neither
//! rule asks where the item is written: an export declared in a body is an export of the
//! program like one at a module's top. D104's `#[bstd::main]` expansion is the case that asks:
//! the process's `fn main() -> Exit` holds, in its body, `#[unsafe(no_mangle)] pub extern "C"
//! fn bstd_hosted_start(..)`, the hosted entry.
//!
//! What is here:
//! - [`walk`]: from one item's whole text (a fn's, a const's or a static's), every export
//!   declared in its blocks, in the blocks of the items declared there, and so on, by a WORK
//!   LIST over texts (no recursion): a body's blocks are read by the one token scan of block
//!   items (`blaze_file_items::nested::scan`), a module, impl or trait declared in a block by
//!   the one outline (`blaze_file_items::outline`). Each item's `cfg` and export attributes
//!   are read from its signature's lowering (`cfg` and `cfg_attr` evaluated under the crate's
//!   cfg), as links.rs and program.rs `symbols_of` read a module-level item's. A text that
//!   does not hold the words `no_mangle` or `export_name` holds no export and is not read
//!   further (an export's attribute is written in its own text, which is a slice of every
//!   text around it).
//! - Each export's KEY is its own local item key, derived as [`super::locals`]'s
//!   `Locals::keep` derives it when the enclosing body is lowered: from the enclosing body's
//!   key ([`body_key`]), its kind's tag, its name and its ordinal among the same-kind,
//!   same-name rows of that body, rows ordered by block (in the order the blocks' `{` is
//!   written, the lowering's scope order) and then by text. [`local_key`] and [`body_key`]
//!   are the ONE home of that derivation (`Locals::keep` calls them). Where the text alone
//!   cannot give the ordinal (a statement macro call before it in the body may write an item
//!   of the same name), or the item is a member of a module, impl or trait written in a block
//!   (its key is the names library's block module's, not derived here), the key is
//!   [`Unkeyed`] and says why: a caller that needs the key refuses by that reason.
//! - The program's glue: [`nested_entries`] adds the exported entries declared in bodies of
//!   crate 0 to program.rs `exported_entry`'s candidates (two under one entry symbol stay its
//!   `TwoEntries`, each named by its whole path), and [`enclosing_bodies`] names the bodies
//!   around such an entry, outermost first, which the program lowers before the entry so the
//!   entry's head is kept.
//!
//! A block the names library writes as a module (one that holds a type, an impl, a trait, a
//! `mod`, an `extern crate` or a statement macro call, and every block around it:
//! blaze-scope `expanded_blocks::written`) is copied whole into that module's text, so its
//! fns are items of the library too, listed with crate 0's items and found there by the
//! module-level rule. With a library key ([`Top::library`]) such a block's items are counted
//! for ordinals but neither reported nor walked into by [`walk`]: one export is never found
//! twice. [`walk_every`] still lists such a block's fns, statics and consts by their
//! body-local keys and walks into their bodies, because `Locals::keep` files them under those
//! keys whatever the library also writes.
//!
//! A new stage: no existing producer's output changes, no key domain is bumped.

use std::alloc::Global;
use std::boxed::Box;
use std::vec::Vec;

use blaze_file_items::nested::{NestedEnd, NestedKind, scan};
use blaze_id::{CrateItemKey, ItemKey as LibKey};
use blaze_lower::{AttrMeta, head};
use blaze_own::{AString, AVec, Alloc};
use blaze_surface::{Edition, Item, ItemDetail, ItemKind, Range};
use blaze_typeck::ItemKey;

use super::foreign_locals::ForeignKind;
use super::heads::CrateCfg;
use super::keys::checker_of;
use super::locals::LocalKind;
use super::program::Front2Refusal;
use super::source::NamesSource;
use crate::front::HOSTED_START;

/// The `n`th hex digit of `k`, most significant first.
fn hex_digit(k: u128, n: usize) -> u8 {
    let d = ((k >> (124 - 4 * n)) & 0xf) as u8;
    if d < 10 { b'0' + d } else { b'a' + d - 10 }
}

/// The key every local item of a body is derived under: from the body's item `item` (a
/// member's body: its container) and the body's checker key `owner` (a member's body is not
/// its container's). THE derivation of locals.rs `Locals::keep`.
pub(super) fn body_key(crate_key: CrateItemKey, item: LibKey, owner: ItemKey) -> LibKey {
    let mut hex = [0u8; 32];
    for (n, b) in hex.iter_mut().enumerate() {
        *b = hex_digit(owner.0, n);
    }
    let name = core::str::from_utf8(&hex).unwrap_or("");
    blaze_scope::task::key_of(crate_key, item, "Body", name, 0)
}

/// The key of the local item of kind tag `tag`, named `name`, the `ordinal`th of that kind
/// and name among the rows of the body whose [`body_key`] is `body`, with
/// [`LibKey::INNER`] set. THE derivation of locals.rs `Locals::keep`.
pub(super) fn local_key(crate_key: CrateItemKey, body: LibKey, tag: &str, name: &str, ordinal: u32) -> LibKey {
    let mut key = blaze_scope::task::key_of(crate_key, body, tag, name, ordinal);
    key.0.0[8] |= LibKey::INNER;
    key
}

/// Whether `text` can hold an export: its attribute's word is in it.
fn mentions(text: &str) -> bool {
    text.contains("no_mangle") || text.contains("export_name")
}

/// `r` of `text`; `None` past its end or across a character.
fn cut(text: &str, r: Range) -> Option<&str> {
    text.get(r.start as usize..r.end as usize)
}

/// A plain string literal's contents; an escape or a raw string is refused by the caller (no
/// symbol of a real crate needs one, and none is guessed).
fn plain_str(text: &str) -> Option<&str> {
    let inner = text.trim().strip_prefix('"')?.strip_suffix('"')?;
    (!inner.contains('\\') && !inner.contains('"')).then_some(inner)
}

/// Why an export declared in a body has no key read from text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Unkeyed {
    /// The walk was given no library key for its top item (links.rs reads symbols only).
    NoLibrary,
    /// A macro call in statement position comes before it in its body's row order: what the
    /// call writes is known only after it is expanded, so the ordinal is not read from text.
    AfterCall,
    /// It is declared in a module, impl or trait written in a block: its key is the names
    /// library's block module's, not a body's local item's.
    InContainer,
}

impl Unkeyed {
    /// The reason's words, for a refusal.
    pub fn words(self) -> &'static str {
        match self {
            Unkeyed::NoLibrary => "an export declared in a block, read without its item's library key",
            Unkeyed::AfterCall => "an export declared in a block after a statement macro call of the same body: its key is read from the body's lowering, which is not done here yet",
            Unkeyed::InContainer => "an export declared in a module, impl or trait written in a block: its key is not derived from text yet",
        }
    }
}

/// What kind of text an enclosing entry is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Form {
    /// A fn's, const's or static's whole text: its blocks are scanned.
    Body,
    /// A module's, impl's or trait's whole text, written in a block: its outline is read.
    Items,
}

/// A text the walk reads: the top item, or an item declared inside one it read.
#[derive(Clone, Copy, Debug)]
pub struct Encloser<'a> {
    /// Its name as written (`impl` for an impl, `_` for `const _`).
    pub name: &'a str,
    /// The encloser it is written in (an index of [`Walk::bodies`]); `None` for the top.
    pub parent: Option<u32>,
    /// Its own item key (the top's library key, a local item's derived key).
    pub key: Result<LibKey, Unkeyed>,
    pub text: &'a str,
    pub form: Form,
}

/// What an export is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExportForm {
    Fn,
    Static,
}

/// One export declared inside the top item.
#[derive(Clone, Copy, Debug)]
pub struct NestedExport<'a> {
    /// `export_name`'s value, else the identifier (`r#` dropped): the Reference, [abi.no_mangle].
    pub symbol: &'a str,
    pub form: ExportForm,
    pub name: &'a str,
    /// Its whole text, attributes included.
    pub text: &'a str,
    pub key: Result<LibKey, Unkeyed>,
    /// The encloser it is declared in (an index of [`Walk::bodies`]).
    pub within: u32,
}

/// One fn, static or const declared inside the top item, exported or not: what
/// [`walk_every`] lists (the lazy program's search for a body-local key it was asked about cold,
/// front2/program/lazy/locate.rs). Its key is derived by the one rule ([`local_key`]).
#[derive(Clone, Copy, Debug)]
pub struct NestedLocal<'a> {
    pub name: &'a str,
    pub kind: LocalKind,
    pub key: Result<LibKey, Unkeyed>,
    /// The encloser it is declared in (an index of [`Walk::bodies`]).
    pub within: u32,
}

/// The walk's answer: every text read, and every export found, in walk order (each body's
/// exports in its row order; a body after the body it is declared in). `locals`: empty but in
/// [`walk_every`].
pub struct Walk<'a> {
    pub bodies: AVec<'a, Encloser<'a>>,
    pub exports: AVec<'a, NestedExport<'a>>,
    pub locals: AVec<'a, NestedLocal<'a>>,
}

/// Why the walk stopped, naming the item by its whole path (`outer::inner::item`).
#[derive(Debug)]
pub enum NestedRefusal<'a> {
    /// A body whose brackets do not balance: its blocks cannot be told apart.
    Unbalanced { path: AString<'a> },
    /// An attribute that does not read; `why` names the rule.
    Attr { path: AString<'a>, why: &'static str },
    /// A `cfg` the crate's cfg set does not answer.
    Cfg { path: AString<'a> },
    /// An `extern` block in a block that does not read (super::foreign_locals).
    Foreign { path: AString<'a>, why: &'static str },
    Alloc,
}

impl core::fmt::Display for NestedRefusal<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            NestedRefusal::Unbalanced { path } => write!(f, "`{}`: its brackets do not balance, so the items declared in its blocks are not read", path.as_str()),
            NestedRefusal::Attr { path, why } => write!(f, "`{}`: {why}", path.as_str()),
            NestedRefusal::Cfg { path } => write!(f, "`{}`: its signature needs a `cfg` the crate does not answer", path.as_str()),
            NestedRefusal::Foreign { path, why } => write!(f, "`{}`: {why}", path.as_str()),
            NestedRefusal::Alloc => write!(f, "the walk of exports declared in blocks ran out of memory"),
        }
    }
}

/// The item the walk starts at.
#[derive(Clone, Copy, Debug)]
pub struct Top<'a> {
    /// Its whole text (a fn's, const's or static's).
    pub text: &'a str,
    /// Its path as shown in a refusal or a finding.
    pub name: &'a str,
    /// Its crate's key and its own library key: given, exports are keyed and blocks the names
    /// library writes as modules are left to the library; `None`: symbols only.
    pub library: Option<(CrateItemKey, LibKey)>,
}

/// The crate's facts the walk reads with.
#[derive(Clone, Copy)]
pub struct Reading<'a, 'c> {
    pub edition: Edition,
    pub atoms: &'c [(&'c str, Option<&'c str>)],
    pub alloc: Alloc<'a>,
}

/// What an item's signature says.
enum Read<'a> {
    /// Its `cfg` is off: it and everything in it are gone.
    Off,
    On { symbol: Option<&'a str> },
}

/// Why an item's signature did not read.
#[derive(Clone, Copy)]
enum Bad {
    Attr(&'static str),
    Cfg,
}

struct Walker<'a, 'c> {
    reading: Reading<'a, 'c>,
    library: Option<(CrateItemKey, LibKey)>,
    /// [`walk_every`]: every fn, static and const of every block is listed and every body
    /// walked into, not only the texts that can hold an export.
    every: bool,
    /// The top body's (item, checker key) its locals are derived under ([`body_key`]), when
    /// it is not the top's own library key and its checker key: a member's body is derived
    /// under its container and the member's key (locals.rs `Enclosing`).
    top_body: Option<(LibKey, ItemKey)>,
    out: Walk<'a>,
}

/// Every export declared inside `top`, by a work list over the texts in it (module comment).
pub fn walk<'a, 'c>(top: Top<'a>, reading: Reading<'a, 'c>) -> Result<Walk<'a>, NestedRefusal<'a>> {
    walk_in(top, reading, false, None)
}

/// Every fn, static and const declared inside `top` ([`Walk::locals`], each keyed by the rule
/// `Locals::keep` keys it by, or [`Unkeyed`] saying why the text cannot), and its exports, by
/// the same work list into every body. `top_body`: for a member's body, its container's library
/// key and the member's checker key (the body key's inputs); `None` for a free item's.
pub fn walk_every<'a, 'c>(top: Top<'a>, top_body: Option<(LibKey, ItemKey)>, reading: Reading<'a, 'c>) -> Result<Walk<'a>, NestedRefusal<'a>> {
    walk_in(top, reading, true, top_body)
}

fn walk_in<'a, 'c>(top: Top<'a>, reading: Reading<'a, 'c>, every: bool, top_body: Option<(LibKey, ItemKey)>) -> Result<Walk<'a>, NestedRefusal<'a>> {
    let alloc = reading.alloc;
    let mut w = Walker { reading, library: top.library, every, top_body, out: Walk { bodies: AVec::new_in(alloc), exports: AVec::new_in(alloc), locals: AVec::new_in(alloc) } };
    if !every && !mentions(top.text) {
        return Ok(w.out);
    }
    let key = match top.library {
        Some((_, lib)) => Ok(lib),
        None => Err(Unkeyed::NoLibrary),
    };
    w.out.bodies.try_push(Encloser { name: top.name, parent: None, key, text: top.text, form: Form::Body }).map_err(|_| NestedRefusal::Alloc)?;
    // A queue over the growing list: each text pushed is a strict part of the one it is in,
    // so the walk ends.
    let mut next = 0usize;
    while next < w.out.bodies.len() {
        let at = next as u32;
        match w.out.bodies[next].form {
            Form::Body => w.body(at)?,
            Form::Items => w.items(at)?,
        }
        next += 1;
    }
    Ok(w.out)
}

impl<'a> Walk<'a> {
    /// The indices of the enclosers from the top to `within`, outermost first.
    fn line(&self, within: u32, alloc: Alloc<'a>) -> Result<AVec<'a, u32>, NestedRefusal<'a>> {
        let mut line: AVec<'a, u32> = AVec::new_in(alloc);
        let mut at = Some(within);
        // A chain is at most as long as the list.
        for _ in 0..self.bodies.len() {
            let Some(here) = at else { break };
            line.try_push(here).map_err(|_| NestedRefusal::Alloc)?;
            at = self.bodies.get(here as usize).and_then(|b| b.parent);
        }
        line.reverse();
        Ok(line)
    }

    /// The whole path of an item named `last` declared in `within`: the enclosers' names from
    /// the top, then `last` (none when empty: the path of `within` itself), joined by `::`.
    pub fn path(&self, within: u32, last: &str, alloc: Alloc<'a>) -> Result<AString<'a>, NestedRefusal<'a>> {
        let mut out = AString::new_in(alloc);
        for (n, &i) in self.line(within, alloc)?.iter().enumerate() {
            if n > 0 {
                out.push_str("::");
            }
            out.push_str(self.bodies[i as usize].name);
        }
        if !last.is_empty() {
            out.push_str("::");
            out.push_str(last);
        }
        Ok(out)
    }

    /// The keys of the bodies around an export, outermost first (the top's library key first):
    /// each must be lowered, in this order, before the export's own head is kept.
    pub fn chain(&self, export: &NestedExport<'a>, alloc: Alloc<'a>) -> Result<Result<AVec<'a, LibKey>, Unkeyed>, NestedRefusal<'a>> {
        self.chain_to(export.within, alloc)
    }

    /// The keys of the bodies from the top to encloser `within`, outermost first: what is
    /// lowered, in this order, before an item declared in `within` is kept.
    pub fn chain_to(&self, within: u32, alloc: Alloc<'a>) -> Result<Result<AVec<'a, LibKey>, Unkeyed>, NestedRefusal<'a>> {
        let mut keys: AVec<'a, LibKey> = AVec::new_in(alloc);
        for &i in self.line(within, alloc)?.iter() {
            match self.bodies[i as usize].key {
                Ok(k) => keys.try_push(k).map_err(|_| NestedRefusal::Alloc)?,
                Err(u) => return Ok(Err(u)),
            }
        }
        Ok(Ok(keys))
    }
}

impl<'a, 'c> Walker<'a, 'c> {
    fn unbalanced(&self, within: u32) -> NestedRefusal<'a> {
        match self.out.path(within, "", self.reading.alloc) {
            Ok(p) => NestedRefusal::Unbalanced { path: p },
            Err(e) => e,
        }
    }

    fn refuse(&self, within: u32, last: &str, bad: Bad) -> NestedRefusal<'a> {
        let path = match self.out.path(within, last, self.reading.alloc) {
            Ok(p) => p,
            Err(e) => return e,
        };
        match bad {
            Bad::Attr(why) => NestedRefusal::Attr { path, why },
            Bad::Cfg => NestedRefusal::Cfg { path },
        }
    }

    /// An item's `cfg` and export, from its signature's lowering; lowered only when its
    /// signature can turn it off or name an export (the Reference, [abi.export_name]: "When
    /// the no_mangle and export_name attributes are both applied to the same item, the symbol
    /// name from export_name is used").
    fn read(&self, item_text: &'a str, entry: &Item) -> Result<Read<'a>, Bad> {
        let sig = cut(item_text, entry.signature).ok_or(Bad::Attr("a signature outside its entry"))?;
        if !(sig.contains("no_mangle") || sig.contains("export_name") || sig.contains("cfg")) {
            return Ok(Read::On { symbol: None });
        }
        let low = blaze_lower::lower_signature(item_text, self.reading.edition, &mut CrateCfg(self.reading.atoms), self.reading.alloc).map_err(|_| Bad::Cfg)?;
        let v = low.view();
        if v.cfg_out() {
            return Ok(Read::Off);
        }
        let bad = Bad::Attr("an attribute row the signature's lowering does not hold");
        let rows = v.rows(v.list(head::ATTRS)).map_err(|_| Bad::Attr("an attribute row the signature's lowering does not hold"))?;
        let (mut no_mangle, mut export_name) = (false, None);
        for &n in rows {
            let a = v.attr(n).map_err(|_| Bad::Attr("an attribute row the signature's lowering does not hold"))?;
            let Some(path) = v.item_relative(a.path).and_then(|r| cut(item_text, r)) else { return Err(bad) };
            let Some(args) = v.item_relative(a.args).and_then(|r| cut(item_text, r)) else { return Err(bad) };
            match path.trim() {
                // `#[unsafe(no_mangle)]`'s row has the inner path (blaze-lower's UNSAFE_META).
                "no_mangle" => no_mangle = true,
                "export_name" => {
                    if a.meta != AttrMeta::KeyValue {
                        return Err(Bad::Attr("`export_name` is `#[export_name = \"..\"]` (Reference: MetaNameValueStr)"));
                    }
                    let value = plain_str(args).ok_or(Bad::Attr("an `export_name` that is no plain string literal"))?;
                    if export_name.replace(value).is_some() {
                        return Err(Bad::Attr("`export_name` given twice"));
                    }
                }
                _ => {}
            }
        }
        let symbol = match (export_name, no_mangle) {
            (Some(s), _) => Some(s),
            // The Reference, [abi.no_mangle]: the symbol is the item's identifier.
            (None, true) => {
                let name = entry.name.and_then(|r| cut(item_text, r)).ok_or(Bad::Attr("a `no_mangle` item without a name in its outline"))?;
                Some(name.strip_prefix("r#").unwrap_or(name))
            }
            (None, false) => None,
        };
        Ok(Read::On { symbol })
    }

    /// One fn's, const's or static's body: the items of its blocks in row order.
    fn body(&mut self, at: u32) -> Result<(), NestedRefusal<'a>> {
        let b = self.out.bodies[at as usize];
        let alloc = self.reading.alloc;
        let nested = scan(b.text, alloc);
        if nested.end == NestedEnd::Unbalanced {
            return Err(self.unbalanced(at));
        }
        // The blocks the names library writes as modules: only the top item's body is the
        // library's (a local item's own body is scanned when it is lowered).
        let written = match (at, self.library) {
            (0, Some((crate_key, lib))) => Some(blaze_scope::blocks::block_modules(crate_key, lib, &nested, alloc)),
            _ => None,
        };
        // The key every local item of this body is derived under.
        let top_body = if at == 0 { self.top_body } else { None };
        let parent = match (self.library, b.key, top_body) {
            (Some((crate_key, _)), Ok(_), Some((item, owner))) => Ok((crate_key, body_key(crate_key, item, owner))),
            (Some((crate_key, _)), Ok(item), None) => Ok((crate_key, body_key(crate_key, item, checker_of(item)))),
            (None, ..) => Err(Unkeyed::NoLibrary),
            (_, Err(u), _) => Err(u),
        };
        // Row order: by block (the order the blocks' `{` is written: the lowering's scopes),
        // then by text.
        let mut order: AVec<'a, u32> = AVec::new_in(alloc);
        for i in 0..nested.items.len() {
            order.try_push(i as u32).map_err(|_| NestedRefusal::Alloc)?;
        }
        order.sort_unstable_by_key(|&i| {
            let it = nested.items[i as usize];
            (it.block, it.range.start)
        });
        // The live fn, static and const rows so far: (kind, name).
        let mut rows: AVec<'a, (LocalKind, &'a str)> = AVec::new_in(alloc);
        for &i in order.iter() {
            let item = nested.items[i as usize];
            let Some(item_text) = cut(b.text, item.range) else { return Err(self.refuse(at, "", Bad::Attr("a block item outside its body"))) };
            let in_written = written.as_ref().is_some_and(|w| w.get(item.block as usize).copied().flatten().is_some());
            match item.kind {
                NestedKind::Fn | NestedKind::Static | NestedKind::Const => {
                    let kind = match item.kind {
                        NestedKind::Fn => LocalKind::Fn,
                        NestedKind::Static => LocalKind::Static,
                        _ => LocalKind::Const,
                    };
                    let outline = blaze_file_items::outline(item_text, self.reading.edition, alloc);
                    let Some(entry) = outline.items().first() else { return Err(self.refuse(at, "", Bad::Attr("a block item the outline does not read"))) };
                    let name: &'a str = entry.name.and_then(|r| cut(item_text, r)).unwrap_or("_");
                    let symbol = match self.read(item_text, entry) {
                        Ok(Read::Off) => continue,
                        Ok(Read::On { symbol }) => symbol,
                        Err(bad) => return Err(self.refuse(at, name, bad)),
                    };
                    let ordinal = rows.iter().filter(|r| r.0 == kind && r.1 == name).count() as u32;
                    rows.try_push((kind, name)).map_err(|_| NestedRefusal::Alloc)?;
                    // A fn, static or const of a block the names library writes as a module is
                    // that module's item too, so its export is found there by the module-level
                    // rule and `walk` passes it over (one export never found twice). Its
                    // BODY-LOCAL key is still the one `Locals::keep` files it under: locals.rs
                    // keys every local fn, const and static by `local_key`, a written block's
                    // too (only a type, trait, `mod` or `extern crate` is the block module's:
                    // `LocalKind::in_block_module`). So `walk_every`, whose locals the lazy
                    // program's cold search matches against that key (program/lazy/locate.rs),
                    // lists it and walks into its body. curve25519-dalek 4.1.3
                    // backend/serial/u64/field.rs:116, `fn m` in `<&FieldElement51 as Mul>::mul`,
                    // whose block the `debug_assert!` statements after it make written: asked
                    // cold, it was "no fn its records hold" (tempo-primitives, 2026-11-21 15:06).
                    if in_written && !self.every {
                        continue;
                    }
                    let after_call = nested.calls.iter().any(|c| !c.expr && (c.block, c.range.start) < (item.block, item.range.start));
                    let key = match parent {
                        Ok(_) if after_call => Err(Unkeyed::AfterCall),
                        Ok((crate_key, body)) => Ok(local_key(crate_key, body, kind.tag(), name, ordinal)),
                        Err(u) => Err(u),
                    };
                    let form = match kind {
                        LocalKind::Fn => Some(ExportForm::Fn),
                        LocalKind::Static => Some(ExportForm::Static),
                        _ => None,
                    };
                    if let (Some(symbol), Some(form), false) = (symbol, form, in_written) {
                        self.out.exports.try_push(NestedExport { symbol, form, name, text: item_text, key, within: at }).map_err(|_| NestedRefusal::Alloc)?;
                    }
                    if self.every {
                        self.out.locals.try_push(NestedLocal { name, kind, key, within: at }).map_err(|_| NestedRefusal::Alloc)?;
                    }
                    if entry.body.is_some() && (self.every || mentions(item_text)) {
                        self.out.bodies.try_push(Encloser { name, parent: Some(at), key, text: item_text, form: Form::Body }).map_err(|_| NestedRefusal::Alloc)?;
                    }
                }
                NestedKind::ExternBlock => {
                    // Its fns and statics are rows of this block (locals.rs `Locals::keep`):
                    // counted for the ordinals; a foreign item is never an export.
                    let decls = match super::foreign_locals::declared(item_text, self.reading.edition, &mut CrateCfg(self.reading.atoms), alloc) {
                        Ok((_, decls)) => decls,
                        Err(r) => {
                            let path = self.out.path(at, "extern", alloc)?;
                            return Err(NestedRefusal::Foreign { path, why: r.form() });
                        }
                    };
                    for d in decls.iter() {
                        let kind = match d.kind {
                            ForeignKind::Fn => LocalKind::Fn,
                            ForeignKind::Static => LocalKind::Static,
                        };
                        rows.try_push((kind, d.name)).map_err(|_| NestedRefusal::Alloc)?;
                    }
                }
                // In `walk_every` too, only for exports: a member of a container written in a
                // block is keyed by its container, not derived here ([`Unkeyed::InContainer`]).
                NestedKind::Mod | NestedKind::Impl | NestedKind::Trait if !in_written && mentions(item_text) => {
                    let name = match item.kind {
                        NestedKind::Impl => "impl",
                        NestedKind::Trait => "trait",
                        _ => "mod",
                    };
                    self.out.bodies.try_push(Encloser { name, parent: Some(at), key: Err(Unkeyed::InContainer), text: item_text, form: Form::Items }).map_err(|_| NestedRefusal::Alloc)?;
                }
                _ => {}
            }
        }
        Ok(())
    }

    /// A module, impl or trait written in a block: its entries by its outline, an entry whose
    /// `cfg` is off passed over with everything written inside it, an `extern` block's entries
    /// never exports.
    fn items(&mut self, at: u32) -> Result<(), NestedRefusal<'a>> {
        let b = self.out.bodies[at as usize];
        let alloc = self.reading.alloc;
        let outline = blaze_file_items::outline(b.text, self.reading.edition, alloc);
        let items = outline.items();
        let mut i = 0usize;
        while i < items.len() {
            let entry = &items[i];
            let Some(item_text) = cut(b.text, entry.range) else { return Err(self.refuse(at, "", Bad::Attr("an outline entry outside its text"))) };
            let name: &'a str = entry.name.and_then(|r| cut(item_text, r)).unwrap_or("_");
            let skip = match entry.detail {
                ItemDetail::ExternBlock { .. } => true,
                ItemDetail::Fn { .. } | ItemDetail::Static { .. } | ItemDetail::Const { .. } => match self.read(item_text, entry) {
                    Ok(Read::Off) => true,
                    Ok(Read::On { symbol }) => {
                        let form = match entry.detail {
                            ItemDetail::Fn { .. } => Some(ExportForm::Fn),
                            ItemDetail::Static { .. } => Some(ExportForm::Static),
                            _ => None,
                        };
                        if let (Some(symbol), Some(form)) = (symbol, form) {
                            self.out.exports.try_push(NestedExport { symbol, form, name, text: item_text, key: Err(Unkeyed::InContainer), within: at }).map_err(|_| NestedRefusal::Alloc)?;
                        }
                        if entry.body.is_some() && mentions(item_text) {
                            self.out.bodies.try_push(Encloser { name, parent: Some(at), key: Err(Unkeyed::InContainer), text: item_text, form: Form::Body }).map_err(|_| NestedRefusal::Alloc)?;
                        }
                        false
                    }
                    Err(bad) => return Err(self.refuse(at, name, bad)),
                },
                ItemDetail::Mod { .. } | ItemDetail::Impl | ItemDetail::Trait => {
                    // A container is lowered only when its attributes can turn it off.
                    let sig = cut(item_text, entry.signature).unwrap_or("");
                    if sig.contains("cfg") {
                        match self.read(item_text, entry) {
                            Ok(Read::Off) => true,
                            Ok(Read::On { .. }) => false,
                            Err(bad) => return Err(self.refuse(at, name, bad)),
                        }
                    } else {
                        false
                    }
                }
                _ => false,
            };
            i = if skip { entry.end.at().max(i + 1) } else { i + 1 };
        }
        Ok(())
    }
}

/// The refusal of the entry rule for a walk's refusal, naming the item.
fn rule_refusal(top: &str, refusal: NestedRefusal<'_>) -> Front2Refusal {
    Front2Refusal::Attrs { function: Box::from(top), why: Box::from(std::format!("an item declared in its blocks: {refusal}").as_str()) }
}

/// Crate 0's free fns, consts and statics whose text can hold an export, in key order.
fn tops<'n, S: NamesSource<'n>>(names: S) -> Result<Vec<LibKey, Global>, Front2Refusal> {
    let mut keys: Vec<LibKey, Global> = Vec::new_in(Global);
    let listed = names.each_item(0, &mut |key, _module| {
        if matches!(names.kind_of_item(key), Some(ItemKind::Function | ItemKind::Const | ItemKind::Static)) && names.member_owner(key).is_none() {
            keys.push(key);
        }
    });
    if !listed {
        return Err(Front2Refusal::Attrs { function: Box::from("crate 0"), why: Box::from("its items were not listed by the names source, so the exports declared in its bodies are not read") });
    }
    keys.sort_unstable();
    Ok(keys)
}

/// Walks the top item `key` of crate 0 (module comment), `None` when its text holds no
/// export's word.
fn walk_top<'n, S: NamesSource<'n>>(names: S, key: LibKey, alloc: Alloc<'n>) -> Result<Option<(Walk<'n>, &'n str)>, Front2Refusal> {
    let Some(it) = names.text_of(key) else { return Ok(None) };
    if !mentions(it.text) {
        return Ok(None);
    }
    let name = it.name.unwrap_or("_");
    let (Some(crate_key), Some(edition), Some(atoms)) = (names.crate_key(0), names.edition_of(0), names.cfg_atoms(0)) else {
        return Err(Front2Refusal::Attrs { function: Box::from(name), why: Box::from("crate 0's key, edition or cfg was not read by the names source") });
    };
    let top = Top { text: it.text, name, library: Some((crate_key, key)) };
    let walked = walk(top, Reading { edition, atoms, alloc }).map_err(|r| rule_refusal(name, r))?;
    Ok(Some((walked, name)))
}

/// THE ENTRY RULE's candidates declared in blocks (program.rs `exported_entry`): every fn of
/// crate 0 exported as [`HOSTED_START`] (rank 0) or `main` (rank 1) from inside a body, each
/// under its own local item key and shown by its whole path, pushed onto `found` beside the
/// module-level candidates. One needed as the entry whose key the text does not give is
/// refused by name ([`Unkeyed`]).
pub(super) fn nested_entries<'n, S: NamesSource<'n>>(names: S, alloc: Alloc<'n>, found: &mut Vec<(u8, LibKey, Box<str>), Global>) -> Result<(), Front2Refusal> {
    for key in tops(names)? {
        let Some((walked, top)) = walk_top(names, key, alloc)? else { continue };
        for e in walked.exports.iter() {
            let rank = match (e.form, e.symbol) {
                (ExportForm::Fn, HOSTED_START) => 0,
                (ExportForm::Fn, "main") => 1,
                _ => continue,
            };
            let path = walked.path(e.within, e.name, alloc).map_err(|r| rule_refusal(top, r))?;
            let lib = e.key.map_err(|u| Front2Refusal::Attrs { function: Box::from(path.as_str()), why: Box::from(u.words()) })?;
            let module = names.text_of(key).and_then(|it| names.module_path(it.module)).unwrap_or("");
            let shown = if module.is_empty() { std::format!("{}` (declared in a block", path.as_str()) } else { std::format!("{module}::{}` (declared in a block", path.as_str()) };
            found.push((rank, lib, shown.into_boxed_str()));
        }
    }
    Ok(())
}

/// The bodies around the entry `entry`, outermost first, when it is declared in a block of a
/// crate 0 item (an [`LibKey::is_inner`] key); empty for any other entry. Each is lowered
/// before the entry, in this order, so its local items are kept (locals.rs `Locals::keep`)
/// and the next one's (at last the entry's) head and text are known.
pub(super) fn enclosing_bodies<'n, S: NamesSource<'n>>(names: S, alloc: Alloc<'n>, entry: LibKey) -> Result<Vec<LibKey, Global>, Front2Refusal> {
    let mut out: Vec<LibKey, Global> = Vec::new_in(Global);
    if !entry.is_inner() {
        return Ok(out);
    }
    for key in tops(names)? {
        let Some((walked, top)) = walk_top(names, key, alloc)? else { continue };
        for e in walked.exports.iter() {
            if e.key != Ok(entry) {
                continue;
            }
            match walked.chain(e, alloc).map_err(|r| rule_refusal(top, r))? {
                Ok(keys) => out.extend(keys.iter().copied()),
                Err(u) => return Err(Front2Refusal::Attrs { function: Box::from(e.name), why: Box::from(u.words()) }),
            }
            return Ok(out);
        }
    }
    Err(Front2Refusal::Attrs {
        function: Box::from("the process's entry"),
        why: Box::from("its key is of an item declared in a block, and no item of crate 0 declares an export under that key"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const MAC: &[(&str, Option<&str>)] = &[("target_os", Some("macos")), ("unix", None)];

    /// The Reference, "Items" ("Items may be declared in a block") and [abi.no_mangle] /
    /// [abi.export_name] (the symbol is the identifier, or `export_name`'s value; both given:
    /// `export_name`'s): exports two and three levels deep are found, a `cfg`-off one is not,
    /// nor anything inside it, a foreign fn of an `extern` block is never an export, and a fn
    /// of an impl written in a block is found but unkeyed. With the walk limited to the top's
    /// own blocks (no work list), `deep` and `in_impl` are missing.
    ///
    /// Prints `test front2::nested_exports::tests::exports_at_every_depth ... ok`.
    #[test]
    fn exports_at_every_depth() {
        let text = "fn outer() {\n\
                    \x20   #[unsafe(no_mangle)] pub extern \"C\" fn one() {}\n\
                    \x20   fn level() {\n\
                    \x20       #[export_name = \"two\"] #[no_mangle] extern \"C\" fn deep() {}\n\
                    \x20       #[cfg(windows)] fn gone() { #[no_mangle] fn hidden() {} }\n\
                    \x20   }\n\
                    \x20   extern \"C\" { fn one(); }\n\
                    \x20   struct S;\n\
                    \x20   impl S { #[no_mangle] fn in_impl() {} }\n\
                    }\n";
        let top = Top { text, name: "outer", library: None };
        let alloc = blaze_own::testing::heap().alloc();
        let walked = walk(top, Reading { edition: Edition::E2021, atoms: MAC, alloc }).unwrap_or_else(|e| panic!("refused: {e}"));
        let mut symbols: std::vec::Vec<&str> = walked.exports.iter().map(|e| e.symbol).collect();
        symbols.sort_unstable();
        assert_eq!(symbols, ["in_impl", "one", "two"]);
        let deep = walked.exports.iter().find(|e| e.symbol == "two").unwrap_or_else(|| panic!("no `two`"));
        let path = walked.path(deep.within, deep.name, alloc).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(path.as_str(), "outer::level::deep");
        assert!(walked.exports.iter().all(|e| e.key == Err(Unkeyed::NoLibrary) || e.key == Err(Unkeyed::InContainer)));
    }
}
