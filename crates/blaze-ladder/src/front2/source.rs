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
//! - [`Records`]: each crate's NAMES record ([`RecordNames`]: a module's scope searched in place
//!   when a path walks through it) and ITEMS record ([`RecordItems`]: an item's row by key, its text
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

use core::cell::{Cell, OnceCell, Ref, RefCell};

use blaze_hash::HashMap;
use blaze_id::{CrateItemKey, ItemKey};
use blaze_own::{ARc, AVec, Alloc, Bump, Erased, LocalShare};
use blaze_scope::attr_values::{RowRefusal, RowValue};
use blaze_scope::items_record::{CurrentFile, ImplRow, ItemsDecoded, ItemsRecord, ItemsRecordError, LoadItems, RecordItems};
use blaze_scope::names::{self, PathError, Resolved, Unheld, VariantHit, Why, World, Written};
use blaze_scope::names_record::{Decoded, NamesRecord, NamesRecordError, Verified};
use blaze_scope::package_path::PackagePath;
use blaze_scope::resolve::{Names, Place};
use blaze_scope::task::{ItemText, MacroBody, MacroSrc, Variant};
use blaze_surface::{CrateRootView, Def, Edition, ItemDetail, ItemIndex, ItemKind, Namespace, ScopeView, ThroughScratch};

mod item_grain;

use super::index::ImplAt;

/// The names as this command solved them: the first [`NamesSource`].
pub type Solved<'n> = &'n Names<'n>;

/// The path a reader resolves against its package directory and the name used in diagnostics.
#[derive(Clone, Copy, Debug)]
pub struct FilePaths<'n> {
    pub package: PackagePath<'n>,
    pub shown: &'n str,
}

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

    /// The count of signature lowerings made through this source, when the source keeps one.
    fn head_lowerings(self) -> Option<&'n super::heads::HeadLowerings>;

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

    /// Which `std` the crate's code names `std` (blaze-unit-graph's `implicit`): Rust's, or
    /// bstd under a blaze edition. What an E0425 after a written `std` notes (std_is_bstd.rs).
    fn std_is(self, krate: usize) -> Option<blaze_unit_graph::StdIs>;

    /// The crate root's own attributes that change what is injected (`#![no_std]`,
    /// `#![no_core]`, `#![blaze::arbitrary_ints]`), as the solve read them (blaze-scope
    /// `CrateNames::root_attrs`; the names record's crate-root head holds the same three).
    fn root_attrs(self, krate: usize) -> Option<blaze_scope::graph::RootAttrs>;

    /// Whether the crate root turns on the language feature `name` (`#![feature(name)]`), from
    /// the crate root record's sorted `features` (blaze-surface `CrateRootView::has_feature`).
    fn has_feature(self, krate: usize, name: &str) -> Option<bool>;

    /// The crate root's `#![blaze::...]` syntax extension flags
    /// (`blaze_file_items::detect_blaze_extensions` on the root file): answered via
    /// `NameCell::Extensions` on `Tables` so adding or editing items in any file never records
    /// `NameCell::Whole { section: Items }`.
    fn blaze_extensions(self, krate: usize) -> parser::Extensions {
        self.file_text(krate, 0)
            .map(blaze_file_items::detect_blaze_extensions)
            .unwrap_or(parser::Extensions::NONE)
    }

    /// The crates the programs of `roots` link (front2 linked.rs, the one reading of the link
    /// rule): each crate's extern prelude and each `extern crate` it writes, transitively, a
    /// proc-macro unit reached never linked. `None`: a fact of a crate's links not answered
    /// (filed in the log).
    fn linked<'a>(self, roots: &[usize], alloc: Alloc<'a>) -> Option<Result<super::linked::Linked<'a>, super::linked::RootsError>>;

    /// The index of Rust's `std` among the crates (`Some(None)`: the program has none).
    fn sysroot_std(self) -> Option<Option<usize>>;

    /// Question 16: whether the crate may write incoherent impls (its root text names
    /// `rustc_attrs` or `rustc_coherence_is_core`).
    fn may_write_incoherent(self, krate: usize) -> Option<bool>;

    /// Question 1: what `written` means in `module`, looked up in `ns`.
    fn walk(self, module: ItemKey, ns: Namespace, written: Written<'_, &str>) -> Result<Resolved, PathError>;

    /// Question 1b: the module a path head `self` (`n == 0`) or `super` written `n` times names
    /// from `module`, a block's module passed over (`names::written_module`): what a glob
    /// `use self::*;` / `use super::*;` reads, with no segment to walk.
    fn written_module(self, module: ItemKey, n: u8) -> Result<ItemKey, PathError>;

    /// Question 2: the traits method resolution may use in `module`, sorted.
    fn traits_in_scope<'b>(self, module: ItemKey, alloc: Alloc<'b>) -> Result<AVec<'b, ItemKey>, PathError>;

    /// Question 3: an enum's variants, in declaration order, as the source holds them: read in
    /// place, never copied per question ([`VariantRows`]).
    fn variants(self, owner: ItemKey) -> Option<VariantRows<'n>>;

    /// The enum a variant is of and its index there (`Resolver::variant_of`).
    fn variant_of(self, variant: ItemKey) -> Option<(ItemKey, u32)>;

    /// Whether `key` is an item with text (of a file or an expansion); a variant is not.
    fn has_text(self, key: ItemKey) -> bool;

    /// Questions 7, 8, 9: an item's whole text, its module, its name, its impl header.
    fn text_of(self, key: ItemKey) -> Option<ItemText<'n>>;

    fn signature_text_of(self, key: ItemKey) -> Option<ItemText<'n>> { self.text_of(key) }
    fn seeded_text_of(self, key: ItemKey) -> Option<ItemText<'n>> { self.text_of(key) }
    fn read_member_signature(self, _member: ItemKey, _written_by: Option<ItemKey>) {}
    fn read_member_body(self, _member: ItemKey, _written_by: Option<ItemKey>) {}

    /// Question 9: an item's name. Reads no text.
    fn name_of(self, key: ItemKey) -> Option<&'n str>;

    /// Question 8: the module an item is declared in. Reads no text.
    fn home_of(self, key: ItemKey) -> Option<ItemKey>;

    /// Question 10: an item's place.
    fn place_of(self, key: ItemKey) -> Option<Place>;

    /// Question 10b: where an item's text begins: the text it is in (a file, or an expansion)
    /// and its outline entry's first byte there. `None`: the item has no text (over records
    /// the row without a place is filed in the log). Reads no text.
    fn item_start(self, key: ItemKey) -> Option<(blaze_scope::resolve::At, u32)>;

    /// Question 10f: file `file` of crate `krate` as a message shows it (its shown path, as a
    /// `#[track_caller]` `Location`'s file is written: [`crate::seam::FrontEnd::caller_site`]).
    /// `None`: no such file (over records the error is in the log).
    fn file_shown(self, krate: usize, file: u32) -> Option<&'n str> {
        self.file_paths(krate, file).map(|paths| paths.shown)
    }

    /// Both paths from the same file row. The package path is portable across checkouts.
    fn file_paths(self, krate: usize, file: u32) -> Option<FilePaths<'n>>;

    /// Question 10g: today's whole text of file `file` of crate `krate`, what an item's
    /// [`NamesSource::item_start`] is an offset into: where a position is turned into a line and
    /// a column, only when printed (rule 71; program/positions.rs). Over records: the text the
    /// record saw, proved by its digest, or the file as it is now when it changed
    /// (item_grain.rs `file_now`). `None` with the reason in the log.
    fn file_text(self, krate: usize, file: u32) -> Option<&'n str>;

    /// Question 10d: the origins of the expansion `call` of crate `krate` made, with the text the
    /// call is written in as it is now (front2 expansion_origin.rs): what a finding in the
    /// expansion's text is filed through. `Err` names why it cannot be.
    fn call_origin(self, krate: usize, call: ItemKey) -> Result<super::expansion_origin::CallOrigin<'n>, super::expansion_origin::Unmapped>;

    /// Question 10h: the text the expansion `call` of crate `krate` is written in (a file, or
    /// another expansion): the call part of its origins (`CallView::text`). What an include in
    /// a macro-made body is relative to (the Reference, `include!`: the file of the outermost
    /// call site; front2 body_host.rs `body_file`). `None`: the call's origins are not served
    /// (over records, the reason in the log).
    fn call_text(self, krate: usize, call: ItemKey) -> Option<blaze_scope::resolve::At> {
        self.call_origin(krate, call).ok().map(|origin| origin.view.text)
    }

    /// Question 10i: the one NAMED member of crate `krate` whose checker key is `item`, by its
    /// library key (`None`: no such member; refused when two begin alike, or the rows did not
    /// read). Where a place kept by its checker key is found again (program/lazy seam_rules.rs
    /// `source_position`). Asked of every names source alike, so a body's place
    /// (`seam_rules::body_place`) is the one rule over solved names, records and a table view
    /// (it was `PositionSource`'s own, which the view did not implement).
    fn member_beginning(self, krate: usize, item: blaze_typeck::ItemKey) -> Result<Option<ItemKey>, crate::seam::PositionGap>;

    /// Question 10j: each item and member of crate `krate` whose body can declare an item (a
    /// fn, const or static whose body holds items), visited by key. Reading these keys opens no
    /// text of an unrelated body.
    fn enclosing_keys(self, krate: usize, visit: impl FnMut(ItemKey) -> Result<(), crate::seam::PositionGap>) -> Result<(), crate::seam::PositionGap>;

    /// Question 10k: the whole text the expansion `call` of crate `krate` made (`None`: not
    /// served; over records the reason is in the log).
    fn expansion_text(self, krate: usize, call: ItemKey) -> Option<&'n str>;

    /// Question 10e: the text the expansion `call` of crate `krate` made with the mark of each
    /// of its tokens (front2 item_site.rs, token-mark-F1): what an item that call writes resolves
    /// its head's paths by. `Ok(None)`: the text keeps no column by design
    /// (`item_site::marks_of`).
    fn text_marks(self, krate: usize, call: ItemKey) -> Result<Option<blaze_scope::crate_roots::TextMarks<'n>>, super::item_site::MarksLost>;

    /// Question 10c: the abi of the `extern` block that declares `key`, unquoted as written,
    /// `"C"` when none is written (the Reference, items.extern.abi.default): `Some(Some(abi))`;
    /// `Some(None)`: no `extern` block declares it (every member, every module-level item
    /// outside one); `None`: not answered (over records the error is in the log).
    ///
    /// Asked of an item with a names row only: an item declared in a body (its key derived at
    /// its body's lowering, front2 locals.rs) has none, and its block's ABI is `Locals::foreign`'s,
    /// which its readers ask instead (heads.rs `fn_head`, program/head_abi.rs `placed_fn`); over
    /// records such a key is `None` with a gap naming it in the log.
    fn extern_abi(self, key: ItemKey) -> Option<Option<&'n str>>;

    /// Question 10d: the value of `attr` (`link_name` or `export_name`) on item `key` when it is
    /// written as a macro call (ring's `#[link_name = concat!(prefix!(), stringify!(f))]`), as
    /// the item's crate's solve expanded it once at the item (blaze-scope attr_values.rs): over
    /// the solved names the crate's `values` rows, over records the crate's items record's value
    /// table (blaze-scope items_record/values.rs), whose rows the `Item` cell's digest covers.
    /// `Some(Some(answer))`: the row (its string, or its refusal); `Some(None)`: no row (a quoted
    /// value is read from the item's text, front2 links.rs `symbol_of`, and an item declared in
    /// a body held for its own solve has none: its call value is refused by name there);
    /// `None`: not answered (over records the error is in the log).
    ///
    /// Required of every source, no default: a wrapper that did not forward it would answer "no
    /// row" for every item, and every call value would be refused.
    fn attr_value(self, key: ItemKey, attr: &'static str) -> Option<Option<RowValue<'n>>>;

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

    /// The impl tables (front2 buckets/records.rs, E3) the check files a question's impls
    /// from, when this source holds them: then no crate is filed whole and the items records'
    /// bucket section is not read. `None`: today's index.
    fn impl_tables(self) -> Option<&'n super::buckets::TableSet<'n>> {
        None
    }

    /// One cell of a crate's impl tables a lookup in [`NamesSource::impl_tables`] read
    /// (buckets/records.rs `cells_of`): a source that records its reads (front2 tables/view.rs)
    /// lists it; one that does not has nothing to list.
    fn impls_read(self, _read: super::tables::recorder::TableRead) {}

    /// Which checker question the reads that follow answer, or that its answer is in
    /// (blaze-typeck `World::asking`): a source that records its reads tells its recorder
    /// (front2 tables/recorder.rs `ReadSink::asking`); one that does not has nothing to tell.
    fn asking(self, _ask: Option<blaze_typeck::Asking>) {}

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

    /// Question 20b: the macro call `call` whose expansion failed (a [`Why::Refused`]'s `call`
    /// with `RefusedWhy::Expansion`): the macro's path as written, whether it is D104's implicit
    /// `::std::main`, and the matcher's reason, decoded into `bump`. `None`: no crate's notes
    /// hold a failure of that call (over records, a row that did not decode is filed in the
    /// log). Read only on an error's path, for its words (path_miss_shown.rs).
    fn failed_call<'b>(self, call: ItemKey, bump: Bump<'b>) -> Option<blaze_scope::names_record::FailedRow<'b>>
    where
        'n: 'b;

    /// Every item of crate `krate` that has text (question 7's rows), each given to `each` as
    /// (key, the module its names are read in), in key order. `false`: the crate is not read.
    /// Opens the crate's items record and reads every item row: for a whole-crate walk (the
    /// run path's reach, a census), never for one body.
    fn each_item(self, krate: usize, each: &mut dyn FnMut(ItemKey, ItemKey)) -> bool;

    /// A block's direct binding, before globs. The outer option says whether the scope was read.
    fn block_binding(self, module: ItemKey, ns: Namespace, name: &str) -> Option<Option<blaze_surface::Binding>>;

    /// Whether `module` (a module, a block's module or an enum) binds `name` in `ns`: its own
    /// binding, else through its glob edges (`Names::lookup`). Asked for a struct, enum, union
    /// or alias declared in a block of a body ([`super::locals`]).
    fn binds(self, module: ItemKey, ns: Namespace, name: &'n str) -> bool;

    /// The item, module or variant whose key begins with `first` (a checker key is a library
    /// key's first 16 bytes, `keys::checker_of`): the inverse the key map asks where it holds
    /// no row ([`super::keys::Keys::library_in`]). Over the solved names it is the names' own
    /// index of items by their first 16 bytes (`Names::key_beginning`): the key map there is
    /// filled when the front end is made (`Front2::new` files every item, variant, module and
    /// impl), so `library_in` never needs it, but the region walk's declared lifetimes (D155,
    /// types.rs `declared_lifetimes`) ask it with no key map. Over records the
    /// key map starts empty and this is one search of the key's crate (found by the key's
    /// first eight bytes): its item rows, then its module rows, then its enums' variants. Two
    /// keys of one table that begin alike are a typed error filed, and `None`.
    fn key_beginning(self, first: [u8; 16]) -> Option<ItemKey>;

    /// The lifetime parameters the struct, enum, union, trait, trait alias or type alias `key`
    /// declares (D155): a fact of its own text, answered from its crate's items record (or,
    /// over the solved names, from the same text by the same count), in any store, whatever
    /// heads were made. `None` for a key the names world does not hold or a kind with none.
    fn declared_lifetimes(self, key: ItemKey) -> Option<u32>;

    /// The own (GAT) lifetime parameters of the associated type `name` (the checker's name,
    /// its text's hash) of the trait `trait_` (D155), as [`NamesSource::declared_lifetimes`].
    fn assoc_lifetimes(self, trait_: ItemKey, name: blaze_typeck::Name) -> Option<u32>;
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
    fn head_lowerings(self) -> Option<&'n super::heads::HeadLowerings> {
        None
    }

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

    fn std_is(self, krate: usize) -> Option<blaze_unit_graph::StdIs> {
        self.crates().get(krate).map(|c| c.implicit.std)
    }

    fn root_attrs(self, krate: usize) -> Option<blaze_scope::graph::RootAttrs> {
        self.crates().get(krate).map(|c| c.root_attrs)
    }

    fn has_feature(self, krate: usize, name: &str) -> Option<bool> {
        self.crates().get(krate).map(|c| c.crate_root.features.binary_search(&name).is_ok())
    }

    fn linked<'a>(self, roots: &[usize], alloc: Alloc<'a>) -> Option<Result<super::linked::Linked<'a>, super::linked::RootsError>> {
        Some(super::linked::Linked::of(self, roots, alloc))
    }

    fn sysroot_std(self) -> Option<Option<usize>> {
        Some(self.graph().sysroot().std.map(|s| s.index()))
    }

    fn may_write_incoherent(self, krate: usize) -> Option<bool> {
        // `#![feature(rustc_attrs)]` is an inner attribute of the crate root, the first file
        // read; a mention anywhere in that text keeps the crate on the wide side.
        self.crates().get(krate).map(|c| blaze_scope::items_record::root_may_write_incoherent(c))
    }

    fn walk(self, module: ItemKey, ns: Namespace, written: Written<'_, &str>) -> Result<Resolved, PathError> {
        // The solved names' own allocator, the one they were resolved in (rule 75): a glob walk's
        // scratch is made in it per call, and allocates only when the walk reaches a glob.
        names::path(self, module, ns, written, Names::alloc(self))
    }

    fn written_module(self, module: ItemKey, n: u8) -> Result<ItemKey, PathError> {
        names::written_module::<&'n str, _>(self, module, n)
    }

    fn traits_in_scope<'b>(self, module: ItemKey, alloc: Alloc<'b>) -> Result<AVec<'b, ItemKey>, PathError> {
        Names::traits_in_scope(self, module, alloc)
    }

    fn variants(self, owner: ItemKey) -> Option<VariantRows<'n>> {
        let c: &'n blaze_scope::resolve::CrateNames<'n> = self.krate(Names::crate_of(self, owner)?);
        c.tables.enums.get(&owner).map(|vs| VariantRows::Solved(&vs[..]))
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

    fn item_start(self, key: ItemKey) -> Option<(blaze_scope::resolve::At, u32)> {
        let Place { krate, at, item } = self.place(key)?;
        let (_, outline) = self.outline_at(krate, at)?;
        Some((at, outline.item(item).range.start))
    }

    fn file_paths(self, krate: usize, file: u32) -> Option<FilePaths<'n>> {
        let row = self.crates().get(krate)?.files.get(file as usize)?;
        Some(FilePaths { package: row.package, shown: row.shown })
    }

    fn file_text(self, krate: usize, file: u32) -> Option<&'n str> {
        self.crates().get(krate)?.files.get(file as usize).map(|f| f.text)
    }

    fn member_beginning(self, krate: usize, item: blaze_typeck::ItemKey) -> Result<Option<ItemKey>, crate::seam::PositionGap> {
        use crate::seam::PositionGap;
        let c = self.crates().get(krate).ok_or(PositionGap::NoItem)?;
        let mut rows = c.tables.members.values().flat_map(|ms| ms.iter())
            .filter(|m| m.name.is_some() && super::keys::checker_of(m.key) == item);
        let first = rows.next().map(|m| m.key);
        if rows.next().is_some() { Err(PositionGap::NoItem) } else { Ok(first) }
    }

    fn enclosing_keys(self, krate: usize, mut visit: impl FnMut(ItemKey) -> Result<(), crate::seam::PositionGap>) -> Result<(), crate::seam::PositionGap> {
        use crate::seam::PositionGap;
        let c = self.crates().get(krate).ok_or(PositionGap::NoItem)?;
        let holds = |detail| matches!(detail, ItemDetail::Fn { body_holds_items: true }
            | ItemDetail::Const { body_holds_items: true } | ItemDetail::Static { body_holds_items: true });
        for (lib, _, place) in self.items(krate) {
            let (_, outline) = self.outline_at(krate, place.at).ok_or(PositionGap::NoItem)?;
            if holds(outline.item(place.item).detail) {
                visit(lib)?;
            }
        }
        for members in c.tables.members.values() {
            for member in members.iter().filter(|m| holds(m.detail)) {
                visit(member.key)?;
            }
        }
        Ok(())
    }

    fn expansion_text(self, krate: usize, call: ItemKey) -> Option<&'n str> {
        Some(self.outline_at(krate, blaze_scope::resolve::At::Expansion(call))?.0)
    }

    fn call_origin(self, krate: usize, call: ItemKey) -> Result<super::expansion_origin::CallOrigin<'n>, super::expansion_origin::Unmapped> {
        use super::expansion_origin::{CallOrigin, Unmapped};
        let c = self.crates().get(krate).ok_or(Unmapped::NoText { call })?;
        let made = c.expansions.get(&call).and_then(|e| e.made).ok_or(Unmapped::NoText { call })?;
        let bytes = made.origins.map_err(|error| Unmapped::Origins { call, error })?;
        let view = blaze_scope::expansion_origins::OriginsView::open(bytes).map_err(|error| Unmapped::Origins { call, error })?;
        let (text, outline) = self.outline_at(krate, view.text).ok_or(Unmapped::NoText { call })?;
        let shown = match view.text {
            blaze_scope::resolve::At::File(i) => c.files.get(i as usize).map(|f| f.shown).ok_or(Unmapped::NoText { call })?,
            // An expansion's text has no file: it prints as its crate, as texts.rs `Row::shown`.
            blaze_scope::resolve::At::Expansion(_) => c.name,
        };
        let entry = outline.item(view.item).range;
        Ok(CallOrigin { view, text, shown, entry: (entry.start, entry.end), edition: c.edition })
    }

    fn text_marks(self, krate: usize, call: ItemKey) -> Result<Option<blaze_scope::crate_roots::TextMarks<'n>>, super::item_site::MarksLost> {
        use super::item_site::{MarksLost, marks_of_column};
        let made = self.crates().get(krate).and_then(|c| c.expansions.get(&call)).and_then(|e| e.made).ok_or(MarksLost::NoExpansion { call })?;
        // The column the solve opened once when it made the text (blaze-scope resolve.rs
        // `ExpansionText::column`): O(1).
        marks_of_column(call, made.text, made.column)
    }

    fn extern_abi(self, key: ItemKey) -> Option<Option<&'n str>> {
        // An item with no place is declared by no block: as `program::placed` answered.
        let Some(Place { krate, at, item }) = self.place(key) else { return Some(None) };
        let Some((text, outline)) = self.outline_at(krate, at) else { return Some(None) };
        // The one reading of a block's ABI, the items record writer's (`foreign_abi`): an ABI
        // it cannot read is not answered (`None`), as over records the record is not written.
        let crate_key = self.crates().get(krate)?.key;
        blaze_scope::items_record::foreign_abi(text, outline, outline.item(item), key, crate_key).ok()
    }

    fn attr_value(self, key: ItemKey, attr: &'static str) -> Option<Option<RowValue<'n>>> {
        // The rows the crate's solve made (`CrateNames::values`, sorted by item and attribute):
        // the very rows its items record's value table is written from.
        let krate: &'n blaze_scope::resolve::CrateNames<'n> = self.crates().get(Names::crate_of(self, key)?).copied()?;
        Some(blaze_scope::attr_values::row(&krate.values, key, attr).map(|row| row.answer()))
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
        if matches!(
            self.kind_of_item(member),
            Some(k) if !matches!(k, ItemKind::Function | ItemKind::Const | ItemKind::TypeAlias)
        ) {
            return None;
        }
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

    fn failed_call<'b>(self, call: ItemKey, bump: Bump<'b>) -> Option<blaze_scope::names_record::FailedRow<'b>>
    where
        'n: 'b,
    {
        // The crate that stored the call refused, joined as its record's writer joins it
        // (blaze-scope names_record.rs `refused_row`, the one join).
        (0..self.crates().len()).find_map(|krate| blaze_scope::names_record::refused_row(self.krate(krate), call, bump))
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

    fn block_binding(self, module: ItemKey, ns: Namespace, name: &str) -> Option<Option<blaze_surface::Binding>> {
        let scope = &self.krate(Names::crate_of(self, module)?).done.get(&module)?.scope;
        Some(scope.get(ns, name).copied())
    }

    fn binds(self, module: ItemKey, ns: Namespace, name: &'n str) -> bool {
        self.lookup(module, ns, name).is_some()
    }

    fn key_beginning(self, first: [u8; 16]) -> Option<ItemKey> {
        // `Front2::new` filed every item, variant, module and impl of every crate in the key
        // map, so `Keys::library_in` asks this only for a key it holds no row for; a reader
        // with no key map (the region walk's declared lifetimes, D155: `types::HeadsArity`)
        // asks it directly, and the solved names' index of items by their first 16 bytes
        // answers it (two keys alike refused the names when they were made, rule 21).
        Names::key_beginning(self, first)
    }

    fn declared_lifetimes(self, key: ItemKey) -> Option<u32> {
        // A text that does not count (it does not lex, no keyword) answers none: the region
        // walk then refuses the count by name (`Unknown::AdtLifetimes`), as over records,
        // where the record was not written for it (`ItemsRecordError::Generics`).
        let krate = self.krate(Names::crate_of(self, key)?);
        blaze_scope::items_record::solved_lifetimes(krate, key).ok()?.map(u32::from)
    }

    fn assoc_lifetimes(self, trait_: ItemKey, name: blaze_typeck::Name) -> Option<u32> {
        let krate = self.krate(Names::crate_of(self, trait_)?);
        blaze_scope::items_record::solved_assoc_lifetimes(krate, trait_, |m| blaze_typeck::Name::of(m) == name).ok()?.map(u32::from)
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
    /// The item's file changed since its items record was written and no single entry of
    /// today's text has its chain `chain` (renamed, removed, or two entries share it): this
    /// item's row is stale and no text of it is answered (item_grain.rs). A rename or removal
    /// moves the file's `Surface`, so the crate's scope is remade by its own key. (A head edit
    /// is not stale: the record holds no head, D114 ruling.)
    ItemStale { krate: usize, name: &'n str, key: ItemKey, file: u32, chain: blaze_id::ContentId },
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
            SourceError::ItemStale { krate, name, key, file, chain } => {
                write!(f, "item {key:x} of crate `{name}` (crate {krate}), file {file}: no single entry of the file as it is now has its place by names (chain {chain:?} when written): its crate's scope is remade by its own key")
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
    /// What the macro call `call` of the crate expanded to: its text AND its origins, from ONE
    /// read of the expansion's record ([`LoadedExpansion`]). `None`: no text is kept for the
    /// call. One method, never a text method beside an origins method: two methods were two
    /// store reads of one record per marks request (D149 piece I4, F4; rule 77), the second past
    /// the items memo that had the text already.
    fn expansion(&self, krate: usize, call: ItemKey) -> Option<LoadedExpansion<'n>>;
    /// The current FileItems snapshot. Text-backed sources produce it once per file.
    fn file_items(&self, krate: usize, index: u32, path: PackagePath<'_>, _surface: blaze_id::ContentId, edition: Edition, bump: Bump<'n>) -> Result<Option<CurrentFile<'n>>, ItemsRecordError> {
        self.file(krate, path).map(|text| CurrentFile::from_text(index, text, edition, bump)).transpose()
    }
    /// An already-verified names record proof for crate `krate` (rule 74), avoiding redundant
    /// `Verified::check` per stretch when the caller verified the record once at load time.
    fn names_verified(&self, _krate: usize) -> Option<Verified<'n>> {
        None
    }
    /// Whether crate `krate` is a proc-macro crate when known from the crate graph without
    /// opening its `ItemsRecord` (still recorded as `NameCell::Root` by `Tables::proc_macro`).
    fn proc_macro(&self, _krate: usize) -> Option<bool> {
        None
    }
}

/// One read of an expansion's record ([`Load::expansion`]): the text and its origins, both
/// borrowed where they lie.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LoadedExpansion<'n> {
    /// The text the call expanded to (the items record checks its digest, not the loader).
    pub text: &'n str,
    /// The origins of that text as the TWO parts they are stored in, never a joined copy: the
    /// call part (where the call stands and the macro's path: an expansion record's crate
    /// directory keeps it) and the origin column (the record's, beside the text), what
    /// `blaze_scope::expansion_origins::OriginsView::open_parts` reads in place. An empty call
    /// part: the text keeps none. No default (rule 3): every loader says what it serves; `None`
    /// is "this loader holds no origins for the call", which a finding in the text files by name
    /// (front2 expansion_origin.rs `Unmapped::NotServed`), never a quiet nothing.
    pub origins: Option<(&'n [u8], &'n [u8])>,
}

/// The origin parts the loader's text reads brought with them, by (crate, call), until
/// [`NamesSource::text_marks`] takes them: what lets one store read serve an expansion's text
/// and its marks (F4). Bounded by the expansion rows of the records' items records (each call
/// is read through the items memo once, `ItemsRecord::whole`'s `seen`), and an entry leaves
/// when its marks are made, so it holds only texts read and not yet marked.
type OriginParts<'n> = RefCell<HashMap<(usize, ItemKey), Option<(&'n [u8], &'n [u8])>, Alloc<'n>>>;

/// The loader the two readers hold: the caller's, with "no record kept" filed in the log, and
/// each expansion read's origin parts kept for its marks ([`OriginParts`]).
#[derive(Clone)]
pub struct Loader<'n> {
    load: &'n dyn Load<'n>,
    log: &'n Log<'n>,
    origins: ARc<'n, OriginParts<'n>>,
}

/// Names records reader using pre-verified [`Verified`] proofs from [`Load::names_verified`]
/// when available (O(1), zero re-verification per stretch), falling back to [`Verified::check`].
struct VerifiedNames<'n> {
    by_prefix: HashMap<[u8; 8], usize, Alloc<'n>>,
    proofs: AVec<'n, OnceCell<Option<Verified<'n>>>>,
    load: &'n dyn Load<'n>,
    log: &'n Log<'n>,
}

impl<'n> VerifiedNames<'n> {
    fn new(keys: &[CrateItemKey], load: &'n dyn Load<'n>, log: &'n Log<'n>, alloc: Alloc<'n>) -> VerifiedNames<'n> {
        let mut by_prefix = HashMap::new_in(alloc);
        for (i, k) in keys.iter().enumerate() {
            by_prefix.insert(k.prefix(), i);
        }
        let mut proofs = AVec::with_capacity_in(keys.len(), alloc);
        proofs.resize_with(keys.len(), OnceCell::new);
        VerifiedNames { by_prefix, proofs, load, log }
    }

    fn crate_of(&self, key: ItemKey) -> Option<usize> {
        self.by_prefix.get(&key.crate_prefix()).copied()
    }

    fn record(&self, krate: usize) -> Option<NamesRecord<'n>> {
        let proof = self.proofs.get(krate)?.get_or_init(|| {
            if let Some(proof) = self.load.names_verified(krate) {
                return Some(proof);
            }
            let Some(bytes) = self.load.names(krate) else {
                self.log.put(SourceError::NoRecord { krate, name: self.log.name(krate), which: Which::Names });
                return None;
            };
            match Verified::check(bytes) {
                Ok(proof) => Some(proof),
                Err(error) => {
                    self.log.put(SourceError::Names { krate, name: self.log.name(krate), error });
                    None
                }
            }
        });
        proof.map(NamesRecord::open)
    }

    fn decoded(&self) -> Decoded {
        let mut sum = Decoded::default();
        for proof in self.proofs.iter().filter_map(|c| c.get()).flatten() {
            sum.crates += 1;
            sum.bytes += proof.byte_len();
        }
        sum
    }

    fn of(&self, key: ItemKey) -> Option<NamesRecord<'n>> {
        self.record(self.crate_of(key)?)
    }
}

impl<'n, 's> World<&'s str> for VerifiedNames<'n> {
    type Scope<'w>
        = ScopeView<'w>
    where
        Self: 'w;
    type Root<'w>
        = CrateRootView<'w>
    where
        Self: 'w;

    fn scope(&self, module: ItemKey) -> Option<ScopeView<'_>> {
        self.of(module)?.scope(module)
    }

    fn chain(&self, module: ItemKey) -> Option<&[ItemKey]> {
        self.of(module)?.chain(module)
    }

    fn root_of(&self, key: ItemKey) -> Option<CrateRootView<'_>> {
        Some(self.of(key)?.crate_root())
    }

    fn root(&self, krate: CrateItemKey) -> Option<CrateRootView<'_>> {
        Some(self.record(*self.by_prefix.get(&krate.prefix())?)?.crate_root())
    }

    fn variant(&self, owner: ItemKey, name: &'s str) -> Option<Option<VariantHit>> {
        Some(self.of(owner)?.variants(owner)?.find(name))
    }

    fn no_implicit_prelude(&self, module: ItemKey) -> bool {
        self.of(module).is_some_and(|r| r.no_implicit_prelude(module))
    }

    fn anonymous(&self, module: ItemKey) -> bool {
        self.of(module).is_some_and(|r| r.anonymous(module))
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

    /// The text, read with its origin parts in one loader read; the parts are kept for the
    /// call's marks ([`OriginParts`]). Asked once per call per records (the items memo, whose
    /// borrow is not held across this read).
    fn expansion(&self, krate: usize, call: ItemKey) -> Option<&'n str> {
        let read = self.load.expansion(krate, call)?;
        self.origins.borrow_mut().insert((krate, call), read.origins);
        Some(read.text)
    }

    fn file_items(&self, krate: usize, index: u32, path: PackagePath<'_>, surface: blaze_id::ContentId, edition: Edition, bump: Bump<'n>) -> Result<Option<CurrentFile<'n>>, ItemsRecordError> {
        self.load.file_items(krate, index, path, surface, edition, bump)
    }
}

/// What the records path read so far: the measure tests/front2_records.rs prints.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Opened {
    pub names: Decoded,
    pub items: ItemsDecoded,
    /// Expansions' origin columns opened (each verified whole, blaze-expand origin_column.rs
    /// `OriginsRef::open`): once per expansion asked of these records, however often its
    /// marks are asked ([`NamesSource::text_marks`]).
    pub origins: u64,
}

/// An enum's variants as a [`NamesSource`] holds them, in declaration order: the solve's list
/// in memory, or the names record's rows read in place. Handed out by value (`Copy`), so no
/// question copies a list anywhere: a per-call copy into the command's frame was a leak by
/// design (it grew the frame by every enum every body matched on, never reset).
#[derive(Clone, Copy)]
pub enum VariantRows<'n> {
    /// The solved names' list ([`Solved`]).
    Solved(&'n [Variant<&'n str>]),
    /// The names record's rows ([`Records`]), each decoded when it is read.
    Record(blaze_scope::names_record::Variants<'n>),
}

impl<'n> VariantRows<'n> {
    pub fn len(&self) -> usize {
        match self {
            VariantRows::Solved(list) => list.len(),
            VariantRows::Record(rows) => rows.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Variant `i`, `None` past the last.
    pub fn get(&self, i: usize) -> Option<Variant<&'n str>> {
        match self {
            VariantRows::Solved(list) => list.get(i).copied(),
            VariantRows::Record(rows) => rows.get(i),
        }
    }

    /// Every variant, in declaration order. The iterator borrows nothing of `self`.
    pub fn iter(&self) -> VariantIter<'n> {
        VariantIter { rows: *self, at: 0, end: self.len() }
    }
}

/// Prints as the list it is (`[Variant { .. }, ..]`), whichever source holds it: a reader
/// that compares two sources' answers by their text sees equal lists as equal.
impl core::fmt::Debug for VariantRows<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_list().entries(self.iter()).finish()
    }
}

/// [`VariantRows::iter`]: each variant by value, in declaration order.
pub struct VariantIter<'n> {
    rows: VariantRows<'n>,
    at: usize,
    end: usize,
}

impl<'n> Iterator for VariantIter<'n> {
    type Item = Variant<&'n str>;

    fn next(&mut self) -> Option<Variant<&'n str>> {
        if self.at >= self.end {
            return None;
        }
        let one = self.rows.get(self.at);
        self.at += 1;
        one
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let left = self.end - self.at;
        (left, Some(left))
    }
}

impl ExactSizeIterator for VariantIter<'_> {}

/// Every crate's names through its two records: the second [`NamesSource`] (as `&Records`).
pub struct Records<'n> {
    paths: VerifiedNames<'n>,
    items: RecordItems<'n, Loader<'n>>,
    keys: &'n [CrateItemKey],
    /// The origin parts the items reader's loader brought with each expansion text it read, the
    /// same table its [`Loader`] fills: [`NamesSource::text_marks`] takes a call's parts here,
    /// never by a second read of its record (F4).
    origin_parts: ARc<'n, OriginParts<'n>>,
    /// The caller's loader, asked here only by the planted bug [`Records::plant_reread_origins`].
    load: &'n dyn Load<'n>,
    /// A test's planted bug: [`NamesSource::text_marks`] reads the record again for its origin
    /// parts ([`Records::plant_reread_origins`]).
    reread_origins: Cell<bool>,
    log: &'n Log<'n>,
    /// Signature lowerings made through this records reader, including unsuccessful attempts.
    head_lowerings: super::heads::HeadLowerings,
    /// The command's frame: what is made ONCE per records and kept for them (the lang crates'
    /// list, [`Records::name_lang_crates`]). Nothing is put here per question: a frame is never
    /// reset under the records, so a per-lookup copy would grow it by every body checked over
    /// them (the guard: blaze-cli engine_bodies.rs measures it per body).
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
    /// A test's planted bug: the item whose declared lifetimes are not answered
    /// ([`Records::plant_no_lifetimes`]).
    no_lifetimes: Cell<Option<ItemKey>>,
    /// What [`NamesSource::text_marks`] answered, by (crate, call): the expansion's origin
    /// column opened and verified ONCE (rule 74), its view handed out after in O(1). Before
    /// (tempo `engine-check`, 2026-11-17): every head and body site of an expansion re-opened
    /// the column, each open reading every mark and row. At most [`MOST_MARKED_TEXTS`].
    marks: RefCell<HashMap<(usize, ItemKey), Result<Option<blaze_scope::crate_roots::TextMarks<'n>>, super::item_site::MarksLost>, Alloc<'n>>>,
    /// Origin columns opened so far ([`Opened::origins`]).
    origins_opened: Cell<u64>,
    /// The ONE glob walk's working memory of every path these records resolve
    /// ([`NamesSource::walk`], [`Records::walk_logged`], [`NamesSource::binds`]): made once, in
    /// `alloc`, cleared by each walk and grown only while the largest walk grows. Before (tempo
    /// `engine-check`, 2026-11-18 stack sample): `names::path` made a fresh scratch per path, per
    /// segment and per lexical layer. Records are one thread's (they hold `RefCell`s), and a walk
    /// calls nothing that walks again, so the borrow is never contended.
    scratch: RefCell<ThroughScratch<'n>>,
    /// The allocator the records were opened with, the caller's: a walk's and a digest's
    /// scratch too.
    alloc: Alloc<'n>,
}

/// The most expansions whose marks one [`Records`] keeps (rule 18): past it a text's marks are
/// refused by name (`MarksLost::TooManyTexts`), never re-opened per ask. tempo's whole
/// workspace writes on the order of 10^5 expansions.
pub const MOST_MARKED_TEXTS: usize = 1 << 20;

impl<'n> Records<'n> {
    /// `keys`: each crate's key by index (index 0 the program's own), as `load` and `log` know
    /// the crates. Nothing is read here.
    pub fn new(keys: &'n [CrateItemKey], load: &'n dyn Load<'n>, log: &'n Log<'n>, frame: &'n blaze_mem::Frame, alloc: Alloc<'n>) -> Records<'n> {
        let origin_parts: ARc<'n, OriginParts<'n>> = ARc::new_in(RefCell::new(HashMap::new_in(alloc)), alloc);
        let bump = frame.alloc(Erased::new(frame)).bump();
        Records {
            paths: VerifiedNames::new(keys, load, log, alloc),
            items: RecordItems::new(keys, Loader { load, log, origins: origin_parts.local_share() }, bump),
            keys,
            origin_parts,
            load,
            reread_origins: Cell::new(false),
            log,
            head_lowerings: super::heads::HeadLowerings::default(),
            frame,
            beginnings: RefCell::new(HashMap::new_in(alloc)),
            no_impls: Cell::new(None),
            no_bucket_impl: Cell::new(None),
            incoherent: Cell::new(None),
            no_item: Cell::new(None),
            no_lifetimes: Cell::new(None),
            marks: RefCell::new(HashMap::new_in(alloc)),
            origins_opened: Cell::new(0),
            scratch: RefCell::new(ThroughScratch::new_in(alloc)),
            alloc,
        }
    }

    /// PLANTED BUG, for tests/front2_token_mark_bodies.rs only: from now on
    /// [`NamesSource::text_marks`] asks the loader again for an expansion's origin parts instead
    /// of taking the ones its text's read brought (the two reads per marked text F4 removed), so
    /// a loader counting its reads sees two for each marked text.
    #[doc(hidden)]
    pub fn plant_reread_origins(&self) {
        self.reread_origins.set(true);
    }

    /// PLANTED BUG, for tests/front2_walk_scratch.rs only: the held glob walk's values are not
    /// cleared between walks from now on (blaze-surface `ThroughScratch::plant_stale_values`),
    /// so a path whose walk reads a glob may name what an earlier walk found.
    #[doc(hidden)]
    pub fn plant_stale_scratch(&self) {
        self.scratch.borrow_mut().plant_stale_values();
    }

    /// [`NamesSource::walk`] with a fresh scratch for this one path (`names::path`), the held
    /// one untouched: the oracle tests/front2_walk_scratch.rs compares the held walk with.
    #[doc(hidden)]
    pub fn walk_fresh(&'n self, module: ItemKey, ns: Namespace, written: Written<'_, &str>) -> Result<Resolved, PathError> {
        names::path(&self.paths, module, ns, written, self.alloc)
    }

    /// The held scratch's walks through globs so far, and its buffers' capacities (a warm pass
    /// grows none).
    pub fn scratch_use(&self) -> (u64, [usize; 5]) {
        let scratch = self.scratch.borrow();
        (scratch.walks(), scratch.capacities())
    }

    /// PLANTED BUG, for tests/front2_records.rs only: the item row of `key` is withheld from
    /// [`NamesSource::text_of`] from now on, as an items record written without that row would
    /// read. A body that needs the item's head must then differ from its check over the solved
    /// names.
    #[doc(hidden)]
    pub fn plant_no_item(&self, key: ItemKey) {
        self.no_item.set(Some(key));
    }

    /// PLANTED BUG, for the region walk's tests only (D155, rule 23): from now on
    /// [`NamesSource::declared_lifetimes`] and [`NamesSource::assoc_lifetimes`] of `key` answer
    /// `None`, as a names world that held no count for it would. A region column that counts
    /// it must then be refused by name (`Unlaid::Counts`), never laid by a guess.
    #[doc(hidden)]
    pub fn plant_no_lifetimes(&self, key: ItemKey) {
        self.no_lifetimes.set(Some(key));
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

    /// The text item `key` is written in, from its items row alone: NO text is read, so no
    /// expansion record is asked. What finds the call that wrote a body (blaze-cli
    /// body_types/covered.rs `written_by`): [`NamesSource::place_of`] proves the text first,
    /// so a body whose expansion record the store lacks had no place and fell out of its call's
    /// task, which then passed clean with nothing checked (rule 3). Placed here, the body stays
    /// its call's, and its check, which reads the text, is refused naming the record.
    /// `None`: no such item, or no place (the reason filed in the log).
    pub fn text_at(&'n self, key: ItemKey) -> Option<blaze_scope::resolve::At> {
        let krate = self.items.crate_of(key)?;
        let record = self.items_of(krate)?;
        let row = self.read(krate, record.identity(key))??;
        let Some(at) = row.at else {
            self.log.put(SourceError::Items { krate, name: self.log.name(krate), error: ItemsRecordError::NoPlace { key } });
            return None;
        };
        Some(at)
    }

    /// Each member row of `owner` alone (its key, name, kind, outline entry and the call that
    /// wrote it), in declaration order: NO member text is read, so no expansion record is asked
    /// (blaze-cli engine_heads.rs `heads_of_crate`, which places members by their entry and never
    /// reads their bytes). [`NamesSource::each_member`] proves each member's text first
    /// (item_grain.rs `member_now`); a reader that checks a member reads its text there. A file
    /// member's row is read against today's file as every items row is (blaze-scope
    /// items_record.rs `item_span`, `current_file`: a file whose surface moved is refused).
    /// `false`: no such owner, or its rows do not read; a row that does not read is filed in the
    /// log, which voids the result, and the rest are still given.
    pub fn each_member_row(&'n self, owner: ItemKey, each: &mut dyn FnMut(&blaze_scope::items_record::MemberRow<'n>)) -> bool {
        let Some(krate) = self.items.crate_of(owner) else { return false };
        if self.items_of(krate).is_none() {
            return false;
        }
        let Some(Some(rows)) = self.read(krate, self.items.members(owner)) else { return false };
        for row in rows.iter() {
            let Some(row) = self.read(krate, row) else { continue };
            each(&row);
        }
        true
    }

    /// [`NamesSource::walk`] with every cell the walk consults put in `sink` (blaze-scope
    /// names_cells.rs `Logged`, the one seam a reader by cell walks through): what the
    /// read-only view (front2 tables/view.rs) answers a path from.
    pub fn walk_logged(&'n self, sink: &dyn blaze_scope::names_cells::CellSink, module: ItemKey, ns: Namespace, written: Written<'_, &str>) -> Result<Resolved, PathError> {
        let logged = blaze_scope::names_cells::Logged { world: &self.paths, log: sink };
        // The segments are read for the call alone ([`Records::walk`]'s note); the glob walk's
        // memory is the records' one scratch.
        names::path_in(&logged, module, ns, written, &mut self.scratch.borrow_mut())
    }

    /// [`NamesSource::written_module`] with every cell consulted put in `sink`.
    pub fn written_module_logged(&'n self, sink: &dyn blaze_scope::names_cells::CellSink, module: ItemKey, n: u8) -> Result<ItemKey, PathError> {
        let logged = blaze_scope::names_cells::Logged { world: &self.paths, log: sink };
        names::written_module::<&'n str, _>(&logged, module, n)
    }

    /// [`NamesSource::traits_in_scope`] with every cell consulted put in `sink`.
    pub fn traits_in_scope_logged<'b>(&'n self, sink: &dyn blaze_scope::names_cells::CellSink, module: ItemKey, alloc: Alloc<'b>) -> Result<AVec<'b, ItemKey>, PathError> {
        let logged = blaze_scope::names_cells::Logged { world: &self.paths, log: sink };
        names::traits_in_scope::<&'n str, _>(&logged, module, alloc)
    }

    /// What was opened and decoded so far.
    pub fn opened(&self) -> Opened {
        Opened { names: self.paths.decoded(), items: self.items.decoded(), origins: self.origins_opened.get() }
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

    /// Crate `krate`'s items record as these records opened it: opened at the first question
    /// about it and kept for the records' life, so a reader of its rows outside the records
    /// (blaze-cli body_types/heads.rs, a head's file) opens nothing again. `None`: the loader
    /// holds no record of it (filed `NoRecord` once). [`Records::opened`]'s `items.crates`
    /// counts the opens: one per crate, however many reads.
    pub fn items_record(&'n self, krate: usize) -> Option<&'n ItemsRecord<'n>> {
        self.items_of(krate)
    }

    fn names_of(&'n self, krate: usize) -> Option<NamesRecord<'n>> {
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
        let item_at = |i: usize| self.read(krate, record.identity_at(i)).flatten().map(|row| row.key);
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
        let mut bytes: AVec<'_, u8> = AVec::new_in(self.alloc);
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

    /// [`NamesSource::extern_abi`] of the free fn or static `key` (not a member): the abi of
    /// the `extern` block that declares it (`Some(None)`: no block does), as its items row
    /// holds it (`ItemRow::foreign`, written by the names task from the outline of the text the
    /// item is in, a file's or a macro call's expansion's alike). `None` with the reason filed.
    ///
    /// No text is read and nothing is outlined: the row is the whole answer, and its `Item`
    /// cell's digest covers it (blaze-scope names_cells.rs `item_digest`). A row whose file
    /// changed is answered at item grain (`row_now`): the block's ABI is in the record's
    /// `Surface` reads (blaze-file-items names_digest.rs, `ExternBlock`: its ABI's text), so a
    /// row found by its chain holds it; a row whose chain is gone is refused by name.
    fn block_abi(&'n self, key: ItemKey) -> Option<Option<&'n str>> {
        let krate = self.items.crate_of(key)?;
        // No record: the loader filed `NoRecord` the one time it was asked.
        let record = self.items_of(krate)?;
        Some(self.read(krate, record.identity(key))??.foreign)
    }

    /// Link closure (same as [`NamesSource::linked`]) invoking `touch(krate)` for every crate
    /// whose `NamesRecord` / `ItemsRecord` is inspected (both reached crates and `proc_macro`
    /// checks on their targets), so [`super::tables::view::Tables::linked`] records exact
    /// [`blaze_scope::names_cells::NameCell::Links`] cells instead of whole-crate sections.
    pub(in crate::front2) fn linked_logged<'a>(
        &'n self,
        roots: &[usize],
        alloc: Alloc<'a>,
        touch: impl Fn(usize),
    ) -> Option<Result<super::linked::Linked<'a>, super::linked::RootsError>> {
        let closure = super::linked::Linked::closure(
            self.keys.len(),
            roots,
            alloc,
            |at, link| {
                touch(at);
                let names = self.names_of(at).ok_or(())?;
                let crate_of = |root: ItemKey| match self.paths.crate_of(root) {
                    Some(i) => Ok(i),
                    None => {
                        self.gap("linked: a crate's extern prelude or `extern crate` names a root module of no crate the records hold", Some(root));
                        Err(())
                    }
                };
                for (_, root) in names.crate_root().externs() {
                    link(crate_of(root)?);
                }
                let items = self.items_of(at).ok_or(())?;
                for m in 0..items.modules() {
                    let Some(row) = self.read(at, items.module_at(m)).ok_or(())? else { break };
                    let Some(scope) = names.scope(row.key) else { continue };
                    for entry in scope.bindings() {
                        if matches!(entry.binding.origin, blaze_surface::Origin::ExternCrate) {
                            link(crate_of(entry.binding.def.key)?);
                        }
                    }
                }
                Ok(())
            },
            |i| {
                touch(i);
                self.proc_macro(i).ok_or(())
            },
        );
        closure.ok()
    }
}

/// The most changed files one command outlines to read their members at item grain (rule 18).
pub const MOST_OUTLINES: usize = 4096;

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
    fn head_lowerings(self) -> Option<&'n super::heads::HeadLowerings> {
        Some(&self.head_lowerings)
    }

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
        if let Some(pm) = self.load.proc_macro(krate) {
            return Some(pm);
        }
        Some(self.items_of(krate)?.proc_macro())
    }

    fn std_is(self, krate: usize) -> Option<blaze_unit_graph::StdIs> {
        Some(self.items_of(krate)?.std_is())
    }

    /// From the names record's crate-root head (blaze-surface `CrateRootHead`), which the solve
    /// wrote from the same `RootAttrs`.
    fn root_attrs(self, krate: usize) -> Option<blaze_scope::graph::RootAttrs> {
        let head = self.names_of(krate)?.crate_root().head();
        Some(blaze_scope::graph::RootAttrs { no_std: head.no_std, no_core: head.no_core, arbitrary_ints: head.arbitrary_ints })
    }

    fn has_feature(self, krate: usize, name: &str) -> Option<bool> {
        Some(self.names_of(krate)?.crate_root().has_feature(name))
    }

    /// The link closure (linked.rs `Linked::closure`, the one work list) over what each
    /// crate's records hold of its links:
    /// - its extern prelude, the names record's crate-root `externs` (blaze-scope resolve.rs
    ///   `root_plan`: the graph's `externs(..).prelude`, the rule's `core`/`std` less what
    ///   `#![no_std]`/`#![no_core]` take away, with the root's macro-written `extern crate`
    ///   names merged in), each a crate root module, its crate by its key's prefix;
    /// - each `extern crate` it writes in any module: a binding of origin
    ///   [`blaze_surface::Origin::ExternCrate`] in that module's solved scope, whose def is the
    ///   named crate's root module (`extern crate self` names its own crate: nothing new).
    ///
    /// A module whose scope the record holds none of (a module the solve failed, its error
    /// the names library's, already made) adds no link. `extern crate name as _;` binds no
    /// name, so no scope holds it: such a crate is linked only if something else names it
    /// (the end form: the items record's `extern crate` row with its target crate).
    fn linked<'a>(self, roots: &[usize], alloc: Alloc<'a>) -> Option<Result<super::linked::Linked<'a>, super::linked::RootsError>> {
        self.linked_logged(roots, alloc, |_| {})
    }

    /// Rust's `std` is the crate whose items record names it `std` (blaze-scope graph.rs
    /// `Sysroot::std`, the unit the toolchain's sysroot calls `std`). The records hold no
    /// sysroot slot, so this reads every crate's name: none is `Some(None)`; two crates named
    /// `std` (a workspace crate of that name beside the sysroot's) is not told apart here and
    /// is refused by name. The end form: crate 0's items record carries the unit graph's
    /// sysroot slots (core, alloc, std, bstd as crate keys).
    fn sysroot_std(self) -> Option<Option<usize>> {
        let mut found: Option<usize> = None;
        for i in 0..self.keys.len() {
            let record = self.items_of(i)?;
            if self.read(i, record.name())? != "std" {
                continue;
            }
            if found.replace(i).is_some() {
                self.gap("sysroot_std: two crates are named `std`, and the records hold no sysroot slot to tell Rust's apart", None);
                return None;
            }
        }
        Some(found)
    }

    fn may_write_incoherent(self, krate: usize) -> Option<bool> {
        Some(self.items_of(krate)?.may_write_incoherent())
    }

    /// The segments are read for this call alone, never kept: `RecordNames` answers
    /// `World<N>` for EVERY name type `N` (blaze-scope names_record.rs: its scope and variant
    /// views compare the name's bytes in place), so the walk's `N` is the caller's `&'w str` and
    /// no segment is copied to the records' lifetime; `Resolved` and `PathError` hold none.
    fn walk(self, module: ItemKey, ns: Namespace, written: Written<'_, &str>) -> Result<Resolved, PathError> {
        names::path_in(&self.paths, module, ns, written, &mut self.scratch.borrow_mut())
    }

    fn written_module(self, module: ItemKey, n: u8) -> Result<ItemKey, PathError> {
        names::written_module::<&'n str, _>(&self.paths, module, n)
    }

    fn traits_in_scope<'b>(self, module: ItemKey, alloc: Alloc<'b>) -> Result<AVec<'b, ItemKey>, PathError> {
        names::traits_in_scope::<&'n str, _>(&self.paths, module, alloc)
    }

    fn variants(self, owner: ItemKey) -> Option<VariantRows<'n>> {
        let krate = self.paths.crate_of(owner)?;
        // The record's rows, read in place: nothing is copied per question (a copy into the
        // command's frame per call grew it by every enum every body matched on: tempo, 2026-11-17).
        Some(VariantRows::Record(self.names_of(krate)?.variants(owner)?))
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
        matches!(self.read(krate, record.identity(key)), Some(Some(_)))
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
        let row = self.row_now(krate, key)?;
        let Some(span) = row.text else {
            self.log.put(SourceError::Items { krate, name: self.log.name(krate), error: ItemsRecordError::NoPlace { key } });
            return None;
        };
        let text = self.span_now(krate, span)?;
        let impl_header = match row.impl_header {
            Some(header) => Some(self.span_now(krate, header)?),
            None => None,
        };
        Some(ItemText { key, module: row.module, item: row.outline, name: row.name, impl_header, text })
    }

    fn name_of(self, key: ItemKey) -> Option<&'n str> {
        let krate = self.items.crate_of(key)?;
        let record = self.items_of(krate)?;
        self.read(krate, record.identity(key))??.name
    }

    fn home_of(self, key: ItemKey) -> Option<ItemKey> {
        let krate = self.items.crate_of(key)?;
        let record = self.items_of(krate)?;
        Some(self.read(krate, record.identity(key))??.module)
    }

    fn place_of(self, key: ItemKey) -> Option<Place> {
        let krate = self.items.crate_of(key)?;
        self.items_of(krate)?;
        let row = self.row_now(krate, key)?;
        Some(Place { krate, at: row.text?.at, item: row.outline })
    }

    fn item_start(self, key: ItemKey) -> Option<(blaze_scope::resolve::At, u32)> {
        let krate = self.items.crate_of(key)?;
        self.items_of(krate)?;
        // Today's place (item_grain.rs): the row's span is in today's text of its file.
        let row = self.row_now(krate, key)?;
        let Some(span) = row.text else {
            self.log.put(SourceError::Items { krate, name: self.log.name(krate), error: ItemsRecordError::NoPlace { key } });
            return None;
        };
        Some((span.at, span.start))
    }

    /// Both columns of the items record's file row, without reading its text.
    fn file_paths(self, krate: usize, file: u32) -> Option<FilePaths<'n>> {
        let record = self.items_of(krate)?;
        match self.read(krate, record.file_surface(file))? {
            Some(row) => Some(FilePaths { package: row.path, shown: row.shown }),
            None => {
                self.log.put(SourceError::Items { krate, name: self.log.name(krate), error: ItemsRecordError::Short { what: "a file row", at: file as usize } });
                None
            }
        }
    }

    /// Today's text of the file ([`Records::file_now`], item_grain.rs): the one the item rows
    /// [`NamesSource::item_start`] answers from are offsets into.
    fn file_text(self, krate: usize, file: u32) -> Option<&'n str> {
        self.file_now(krate, file)
    }

    fn member_beginning(self, krate: usize, item: blaze_typeck::ItemKey) -> Result<Option<ItemKey>, crate::seam::PositionGap> {
        use crate::seam::PositionGap;
        // A failed item-prefix read invalidates the lookup, even when a
        // different table has a row beginning with those bytes.
        if !self.errors().is_empty() {
            return Err(PositionGap::NoItem);
        }
        let record = self.items_record(krate).ok_or(PositionGap::NoItem)?;
        let get = |i| record.owned_key_at(i).map_err(|_| PositionGap::NoItem);
        let (mut lo, mut hi) = (0, record.owned_members());
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            if super::keys::checker_of(get(mid)?.ok_or(PositionGap::NoItem)?) < item {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        let first = get(lo)?.filter(|k| super::keys::checker_of(*k) == item);
        if first.is_some() && get(lo + 1)?.is_some_and(|k| super::keys::checker_of(k) == item) {
            return Err(PositionGap::NoItem);
        }
        Ok(first)
    }

    fn enclosing_keys(self, krate: usize, mut visit: impl FnMut(ItemKey) -> Result<(), crate::seam::PositionGap>) -> Result<(), crate::seam::PositionGap> {
        use crate::seam::PositionGap;
        let record = self.items_record(krate).ok_or(PositionGap::NoItem)?;
        for i in 0..record.items() {
            let row = record.item_at(i).map_err(|_| PositionGap::NoItem)?.ok_or(PositionGap::NoItem)?;
            if row.body_holds_items && matches!(row.kind, ItemKind::Function | ItemKind::Const | ItemKind::Static) {
                visit(row.key)?;
            }
            if matches!(row.kind, ItemKind::Impl | ItemKind::Trait)
                && let Some(members) = record.members(row.key).map_err(|_| PositionGap::NoItem)?
            {
                for member in members.iter() {
                    let member = member.map_err(|_| PositionGap::NoItem)?;
                    if member.body_holds_items && matches!(member.kind, ItemKind::Function | ItemKind::Const) {
                        visit(member.key)?;
                    }
                }
            }
        }
        Ok(())
    }

    fn expansion_text(self, krate: usize, call: ItemKey) -> Option<&'n str> {
        self.whole_text(krate, blaze_scope::resolve::At::Expansion(call))
    }

    fn call_origin(self, krate: usize, call: ItemKey) -> Result<super::expansion_origin::CallOrigin<'n>, super::expansion_origin::Unmapped> {
        // The origins' two parts as `call_text` reads them: those the text's read brought
        // (`OriginParts`), peeked, never taken (`text_marks` takes them); else one loader read.
        // The calling entry is then found in the items record and today's text of its file
        // (program/lazy/seam_rules.rs `recorded_call_origin`). Before (tempo tests t0850,
        // 2026-11-21): refused here by name, so no item a derive or an attribute macro wrote
        // had a `#[track_caller]` place over records (132 tests not run).
        use super::expansion_origin::Unmapped;
        let kept = self.origin_parts.borrow().get(&(krate, call)).copied();
        let parts = match kept {
            Some(parts) => parts,
            None => self.load.expansion(krate, call).ok_or(Unmapped::NotServed { call })?.origins,
        };
        let parts = parts.ok_or(Unmapped::NotServed { call })?;
        // An empty call part is a text that keeps no origins by design (blaze-scope
        // expansion_records.rs `write`), answered typed and filed in no log: the log voids the
        // whole instance (tempo a1425), and only a place through this call is lost.
        if parts.0.is_empty() {
            return Err(Unmapped::NoCallPart { call });
        }
        self.recorded_call_origin(parts, krate, call, self.alloc)
    }

    fn call_text(self, krate: usize, call: ItemKey) -> Option<blaze_scope::resolve::At> {
        use blaze_scope::expansion_origins::CallView;
        // The call part alone (`CallView::open`: its fixed words, no column), which the
        // expansion's record keeps: the parts the text's read brought (`Loader`'s
        // `LoadItems::expansion`), peeked, never taken (`text_marks` takes them); a call whose
        // parts were taken or whose text was not read here is one loader read.
        let kept = self.origin_parts.borrow().get(&(krate, call)).copied();
        let parts = match kept {
            Some(parts) => parts,
            None => self.load.expansion(krate, call)?.origins,
        };
        let Some((call_part, _)) = parts else {
            self.gap("`call_text`: the loader holds no origins for the expansion", Some(call));
            return None;
        };
        // A text that keeps no origins by its writer's design (blaze-scope expansion_records.rs
        // `write`; `Unmapped::NoCallPart`, as `call_origin` answers it): no calling file is
        // known, which is an answer, not a records fault. `body_file` answers no file, and an
        // include or `file!()` in that body is refused by name there (body_host.rs
        // `HostAsk::Include`, `HostAsk::Place`). Before (tempo a1425, 2026-11-21): filed in the
        // log here, so every body such a text wrote voided its instance (body.rs asks
        // `body_file` of each body with sources) and most packages' suites stopped.
        if call_part.is_empty() {
            return None;
        }
        match CallView::open(call_part) {
            Ok(view) => Some(view.text),
            Err(_) => {
                self.gap("`call_text`: the expansion's call part does not open (blaze-scope `CallView::open`)", Some(call));
                None
            }
        }
    }

    fn text_marks(self, krate: usize, call: ItemKey) -> Result<Option<blaze_scope::crate_roots::TextMarks<'n>>, super::item_site::MarksLost> {
        use super::item_site::{MarksLost, marks_of_column};
        use blaze_scope::crate_roots::RootsLost;
        use blaze_scope::expansion_origins::OriginsView;
        // Asked before: the opened view, O(1) (the field's docs).
        if let Some(answer) = self.marks.borrow().get(&(krate, call)) {
            return *answer;
        }
        if self.marks.borrow().len() >= MOST_MARKED_TEXTS {
            return Err(MarksLost::TooManyTexts { call, most: MOST_MARKED_TEXTS });
        }
        let answer = (|| -> Result<Option<blaze_scope::crate_roots::TextMarks<'n>>, MarksLost> {
            // The text through the items memo ([`Records::whole_text`], blaze-scope
            // items_record.rs `ItemsRecord::whole` and its `seen` memo): the text a body or row
            // of this call already read in these records, checked against the record's digest
            // once, never a second store read past the memo (D149 piece I4, rule 77). A text the
            // record does not keep or that moved is filed in the log there, which voids the
            // result, and the marks are lost by name.
            let text = self.whole_text(krate, blaze_scope::resolve::At::Expansion(call)).ok_or(MarksLost::NoExpansion { call })?;
            // The origin parts the SAME read brought (`Loader`'s `LoadItems::expansion` kept
            // them, F4): one store read of the record serves the text and its marks. The text
            // read above went through the loader (only it fills the items memo of these
            // records), so a missing entry is a broken invariant, filed by name (rule 3), never
            // a second read.
            let kept = self.origin_parts.borrow_mut().remove(&(krate, call));
            // The planted bug (tests/front2_token_mark_bodies.rs): the second read F4 removed.
            let kept = if self.reread_origins.get() { self.load.expansion(krate, call).map(|read| read.origins) } else { kept };
            let Some(parts) = kept else {
                self.gap("`text_marks`: the origin parts of an expansion text read through the records' loader were not kept beside it (source.rs `OriginParts`)", Some(call));
                return Err(MarksLost::NoExpansion { call });
            };
            // `LoadedExpansion::origins`: `None` is a loader that holds no origins for the call
            // and an empty call part a text that keeps none; both are a text with no column, as
            // `item_site::marks_of`'s `Ok(None)` (OPEN: some records loaders of tests/ serve
            // none, so a def-site head over records resolves at its module until they do). The
            // two parts are opened where they lie (`OriginsView::open_parts`): no joined copy.
            match parts {
                None | Some(([], _)) => Ok(None),
                Some((call_part, column)) => {
                    self.origins_opened.set(self.origins_opened.get() + 1);
                    let column = OriginsView::open_parts(call_part, column).map(|view| view.column).map_err(|error| RootsLost::NoOrigins { error });
                    marks_of_column(call, text, column)
                }
            }
        })();
        self.marks.borrow_mut().insert((krate, call), answer);
        answer
    }

    fn extern_abi(self, key: ItemKey) -> Option<Option<&'n str>> {
        // A member of a trait or an impl is never an `extern` block's.
        if self.member_owner(key).is_some() {
            return Some(None);
        }
        let Some(kind) = self.kind_of_item(key) else {
            // No row: an item declared in a body (its key is its body's lowering's, front2
            // locals.rs, and its block's ABI is `Locals::foreign`'s, which every head and
            // program reader asks first: heads.rs `fn_head`, program/head_abi.rs `placed_fn`),
            // or a key of no item. Never answered as "no block" (rule 3): named in the log, so
            // a reader that asked it anyway is found by the key it asked.
            self.gap("`extern_abi` of a key the items record holds no row for (an item declared in a body is answered by its body's lowering, `Locals::foreign`)", Some(key));
            return None;
        };
        match kind {
            // Only a fn or a static can be a foreign item: its items row's `foreign` field
            // (`Records::block_abi`), which needs no text read.
            ItemKind::Function | ItemKind::Static => self.block_abi(key),
            _ => Some(None),
        }
    }

    /// The crate's items record's value table (blaze-scope items_record/values.rs): one binary
    /// search, no text read, nothing expanded again (rule 77). A refusal comes back as the words
    /// the solve wrote with the record.
    fn attr_value(self, key: ItemKey, attr: &'static str) -> Option<Option<RowValue<'n>>> {
        let Some(krate) = self.items.crate_of(key) else {
            self.gap("`attr_value` of a key of no crate the records hold", Some(key));
            return None;
        };
        // No record: the loader filed `NoRecord` the one time it was asked.
        let record = self.items_of(krate)?;
        Some(self.read(krate, record.attr_value(key, attr))?.map(|answer| answer.map_err(RowRefusal::Recorded)))
    }

    fn kind_of_item(self, key: ItemKey) -> Option<ItemKind> {
        let krate = self.items.crate_of(key)?;
        let record = self.items_of(krate)?;
        Some(self.read(krate, record.identity(key))??.kind)
    }

    fn detail_of(self, key: ItemKey) -> Option<ItemDetail> {
        let krate = self.items.crate_of(key)?;
        let record = self.items_of(krate)?;
        let row = self.read(krate, record.identity(key))??;
        let detail = detail_from(row.kind, row.body_holds_items);
        if detail.is_none() {
            self.gap("an outline entry's detail beyond its kind and its body-holds-items flag (question 11: the file's own FileItems record)", Some(key));
        }
        detail
    }

    fn is_ctor(self, key: ItemKey) -> bool {
        let Some(krate) = self.items.crate_of(key) else { return false };
        let Some(record) = self.items_of(krate) else { return false };
        matches!(self.read(krate, record.identity(key)), Some(Some(row)) if row.ctor)
    }

    fn each_member(self, owner: ItemKey, each: &mut dyn FnMut(MemberOf<'n>)) -> bool {
        let Some(krate) = self.items.crate_of(owner) else { return false };
        if self.items_of(krate).is_none() {
            return false;
        }
        let Some(Some(rows)) = self.read(krate, self.items.members(owner)) else { return false };
        for row in rows.iter() {
            // A member whose row or text does not read is an error filed; the rest are still
            // given, and the log voids the result.
            let Some(row) = self.read(krate, row) else { continue };
            // Its entry and text as they are now (item_grain.rs `member_now`).
            let Some((item, text)) = self.member_now(krate, owner, &row) else { continue };
            each(MemberOf {
                key: row.key,
                name: row.name,
                item,
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
            // A refused derive's gap has no impl item and no header text: it is filed from its
            // row alone (buckets.rs `file_rows`, D193 point 5), never read as text.
            if row.refused_call {
                continue;
            }
            // At item grain (item_grain.rs): a body edit in the impl's file leaves its header.
            let Some(now) = self.row_now(krate, row.key) else { continue };
            let header = match now.impl_header {
                Some(span) => match self.span_now(krate, span) {
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
        self.read(krate, record.bucket_digest(key, self.alloc))
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
        self.read(krate, record.unfiled_digest(self.alloc))
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
        let Some(span) = now.impl_header else {
            self.gap("an impl's header text (its item row has no header range)", Some(key));
            return None;
        };
        let header = self.span_now(krate, span)?;
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
        Ok(self.macro_at(module, written)?.src.map(|s| s.body))
    }

    fn macro_at(self, module: ItemKey, written: Written<'_, &str>) -> Result<MacroAt<'n>, PathError> {
        let found = self.walk(module, Namespace::Macros, written)?;
        let len = written.segments.len();
        let def = found.whole(len).ok_or(PathError { module, at: found.consumed, why: Why::NotAModule })?;
        let unread = || PathError { module, at: len.saturating_sub(1), why: Why::NotRead { module: def.key, layer: Unheld::Step } };
        let krate = self.items.crate_of(def.key).ok_or_else(unread)?;
        // No record: the loader filed `NoRecord`; the path is then one into a crate not read.
        let record = self.items_of(krate).ok_or_else(unread)?;
        let src = self.items.macro_src(def.key).map_err(|error| {
            self.log.put(SourceError::Items { krate, name: self.log.name(krate), error });
            unread()
        })?;
        Ok(MacroAt { krate, def, src, crate_key: record.key(), proc_key: record.proc_key(), edition: record.edition() })
    }

    fn module_path(self, module: ItemKey) -> Option<&'n str> {
        let krate = self.items.crate_of(module)?;
        self.items_of(krate)?;
        self.read(krate, self.items.module(module))??.path
    }

    /// From the failed-calls section of the names record of the call's crate, by its key's
    /// prefix (blaze-scope names_record.rs `NamesRecord::failed`): no other crate's record is
    /// opened for it.
    fn failed_call<'b>(self, call: ItemKey, bump: Bump<'b>) -> Option<blaze_scope::names_record::FailedRow<'b>>
    where
        'n: 'b,
    {
        let names = self.names_of(self.items.crate_of(call)?)?;
        match names.failed(call, bump) {
            Ok(row) => row,
            Err(_) => {
                self.gap("failed_call: a names record's failed-call row did not decode", Some(call));
                None
            }
        }
    }

    fn each_item(self, krate: usize, each: &mut dyn FnMut(ItemKey, ItemKey)) -> bool {
        let Some(record) = self.items_of(krate) else { return false };
        // The item rows are sorted by key.
        for i in 0..record.items() {
            if let Some(Some(row)) = self.read(krate, record.identity_at(i)) {
                each(row.key, row.module);
            }
        }
        true
    }

    fn block_binding(self, module: ItemKey, ns: Namespace, name: &str) -> Option<Option<blaze_surface::Binding>> {
        Some(self.names_of(self.crate_of(module)?)?.scope(module)?.get(ns, name))
    }

    fn binds(self, module: ItemKey, ns: Namespace, name: &'n str) -> bool {
        // The names record holds a block's module as any module: its writer writes a scope row
        // for every finished scope of the crate (`names_record::encode`: `CrateNames::done`),
        // the table `Names::lookup` itself reads. `names::binds` looks the name up IN `module`:
        // its own binding, then its glob edges, as `Names::lookup` does. (A walk of a written
        // `self::name` is not that: `self` passes over a block's module, so it never read the
        // block's own items.) `false` is the answer "no"; a refusal (the scope is not held, an
        // ambiguous or unbounded glob walk) is no answer: a typed gap naming the module, never
        // taken for "no".
        let bound = names::binds_in(&self.paths, module, ns, name, &mut self.scratch.borrow_mut());
        match bound {
            Ok(bound) => bound,
            Err(PathError { why: Why::NotRead { .. }, .. }) => {
                self.gap("whether a block's module binds a name: the names record holds no scope row (or no chain) for the module asked", Some(module));
                false
            }
            Err(_) => {
                self.gap("whether a block's module binds a name: the lookup in the module was refused (ambiguous or past its bound), where `Names::lookup` reads the binding itself", Some(module));
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

    fn declared_lifetimes(self, key: ItemKey) -> Option<u32> {
        if self.no_lifetimes.get() == Some(key) {
            return None;
        }
        let krate = self.items.crate_of(key)?;
        // No record: the loader filed `NoRecord` the one time it was asked; a refused read
        // is filed by `read`.
        let record = self.items_of(krate)?;
        self.read(krate, record.declared_lifetimes(key))?.map(u32::from)
    }

    fn assoc_lifetimes(self, trait_: ItemKey, name: blaze_typeck::Name) -> Option<u32> {
        if self.no_lifetimes.get() == Some(trait_) {
            return None;
        }
        let krate = self.items.crate_of(trait_)?;
        let record = self.items_of(krate)?;
        self.read(krate, record.assoc_lifetimes(trait_, |m| blaze_typeck::Name::of(m) == name))?.map(u32::from)
    }
}

#[cfg(test)]
mod tests {
    use core::cell::Cell;

    use blaze_id::{ContentId, CrateItemKey};
    use blaze_own::Alloc;
    use blaze_scope::graph::Sysroot;
    use blaze_scope::items_record;
    use blaze_scope::names_record::{self, Verified};
    use blaze_scope::resolve::{CrateInput, SourceTexts, aligned, resolve_crates};
    use blaze_surface::Edition;
    use blaze_unit_graph::{Implicit, Prelude, StdIs};
    use bstd::mem::Frame;

    use super::*;

    const LIB_SRC: &str = "#![no_core]\npub struct Alpha;\n";
    const PM_SRC: &str = "#![no_core]\npub struct Beta;\n";

    struct MemTexts;
    impl<'a> SourceTexts<'a> for MemTexts {
        fn read(&self, _krate: usize, path: &str) -> Option<&'a str> {
            match path {
                "/0/lib/lib.rs" => Some(LIB_SRC),
                "/1/pm/lib.rs" => Some(PM_SRC),
                _ => None,
            }
        }
    }

    struct FastLoad<'n> {
        names_bytes: &'n [&'n [u8]],
        proofs: &'n [Verified<'n>],
        items_bytes: &'n [&'n [u8]],
        proc_macros: &'n [bool],
        use_fast: bool,
        names_calls: Cell<u32>,
        items_calls: Cell<u32>,
    }

    impl<'n> Load<'n> for FastLoad<'n> {
        fn names(&self, krate: usize) -> Option<&'n [u8]> {
            self.names_calls.set(self.names_calls.get() + 1);
            self.names_bytes.get(krate).copied()
        }

        fn names_verified(&self, krate: usize) -> Option<Verified<'n>> {
            self.use_fast.then(|| self.proofs.get(krate).copied()).flatten()
        }

        fn items(&self, krate: usize) -> Option<&'n [u8]> {
            self.items_calls.set(self.items_calls.get() + 1);
            self.items_bytes.get(krate).copied()
        }

        fn proc_macro(&self, krate: usize) -> Option<bool> {
            self.use_fast.then(|| self.proc_macros.get(krate).copied()).flatten()
        }

        fn file(&self, _krate: usize, _path: PackagePath<'_>) -> Option<&'n str> {
            None
        }

        fn expansion(&self, _krate: usize, _call: ItemKey) -> Option<LoadedExpansion<'n>> {
            None
        }
    }

    #[test]
    fn names_verified_and_proc_macro_fast_paths_skip_recheck_and_items_open() {
        let g = blaze_own::testing::heap().alloc();
        let frame = Frame::with_reserve(1 << 20, bstd::mem::Tag::Other).expect("test frame");
        let erased = blaze_own::Erased::new(&frame);
        let bump = erased.bump();
        let plain = Implicit { std: StdIs::Rust, prelude: Prelude::Rust(edition::Edition::Edition2021), proc_macro: false, entry: false };
        let pm = Implicit { proc_macro: true, ..plain };
        let inputs = [
            CrateInput {
                name: "lib",
                key: CrateItemKey(ContentId::of("front2 source test", b"lib")),
                edition: Edition::E2021,
                implicit: plain,
                root: "/0/lib/lib.rs",
                shown_from: "/",
                features: &[],
                deps: &[],
                cfg: &[],
                script: None,
            },
            CrateInput {
                name: "pm",
                key: CrateItemKey(ContentId::of("front2 source test", b"pm")),
                edition: Edition::E2021,
                implicit: pm,
                root: "/1/pm/lib.rs",
                shown_from: "/",
                features: &[],
                deps: &[],
                cfg: &[],
                script: None,
            },
        ];
        let solved = resolve_crates(&inputs, Sysroot { core: None, alloc: None, std: None, bstd: None }, &MemTexts, &blaze_scope::task::Serial, bump)
            .expect("solve test crates");
        let n0 = aligned(&names_record::encode(&solved.crates()[0], g).expect("encode n0"), bump);
        let n1 = aligned(&names_record::encode(&solved.crates()[1], g).expect("encode n1"), bump);
        let i0 = aligned(&items_record::encode(&solved.crates()[0], g).expect("encode i0"), bump);
        let i1 = aligned(&items_record::encode(&solved.crates()[1], g).expect("encode i1"), bump);
        let p0 = Verified::check(n0).expect("verify n0");
        let p1 = Verified::check(n1).expect("verify n1");
        let keys = [inputs[0].key, inputs[1].key];
        let names_bytes = [n0, n1];
        let proofs = [p0, p1];
        let items_bytes = [i0, i1];
        let proc_macros = [false, true];
        let crate_names = ["lib", "pm"];

        let root0 = solved.crates()[0].root;
        // Fast path: `names_verified` and `proc_macro` provided.
        let fast_pm = {
            let fast_frame = Frame::with_reserve(1 << 20, bstd::mem::Tag::Other).expect("fast frame");
            let fast_load = FastLoad { names_bytes: &names_bytes, proofs: &proofs, items_bytes: &items_bytes, proc_macros: &proc_macros, use_fast: true, names_calls: Cell::new(0), items_calls: Cell::new(0) };
            let fast_log = fast_frame.alloc(Log::new(&crate_names, g));
            let fast = fast_frame.alloc(Records::new(&keys, &fast_load, fast_log, &fast_frame, g));
            let pm0 = NamesSource::proc_macro(&*fast, 0);
            let pm1 = NamesSource::proc_macro(&*fast, 1);
            assert_eq!(pm0, Some(false));
            assert_eq!(pm1, Some(true));
            assert_eq!(fast_load.items_calls.get(), 0, "proc_macro fast-path must not open ItemsRecord");
            assert_eq!(fast.opened().items.crates, 0, "no ItemsRecord decoded for proc_macro");
            assert!(NamesSource::binds(&*fast, root0, Namespace::Types, "Alpha"));
            assert_eq!(fast_load.names_calls.get(), 0, "names_verified fast-path must not call Load::names");
            assert!(fast_log.errors().is_empty(), "zero source errors on fast path");
            (pm0, pm1)
        };

        // Fallback path: default `None` for both hooks matches byte-for-byte.
        {
            let slow_frame = Frame::with_reserve(1 << 20, bstd::mem::Tag::Other).expect("slow frame");
            let slow_load = FastLoad { names_bytes: &names_bytes, proofs: &proofs, items_bytes: &items_bytes, proc_macros: &proc_macros, use_fast: false, names_calls: Cell::new(0), items_calls: Cell::new(0) };
            let slow_log = slow_frame.alloc(Log::new(&crate_names, g));
            let slow = slow_frame.alloc(Records::new(&keys, &slow_load, slow_log, &slow_frame, g));
            assert_eq!(NamesSource::proc_macro(&*slow, 0), fast_pm.0);
            assert_eq!(NamesSource::proc_macro(&*slow, 1), fast_pm.1);
            assert_eq!(slow_load.items_calls.get(), 2, "fallback opens ItemsRecord for each crate");
            assert!(NamesSource::binds(&*slow, root0, Namespace::Types, "Alpha"));
            assert_eq!(slow_load.names_calls.get(), 1, "fallback calls Load::names");
            assert!(slow_log.errors().is_empty(), "zero source errors on fallback path");
        }
    }
}
