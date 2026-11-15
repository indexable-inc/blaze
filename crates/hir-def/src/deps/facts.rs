//! What a dependent reads of an item once its name is found, as entries of that item (AGENTS.md
//! rule 65; docs/v2/plan/design/restore.md, "Items read in place", rows B, C and E): the value
//! of an attribute (`#[link_name]`, `#[unstable(feature)]`, `#[target_feature]`, `#[cfg]`, ..),
//! and the facts of an item's header that were read off its syntax (an enum's unsafe fields, a
//! receiver's view, an extern block's ABI, a written visibility, an `extern crate`'s name, a
//! macro's name).
//!
//! An entry is the answer of the query it stands before, under a key of the item's own source
//! hashes (its row of its file's index, `hir_expand::declarative::stored::index`), the family,
//! how its file is read and, where the hashed text names `cfg`, its crate's cfg. The query asks
//! here first: found, nothing is parsed (the file's index is found by the file's content, the
//! entry by its key, a few dozen bytes read into the reader's frame); not found, the query runs
//! as it always did, off its file's outline, and its answer is kept. So a sealed crate's items
//! answer from their entries, a workspace crate's from the outline the first time and from
//! their entries after, and a body edit moves no key here (no part hashed is a body).
//!
//! | part | what the answer reads | families |
//! |---|---|---|
//! | `attrs` | the item's attributes alone | every attribute value |
//! | `header` | the item with its bodies cut | `unsafe fields`, `self view`, `extern abi`, `visibility`, `extern crate name`, `macro name` |
//!
//! Only what is a function of that key is kept: an item written outside any body in a file or
//! in an expansion with a key (`stored::expansion::key`, the expansion's inputs; its index is
//! found by that key with no expansion), and of it only an answer that read nothing beside its
//! text: an attribute value a macro wrote (`#[doc = include_str!(..)]`, `#[export_name =
//! concat!(..)]`) reads which macro its path names, and is made again each time; a name with a
//! hygiene key, a path naming `$crate` or a context that is not its file's root is not kept. A
//! module's attributes are its declaration's, its file's and, of a crate root, the command
//! line's: not one row, so no module is kept here.
//!
//! The values name nothing of a database or a file: symbols as their text, an ABI as its
//! place among `ExternAbi::ALL_VARIANTS`, a set in its text order (rule 25).
//!
//! The floor of a found fact: the file's index, found once a file per process (one store
//! read), one hash of under a hundred bytes, one entry read of the value's bytes. That is the
//! cost here; nothing else is read. The floor of a miss is the query's own run, and this adds
//! one hash and one put.
//!
//! The invariant: equal keys are equal bytes; a found fact is the answer the query makes.

use base_db::{Crate, Db};
use blaze_ids::{ContentId, Hasher};
use cfg::{CfgAtom, CfgExpr};
use hir_expand::{
    HirFileId, Lookup,
    declarative::stored::{self, index::ItemSource},
    mod_path::{ModPath, PathKind},
    name::Name,
};
use intern::Symbol;
use rustc_abi::ExternAbi;
use span::{AstIdNode, Edition, ErasedFileAstId, SyntaxContext};

use crate::{
    AdtId, AstIdLoc, AttrDefId, HasModule, MacroId,
    item_tree::{RawVisibility, VisibilityExplicitness},
    nameres::frozen::{Unreadable, entries},
    signatures::WrittenSelfView,
};

#[cfg(test)]
mod tests;

type R<T = ()> = Result<T, String>;

/// The layout of every fact's value: part of every key, so an entry of another layout is never
/// found (a miss, made again), never read as this one.
pub const LAYOUT: u32 = 1;

const DOMAIN: &str = "blaze v2 item fact";

/// Which hash of an item's row a fact is keyed by.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Part {
    /// Its attributes and doc comments: an attribute's value.
    Attrs,
    /// The item with its bodies cut (covers `Attrs`): a fact of its header.
    Header,
}

/// Where an item is written: its file, its ast id there, and the crate it is read for.
#[derive(Clone, Copy, Debug)]
pub(crate) struct At {
    pub(crate) file: HirFileId,
    pub(crate) ast: ErasedFileAstId,
    pub(crate) krate: Crate,
}

/// An item with a row in its file's index or its expansion's (keyed by the expansion's
/// inputs, `stored::expansion::key`), in a database that keeps entries.
struct Place {
    at: At,
    /// The set its entries are packed in: its file's, or its expansion's key.
    set: ContentId,
    /// How its text is read: its file's `read_as`, or `stored::expansion::EXPANDED`.
    read_as: u8,
    /// The edition whose root context a kept value's contexts are: its file's, or its crate's
    /// for an expansion (whose names carry other contexts, and are not kept).
    edition: Edition,
    source: ItemSource,
}

/// What a fact's value is written and read with beside its bytes: what the file is read as,
/// which a value's contexts are the root of.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Ctx {
    pub(crate) edition: Edition,
}

/// The place of the item `lookup` names.
pub(crate) fn at<N: AstIdNode>(
    db: &Db,
    lookup: impl Lookup<Data = impl AstIdLoc<Ast = N> + HasModule> + Copy,
) -> At {
    let loc = lookup.lookup(db);
    let id = loc.ast_id();
    At { file: id.file_id, ast: id.value.erase(), krate: loc.krate(db) }
}

/// The kind of an attribute owner in a key, and where it is written: `None` for a module.
pub(crate) fn owner_at(db: &Db, owner: AttrDefId) -> Option<(u8, At)> {
    Some(match owner {
        AttrDefId::ModuleId(_) => return None,
        AttrDefId::AdtId(AdtId::StructId(it)) => (1, at(db, it)),
        AttrDefId::AdtId(AdtId::UnionId(it)) => (2, at(db, it)),
        AttrDefId::AdtId(AdtId::EnumId(it)) => (3, at(db, it)),
        AttrDefId::FunctionId(it) => (4, at(db, it)),
        AttrDefId::EnumVariantId(it) => (5, at(db, it)),
        AttrDefId::StaticId(it) => (6, at(db, it)),
        AttrDefId::ConstId(it) => (7, at(db, it)),
        AttrDefId::TraitId(it) => (8, at(db, it)),
        AttrDefId::TypeAliasId(it) => (9, at(db, it)),
        AttrDefId::MacroId(MacroId::MacroRulesId(it)) => (10, at(db, it)),
        AttrDefId::MacroId(MacroId::Macro2Id(it)) => (11, at(db, it)),
        AttrDefId::MacroId(MacroId::ProcMacroId(it)) => (12, at(db, it)),
        AttrDefId::ImplId(it) => (13, at(db, it)),
        AttrDefId::ExternBlockId(it) => (14, at(db, it)),
        AttrDefId::ExternCrateId(it) => (15, at(db, it)),
        AttrDefId::UseId(it) => (16, at(db, it)),
    })
}

fn place(db: &Db, at: At) -> Option<Place> {
    entries::of(db)?;
    let source = stored::index::source(db, at.file, at.ast, at.krate)?;
    let (set, read_as) = stored::set_of(db, at.file)?;
    let edition = match at.file {
        HirFileId::FileId(file) => file.edition(db),
        HirFileId::MacroFile(_) => at.krate.data(db).edition,
    };
    Some(Place { at, set, read_as, edition, source })
}

fn key(db: &Db, family: &str, what: &str, kind: u8, place: &Place, part: Part) -> ContentId {
    #[cfg(test)]
    let layout = LAYOUT_AS.get().unwrap_or(LAYOUT);
    #[cfg(not(test))]
    let layout = LAYOUT;
    let mut hasher = Hasher::new(DOMAIN);
    hasher
        .update(&layout.to_le_bytes())
        .update(&(family.len() as u32).to_le_bytes())
        .update(family.as_bytes())
        .update(&(what.len() as u32).to_le_bytes())
        .update(what.as_bytes())
        .update(&[kind, place.read_as]);
    let (hash, cfg) = match part {
        Part::Attrs => (place.source.attrs, place.source.attrs_cfg),
        Part::Header => (place.source.header, place.source.header_cfg),
    };
    hasher.update(&hash.to_le_bytes());
    if cfg {
        hasher.update(&[1]).id(&base_db::cfg_options_content(place.at.krate.cfg_options(db)));
    } else {
        hasher.update(&[0]);
    }
    #[cfg(test)]
    if KEYED_BY_FILE.get() {
        hasher.id(&place.set);
    }
    hasher.finish()
}

// A test's controls. `LIVE`: with it on, no fact is looked up or kept, so each reader goes to
// its file's syntax as before this module (the control of the no-parse count). `KEYED_BY_FILE`:
// a key takes its file's set, as a key of a file or a crate would, and an edit anywhere in the
// file misses every fact of it. `RAN`: how many times a fact's query ran its own work for an
// item that has a place. `LAYOUT_AS`: the layout a key is made with in place of [`LAYOUT`], so
// a test writes the entries of a previous layout.
#[cfg(test)]
thread_local! {
    pub(super) static LAYOUT_AS: std::cell::Cell<Option<u32>> = const { std::cell::Cell::new(None) };
    pub(super) static LIVE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    pub(super) static KEYED_BY_FILE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    pub(super) static RAN: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

fn live() -> bool {
    #[cfg(test)]
    if LIVE.get() {
        return true;
    }
    false
}

fn ran() {
    #[cfg(test)]
    RAN.set(RAN.get() + 1);
}

/// A fact's value as its entry holds it.
pub(crate) trait Value: Sized {
    /// Appends the value, or says `false` when it holds something of a file or a database
    /// (a hygiene key, `$crate`, a context not the file's root): such a value is not kept.
    fn put(&self, ctx: Ctx, out: &mut Out) -> bool;
    fn take(ctx: Ctx, input: &mut In<'_>) -> R<Self>;
}

/// The answer of the query `family` (with `what`, the part of it a shared family names, as
/// the attribute's name) of the item at `at`, of the kind `kind`: its entry where one is kept,
/// else `make`'s, kept where the item has a place and `make` says its answer read nothing but
/// the hashed part.
pub(crate) fn fact<T: Value>(
    db: &Db,
    family: &'static str,
    what: &str,
    kind: u8,
    at: Option<At>,
    part: Part,
    make: impl FnOnce() -> (T, bool),
) -> T {
    let Some(place) = at.filter(|_| !live()).and_then(|at| place(db, at)) else {
        return make().0;
    };
    let ctx = Ctx { edition: place.edition };
    let key = key(db, family, what, kind, &place, part);
    let read = |bytes: &[u8]| {
        let mut input = In(bytes);
        let value = T::take(ctx, &mut input)?;
        if !input.0.is_empty() {
            // alloc: error path
            return Err(format!("{} bytes after the value", input.0.len()));
        }
        Ok(value)
    };
    match entries::entry(db, family, &place.set, key, read) {
        Ok(Some(found)) => return found,
        Ok(None) => {}
        Err(why) => refuse(db, &place, family, what, why),
    }
    ran();
    let (value, keep) = make();
    if keep {
        // alloc: one value's bytes, a few dozen, while it is kept
        let mut out = Out(Vec::new());
        if value.put(ctx, &mut out)
            && let Err(why) = entries::keep(db, family, &place.set, key, &out.0)
        {
            refuse(db, &place, family, what, why)
        }
    }
    value
}

/// The family of every attribute's value: `what` names the attribute.
pub(crate) const ATTR: &str = "attr value";

/// [`fact`] of the attribute `what` of `owner`, keyed by its attributes alone: a module's is
/// made each time ([`owner_at`]).
pub(crate) fn of_attrs<T: Value>(
    db: &Db,
    what: &str,
    owner: AttrDefId,
    make: impl FnOnce() -> (T, bool),
) -> T {
    match owner_at(db, owner) {
        Some((kind, at)) => fact(db, ATTR, what, kind, Some(at), Part::Attrs, make),
        None => make().0,
    }
}

/// Refuses the fact of `family` of the item at `place`, naming the item by its file and ast
/// id and the fact (rule 19).
fn refuse(db: &Db, place: &Place, family: &'static str, what: &str, why: String) -> ! {
    let krate = place.at.krate;
    // alloc: error path
    let written = match place.at.file {
        HirFileId::FileId(file) => {
            let file = file.file_id(db);
            db.path_for_file(file).map_or_else(|| format!("{file:?}"), |it| it.to_string())
        }
        HirFileId::MacroFile(call) => format!("the expansion {call:?}"),
    };
    // alloc: error path
    let why = format!("the {family} fact `{what}` of the item {:?} of {written}: {why}", place.at.ast);
    // alloc: an error's crate name, once
    let name = krate.extra_data(db).display_name.as_ref().map(|it| it.crate_name().to_string());
    Unreadable { krate, name, kind: family, why }.throw()
}

/// A value being written.
pub(crate) struct Out(Vec<u8>);

impl Out {
    pub(crate) fn u8(&mut self, it: u8) {
        self.0.push(it);
    }

    pub(crate) fn u32(&mut self, it: u32) {
        self.0.extend_from_slice(&it.to_le_bytes());
    }

    pub(crate) fn str(&mut self, it: &str) {
        self.u32(it.len() as u32);
        self.0.extend_from_slice(it.as_bytes());
    }

    /// A name with no hygiene key: `false` for one with a key.
    pub(crate) fn name(&mut self, it: &Name) -> bool {
        if it.key().is_some() {
            return false;
        }
        self.str(it.symbol().as_str());
        true
    }

    #[cfg(test)]
    pub(crate) fn bytes(&self) -> &[u8] {
        &self.0
    }

    #[cfg(test)]
    pub(crate) fn new() -> Out {
        Out(Vec::new())
    }
}

/// What is left of a value being read.
pub(crate) struct In<'a>(pub(crate) &'a [u8]);

impl<'a> In<'a> {
    fn take_bytes(&mut self, len: usize) -> R<&'a [u8]> {
        if self.0.len() < len {
            // alloc: error path
            return Err(format!("{} bytes where {len} are needed", self.0.len()));
        }
        let (taken, rest) = self.0.split_at(len);
        self.0 = rest;
        Ok(taken)
    }

    pub(crate) fn u8(&mut self) -> R<u8> {
        Ok(self.take_bytes(1)?[0])
    }

    pub(crate) fn u32(&mut self) -> R<u32> {
        Ok(u32::from_le_bytes(self.take_bytes(4)?.try_into().expect("four bytes")))
    }

    pub(crate) fn str(&mut self) -> R<&'a str> {
        let len = self.u32()? as usize;
        std::str::from_utf8(self.take_bytes(len)?).map_err(|_| "text that is not UTF-8".into())
    }

    pub(crate) fn symbol(&mut self) -> R<Symbol> {
        Ok(Symbol::intern(self.str()?))
    }

    pub(crate) fn name(&mut self) -> R<Name> {
        Ok(Name::new_symbol_root(self.symbol()?))
    }

    /// A count of things each at least `least` bytes long: refused when the bytes left
    /// cannot hold them, so a bad count allocates nothing.
    fn count(&mut self, least: usize) -> R<usize> {
        let count = self.u32()? as usize;
        if count.saturating_mul(least) > self.0.len() {
            // alloc: error path
            return Err(format!("{count} things in {} bytes", self.0.len()));
        }
        Ok(count)
    }
}

fn tag_error(what: &str, tag: u8) -> String {
    // alloc: error path
    format!("{what} of tag {tag}")
}

impl Value for bool {
    fn put(&self, _: Ctx, out: &mut Out) -> bool {
        out.u8(u8::from(*self));
        true
    }

    fn take(_: Ctx, input: &mut In<'_>) -> R<bool> {
        match input.u8()? {
            0 => Ok(false),
            1 => Ok(true),
            tag => Err(tag_error("a bool", tag)),
        }
    }
}

impl Value for u8 {
    fn put(&self, _: Ctx, out: &mut Out) -> bool {
        out.u8(*self);
        true
    }

    fn take(_: Ctx, input: &mut In<'_>) -> R<u8> {
        input.u8()
    }
}

impl Value for Symbol {
    fn put(&self, _: Ctx, out: &mut Out) -> bool {
        out.str(self.as_str());
        true
    }

    fn take(_: Ctx, input: &mut In<'_>) -> R<Symbol> {
        input.symbol()
    }
}

impl Value for Name {
    fn put(&self, _: Ctx, out: &mut Out) -> bool {
        out.name(self)
    }

    fn take(_: Ctx, input: &mut In<'_>) -> R<Name> {
        input.name()
    }
}

impl Value for Box<str> {
    fn put(&self, _: Ctx, out: &mut Out) -> bool {
        out.str(self);
        true
    }

    fn take(_: Ctx, input: &mut In<'_>) -> R<Box<str>> {
        // alloc: the memo's text
        Ok(Box::from(input.str()?))
    }
}

impl<T: Value> Value for Option<T> {
    fn put(&self, ctx: Ctx, out: &mut Out) -> bool {
        match self {
            None => {
                out.u8(0);
                true
            }
            Some(it) => {
                out.u8(1);
                it.put(ctx, out)
            }
        }
    }

    fn take(ctx: Ctx, input: &mut In<'_>) -> R<Option<T>> {
        match input.u8()? {
            0 => Ok(None),
            1 => Ok(Some(T::take(ctx, input)?)),
            tag => Err(tag_error("an option", tag)),
        }
    }
}

impl<T: Value> Value for Box<[T]> {
    fn put(&self, ctx: Ctx, out: &mut Out) -> bool {
        out.u32(self.len() as u32);
        self.iter().all(|it| it.put(ctx, out))
    }

    fn take(ctx: Ctx, input: &mut In<'_>) -> R<Box<[T]>> {
        let count = input.count(1)?;
        // alloc: the memo's list, exact
        let mut out = Vec::with_capacity(count);
        for _ in 0..count {
            out.push(T::take(ctx, input)?);
        }
        Ok(out.into_boxed_slice())
    }
}

impl Value for u32 {
    fn put(&self, _: Ctx, out: &mut Out) -> bool {
        out.u32(*self);
        true
    }

    fn take(_: Ctx, input: &mut In<'_>) -> R<u32> {
        input.u32()
    }
}

/// A set of symbols, written in their text's order so one set is one value (rule 25).
impl Value for blaze_hash::HashSet<Symbol, std::alloc::Global> {
    fn put(&self, _: Ctx, out: &mut Out) -> bool {
        // alloc: the set's texts, sorted once as the value is written
        let mut texts: Vec<&str> = self.iter().map(Symbol::as_str).collect();
        texts.sort_unstable();
        out.u32(texts.len() as u32);
        for text in texts {
            out.str(text);
        }
        true
    }

    fn take(_: Ctx, input: &mut In<'_>) -> R<Self> {
        let count = input.count(4)?;
        // alloc: the memo's set, exact
        let mut set = blaze_hash::HashSet::with_capacity_in(count, std::alloc::Global);
        for _ in 0..count {
            set.insert(input.symbol()?);
        }
        Ok(set)
    }
}

impl Value for CfgExpr {
    fn put(&self, ctx: Ctx, out: &mut Out) -> bool {
        match self {
            CfgExpr::Invalid => out.u8(0),
            CfgExpr::Atom(CfgAtom::Flag(flag)) => {
                out.u8(1);
                out.str(flag.as_str());
            }
            CfgExpr::Atom(CfgAtom::KeyValue { key, value }) => {
                out.u8(2);
                out.str(key.as_str());
                out.str(value.as_str());
            }
            CfgExpr::All(all) | CfgExpr::Any(all) => {
                out.u8(if matches!(self, CfgExpr::All(_)) { 3 } else { 4 });
                out.u32(all.len() as u32);
                return all.iter().all(|it| it.put(ctx, out));
            }
            CfgExpr::Not(it) => {
                out.u8(5);
                return it.put(ctx, out);
            }
        }
        true
    }

    fn take(ctx: Ctx, input: &mut In<'_>) -> R<CfgExpr> {
        Ok(match input.u8()? {
            0 => CfgExpr::Invalid,
            1 => CfgExpr::Atom(CfgAtom::Flag(input.symbol()?)),
            2 => {
                let key = input.symbol()?;
                let value = input.symbol()?;
                CfgExpr::Atom(CfgAtom::KeyValue { key, value })
            }
            tag @ (3 | 4) => {
                let all = <Box<[CfgExpr]>>::take(ctx, input)?;
                if tag == 3 { CfgExpr::All(all) } else { CfgExpr::Any(all) }
            }
            // alloc: the memo's expression
            5 => CfgExpr::Not(Box::new(CfgExpr::take(ctx, input)?)),
            tag => return Err(tag_error("a cfg", tag)),
        })
    }
}

impl Value for ExternAbi {
    fn put(&self, _: Ctx, out: &mut Out) -> bool {
        out.u8(self.as_packed());
        true
    }

    fn take(_: Ctx, input: &mut In<'_>) -> R<ExternAbi> {
        let at = input.u8()?;
        ExternAbi::ALL_VARIANTS.get(usize::from(at)).copied().ok_or_else(|| tag_error("an ABI", at))
    }
}

impl Value for WrittenSelfView {
    fn put(&self, ctx: Ctx, out: &mut Out) -> bool {
        out.u8(u8::from(self.mutable));
        self.fields.put(ctx, out)
    }

    fn take(ctx: Ctx, input: &mut In<'_>) -> R<WrittenSelfView> {
        let mutable = bool::take(ctx, input)?;
        Ok(WrittenSelfView { fields: <Box<[Name]>>::take(ctx, input)?, mutable })
    }
}

fn explicitness(it: VisibilityExplicitness) -> u8 {
    match it {
        VisibilityExplicitness::Explicit => 0,
        VisibilityExplicitness::Implicit => 1,
    }
}

fn explicitness_of(tag: u8) -> R<VisibilityExplicitness> {
    match tag {
        0 => Ok(VisibilityExplicitness::Explicit),
        1 => Ok(VisibilityExplicitness::Implicit),
        tag => Err(tag_error("an explicitness", tag)),
    }
}

/// A visibility as written in a real file: a `pub(in path)` whose path and first token are of
/// the file's root context. One that names `$crate` or another context is not kept.
impl Value for RawVisibility {
    fn put(&self, ctx: Ctx, out: &mut Out) -> bool {
        match self {
            RawVisibility::Module(path, explicit, context) => {
                if *context != SyntaxContext::root(ctx.edition) || path.contexts().is_some() {
                    return false;
                }
                match path.kind {
                    PathKind::Plain => out.u8(0),
                    PathKind::Super(n) => {
                        out.u8(1);
                        out.u8(n);
                    }
                    PathKind::Crate => out.u8(2),
                    PathKind::Abs => out.u8(3),
                    PathKind::DollarCrate(_) => return false,
                }
                out.u8(explicitness(*explicit));
                out.u32(path.segments().len() as u32);
                path.segments().iter().all(|it| it.put(ctx, out))
            }
            RawVisibility::PubSelf(explicit) => {
                out.u8(4);
                out.u8(explicitness(*explicit));
                true
            }
            RawVisibility::PubCrate => {
                out.u8(5);
                true
            }
            RawVisibility::Public => {
                out.u8(6);
                true
            }
        }
    }

    fn take(ctx: Ctx, input: &mut In<'_>) -> R<RawVisibility> {
        let path = |kind: PathKind, input: &mut In<'_>| -> R<RawVisibility> {
            let explicit = explicitness_of(input.u8()?)?;
            let segments = <Box<[Name]>>::take(ctx, input)?;
            let path = ModPath::from_segments(kind, segments.into_vec());
            Ok(RawVisibility::Module(path, explicit, SyntaxContext::root(ctx.edition)))
        };
        Ok(match input.u8()? {
            0 => path(PathKind::Plain, input)?,
            1 => {
                let n = input.u8()?;
                path(PathKind::Super(n), input)?
            }
            2 => path(PathKind::Crate, input)?,
            3 => path(PathKind::Abs, input)?,
            4 => RawVisibility::PubSelf(explicitness_of(input.u8()?)?),
            5 => RawVisibility::PubCrate,
            6 => RawVisibility::Public,
            tag => return Err(tag_error("a visibility", tag)),
        })
    }
}

/// A field's or an item's flags: 16 bytes, low word first. A bit no flag names is refused.
impl Value for crate::attrs::AttrFlags {
    fn put(&self, _: Ctx, out: &mut Out) -> bool {
        let bits = self.bits();
        for word in 0..4 {
            out.u32((bits >> (32 * word)) as u32);
        }
        true
    }

    fn take(_: Ctx, input: &mut In<'_>) -> R<crate::attrs::AttrFlags> {
        let mut bits = 0u128;
        for word in 0..4 {
            bits |= u128::from(input.u32()?) << (32 * word);
        }
        crate::attrs::AttrFlags::from_bits(bits).ok_or_else(|| {
            // alloc: error path
            format!("attribute flags {bits:#x} with a bit no flag names")
        })
    }
}

impl<A: Value, B: Value> Value for (A, B) {
    fn put(&self, ctx: Ctx, out: &mut Out) -> bool {
        self.0.put(ctx, out) && self.1.put(ctx, out)
    }

    fn take(ctx: Ctx, input: &mut In<'_>) -> R<(A, B)> {
        let a = A::take(ctx, input)?;
        Ok((a, B::take(ctx, input)?))
    }
}

/// A path as written, of root names: one with `$crate` or a hygiene context is not kept.
impl Value for ModPath {
    fn put(&self, ctx: Ctx, out: &mut Out) -> bool {
        if self.contexts().is_some() {
            return false;
        }
        match self.kind {
            PathKind::Plain => out.u8(0),
            PathKind::Super(n) => {
                out.u8(1);
                out.u8(n);
            }
            PathKind::Crate => out.u8(2),
            PathKind::Abs => out.u8(3),
            PathKind::DollarCrate(_) => return false,
        }
        out.u32(self.segments().len() as u32);
        self.segments().iter().all(|it| it.put(ctx, out))
    }

    fn take(ctx: Ctx, input: &mut In<'_>) -> R<ModPath> {
        let kind = match input.u8()? {
            0 => PathKind::Plain,
            1 => PathKind::Super(input.u8()?),
            2 => PathKind::Crate,
            3 => PathKind::Abs,
            tag => return Err(tag_error("a path kind", tag)),
        };
        let segments = <Box<[Name]>>::take(ctx, input)?;
        Ok(ModPath::from_segments(kind, segments.into_vec()))
    }
}

fn integer_tag(it: rustc_abi::Integer) -> u8 {
    use rustc_abi::Integer::*;
    match it {
        I8 => 0,
        I16 => 1,
        I32 => 2,
        I64 => 3,
        I128 => 4,
    }
}

fn integer_of(tag: u8) -> R<rustc_abi::Integer> {
    use rustc_abi::Integer::*;
    Ok(match tag {
        0 => I8,
        1 => I16,
        2 => I32,
        3 => I64,
        4 => I128,
        tag => return Err(tag_error("an integer", tag)),
    })
}

impl Value for rustc_abi::Align {
    fn put(&self, _: Ctx, out: &mut Out) -> bool {
        // An alignment is a power of two: its exponent, below 64.
        out.u8(self.bytes().trailing_zeros() as u8);
        true
    }

    fn take(_: Ctx, input: &mut In<'_>) -> R<rustc_abi::Align> {
        let pow2 = input.u8()?;
        let bytes = 1u64.checked_shl(u32::from(pow2)).ok_or_else(|| tag_error("an alignment", pow2))?;
        rustc_abi::Align::from_bytes(bytes).map_err(|_| tag_error("an alignment", pow2))
    }
}

/// A `#[repr(..)]` and `#[rustc_scalable_vector]` as written. The layout seed is never written
/// by an attribute: a value carrying one is not kept, and a value read has the default.
impl Value for rustc_abi::ReprOptions {
    fn put(&self, ctx: Ctx, out: &mut Out) -> bool {
        use rustc_abi::{IntegerType, ReprOptions, ScalableElt};
        let written = ReprOptions {
            field_shuffle_seed: ReprOptions::default().field_shuffle_seed,
            ..*self
        };
        if written != *self {
            return false;
        }
        match self.int {
            None => out.u8(0),
            Some(IntegerType::Pointer(signed)) => {
                out.u8(1);
                out.u8(u8::from(signed));
            }
            Some(IntegerType::Fixed(int, signed)) => {
                out.u8(2);
                out.u8(integer_tag(int));
                out.u8(u8::from(signed));
            }
        }
        if !self.align.put(ctx, out) || !self.pack.put(ctx, out) {
            return false;
        }
        out.u8(self.flags.bits());
        match self.scalable {
            None => out.u8(0),
            Some(ScalableElt::ElementCount(n)) => {
                out.u8(1);
                out.u32(u32::from(n));
            }
            Some(ScalableElt::Container) => out.u8(2),
        }
        true
    }

    fn take(ctx: Ctx, input: &mut In<'_>) -> R<rustc_abi::ReprOptions> {
        use rustc_abi::{IntegerType, ReprFlags, ReprOptions, ScalableElt};
        let int = match input.u8()? {
            0 => None,
            1 => Some(IntegerType::Pointer(bool::take(ctx, input)?)),
            2 => {
                let int = integer_of(input.u8()?)?;
                Some(IntegerType::Fixed(int, bool::take(ctx, input)?))
            }
            tag => return Err(tag_error("a repr integer", tag)),
        };
        // The trait's `take`: `Option::take` names the inherent `Option::take(&mut self)`.
        let align = <Option<rustc_abi::Align> as Value>::take(ctx, input)?;
        let pack = <Option<rustc_abi::Align> as Value>::take(ctx, input)?;
        let bits = input.u8()?;
        let flags = ReprFlags::from_bits(bits).ok_or_else(|| tag_error("repr flags", bits))?;
        let scalable = match input.u8()? {
            0 => None,
            1 => {
                let n = input.u32()?;
                let n = u16::try_from(n).map_err(|_| tag_error("a scalable count", 1))?;
                Some(ScalableElt::ElementCount(n))
            }
            2 => Some(ScalableElt::Container),
            tag => return Err(tag_error("a scalable vector", tag)),
        };
        Ok(ReprOptions { int, align, pack, flags, scalable, ..ReprOptions::default() })
    }
}

impl Value for crate::attrs::NativeLibKind {
    fn put(&self, _: Ctx, out: &mut Out) -> bool {
        use crate::attrs::NativeLibKind::*;
        match self {
            Unspecified => out.u8(0),
            Dylib => out.u8(1),
            Static => out.u8(2),
            Framework => out.u8(3),
            RawDylib => out.u8(4),
            Other(kind) => {
                out.u8(5);
                out.str(kind.as_str());
            }
        }
        true
    }

    fn take(_: Ctx, input: &mut In<'_>) -> R<crate::attrs::NativeLibKind> {
        use crate::attrs::NativeLibKind::*;
        Ok(match input.u8()? {
            0 => Unspecified,
            1 => Dylib,
            2 => Static,
            3 => Framework,
            4 => RawDylib,
            5 => Other(input.symbol()?),
            tag => return Err(tag_error("a library kind", tag)),
        })
    }
}

impl Value for crate::attrs::NativeLib {
    fn put(&self, ctx: Ctx, out: &mut Out) -> bool {
        self.name.put(ctx, out)
            && self.kind.put(ctx, out)
            && self.verbatim.put(ctx, out)
            && self.whole_archive.put(ctx, out)
            && self.bundle.put(ctx, out)
            && self.as_needed.put(ctx, out)
            && self.import_name_type.put(ctx, out)
    }

    fn take(ctx: Ctx, input: &mut In<'_>) -> R<crate::attrs::NativeLib> {
        Ok(crate::attrs::NativeLib {
            name: Symbol::take(ctx, input)?,
            kind: Value::take(ctx, input)?,
            verbatim: bool::take(ctx, input)?,
            whole_archive: bool::take(ctx, input)?,
            bundle: bool::take(ctx, input)?,
            as_needed: <Option<bool> as Value>::take(ctx, input)?,
            import_name_type: <Option<Symbol> as Value>::take(ctx, input)?,
        })
    }
}

/// Asks every per-item entry of `krate` once, so each is kept: what `deps::store` does for a
/// crate it images, in the database that resolved it from source (docs/v2/plan/design/
/// restore.md, "Written when"). Each item's flags (`entries::items`), each impl's and trait's
/// assoc items, and each fact here, through the queries themselves: an answer a database
/// holds already is not made again, and one it lacks is made off the parse it holds. What a
/// dependent asks of the crate later is then found, with no parse.
///
/// Asked through the same queries a dependent asks through, so the set kept is the set read:
/// a fact a query adds is kept here once it is asked here, and a fact asked by a dependent and
/// not here is made again by that dependent (a miss, never wrong: rule 21), which the bar test
/// of this module counts.
pub(crate) fn keep_all(db: &Db, krate: Crate) {
    use crate::{
        AssocItemId, ModuleDefId,
        attrs::AttrFlags,
        nameres::{
            assoc::{ImplItems, TraitItems},
            crate_def_map,
        },
        signatures::{
            EnumVariants, enum_has_unsafe_fields, extern_block_abi, written_self_view,
            written_visibility,
        },
    };
    use either::Either;

    let _span =
        tracing::info_span!(target: "blaze", "item entries kept", krate = %krate.named(db))
            .entered();
    // Every attribute value whose flag says the item has it, as a dependent asks it.
    let attrs = |owner: AttrDefId| {
        let flags = AttrFlags::query(db, owner);
        _ = AttrFlags::lang_item(db, owner);
        _ = AttrFlags::link_name(db, owner);
        _ = AttrFlags::diagnostic_item(db, owner);
        _ = AttrFlags::link_section(db, owner);
        _ = AttrFlags::linkage(db, owner);
        _ = AttrFlags::export_name(db, owner);
        _ = AttrFlags::must_use_message(db, owner);
        _ = flags.unstable_feature(db, owner);
        _ = flags.cfgs(db, Either::Left(owner));
    };
    let function = |it: crate::FunctionId| {
        let owner = AttrDefId::FunctionId(it);
        attrs(owner);
        let flags = AttrFlags::query(db, owner);
        _ = AttrFlags::target_features(db, it);
        if flags.contains(AttrFlags::HAS_LEGACY_CONST_GENERICS) {
            _ = AttrFlags::legacy_const_generic_indices(db, it);
        }
        _ = AttrFlags::define_opaque(db, it);
        _ = crate::attrs::disabled_sanitizers(db, it);
        _ = written_self_view(db, it);
        _ = written_visibility(db, it.lookup(db).ast_id(), it.module(db));
    };
    let assoc = |items: &[(Name, AssocItemId)]| {
        for &(_, item) in items {
            match item {
                AssocItemId::FunctionId(it) => function(it),
                AssocItemId::ConstId(it) => {
                    attrs(AttrDefId::ConstId(it));
                    _ = written_visibility(db, it.lookup(db).ast_id(), it.module(db));
                }
                AssocItemId::TypeAliasId(it) => {
                    attrs(AttrDefId::TypeAliasId(it));
                    _ = written_visibility(db, it.lookup(db).ast_id(), it.module(db));
                }
            }
        }
    };
    // A variant's field flags, one memo of every field: asked through its first.
    let fields = |parent: crate::VariantId| {
        let local_id = crate::LocalFieldId::from_raw(la_arena::RawIdx::from_u32(0));
        _ = AttrFlags::query_field(db, crate::FieldId { parent, local_id });
    };
    let def_map = crate_def_map(db, krate);
    for (module, data) in def_map.modules(db) {
        // An item in a block is the block's body's, made with it (lane records-bodies).
        if module.block(db).is_some() {
            continue;
        }
        let scope = data.scope(db);
        for def in scope.declarations() {
            match def {
                ModuleDefId::FunctionId(it) => function(it),
                ModuleDefId::AdtId(it) => {
                    attrs(AttrDefId::AdtId(it));
                    _ = AttrFlags::repr(db, it);
                    _ = match it {
                        AdtId::StructId(it) => {
                            fields(crate::VariantId::StructId(it));
                            written_visibility(db, it.lookup(db).ast_id(), module)
                        }
                        AdtId::UnionId(it) => {
                            fields(crate::VariantId::UnionId(it));
                            written_visibility(db, it.lookup(db).ast_id(), module)
                        }
                        AdtId::EnumId(it) => {
                            _ = enum_has_unsafe_fields(db, it);
                            for &(variant, _) in EnumVariants::of(db, it).0.variants.values() {
                                attrs(AttrDefId::EnumVariantId(variant));
                                fields(crate::VariantId::EnumVariantId(variant));
                            }
                            written_visibility(db, it.lookup(db).ast_id(), module)
                        }
                    };
                }
                ModuleDefId::ConstId(it) => {
                    attrs(AttrDefId::ConstId(it));
                    _ = written_visibility(db, it.lookup(db).ast_id(), it.module(db));
                }
                ModuleDefId::StaticId(it) => {
                    attrs(AttrDefId::StaticId(it));
                    _ = written_visibility(db, it.lookup(db).ast_id(), it.module(db));
                }
                ModuleDefId::TraitId(it) => {
                    attrs(AttrDefId::TraitId(it));
                    _ = AttrFlags::must_implement_one_of(db, it);
                    _ = written_visibility(db, it.lookup(db).ast_id(), module);
                    assoc(&TraitItems::query_with_diagnostics(db, it).0.items);
                }
                ModuleDefId::TypeAliasId(it) => {
                    attrs(AttrDefId::TypeAliasId(it));
                    _ = written_visibility(db, it.lookup(db).ast_id(), it.module(db));
                }
                ModuleDefId::MacroId(it) => {
                    attrs(AttrDefId::MacroId(it));
                    _ = it.name(db);
                }
                // A module's attributes are its declaration's and its file's: not one row
                // (this module's doc); a variant is asked with its enum.
                ModuleDefId::ModuleId(_)
                | ModuleDefId::EnumVariantId(_)
                | ModuleDefId::BuiltinType(_) => {}
            }
        }
        for (_, macros) in scope.legacy_macros() {
            for &it in macros {
                attrs(AttrDefId::MacroId(it));
                _ = it.name(db);
            }
        }
        for it in scope.unnamed_consts() {
            attrs(AttrDefId::ConstId(it));
        }
        for it in scope.impls() {
            attrs(AttrDefId::ImplId(it));
            assoc(&ImplItems::of(db, it).0.items);
        }
        for it in scope.extern_crate_decls() {
            attrs(AttrDefId::ExternCrateId(it));
            _ = it.alias_or_name(db);
        }
        for it in scope.extern_blocks() {
            attrs(AttrDefId::ExternBlockId(it));
            _ = extern_block_abi(db, it);
            _ = AttrFlags::wasm_import_module(db, it);
            _ = AttrFlags::native_libs(db, it);
        }
    }
}
