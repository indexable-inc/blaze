//! Where the front end's questions about names are answered from: the ONE seam between front2
//! and the names library (rule 78: the strategy is a type parameter, each instantiation tested
//! equal by tests/front2_records.rs).
//!
//! [`NamesSource`] has the questions of the table in the port notes
//! (blaze-int-1112-notes/port27/per-crate-entry.md, "Runner twenty-one", part 1), and two
//! answers:
//!
//! - [`Solved`] (`&Names`): every crate's names resolved in this command, in memory. Each method
//!   is the expression front2 wrote at its call sites before this file, moved here unchanged.
//! - [`Records`]: each crate's NAMES record ([`RecordNames`]: a module's scope decoded when a
//!   path walks through it) and ITEMS record ([`RecordItems`]: an item's row by key, its text
//!   read from its file and checked against the row's digest), each crate opened the first time
//!   a question is about it. Nothing is read when a `Records` is made.
//!
//! A function of front2 is switched by taking `names: S` with `S: NamesSource<'n>` in place of
//! `names: &'n Names<'n>`: its callers that still hold a `&Names` keep compiling (`&Names` IS a
//! source), and it can only call functions that were switched too, so "does the records path
//! reach this code" is a compile-time fact. A switched function that must still call an
//! unswitched one asks [`NamesSource::solved`] and, over records, notes a [`SourceError::Gap`]
//! naming itself.
//!
//! What `Records` cannot answer is never an empty answer taken for a fact: the method answers
//! `None` AND files a typed [`SourceError`] in its [`Log`] (no record for crate `x`; the record
//! did not read; the file changed; a question no record holds yet). A command over records
//! reads the log after each body ([`Records::errors`]) and, when it is not empty, prints those
//! INSTEAD of the body's result: the result of a body checked over a logged error is void.

use core::cell::{Cell, Ref, RefCell};

use blaze_hash::HashMap;
use blaze_id::{CrateItemKey, ItemKey};
use blaze_own::{AVec, Alloc};
use blaze_scope::items_record::{ImplRow, ItemsDecoded, ItemsRecord, ItemsRecordError, LoadItems, RecordItems};
use blaze_scope::names::{self, PathError, Resolved, Why, Written};
use blaze_scope::names_record::{Decoded, LoadNames, NamesRecord, NamesRecordError, RecordNames};
use blaze_scope::package_path::PackagePath;
use blaze_scope::resolve::{Names, Place};
use blaze_scope::task::{ItemText, MacroBody, MacroSrc, Variant};
use blaze_surface::{Def, Edition, ItemDetail, ItemIndex, ItemKind, Namespace};

mod item_grain;

use super::index::ImplAt;

/// The names as this command solved them: the first [`NamesSource`].
pub type Solved<'n> = &'n Names<'n>;

/// A member of a trait or an impl, as either source lists it (question 13).
#[derive(Clone, Copy, Debug)]
pub struct MemberOf<'n> {
    pub key: ItemKey,
    pub name: Option<&'n str>,
    /// Its entry in the outline of the text it is in.
    pub item: ItemIndex,
    pub kind: ItemKind,
    pub body_holds_items: bool,
    /// Its outline entry's detail. Over records it is made from `kind` and `body_holds_items`
    /// ([`detail_from`]); `None` for a kind whose detail holds ranges no record has (a macro
    /// call's path and input).
    pub detail: Option<ItemDetail>,
    /// Its whole text, attributes included.
    pub text: &'n str,
    /// The call whose expansion it is in; `None` when written in its owner's body.
    pub written_by: Option<ItemKey>,
}

/// The outline detail of an item of `kind`, for the kinds whose detail is its kind and the one
/// flag the items record holds. `None` for a kind whose detail has ranges of its text (`mod`'s
/// `#[path]`, `use`'s tree, a macro call's path and input, ..): those are the file's own
/// `FileItems` record's, not the items record's (question 11).
pub fn detail_from(kind: ItemKind, body_holds_items: bool) -> Option<ItemDetail> {
    Some(match kind {
        ItemKind::Function => ItemDetail::Fn { body_holds_items },
        ItemKind::Struct => ItemDetail::Struct,
        ItemKind::Enum => ItemDetail::Enum,
        ItemKind::Union => ItemDetail::Union,
        ItemKind::Trait => ItemDetail::Trait,
        ItemKind::TraitAlias => ItemDetail::TraitAlias,
        ItemKind::Impl => ItemDetail::Impl,
        ItemKind::Const => ItemDetail::Const { body_holds_items },
        ItemKind::Static => ItemDetail::Static { body_holds_items },
        ItemKind::TypeAlias => ItemDetail::TypeAlias,
        ItemKind::Macro2 => ItemDetail::MacroDef,
        _ => return None,
    })
}

/// Whether an outline detail says its body holds items (the flag the items record keeps).
fn holds_items(detail: ItemDetail) -> bool {
    matches!(detail, ItemDetail::Fn { body_holds_items: true } | ItemDetail::Const { body_holds_items: true } | ItemDetail::Static { body_holds_items: true })
}

/// Where front2's questions about names are answered from. `Copy`: a source is a reference.
///
/// Every answer that borrows lives for `'n` (the command). `None` from [`Solved`] means what it
/// meant at the call site the expression came from; `None` from [`Records`] means that, or an
/// error filed in the log (module docs).
pub trait NamesSource<'n>: Copy {
    /// The solved names, for a caller of a function not yet switched to a source; `None` over
    /// records (the caller then notes [`NamesSource::gap`] naming itself and refuses).
    fn solved(self) -> Option<&'n Names<'n>>;

    /// Notes that `question` (about `key`) has no answer from this source. Nothing over
    /// [`Solved`], which answers everything.
    fn gap(self, question: &'static str, key: Option<ItemKey>);

    /// How many crates the program has (index 0 is the program's own).
    fn crate_count(self) -> usize;

    /// Question 4: the crate of a key, by its prefix. Opens nothing.
    fn crate_of(self, key: ItemKey) -> Option<usize>;

    /// Question 22: a crate's key.
    fn crate_key(self, krate: usize) -> Option<CrateItemKey>;

    /// A crate's root module.
    fn root_module(self, krate: usize) -> Option<ItemKey>;

    /// Question 5: a crate's edition.
    fn edition_of(self, krate: usize) -> Option<Edition>;

    /// Question 6: a crate's cfg atoms, sorted, as `CrateCfg` takes them.
    fn cfg_atoms(self, krate: usize) -> Option<&'n [(&'n str, Option<&'n str>)]>;

    /// Question 17: whether the crate is a proc-macro unit (its impls answer no question of the
    /// program's).
    fn proc_macro(self, krate: usize) -> Option<bool>;

    /// Question 16: whether the crate may write incoherent impls (its root text names
    /// `rustc_attrs` or `rustc_coherence_is_core`).
    fn may_write_incoherent(self, krate: usize) -> Option<bool>;

    /// Question 1: what `written` means in `module`, looked up in `ns`.
    fn walk(self, module: ItemKey, ns: Namespace, written: Written<'_, &str>) -> Result<Resolved, PathError>;

    /// Question 2: the traits method resolution may use in `module`, sorted.
    fn traits_in_scope<'b>(self, module: ItemKey, alloc: Alloc<'b>) -> Result<AVec<'b, ItemKey>, PathError>;

    /// Question 3: an enum's variants, in declaration order.
    fn variants(self, owner: ItemKey) -> Option<&'n [Variant<&'n str>]>;

    /// The enum a variant is of and its index there (`Resolver::variant_of`).
    fn variant_of(self, variant: ItemKey) -> Option<(ItemKey, u32)>;

    /// Whether `key` is an item with text (of a file or an expansion); a variant is not.
    fn has_text(self, key: ItemKey) -> bool;

    /// Questions 7, 8, 9: an item's whole text, its module, its name, its impl header.
    fn text_of(self, key: ItemKey) -> Option<ItemText<'n>>;

    /// Question 9: an item's name. Reads no text.
    fn name_of(self, key: ItemKey) -> Option<&'n str>;

    /// Question 8: the module an item is declared in. Reads no text.
    fn home_of(self, key: ItemKey) -> Option<ItemKey>;

    /// Question 10: an item's place.
    fn place_of(self, key: ItemKey) -> Option<Place>;

    /// Question 11, the kind: what an item's outline entry is, without its detail's data.
    fn kind_of_item(self, key: ItemKey) -> Option<ItemKind>;

    /// Question 11, the detail (`Resolver::detail`).
    fn detail_of(self, key: ItemKey) -> Option<ItemDetail>;

    /// Question 12: whether a struct is its own constructor (a tuple or unit struct).
    fn is_ctor(self, key: ItemKey) -> bool;

    /// Question 13: the members of the trait or impl `owner`, in declaration order, each given
    /// to `each`. `false`: the owner has no member row (a container with no member has none).
    fn each_member(self, owner: ItemKey, each: &mut dyn FnMut(MemberOf<'n>)) -> bool;

    /// The trait or impl a member is of, and the member's name (`Resolver::member_of`).
    fn member_owner(self, member: ItemKey) -> Option<(ItemKey, &'n str)>;

    /// Question 14: the impls written in crate `krate`, each given to `each` (in no promised
    /// order: the caller sorts by key).
    fn each_impl(self, krate: usize, each: &mut dyn FnMut(ImplAt<'n>));

    /// Question 14 from ROWS: the impls crate `krate` writes, each given to `each` as its row
    /// of the items record (what an impl index files it by: the trait's path, the self type's
    /// head, both as written; no text is opened). `false`: this source has no impl rows (the
    /// solved names: [`NamesSource::each_impl`] gives each impl with its header's text) and
    /// `each` was not called.
    fn each_impl_row(self, krate: usize, each: &mut dyn FnMut(ImplRow<'n>)) -> bool;

    /// Whether an impl question is answered BY BUCKET (`buckets::Buckets::admit_asked`): the
    /// source files each crate's impls by resolved (trait or inherent, self type's head), so
    /// one question reads one bucket of each crate asked and no other impl's row. `false`
    /// over the solved names, where every impl is filed when the index is made.
    fn has_buckets(self) -> bool {
        false
    }

    /// Each impl crate `krate` files under `key` (a whole bucket, or a prefix: every negative
    /// impl of a trait), and the digest of that answer (what a body's read list holds for the
    /// (crate, bucket) it asked). `None`: no bucket index (the solved names), or no record of
    /// the crate, or a record that did not read (its error is filed).
    fn bucket(self, _krate: usize, _key: &blaze_scope::items_record::buckets::BucketKey, _each: &mut dyn FnMut(blaze_scope::items_record::buckets::BucketImpl)) -> Option<blaze_id::ContentId> {
        None
    }

    /// Each impl of crate `krate` its record's writer filed under NO bucket, as its impl row
    /// with why, and the digest of the list: candidates of every question the crate is asked,
    /// filed by the reader. `None` as [`NamesSource::bucket`].
    fn unfiled(self, _krate: usize, _each: &mut dyn FnMut(ImplRow<'n>, blaze_scope::items_record::buckets::NotFiled)) -> Option<blaze_id::ContentId> {
        None
    }

    /// THE CRATES THAT MAY WRITE AN INCOHERENT INHERENT IMPL (an inherent impl of a type they
    /// do not define: `impl bool` in core, `impl [T]` in alloc, `impl f64` in std), as ONE
    /// fact of the command with its digest, read once and never by opening every crate's
    /// record. `None`: the source was not told which crates to ask (a caller then files a
    /// gap by name).
    fn incoherent_crates(self) -> Option<(&'n [usize], blaze_id::ContentId)> {
        None
    }

    /// The impl `key` of crate `krate`, written in `module`, with its header's text: for the
    /// one impl a row does not file ([`NamesSource::each_impl_row`]'s reader). `None`: the
    /// text did not read (filed in the log).
    fn impl_with_header(self, krate: usize, key: ItemKey, module: ItemKey) -> Option<ImplAt<'n>>;

    /// Question 15: the lang items crate `krate` declares, each given to `each`.
    fn each_lang(self, krate: usize, each: &mut dyn FnMut(&'n str, ItemKey));

    /// Question 21, the body: the definition the macro path `written` in `module` names
    /// (`None`: a definition with no source record).
    fn macro_body_of(self, module: ItemKey, written: Written<'_, &str>) -> Result<Option<MacroBody<'n>>, PathError>;

    /// Question 21, whole: the item the macro path `written` in `module` names, its lowered
    /// source and its defining crate's facts ([`MacroAt`]): what `blaze_scope::resolve::
    /// definition_of` makes a definition from ([`super::scoped::ScopedBy`]).
    fn macro_at(self, module: ItemKey, written: Written<'_, &str>) -> Result<MacroAt<'n>, PathError>;

    /// Question 20: a module's path as a finding shows it (`krate::a::b`). `None`: the module
    /// has no path (a block's module), or its crate is not read.
    fn module_path(self, module: ItemKey) -> Option<&'n str>;

    /// Every item of crate `krate` that has text (question 7's rows), each given to `each` as
    /// (key, the module its names are read in), in key order. `false`: the crate is not read.
    /// Opens the crate's items record and reads every item row: for a whole-crate walk (the
    /// run path's reach, a census), never for one body.
    fn each_item(self, krate: usize, each: &mut dyn FnMut(ItemKey, ItemKey)) -> bool;

    /// Whether `module` (a module, a block's module or an enum) binds `name` in `ns`: its own
    /// binding, else through its glob edges (`Names::lookup`). Asked for a struct, enum, union
    /// or alias declared in a block of a body ([`super::locals`]).
    fn binds(self, module: ItemKey, ns: Namespace, name: &'n str) -> bool;

    /// The item, module or variant whose key begins with `first` (a checker key is a library
    /// key's first 16 bytes, `keys::checker_of`): the inverse the key map asks where it holds
    /// no row ([`super::keys::Keys::library_in`]). Over the solved names always `None`: there
    /// the key map is filled when the front end is made (`Front2::new` files every item,
    /// variant, module and impl), so a key in no row is no library item's. Over records the
    /// key map starts empty and this is one search of the key's crate (found by the key's
    /// first eight bytes): its item rows, then its module rows, then its enums' variants. Two
    /// keys of one table that begin alike are a typed error filed, and `None`.
    fn key_beginning(self, first: [u8; 16]) -> Option<ItemKey>;
}

/// What a macro path names ([`NamesSource::macro_at`]).
#[derive(Clone, Copy, Debug)]
pub struct MacroAt<'n> {
    /// The defining crate's index.
    pub krate: usize,
    pub def: Def,
    /// Its source as lowered; `None` for a macro with no source row (a procedural macro named
    /// through a crate whose record lists none, or one not read).
    pub src: Option<MacroSrc<'n>>,
    /// The defining crate's key (its `$crate`), its proc-macro runner's key and its edition.
    pub crate_key: CrateItemKey,
    pub proc_key: Option<CrateItemKey>,
    pub edition: Edition,
}

// ---------------------------------------------------------------------------------------------
// Solved: each method is the expression front2 wrote, moved.

impl<'n> NamesSource<'n> for &'n Names<'n> {
    fn solved(self) -> Option<&'n Names<'n>> {
        Some(self)
    }

    fn gap(self, _question: &'static str, _key: Option<ItemKey>) {}

    fn crate_count(self) -> usize {
        self.crates().len()
    }

    fn crate_of(self, key: ItemKey) -> Option<usize> {
        Names::crate_of(self, key)
    }

    fn crate_key(self, krate: usize) -> Option<CrateItemKey> {
        self.crates().get(krate).map(|c| c.key)
    }

    fn root_module(self, krate: usize) -> Option<ItemKey> {
        self.crates().get(krate).map(|c| c.root)
    }

    fn edition_of(self, krate: usize) -> Option<Edition> {
        self.crates().get(krate).map(|c| c.edition)
    }

    fn cfg_atoms(self, krate: usize) -> Option<&'n [(&'n str, Option<&'n str>)]> {
        let c: &'n blaze_scope::resolve::CrateNames<'n> = self.crates().get(krate).copied()?;
        Some(c.cfg.atoms())
    }

    fn proc_macro(self, krate: usize) -> Option<bool> {
        self.crates().get(krate).map(|c| c.implicit.proc_macro)
    }

    fn may_write_incoherent(self, krate: usize) -> Option<bool> {
        // `#![feature(rustc_attrs)]` is an inner attribute of the crate root, the first file
        // read; a mention anywhere in that text keeps the crate on the wide side.
        self.crates().get(krate).map(|c| c.files.first().is_none_or(|f| f.text.contains("rustc_attrs") || f.text.contains("rustc_coherence_is_core")))
    }

    fn walk(self, module: ItemKey, ns: Namespace, written: Written<'_, &str>) -> Result<Resolved, PathError> {
        names::path(self, module, ns, written, Alloc::global())
    }

    fn traits_in_scope<'b>(self, module: ItemKey, alloc: Alloc<'b>) -> Result<AVec<'b, ItemKey>, PathError> {
        Names::traits_in_scope(self, module, alloc)
    }

    fn variants(self, owner: ItemKey) -> Option<&'n [Variant<&'n str>]> {
        let c: &'n blaze_scope::resolve::CrateNames<'n> = self.krate(Names::crate_of(self, owner)?);
        c.tables.enums.get(&owner).map(|vs| &vs[..])
    }

    fn variant_of(self, variant: ItemKey) -> Option<(ItemKey, u32)> {
        let c = Names::crate_of(self, variant)?;
        self.krate(c).tables.enums.iter().find_map(|(e, vs)| vs.iter().position(|v| v.key == variant).map(|i| (*e, i as u32)))
    }

    fn has_text(self, key: ItemKey) -> bool {
        self.place(key).is_some()
    }

    fn text_of(self, key: ItemKey) -> Option<ItemText<'n>> {
        self.item_text(key).copied()
    }

    fn name_of(self, key: ItemKey) -> Option<&'n str> {
        self.item_text(key).and_then(|t| t.name)
    }

    fn home_of(self, key: ItemKey) -> Option<ItemKey> {
        self.module_of(key)
    }

    fn place_of(self, key: ItemKey) -> Option<Place> {
        self.place(key)
    }

    fn kind_of_item(self, key: ItemKey) -> Option<ItemKind> {
        self.detail_of(key).map(|d| d.kind())
    }

    fn detail_of(self, key: ItemKey) -> Option<ItemDetail> {
        let Place { krate, at, item } = self.place(key)?;
        let (_, outline) = self.outline_at(krate, at)?;
        Some(outline.item(item).detail)
    }

    fn is_ctor(self, key: ItemKey) -> bool {
        self.ctor(key).is_some()
    }

    fn each_member(self, owner: ItemKey, each: &mut dyn FnMut(MemberOf<'n>)) -> bool {
        let Some(c) = Names::crate_of(self, owner) else { return false };
        let c: &'n blaze_scope::resolve::CrateNames<'n> = self.krate(c);
        let Some(ms) = c.tables.members.get(&owner) else { return false };
        for m in ms.iter() {
            each(MemberOf {
                key: m.key,
                name: m.name,
                item: m.item,
                kind: m.detail.kind(),
                body_holds_items: holds_items(m.detail),
                detail: Some(m.detail),
                text: m.text,
                written_by: m.written_by,
            });
        }
        true
    }

    fn member_owner(self, member: ItemKey) -> Option<(ItemKey, &'n str)> {
        let c = Names::crate_of(self, member)?;
        self.krate(c).tables.members.iter().find_map(|(owner, ms)| ms.iter().find(|m| m.key == member).and_then(|m| m.name.map(|n| (*owner, n))))
    }

    fn each_impl(self, krate: usize, each: &mut dyn FnMut(ImplAt<'n>)) {
        let Some(c) = self.crates().get(krate).copied() else { return };
        for i in c.tables.impls.iter() {
            each(ImplAt { krate, key: i.key, module: i.module, header: i.header });
        }
    }

    fn each_impl_row(self, _krate: usize, _each: &mut dyn FnMut(ImplRow<'n>)) -> bool {
        // The solved names hold each impl's header text, not a row: `each_impl`.
        false
    }

    fn impl_with_header(self, krate: usize, key: ItemKey, _module: ItemKey) -> Option<ImplAt<'n>> {
        let c = self.crates().get(krate).copied()?;
        c.tables.impls.iter().find(|i| i.key == key).map(|i| ImplAt { krate, key: i.key, module: i.module, header: i.header })
    }

    fn each_lang(self, krate: usize, each: &mut dyn FnMut(&'n str, ItemKey)) {
        let Some(c) = self.crates().get(krate).copied() else { return };
        for l in c.tables.langs.iter() {
            each(l.name, l.key);
        }
    }

    fn macro_body_of(self, module: ItemKey, written: Written<'_, &str>) -> Result<Option<MacroBody<'n>>, PathError> {
        let found = self.item_at(module, Namespace::Macros, written, Alloc::global())?;
        Ok(found.macro_src.map(|s| s.body))
    }

    fn macro_at(self, module: ItemKey, written: Written<'_, &str>) -> Result<MacroAt<'n>, PathError> {
        let found = self.item_at(module, Namespace::Macros, written, Alloc::global())?;
        let owner: &'n blaze_scope::resolve::CrateNames<'n> = self.krate(found.krate);
        Ok(MacroAt { krate: found.krate, def: found.def, src: found.macro_src, crate_key: owner.key, proc_key: owner.proc_key, edition: owner.edition })
    }

    fn module_path(self, module: ItemKey) -> Option<&'n str> {
        let c: &'n blaze_scope::resolve::CrateNames<'n> = self.krate(Names::crate_of(self, module)?);
        c.path_of(module)
    }

    fn each_item(self, krate: usize, each: &mut dyn FnMut(ItemKey, ItemKey)) -> bool {
        if krate >= self.crates().len() {
            return false;
        }
        // The texts table has no order of its own: sorted here, so both sources give one order.
        let mut rows: AVec<'static, (ItemKey, ItemKey)> = AVec::new_in(Alloc::global());
        rows.extend(self.items(krate).map(|(key, module, _)| (key, module)));
        rows.sort_unstable_by_key(|&(key, _)| key);
        for &(key, module) in rows.iter() {
            each(key, module);
        }
        true
    }

    fn binds(self, module: ItemKey, ns: Namespace, name: &'n str) -> bool {
        self.lookup(module, ns, name).is_some()
    }

    fn key_beginning(self, _first: [u8; 16]) -> Option<ItemKey> {
        // `Front2::new` filed every item, variant, module and impl of every crate: a checker
        // key the key map holds no row for is no library item's.
        None
    }
}

// ---------------------------------------------------------------------------------------------
// Records.

/// Which of a crate's two records.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Which {
    Names,
    Items,
}

/// Why the records did not answer a question. Filed in the [`Log`]; never an empty answer.
#[derive(Debug)]
pub enum SourceError<'n> {
    /// No record of this kind is kept for the crate.
    NoRecord { krate: usize, name: &'n str, which: Which },
    /// The crate's items record did not read, or a text it points into is gone or changed.
    Items { krate: usize, name: &'n str, error: ItemsRecordError },
    /// The crate's names record did not read.
    Names { krate: usize, name: &'n str, error: NamesRecordError },
    /// A question no record answers yet (the names library's next piece), or a function of
    /// front2 not yet switched to a source: `question` names it.
    Gap { question: &'static str, key: Option<ItemKey> },
    /// Two keys of one table of the crate's records begin with the same 16 bytes: a checker
    /// key (a library key's first 16 bytes) would stand for both (`keys::KeyClash`, as the
    /// records path meets it: [`NamesSource::key_beginning`]).
    KeysBeginAlike { krate: usize, name: &'n str, first: ItemKey, second: ItemKey },
    /// The item's file changed since its items record was written and the item's HEAD moved
    /// (`found`: today's head) or no single entry of today's text has its chain (`found:
    /// None`): this item's row is stale and no text of it is answered (item_grain.rs). The
    /// crate's scope is remade by its own key.
    ItemStale { krate: usize, name: &'n str, key: ItemKey, file: u32, saw: blaze_id::ContentId, found: Option<blaze_id::ContentId> },
}

impl core::fmt::Display for SourceError<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            SourceError::NoRecord { krate, name, which: Which::Names } => write!(f, "no names record for crate `{name}` (crate {krate})"),
            SourceError::NoRecord { krate, name, which: Which::Items } => write!(f, "no items record for crate `{name}` (crate {krate})"),
            SourceError::Items { krate, name, error } => write!(f, "the items record of crate `{name}` (crate {krate}): {error:?}"),
            SourceError::Names { krate, name, error } => write!(f, "the names record of crate `{name}` (crate {krate}): {error:?}"),
            SourceError::Gap { question, key: Some(key) } => write!(f, "no record answers yet: {question} (asked of {key:x})"),
            SourceError::Gap { question, key: None } => write!(f, "no record answers yet: {question}"),
            SourceError::KeysBeginAlike { krate, name, first, second } => {
                write!(f, "the records of crate `{name}` (crate {krate}) hold two keys that begin with the same 16 bytes, {first:x} and {second:x}: one checker key would stand for both")
            }
            SourceError::ItemStale { krate, name, key, file, saw, found: Some(found) } => {
                write!(f, "item {key:x} of crate `{name}` (crate {krate}), file {file}: its head changed since its items record was written (saw {saw:?}, today {found:?}): its crate's scope is remade by its own key")
            }
            SourceError::ItemStale { krate, name, key, file, saw, found: None } => {
                write!(f, "item {key:x} of crate `{name}` (crate {krate}), file {file}: no single entry of the file as it is now has its place by names (head {saw:?} when written): its crate's scope is remade by its own key")
            }
        }
    }
}

/// The most errors a log keeps (rule 18); the rest are counted ([`Log::dropped`]).
pub const MOST_ERRORS: usize = 64;

/// What the records could not answer in this command, in the order met.
pub struct Log<'n> {
    /// Each crate's name, by index: what an error names.
    crate_names: &'n [&'n str],
    errors: RefCell<AVec<'n, SourceError<'n>>>,
    dropped: Cell<u32>,
}

impl<'n> Log<'n> {
    pub fn new(crate_names: &'n [&'n str], alloc: Alloc<'n>) -> Log<'n> {
        Log { crate_names, errors: RefCell::new(AVec::with_capacity_in(8, alloc)), dropped: Cell::new(0) }
    }

    fn name(&self, krate: usize) -> &'n str {
        self.crate_names.get(krate).copied().unwrap_or("(a crate with no name given)")
    }

    fn put(&self, error: SourceError<'n>) {
        let mut errors = self.errors.borrow_mut();
        if errors.len() < MOST_ERRORS {
            errors.push(error);
        } else {
            self.dropped.set(self.dropped.get().saturating_add(1));
        }
    }

    /// The errors kept, in the order met.
    pub fn errors(&self) -> Ref<'_, AVec<'n, SourceError<'n>>> {
        self.errors.borrow()
    }

    /// How many errors past [`MOST_ERRORS`] were met and not kept.
    pub fn dropped(&self) -> u32 {
        self.dropped.get()
    }

    pub fn is_empty(&self) -> bool {
        self.errors.borrow().is_empty()
    }
}

/// Where a crate's two records and the texts they point into come from: the caller's effect
/// layer (kept files, the store). Asked the first time each is needed.
pub trait Load<'n> {
    /// The names record of crate `krate`, 16-aligned; `None` when none is kept.
    fn names(&self, krate: usize) -> Option<&'n [u8]>;
    /// The items record of crate `krate`, 16-aligned; `None` when none is kept.
    fn items(&self, krate: usize) -> Option<&'n [u8]>;
    /// The text of the crate's file at `path` in its package as it is now (the record checks
    /// its digest): the loader joins `path` onto the directory its world placed the crate's
    /// package at (D96).
    fn file(&self, krate: usize, path: PackagePath<'_>) -> Option<&'n str>;
    /// The text the macro call `call` of the crate expanded to.
    fn expansion(&self, krate: usize, call: ItemKey) -> Option<&'n str>;
    /// The entry of the crate's file at `path`, as it is now, whose item chain is `chain`: what
    /// an items record row whose file changed is checked against, item by item
    /// (`blaze_scope::items_record::ItemsRecord::item_now`). This one outlines the file's text
    /// now; a caller that holds the file's `FileItems` record answers from it instead.
    fn item(&self, krate: usize, path: PackagePath<'_>, chain: blaze_id::ContentId, edition: Edition, alloc: Alloc<'n>) -> Option<blaze_scope::items_record::ItemToday<'n>> {
        blaze_scope::items_record::grain::outline_today(self.file(krate, path)?, chain, edition, alloc)
    }
}

/// The loader the two readers hold: the caller's, with "no record kept" filed in the log.
#[derive(Clone, Copy)]
pub struct Loader<'n> {
    load: &'n dyn Load<'n>,
    log: &'n Log<'n>,
}

impl<'n> LoadNames<'n> for Loader<'n> {
    fn load(&self, krate: usize) -> Option<&'n [u8]> {
        let bytes = self.load.names(krate);
        if bytes.is_none() {
            self.log.put(SourceError::NoRecord { krate, name: self.log.name(krate), which: Which::Names });
        }
        bytes
    }
}

impl<'n> LoadItems<'n> for Loader<'n> {
    fn load(&self, krate: usize) -> Option<&'n [u8]> {
        let bytes = self.load.items(krate);
        if bytes.is_none() {
            self.log.put(SourceError::NoRecord { krate, name: self.log.name(krate), which: Which::Items });
        }
        bytes
    }

    fn file(&self, krate: usize, path: PackagePath<'_>) -> Option<&'n str> {
        self.load.file(krate, path)
    }

    fn expansion(&self, krate: usize, call: ItemKey) -> Option<&'n str> {
        self.load.expansion(krate, call)
    }

    fn item(&self, krate: usize, path: PackagePath<'_>, chain: blaze_id::ContentId, edition: Edition, alloc: Alloc<'n>) -> Option<blaze_scope::items_record::ItemToday<'n>> {
        self.load.item(krate, path, chain, edition, alloc)
    }
}

/// What the records path read so far: the measure tests/front2_records.rs prints.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Opened {
    pub names: Decoded,
    pub items: ItemsDecoded,
}

/// Every crate's names through its two records: the second [`NamesSource`] (as `&Records`).
pub struct Records<'n> {
    paths: RecordNames<'n, Loader<'n>>,
    items: RecordItems<'n, Loader<'n>>,
    keys: &'n [CrateItemKey],
    log: &'n Log<'n>,
    /// The command's frame: a path's segments are copied into it for the walk (the names
    /// record's `World` is over `&'n str`, and a caller's segments live less long).
    frame: &'n blaze_mem::Frame,
    /// What [`NamesSource::key_beginning`] answered, by the 16 bytes asked: one search per key
    /// per command.
    beginnings: RefCell<HashMap<[u8; 16], Option<ItemKey>, Alloc<'n>>>,
    /// A test's planted bug: the crate whose impl rows are withheld ([`Records::plant_no_impls`]).
    no_impls: Cell<Option<usize>>,
    /// A test's planted bug: the impl whose bucket row is withheld
    /// ([`Records::plant_no_bucket_impl`]).
    no_bucket_impl: Cell<Option<ItemKey>>,
    /// The crates that may write incoherent impls and the fact's digest
    /// ([`Records::name_lang_crates`]); `None` until the lang crates are named.
    incoherent: Cell<Option<(&'n [usize], blaze_id::ContentId)>>,
    /// A test's planted bug: the item whose row is withheld ([`Records::plant_no_item`]).
    no_item: Cell<Option<ItemKey>>,
}

impl<'n> Records<'n> {
    /// `keys`: each crate's key by index (index 0 the program's own), as `load` and `log` know
    /// the crates. Nothing is read here.
    pub fn new(keys: &'n [CrateItemKey], load: &'n dyn Load<'n>, log: &'n Log<'n>, frame: &'n blaze_mem::Frame, alloc: Alloc<'n>) -> Records<'n> {
        let loader = Loader { load, log };
        Records {
            paths: RecordNames::new(keys, loader, alloc),
            items: RecordItems::new(keys, loader, alloc),
            keys,
            log,
            frame,
            beginnings: RefCell::new(HashMap::new_in(alloc)),
            no_impls: Cell::new(None),
            no_bucket_impl: Cell::new(None),
            incoherent: Cell::new(None),
            no_item: Cell::new(None),
        }
    }

    /// PLANTED BUG, for tests/front2_records.rs only: the item row of `key` is withheld from
    /// [`NamesSource::text_of`] from now on, as an items record written without that row would
    /// read. A body that needs the item's head must then differ from its check over the solved
    /// names.
    #[doc(hidden)]
    pub fn plant_no_item(&self, key: ItemKey) {
        self.no_item.set(Some(key));
    }

    /// What could not be answered so far. Not empty: the result of whatever was checked over
    /// these records since the log was last read is void (module docs).
    pub fn errors(&self) -> Ref<'_, AVec<'n, SourceError<'n>>> {
        self.log.errors()
    }

    pub fn log(&self) -> &'n Log<'n> {
        self.log
    }

    /// The whole text `at` of crate `krate`, as the loader has it today and the items record
    /// proves it (its digest): what a body's chain is computed in
    /// (`project_records::BodySource`). `None` with the reason filed in the log.
    ///
    /// The WHOLE text's digest is what this asks: a file with any byte changed is refused.
    /// A reader that reads the items in a file one by one asks [`Records::file_now`]
    /// (item_grain.rs) instead.
    pub fn whole_text(&'n self, krate: usize, at: blaze_scope::resolve::At) -> Option<&'n str> {
        self.items_of(krate)?;
        self.read(krate, self.items.whole(krate, at))
    }

    /// What was opened and decoded so far.
    pub fn opened(&self) -> Opened {
        Opened { names: self.paths.decoded(), items: self.items.decoded() }
    }

    /// PLANTED BUG, for tests/front2_records.rs only: crate `krate`'s impl rows are withheld
    /// from [`NamesSource::each_impl`] from now on, as a record written without them would
    /// read. A question whose answer is an impl of that crate must then differ from the solved
    /// names' answer.
    #[doc(hidden)]
    pub fn plant_no_impls(&self, krate: usize) {
        self.no_impls.set(Some(krate));
    }

    fn items_of(&'n self, krate: usize) -> Option<&'n ItemsRecord<'n>> {
        // No record: the loader filed `NoRecord` the one time it was asked.
        self.items.record(krate)
    }

    fn names_of(&'n self, krate: usize) -> Option<&'n NamesRecord<'n>> {
        self.paths.record(krate)
    }

    /// An items record's answer, its error filed.
    fn read<T>(&self, krate: usize, answer: Result<T, ItemsRecordError>) -> Option<T> {
        match answer {
            Ok(t) => Some(t),
            Err(error) => {
                self.log.put(SourceError::Items { krate, name: self.log.name(krate), error });
                None
            }
        }
    }

    /// A names record's answer, its error filed.
    fn read_names<T>(&self, krate: usize, answer: Result<T, NamesRecordError>) -> Option<T> {
        match answer {
            Ok(t) => Some(t),
            Err(error) => {
                self.log.put(SourceError::Names { krate, name: self.log.name(krate), error });
                None
            }
        }
    }

    /// `written` with its segments copied into the command's frame, so they live as long as
    /// the names records' `World` wants (`RecordNames` is invariant in its lifetime).
    fn kept<'w>(&self, written: Written<'w, &str>) -> Written<'n, &'n str> {
        let frame: &'n blaze_mem::Frame = self.frame;
        let segments: &'n [&'n str] = frame.alloc_slice_fill_with(written.segments.len(), |i| {
            let s: &'n str = frame.alloc_str(written.segments[i]);
            s
        });
        Written { kind: written.kind, segments }
    }

    /// In a table of `count` rows sorted by key (`key_at(i)` is row `i`'s key, `None` where the
    /// row did not read: its error is filed), the one key that begins with `first`.
    fn one_beginning(&self, krate: usize, first: [u8; 16], count: usize, key_at: &dyn Fn(usize) -> Option<ItemKey>) -> Beginning {
        // The first row whose key's first 16 bytes are not below `first`.
        let (mut lo, mut hi) = (0usize, count);
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            let Some(key) = key_at(mid) else { return Beginning::Void };
            if first_16(key) < first {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        if lo >= count {
            return Beginning::Absent;
        }
        let Some(key) = key_at(lo) else { return Beginning::Void };
        if first_16(key) != first {
            return Beginning::Absent;
        }
        // The rows are sorted: a second key beginning alike is the next row.
        if lo + 1 < count {
            let Some(next) = key_at(lo + 1) else { return Beginning::Void };
            if first_16(next) == first {
                self.log.put(SourceError::KeysBeginAlike { krate, name: self.log.name(krate), first: key, second: next });
                return Beginning::Void;
            }
        }
        Beginning::One(key)
    }

    /// [`NamesSource::key_beginning`], not kept: the key's crate by its first eight bytes (no
    /// record is opened to find it), then that crate's item rows, its module rows, and its
    /// variant table (`blaze_scope::items_record::variants`, sorted by variant key): three
    /// binary searches at most, no crate scanned.
    fn find_beginning(&'n self, first: [u8; 16]) -> Option<ItemKey> {
        let mut whole = [0u8; 32];
        whole[..16].copy_from_slice(&first);
        let krate = self.items.crate_of(ItemKey(blaze_id::ContentId(whole)))?;
        let record = self.items_of(krate)?;
        let item_at = |i: usize| self.read(krate, record.item_at(i)).flatten().map(|row| row.key);
        match self.one_beginning(krate, first, record.items(), &item_at) {
            Beginning::One(key) => return Some(key),
            Beginning::Void => return None,
            Beginning::Absent => {}
        }
        let module_at = |i: usize| self.read(krate, record.module_at(i)).flatten().map(|row| row.key);
        match self.one_beginning(krate, first, record.modules(), &module_at) {
            Beginning::One(key) => return Some(key),
            Beginning::Void => return None,
            Beginning::Absent => {}
        }
        let variant_at = |i: usize| self.read(krate, record.variant_at(i)).flatten().map(|row| row.0);
        match self.one_beginning(krate, first, record.variant_rows(), &variant_at) {
            Beginning::One(key) => Some(key),
            Beginning::Void | Beginning::Absent => None,
        }
    }
}

impl<'n> Records<'n> {
    /// Names the crates that can carry the library's coherence exceptions (`lang_crates`: core,
    /// alloc, std and the program's own, as the driver's crate graph names them: the ones
    /// `Front2::from_records` reads the lang rows of) and reads, from THEIR items records only,
    /// which may write an incoherent inherent impl ([`NamesSource::incoherent_crates`]): one
    /// read for the whole graph, kept for the command; a second call changes nothing.
    ///
    /// Why those crates: an inherent impl of a type its crate does not define is E0116/E0390
    /// (the Reference, "Inherent implementations": "An implementing type must be defined
    /// within the same crate as the original type definition"), lifted only by the library's
    /// internal attributes. In the library's source on the toolchain shelf
    /// (`rustlib/src/rust/library`, read 2026-11-16): `#![rustc_coherence_is_core]` is written
    /// in core/src/lib.rs alone, and `#[rustc_allow_incoherent_impl]` in core (23 places),
    /// alloc (44) and std (141), in no other library crate (proc_macro, test, panic_unwind,
    /// panic_abort: 0, though their roots turn on `rustc_attrs`).
    ///
    /// THE GAP THIS LEAVES, by name: a crate NOT in `lang_crates` whose root turns on
    /// `#![feature(rustc_attrs)]` and writes `#[rustc_allow_incoherent_impl]` (nightly only,
    /// outside the library) is not asked, so its incoherent impls are no candidates. The
    /// crate-graph row should carry the flag (asked of the driver's lane); until it does the
    /// fact is exactly "of the lang crates named, which carry the mark".
    pub fn name_lang_crates(&'n self, lang_crates: &[usize]) {
        if self.incoherent.get().is_some() {
            return;
        }
        let marked = |krate: usize| self.items_of(krate).is_some_and(|record| record.may_write_incoherent());
        let count = lang_crates.iter().filter(|krate| marked(**krate)).count();
        let mut each = lang_crates.iter().copied().filter(|krate| marked(*krate));
        let frame: &'n blaze_mem::Frame = self.frame;
        let crates: &'n [usize] = frame.alloc_slice_fill_with(count, |_| each.next().unwrap_or(usize::MAX));
        // The fact's digest: each crate asked, by key, with its answer.
        let mut bytes: AVec<'_, u8> = AVec::new_in(Alloc::global());
        for krate in lang_crates.iter() {
            if let Some(key) = self.keys.get(*krate) {
                bytes.extend_from_slice(&key.0.0);
                bytes.push(u8::from(crates.contains(krate)));
            }
        }
        self.incoherent.set(Some((crates, blaze_id::ContentId::of("blaze-ladder front2 incoherent crates 1", &bytes))));
    }

    /// PLANTED BUG, for tests/front2_records.rs only: the bucket row of impl `key` is withheld
    /// from [`NamesSource::bucket`] from now on, as a bucket index written without that row
    /// would read. A question whose answer is that impl must then differ.
    #[doc(hidden)]
    pub fn plant_no_bucket_impl(&self, key: ItemKey) {
        self.no_bucket_impl.set(Some(key));
    }
}

/// A library key's first 16 bytes: what `keys::checker_of` keeps of it.
fn first_16(key: ItemKey) -> [u8; 16] {
    let mut first = [0u8; 16];
    first.copy_from_slice(&key.0.0[..16]);
    first
}

/// What a sorted table holds for 16 leading bytes ([`Records::one_beginning`]).
enum Beginning {
    One(ItemKey),
    /// No key of the table begins so.
    Absent,
    /// A row did not read, or two keys begin so: the error is filed and the answer is void.
    Void,
}

impl<'n> NamesSource<'n> for &'n Records<'n> {
    fn solved(self) -> Option<&'n Names<'n>> {
        None
    }

    fn gap(self, question: &'static str, key: Option<ItemKey>) {
        self.log.put(SourceError::Gap { question, key });
    }

    fn crate_count(self) -> usize {
        self.keys.len()
    }

    fn crate_of(self, key: ItemKey) -> Option<usize> {
        self.items.crate_of(key)
    }

    fn crate_key(self, krate: usize) -> Option<CrateItemKey> {
        self.keys.get(krate).copied()
    }

    fn root_module(self, krate: usize) -> Option<ItemKey> {
        Some(self.items_of(krate)?.root_module())
    }

    fn edition_of(self, krate: usize) -> Option<Edition> {
        Some(self.items_of(krate)?.edition())
    }

    fn cfg_atoms(self, krate: usize) -> Option<&'n [(&'n str, Option<&'n str>)]> {
        let record = self.items_of(krate)?;
        self.read(krate, record.cfg_atoms())
    }

    fn proc_macro(self, krate: usize) -> Option<bool> {
        Some(self.items_of(krate)?.proc_macro())
    }

    fn may_write_incoherent(self, krate: usize) -> Option<bool> {
        Some(self.items_of(krate)?.may_write_incoherent())
    }

    fn walk(self, module: ItemKey, ns: Namespace, written: Written<'_, &str>) -> Result<Resolved, PathError> {
        names::path(&self.paths, module, ns, self.kept(written), Alloc::global())
    }

    fn traits_in_scope<'b>(self, module: ItemKey, alloc: Alloc<'b>) -> Result<AVec<'b, ItemKey>, PathError> {
        names::traits_in_scope::<&'n str, _>(&self.paths, module, alloc)
    }

    fn variants(self, owner: ItemKey) -> Option<&'n [Variant<&'n str>]> {
        let krate = self.paths.crate_of(owner)?;
        let record = self.names_of(krate)?;
        self.read_names(krate, record.variants(owner))?
    }

    fn variant_of(self, variant: ItemKey) -> Option<(ItemKey, u32)> {
        // One search of the items record's variant table (`blaze_scope::items_record::
        // variants`: each variant's key with its enum and index, sorted by variant key): no
        // enum of the crate is scanned.
        let krate = self.items.crate_of(variant)?;
        // No record: the loader filed `NoRecord` the one time it was asked.
        let record = self.items_of(krate)?;
        self.read(krate, record.variant_home(variant))?
    }

    fn has_text(self, key: ItemKey) -> bool {
        let Some(krate) = self.items.crate_of(key) else { return false };
        let Some(record) = self.items_of(krate) else { return false };
        matches!(self.read(krate, record.item(key)), Some(Some(_)))
    }

    fn text_of(self, key: ItemKey) -> Option<ItemText<'n>> {
        if self.no_item.get() == Some(key) {
            return None;
        }
        let krate = self.items.crate_of(key)?;
        // No record: the loader filed `NoRecord` the one time it was asked.
        self.items_of(krate)?;
        // At item grain (item_grain.rs): an edit elsewhere in the file, or inside this item's
        // body, leaves its row standing at today's place; a moved head is refused by name.
        let now = self.row_now(krate, key)?;
        let row = now.row;
        let Some(span) = row.text else {
            self.log.put(SourceError::Items { krate, name: self.log.name(krate), error: ItemsRecordError::NoPlace { key } });
            return None;
        };
        let text = self.span_now(krate, &now, span)?;
        let impl_header = match row.impl_header {
            Some(header) => Some(self.span_now(krate, &now, header)?),
            None => None,
        };
        Some(ItemText { key, module: row.module, item: row.outline, name: row.name, impl_header, text })
    }

    fn name_of(self, key: ItemKey) -> Option<&'n str> {
        let krate = self.items.crate_of(key)?;
        let record = self.items_of(krate)?;
        self.read(krate, record.item(key))??.name
    }

    fn home_of(self, key: ItemKey) -> Option<ItemKey> {
        let krate = self.items.crate_of(key)?;
        let record = self.items_of(krate)?;
        Some(self.read(krate, record.item(key))??.module)
    }

    fn place_of(self, key: ItemKey) -> Option<Place> {
        let krate = self.items.crate_of(key)?;
        self.items_of(krate)?;
        // Today's outline entry (item_grain.rs): a body edit that adds or removes an item
        // nested in a body moves the entries after it.
        let row = self.row_now(krate, key)?.row;
        Some(Place { krate, at: row.text?.at, item: row.outline })
    }

    fn kind_of_item(self, key: ItemKey) -> Option<ItemKind> {
        let krate = self.items.crate_of(key)?;
        let record = self.items_of(krate)?;
        Some(self.read(krate, record.item(key))??.kind)
    }

    fn detail_of(self, key: ItemKey) -> Option<ItemDetail> {
        let krate = self.items.crate_of(key)?;
        let record = self.items_of(krate)?;
        let row = self.read(krate, record.item(key))??;
        let detail = detail_from(row.kind, row.body_holds_items);
        if detail.is_none() {
            self.gap("an outline entry's detail beyond its kind and its body-holds-items flag (question 11: the file's own FileItems record)", Some(key));
        }
        detail
    }

    fn is_ctor(self, key: ItemKey) -> bool {
        let Some(krate) = self.items.crate_of(key) else { return false };
        let Some(record) = self.items_of(krate) else { return false };
        matches!(self.read(krate, record.item(key)), Some(Some(row)) if row.ctor)
    }

    fn each_member(self, owner: ItemKey, each: &mut dyn FnMut(MemberOf<'n>)) -> bool {
        let Some(krate) = self.items.crate_of(owner) else { return false };
        if self.items_of(krate).is_none() {
            return false;
        }
        let Some(Some(rows)) = self.read(krate, self.items.members(owner)) else { return false };
        for row in rows.iter() {
            // A member whose text does not read is an error filed; the rest are still given,
            // and the log voids the result.
            let Some(text) = self.member_text_now(krate, owner, row) else { continue };
            each(MemberOf {
                key: row.key,
                name: row.name,
                item: row.outline,
                kind: row.kind,
                body_holds_items: row.body_holds_items,
                detail: detail_from(row.kind, row.body_holds_items),
                text,
                written_by: row.written_by,
            });
        }
        true
    }

    fn member_owner(self, member: ItemKey) -> Option<(ItemKey, &'n str)> {
        // One search of the items record's member owners section (`blaze_scope::items_record::
        // owners`: each named member's key with its owner's row, sorted by member key), then
        // one owner row and one member row read in place: no member list is decoded and no
        // crate is scanned.
        let krate = self.items.crate_of(member)?;
        // No record: the loader filed `NoRecord` the one time it was asked.
        self.items_of(krate)?;
        self.read(krate, self.items.member_owner(member))?
    }

    fn each_impl(self, krate: usize, each: &mut dyn FnMut(ImplAt<'n>)) {
        let Some(record) = self.items_of(krate) else { return };
        if self.no_impls.get() == Some(krate) {
            return;
        }
        for i in 0..record.impls() {
            let Some(Some(row)) = self.read(krate, record.impl_at(i)) else { continue };
            // At item grain (item_grain.rs): a body edit in the impl's file leaves its header.
            let Some(now) = self.row_now(krate, row.key) else { continue };
            let header = match now.row.impl_header {
                Some(span) => match self.span_now(krate, &now, span) {
                    Some(text) => text,
                    None => continue,
                },
                None => {
                    self.gap("an impl's header text (its item row has no header range)", Some(row.key));
                    continue;
                }
            };
            each(ImplAt { krate, key: row.key, module: row.module, header });
        }
    }

    fn has_buckets(self) -> bool {
        true
    }

    fn bucket(self, krate: usize, key: &blaze_scope::items_record::buckets::BucketKey, each: &mut dyn FnMut(blaze_scope::items_record::buckets::BucketImpl)) -> Option<blaze_id::ContentId> {
        // No record: the loader filed `NoRecord` the one time it was asked.
        let record = self.items_of(krate)?;
        if self.no_impls.get() != Some(krate) {
            for at in self.read(krate, record.bucket(key))? {
                let it = self.read(krate, record.bucket_impl(at))?;
                if self.no_bucket_impl.get() != Some(it.key) {
                    each(it);
                }
            }
        }
        self.read(krate, record.bucket_digest(key, Alloc::global()))
    }

    fn unfiled(self, krate: usize, each: &mut dyn FnMut(ImplRow<'n>, blaze_scope::items_record::buckets::NotFiled)) -> Option<blaze_id::ContentId> {
        let record = self.items_of(krate)?;
        if self.no_impls.get() != Some(krate) {
            let mut i = 0;
            while let Some((it, why)) = self.read(krate, record.unfiled_at(i))? {
                // A row that does not read is filed by `read`: the list is then void.
                each(self.read(krate, record.impl_at(it.row))??, why);
                i += 1;
            }
        }
        self.read(krate, record.unfiled_digest(Alloc::global()))
    }

    fn incoherent_crates(self) -> Option<(&'n [usize], blaze_id::ContentId)> {
        self.incoherent.get()
    }

    fn each_impl_row(self, krate: usize, each: &mut dyn FnMut(ImplRow<'n>)) -> bool {
        // No record: the loader filed `NoRecord`; the crate then has no impl this can file.
        let Some(record) = self.items_of(krate) else { return true };
        if self.no_impls.get() == Some(krate) {
            return true;
        }
        for i in 0..record.impls() {
            if let Some(Some(row)) = self.read(krate, record.impl_at(i)) {
                each(row);
            }
        }
        true
    }

    fn impl_with_header(self, krate: usize, key: ItemKey, module: ItemKey) -> Option<ImplAt<'n>> {
        self.items_of(krate)?;
        // At item grain (item_grain.rs), as `text_of`.
        let now = self.row_now(krate, key)?;
        let Some(span) = now.row.impl_header else {
            self.gap("an impl's header text (its item row has no header range)", Some(key));
            return None;
        };
        let header = self.span_now(krate, &now, span)?;
        Some(ImplAt { krate, key, module, header })
    }

    fn each_lang(self, krate: usize, each: &mut dyn FnMut(&'n str, ItemKey)) {
        let Some(record) = self.items_of(krate) else { return };
        for i in 0..record.langs() {
            if let Some(Some((name, key))) = self.read(krate, record.lang_at(i)) {
                each(name, key);
            }
        }
    }

    fn macro_body_of(self, module: ItemKey, written: Written<'_, &str>) -> Result<Option<MacroBody<'n>>, PathError> {
        let found = self.walk(module, Namespace::Macros, written)?;
        let len = written.segments.len();
        let def = found.whole(len).ok_or(PathError { module, at: found.consumed, why: Why::NotAModule })?;
        let krate = self.items.crate_of(def.key).ok_or(PathError { module, at: len.saturating_sub(1), why: Why::NotRead { module: def.key } })?;
        if self.items_of(krate).is_none() {
            return Err(PathError { module, at: len.saturating_sub(1), why: Why::NotRead { module: def.key } });
        }
        Ok(self.read(krate, self.items.macro_src(def.key)).flatten().map(|s| s.body))
    }

    fn macro_at(self, module: ItemKey, written: Written<'_, &str>) -> Result<MacroAt<'n>, PathError> {
        let found = self.walk(module, Namespace::Macros, written)?;
        let len = written.segments.len();
        let def = found.whole(len).ok_or(PathError { module, at: found.consumed, why: Why::NotAModule })?;
        let krate = self.items.crate_of(def.key).ok_or(PathError { module, at: len.saturating_sub(1), why: Why::NotRead { module: def.key } })?;
        // No record: the loader filed `NoRecord`; the path is then one into a crate not read.
        let record = self.items_of(krate).ok_or(PathError { module, at: len.saturating_sub(1), why: Why::NotRead { module: def.key } })?;
        // A macro row whose body text does not read is an error filed (the log voids the
        // body's result); it is then answered as a definition with no source.
        let src = self.read(krate, self.items.macro_src(def.key)).flatten();
        Ok(MacroAt { krate, def, src, crate_key: record.key(), proc_key: record.proc_key(), edition: record.edition() })
    }

    fn module_path(self, module: ItemKey) -> Option<&'n str> {
        let krate = self.items.crate_of(module)?;
        self.items_of(krate)?;
        self.read(krate, self.items.module(module))??.path
    }

    fn each_item(self, krate: usize, each: &mut dyn FnMut(ItemKey, ItemKey)) -> bool {
        let Some(record) = self.items_of(krate) else { return false };
        // The item rows are sorted by key.
        for i in 0..record.items() {
            if let Some(Some(row)) = self.read(krate, record.item_at(i)) {
                each(row.key, row.module);
            }
        }
        true
    }

    fn binds(self, module: ItemKey, ns: Namespace, name: &'n str) -> bool {
        // The names record holds a block's module as any module: its writer writes a scope row
        // for every finished scope of the crate (`names_record::encode`: `CrateNames::done`),
        // the table `Names::lookup` itself reads. The walk of `self::name` starts IN `module`
        // (`names::walk`: `PathKind::SelfMod` is the module the path is written in, a block's
        // module included) and looks the one segment up there: the module's own binding, then
        // its glob edges, which is what `Names::lookup` reads. `NotFound` is the answer "no";
        // any other refusal (the scope is not held, an ambiguous or unbounded glob walk) is
        // no answer: a typed gap naming the module, never taken for "no".
        match self.walk(module, ns, Written { kind: blaze_surface::PathKind::SelfMod, segments: &[name] }) {
            Ok(found) => found.consumed == 1,
            Err(PathError { why: Why::NotFound, .. }) => false,
            Err(PathError { why: Why::NotRead { .. }, .. }) => {
                self.gap("whether a block's module binds a name: the names record holds no scope row (or no chain) for the module asked", Some(module));
                false
            }
            Err(_) => {
                self.gap("whether a block's module binds a name: the walk of `self::name` in the module was refused (private, ambiguous or past its bound), where `Names::lookup` reads the binding itself", Some(module));
                false
            }
        }
    }

    fn key_beginning(self, first: [u8; 16]) -> Option<ItemKey> {
        if let Some(found) = self.beginnings.borrow().get(&first) {
            return *found;
        }
        let found = self.find_beginning(first);
        self.beginnings.borrow_mut().insert(first, found);
        found
    }
}
