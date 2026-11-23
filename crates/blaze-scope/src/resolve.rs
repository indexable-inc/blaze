//! Names of a graph of crates, end to end (docs/v2/plan/design/bottom.md, section 6): one call,
//! [`resolve_crates`], from each crate's root file to every module's finished scope.
//!
//! Per crate, after the crates it depends on: its files read through the caller's
//! [`SourceTexts`] (the library does no IO) and outlined, the module tree walked, each module's
//! text lowered to its declarations, the `CrateRoot` (extern prelude, deps, `#[macro_use]`
//! prelude, the standard prelude from the graph's rule), each record written and read back
//! (what a later crate reads is the record, never the lowering), then the names fill
//! ([`crate::task::Fill`]), which expands each item macro it names as it names it. A lookup
//! that leaves the crate reads the other crate's finished scopes, never resolving it again.
//!
//! Memory: everything is made in the one [`Alloc`] passed in and lives as long as it; texts
//! made here (expansions, paths, reasons) are leaked into it, so a caller passes a frame it
//! frees whole. The written records are copied to 16-byte aligned blocks in it before they are
//! read back in place.
//!
//! Threads: the fill runs its waves through the caller's [`Par`], and an expansion is made
//! inside a wave, so what expansions add (their declarations, the modules they write, the
//! macros, calls and enums they declare, the files those modules are in) is held behind
//! `std::sync` locks: the example this began in used them, and blaze's named locks
//! (`blaze_core::lock`) live in a crate with the OS under it, which this sans-IO crate does not
//! take. Crates of one wave are resolved one after another, each with the whole `Par`.
//!
//! What is refused stays refused: a procedural macro (attribute, derive or function-like) is
//! not run (the host answers `Refused`); a derive is not expanded (it writes impls, no names);
//! an eager builtin whose arguments need expanding first is noted, not driven.
#![expect(
    clippy::disallowed_types,
    reason = "std locks between the fill's threads, as the module docs say; blaze_core::lock is not a dependency of this sans-IO crate"
)]

use core::convert::Infallible;
use core::fmt::{self, Write as _};
use std::sync::{LockResult, Mutex, RwLock};

use blaze_expand::{
    BuiltinAttr, BuiltinDerive, Call, CallSite, Ctx, DefAt, ItemAttrs, ItemAttrsError, NextAttr, DefSite, Delim, Diagnostic, ExpandCx, ExpandError, Expanded,
    Export, HostAsk, HostError, HostWhy, IncludeAs, Included, LexError, LitKind, LocalInner,
    MacroDefinition, Place as HostPlace, PlaceOf, ProcCall, Produced, Raw, TokenKind,
    TokenStream, expand, lex, CallKind, Inside, NestedCall, NestedKind, PathScope, call_key,
};
use blaze_file_items::{Outline, outline};
use blaze_format::FormatError;
use blaze_hash::HashMap;
use blaze_id::{ContentId, CrateItemKey, ItemKey};
use blaze_own::{AString, AVec, Alloc, BString, BVec, Bump};
pub use blaze_own::found::{Found, Unread};
use blaze_surface::{
    Binding, BindingState, BuildError, BuiltinFn, CallShape, CrateRootBuilder, CrateRootView,
    DeclsBuilder, Def, DefKind, Edition, ExternCrateDecl, ExternTarget, ItemIndex, MacroShapes, ModuleDeclsView,
    ModuleInfo, Namespace, Origin, Rename, ScopeBuilder, SurfaceError, Visibility, WrittenVis,
};
use blaze_build_script::{cx::ScriptCx, outputs::OutputsRef};
use blaze_unit_graph::Implicit;

use crate::graph::{CrateGraph, CrateId, ExternGone, GraphCrate, GraphDep, GraphError, RootAttrs, Sysroot};
use crate::names::{PathError, Unheld, VariantHit, Why, World, Written, variant_in};
use crate::task::{
    CallSrc, Cfg, Fill, FillEnv, FillReport, Files, First, Fragment, Held, ImplSrc,
    ItemText, LangSrc, Loc, LowerCx, LowerNote, Lowered, MacroBody, MacroSrc, Member, ModMade, Par,
    PreludeImport, RootInput, RootNote, RootPlan, TaskError, Tree, Variant, Where,
    crate_root, crate_tools, file_attrs, lower, walk,
};
use crate::task::{joined, pop_dir};
use crate::task::ExpandPart;
use crate::task::{AttrOrder, Attrs, inline_child, walk_rows};
use crate::tree_record::{TreeEnum, TreeModule, TreeRecord};
use crate::task::{ProcAttr, ProcDecls, ProcExport};
use blaze_surface::ProcKind;
use crate::{ExpandRequest, NameAnswer, Node, Passed, Resolved, ScopeFacts};
use crate::crate_scope::Dep;
use crate::prepares::{Kept, Prepares};

// ---------------------------------------------------------------------------------------------
// The input.

/// Where a crate's texts come from: the caller reads them (from disk, the store, a test's
/// table). Called from the fill's threads at once, for the files a macro's `mod x;` names.
pub trait SourceTexts<'a> {
    /// A TABLE's text of the file at `path` (absolute, `/`-separated, without `.` or `..`) of
    /// the crate at `krate` (its index in the inputs), or `None` when the table has no such
    /// file: the form for a source that cannot fail to read (a test's table, the toolchain's
    /// records), reached through [`SourceTexts::source`]'s default, as [`Found::of_table`]; a
    /// source that reads a disk or a store gives [`SourceTexts::source`] itself, with its
    /// failures as [`Found::Unknown`]. PORT IN PROGRESS (2026-11-21): the walk's reads
    /// (`walked_files`, `from_tree_record`) ask `source`; the include read (`Host::include`),
    /// `file_at`, lib_features.rs and the front2 wrappers still ask this, and it goes once they
    /// and the table implementors are moved to `source`.
    fn read(&self, krate: usize, path: &str) -> Option<&'a str> {
        let _ = (krate, path);
        None
    }

    /// The text of the file at `path` of the crate at `krate`: [`Found::Known`],
    /// [`Found::Absent`] when there is no such file, or [`Found::Unknown`] when it could not
    /// be read (a permission, a read that moved, the driver): the asker fails, naming the file
    /// and the cause, and never reads it as absent (D193 point 5). By default the table's
    /// [`SourceTexts::read`].
    fn source(&self, krate: usize, path: &str) -> Found<&'a str, Unread<'a>> {
        Found::of_table(self.read(krate, path))
    }

    /// A TABLE's bytes of the file `from` reads as data ([`SourceTexts::source_data`]), as
    /// [`SourceTexts::read`] is a table's text.
    fn read_data(&self, krate: usize, from: &str, path: &str) -> Option<&'a [u8]> {
        let _ = from;
        self.read(krate, path).map(str::as_bytes)
    }

    /// The bytes of the file at `path` (as [`SourceTexts::source`]'s) that the file `from` (the
    /// calling file, absolute) reads as DATA by `include_str!` or `include_bytes!`, as
    /// [`SourceTexts::source`] answers. Data is no module: a source that records what a crate
    /// read records it as a read of the bytes, never as a module file wanting its surface
    /// (blaze-cli names.rs). By default the table's [`SourceTexts::read_data`]; a file that
    /// is not UTF-8 is then absent.
    fn source_data(&self, krate: usize, from: &str, path: &str) -> Found<&'a [u8], Unread<'a>> {
        Found::of_table(self.read_data(krate, from, path))
    }

    /// A read of a call in the crate at `krate` that left its own package and stayed inside
    /// the workspace (decision D111: allowed and reported): an include's (asked before the
    /// file is read) or a member's proc macro's (from its read list, after the run or the
    /// kept answer: `Host::run_proc_macro`). A source that files findings files it as a
    /// warning at the call (blaze-cli names/include_outside.rs). By default nothing is filed
    /// (a source with no record: a test's table).
    fn left_package(&self, krate: usize, left: crate::include_place::LeftPackage<'_>) {
        let _ = (krate, left);
    }

    /// The key the proc-macro runner knows the proc-macro crate at `krate` by (blaze-proc-macro's
    /// `hook::crate_key` of its module key): what its macros' definitions name it by. `None`:
    /// no runner has it, and each of its macros' calls is refused, one note per crate.
    fn proc_macro_key(&self, krate: usize) -> Option<CrateItemKey> {
        let _ = krate;
        None
    }

    /// The macros the built module of the proc-macro crate at `krate` declares to the host:
    /// what the crate exports, declared at its root in place of its text's `#[proc_macro*]`
    /// attributes. `None`: no module is held, and the text is read. The list is a function of
    /// the module, whose key [`SourceTexts::proc_macro_key`] is made from.
    fn proc_exports(&self, krate: usize) -> Option<&'a [ProcExport<'a>]> {
        let _ = krate;
        None
    }

    /// Runs the proc macro `def` (a definition of a crate [`SourceTexts::proc_macro_key`] gave)
    /// on `call`, a call written in the crate at `krate` (its package and edition are the run's):
    /// blaze-proc-macro's hook. Without a runner every call is refused.
    fn run_proc_macro<'b>(&self, krate: usize, def: &MacroDefinition<'_>, call: ProcCall<'_>, alloc: Alloc<'b>) -> Result<Produced<'b>, HostError> {
        let _ = (krate, def, call, alloc);
        Err(REFUSED)
    }

    /// Why the proc macro `def`, called in the crate at `krate`, was refused, asked just after a
    /// run of it was ([`SourceTexts::run_proc_macro`]'s bare `HostError`): the runner's words
    /// naming the macro's crate and why its module cannot run the macro (not in the store under
    /// the key asked, does not load, exports no such macro: [`ProcUnusable::Module`]), or, its
    /// module able to, why that run stopped ([`ProcUnusable::Run`]: a sandbox limit naming the
    /// path as the macro named it, a variable, a failure). `None`: the runner kept no words, or
    /// no runner here (a test's table).
    fn proc_macro_unusable(&self, krate: usize, def: &MacroDefinition<'_>) -> Option<ProcUnusable<'a>> {
        let _ = (krate, def);
        None
    }

    /// The environment the crate at `krate` is compiled in ([`crate::crate_env::CrateEnv`]:
    /// one per unit, made by `UnitCrates::env_of`): what `env!` and `option_env!` read, the
    /// same for its items and its bodies. `None`: this source has none, and every `env!` of
    /// the crate is refused by name, never answered unset.
    fn crate_env(&self, krate: usize) -> Option<&crate::crate_env::CrateEnv<'a>> {
        let _ = krate;
        None
    }

    /// The planted bug of the attribute-path tests (rule 23,
    /// tests/attr_path_from_expansion.rs): [`AttrPlant::Spelling`] finds an attribute or derive
    /// call on its item by the path's text with the spaces taken out, as before, so a `$crate`
    /// an expansion wrote is not found. Every real source answers [`AttrPlant::None`].
    fn attr_plant(&self) -> AttrPlant {
        AttrPlant::None
    }

    /// How a derived item's derives and the attributes after them are sent (attr-order-1,
    /// tests/attr_order.rs): every real source answers [`AttrOrder::Written`], the Reference's
    /// order; [`AttrOrder::DerivesAtOnce`] is that test's planted bug.
    fn attr_order(&self) -> AttrOrder {
        AttrOrder::Written
    }

    /// The planted bug of the body-glob test (rule 23, blaze-ladder
    /// tests/front2_body_scope_impls.rs): [`ReachPlant::Unplaced`] places no finished module
    /// for a body solve ([`crate::cells::Outside::chain`] answers `None`), so a glob of a
    /// module an expansion wrote outside the body is refused (`GlobSource`) as before D189's
    /// fix. Every real source answers [`ReachPlant::None`].
    fn reach_plant(&self) -> ReachPlant {
        ReachPlant::None
    }
}

/// How a body solve reaches a finished module no text it reads names
/// ([`SourceTexts::reach_plant`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ReachPlant {
    /// Through its ancestors' finished scopes, from the crate's chains: the one way.
    None,
    /// The planted bug: not reached; a glob of it is refused.
    Unplaced,
    /// The planted cause of bodies × modules (blaze-ladder tests/front2_body_scope_scratch.rs):
    /// each body solve makes its crate's whole module tree at its load, as before
    /// `Cells::load_body`, and copies and indexes every file of its crate, as before
    /// `FileSet::over`; placed as [`ReachPlant::None`].
    WholeTree,
}

/// How an attribute or derive call is found among its item's attributes
/// ([`SourceTexts::attr_plant`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AttrPlant {
    /// By the path's words (blaze-expand `same_path`): the one way.
    None,
    /// The planted bug: by the path's text, spaces taken out.
    Spelling,
}

/// One crate to resolve. Its index in the slice given to [`resolve_crates`] is its
/// [`CrateId`] in the graph, and what [`SourceTexts::read`] is told.
#[derive(Clone, Copy)]
pub struct CrateInput<'a> {
    /// Its library name: what a dependent calls it unless it renames it.
    pub name: &'a str,
    /// The key every item of it is under.
    pub key: CrateItemKey,
    /// The Rust edition its syntax is.
    pub edition: Edition,
    /// What it is given without naming it (blaze-unit-graph's `implicit`): under a blaze
    /// edition its `std` is bstd and its prelude `std::prelude::blaze_2026_11`.
    pub implicit: Implicit,
    /// Its root file's path.
    pub root: &'a str,
    /// The crate's package directory in the caller's path space: a file's path in its package
    /// ([`SourceFile::package`], what records hold, D96) and its shown path (`file!()`) are both
    /// its path past this directory.
    pub shown_from: &'a str,
    /// The features turned on, sorted (the graph's record of them).
    pub features: &'a [&'a str],
    /// Its dependencies: the name it calls each, and that crate's index.
    pub deps: &'a [(&'a str, usize)],
    /// The cfg its text is lowered under, each `(name, value)`.
    pub cfg: &'a [(&'a str, Option<&'a str>)],
    /// Its build script's outputs, read in place from the script's record, when it has a
    /// script: `env!`/`include!` of `OUT_DIR` read them (their cfgs are already in `cfg`).
    pub script: Option<OutputsRef<'a>>,
}

/// A file of a crate, read and outlined once.
#[derive(Debug)]
pub struct SourceFile<'a> {
    /// Its path in the caller's path space (blaze-world's `/<tree>/...`): what the solve reads
    /// it by, never what a record holds.
    pub path: &'a str,
    /// Its path in its package, past [`CrateInput::shown_from`] (D96): what a record holds
    /// and a reader finds the file by, with the crate it reads for.
    pub package: crate::package_path::PackagePath<'a>,
    /// The path `file!()` gives: past [`CrateInput::shown_from`].
    pub shown: &'a str,
    contents: DigestedText<'a>,
    pub outline: Outline<'a>,
}

/// The immutable source bytes paired with the digest computed for them by the solve.
/// Files and expansions expose this pair read-only, including to their record writers.
#[derive(Debug)]
pub struct DigestedText<'a> {
    pub text: &'a str,
    pub id: ContentId,
}

/// An owned outline whose columns can travel from a file worker to the crate's frame.
pub struct SharedOutline<'a>(Outline<'a>);

// SAFETY: the only constructor copies the parser's plain columns into a SharedAlloc.
// No input text or parser scratch is borrowed, and no method exposes the erased allocator.
unsafe impl Send for SharedOutline<'_> {}

impl<'a> SharedOutline<'a> {
    pub fn parse(text: &str, edition: Edition, heap: blaze_own::SharedAlloc<'a>) -> Self {
        Self(outline(text, edition, heap.local()))
    }

    pub fn into_frame<'b>(self, alloc: Alloc<'b>) -> Outline<'b> {
        self.0.copy_in(alloc)
    }
}

impl<'a> core::ops::Deref for SourceFile<'a> {
    type Target = DigestedText<'a>;

    fn deref(&self) -> &Self::Target { &self.contents }
}

/// Where an item's text is: a file of its crate, or the expansion of a macro call.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum At {
    File(u32),
    /// The expansion of the call with this key ([`CrateNames::expansions`]).
    Expansion(ItemKey),
}

/// Where an item is: its crate, the text it is in, and its entry in that text's outline.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Place {
    pub krate: usize,
    pub at: At,
    pub item: ItemIndex,
}

// ---------------------------------------------------------------------------------------------
// The output.

/// The lowered tables of a crate: of its files' texts and of every expansion made.
#[derive(Debug)]
pub struct Tables<'a> {
    pub macros: HashMap<ItemKey, MacroSrc<'a>, Alloc<'a>>,
    pub calls: HashMap<ItemKey, CallSrc<'a>, Alloc<'a>>,
    pub enums: HashMap<ItemKey, AVec<'a, Variant<&'a str>>, Alloc<'a>>,
    pub impls: AVec<'a, ImplSrc<'a>>,
    pub langs: AVec<'a, LangSrc<'a>>,
    /// Each fragment's `use` items' places, by the fragment's key (a module's, a call's).
    pub use_locs: HashMap<ItemKey, AVec<'a, Loc>, Alloc<'a>>,
    /// Each item's text, and which text it is in.
    pub texts: HashMap<ItemKey, (ItemText<'a>, At), Alloc<'a>>,
    /// The other way: the item at an entry of a text's outline.
    pub by_place: HashMap<(At, ItemIndex), ItemKey, Alloc<'a>>,
    /// Each trait's and impl's members on under the cfg, in declaration order.
    pub members: HashMap<ItemKey, AVec<'a, Member<'a>>, Alloc<'a>>,
    /// The tuple and unit structs, each its own constructor.
    pub ctors: HashMap<ItemKey, (), Alloc<'a>>,
    /// The `link_name`/`export_name` values written as macro calls, each at its item's place:
    /// expanded once the names are solved ([`crate::attr_values::value_rows`]).
    pub values: AVec<'a, crate::attr_values::ValueSite<'a>>,
}

impl<'a> Tables<'a> {
    fn new_in(alloc: Alloc<'a>) -> Tables<'a> {
        Tables {
            values: AVec::new_in(alloc),
            macros: HashMap::new_in(alloc),
            calls: HashMap::new_in(alloc),
            enums: HashMap::new_in(alloc),
            impls: AVec::new_in(alloc),
            langs: AVec::new_in(alloc),
            use_locs: HashMap::new_in(alloc),
            texts: HashMap::new_in(alloc),
            by_place: HashMap::new_in(alloc),
            members: HashMap::new_in(alloc),
            ctors: HashMap::new_in(alloc),
        }
    }

    /// `list` joined to `owner`'s members: an owner's body and each expansion of a call in it
    /// (in any order the fill finishes them) give a part. Kept in one order whatever the
    /// order of joining (deterministic): the body's in declaration order, then each
    /// expansion's, by its call's key, in its own declaration order.
    fn join_members(&mut self, owner: ItemKey, list: AVec<'a, Member<'a>>) {
        match self.members.get_mut(&owner) {
            None => {
                self.members.insert(owner, list);
            }
            Some(have) => {
                have.extend(list);
                have.sort_by_key(|m| (m.written_by, m.item.0));
            }
        }
    }

    /// A fragment's tables (not its declarations, nor the modules it made) added; `owner` is
    /// the fragment's key, `at` the text it is.
    fn add(&mut self, owner: ItemKey, at: At, part: Lowered<'a, 'a>) -> AVec<'a, LowerNote<'a>> {
        let Lowered { macros, calls, enums, impls, langs, use_locs, texts, members, ctors, notes, values, .. } = part;
        for (owner, list) in members {
            self.join_members(owner, list);
        }
        self.values.extend(values);
        self.ctors.extend(ctors.into_iter().map(|k| (k, ())));
        self.macros.extend(macros);
        // The one door a lowered call enters the tables by: its calling text is `at`
        // (crate::expansion_origins `CallInText::text`).
        self.calls.extend(calls.into_iter().map(|(call, mut src)| {
            if let Some(place) = src.place.as_mut() {
                place.text = Some(at);
            }
            (call, src)
        }));
        self.enums.extend(enums);
        self.impls.extend(impls);
        self.langs.extend(langs);
        self.use_locs.insert(owner, use_locs);
        for t in texts.into_iter() {
            self.by_place.insert((at, t.item), t.key);
            self.texts.insert(t.key, (t, at));
        }
        notes
    }

    fn merge(&mut self, other: Tables<'a>) {
        let Tables { macros, calls, enums, impls, langs, use_locs, texts, by_place, members, ctors, values } = other;
        self.by_place.extend(by_place);
        self.values.extend(values);
        for (owner, list) in members {
            self.join_members(owner, list);
        }
        self.ctors.extend(ctors);
        self.macros.extend(macros);
        self.calls.extend(calls);
        self.enums.extend(enums);
        self.impls.extend(impls);
        self.langs.extend(langs);
        self.use_locs.extend(use_locs);
        self.texts.extend(texts);
    }
}

/// A module a macro wrote (or one inside it): what the walk's `Module` and its record hold
/// for a walked one.
#[derive(Debug)]
pub struct MadeModule<'a> {
    pub info: ModuleInfo<&'a str>,
    pub decls: DeclsBuilder<'a, &'a str>,
    /// Itself, then each module up to the crate's root.
    pub chain: AVec<'a, ItemKey>,
    pub path: &'a str,
    /// Its children's directory, and what their `#[path]` is relative to.
    pub dir: &'a str,
    pub path_dir: &'a str,
    /// The text its items are in, and its own item there (`None`: the whole file).
    pub at: At,
    pub item: Option<ItemIndex>,
}

/// An expansion's text, outlined, with its origins (crate::expansion_origins: where each token
/// came from, and the call it maps to). `Err`: the text is no rendering the column places, or the
/// column could not be made: a finding in it says so, never maps by a guess.
#[derive(Debug)]
pub struct ExpansionText<'a> {
    contents: DigestedText<'a>,
    pub outline: Outline<'a>,
    pub origins: Result<&'a [u8], crate::expansion_origins::OriginsError>,
    /// `origins`' column, opened and verified ONCE when the text is made
    /// ([`crate::crate_roots::column_in`], rule 74): what the marks of every head and body of
    /// this text are read by (front2 item_site.rs `marks_of_column`), never re-opened per ask.
    pub column: Result<blaze_expand::origin_column::OriginsRef<'a>, crate::crate_roots::RootsLost>,
}

impl<'a> ExpansionText<'a> {
    /// The text with its column opened once (the field's docs).
    pub fn new(text: &'a str, outline: Outline<'a>, origins: Result<&'a [u8], crate::expansion_origins::OriginsError>) -> ExpansionText<'a> {
        let column = crate::crate_roots::column_in(&origins);
        let contents = DigestedText { text, id: crate::items_record::expansion_id(text) };
        ExpansionText { contents, outline, origins, column }
    }
}

impl<'a> core::ops::Deref for ExpansionText<'a> {
    type Target = DigestedText<'a>;

    fn deref(&self) -> &Self::Target { &self.contents }
}

/// A macro call's expansion: what it adds to its module.
#[derive(Debug)]
pub struct Expansion<'a> {
    /// Empty when the call is not expanded: then `refused` says why (a refused call), or the
    /// call wrote nothing (a derive of a cfg-off item, a statement call that declares nothing).
    pub decls: DeclsBuilder<'a, &'a str>,
    /// `None` when no text was made.
    pub made: Option<&'a ExpansionText<'a>>,
    /// The call's expansion was refused ([`crate::refused`]): its module's scope says so, and
    /// a name looked up behind it is unknown, never absent.
    pub refused: Option<crate::refused::Refused<'a>>,
    /// For a refused call, the note that refused it (the last the solve noted for the call
    /// before it returned nothing) and the first `env!` variable the host refused during it:
    /// the reason the names record's failed-call row keeps ([`crate::call_refusal`]). `None`
    /// for a call that expanded, and for a refused call whose driver noted nothing (a bug,
    /// kept as [`crate::call_refusal::RefusalKind::Unnoted`]).
    pub refused_by: Option<(ExpandNote<'a>, Option<&'a str>)>,
}

/// Which text of a call did not lex.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Lexed {
    Body,
    Parameters,
    Input,
    /// The path of the attribute or derive the lowering read for an item call: what the call
    /// is found among the item's attributes by.
    Path,
}

/// Why a call's expansion adds nothing, or adds with a fault.
#[derive(Clone, Copy, Debug)]
pub enum ExpandNote<'a> {
    /// The fill asked for a call the lowering does not know.
    UnknownCall,
    /// An item's attributes left no answer for an attribute macro's or a derive's input.
    ItemAttrs { macro_name: &'a str, error: ItemAttrsError<Infallible> },
    /// The attribute or derive the lowering named is not among the item's attributes as
    /// blaze-expand walks them.
    AttrNotFound { name: &'a str },
    /// No definition of the macro is held (a procedural macro's, or one not read).
    NoDefinition { shape: CallShape, name: &'a str },
    Lex { macro_name: &'a str, what: Lexed, error: LexError },
    /// A builtin blaze-expand has no form of.
    BuiltinUnknown { name: &'a str },
    /// A builtin attribute or derive called function-like.
    BuiltinOtherCalled { shape: CallShape, macro_name: &'a str },
    /// An eager builtin whose arguments hold a call ([`EagerDriver`]) that cannot be expanded
    /// first: the nested call's path as written, and why.
    Eager { macro_name: &'a str, nested: &'a str, why: EagerRefusal },
    Expand { macro_name: &'a str, error: ExpandError },
    /// A diagnostic of the macro's that is no error (a proc macro's warning, note or help): its
    /// expansion is kept.
    Diagnostic { macro_name: &'a str, diagnostic: Diagnostic },
    /// The call FAILED: its expansion holds an error ([`Diagnostic::is_error`]: no rule matched,
    /// `compile_error!`, a proc macro's error). It writes nothing and is stored refused
    /// ([`crate::refused`]), so a name looked up behind it answers `Why::Refused` with this
    /// call's key, which [`crate::failed_call::FailedCall::of`] joins to this note. `path` is the
    /// macro as written (`::std::main` for the implicit call of D104: `implicit`), `failed` the
    /// expansion's own reason with its words.
    Failed { path: &'a str, implicit: bool, failed: blaze_expand::Failed<'a> },
    /// The expansion does not read as items: the outline's first syntax error.
    NotItems { macro_name: &'a str, message: &'a str },
    /// The expansion writes a macro definition holding another crate's `$crate`
    /// ([`crate::crate_roots::ForeignDollarCrate`]): refused until token-mark-3.
    ForeignDollarCrate { macro_name: &'a str, krate: CrateItemKey },
    /// The expansion writes another crate's root and keeps no marks to tell it by
    /// ([`crate::crate_roots::RootsLost`]).
    RootsLost { macro_name: &'a str, lost: crate::crate_roots::RootsLost },
    /// A call written in an expansion whose input is no range of that expansion's tokens
    /// ([`crate::nested_input`]): refused, never lexed from the rendered text.
    NestedInput { macro_name: &'a str, error: crate::nested_input::NestedInputError },
    /// A proc macro the host refused to run, and why ([`SourceTexts::proc_macro_unusable`]): its
    /// crate's module cannot run it, or its run stopped; the runner's own words in place of the
    /// expander's bare `HostError`.
    ProcModule { macro_name: &'a str, why: ProcUnusable<'a> },
}

/// Why the host refused a proc macro's run ([`ExpandNote::ProcModule`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ProcUnusable<'a> {
    /// No runner knows the crate: it has no runner key ([`SourceTexts::proc_macro_key`]), so
    /// its macros have no definition to run.
    NoRunner,
    /// The runner's words: which crate, and why its module cannot run the macro (not in the
    /// store under the key asked, does not load, exports no such macro).
    Module(&'a str),
    /// The module ran the macro and the run stopped: the runner's words naming what stopped it
    /// as the macro saw it (its sandbox refusing a path, by the path from the tree it is
    /// served; a limit; a variable this build does not know; a failure). Rule 19.
    Run(&'a str),
}

/// Why a call nested in an eager builtin's input is not expanded.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum EagerRefusal {
    /// Its path names no library builtin (only the builtins are expanded first here).
    NotBuiltin,
    /// It names a library builtin, but the crate defines a macro of that name too.
    Shadowed,
    /// A `macro_rules!` or `macro` definition, not a call.
    NotACall,
    /// More than [`EAGER_DEPTH`] calls deep.
    TooDeep,
    /// The builtin asked for its nested calls again after it was given their answers.
    AskedAgain,
    /// Its arguments could not be copied out of the input.
    Stream,
}

/// How deep calls nest inside eager builtins' inputs before the expansion is refused.
pub const EAGER_DEPTH: usize = 8;

/// Why a module a macro wrote is not made.
#[derive(Clone, Copy, Debug)]
pub enum MadeNote<'a> {
    /// More than [`MAX_MADE_MODULES`] in the crate.
    TooMany,
    /// Its parent module has no place (no directory to find its file in).
    NoPlace,
    FileNotFound { path: &'a str },
    /// Its file's `#![cfg]` is off.
    CfgOff,
}

/// What a crate's resolution could not read or do: typed, each naming its item. The walk's
/// own notes are [`Tree::notes`], the fill's failed tasks [`CrateNames::failed`].
#[derive(Clone, Debug)]
pub enum NamesNote<'a> {
    Lower(LowerNote<'a>),
    Root(RootNote<'a, 'a>),
    /// A `#[prelude_import]` in a module other than the root.
    PreludeImportOutsideRoot { module: ItemKey },
    /// A module's `ModuleDecls` record that was not written or does not read back: the
    /// module has no record, and a read of it is a task's failure.
    DeclsRecord { module: ItemKey, error: RecordError },
    Expand { call: ItemKey, at: Option<Loc>, note: ExpandNote<'a> },
    Made { name: &'a str, at: Loc, note: MadeNote<'a> },
    /// The crate `krate`'s extern prelude lacks `name`, which the rule gives it
    /// ([`CrateGraph::externs`]): its paths through `name` resolve to nothing.
    ExternDropped { krate: &'a str, name: &'static str, why: ExternGone },
}

/// Why a record is not held.
#[derive(Clone, Debug)]
pub enum RecordError {
    Refused(BuildError),
    NotRead(FormatError),
}

/// Why no names at all.
#[derive(Clone, Debug)]
pub enum NamesError<'a> {
    Graph(GraphError<'a, &'a str>),
    /// A dependency index past the inputs.
    NoSuchDep { krate: &'a str, name: &'a str, index: usize },
    RootNotRead { krate: &'a str, path: &'a str },
    /// A file of `krate` (its root, or one a `mod` names) could not be read: neither its text
    /// nor its absence is known ([`Found::Unknown`]), so no names are made from a guess.
    SourceUnread { krate: &'a str, error: Unread<'a> },
    /// The walk kept finding files past [`MOST_WALKS`] passes.
    WalkUnbounded { krate: &'a str },
    OutlineWorker { krate: &'a str, path: &'a str, why: &'a str },
    OutlineCount { krate: &'a str, files: usize, outlines: usize },
    RootRecord { krate: &'a str, error: RecordError },
    /// A crate the graph's waves never reached.
    NotResolved { krate: &'a str },
    /// `krate`'s root names the crate `dep` as `name` (its extern prelude, or what an `extern
    /// crate` may name), and `dep` is not finished: every crate is resolved after the crates it
    /// names (the graph's order, a per-crate call's records), so this is a caller's broken
    /// promise, refused rather than leaving `name` out of the root.
    DepNotFinished { krate: &'a str, name: &'a str, dep: &'a str },
    /// The cell core, the names records' writer, refused the crate.
    Cells { krate: &'a str, error: crate::cells::Unwritten },
    /// The module tree record was not written.
    TreeRecord { krate: &'a str, error: crate::tree_record::TreeRecordError },
    /// Two items' keys begin with the same 16 bytes (a checker key's, front2 keys.rs
    /// `checker_of`), so [`Names::key_beginning`] could not say which one a checker key names
    /// (rule 21: two values under one key is a refusal; the fix is the key). `first` and
    /// `second` in key order, and of all such pairs the least, so the error is the same in any
    /// order of the crates' maps.
    KeyBeginning { first: ItemKey, second: ItemKey },
}

impl fmt::Display for NamesError<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            NamesError::Graph(error) => write!(f, "the crate graph: {error}"),
            NamesError::NoSuchDep { krate, name, index } => {
                write!(f, "crate `{krate}`: dependency `{name}` is crate {index}, past the inputs")
            }
            NamesError::RootNotRead { krate, path } => write!(f, "crate `{krate}`: its root file {path} is not read"),
            NamesError::SourceUnread { krate, error } => write!(f, "crate `{krate}`: {error} (D193 point 5: an unread file is no absent one)"),
            NamesError::WalkUnbounded { krate } => {
                write!(f, "crate `{krate}`: the module walk found new files past {MOST_WALKS} passes")
            }
            NamesError::OutlineWorker { krate, path, why } => write!(f, "crate `{krate}`: outlining `{path}`: {why}"),
            NamesError::OutlineCount { krate, files, outlines } => write!(f, "crate `{krate}`: {files} module files yielded {outlines} outlines"),
            NamesError::RootRecord { krate, error } => write!(f, "crate `{krate}`: its CrateRoot record: {error:?}"),
            NamesError::NotResolved { krate } => write!(f, "crate `{krate}`: not reached by the graph's waves"),
            NamesError::DepNotFinished { krate, name, dep } => {
                write!(f, "crate `{krate}`: its dependency `{name}` (crate `{dep}`) is not finished before it")
            }
            NamesError::Cells { krate, error } => write!(f, "crate `{krate}`: {error}"),
            NamesError::TreeRecord { krate, error } => write!(f, "crate `{krate}`: its module tree record: {error:?}"),
            NamesError::KeyBeginning { first, second } => {
                write!(f, "items {first:?} and {second:?}: their keys begin with the same 16 bytes, so a checker key cannot name one (rule 21, keys-cover-every-input)")
            }
        }
    }
}

/// One crate's names, and everything made on the way.
#[derive(Debug)]
pub struct CrateNames<'a> {
    pub name: &'a str,
    pub key: CrateItemKey,
    pub edition: Edition,
    pub implicit: Implicit,
    /// For a proc-macro crate, the key its proc-macro runner knows it by
    /// ([`SourceTexts::proc_macro_key`]); `None` for any other crate, or one no runner has.
    pub proc_key: Option<CrateItemKey>,
    pub cfg: Cfg<'a, 'a>,
    /// Its root file's own `#![no_std]` and `#![no_core]`.
    pub root_attrs: RootAttrs,
    /// Every file read, by index (a [`Loc`]'s and an [`At::File`]'s).
    pub files: AVec<'a, &'a SourceFile<'a>>,
    pub tree: Tree<'a, 'a>,
    /// The module tree record ([`crate::tree_record`]): what a one-group solve of this crate
    /// reads instead of walking and lowering every file.
    pub tree_record: &'a [u8],
    /// The root module.
    pub root: ItemKey,
    /// Each walked module's place and declarations, read back from its record.
    pub decls: HashMap<ItemKey, (ModuleInfo<&'a str>, DeclsBuilder<'a, &'a str>), Alloc<'a>>,
    /// The `CrateRoot` record, read back.
    pub crate_root: CrateRootBuilder<'a, &'a str>,
    /// Each module's chain up to the root, itself first (walked and made).
    pub chains: HashMap<ItemKey, AVec<'a, ItemKey>, Alloc<'a>>,
    pub tables: Tables<'a>,
    pub made: HashMap<ItemKey, MadeModule<'a>, Alloc<'a>>,
    pub expansions: HashMap<ItemKey, Expansion<'a>, Alloc<'a>>,
    /// Each module's finished scope and the read list it is valid by.
    pub done: HashMap<ItemKey, Resolved<'a, &'a str>, Alloc<'a>>,
    /// The written modules by group (E4, cells/groups.rs): one record per strongly connected
    /// component of their reads, sorted by first member. Empty unless the caller asked
    /// ([`crate::task::Par::names_groups`]).
    pub groups: AVec<'a, crate::NameGroup<'a>>,
    /// The modules whose task failed, and why.
    pub failed: HashMap<ItemKey, TaskError, Alloc<'a>>,
    pub report: FillReport,
    pub notes: AVec<'a, NamesNote<'a>>,
    /// D189 step 4 (`BlockScopes::Bodies`): the bodies that may write an item in a block, held
    /// for their own solve ([`crate::body_scope`]), by key. Empty under `Modules`, where their
    /// blocks are modules of the crate.
    pub bodies: AVec<'a, HeldBody<'a>>,
    /// Each `link_name`/`export_name` value written as a macro call, expanded at its item's
    /// place: one row per item and attribute, sorted ([`crate::attr_values::row`] reads one).
    pub values: AVec<'a, crate::attr_values::ValueRow<'a>>,
}

/// A body whose blocks a crate solve left to the body's own solve (D189 step 4).
#[derive(Clone, Copy, Debug)]
pub struct HeldBody<'a> {
    pub src: crate::task::BodySrc<'a>,
    /// The expansion that wrote the body (its origins), `None` for a file's.
    pub writer: Option<&'a ExpansionText<'a>>,
    /// The length of the body's module's textual sequence at the body's place (cells/body.rs
    /// `textual_len_at`): what a `macro_rules!` walk out of its blocks reads of the module.
    /// `None`: the crate solve did not read that place (a walk out of the body is refused).
    pub cut: Option<u32>,
}

impl<'a> CrateNames<'a> {
    /// The file and 1-based line of `loc`.
    pub fn line_of(&self, loc: Loc) -> Option<(&'a SourceFile<'a>, usize)> {
        let file = *self.files.get(loc.file as usize)?;
        let end = (loc.offset as usize).min(file.text.len());
        let line = file.text.as_bytes()[..end].iter().filter(|it| **it == b'\n').count() + 1;
        Some((file, line))
    }

    /// The text `at` is, and its outline.
    pub fn outline_at(&self, at: At) -> Option<(&'a str, &'a Outline<'a>)> {
        match at {
            At::File(file) => self.files.get(file as usize).map(|f| (f.text, &f.outline)),
            At::Expansion(call) => self.expansions.get(&call).and_then(|e| e.made).map(|m| (m.text, &m.outline)),
        }
    }

    /// The module path of `module` (`krate::a::b`), walked or made.
    pub fn path_of(&self, module: ItemKey) -> Option<&str> {
        if let Some(&i) = self.tree.by_key.get(&module) {
            return Some(self.tree.modules[i].path.as_str());
        }
        self.made.get(&module).map(|m| m.path)
    }

    /// The path of the module whose own text declares enum `key`, among the modules this solve
    /// lowered (a refusal naming an enum prints where it is: rule 19).
    pub fn enum_path_of(&self, key: ItemKey) -> Option<&str> {
        let (&module, _) = self.decls.iter().find(|(_, (_, d))| d.items.iter().any(|it| it.def.key == key && it.def.kind == DefKind::Enum))?;
        self.path_of(module)
    }

    /// A module's place, walked or made.
    pub(crate) fn info(&self, module: ItemKey) -> Option<ModuleInfo<&'a str>> {
        self.decls.get(&module).map(|it| it.0).or_else(|| self.made.get(&module).map(|m| m.info))
    }

    /// Whether `module` is a block's module ([`crate::blocks::is_block_module`]).
    pub(crate) fn anonymous(&self, module: ItemKey) -> bool {
        self.info(module).is_some_and(|info| info.name.is_some_and(crate::blocks::is_block_module))
    }
}

/// A module of a crate, walked or written by a macro.
#[derive(Clone, Copy, Debug)]
pub struct ModuleAt<'r> {
    pub key: ItemKey,
    pub parent: Option<ItemKey>,
    pub path: &'r str,
    /// The text its items are in, and its own item there (`None`: the whole file).
    pub at: At,
    pub item: Option<ItemIndex>,
}

/// The names of every crate of a graph.
#[derive(Debug)]
pub struct Names<'a> {
    graph: CrateGraph<'a, &'a str>,
    /// By [`CrateId`].
    crates: AVec<'a, &'a CrateNames<'a>>,
    /// The same, as the resolution held them while it ran (every one `Some`): what a lookup
    /// reads.
    slots: AVec<'a, Option<Dep<'a>>>,
    by_prefix: HashMap<[u8; 8], usize, Alloc<'a>>,
    /// Each item with text, by its key's first 16 bytes (a checker key's: front2 keys.rs
    /// `checker_of`); two keys beginning alike refuse the names at construction
    /// ([`NamesError::KeyBeginning`]). What [`Names::key_beginning`] answers, so a reader
    /// holding a checker key finds the item's own facts (its declared lifetimes, D155) without
    /// a map filed in a fixed order.
    by_beginning: HashMap<[u8; 16], ItemKey, Alloc<'a>>,
    alloc: Alloc<'a>,
}

/// [`Names::by_beginning`] over every crate's items, or the least pair of items whose keys
/// begin alike (rule 21). Every clash is seen before one is reported, so which pair is named
/// does not depend on the maps' order (rule 25).
fn beginnings<'a>(all: &[&'a CrateNames<'a>], alloc: Alloc<'a>) -> Result<HashMap<[u8; 16], ItemKey, Alloc<'a>>, NamesError<'a>> {
    let mut by: HashMap<[u8; 16], ItemKey, Alloc<'a>> = HashMap::new_in(alloc);
    let mut clash: Option<(ItemKey, ItemKey)> = None;
    for c in all {
        for &key in c.tables.texts.keys() {
            let mut first = [0u8; 16];
            first.copy_from_slice(&key.0.0[..16]);
            match by.get(&first) {
                Some(&held) if held != key => {
                    let pair = if held < key { (held, key) } else { (key, held) };
                    if clash.is_none_or(|least| pair < least) {
                        clash = Some(pair);
                    }
                }
                Some(_) => {}
                None => {
                    by.insert(first, key);
                }
            }
        }
    }
    match clash {
        Some((first, second)) => Err(NamesError::KeyBeginning { first, second }),
        None => Ok(by),
    }
}

impl<'a> Names<'a> {
    /// The item whose key begins with `first` (a checker key's 16 bytes), or `None` when no
    /// item with text does. Never ambiguous: two keys beginning alike refused the names when
    /// they were made ([`NamesError::KeyBeginning`]).
    pub fn key_beginning(&self, first: [u8; 16]) -> Option<ItemKey> {
        self.by_beginning.get(&first).copied()
    }

    /// The crates of `crates` resolved one by one (`crate_scope::resolve_by_records`, `all` by
    /// crate index), read as one set: lookups go through each crate's own names.
    pub fn of_crates(crates: &[CrateInput<'a>], sysroot: Sysroot, all: &[&'a CrateNames<'a>], alloc: Alloc<'a>) -> Result<Names<'a>, NamesError<'a>> {
        let graph = graph_of(crates, sysroot, alloc)?;
        let mut by_prefix = HashMap::new_in(alloc);
        let mut slots = AVec::with_capacity_in(all.len(), alloc);
        for (at, c) in all.iter().enumerate() {
            by_prefix.insert(c.key.prefix(), at);
            slots.push(Some(Dep::Whole(*c)));
        }
        let by_beginning = beginnings(all, alloc)?;
        Ok(Names { graph, crates: AVec::from_slice_in(all, alloc), slots, by_prefix, by_beginning, alloc })
    }

    pub fn graph(&self) -> &CrateGraph<'a, &'a str> {
        &self.graph
    }

    /// The allocator these names were made in: a walk's held scratch is made in it, so it
    /// lives as long as the names it reads (blaze-ladder front2/source.rs).
    pub fn alloc(&self) -> Alloc<'a> {
        self.alloc
    }

    /// Every crate, by [`CrateId`] (its index in the inputs).
    pub fn crates(&self) -> &[&'a CrateNames<'a>] {
        &self.crates
    }

    pub fn krate(&self, krate: usize) -> &'a CrateNames<'a> {
        self.crates[krate]
    }

    /// The crate `key` (a module, item, macro or variant) is of.
    pub fn crate_of(&self, key: ItemKey) -> Option<usize> {
        self.by_prefix.get(&prefix(key)).copied()
    }

    pub fn edition(&self, krate: usize) -> Edition {
        self.crates[krate].edition
    }

    /// The crate's root module.
    pub fn root(&self, krate: usize) -> ItemKey {
        self.crates[krate].root
    }

    /// The module an item is declared in (items with text: of files and of expansions).
    pub fn module_of(&self, item: ItemKey) -> Option<ItemKey> {
        self.item_text(item).map(|t| t.module)
    }

    pub fn item_text(&self, item: ItemKey) -> Option<&ItemText<'a>> {
        self.crates[self.crate_of(item)?].tables.texts.get(&item).map(|it| &it.0)
    }

    /// The item at entry `item` of the text `at` of `krate` (a file, or a call's expansion):
    /// [`Names::place`] the other way.
    pub fn key_at(&self, krate: usize, at: At, item: ItemIndex) -> Option<ItemKey> {
        self.crates.get(krate)?.tables.by_place.get(&(at, item)).copied()
    }

    pub fn place(&self, item: ItemKey) -> Option<Place> {
        let krate = self.crate_of(item)?;
        let (text, at) = self.crates[krate].tables.texts.get(&item)?;
        Some(Place { krate, at: *at, item: text.item })
    }

    /// Every item with text of `krate`: its key, its module, its place.
    pub fn items(&self, krate: usize) -> impl Iterator<Item = (ItemKey, ItemKey, Place)> + '_ {
        self.crates[krate].tables.texts.iter().map(move |(&key, (text, at))| (key, text.module, Place { krate, at: *at, item: text.item }))
    }

    /// Every module of `krate`: the walked ones in walk order, then those macros wrote.
    pub fn modules(&self, krate: usize) -> impl Iterator<Item = ModuleAt<'_>> + '_ {
        let c = self.crates[krate];
        let walked = c.tree.modules.iter().map(move |m| ModuleAt {
            key: m.key,
            parent: m.parent.map(|p| c.tree.modules[p].key),
            path: m.path.as_str(),
            at: At::File(m.file),
            item: m.item,
        });
        let made = c.made.iter().map(|(&key, m)| ModuleAt { key, parent: m.info.parent, path: m.path, at: m.at, item: m.item });
        walked.chain(made)
    }

    pub fn macro_src(&self, def: ItemKey) -> Option<MacroSrc<'a>> {
        self.crates[self.crate_of(def)?].tables.macros.get(&def).copied()
    }

    /// A file's outline, made once by the resolution.
    pub fn outline(&self, krate: usize, file: u32) -> Option<&'a Outline<'a>> {
        self.crates.get(krate)?.files.get(file as usize).map(|f| &f.outline)
    }

    /// The text `at` is in `krate`, and its outline.
    pub fn outline_at(&self, krate: usize, at: At) -> Option<(&'a str, &'a Outline<'a>)> {
        self.crates.get(krate)?.outline_at(at)
    }

    pub fn notes(&self, krate: usize) -> &[NamesNote<'a>] {
        &self.crates[krate].notes
    }

    /// `name` in `ns` of `module` (a module or an enum of any crate) as a reader of the
    /// finished scopes finds it: its own binding, else through its glob edges.
    pub fn lookup(&self, module: ItemKey, ns: Namespace, name: &'a str) -> Option<Binding> {
        Built { crates: &self.slots, by_prefix: &self.by_prefix, asked: None }.lookup(module, ns, name, self.alloc)
    }

    /// [`crate::names::path`] with segments of any lifetime (a lowered body's view, an
    /// expansion's text): the records are read as of the segments' shorter borrow.
    pub fn path(&self, module: ItemKey, ns: Namespace, written: Written<'_, &str>, alloc: Alloc<'_>) -> Result<crate::names::Resolved, PathError> {
        crate::names::path(self.shorter(), module, ns, written, alloc)
    }

    /// [`crate::names::traits_in_scope`] over these names.
    pub fn traits_in_scope<'b>(&self, module: ItemKey, alloc: Alloc<'b>) -> Result<AVec<'b, ItemKey>, PathError> {
        crate::names::traits_in_scope::<&'a str, _>(self, module, alloc)
    }

    /// A trait's or an impl's members on under its crate's cfg, in declaration order; `None`
    /// when `owner` is no trait or impl held.
    pub fn members(&self, owner: ItemKey) -> Option<&[Member<'a>]> {
        self.crates[self.crate_of(owner)?].tables.members.get(&owner).map(|m| m.as_slice())
    }

    /// The constructor of a tuple or unit struct: the struct's own key, which its value-namespace
    /// binding names; `None` for a struct with named fields or anything else.
    pub fn ctor(&self, strukt: ItemKey) -> Option<ItemKey> {
        self.crates[self.crate_of(strukt)?].tables.ctors.contains_key(&strukt).then_some(strukt)
    }

    /// An enum's variants on under its crate's cfg, in declaration order.
    pub fn variants(&self, owner: ItemKey) -> Option<&[Variant<&'a str>]> {
        self.crates[self.crate_of(owner)?].tables.enums.get(&owner).map(|v| v.as_slice())
    }

    /// These names, read under a shorter lifetime: every name a record holds outlives it.
    fn shorter<'s>(&'s self) -> &'s Names<'s> {
        self
    }

    /// The item a whole path written in `module` names in `ns`, in whichever crate defines it:
    /// a body's macro call or a derive's name (`Namespace::Macros`), a type, a fn. `module` is
    /// the module the path is written in (for a path in a body, the body's enclosing module,
    /// [`Names::module_of`]); a `macro_rules!` written earlier in the same body is the caller's
    /// to try first. A path whose prefix names something with no names under it (`Type::new`)
    /// is [`Why::NotAModule`] at the first segment not consumed: the type checker's, not this.
    /// `alloc` is scratch for the walk; nothing is kept in it.
    pub fn item_at(&self, module: ItemKey, ns: Namespace, written: Written<'_, &str>, alloc: Alloc<'_>) -> Result<ItemAt<'a>, PathError> {
        let found = self.path(module, ns, written, alloc)?;
        let len = written.segments.len();
        let def = found.whole(len).ok_or(PathError { module, at: found.consumed, why: Why::NotAModule })?;
        let krate = self.crate_of(def.key).ok_or(PathError { module, at: len - 1, why: Why::NotRead { module: def.key, layer: Unheld::Step } })?;
        let macro_src = match ns {
            Namespace::Macros => self.macro_src(def.key),
            _ => None,
        };
        Ok(ItemAt { krate, def, macro_src })
    }
}

impl<'a> Names<'a> {
    /// The definition the macro path `written` in `module` names, called as `shape`: for a
    /// body's call of a dependency's macro, built where the fill builds its own
    /// ([`definition_of`]), in the defining crate's `$crate` and edition.
    pub fn macro_definition(&self, module: ItemKey, shape: CallShape, written: Written<'_, &str>, alloc: Alloc<'a>) -> Result<MacroDefinition<'a>, MacroDefError<'a>> {
        let found = self.item_at(module, Namespace::Macros, written, alloc).map_err(MacroDefError::Path)?;
        let Some(src) = found.macro_src else { return Err(MacroDefError::NoSource { def: found.def, krate: found.krate }) };
        let owner = self.crates[found.krate];
        definition_of(src, owner.key, owner.proc_key, owner.edition, shape, alloc).map_err(MacroDefError::Refused)
    }
}

/// What [`Names::item_at`] found: the defining crate (an index of [`Names::crates`]), the
/// definition, and for a macro its source as lowered (`None` for a builtin or procedural one,
/// whose definition is not a `macro_rules!`/`macro` body).
#[derive(Clone, Copy, Debug)]
pub struct ItemAt<'a> {
    pub krate: usize,
    pub def: Def,
    pub macro_src: Option<MacroSrc<'a>>,
}

impl<'a> World<&'a str> for Names<'a> {
    type Scope<'w>
        = &'w ScopeBuilder<'w, &'a str>
    where
        Self: 'w;
    type Root<'w>
        = &'w CrateRootBuilder<'w, &'a str>
    where
        Self: 'w;

    fn scope(&self, module: ItemKey) -> Option<&ScopeBuilder<'_, &'a str>> {
        self.crates[self.crate_of(module)?].done.get(&module).map(|r| &r.scope)
    }

    fn chain(&self, module: ItemKey) -> Option<&[ItemKey]> {
        self.crates[self.crate_of(module)?].chains.get(&module).map(|c| c.as_slice())
    }

    fn root_of(&self, key: ItemKey) -> Option<&CrateRootBuilder<'_, &'a str>> {
        Some(&self.crates[self.crate_of(key)?].crate_root)
    }

    fn root(&self, krate: CrateItemKey) -> Option<&CrateRootBuilder<'_, &'a str>> {
        self.by_prefix.get(&krate.prefix()).map(|&at| &self.crates[at].crate_root)
    }

    fn variant(&self, owner: ItemKey, name: &'a str) -> Option<Option<VariantHit>> {
        Some(variant_in(Names::variants(self, owner)?, name))
    }

    fn no_implicit_prelude(&self, module: ItemKey) -> bool {
        let Some(at) = self.crate_of(module) else { return false };
        let c = self.crates[at];
        let Some(chain) = c.chains.get(&module) else { return false };
        chain.iter().any(|m| c.info(*m).is_some_and(|info| info.no_implicit_prelude))
    }

    fn anonymous(&self, module: ItemKey) -> bool {
        self.crate_of(module).is_some_and(|at| self.crates[at].anonymous(module))
    }
}

// ---------------------------------------------------------------------------------------------
// The call.

/// The most passes of the module walk: each pass reads the files the last one found named.
pub const MOST_WALKS: usize = 1 << 12;

/// The most modules macros may write in one crate: past it, a module is refused by name.
pub const MAX_MADE_MODULES: usize = 1 << 16;

/// The names of every crate of `crates`, each after its dependencies; `sysroot` says which
/// of them are core, alloc, std and bstd. The first tasks come from the syntactic prefetch.
pub fn resolve_crates<'a, S: SourceTexts<'a>, P: Par>(
    crates: &[CrateInput<'a>],
    sysroot: Sysroot,
    sources: &S,
    par: &P,
    alloc: Bump<'a>,
) -> Result<Names<'a>, NamesError<'a>> {
    resolve_crates_with::<S, P>(crates, sysroot, sources, par, First::Prefetch, alloc)
}

/// [`resolve_crates`], the first tasks made as `first` says.
pub fn resolve_crates_with<'a, S: SourceTexts<'a>, P: Par>(
    crates: &[CrateInput<'a>],
    sysroot: Sysroot,
    sources: &S,
    par: &P,
    first: First,
    bump: Bump<'a>,
) -> Result<Names<'a>, NamesError<'a>> {
    let alloc = bump.alloc();
    let graph = graph_of(crates, sysroot, alloc)?;
    let mut finished: AVec<'a, Option<Dep<'a>>> = AVec::with_capacity_in(crates.len(), alloc);
    finished.extend(crates.iter().map(|_| None));
    let mut whole: AVec<'a, Option<&'a CrateNames<'a>>> = AVec::with_capacity_in(crates.len(), alloc);
    whole.extend(crates.iter().map(|_| None));
    let mut by_prefix = HashMap::new_in(alloc);
    let prepares = Prepares::new_in(bump);
    for wave in graph.waves() {
        for &id in wave {
            let Some(input) = crates.get(id.index()) else {
                continue;
            };
            let done = {
                let built = Built { crates: &finished, by_prefix: &by_prefix, asked: None };
                resolve_one::<S, P>(&graph, input, id, id.index(), &built, sources, par, first, prepares, &crate::expansions::KeptExpansions::none(alloc), bump)?
            };
            let done: &'a CrateNames<'a> = bump.alloc_val(done);
            by_prefix.insert(done.key.prefix(), id.index());
            finished[id.index()] = Some(Dep::Whole(done));
            whole[id.index()] = Some(done);
        }
    }
    let mut all = AVec::with_capacity_in(crates.len(), alloc);
    for (at, c) in whole.iter().enumerate() {
        match c {
            Some(c) => all.push(*c),
            None => return Err(NamesError::NotResolved { krate: crates[at].name }),
        }
    }
    let by_beginning = beginnings(&all, alloc)?;
    Ok(Names { graph, crates: all, slots: finished, by_prefix, by_beginning, alloc })
}

/// The graph of `crates` (each one's `deps` index `crates`) over `sysroot`.
pub(crate) fn graph_of<'a>(crates: &[CrateInput<'a>], sysroot: Sysroot, alloc: Alloc<'a>) -> Result<CrateGraph<'a, &'a str>, NamesError<'a>> {
    let mut graph_crates = AVec::with_capacity_in(crates.len(), alloc);
    for c in crates {
        let mut deps = AVec::with_capacity_in(c.deps.len(), alloc);
        for &(name, index) in c.deps {
            let krate = u32::try_from(index)
                .ok()
                .filter(|_| index < crates.len())
                .ok_or(NamesError::NoSuchDep { krate: c.name, name, index })?;
            deps.push(GraphDep { name, krate: CrateId(krate) });
        }
        graph_crates.push(GraphCrate {
            name: c.name,
            edition: c.edition,
            implicit: c.implicit,
            features: AVec::from_slice_in(c.features, alloc),
            cfg: AVec::from_slice_in(c.cfg, alloc),
            deps,
        });
    }
    CrateGraph::new_in(graph_crates, sysroot, alloc).map_err(NamesError::Graph)
}

// ---------------------------------------------------------------------------------------------
// The crates finished so far.

/// The crates finished before the one resolving, found by the prefix every key of a crate
/// begins with.
pub(crate) struct Built<'r, 'a> {
    pub(crate) crates: &'r [Option<Dep<'a>>],
    pub(crate) by_prefix: &'r HashMap<[u8; 8], usize, Alloc<'a>>,
    /// Where each question asked of these crates is noted ([`crate::asked`]): a per-crate
    /// solve's read list; `None` where nothing is kept.
    pub(crate) asked: Option<&'r crate::asked::Asked<'a>>,
}

fn prefix(key: ItemKey) -> [u8; 8] {
    let mut out = [0u8; 8];
    out.copy_from_slice(&key.0.0[..8]);
    out
}

impl<'r, 'a> Built<'r, 'a> {
    fn at(&self, index: usize) -> Option<Dep<'a>> {
        self.crates.get(index).copied().flatten()
    }

    pub(crate) fn of(&self, key: ItemKey) -> Option<Dep<'a>> {
        self.at(*self.by_prefix.get(&prefix(key))?)
    }

    /// Notes a question asked of these crates (no-op where nothing is kept).
    pub(crate) fn ask(&self, question: impl FnOnce() -> crate::asked::Question<'a>) {
        if let Some(asked) = self.asked {
            asked.push(question());
        }
    }

    /// [`Built::ask`] for a read that may be no question ([`crate::asked::Question::of_read`]).
    pub(crate) fn ask_maybe(&self, question: impl FnOnce() -> Option<crate::asked::Question<'a>>) {
        if let Some(asked) = self.asked
            && let Some(q) = question()
        {
            asked.push(q);
        }
    }

    /// See [`Names::lookup`]: [`Built::lookup_in`] with a glob walk's scratch made for this one
    /// call.
    pub(crate) fn lookup(&self, module: ItemKey, ns: Namespace, name: &'a str, alloc: Alloc<'_>) -> Option<Binding> {
        self.lookup_in(module, ns, name, &mut blaze_surface::ThroughScratch::new_in(alloc))
    }

    /// [`Built::lookup`] with the glob walk in `scratch`, which a caller asking many names keeps
    /// (the cell core's dependency reads, cells/deps.rs): the same answer from the same reads.
    pub(crate) fn lookup_in(&self, module: ItemKey, ns: Namespace, name: &'a str, scratch: &mut blaze_surface::ThroughScratch<'_>) -> Option<Binding> {
        struct Records<'q, 'r, 'a> {
            built: &'q Built<'r, 'a>,
            ns: Namespace,
            name: &'a str,
        }
        impl<'a> crate::Sources<'a> for Records<'_, '_, 'a> {
            type Stop = ();
            fn node(&mut self, source: Def) -> Result<Node<'a>, ()> {
                let Some(other) = self.built.of(source.key) else { return Ok(Node::Value(None)) };
                let Some(scope) = other.scope(source.key) else {
                    let found = other.enum_of(source.key).and_then(|v| match variant_answer(v.iter(), self.ns, self.name) {
                        NameAnswer::Found { def, vis, state, .. } => Some(Passed { def, vis, state }),
                        NameAnswer::Absent => None,
                    });
                    return Ok(Node::Value(found));
                };
                if let Some(b) = scope.get(self.ns, self.name) {
                    return Ok(Node::Value(Some(Passed { def: b.def, vis: b.vis, state: b.state })));
                }
                let Some(chain) = other.chain(source.key) else { return Err(()) };
                Ok(Node::Edges { edges: scope.globs(), krate: other.key(), chain })
            }
        }
        let other = self.of(module)?;
        if let Some(b) = other.scope(module).and_then(|s| s.get(ns, name)) {
            return Some(b);
        }
        let mut sources = Records { built: self, ns, name };
        let p = blaze_surface::through_in(&mut sources, Def::module(module), scratch).ok()??;
        Some(Binding { def: p.def, vis: p.vis, origin: Origin::Glob, expansion: 0, state: p.state })
    }
}

/// A `Name` read of an enum: its variant of the name (a record variant has no value).
fn variant_answer<'v>(variants: impl IntoIterator<Item = Variant<&'v str>>, ns: Namespace, name: &str) -> NameAnswer {
    let hit = variants.into_iter().find(|v| v.name == name && (ns == Namespace::Types || (ns == Namespace::Values && !v.record)));
    match hit {
        Some(v) => NameAnswer::Found {
            def: Def { key: v.key, kind: DefKind::Other },
            vis: Visibility::Public,
            state: BindingState::Determined,
            origin: Origin::Declared,
        },
        None => NameAnswer::Absent,
    }
}

// ---------------------------------------------------------------------------------------------
// Files.

/// A crate's files by path, each with its index in `CrateNames::files`: made once per crate
/// ([`ReadTree::of`]) for every held body's solve to find the crate's files in place.
type FileIndex<'a> = HashMap<&'a str, u32, Alloc<'a>>;

/// A crate's files: a list by index and an index by path. A body solve's set starts from its
/// finished crate's files read in place (`base`, indices `0..base.len()`, found through
/// `base_index`, [`FileSet::over`]): a body copies none of them, and adds only a file one of
/// its own expansions reads first. A crate's own solve has no base ([`FileSet::new_in`]).
struct FileSet<'a> {
    base: &'a [&'a SourceFile<'a>],
    base_index: Option<&'a FileIndex<'a>>,
    /// The files this solve read itself, numbered after `base`.
    files: AVec<'a, &'a SourceFile<'a>>,
    index: FileIndex<'a>,
    alloc: Alloc<'a>,
}

impl<'a> FileSet<'a> {
    fn new_in(alloc: Alloc<'a>) -> FileSet<'a> {
        FileSet { base: &[], base_index: None, files: AVec::new_in(alloc), index: HashMap::new_in(alloc), alloc }
    }

    /// A body solve's set: its crate's finished files `base`, found by `base_index`.
    fn over(base: &'a [&'a SourceFile<'a>], base_index: &'a FileIndex<'a>, alloc: Alloc<'a>) -> FileSet<'a> {
        FileSet { base, base_index: Some(base_index), files: AVec::new_in(alloc), index: HashMap::new_in(alloc), alloc }
    }

    fn add(&mut self, file: &'a SourceFile<'a>) -> u32 {
        let at = u32::try_from(self.base.len() + self.files.len()).expect("a crate's files are counted in u32");
        self.files.push(file);
        self.index.insert(file.path, at);
        at
    }

    /// File `i`: the base's below its length, then this solve's own.
    fn at(&self, i: usize) -> &'a SourceFile<'a> {
        match i.checked_sub(self.base.len()) {
            None => self.base[i],
            Some(own) => self.files[own],
        }
    }

    /// Every file by index, the base's first.
    fn into_files(self) -> AVec<'a, &'a SourceFile<'a>> {
        if self.base.is_empty() {
            return self.files;
        }
        let mut all = AVec::with_capacity_in(self.base.len() + self.files.len(), self.alloc);
        all.extend_from_slice(self.base);
        all.extend_from_slice(&self.files);
        all
    }
}

impl<'a> Files<'a> for FileSet<'a> {
    fn find(&self, path: &str) -> Option<u32> {
        self.index.get(path).or_else(|| self.base_index.and_then(|b| b.get(path))).copied()
    }

    fn text(&self, file: u32) -> &'a str {
        self.at(file as usize).text
    }

    fn outline(&self, file: u32) -> &Outline<'_> {
        &self.at(file as usize).outline
    }
}

/// The content id of a source file's text: what a `SourceFile::id` is, and what an items
/// record's file row holds ([`crate::items_record`] checks a text against it on read).
pub(crate) fn file_id(text: &str) -> ContentId {
    ContentId::of("blaze-scope source file 1", text.as_bytes())
}

/// `path` past the directory `from` (`path` itself when it is not under it).
fn shown<'a>(path: &'a str, from: &str) -> &'a str {
    let from = from.trim_end_matches('/');
    match path.strip_prefix(from) {
        Some(rest) if !from.is_empty() && rest.starts_with('/') => &rest[1..],
        _ => path,
    }
}

/// `text` kept as long as `alloc`.
fn keep<'a>(text: &str, alloc: Bump<'a>) -> &'a str {
    alloc.alloc_str(text)
}

/// A file read and outlined, kept as long as `alloc`.
fn file_of<'a>(path: &'a str, text: &'a str, outlined: Outline<'a>, from: &str, alloc: Bump<'a>) -> &'a SourceFile<'a> {
    let package = crate::package_path::PackagePath::of(path, from, alloc);
    let contents = DigestedText { text, id: file_id(text) };
    alloc.alloc_val(SourceFile { path, package, shown: shown(path, from), contents, outline: outlined })
}

/// A record's bytes copied to blocks aligned as blaze-format reads them in place.
#[repr(C, align(16))]
#[derive(Clone, Copy)]
struct Block([u8; 16]);

const _: () = assert!(core::mem::align_of::<Block>() == blaze_format::ALIGN);

/// Owned 16-aligned copy of a record's bytes: drops its buffer instead of freezing in a bump arena.
pub(crate) struct AlignedBuf<'a> {
    bytes: AVec<'a, u8>,
    pad: usize,
}

impl<'a> AlignedBuf<'a> {
    pub(crate) fn copy_from(bytes: &[u8], alloc: Alloc<'a>) -> AlignedBuf<'a> {
        const ALIGN: usize = blaze_format::ALIGN;
        let mut buf: AVec<'a, u8> = AVec::with_capacity_in(bytes.len() + ALIGN, alloc);
        let pad = buf.as_ptr().align_offset(ALIGN).min(ALIGN);
        buf.resize(pad, 0);
        buf.extend_from_slice(bytes);
        AlignedBuf { bytes: buf, pad }
    }

    pub(crate) fn as_slice(&self) -> &[u8] {
        self.bytes.get(self.pad..).unwrap_or(&[])
    }
}

pub fn aligned<'a>(bytes: &[u8], alloc: Bump<'a>) -> &'a [u8] {
    let mut blocks = BVec::with_capacity_in(bytes.len().div_ceil(16), alloc);
    for chunk in bytes.chunks(16) {
        let mut block = [0u8; 16];
        block[..chunk.len()].copy_from_slice(chunk);
        blocks.push(Block(block));
    }
    let blocks: &'a [Block] = blocks.into_bump_slice();
    // SAFETY: `Block` is `repr(C)` around `[u8; 16]`, so `blocks` is `16 * len` initialized
    // bytes with no padding, borrowed for `'a` and never written again.
    let all: &'a [u8] = unsafe { core::slice::from_raw_parts(blocks.as_ptr().cast::<u8>(), blocks.len() * 16) };
    &all[..bytes.len()]
}

// ---------------------------------------------------------------------------------------------
// One crate.

/// What the lowering of the walked modules gives.
struct Lowering<'a> {
    /// What each lowered module's record is written from, by its place in `at`.
    decls: AVec<'a, DeclsBuilder<'a, &'a str>>,
    /// The index in the tree of each module lowered, ascending (the root's first when it is).
    at: AVec<'a, usize>,
    tables: Tables<'a>,
    prelude: Option<PreludeImport<'a, 'a>>,
    macro_use: AVec<'a, &'a str>,
    macro_exports: AVec<'a, (&'a str, ItemKey, MacroShapes)>,
    /// The files' bodies that may write an impl in a block, in module then text order.
    bodies: AVec<'a, crate::task::BodySrc<'a>>,
}

#[expect(clippy::too_many_arguments, reason = "one crate's every input, named")]
#[allow(clippy::too_many_arguments, reason = "the graph's id and the sources' index differ in a per-crate call")]
pub(crate) fn resolve_one<'a, S: SourceTexts<'a>, P: Par>(
    graph: &CrateGraph<'a, &'a str>,
    input: &CrateInput<'a>,
    id: CrateId,
    source: usize,
    built: &Built<'_, 'a>,
    sources: &S,
    par: &P,
    first: First,
    prepares: &'a Prepares<'a>,
    kept: &crate::expansions::KeptExpansions<'_, 'a>,
    bump: Bump<'a>,
) -> Result<CrateNames<'a>, NamesError<'a>> {
    let alloc = bump.alloc();
    // The crate's index for `sources`: its graph id in a whole-set call, the caller's index
    // in a per-crate one ([`crate::crate_scope::resolve_crate`]).
    let index = source;
    let edition = input.edition;
    // `input.cfg` already holds the build script's `cargo:rustc-cfg`s: the caller joins them
    // (blaze-world's `ScriptOutputs`), as the crate's key does.
    let cfg = Cfg::from_atoms(input.cfg, alloc);
    let mut notes: AVec<'a, NamesNote<'a>> = AVec::new_in(alloc);

    // A one-group solve handed the module tree record reads, outlines and lowers its members'
    // files alone (tree_record.rs); every other solve walks the files from the root.
    let held_tree = par.names_members().and_then(|m| m.tree.map(|b| (m, aligned(b, bump))));
    // A member that moved the tree (its walk rows, `mod` lines or exports are not the record's,
    // or it is in no row) makes the tree again from the files, as the record's inputs moved:
    // the solve's `tree_record` is then the new one.
    let from_record = match held_tree {
        Some((members, bytes)) => from_tree_record(graph, input, id, index, edition, &cfg, built, sources, par, members, bytes, &mut notes, bump)?,
        None => None,
    };
    if held_tree.is_some() && from_record.is_none() {
        par.names_part(crate::task::NamesPart::TreeAgain);
    }
    let prepared = match from_record {
        Some(prepared) => prepared,
        None => walked_files(graph, input, id, index, edition, &cfg, built, sources, par, &mut notes, bump)?,
    };
    let Prepared { set, tree, low, listed, root_attrs, root_bytes, record } = prepared;

    // The records, written and read back: what the fill and later crates read.
    par.stage(crate::task::Stage::Decls);
    let mut decls = HashMap::new_in(alloc);
    for (&i, d) in low.at.iter().zip(low.decls.iter()) {
        let m = &tree.modules[i];
        let info = module_info(input.key, &tree, i);
        let bytes = match d.write(m.key, &info, alloc) {
            Ok(bytes) => aligned(&bytes, bump),
            Err(error) => {
                notes.push(NamesNote::DeclsRecord { module: m.key, error: RecordError::Refused(error) });
                continue;
            }
        };
        match ModuleDeclsView::read(bytes) {
            Ok(view) => {
                decls.insert(m.key, (view.info(), DeclsBuilder::from(&view, alloc)));
            }
            Err(error) => notes.push(NamesNote::DeclsRecord { module: m.key, error: RecordError::NotRead(error) }),
        }
    }
    let root_record = |error| NamesError::RootRecord { krate: input.name, error };
    let view = CrateRootView::read(root_bytes).map_err(|e| root_record(RecordError::NotRead(e)))?;
    let mut crate_root_record = CrateRootBuilder::from(&view, alloc);
    // The module tree record: held in (its members matched their rows), or written from this
    // walk.
    let tree_record: &'a [u8] = match &record {
        Some((bytes, _)) => bytes,
        None => {
            let written = tree_record_of(&set, &tree, &decls, &low.tables.macros, &low.tables.enums, &cfg, edition, &crate_root_record.tools, root_attrs, root_bytes, bump);
            aligned(&written.map_err(|error| NamesError::TreeRecord { krate: input.name, error })?, bump)
        }
    };
    let mut chains: HashMap<ItemKey, AVec<'a, ItemKey>, Alloc<'a>> = HashMap::new_in(alloc);
    let mut by_index: AVec<'a, AVec<'a, ItemKey>> = AVec::with_capacity_in(tree.modules.len(), alloc);
    for m in tree.modules.iter() {
        let mut chain = AVec::new_in(alloc);
        chain.push(m.key);
        if let Some(p) = m.parent {
            chain.extend_from_slice(&by_index[p]);
        }
        by_index.push(chain);
    }
    for (m, chain) in tree.modules.iter().zip(by_index) {
        chains.insert(m.key, chain);
    }

    // The names.
    let run = Run::new_in(set, prepares, alloc);
    let mut fill: Fill<'a, &'a str> = Fill::new_in(alloc);
    fill.first = first;
    let added_externs: AVec<'a, (&'a str, ItemKey)>;
    let shapes: AVec<'a, crate::GroupShape<'a>>;
    let cuts: AVec<'a, ((ItemKey, u32), u32)>;
    {
        let base = Base {
            index,
            key: input.key,
            edition,
            shown_from: input.shown_from,
            script: input.script,
            cfg: &cfg,
            tree: &tree,
            tables: &low.tables,
            decls: &decls,
            root: &crate_root_record,
            chains: &chains,
            record: record.as_ref().map(|r| &r.1),
            built,
            sources,
            listed: listed.is_some(),
            kept,
            // D189's seam: under `Bodies` the crate solve holds every body's blocks for the
            // body's own solve (`crate::body_scope`); no block is a module of the crate.
            blocks: match par.block_scopes() {
                crate::surface::BlockScopes::Modules => BlocksAt::Crate,
                crate::surface::BlockScopes::Bodies => BlocksAt::Held,
            },
            made_before: None,
            alloc,
            bump,
        };
        let mark = |stage: crate::task::Stage| par.stage(stage);
        let part = |part: ExpandPart, macro_name: &str, size: usize| par.expand_part(part, macro_name, size);
        let seen = |def: ItemKey, input: &str| par.expand_call(def, input);
        // Where each macro call's working memory is held (lane N5): the Par's scratch, reset
        // after each call, or (`Scratch::Kept`) the crate's allocator.
        let scratch = |work: &mut dyn FnMut(Alloc<'_>)| par.call_scratch(work);
        let cx = FillCx { base: &base, run: &run, facts: Facts { base: &base, run: &run }, mark: &mark, part: &part, seen: &seen, scratch: &scratch };
        // The cell core solves and writes the scopes. The files' blocks that hold impls are
        // modules before it starts: it is told of them as of any made module.
        par.stage(crate::task::Stage::Solve);
        par.names_part(crate::task::NamesPart::Blocks);
        blocks_of(&base, &run, &low.bodies, None);
        let places = |m: ItemKey| held(run.block_places.lock(), "block places").get(&m).copied();
        let expand = |call: ItemKey, def: Def| {
            let _ = crate::task::Expand::expand(&cx, ExpandRequest { call, def });
        };
        let made = |out: &mut AVec<'a, ItemKey>| out.extend(held(run.made.read(), "made modules").keys().copied());
        let names_part = |p: crate::task::NamesPart| par.names_part(p);
        // The places counted: each held body's, and each attribute value's written as a call
        // (crate::attr_values: its macros are looked up at its item's place).
        let held_places = |out: &mut AVec<'a, (ItemKey, u32)>| {
            out.extend(held(run.held.lock(), "held bodies").iter().map(|b| b.0.place));
            out.extend(low.tables.values.iter().map(|v| v.place));
            out.extend(held(run.tables.read(), "tables").values.iter().map(|v| v.place));
        };
        let solved = crate::cells::solve_and_write(&cx.facts, built, &cx.facts, &places, tree.modules[0].key, &expand, &made, &held_places, par.decide_check(), par.decide_plant(), par.names_groups(), par.names_members(), &names_part, bump)
            .map_err(|error| NamesError::Cells { krate: input.name, error })?;
        if let Some(check) = &solved.decide {
            par.decided(input.name, check);
        }
        fill.done = solved.done;
        added_externs = solved.added_externs;
        shapes = solved.groups;
        cuts = solved.cuts;
    }
    // The extern prelude of the record is the whole one: the root's macro-written `extern crate`
    // items' names too (libc's `cfg_if! { extern crate rustc_std_workspace_core as core; }`
    // under `no_core`), which a walk over the record (names.rs `walk`, `lexical`) reads; none of
    // them was in it before (cells R8), so the merge only adds.
    if !added_externs.is_empty() {
        crate_root_record.extern_prelude.extend_from_slice(&added_externs);
        crate_root_record.extern_prelude.sort_by(|a, b| a.0.cmp(b.0));
        crate_root_record.extern_prelude.dedup_by_key(|it| it.0);
    }
    par.stage(crate::task::Stage::Publish);
    let mut done = core::mem::replace(&mut fill.done, HashMap::new_in(alloc));
    let mut in_made: AVec<'a, (&'a str, ItemKey, MacroShapes)> = AVec::new_in(alloc);
    for module in held(run.made.read(), "made modules").values() {
        in_made.extend(module.decls.macro_rules.iter().filter(|r| r.export).map(|r| (r.name, r.def, r.shapes)));
    }
    for name in crate::task::publish_expanded_exports(&mut done, &in_made, tree.modules[0].key, &mut crate_root_record, alloc) {
        notes.push(NamesNote::Root(RootNote::MacroExportedTwice { name }));
    }
    // Each refused call into its module's scope: what a dependent's lookup behind it reads.
    let expansions_now = held(run.expansions.lock(), "expansions");
    crate::refused::write_refused(&mut done, &expansions_now, alloc);
    // E4: each group's record, over the final scopes and its members' expansions.
    let made_text = |call: ItemKey| expansions_now.get(&call).and_then(|x| x.made).map(|m| m.text);
    par.names_part(crate::task::NamesPart::Records);
    let groups = crate::write_groups(&shapes, &done, &made_text, alloc).map_err(|error| NamesError::Cells { krate: input.name, error: crate::cells::Unwritten::Groups(error) })?;
    drop(expansions_now);
    let failed = core::mem::replace(&mut fill.failed, HashMap::new_in(alloc));
    let report = fill.report;

    let Run { files, tables, expansions, made, notes: run_notes, new_modules: _, block_places: _, prepares: _, held: held_bodies } = run;
    // The held bodies (D189 step 4), by key (the fill's threads held them in no fixed order),
    // each with its cut of its module's textual sequence.
    let mut bodies: AVec<'a, HeldBody<'a>> = AVec::new_in(alloc);
    bodies.extend(held(held_bodies.into_inner(), "held bodies").iter().map(|&(src, writer)| HeldBody {
        src,
        writer,
        cut: cuts.binary_search_by_key(&src.place, |c| c.0).ok().map(|i| cuts[i].1),
    }));
    bodies.sort_unstable_by_key(|b| b.src.key);
    let files = held(files.into_inner(), "files").into_files();
    let mut tables_all = low.tables;
    tables_all.merge(held(tables.into_inner(), "tables"));
    let made = held(made.into_inner(), "made modules");
    for (&m, module) in made.iter() {
        chains.insert(m, module.chain.clone_in(alloc));
    }
    notes.extend(held(run_notes.into_inner(), "notes"));
    let mut names = CrateNames {
        name: input.name,
        key: input.key,
        edition,
        implicit: input.implicit,
        proc_key: if input.implicit.proc_macro { sources.proc_macro_key(index) } else { None },
        cfg,
        root_attrs,
        files,
        root: tree.modules[0].key,
        tree,
        tree_record,
        decls,
        crate_root: crate_root_record,
        chains,
        tables: tables_all,
        made,
        expansions: held(expansions.into_inner(), "expansions"),
        done,
        groups,
        failed,
        report,
        notes,
        bodies,
        values: AVec::new_in(alloc),
    };
    // The `link_name`/`export_name` values written as macro calls, each expanded at its item's
    // place over the finished names (crate::attr_values), by the host a call there has: its
    // file, its module's path, the crate's environment (and its build script's outputs).
    let host_of = |site: &crate::attr_values::ValueSite<'a>| {
        let file: &'a SourceFile<'a> = names.files.get(site.loc.file as usize).copied().unwrap_or_else(|| {
            panic!("crate `{}`: a `#[{}]` value's file {} is none of the crate's {} files", input.name, site.attr, site.loc.file, names.files.len())
        });
        let in_body = names.made.get(&site.module).is_some_and(|it| {
            it.chain.iter().any(|up| names.made.get(up).is_some_and(|m| m.info.name.is_some_and(crate::blocks::is_block_module)))
        });
        Host {
            cfg: &names.cfg,
            file: shown(file.path, input.shown_from),
            shown_file: core::cell::OnceCell::new(),
            path: site.written,
            package: input.shown_from,
            krate: index,
            text: file.text,
            call_at: site.loc.offset,
            call_name: site.attr,
            call_shape: CallShape::FnLike,
            module_path: names.path_of(site.module),
            in_body,
            sources,
            alloc,
        }
    };
    let values = match input.script {
        Some(out) => crate::attr_values::value_rows(&names, built, &cuts, &|site: &crate::attr_values::ValueSite<'a>| ScriptCx { inner: host_of(site), out }, bump),
        None => crate::attr_values::value_rows(&names, built, &cuts, &host_of, bump),
    };
    names.values = values;
    Ok(names)
}

/// A lock's value; a lock poisoned is a fill thread that panicked holding it, and that panic
/// is this crate's failure (rule 20), named.
fn held<T>(value: LockResult<T>, what: &str) -> T {
    match value {
        Ok(value) => value,
        Err(_) => panic!("a fill thread panicked holding the {what} lock"),
    }
}

// ---------------------------------------------------------------------------------------------
// One body's blocks (D189 step 4).

/// What one held body's own solve made ([`solve_held_body`]): the modules its blocks are (each
/// block that writes an item, the blocks around it, every block of a body lowered in them, and
/// every module an expansion in them wrote), their scopes, what their items add to the
/// tables, and what the solve read outside the body.
#[derive(Debug)]
pub struct BodySolved<'a> {
    /// Each module the body's blocks made, its scope, by key.
    pub scopes: AVec<'a, (ItemKey, ScopeBuilder<'a, &'a str>)>,
    /// How many of the crate's modules outside the body the solve made (its module's chain,
    /// their `#[macro_use]` children, and what its lookups reached): the profile's "modules
    /// made for body solves". Bounded by what the body reads, not by its crate's modules.
    pub made_outside: usize,
    /// The same modules' places and declarations.
    pub made: HashMap<ItemKey, MadeModule<'a>, Alloc<'a>>,
    /// The body's outermost block modules (each keyed by [`make_blocks`] from the body), sorted.
    pub outermost: AVec<'a, ItemKey>,
    /// What the blocks' items add: their impls (`impls`), macros, calls, item texts, members.
    pub tables: Tables<'a>,
    pub expansions: HashMap<ItemKey, Expansion<'a>, Alloc<'a>>,
    pub notes: AVec<'a, NamesNote<'a>>,
    pub reads: BodyReads<'a>,
}

/// What a body's own solve read outside its own text: the read list its record is valid by
/// (`crate::body_scope`).
#[derive(Debug)]
pub struct BodyReads<'a> {
    /// The crate's finished modules a name lookup of the blocks read, sorted.
    pub modules: AVec<'a, ItemKey>,
    /// The crate's enums asked of, sorted.
    pub enums: AVec<'a, ItemKey>,
    /// The crate's own macros (its tables', not the body's) the blocks' calls expanded, each
    /// with its source's digest ([`crate::asked::macro_digest`]), sorted.
    pub macros: AVec<'a, (ItemKey, ContentId)>,
    /// Every question asked of the dependencies, as a read list record ([`crate::asked::encode`]).
    pub deps: AVec<'a, u8>,
}

/// Why a held body's blocks were not solved, naming the body (rules 3, 19).
#[derive(Clone, Debug)]
pub enum BodySolveError {
    /// The crate's module tree record, which places every walked module for the body solve,
    /// does not read.
    TreeRecord { body: ItemKey, error: crate::tree_record::TreeRecordError },
    /// The cell core wrote no scopes for the body's blocks.
    Cells { body: ItemKey, error: crate::cells::Unwritten },
}

impl fmt::Display for BodySolveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BodySolveError::TreeRecord { body, error } => write!(f, "body {body:?}: the crate's module tree record: {error:?}"),
            BodySolveError::Cells { body, error } => write!(f, "body {body:?}: its blocks: {error}"),
        }
    }
}

/// The dependencies one crate's body solves read ([`solve_held_body`]): every crate's slot
/// but the crate's own, whose keys are its finished scopes'. Made once per crate and lent to
/// each of its bodies: from the whole graph's names ([`BodyDeps::of_names`]) or from a
/// per-crate solve's records (crate_scope.rs, `BodyDeps::of_built`).
pub struct BodyDeps<'r, 'a> {
    slots: AVec<'a, Option<Dep<'a>>>,
    by_prefix: &'r HashMap<[u8; 8], usize, Alloc<'a>>,
}

impl<'r, 'a> BodyDeps<'r, 'a> {
    /// The dependencies of crate `krate` of `names`.
    pub fn of_names(names: &'r Names<'a>, krate: usize, alloc: Alloc<'a>) -> BodyDeps<'r, 'a> {
        let mut slots: AVec<'a, Option<Dep<'a>>> = AVec::from_slice_in(&names.slots, alloc);
        if let Some(own) = slots.get_mut(krate) {
            *own = None;
        }
        BodyDeps { slots, by_prefix: &names.by_prefix }
    }

    /// The dependencies a per-crate solve read (its own slot is already `None`).
    pub(crate) fn of_built(built: &Built<'r, 'a>, alloc: Alloc<'a>) -> BodyDeps<'r, 'a> {
        BodyDeps { slots: AVec::from_slice_in(built.crates, alloc), by_prefix: built.by_prefix }
    }
}

/// Solves the blocks of one body `body` of the crate `c` (its crate solve held it:
/// [`CrateNames::bodies`]) as the BODY'S OWN (D189 step 4): its blocks are made as
/// [`make_blocks`] makes them, every macro call in them expanded here, and the cell core solves
/// them alone ([`crate::cells::solve_body_in`]): every module of the crate is answered from its
/// finished scope, every dependency from its record (`deps`). `input` and `source` are the
/// crate's input and its index for `sources`, as its crate solve had them. Nothing of the crate
/// is solved again; nothing here is kept but what is returned.
#[expect(clippy::too_many_arguments, reason = "one body's every input, named")]
pub fn solve_held_body<'a, S: SourceTexts<'a>>(
    c: &'a CrateNames<'a>,
    deps: &BodyDeps<'_, 'a>,
    input: &CrateInput<'a>,
    source: usize,
    body: HeldBody<'a>,
    sources: &S,
    prepares: &'a Prepares<'a>,
    alloc: Bump<'a>,
) -> Result<BodySolved<'a>, BodySolveError> {
    let tree = ReadTree::of(c, alloc).map_err(|error| BodySolveError::TreeRecord { body: body.src.key, error })?;
    solve_held_body_in(&tree, deps, input, source, body, sources, prepares, alloc)
}

/// A crate's module tree record read once (rule 74): the proof every held body's solve of the
/// crate takes ([`solve_held_body_in`]), so a crate of N held bodies decodes its record once,
/// not N times. It carries the crate's names it was read from, so a solve cannot be handed
/// another crate's tree.
pub struct ReadTree<'a> {
    names: &'a CrateNames<'a>,
    record: TreeRecord<'a>,
    /// The crate's files by path, made once: each body's solve finds them in place
    /// ([`FileSet::over`]) instead of indexing every file of its crate again.
    files: &'a FileIndex<'a>,
}

impl<'a> ReadTree<'a> {
    /// `c`'s tree record (`CrateNames::tree_record`), read and checked once, and its files'
    /// index by path.
    pub fn of(c: &'a CrateNames<'a>, bump: Bump<'a>) -> Result<ReadTree<'a>, crate::tree_record::TreeRecordError> {
        let alloc = bump.alloc();
        let mut files: FileIndex<'a> = HashMap::new_in(alloc);
        for (i, f) in c.files.iter().enumerate() {
            files.insert(f.path, u32::try_from(i).expect("a crate's files are counted in u32"));
        }
        let files: &'a FileIndex<'a> = bump.alloc_val(files);
        Ok(ReadTree { names: c, record: TreeRecord::read(c.tree_record, alloc)?, files })
    }

    /// The crate whose record this is.
    pub fn names(&self) -> &'a CrateNames<'a> {
        self.names
    }
}

/// [`solve_held_body`] given its crate's tree record already read ([`ReadTree`]).
#[allow(clippy::too_many_arguments, reason = "solve_held_body's arguments, the crate's read tree in place of its names")]
pub fn solve_held_body_in<'a, S: SourceTexts<'a>>(
    tree: &ReadTree<'a>,
    deps: &BodyDeps<'_, 'a>,
    input: &CrateInput<'a>,
    source: usize,
    body: HeldBody<'a>,
    sources: &S,
    prepares: &'a Prepares<'a>,
    alloc: Bump<'a>,
) -> Result<BodySolved<'a>, BodySolveError> {
    solve_held_body_scratch(tree, deps, input, source, body, sources, prepares, &crate::task::Scratch::kept, alloc)
}

/// [`solve_held_body_in`] with the cell core's working memory in the driver's scratch `hook`
/// (crate::task::Scratch::run; cells/body.rs `solve_body_in` says what it holds): what a pass
/// over all of a crate's held bodies calls, so one body's work is gone before the next body's
/// starts (the block-scopes differential, crate_scope.rs).
#[allow(clippy::too_many_arguments, reason = "solve_held_body_in's arguments and the driver's scratch")]
pub fn solve_held_body_scratch<'a, S: SourceTexts<'a>>(
    tree: &ReadTree<'a>,
    deps: &BodyDeps<'_, 'a>,
    input: &CrateInput<'a>,
    source: usize,
    body: HeldBody<'a>,
    sources: &S,
    prepares: &'a Prepares<'a>,
    hook: crate::task::ScratchHook<'_>,
    bump: Bump<'a>,
) -> Result<BodySolved<'a>, BodySolveError> {
    let alloc = bump.alloc();
    let c = tree.names;
    let at = body.src.key;
    // Every walked module's place and `mod` lines, read from the record (no module's
    // declarations are cloned for a body).
    let record = &tree.record;
    let asked = crate::asked::Asked::new_in(bump);
    let built = Built { crates: &deps.slots, by_prefix: deps.by_prefix, asked: Some(&asked) };
    let kept = crate::expansions::KeptExpansions::none(alloc);
    let no_decls: HashMap<ItemKey, (ModuleInfo<&'a str>, DeclsBuilder<'a, &'a str>), Alloc<'a>> = HashMap::new_in(alloc);
    // The crate's files in place, indexed once per crate (`ReadTree::files`); the planted
    // control (`ReachPlant::WholeTree`) copies and indexes every one for each body, as before.
    let whole_tree = sources.reach_plant() == ReachPlant::WholeTree;
    let files = if whole_tree {
        let mut files = FileSet::new_in(alloc);
        for &f in c.files.iter() {
            files.add(f);
        }
        files
    } else {
        FileSet::over(&c.files[..], tree.files, alloc)
    };
    let run = Run::new_in(files, prepares, alloc);
    let base = Base {
        index: source,
        key: c.key,
        edition: c.edition,
        shown_from: input.shown_from,
        script: input.script,
        cfg: &c.cfg,
        tree: &c.tree,
        tables: &c.tables,
        decls: &no_decls,
        root: &c.crate_root,
        chains: &c.chains,
        record: Some(record),
        built: &built,
        sources,
        listed: input.implicit.proc_macro && sources.proc_exports(source).is_some(),
        kept: &kept,
        blocks: BlocksAt::Body,
        made_before: Some(&c.made),
        alloc,
        bump,
    };
    let macros_read: core::cell::RefCell<AVec<'a, (ItemKey, ContentId)>> = core::cell::RefCell::new(AVec::new_in(alloc));
    let (scopes, made_outside, outermost, modules, enums) = {
        let mark = |_: crate::task::Stage| {};
        let part = |_: ExpandPart, _: &str, _: usize| {};
        let seen = |_: ItemKey, _: &str| {};
        let scratch = |_: &mut dyn FnMut(Alloc<'_>)| crate::task::Scratch::Kept;
        let cx = FillCx { base: &base, run: &run, facts: Facts { base: &base, run: &run }, mark: &mark, part: &part, seen: &seen, scratch: &scratch };
        make_blocks(&base, &run, core::slice::from_ref(&body.src), body.writer);
        let mut outermost: AVec<'a, ItemKey> = AVec::new_in(alloc);
        outermost.extend(held(run.block_places.lock(), "block places").iter().filter(|&(_, &p)| p == body.src.place).map(|(&k, _)| k));
        outermost.sort_unstable();
        // A `macro_rules!` walk out of an outermost block reads the body's module's sequence
        // before the body's place; with no count of that place, a walk out is refused.
        let mut cuts: AVec<'a, (ItemKey, u32)> = AVec::new_in(alloc);
        if let Some(len) = body.cut {
            cuts.extend(outermost.iter().map(|&k| (k, len)));
        }
        // The made modules the body's module is in (an expansion's module): loaded beside the
        // blocks so each block's parent is a module of the solve, answered from its scope.
        let mut chain_made: AVec<'a, ItemKey> = AVec::new_in(alloc);
        if let Some(chain) = c.chains.get(&body.src.module) {
            chain_made.extend(chain.iter().copied().filter(|k| c.made.contains_key(k)));
        }
        let made = |out: &mut AVec<'a, ItemKey>| {
            out.extend_from_slice(&chain_made);
            out.extend(held(run.made.read(), "made modules").keys().copied());
        };
        let own = |k: ItemKey| held(run.made.read(), "made modules").contains_key(&k);
        let places = |m: ItemKey| held(run.block_places.lock(), "block places").get(&m).copied();
        let expand = |call: ItemKey, def: Def| {
            if prefix(def.key) == c.key.prefix()
                && let Some(src) = c.tables.macros.get(&def.key)
            {
                macros_read.borrow_mut().push((def.key, crate::asked::macro_digest(Some(*src))));
            }
            let _ = crate::task::Expand::expand(&cx, ExpandRequest { call, def });
        };
        let enums = crate::cells::LoggedEnums::new(&cx.facts, alloc);
        let placed = match sources.reach_plant() {
            ReachPlant::None | ReachPlant::WholeTree => Some(&c.chains),
            ReachPlant::Unplaced => None,
        };
        let finished = crate::cells::FinishedScopes::new(&c.done, placed, alloc);
        // The body's module and its ancestors: the modules the solve makes at its load (every
        // other module of the crate when a lookup reaches it, cells/body.rs `solve_body_in`).
        // From the root down to the first made module the body is in: that one and what is
        // under it come in round 1 (`made`, their subtrees), as their texts are the expansions'
        // (no `mod` line of the tree names them).
        let mut chain: AVec<'a, ItemKey> = AVec::new_in(alloc);
        if let Some(up) = c.chains.get(&body.src.module) {
            chain.extend(up.iter().rev().copied().take_while(|k| !c.made.contains_key(k)));
        }
        // The crate's own standard prelude (core's, std's) is read as one of its modules.
        let prelude: &[ItemKey] = c.crate_root.head.prelude.and_then(|p| c.chains.get(&p)).map(|up| &up[..]).unwrap_or(&[]);
        let solved = crate::task::Scratch::run(hook, alloc, |work: Alloc<'_>| {
            let tree = crate::cells::BodyTree { made: &made, members: &own, places: &places, cuts: &cuts, chain: &chain[..], prelude, whole_tree };
            crate::cells::solve_body_in(&cx.facts, &built, &enums, &finished, c.root, tree, &expand, work, alloc)
        })
        .map_err(|error| BodySolveError::Cells { body: at, error })?;
        (solved.scopes, solved.made_outside, outermost, finished.read(), enums.read())
    };
    let mut macros = macros_read.into_inner();
    macros.sort_unstable();
    macros.dedup();
    let questions = asked.take();
    let deps = crate::asked::encode(c.key, &[], &questions, alloc);
    let Run { files: _, tables, expansions, made, notes, new_modules: _, block_places: _, held: _, prepares: _ } = run;
    Ok(BodySolved {
        scopes,
        made_outside,
        made: held(made.into_inner(), "made modules"),
        outermost,
        tables: held(tables.into_inner(), "tables"),
        expansions: held(expansions.into_inner(), "expansions"),
        notes: held(notes.into_inner(), "notes"),
        reads: BodyReads { modules, enums, macros, deps },
    })
}

/// What a crate's names are solved over: the files read, the module tree, the modules lowered,
/// and the crate root's record before the cells run.
struct Prepared<'a> {
    set: FileSet<'a>,
    tree: Tree<'a, 'a>,
    low: Lowering<'a>,
    listed: Option<&'a [ProcExport<'a>]>,
    root_attrs: RootAttrs,
    /// The crate root's `CrateRoot` record, aligned.
    root_bytes: &'a [u8],
    /// The module tree record handed in (its bytes, read); `None`: it is written from the walk.
    record: Option<(&'a [u8], TreeRecord<'a>)>,
}

/// Module `i`'s place, as its `ModuleDecls` record's head holds it.
fn module_info<'a>(krate: CrateItemKey, tree: &Tree<'a, 'a>, i: usize) -> ModuleInfo<&'a str> {
    let m = &tree.modules[i];
    ModuleInfo { krate, parent: m.parent.map(|p| tree.modules[p].key), name: m.name, no_implicit_prelude: m.no_implicit_prelude }
}

/// The files read from the root, each the walk names, until it names no file not read; every
/// module lowered; the crate root made.
#[allow(clippy::too_many_arguments, reason = "one crate's inputs, named")]
fn walked_files<'a, S: SourceTexts<'a>, P: Par>(
    graph: &CrateGraph<'a, &'a str>,
    input: &CrateInput<'a>,
    id: CrateId,
    index: usize,
    edition: Edition,
    cfg: &Cfg<'a, 'a>,
    built: &Built<'_, 'a>,
    sources: &S,
    par: &P,
    notes: &mut AVec<'a, NamesNote<'a>>,
    bump: Bump<'a>,
) -> Result<Prepared<'a>, NamesError<'a>> {
    let alloc = bump.alloc();
    par.stage(crate::task::Stage::Files);
    let unread = |error| NamesError::SourceUnread { krate: input.name, error };
    let root_text = sources.source(index, input.root).known().map_err(unread)?.ok_or(NamesError::RootNotRead { krate: input.name, path: input.root })?;
    let mut set = FileSet::new_in(alloc);
    let root_outline = blaze_file_items::outline_auto_extensions(root_text, edition, blaze_file_items::Bodies::Skipped, alloc);
    let root_file = set.add(file_of(input.root, root_text, root_outline, input.shown_from, bump));
    let mut root_dir = AString::from_str_in(input.root, alloc);
    pop_dir(&mut root_dir);
    let mut tried: HashMap<&'a str, (), Alloc<'a>> = HashMap::new_in(alloc);
    let mut walked = None;
    for _ in 0..MOST_WALKS {
        par.names_part(crate::task::NamesPart::Walk);
        let tree = walk(&set, cfg, input.key, input.name, root_file, &root_dir, edition, bump);
        par.names_part(crate::task::NamesPart::Outline);
        let mut read: AVec<'a, (&'a str, &'a str)> = AVec::new_in(alloc);
        for path in tree.missing.iter() {
            if set.find(path).is_some() || tried.contains_key(path.as_str()) {
                continue;
            }
            let path = keep(path, bump);
            tried.insert(path, ());
            if let Some(text) = sources.source(index, path).known().map_err(unread)? {
                read.push((path, text));
            }
        }
        if read.is_empty() {
            walked = Some(tree);
            break;
        }
        let mut outlines = AVec::with_capacity_in(read.len(), alloc);
        par.outline_files(input.name, &read, edition, alloc, &mut outlines)?;
        if outlines.len() != read.len() {
            return Err(NamesError::OutlineCount { krate: input.name, files: read.len(), outlines: outlines.len() });
        }
        for (&(path, text), outlined) in read.iter().zip(outlines) {
            set.add(file_of(path, text, outlined, input.shown_from, bump));
        }
    }
    let tree = walked.ok_or(NamesError::WalkUnbounded { krate: input.name })?;

    // Each walked module's own text, lowered.
    par.stage(crate::task::Stage::Lower);
    let listed = if input.implicit.proc_macro { sources.proc_exports(index) } else { None };
    // The root's attributes first: its `#![register_tool]`s are the crate's tools, which every
    // module's lowering reads to know an item's taker (names-attr-3).
    let root = set.at(root_file as usize);
    let attrs = file_attrs(cfg, &root.outline, root.text, alloc);
    let tools = crate_tools(&attrs, alloc);
    let mut low = lower_modules(input.key, input.implicit.entry, input.implicit.proc_macro, edition, cfg, &tools, &set, &tree, listed, notes, par, None, sources.attr_order(), bump);
    let root_attrs = RootAttrs { no_std: attrs.no_std, no_core: attrs.no_core, arbitrary_ints: attrs.arbitrary_ints };
    let macro_exports = core::mem::replace(&mut low.macro_exports, AVec::new_in(alloc));
    let root_bytes = root_record_of(graph, input, id, root.id, &attrs, &tree, &mut low, &macro_exports, built, notes, bump)?;
    Ok(Prepared { set, tree, low, listed, root_attrs, root_bytes, record: None })
}

/// The crate root's record from the root's lowering (`low.at[0]` is the root) and the crate's
/// `#[macro_export]` macros, aligned.
#[allow(clippy::too_many_arguments, reason = "the root's inputs, named")]
fn root_record_of<'a>(
    graph: &CrateGraph<'a, &'a str>,
    input: &CrateInput<'a>,
    id: CrateId,
    root_file: ContentId,
    attrs: &Attrs<'a, 'a>,
    tree: &Tree<'a, 'a>,
    low: &mut Lowering<'a>,
    macro_exports: &[(&'a str, ItemKey, MacroShapes)],
    built: &Built<'_, 'a>,
    notes: &mut AVec<'a, NamesNote<'a>>,
    bump: Bump<'a>,
) -> Result<&'a [u8], NamesError<'a>> {
    let alloc = bump.alloc();
    let root_attrs = RootAttrs { no_std: attrs.no_std, no_core: attrs.no_core, arbitrary_ints: attrs.arbitrary_ints };
    let plan = root_plan(graph, id, root_attrs, built, notes, bump)?;
    // From 2018 the injected item's name is def-site (rustc's `std_inject`: `ident_span = if
    // edition >= Edition2018 { span } else { call_site }`, `span` with the pass's def-site
    // context): a name the root writes itself is a different name and stands, no duplicate.
    // alloy 2.5.0 under `#![no_std]` (a host unit's features, tempo's precompiles-macros):
    // `pub use alloy_core as core; pub use self::core::primitives;` bound `self::core` to the
    // injected `core` (an item beside an import wins, made.rs), so `alloy::primitives` was not
    // there. The written name is read from the root's own text; one a root macro call writes
    // is not seen (stated gap).
    if let Some(name) = plan.injected.filter(|name| input.edition == Edition::E2015 || !root_binds_type(&low.decls[0], name)) {
        low.decls[0].extern_crates.push(ExternCrateDecl { target: ExternTarget::Dep(name), rename: Rename::Same, vis: WrittenVis::Private });
    }
    let root_input = RootInput {
        krate: input.key,
        root_file,
        edition: input.edition,
        attrs,
        tree,
        prelude_import: low.prelude.as_ref(),
        extern_crates: &low.decls[0].extern_crates,
        macro_use: &low.macro_use,
        macro_exports,
    };
    let mut root_notes = AVec::new_in(alloc);
    let cr = crate_root(&root_input, &plan, &mut root_notes, alloc);
    notes.extend(root_notes.into_iter().map(NamesNote::Root));
    let bytes = cr.write(alloc).map_err(|e| NamesError::RootRecord { krate: input.name, error: RecordError::Refused(e) })?;
    Ok(aligned(&bytes, bump))
}

/// A one-group solve's files and tree from the module tree record `bytes`: the tree is the
/// record's rows; the members' files alone are read and outlined, and the members alone
/// lowered, each checked against what the walk read of it; the crate root is the record's, or
/// made again when the root is a member (its own text's parts, the crate's exports the
/// record's).
#[allow(clippy::too_many_arguments, reason = "one crate's inputs, named")]
fn from_tree_record<'a, S: SourceTexts<'a>, P: Par>(
    graph: &CrateGraph<'a, &'a str>,
    input: &CrateInput<'a>,
    id: CrateId,
    index: usize,
    edition: Edition,
    cfg: &Cfg<'a, 'a>,
    built: &Built<'_, 'a>,
    sources: &S,
    par: &P,
    members: crate::cells::Members<'_>,
    bytes: &'a [u8],
    notes: &mut AVec<'a, NamesNote<'a>>,
    bump: Bump<'a>,
) -> Result<Option<Prepared<'a>>, NamesError<'a>> {
    use crate::cells::MemberRefusal;
    let alloc = bump.alloc();
    let refuse = |r: MemberRefusal| NamesError::Cells { krate: input.name, error: crate::cells::Unwritten::Member(r) };
    par.stage(crate::task::Stage::Files);
    par.tree_record_read(bytes.len());
    let record = TreeRecord::read(bytes, alloc).map_err(|error| refuse(MemberRefusal::TreeRecord { error }))?;
    par.names_part(crate::task::NamesPart::Outline);
    let n = record.modules.len();
    // A member in no row: a module the record's walk did not make (one just added).
    if members.members.iter().any(|k| !record.modules.iter().any(|m| m.key == *k)) {
        return Ok(None);
    }
    let mut set = FileSet::new_in(alloc);
    let mut tree = Tree { modules: AVec::with_capacity_in(n, alloc), by_key: HashMap::with_capacity_in(n, alloc), notes: AVec::new_in(alloc), missing: AVec::new_in(alloc) };
    let mut at: AVec<'a, usize> = AVec::new_in(alloc);
    // The modules outside the members that define macros: lowered for the crate's macro table
    // alone (a member's call, and `shadowed`, read every macro of the crate by its text now).
    let mut defining: AVec<'a, usize> = AVec::new_in(alloc);
    for (i, m) in record.modules.iter().enumerate() {
        let member = members.holds(m.key);
        if member {
            at.push(i);
        } else if m.macros > 0 {
            defining.push(i);
        }
        if member || m.macros > 0 {
            if set.find(m.file).is_none() {
                // A file gone: the file set moved under the record. One not read is no gone one.
                let found = sources.source(index, m.file).known().map_err(|error| NamesError::SourceUnread { krate: input.name, error })?;
                let Some(text) = found else { return Ok(None) };
                set.add(file_of(m.file, text, outline(text, edition, alloc), input.shown_from, bump));
            }
        }
    }
    for (i, m) in record.modules.iter().enumerate() {
        // A module whose text is not read has no file here: nothing of this solve reads one.
        let file = set.find(m.file).unwrap_or(u32::MAX);
        // An inline module of a file read here is found in the file's outline as it is now, by
        // its name under its parent (in the same file, found before it): the record's position
        // moves with every item added before it (the edit to `a` of USE_ADDED moved `r`'s, and
        // `r` was lowered from `b`'s place).
        let item = match (m.item, m.parent) {
            (Some(_), Some(p)) if file != u32::MAX => {
                let f = set.at(file as usize);
                let parent = tree.modules[p as usize].item;
                let Some(name) = m.name else { return Ok(None) };
                let Some(found) = inline_child(cfg, &f.outline, f.text, parent, name, alloc) else { return Ok(None) };
                Some(found)
            }
            (item, _) => item.map(ItemIndex),
        };
        tree.by_key.insert(m.key, i);
        tree.modules.push(crate::task::Module {
            key: m.key,
            parent: m.parent.map(|p| p as usize),
            name: m.name,
            file,
            item,
            dir: AString::from_str_in(m.dir, alloc),
            path_dir: AString::from_str_in(m.path_dir, alloc),
            no_implicit_prelude: m.no_implicit_prelude,
            path: AString::from_str_in(m.path, alloc),
        });
    }
    // Each member's text as the walk reads it: equal to its row, the walk makes the same
    // children of it.
    par.names_part(crate::task::NamesPart::Walk);
    let mut rows = AVec::new_in(alloc);
    for &i in at.iter() {
        let m = &tree.modules[i];
        let f = set.at(m.file as usize);
        rows.clear();
        let file_attrs = walk_rows(cfg, &f.outline, f.text, m.item, edition, &mut rows, bump);
        let row = &record.modules[i];
        if rows[..] != row.rows[..] || file_attrs != row.file_attrs {
            return Ok(None);
        }
    }

    par.stage(crate::task::Stage::Lower);
    let listed = if input.implicit.proc_macro { sources.proc_exports(index) } else { None };
    let root_member = at.first() == Some(&0);
    let stored = aligned(record.crate_root, bump);
    let (tools, attrs) = if root_member {
        let root = set.at(tree.modules[0].file as usize);
        let attrs = file_attrs(cfg, &root.outline, root.text, alloc);
        (crate_tools(&attrs, alloc), Some((root.id, attrs)))
    } else {
        (AVec::from_slice_in(&record.tools, alloc), None)
    };
    // The members' notes are kept only when the record holds: a tree made again notes its own.
    let mut own_notes = AVec::new_in(alloc);
    let mut low = lower_modules(input.key, input.implicit.entry, input.implicit.proc_macro, edition, cfg, &tools, &set, &tree, listed, &mut own_notes, par, Some(at), sources.attr_order(), bump);
    let stored_view = CrateRootView::read(stored).map_err(|_| refuse(MemberRefusal::TreeCrateRoot))?;
    if members_moved(input.key, &tree, &low, &record, &stored_view, bump) {
        return Ok(None);
    }
    if !defining.is_empty() {
        // Their macro table rows alone: their scopes, notes and calls are their records'.
        let mut dropped = AVec::new_in(alloc);
        let outside = lower_modules(input.key, input.implicit.entry, input.implicit.proc_macro, edition, cfg, &tools, &set, &tree, listed, &mut dropped, par, Some(defining), sources.attr_order(), bump);
        let found = macros_by_module(&outside.tables.macros, alloc);
        if outside.at.iter().any(|&i| found.get(&tree.modules[i].key).copied().unwrap_or(0) != record.modules[i].macros) {
            // A module's macros moved under the record: its inputs moved.
            return Ok(None);
        }
        low.tables.macros.extend(outside.tables.macros);
    }
    let notes = {
        notes.extend(own_notes);
        notes
    };
    let (root_attrs, root_bytes) = match attrs {
        None => (RootAttrs { no_std: record.no_std, no_core: record.no_core, arbitrary_ints: record.arbitrary_ints }, stored),
        Some((root_file, attrs)) => {
            let exports: AVec<'a, (&'a str, ItemKey, MacroShapes)> = AVec::from_iter_in(stored_view.macro_exports(), alloc);
            let root_attrs = RootAttrs { no_std: attrs.no_std, no_core: attrs.no_core, arbitrary_ints: attrs.arbitrary_ints };
            (root_attrs, root_record_of(graph, input, id, root_file, &attrs, &tree, &mut low, &exports, built, notes, bump)?)
        }
    };
    Ok(Some(Prepared { set, tree, low, listed, root_attrs, root_bytes, record: Some((bytes, record)) }))
}

/// Whether a member's `mod` lines or `#[macro_export]` macros (its record written and read
/// back, the form the tree record's lines were taken in) are not the tree record's row and
/// crate root's: the record's inputs moved.
fn members_moved<'a>(krate: CrateItemKey, tree: &Tree<'a, 'a>, low: &Lowering<'a>, record: &TreeRecord<'a>, root: &CrateRootView<'_>, bump: Bump<'a>) -> bool {
    let alloc = bump.alloc();
    for (&i, d) in low.at.iter().zip(low.decls.iter()) {
        let key = tree.modules[i].key;
        let row = &record.modules[i];
        let Ok(bytes) = d.write(key, &module_info(krate, tree, i), alloc) else { return true };
        let Ok(view) = ModuleDeclsView::read(aligned(&bytes, bump)) else { return true };
        let d = DeclsBuilder::from(&view, alloc);
        if d.mods.len() != row.lines.len() || d.mods.iter().zip(row.lines.iter()).any(|(c, l)| (c.module, c.macro_use) != *l) {
            return true;
        }
        let mut exports = 0u32;
        for r in d.macro_rules.iter().filter(|r| r.export) {
            exports += 1;
            if !root.macro_exports().any(|(name, def, shapes)| name == r.name && def == r.def && shapes == r.shapes) {
                return true;
            }
        }
        if exports != row.exports {
            return true;
        }
        // Its enums and their variants: what the record answers a solve that does not lower it.
        let mut own: AVec<'a, (ItemKey, &[Variant<&'a str>])> = AVec::from_iter_in(module_enums(&d, &low.tables.enums), alloc);
        own.sort_unstable_by_key(|e| e.0);
        own.dedup_by_key(|e| e.0);
        let mut recorded = record.enums.iter().filter(|e| e.module as usize == i);
        for &(key, variants) in own.iter() {
            match recorded.next() {
                Some(e) if e.key == key && e.variants[..] == *variants => {}
                _ => return true,
            }
        }
        if recorded.next().is_some() {
            return true;
        }
    }
    let defined = macros_by_module(&low.tables.macros, alloc);
    low.at.iter().any(|&i| defined.get(&tree.modules[i].key).copied().unwrap_or(0) != record.modules[i].macros)
}

/// How many macros each module's own text defines, by the lowered macro table.
fn macros_by_module<'a>(macros: &HashMap<ItemKey, MacroSrc<'a>, Alloc<'a>>, alloc: Alloc<'a>) -> HashMap<ItemKey, u32, Alloc<'a>> {
    let mut defined: HashMap<ItemKey, u32, Alloc<'a>> = HashMap::new_in(alloc);
    for m in macros.values() {
        *defined.entry(m.module).or_insert(0) += 1;
    }
    defined
}

/// The module tree record of a walked crate, written: each module's place and file, its `mod`
/// lines and exports from its record read back, what the walk read of its text.
#[allow(clippy::too_many_arguments, reason = "the record's parts, named")]
fn tree_record_of<'a>(
    set: &FileSet<'a>,
    tree: &Tree<'a, 'a>,
    decls: &HashMap<ItemKey, (ModuleInfo<&'a str>, DeclsBuilder<'a, &'a str>), Alloc<'a>>,
    macros: &HashMap<ItemKey, MacroSrc<'a>, Alloc<'a>>,
    enums: &HashMap<ItemKey, AVec<'a, Variant<&'a str>>, Alloc<'a>>,
    cfg: &Cfg<'a, 'a>,
    edition: Edition,
    tools: &[&'a str],
    root_attrs: RootAttrs,
    crate_root: &'a [u8],
    bump: Bump<'a>,
) -> Result<AVec<'a, u8>, crate::tree_record::TreeRecordError> {
    let alloc = bump.alloc();
    let defined = macros_by_module(macros, alloc);
    let mut modules = AVec::with_capacity_in(tree.modules.len(), alloc);
    for m in tree.modules.iter() {
        let f = set.at(m.file as usize);
        let mut rows = AVec::new_in(alloc);
        let file_attrs = walk_rows(cfg, &f.outline, f.text, m.item, edition, &mut rows, bump);
        let (lines, exports) = match decls.get(&m.key) {
            Some((_, d)) => (AVec::from_iter_in(d.mods.iter().map(|c| (c.module, c.macro_use)), alloc), d.macro_rules.iter().filter(|r| r.export).count()),
            None => (AVec::new_in(alloc), 0),
        };
        modules.push(TreeModule {
            key: m.key,
            parent: m.parent.map(|p| p as u32),
            name: m.name,
            file: f.path,
            item: m.item.map(|it| it.0),
            dir: m.dir.as_str(),
            path_dir: m.path_dir.as_str(),
            no_implicit_prelude: m.no_implicit_prelude,
            path: m.path.as_str(),
            lines,
            rows,
            file_attrs,
            exports: exports as u32,
            macros: defined.get(&m.key).copied().unwrap_or(0),
        });
    }
    let mut tree_enums: AVec<'a, TreeEnum<'a>> = AVec::new_in(alloc);
    for (i, m) in tree.modules.iter().enumerate() {
        if let Some((_, d)) = decls.get(&m.key) {
            for (key, variants) in module_enums(d, enums) {
                tree_enums.push(TreeEnum { key, module: i as u32, variants: AVec::from_slice_in(variants, alloc) });
            }
        }
    }
    tree_enums.sort_unstable_by_key(|e| e.key);
    tree_enums.dedup_by_key(|e| e.key);
    let record = TreeRecord {
        modules,
        enums: tree_enums,
        tools: AVec::from_slice_in(tools, alloc),
        no_std: root_attrs.no_std,
        no_core: root_attrs.no_core,
        arbitrary_ints: root_attrs.arbitrary_ints,
        crate_root,
    };
    record.write(alloc)
}

/// The enums module `d`'s own text declares (its `ModuleDecls` read back), in declaration
/// order, with their variants as the lowering made them (`enums`, the lowered table). The
/// callers sort them by key and keep each once, the tree record's form.
fn module_enums<'d, 'a>(d: &'d DeclsBuilder<'a, &'a str>, enums: &'d HashMap<ItemKey, AVec<'a, Variant<&'a str>>, Alloc<'a>>) -> impl Iterator<Item = (ItemKey, &'d [Variant<&'a str>])> + 'd {
    d.items.iter().filter(|it| it.def.kind == DefKind::Enum).filter_map(move |it| enums.get(&it.def.key).map(|v| (it.def.key, v.as_slice())))
}

fn lower_modules<'a, P: Par>(
    krate: CrateItemKey,
    entry: bool,
    // `Implicit::proc_macro`: the root's fragment ends with the harness call (D175).
    harness: bool,
    edition: Edition,
    cfg: &Cfg<'a, 'a>,
    tools: &[&'a str],
    set: &FileSet<'a>,
    tree: &Tree<'a, 'a>,
    listed: Option<&'a [ProcExport<'a>]>,
    notes: &mut AVec<'a, NamesNote<'a>>,
    par: &P,
    only: Option<AVec<'a, usize>>,
    order: AttrOrder,
    bump: Bump<'a>,
) -> Lowering<'a> {
    let alloc = bump.alloc();
    // The modules lowered, ascending: every one, or a one-group solve's members.
    let at: AVec<'a, usize> = only.unwrap_or_else(|| AVec::from_iter_in(0..tree.modules.len(), alloc));
    let mut parts = AVec::with_capacity_in(at.len(), alloc);
    par.map(
        at.len(),
        |j| {
            let i = at[j];
            let m = &tree.modules[i];
            let file = set.at(m.file as usize);
            let fragment = Fragment {
                // A file: no expansion wrote it.
                marks: None,
                tokens: None,
                module: m.key,
                owner: m.key,
                text: file.text,
                outline: &file.outline,
                items: m.item,
                at: Where::File(m.file),
                depth: 0,
                made: false,
                members_of: None,
                written: file.path,
                kept: None,
            };
            // The root's fragment (the walk's first module) declares the module's list.
            let procs = match listed {
                None => ProcDecls::Text,
                Some(list) => ProcDecls::Module { here: if i == 0 { list } else { &[] } },
            };
            lower(LowerCx { krate, tools, edition, cfg, walked: tree, procs, entry: entry && i == 0, harness: harness && i == 0, order }, fragment, bump)
        },
        &mut parts,
    );
    let mut out = Lowering {
        decls: AVec::with_capacity_in(parts.len(), alloc),
        at: AVec::new_in(alloc),
        tables: Tables::new_in(alloc),
        prelude: None,
        macro_use: AVec::new_in(alloc),
        macro_exports: AVec::new_in(alloc),
        bodies: AVec::new_in(alloc),
    };
    for (&i, mut part) in at.iter().zip(parts) {
        let m = &tree.modules[i];
        out.bodies.extend(part.bodies.iter().copied());
        for rules in part.decls.macro_rules.iter() {
            if rules.export {
                out.macro_exports.push((rules.name, rules.def, rules.shapes));
            }
        }
        let prelude = part.prelude.take();
        if i == 0 {
            out.prelude = prelude;
            out.macro_use = core::mem::replace(&mut part.macro_use, AVec::new_in(alloc));
        } else if prelude.is_some() {
            notes.push(NamesNote::PreludeImportOutsideRoot { module: m.key });
        }
        let decls = core::mem::replace(&mut part.decls, DeclsBuilder::new_in(alloc));
        let lowered = out.tables.add(m.key, At::File(m.file), part);
        notes.extend(lowered.into_iter().map(NamesNote::Lower));
        out.decls.push(decls);
    }
    out.at = at;
    out
}

/// Whether the root's own text binds `name` in the type namespace: an item, a `mod`, a named
/// `use` (as its last segment or its rename) or an `extern crate` (the injected one is added
/// after this is asked).
fn root_binds_type(root: &DeclsBuilder<'_, &str>, name: &str) -> bool {
    root.items.iter().any(|item| item.ns == Namespace::Types && item.name == name)
        || root.mods.iter().any(|m| m.name == name)
        || root.uses.iter().any(|u| match &u.leaf {
            blaze_surface::UseForm::Named { rename: Rename::Same, .. } => u.path.segments.last().is_some_and(|last| *last == name),
            blaze_surface::UseForm::Named { rename: Rename::To(to), .. } => *to == name,
            _ => false,
        })
        || root.extern_crates.iter().any(|ec| match (&ec.rename, &ec.target) {
            (Rename::To(to), _) => *to == name,
            (Rename::Same, ExternTarget::Dep(dep)) => *dep == name,
            _ => false,
        })
}

/// The crate root's plan from the graph and the finished crates.
fn root_plan<'a>(
    graph: &CrateGraph<'a, &'a str>,
    id: CrateId,
    attrs: RootAttrs,
    built: &Built<'_, 'a>,
    notes: &mut AVec<'a, NamesNote<'a>>,
    bump: Bump<'a>,
) -> Result<RootPlan<'a, 'a>, NamesError<'a>> {
    let alloc = bump.alloc();
    let externs = graph.externs(id, attrs);
    let krate = graph.get(id).name;
    notes.extend(externs.dropped.iter().map(|d| NamesNote::ExternDropped { krate, name: d.name, why: d.why }));
    let root_of = |c: CrateId| built.at(c.index()).map(|f| (f.key(), f.root()));
    // A crate the plan names is finished before this one; one that is not is refused by name,
    // never left out of the root (its paths would then resolve to nothing, with no word why).
    let finished = |d: &GraphDep<&'a str>| {
        root_of(d.krate).ok_or(NamesError::DepNotFinished { krate, name: d.name, dep: graph.get(d.krate).name })
    };
    let mut extern_prelude = AVec::new_in(alloc);
    for d in externs.prelude.iter() {
        let (_, root) = finished(d)?;
        extern_prelude.push((d.name, root));
    }
    let mut deps = AVec::new_in(alloc);
    for d in externs.deps.iter() {
        let (dep, root) = finished(d)?;
        deps.push(blaze_surface::Dep { name: d.name, krate: dep, root });
    }
    let macro_use = externs.macro_use.and_then(|c| root_of(c).map(|it| it.1));
    // rustc's injected `#[macro_use] extern crate std;` (`core` under `#![no_std]`) is an item
    // of the root: `crate::std` names it, and in 2015 `use std::..` reads it there.
    let injected = externs.macro_use.filter(|c| root_of(*c).is_some()).map(|_| if attrs.no_std { "core" } else { "std" });
    let prelude = externs.std_prelude.map(|p| {
        let mut why = BString::new_in(bump);
        let Some(other) = built.at(p.krate.index()) else {
            push_fmt(&mut why, format_args!("crate {} not finished", p.krate.0));
            return Err(&*why.into_bump_str());
        };
        let mut at = other.root();
        for segment in p.path {
            let found = built.lookup(at, Namespace::Types, segment, alloc);
            built.ask(|| crate::asked::Question::lookup(at, Namespace::Types, segment, found, alloc));
            match found {
                Some(b) if b.state == BindingState::Determined => at = b.def.key,
                found => {
                    push_fmt(&mut why, format_args!("`{}", other.name()));
                    for s in p.path {
                        push_fmt(&mut why, format_args!("::{s}"));
                    }
                    push_fmt(&mut why, format_args!("`: `{segment}` is {found:?}"));
                    return Err(&*why.into_bump_str());
                }
            }
        }
        Ok(at)
    });
    Ok(RootPlan { extern_prelude, deps, macro_use, prelude, injected })
}

fn push_fmt<A: core::alloc::Allocator>(out: &mut blaze_own::String<A>, args: fmt::Arguments<'_>) {
    out.write_fmt(args).expect("writing to a string fails only when its allocator does");
}

// ---------------------------------------------------------------------------------------------
// The fill's environment: the crate's records, the finished crates, and what expansions add.

struct Base<'r, 'a, S> {
    index: usize,
    key: CrateItemKey,
    edition: Edition,
    shown_from: &'a str,
    /// The crate's build-script outputs, laid over the expansion host.
    script: Option<OutputsRef<'a>>,
    cfg: &'r Cfg<'a, 'a>,
    tree: &'r Tree<'a, 'a>,
    /// The tables of the walked modules' own texts.
    tables: &'r Tables<'a>,
    decls: &'r HashMap<ItemKey, (ModuleInfo<&'a str>, DeclsBuilder<'a, &'a str>), Alloc<'a>>,
    root: &'r CrateRootBuilder<'a, &'a str>,
    chains: &'r HashMap<ItemKey, AVec<'a, ItemKey>, Alloc<'a>>,
    /// A one-group solve's module tree record: the place and `mod` lines of every module
    /// whose text it does not lower.
    record: Option<&'r TreeRecord<'a>>,
    built: &'r Built<'r, 'a>,
    sources: &'r S,
    /// A macro crate whose module's list declared its macros: an expansion declares none.
    listed: bool,
    /// The crate's expansions kept between solves ([`crate::expansions`]): asked and told by
    /// [`expand_decls`] alone.
    kept: &'r crate::expansions::KeptExpansions<'r, 'a>,
    /// What becomes of the blocks of a body lowered here (D189's seam).
    blocks: BlocksAt,
    /// A body's own solve ([`BlocksAt::Body`]): the modules the crate's expansions made, as the
    /// crate solve finished them: a place, `mod` lines and a parent for a made module the body
    /// is written in (its blocks' parent), read where the run's own made modules are.
    /// `None` in a crate solve.
    made_before: Option<&'r HashMap<ItemKey, MadeModule<'a>, Alloc<'a>>>,
    alloc: Alloc<'a>,
    bump: Bump<'a>,
}

/// What becomes of the blocks of a body lowered in a solve (D189's seam,
/// [`crate::surface::BlockScopes`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum BlocksAt {
    /// `Modules`: the crate solve makes them modules of the crate ([`make_blocks`]).
    Crate,
    /// `Bodies`, the crate solve: the body is held ([`HeldBody`]) for its own solve.
    Held,
    /// `Bodies`, one body's own solve ([`solve_held_body`]): its blocks, and those of each
    /// body lowered in them, are made here ([`make_blocks`]).
    Body,
}

/// The blocks of `bodies` (written by `writer`, an expansion, or by a file: `None`) as the
/// solve's [`BlocksAt`] says: made, or held. A body is held only when its blocks can write an
/// item ([`blaze_file_items::nested::Nested::writes`], the scan [`make_blocks`] reads first):
/// any other body's blocks declare nothing a name or an impl table reads, and its own lowering
/// answers its block-local names.
fn blocks_of<'a, S: SourceTexts<'a>>(base: &Base<'_, 'a, S>, run: &Run<'a>, bodies: &[crate::task::BodySrc<'a>], writer: Option<&'a ExpansionText<'a>>) {
    match base.blocks {
        BlocksAt::Crate | BlocksAt::Body => make_blocks(base, run, bodies, writer),
        BlocksAt::Held => {
            let writes = |b: &crate::task::BodySrc<'a>| {
                let nested = blaze_file_items::nested::scan(b.text, base.alloc);
                nested.end == blaze_file_items::nested::NestedEnd::Whole && nested.writes()
            };
            let mut held_bodies = held(run.held.lock(), "held bodies");
            held_bodies.extend(bodies.iter().filter(|b| writes(b)).map(|&b| (b, writer)));
        }
    }
}

impl<'a, S> Base<'_, 'a, S> {
    /// Where an expansion's procedural macros are declared from: never from its text when
    /// the module's list declared the crate's.
    fn expansion_procs(&self) -> ProcDecls<'a> {
        if self.listed { ProcDecls::Module { here: &[] } } else { ProcDecls::Text }
    }

    /// Macro `def`'s source: this crate's lowered tables (a proc macro is declared by its
    /// crate's text, never by an expansion), else its defining crate's record, the read asked so
    /// the crate's key holds it.
    fn macro_src(&self, def: ItemKey) -> Option<MacroSrc<'a>> {
        if let Some(src) = self.tables.macros.get(&def) {
            return Some(*src);
        }
        let src = self.built.of(def).and_then(|d| d.macro_of(def));
        self.built.ask(|| crate::asked::Question::macro_src(def, src, self.alloc));
        src
    }
}

/// What the expansions of one crate add, shared by the fill's threads.
struct Run<'a> {
    files: Mutex<FileSet<'a>>,
    tables: RwLock<Tables<'a>>,
    expansions: Mutex<HashMap<ItemKey, Expansion<'a>, Alloc<'a>>>,
    made: RwLock<HashMap<ItemKey, MadeModule<'a>, Alloc<'a>>>,
    notes: Mutex<AVec<'a, NamesNote<'a>>>,
    /// Made modules the fill has not been told of yet.
    new_modules: Mutex<AVec<'a, ItemKey>>,
    /// Each body's outermost block module: where the body is written (`BodySrc::place`), which
    /// its textual scope goes on from. Lookups only.
    block_places: Mutex<HashMap<ItemKey, (ItemKey, u32), Alloc<'a>>>,
    /// The bodies whose blocks are left to their own solve ([`BlocksAt::Held`]), each with the
    /// expansion that wrote it.
    held: Mutex<AVec<'a, (crate::task::BodySrc<'a>, Option<&'a ExpansionText<'a>>)>>,
    /// Each declarative macro this crate's item calls name, prepared once: the call's set,
    /// shared by every crate it resolves (a definition is keyed by its defining item, which
    /// names its crate).
    prepares: &'a Prepares<'a>,
}

impl<'a> Run<'a> {
    fn new_in(files: FileSet<'a>, prepares: &'a Prepares<'a>, alloc: Alloc<'a>) -> Run<'a> {
        Run {
            prepares,
            files: Mutex::new(files),
            tables: RwLock::new(Tables::new_in(alloc)),
            expansions: Mutex::new(HashMap::new_in(alloc)),
            made: RwLock::new(HashMap::new_in(alloc)),
            notes: Mutex::new(AVec::new_in(alloc)),
            new_modules: Mutex::new(AVec::new_in(alloc)),
            block_places: Mutex::new(HashMap::new_in(alloc)),
            held: Mutex::new(AVec::new_in(alloc)),
        }
    }

    fn note(&self, note: NamesNote<'a>) {
        held(self.notes.lock(), "notes").push(note);
    }
}

struct Facts<'r, 'a, S> {
    base: &'r Base<'r, 'a, S>,
    run: &'r Run<'a>,
}

/// This crate's enums for the cell core: the lowered text's, then the expansions' so far.
impl<'a, S> crate::cells::EnumNames<&'a str> for Facts<'_, 'a, S> {
    fn holds(&self, of: ItemKey) -> bool {
        self.base.tables.enums.contains_key(&of) || held(self.run.tables.read(), "tables").enums.contains_key(&of) || self.recorded_enum(of).is_some()
    }

    fn variant(&self, of: ItemKey, ns: Namespace, name: &'a str) -> crate::cells::DepAnswer {
        let answer = |variants: &[Variant<&str>]| match variant_answer(variants.iter().copied(), ns, name) {
            NameAnswer::Found { def, vis, .. } => crate::cells::DepAnswer::Found(def, vis),
            NameAnswer::Absent => crate::cells::DepAnswer::NotFound,
        };
        if let Some(v) = self.base.tables.enums.get(&of) {
            return answer(v);
        }
        if let Some(v) = held(self.run.tables.read(), "tables").enums.get(&of) {
            return answer(v);
        }
        match self.recorded_enum(of) {
            Some(v) => answer(v),
            None => crate::cells::DepAnswer::Unknown,
        }
    }
}

impl<'a, S> Facts<'_, 'a, S> {
    /// An enum of a module this one-group solve did not lower, as the module tree record holds
    /// it (the module's text is the record's: `members_moved` walked again otherwise). A lowered
    /// module's enums are the tables' (equal to its row), an expansion's are the run's.
    fn recorded_enum(&self, of: ItemKey) -> Option<&[Variant<&'a str>]> {
        self.base.record.and_then(|r| r.enum_of(of)).map(|e| &e.variants[..])
    }
}

impl<'a, S> ScopeFacts for Facts<'_, 'a, S> {
    type Name = &'a str;

    fn module(&self, module: ItemKey) -> Result<ModuleInfo<&'a str>, SurfaceError> {
        if let Some(it) = self.base.decls.get(&module) {
            return Ok(it.0);
        }
        // A module a one-group solve does not lower: its place is the tree record's.
        if let (Some(_), Some(&i)) = (self.base.record, self.base.tree.by_key.get(&module)) {
            return Ok(module_info(self.base.key, self.base.tree, i));
        }
        if let Some(it) = self.base.made_before.and_then(|m| m.get(&module)) {
            return Ok(it.info);
        }
        held(self.run.made.read(), "made modules")
            .get(&module)
            .map(|it| it.info)
            .ok_or(SurfaceError::NoRecord { fact: "ModuleDecls", item: module })
    }

    fn decls(&self, module: ItemKey) -> Result<DeclsBuilder<'_, &'a str>, SurfaceError> {
        if let Some(it) = self.base.decls.get(&module) {
            return Ok(it.1.clone());
        }
        if let Some(it) = self.base.made_before.and_then(|m| m.get(&module)) {
            return Ok(it.decls.clone());
        }
        held(self.run.made.read(), "made modules")
            .get(&module)
            .map(|it| it.decls.clone())
            .ok_or(SurfaceError::NoRecord { fact: "ModuleDecls", item: module })
    }

    fn crate_root(&self, krate: CrateItemKey) -> Result<CrateRootBuilder<'_, &'a str>, SurfaceError> {
        if krate == self.base.key {
            Ok(self.base.root.clone())
        } else {
            Err(SurfaceError::NoCrateRecord { fact: "CrateRoot", krate })
        }
    }

    fn expansion(&self, call: ItemKey) -> Result<Option<DeclsBuilder<'_, &'a str>>, SurfaceError> {
        Ok(held(self.run.expansions.lock(), "expansions").get(&call).map(|e| e.decls.clone()))
    }

    /// A derive's helpers from its `attributes(..)`, as written: a proc derive's
    /// (`#[proc_macro_derive(D, attributes(h))]`) and a built-in derive's, which the pinned
    /// library writes the same way (core/src/default.rs `#[rustc_builtin_macro(Default,
    /// attributes(default))]`), kept in its record (names-attr-4). A macro that is no derive
    /// declares none.
    fn derive_helpers(&self, derive: ItemKey, out: &mut AVec<'_, &'a str>) -> Result<(), SurfaceError> {
        let Some(src) = self.base.macro_src(derive) else {
            return Err(SurfaceError::NoRecord { fact: "MacroSrc", item: derive });
        };
        match src.body {
            MacroBody::Proc { kind: ProcKind::Derive, helpers } | MacroBody::BuiltinDerive { helpers } => {
                out.extend(ProcAttr::helper_names(helpers));
                Ok(())
            }
            MacroBody::Rules(_) | MacroBody::Macro2 { .. } | MacroBody::BuiltinFn(_) | MacroBody::BuiltinOther(_) | MacroBody::Proc { .. } => Ok(()),
        }
    }

    /// A module a one-group solve does not lower: its `mod` lines are the tree record's.
    fn mod_lines(&self, module: ItemKey, out: &mut AVec<'_, (ItemKey, bool)>) -> bool {
        let Some(record) = self.base.record else { return false };
        if self.base.decls.contains_key(&module) {
            return false;
        }
        match self.base.tree.by_key.get(&module) {
            Some(&i) => {
                out.extend_from_slice(&record.modules[i].lines);
                true
            }
            None => false,
        }
    }
}

struct FillCx<'r, 'a, S> {
    base: &'r Base<'r, 'a, S>,
    run: &'r Run<'a>,
    facts: Facts<'r, 'a, S>,
    /// The caller's stage mark ([`crate::task::Par::stage`]): an expansion is its own stretch.
    mark: &'r dyn Fn(crate::task::Stage),
    /// The caller's part mark ([`crate::task::Par::expand_part`]).
    part: PartMark<'r>,
    /// The caller's call mark ([`crate::task::Par::expand_call`]).
    seen: &'r dyn Fn(ItemKey, &str),
    /// The caller's call scratch ([`crate::task::Par::call_scratch`]).
    scratch: CallScratch<'r>,
}

/// [`crate::task::Par::expand_part`] as [`expand_decls`] takes it.
type PartMark<'r> = &'r dyn Fn(ExpandPart, &str, usize);

/// [`crate::task::Par::call_scratch`] as [`expand_decls`] takes it.
type CallScratch<'r> = &'r dyn Fn(&mut dyn FnMut(Alloc<'_>)) -> crate::task::Scratch;

impl<'a, S: SourceTexts<'a>> crate::task::Expand for FillCx<'_, 'a, S> {
    fn expand(&self, request: ExpandRequest) -> crate::task::Expanded {
        (self.mark)(crate::task::Stage::Expand);
        (self.part)(ExpandPart::Find, "", 0);
        // A refused call is stored REFUSED, never as an empty expansion (rules 3 and 19): its
        // module's scope then holds the refusal (`crate::refused::write_refused`), and a name
        // looked up behind it is unknown because of it, not absent.
        let env = crate::refused::EnvRefusal::new_in(self.base.bump);
        // The note that refuses the call: the last the driver notes for it before it returns
        // nothing (every way it does notes why first), kept for the names record's row.
        let last = core::cell::Cell::new(None);
        let expansion = match expand_decls(self.base, self.run, request, self.part, self.seen, self.scratch, &env, &last) {
            Some((decls, made)) => Expansion { decls, made, refused: None, refused_by: None },
            None => {
                let src = self.base.tables.calls.get(&request.call).copied().or_else(|| held(self.run.tables.read(), "tables").calls.get(&request.call).copied());
                // `OUT_DIR` is set for the package (it has a build script) but not known: no
                // outputs record is joined (crate_env.rs `CrateEnv::of`).
                let out_dir_unknown = self.base.script.is_none()
                    && self.base.sources.crate_env(self.base.index).is_some_and(|e| e.get("OUT_DIR") == Err(crate::crate_env::EnvUnknown));
                let refused = crate::refused::Refused::of(src.map(|s| (s.module, s.name)), env.first(), out_dir_unknown);
                Expansion { decls: DeclsBuilder::new_in(self.base.alloc), made: None, refused: Some(refused), refused_by: last.get().map(|note| (note, env.first())) }
            }
        };
        (self.part)(ExpandPart::Store, "", 0);
        held(self.run.expansions.lock(), "expansions").insert(request.call, expansion);
        (self.part)(ExpandPart::End, "", 0);
        (self.mark)(crate::task::Stage::Solve);
        crate::task::Expanded::Made
    }
}

impl<'r, 'a, S: SourceTexts<'a>> FillEnv<&'a str> for FillCx<'r, 'a, S> {
    type Facts = Facts<'r, 'a, S>;

    fn facts(&self) -> &Facts<'r, 'a, S> {
        &self.facts
    }

    fn here(&self) -> CrateItemKey {
        self.base.key
    }

    fn parent(&self, module: ItemKey) -> Option<Option<ItemKey>> {
        if let Some(it) = self.base.decls.get(&module) {
            return Some(it.0.parent);
        }
        if let (Some(_), Some(&i)) = (self.base.record, self.base.tree.by_key.get(&module)) {
            return Some(self.base.tree.modules[i].parent.map(|p| self.base.tree.modules[p].key));
        }
        if let Some(it) = self.base.made_before.and_then(|m| m.get(&module)) {
            return Some(it.info.parent);
        }
        held(self.run.made.read(), "made modules").get(&module).map(|it| it.info.parent)
    }

    fn outer(&self, key: ItemKey) -> Held<'_, &'a str> {
        let alloc = self.base.alloc;
        if let Some(v) = self.base.tables.enums.get(&key) {
            return Held::Variants { variants: AVec::from_slice_in(v, alloc), krate: self.base.key };
        }
        if let Some(v) = held(self.run.tables.read(), "tables").enums.get(&key) {
            return Held::Variants { variants: AVec::from_slice_in(v, alloc), krate: self.base.key };
        }
        if let Some(v) = self.facts.recorded_enum(key) {
            return Held::Variants { variants: AVec::from_slice_in(v, alloc), krate: self.base.key };
        }
        let Some(other) = self.base.built.of(key) else { return Held::Unknown };
        if let Some(scope) = other.scope(key) {
            return Held::Scope { scope, krate: other.key(), chain: other.chain(key) };
        }
        if let Some(v) = other.enum_of(key) {
            return Held::Variants { variants: AVec::from_iter_in(v.iter(), alloc), krate: other.key() };
        }
        Held::Failed { krate: other.key(), chain: None }
    }

    fn asked(&self, read: crate::read::Read<&'a str>, own: &crate::task::Own<'_, &'a str>) {
        // Only a question of another crate's record is a read of the list; this crate's own
        // modules are solved here.
        if read.module().0.0[..8] != self.base.key.prefix() {
            self.base.built.ask_maybe(|| crate::asked::Question::of_read(read, own, self.base.alloc));
        }
    }

    fn made(&self, out: &mut AVec<'_, ItemKey>) {
        let mut new = held(self.run.new_modules.lock(), "new modules");
        out.extend_from_slice(&new);
        new.clear();
    }

    fn derive_helpers(&self, derive: ItemKey) -> Option<&str> {
        let src = self.base.macro_src(derive)?;
        match src.body {
            MacroBody::Proc { kind: ProcKind::Derive, helpers } | MacroBody::BuiltinDerive { helpers } => Some(helpers),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Expansion.

/// The outside of an expansion: the crate's cfg, no environment, no files; proc macros run by
/// the sources' runner ([`SourceTexts::run_proc_macro`]).
struct Host<'r, 'a, S> {
    cfg: &'r Cfg<'a, 'a>,
    /// The calling file's package-relative path (`src/lib.rs`).
    file: &'a str,
    /// What `file!()` answers, made from `file` on the first ask ([`CrateEnv::shown_file`],
    /// D96 (4)); `None` inside: the crate's environment does not name its package, refused.
    ///
    /// [`CrateEnv::shown_file`]: crate::crate_env::CrateEnv::shown_file
    shown_file: core::cell::OnceCell<Option<AString<'a>>>,
    /// The calling file's path (absolute): where `include!`'s path starts.
    path: &'a str,
    /// The package's folder: the one folder an `include!` may read in (no build script output
    /// is held here: `OUT_DIR` is no environment this host gives).
    package: &'a str,
    /// The calling crate's index.
    krate: usize,
    /// The calling file's text and the byte offset of the call's path in it (the topmost
    /// call's: a call inside an expansion is placed at the call that made it): where
    /// `line!()` and `column!()` in eager input are counted.
    text: &'a str,
    call_at: u32,
    /// The called macro's path as written at the call (`macros::sol`, `Abi`) and the call's
    /// shape: what a D111 finding of a proc macro's read names
    /// ([`LeftPackage::call`](crate::include_place::LeftPackage::call)).
    call_name: &'a str,
    call_shape: CallShape,
    /// The calling module's path (`krate::a::b`): what `module_path!()` answers; `None` for a
    /// module with no place (refused by name).
    module_path: Option<&'r str>,
    /// Whether the call is written in a body: its module is a block's or under one
    /// ([`crate::blocks::is_block_module`] on its chain). An `include!` there reads its file
    /// as DATA ([`ExpandCx::include`] below).
    in_body: bool,
    sources: &'r S,
    alloc: Alloc<'a>,
}

const REFUSED: HostError = HostError { ask: HostAsk::ProcMacro, why: HostWhy::Refused };

impl<'a, S: SourceTexts<'a>> ExpandCx for Host<'_, 'a, S> {
    /// The crate's environment ([`SourceTexts::crate_env`]), the one a body of the crate reads
    /// too; a source without one, or a variable cargo sets whose value is not known, is refused
    /// (rule 3), never answered unset.
    fn env(&self, name: &str) -> Result<Option<&str>, HostError> {
        let refused = HostError { ask: HostAsk::Env, why: HostWhy::Refused };
        self.sources.crate_env(self.krate).ok_or(refused)?.get(name).map_err(|_| refused)
    }

    /// A predicate the cfg does not read is false, as the lowering takes it.
    fn cfg(&self, predicate: &TokenStream<'_>) -> Result<bool, HostError> {
        // No `here` and no mark: a predicate writes no definition's `$crate`; a refusal would
        // still be one, never read as false.
        let text = render_with(predicate, None, |_| None, |_| false, &mut |_, _, _, _| {}, Option::<&mut AVec<'_, Option<u32>>>::None, self.alloc)
            .map_err(|_| HostError { ask: HostAsk::Cfg, why: HostWhy::Refused })?;
        Ok(self.cfg.eval(&text).unwrap_or(false))
    }

    /// The Reference ("Macros", built-in `include!`; core's `include_str!`): "The file is
    /// located relative to the current file (similarly to how modules are found)": the calling
    /// file's folder joined with the written path (`blaze_expand::include_target`), read from
    /// the crate's texts. These texts hold the package alone, so the bound here is the package
    /// (`SourceOrigin::Package`); a member's item-position read under its source root (D60)
    /// needs the root from `SourceTexts`, not given yet.
    ///
    /// The kind and the place decide the read: an item-position `include!`'s file outside any
    /// body is a module file ([`SourceTexts::read`]: its items join the crate, its surface is
    /// what the names read); `include_str!`'s and `include_bytes!`'s are data
    /// ([`SourceTexts::read_data`], named with the calling file): a read of the bytes, never a
    /// module (tempo's `crates/node/tests/assets/test-genesis.json`, 2026-11-16: read as a
    /// module, it stopped the check). An `include!` in a body ([`Host::in_body`]) is data too:
    /// in expression position its file is parsed as ONE expression (the Reference, "Macros",
    /// core `include!`: "the included file is parsed as an expression"), so what its block
    /// declares is a property of the file's whole text, no module surface; the calling file's
    /// outline lists it as a data part as it lists `include_str!`'s (blaze-cli
    /// data_include.rs), and the body's lowering reads it so too (blaze-ladder body_host.rs).
    /// A path leaving the package is `Outside` for every kind, refused by the expansion by name.
    fn include(&self, written: &str, how: IncludeAs) -> Result<Included<'_>, HostError> {
        // The one place the path is made: the modules the file writes are found from it too.
        let seen = self.sources.crate_env(self.krate).map(|env| env.seen());
        // D111: a member reads under the workspace root, a dependency under its package;
        // outside that, `Outside`, refused by the expansion by its path and the rule.
        let Some(place) = crate::include_place::include_place(self.package, seen, self.path, written, self.alloc) else {
            return Ok(Included::Outside);
        };
        let path = place.path;
        if place.outside_package {
            let call = match how {
                IncludeAs::Tokens => "include",
                IncludeAs::Str => "include_str",
                IncludeAs::Bytes => "include_bytes",
            };
            self.sources.left_package(self.krate, crate::include_place::LeftPackage { from: self.path, at: self.call_at, call, shape: CallShape::FnLike, path: &path });
        }
        let found = match how {
            IncludeAs::Tokens if !self.in_body => self.sources.read(self.krate, &path).map(str::as_bytes),
            IncludeAs::Tokens | IncludeAs::Str | IncludeAs::Bytes => self.sources.read_data(self.krate, self.path, &path),
        };
        Ok(found.map_or(Included::Missing, Included::Bytes))
    }

    /// Asked only from eager input (`concat!(file!(), ..)`: a plain call is a hole). One answer
    /// with the body host (front2 body_host.rs), D96 (5): `file!()` is the crate's shown path
    /// ([`CrateEnv::shown_file`](crate::crate_env::CrateEnv::shown_file)); `module_path!()` the
    /// calling module's path, as the body host gives it. core `macros/mod.rs`, `line!`: "the
    /// returned line is not necessarily the line of the line! invocation itself, but rather the
    /// first macro invocation leading up to the expansion"; `column!`: the same, "1-based ...
    /// counted in UTF-8 characters" from the line's start: so both are the call this host was
    /// made for (the topmost one), counted in its file's text. The answer is a read of the
    /// expansion, asked again before a kept one is reused (`Reads::hold`): the record holds
    /// no position, only what the call read. A refusal is the call's error, named.
    fn place(&self, what: PlaceOf) -> Result<HostPlace<'_>, HostError> {
        let refused = HostError { ask: HostAsk::Place, why: HostWhy::Refused };
        match what {
            PlaceOf::File => {
                let shown = self.shown_file.get_or_init(|| {
                    self.sources.crate_env(self.krate).and_then(|env| env.shown_file(self.file, self.alloc))
                });
                shown.as_ref().map(|text| HostPlace::Text(text.as_str())).ok_or(refused)
            }
            PlaceOf::ModulePath => self.module_path.map(HostPlace::Text).ok_or(refused),
            PlaceOf::Line | PlaceOf::Column => {
                let unreadable = HostError { ask: HostAsk::Place, why: HostWhy::Unreadable };
                let before = self.text.get(..self.call_at as usize).ok_or(unreadable)?;
                let line_start = before.rfind('\n').map_or(0, |at| at + 1);
                let number = match what {
                    PlaceOf::Line => before.bytes().filter(|b| *b == b'\n').count() + 1,
                    _ => before[line_start..].chars().count() + 1,
                };
                Ok(HostPlace::Number(u32::try_from(number).map_err(|_| unreadable)?))
            }
        }
    }

    /// The sources' runner, then decision D111 for a member's macro: each file its run read
    /// (a `file:<path from the served tree>` read, blaze-proc-macro sandbox.rs) outside the
    /// member's own dir ([`SeenRoot::left_package`]) is told to the sources
    /// ([`SourceTexts::left_package`]) as a finding at this call, named by the macro as
    /// written, the file by its path in the reader's space ([`served_tree`] joined with it,
    /// the form an include's finding has; the tree-relative path itself when the texts and the
    /// environment disagree on the member's folder). Derived from the read list, which a kept
    /// answer carries too (blaze-proc-macro's table, hook.rs `run`), so a kept answer gives the
    /// finding its run gave, byte for byte. A crate with no environment, or a dependency, has
    /// no such read: its sandbox serves its package alone and refuses a path leaving it by name.
    ///
    /// [`SeenRoot::left_package`]: blaze_expand::seen::SeenRoot::left_package
    /// [`served_tree`]: crate::include_place::served_tree
    fn run_proc_macro<'b>(&self, def: &MacroDefinition<'_>, call: ProcCall<'_>, alloc: Alloc<'b>) -> Result<Produced<'b>, HostError> {
        let produced = self.sources.run_proc_macro(self.krate, def, call, alloc)?;
        // D111 through the one filing every host uses (left_reads.rs; the body host's too).
        let at = crate::left_reads::CallAt { from: self.path, at: self.call_at, call: self.call_name, shape: self.call_shape };
        crate::left_reads::file_left_reads(self.sources, self.krate, at, &produced.reads, 0..produced.reads.list().len(), self.alloc);
        Ok(produced)
    }
}

/// A token stream as source text: a space between tokens unless the first is glued to the
/// next ([`blaze_expand::glued_in_text`]: joint, except an unsuffixed integer before a `.`,
/// which would lex back as a float); a `$crate` at token `at` is written `$crate` when it sits
/// inside a macro definition the expansion writes (`macro_rules! m { .. }`, `macro m { .. }`),
/// else `crate`, placed with the crate `dollar(at)` names; invisible groups dropped.
///
/// The Reference, "Macros By Example", "Hygiene": `$crate` names the crate that defines the
/// macro wherever the macro is called, so a definition keeps it: alloy-primitives'
/// `fixed_bytes_macros!` writes `address!`, whose body calls `$crate::hex!`; written as
/// `crate::hex!` a call from another crate would name the caller's `hex`.
///
/// token-mark-2 (drive.md "A token keeps its mark"): no spelled token. Outside a definition a
/// `$crate` is written as `crate` and placed with its crate (`placed`'s fourth value), whose row
/// carries [`crate::crate_roots::root_mark`]; inside one it is written `$crate`, which the written
/// definition reads as the crate expanding (`here`): another crate's is refused
/// ([`crate::crate_roots::ForeignDollarCrate`]).
///
/// A token `def_crate(at)` says begins a single-name call the body of a `local_inner_macros`
/// macro wrote ([`blaze_expand::PathScope::DefCrate`]) is rendered with the defining crate's
/// `$crate::` before it. The Reference, "Macros By Example", "Hygiene": "Additionally, the
/// `local_inner_macros` key [of `macro_export`] can be used to automatically prefix all
/// contained macro invocations with `$crate::`."
pub(crate) fn render_with<'a, W: core::alloc::Allocator>(
    stream: &TokenStream<'_>,
    here: Option<CrateItemKey>,
    dollar: impl Fn(usize) -> Option<CrateItemKey>,
    def_crate: impl Fn(usize) -> bool,
    // Told each written token's index and its bytes in the text (a `$crate` pair: the `$`'s
    // index; a `crate::` written before a token: part of it): the origin column's places.
    placed: &mut dyn FnMut(usize, usize, usize, Option<CrateItemKey>),
    // With `Some`, one entry pushed per stream token, in order: the index of the token's first
    // word in the text as a lexer reads it back (`None`: an invisible delimiter, which writes
    // none). The one place that knows how many words each token is written as
    // ([`crate::nested_input::InputTokens::written`]: a nested call's input indexes as written).
    mut words: Option<&mut blaze_own::Vec<Option<u32>, W>>,
    alloc: Alloc<'a>,
) -> Result<AString<'a>, crate::crate_roots::ForeignDollarCrate> {
    let tokens = stream.tokens();
    let mut out = AString::new_in(alloc);
    let mut prev: Option<TokenKind> = None;
    let mut at = 0;
    // The words written so far: `$crate` as `crate` is one, as `$crate` two (`$`, `crate`);
    // a `local_inner_macros` prefix is its crate's word(s) and `::`'s two.
    let mut word = 0u32;
    fn note_word<W: core::alloc::Allocator>(words: &mut Option<&mut blaze_own::Vec<Option<u32>, W>>, at: Option<u32>) {
        if let Some(words) = words {
            words.push(at);
        }
    }
    // Groups open now; the depth a definition's body opened at while inside one; how far the
    // tokens read so far are into `macro_rules ! name` (1, 2, 3) or `macro name` (2, 3).
    let mut depth = 0u32;
    let mut in_def: Option<u32> = None;
    let mut def_head = 0u8;
    // A `$crate` no expansion wrote (tokio's `doc! { macro_rules! try_join { .. $crate .. } }`:
    // in the call's own tokens) names the crate whose text holds it, the one expanding.
    // Returns the crate a written `crate` stands for (its row's root), `None` for none.
    let crate_of = |out: &mut AString<'_>, krate: Option<CrateItemKey>, in_def: bool| -> Result<Option<CrateItemKey>, crate::crate_roots::ForeignDollarCrate> {
        if in_def {
            return match krate {
                Some(k) if here.is_some_and(|h| h != k) => Err(crate::crate_roots::ForeignDollarCrate { krate: k }),
                _ => {
                    out.push_str("$crate");
                    Ok(None)
                }
            };
        }
        out.push_str("crate");
        Ok(krate)
    };
    /// A literal's body between `quote`s, `start` and `hashes` before, `suffix` after.
    fn quoted(out: &mut AString<'_>, start: &str, hashes: u8, quote: char, body: &str, suffix: &str) {
        out.push_str(start);
        for _ in 0..hashes {
            out.push('#');
        }
        out.push(quote);
        out.push_str(body);
        out.push(quote);
        for _ in 0..hashes {
            out.push('#');
        }
        out.push_str(suffix);
    }
    while at < tokens.len() {
        let token = &tokens[at];
        let text = stream.text_of(token);
        if matches!(token.kind, TokenKind::Open(Delim::Invisible) | TokenKind::Close(Delim::Invisible)) {
            note_word(&mut words, None);
            at += 1;
            continue;
        }
        if prev.is_some_and(|p| !blaze_expand::glued_in_text(p, token.kind)) {
            out.push(' ');
        }
        let (first, start) = (at, out.len());
        // The written `crate`'s crate, and where the row of the token itself starts (after a
        // `local_inner_macros` prefix, which is its own row).
        let mut root: Option<CrateItemKey> = None;
        let mut name_start = start;
        let named_def = def_head == 3;
        def_head = match (def_head, token.kind) {
            (_, TokenKind::Ident { raw: Raw::No, .. }) if text == "macro_rules" => 1,
            (_, TokenKind::Ident { raw: Raw::No, .. }) if text == "macro" => 2,
            (1, TokenKind::Punct { char: '!', .. }) => 2,
            (2, TokenKind::Ident { .. }) => 3,
            _ => 0,
        };
        match token.kind {
            TokenKind::Open(Delim::Paren | Delim::Brace | Delim::Bracket) => {
                if named_def && in_def.is_none() {
                    in_def = Some(depth);
                }
                depth += 1;
            }
            TokenKind::Close(Delim::Paren | Delim::Brace | Delim::Bracket) => {
                depth = depth.saturating_sub(1);
                if in_def == Some(depth) {
                    in_def = None;
                }
            }
            _ => {}
        }
        // A `$crate` is one word written as `crate`, two as `$crate` (`$`, `crate`).
        let crate_words = if in_def.is_some() { 2 } else { 1 };
        if def_crate(at) {
            let prefix = crate_of(&mut out, dollar(at), in_def.is_some())?;
            if prefix.is_some() {
                placed(first, start, out.len(), prefix);
            }
            out.push_str("::");
            if prefix.is_some() {
                name_start = out.len();
            }
            word += crate_words + 2;
        }
        note_word(&mut words, Some(word));
        let mut own = 1;
        match token.kind {
            TokenKind::Punct { char: '$', .. } if tokens.get(at + 1).is_some_and(|next| stream.text_of(next) == "crate") => {
                at += 1;
                // The pair's `crate`: the same word(s) as its `$`.
                note_word(&mut words, Some(word));
                own = crate_words;
                root = crate_of(&mut out, dollar(at), in_def.is_some())?;
            }
            TokenKind::Ident { .. } if text == "$crate" => {
                own = crate_words;
                root = crate_of(&mut out, dollar(at), in_def.is_some())?;
            }
            TokenKind::Ident { raw: Raw::Yes, .. } => {
                out.push_str("r#");
                out.push_str(text);
            }
            TokenKind::Ident { raw: Raw::No, .. } => out.push_str(text),
            TokenKind::Punct { char, .. } => out.push(char),
            TokenKind::Literal { kind, suffix_len, .. } => {
                let split = text.len().saturating_sub(suffix_len as usize);
                let (body, suffix) = text.split_at(split);
                match kind {
                    LitKind::Str => quoted(&mut out, "", 0, '"', body, suffix),
                    LitKind::StrRaw(n) => quoted(&mut out, "r", n, '"', body, suffix),
                    LitKind::ByteStr => quoted(&mut out, "b", 0, '"', body, suffix),
                    LitKind::ByteStrRaw(n) => quoted(&mut out, "br", n, '"', body, suffix),
                    LitKind::CStr => quoted(&mut out, "c", 0, '"', body, suffix),
                    LitKind::CStrRaw(n) => quoted(&mut out, "cr", n, '"', body, suffix),
                    LitKind::Char => quoted(&mut out, "", 0, '\'', body, suffix),
                    LitKind::Byte => quoted(&mut out, "b", 0, '\'', body, suffix),
                    LitKind::Integer | LitKind::Float | LitKind::Err => out.push_str(text),
                }
            }
            TokenKind::Open(Delim::Paren) => out.push('('),
            TokenKind::Open(Delim::Brace) => out.push('{'),
            TokenKind::Open(Delim::Bracket) => out.push('['),
            TokenKind::Close(Delim::Paren) => out.push(')'),
            TokenKind::Close(Delim::Brace) => out.push('}'),
            TokenKind::Close(Delim::Bracket) => out.push(']'),
            TokenKind::Open(Delim::Invisible) | TokenKind::Close(Delim::Invisible) => {}
        }
        placed(first, name_start, out.len(), root);
        word += own;
        prev = Some(token.kind);
        at += 1;
    }
    Ok(out)
}

/// Expands one call the fill asked for: its declarations and text, or `None` (noted) when it
/// adds nothing.
///
/// The call's working memory (its input, the matcher's expansion, the rendering, the origin
/// column, the kept entry's bytes) is held where `scratch` says ([`crate::task::Par::
/// call_scratch`]): in a scratch the Par resets once [`expand_call`] returns, or in the
/// crate's allocator. What outlives the call [`expand_call`] writes in the crate's allocator
/// itself (`base.alloc`), each piece at its exact size: so the frame's peak is what is kept
/// plus one call's scratch, and the two answers write equal bytes.
#[allow(clippy::too_many_arguments, reason = "the call's context, each named")]
fn expand_decls<'a, S: SourceTexts<'a>>(
    base: &Base<'_, 'a, S>,
    run: &Run<'a>,
    request: ExpandRequest,
    part: PartMark<'_>,
    seen: &dyn Fn(ItemKey, &str),
    scratch: CallScratch<'_>,
    refused_env: &crate::refused::EnvRefusal<'a>,
    last: &core::cell::Cell<Option<ExpandNote<'a>>>,
) -> Option<(DeclsBuilder<'a, &'a str>, Option<&'a ExpansionText<'a>>)> {
    let mut out = None;
    let mut work = |s: Alloc<'_>| out = expand_call(base, run, request, part, seen, refused_env, last, s);
    if scratch(&mut work) == crate::task::Scratch::Kept {
        work(base.alloc);
    }
    out
}

/// [`expand_decls`]'s one call, its work in `alloc` (a scratch or the crate's allocator), what
/// it keeps in `base.alloc` (`home`).
#[allow(clippy::too_many_arguments, reason = "the call's context, each named")]
fn expand_call<'a, S: SourceTexts<'a>>(
    base: &Base<'_, 'a, S>,
    run: &Run<'a>,
    request: ExpandRequest,
    part: PartMark<'_>,
    seen: &dyn Fn(ItemKey, &str),
    refused_env: &crate::refused::EnvRefusal<'a>,
    last: &core::cell::Cell<Option<ExpandNote<'a>>>,
    alloc: Alloc<'_>,
) -> Option<(DeclsBuilder<'a, &'a str>, Option<&'a ExpansionText<'a>>)> {
    // `home` keeps what outlives the call; `alloc` is the call's work.
    let home = base.alloc;
    let home_bump = base.bump;
    let call = request.call;
    let src = base.tables.calls.get(&call).copied().or_else(|| held(run.tables.read(), "tables").calls.get(&call).copied());
    let Some(src) = src else {
        last.set(Some(ExpandNote::UnknownCall));
        run.note(NamesNote::Expand { call, at: None, note: ExpandNote::UnknownCall });
        return None;
    };
    // Every note of the call is kept as its last: a return of nothing follows the note that
    // says why (`FillCx::expand` keeps it as the refusal's reason).
    let note = |note: ExpandNote<'a>| {
        last.set(Some(note));
        run.note(NamesNote::Expand { call, at: Some(src.loc), note });
    };
    // A derived item's keep call: no macro, its expansion the item itself (attr-bang-kind-2).
    if request.def == crate::cells::KEEP_ITEM {
        return keep_item(base, run, call, src, part, KeepAs::Item, &note);
    }
    // A proc-macro crate's harness: no macro, its expansion the `_DECLS` static (D175).
    if request.def == crate::cells::PROC_MACRO_HARNESS {
        return keep_item(base, run, call, src, part, KeepAs::Harness, &note);
    }
    let def_key = request.def.key;
    let mac = base.tables.macros.get(&def_key).copied().or_else(|| held(run.tables.read(), "tables").macros.get(&def_key).copied());
    // A macro of another crate: its definition there, its `$crate` and edition that crate's.
    let (mac, def_krate, def_edition, runner) = match mac {
        Some(mac) => (Some(mac), base.key, base.edition, base.sources.proc_macro_key(base.index)),
        None => match base.built.of(def_key) {
            Some(other) => {
                let mac = other.macro_of(def_key);
                base.built.ask(|| crate::asked::Question::macro_src(def_key, mac, home));
                (mac, other.key(), other.edition(), other.proc_key())
            }
            None => (None, base.key, base.edition, None),
        },
    };
    let Some(mac) = mac else {
        note(ExpandNote::NoDefinition { shape: src.kind, name: src.name });
        return None;
    };
    // A function-like call a block's module holds ([`crate::blocks`]: at a statement's start or
    // in an expression, `let v = vec![1]`) is expanded here only for the items its expansion may
    // declare in that block (the Reference, "Statements", "Item declarations"). An expansion is
    // its definition's transcribed text with fragments of the call's input: when neither may
    // write an item (blaze_file_items::nested::may_declare, a superset; the definition's answer
    // read once by its key, `Prepares::may_declare`), it declares nothing, and its blocks hold
    // no item or call, so no module of it is named by the body (front2 scope_table.rs gives
    // such a block no ordinal). The body's lowering expands it alone, as before it was listed.
    // A builtin writes no item but `include!`'s file and `global_asm!`; a proc macro's output
    // is not known before it runs, so a proc macro is always expanded.
    if src.kind == CallShape::FnLike
        && held(run.made.read(), "made modules").get(&src.module).is_some_and(|m| m.info.name.is_some_and(crate::blocks::is_block_module))
    {
        // A builtin whose output holds no token of its input is a literal, a path to `Option`
        // or nothing (core `macros/mod.rs`: `include_str!` "expands to an expression of type
        // `&'static str`", `concat!` "yields an expression of type `&'static str`", `env!`,
        // `cfg!` "evaluates to true or false", ..): it declares nothing whatever its input
        // writes, so its input is not asked either. The names library never expands one in a
        // block: its eager input (`include_str!(concat!(env!("CARGO_MANIFEST_DIR"), ..))`,
        // tempo zones/l1 tests.rs) would read a data file no outline lists (blaze-cli
        // data_include.rs lists literal paths), and the body's lowering reads it instead.
        if matches!(mac.body, MacroBody::BuiltinFn(name) if LITERAL_BUILTINS.iter().any(|b| *b == name)) {
            return Some((DeclsBuilder::new_in(home), None));
        }
        // The definition's TEXT alone answers, every call in it taken to write an item: a
        // call skipped here must leave no block of its expansion written (an item or a call in
        // it), because the body numbers its expansion's written blocks and reads each as the
        // module this step files (front2 scope_table.rs `build`). Reading a dependency's
        // `$crate` callees instead (alloc's `format!` writes no item) skipped `format!` and
        // `assert_eq!`, whose expansions hold `format_args!` in a block: 161 tempo bodies then
        // asked for a block module never filed (tests/blocks.rs
        // `a_dependency_s_call_whose_expansion_writes_a_block_is_expanded`). D189 moves the
        // `$crate` walk to the surface pass (macro_effect.rs), where no block module exists.
        let def_may = match mac.body {
            MacroBody::Rules(rules) => Some(run.prepares.may_declare(def_key, &[rules])),
            MacroBody::Macro2 { params, body } => Some(run.prepares.may_declare(def_key, &[params.unwrap_or(""), body])),
            MacroBody::BuiltinFn(name) => Some(matches!(name, "include" | "global_asm")),
            MacroBody::BuiltinDerive { .. } | MacroBody::BuiltinOther(_) | MacroBody::Proc { .. } => None,
        };
        if def_may == Some(false) && !blaze_file_items::nested::may_declare(src.input) {
            return Some((DeclsBuilder::new_in(home), None));
        }
    }
    let edition = base.edition;
    let lexed = |text: &str, what: Lexed| match lex(text, edition, alloc) {
        Ok(stream) => Some(stream),
        Err(error) => {
            note(ExpandNote::Lex { macro_name: mac.name, what, error });
            None
        }
    };
    // A `macro_rules!` or `macro` is lexed and compiled once, by its defining item's key; a
    // builtin or a proc macro has nothing to compile and is built for its call's shape.
    let declarative = matches!(mac.body, MacroBody::Rules(_) | MacroBody::Macro2 { .. });
    seen(def_key, src.input);
    let site = CallSite { edition, panic_edition: edition, ctx: Ctx::root(edition) };
    let file = held(base_files(run), "files").at(src.loc.file as usize);
    // The file the call is written in ([`CallSrc::written`]): where its `include!` reads from.
    let calling: &'a str = src.written;
    // The calling module's path, for `module_path!()`: a walked module's, or one a macro made.
    let module_path: Option<&str> = match base.tree.by_key.get(&src.module) {
        Some(&i) => Some(base.tree.modules[i].path.as_str()),
        None => held(run.made.read(), "made modules").get(&src.module).map(|it| it.path),
    };
    // A body's modules are made ones (crate::blocks): the call is in a body when its module or
    // one around it is a block's. A walked module is in no body.
    let in_body = {
        let made = held(run.made.read(), "made modules");
        made.get(&src.module).is_some_and(|it| {
            it.chain.iter().any(|up| made.get(up).is_some_and(|m| m.info.name.is_some_and(crate::blocks::is_block_module)))
        })
    };
    let host = || Host {
        cfg: base.cfg,
        file: shown(file.path, base.shown_from),
        shown_file: core::cell::OnceCell::new(),
        path: calling,
        package: base.shown_from,
        krate: base.index,
        text: file.text,
        call_at: src.loc.offset,
        call_name: src.name,
        call_shape: src.kind,
        module_path,
        in_body,
        sources: base.sources,
        // A host's answers (`file!()`'s text, a D111 finding) are kept with the crate.
        alloc: home,
    };
    let cfg_atom = |name: &str, value: Option<&str>| base.cfg.atoms().binary_search(&(name, value)).is_ok();
    // The crate's kept expansions ([`crate::expansions`]) are keyed by the ONE key of an
    // expansion, [`blaze_expand::call_key`]: the definition's content and site and the call's
    // tokens as shaped, made below once the call is (the body's lowering keys its calls by the
    // same function, so one call is one key in both layers). A function-like builtin has no
    // key: it is expanded each solve.
    let kept = base.kept;
    if kept.keeps() {
        part(ExpandPart::Kept, mac.name, src.input.len());
    }
    let keyed = kept.keeps()
        && match mac.body {
            MacroBody::Rules(_) | MacroBody::Macro2 { .. } => true,
            MacroBody::BuiltinDerive { .. } => src.kind == CallShape::Derive,
            MacroBody::Proc { .. } => runner.is_some(),
            MacroBody::BuiltinFn(_) | MacroBody::BuiltinOther(_) => false,
        };
    part(ExpandPart::Define, mac.name, 0);
    let written;
    let def: Kept<'_> = match run.prepares.get(def_key).filter(|_| declarative) {
        Some(kept) => kept,
        // In `home`: a declarative definition is kept for every call of it (`prepares`), and
        // its refusal is a note; a builtin's or a proc macro's is a few words.
        None => match definition_of(mac, def_krate, runner, def_edition, src.kind, home) {
            Ok(def) if declarative => run.prepares.keep(def_key, def),
            Ok(def) => {
                written = def;
                Kept::Written(&written)
            }
            // A proc macro with no runner key has no definition: said as its crate's module
            // that no runner knows, not as a macro this crate's solve did not read.
            Err(ExpandNote::NoDefinition { .. }) if matches!(mac.body, MacroBody::Proc { .. }) && runner.is_none() => {
                note(ExpandNote::ProcModule { macro_name: mac.name, why: ProcUnusable::NoRunner });
                return None;
            }
            Err(why) => {
                note(why);
                return None;
            }
        },
    };
    part(ExpandPart::LexInput, mac.name, src.input.len());
    // A function-like call's input is the stream inside its delimiters, its last token joint
    // with the closer when it touches it: the form the body's lowering copies out of the body
    // (blaze_expand::lex_inside), so one call is one key in both layers. An item macro's input
    // is the item, lexed as text of its own. A call written in an expansion takes its input as
    // that expansion's sealed tokens (crate::nested_input): each keeps its context, so a
    // `$crate` or a definition-site name passed down keeps its mark, and the key holds them.
    let input = match src.tokens {
        crate::nested_input::CallTokens::Expansion(tokens) => match tokens.copy_in(alloc) {
            Ok(stream) => stream,
            Err(error) => {
                note(ExpandNote::Expand { macro_name: mac.name, error: ExpandError::Stream { stream: None, error } });
                return None;
            }
        },
        crate::nested_input::CallTokens::Refused(error) => {
            note(ExpandNote::NestedInput { macro_name: mac.name, error });
            return None;
        }
        crate::nested_input::CallTokens::Text => match src.kind {
            CallShape::FnLike => match blaze_expand::lex_inside(src.input, edition, alloc) {
                Ok(stream) => stream,
                Err(error) => {
                    note(ExpandNote::Lex { macro_name: mac.name, what: Lexed::Input, error });
                    return None;
                }
            },
            CallShape::Attr | CallShape::Derive => lexed(src.input, Lexed::Input)?,
        },
    };
    part(ExpandPart::Shape, mac.name, 0);
    let input_tokens = input.len();
    // An item macro's input as its shape takes it: the item without the attribute (and the
    // attribute's arguments), or the item as a derive sees it. Each cfg atom the walk asks is
    // a read the call's key does not hold: kept with its answer.
    let mut asked = blaze_expand::kept::Asked::new_in(alloc);
    let mut asked_fits = true;
    let atom = &mut |name: &str, value: Option<&str>| {
        let answer = cfg_atom(name, value);
        if keyed && asked.push(name, value, answer).is_err() {
            asked_fits = false;
        }
        Ok::<bool, Infallible>(answer)
    };
    let item_note = |error: ItemAttrsError<Infallible>| note(ExpandNote::ItemAttrs { macro_name: mac.name, error });
    let (args, item): (Option<TokenStream<'_>>, TokenStream<'_>);
    let derive_facts;
    let call_of = match src.kind {
        CallShape::FnLike => Call::fn_like(&input, site),
        CallShape::Attr if src.implicit => {
            // D104: no attribute names the call; it stands outermost, so the item is all of
            // the input, each `#[cfg_attr]` applied (blaze-expand `ItemAttrs::implicit_call`).
            let made = match ItemAttrs::new(input).implicit_call(atom) {
                Ok(made) => made,
                Err(error) => {
                    item_note(error);
                    return None;
                }
            };
            (args, item) = (made.args, made.item);
            Call::attr(args.as_ref(), &item, site)
        }
        CallShape::Attr => {
            // The attribute the lowering read is found by its path's words (blaze-expand
            // `same_path`), never by its text: an item an expansion wrote holds `$crate` where
            // the lowering's rendered text says `crate`, and spaces every token (tempo's
            // `#[$crate::linkme::distributed_slice(..)]` statics, 2026-11-17).
            let plant = base.sources.attr_plant();
            let named = lexed(src.name, Lexed::Path)?;
            let mut walk = ItemAttrs::new(input);
            let attr = loop {
                match walk.next(atom) {
                    Ok(NextAttr::Call { path, attr })
                        if match plant {
                            AttrPlant::None => walk.attr_is(attr.start..attr.end, &named),
                            AttrPlant::Spelling => same_spelling(&path, src.name),
                        } =>
                    {
                        break attr;
                    }
                    Ok(NextAttr::Call { .. }) => {}
                    // The lowering kept the item, so its cfg was true: no attribute left is
                    // the lowering and this walk disagreeing.
                    Ok(NextAttr::Removed | NextAttr::End) => {
                        note(ExpandNote::AttrNotFound { name: src.name });
                        return None;
                    }
                    Err(error) => {
                        item_note(error);
                        return None;
                    }
                }
            };
            let made = match walk.attr_call(attr) {
                Ok(made) => made,
                Err(error) => {
                    item_note(ItemAttrsError::Stream(error));
                    return None;
                }
            };
            (args, item) = (made.args, made.item);
            Call::attr(args.as_ref(), &item, site)
        }
        CallShape::Derive => {
            let derives = match ItemAttrs::new(input).derives(atom) {
                Ok(Some(derives)) => derives,
                // The item's cfg is false: nothing of it is derived.
                Ok(None) => return Some((DeclsBuilder::new_in(home), None)),
                Err(error) => {
                    item_note(error);
                    return None;
                }
            };
            // Found by the path's words, as an attribute call is (the arm above).
            let plant = base.sources.attr_plant();
            let named = lexed(src.name, Lexed::Path)?;
            let Some(used) = derives.uses.iter().find(|u| match plant {
                AttrPlant::None => blaze_expand::same_path(&u.tokens, 0..u.tokens.len(), &named, 0..named.len()),
                AttrPlant::Spelling => same_spelling(&u.path, src.name),
            }) else {
                note(ExpandNote::AttrNotFound { name: src.name });
                return None;
            };
            derive_facts = derives.facts(used);
            item = derives.input;
            Call::derive(&item, derive_facts, site)
        }
    };
    // The call's one key, before the work: the definition's identity and the call as shaped
    // (an attribute's item without it, a derive's item as it sees it: the cfg answers the walk
    // took are in those tokens, and in the entry's asked list too).
    // The key the expansion's own marks are sealed with before a call written in it takes its
    // tokens (as the body layer seals them, blaze-lower `by_name.rs`): every call has it, kept
    // or not, so a mark names the expansion it came from.
    let own_key = blaze_expand::call_key(def.def(), &call_of);
    let call_key = keyed.then(|| match kept.planted() {
        crate::expansions::Planted::None => own_key,
        crate::expansions::Planted::KeyWithoutTheDefinition => {
            blaze_expand::call_key_of(blaze_expand::MacroContent(blaze_id::ContentId([0; 32])), def.def().site(), &call_of)
        }
    });
    // A kept expansion is taken when its read list is answered today as it was: each cfg atom
    // the item walk asked, then each read the expansion made.
    let mut found: Option<blaze_expand::Expansion<'_>> = None;
    if let Some(key) = call_key {
        let entry = kept.find(&key).unwrap_or_else(|e| panic!("the kept expansion of a call of `{}` did not open: {e}", mac.name));
        if let Some((index, entry)) = entry {
            let mut taken = None;
            if entry.asked().all(|asked| cfg_atom(asked.name, asked.value) == asked.answer) {
                let expansion = entry.expansion(alloc).unwrap_or_else(|e| panic!("the kept expansion of a call of `{}` did not read: {e}", mac.name));
                let held = match base.script {
                    _ if expansion.reads.list().is_empty() => Ok(blaze_expand::Held::Holds),
                    Some(out) => expansion.reads.hold(&ScriptCx { inner: host(), out }, edition, alloc),
                    None => expansion.reads.hold(&host(), edition, alloc),
                };
                // A read that cannot be asked today is no answer: the call is expanded, and
                // says so itself.
                if held == Ok(blaze_expand::Held::Holds) {
                    taken = Some(expansion);
                }
            }
            match taken {
                Some(expansion) => {
                    kept.found(key, index);
                    found = Some(expansion);
                }
                None => kept.moved(),
            }
        }
    }
    let expansion = match found {
    Some(expansion) => expansion,
    None => {
    // A nested call of an eager builtin names a library builtin unless this crate defines a
    // macro of that name (then the name may be the crate's: refused, never guessed); the
    // library's own `#[rustc_builtin_macro]` definition (core's) is that builtin.
    let shadowed = |name: &str| base.tables.macros.values().any(|m| m.name == name && !matches!(m.body, MacroBody::BuiltinFn(b) if b == name));
    let nested_at = DefAt { site: DefSite { krate: def_krate, module: None }, edition: def_edition, export: Export::Exported, local_inner: LocalInner::No, transparency: None };
    let eager = EagerDriver { macro_name: mac.name, def_at: nested_at, shadowed: &shadowed, keep: home_bump, alloc };
    let run_part = match mac.body {
        MacroBody::Rules(_) | MacroBody::Macro2 { .. } => ExpandPart::RunRules,
        MacroBody::BuiltinFn(_) => ExpandPart::RunBuiltin,
        MacroBody::BuiltinDerive { .. } | MacroBody::BuiltinOther(_) => ExpandPart::RunDerive,
        MacroBody::Proc { .. } => ExpandPart::RunProc,
    };
    part(run_part, mac.name, input_tokens);
    let expanded = match base.script {
        // Each refused `env!` read is noted: the cause a refusal names (`crate::refused`).
        Some(out) => eager.expand(def, &call_of, &crate::refused::Watched { inner: &ScriptCx { inner: host(), out }, env: refused_env }),
        None => eager.expand(def, &call_of, &crate::refused::Watched { inner: &host(), env: refused_env }),
    };
    let expansion = match expanded {
        Ok(expansion) => expansion,
        // A proc macro's run the host refused: the runner says why its module cannot run it
        // (the crate, and the module key asked or the load's error), or why its run stopped
        // (the path its sandbox refused, as the macro named it).
        Err(why) => {
            let module = match why {
                ExpandNote::Expand { error: ExpandError::Host(HostError { ask: HostAsk::ProcMacro, .. }), .. } => base.sources.proc_macro_unusable(base.index, def.def()),
                _ => None,
            };
            match module {
                Some(why) => note(ExpandNote::ProcModule { macro_name: mac.name, why }),
                None => note(why),
            }
            return None;
        }
    };
    // Kept for the crate's next solve, unless a read of it cannot be asked again.
    match call_key {
        Some(key) => {
            part(ExpandPart::Keep, mac.name, expansion.tokens.len());
            let entry = match expansion.reads.askable() {
                // The entry's token record counts origins in record tokens of the call's sources.
                Ok(()) if asked_fits => {
                    let sources = blaze_expand::SourceIndexes::of(def.def(), &call_of, alloc);
                    blaze_expand::kept::write_entry(&asked, &expansion, &sources.maps(), alloc).ok()
                }
                _ => None,
            };
            // Written in the call's work, kept at its exact size: an entry grows as it is
            // written, and its growth stays behind in the scratch, never in `home`.
            match entry {
                Some(entry) => kept.made(key, home_bump.alloc_slice_copy(&entry[..])),
                None => kept.unkept(),
            }
        }
        None if kept.keeps() => kept.unkept(),
        None => {}
    }
    expansion
    }
    };
    // A call whose expansion holds an error FAILED (the Reference, "Macros By Example": if no
    // rule matches, "an error is reported"): it writes nothing, and what it might have written
    // is unknown because of it, never absent (rules 19 and 54). Returning `None` stores it
    // refused (`FillCx::expand`, `crate::refused`); the note keeps the matcher's own reason.
    if let Some(failed) = blaze_expand::Failed::of(&expansion, home_bump) {
        note(ExpandNote::Failed { path: src.name, implicit: src.implicit, failed });
        return None;
    }
    for diagnostic in expansion.diagnostics.iter() {
        note(ExpandNote::Diagnostic { macro_name: mac.name, diagnostic: *diagnostic });
    }
    let here = base.key;
    let mut def_crate_starts: AVec<'_, u32> = AVec::new_in(alloc);
    def_crate_starts.extend(expansion.nested.iter().filter(|c| c.scope == PathScope::DefCrate).map(|c| c.path.start));
    def_crate_starts.sort_unstable();
    let def_crate = |at: usize| def_crate_starts.binary_search(&(at as u32)).is_ok();
    // The column's input indexes are the call's text as written (drive.md, ruled 20:57): a
    // fn-like call's input is that text; an item macro's is the item its walk made, matched to
    // the written item from its end (blaze-expand `align_suffix`). Lexed as `lexed` lexed it.
    // A call written in an expansion: its input's tokens indexed as written are the words the
    // outer renderer placed them at (crate::nested_input::InputTokens::written), never the
    // text lexed again.
    let written_input = match (call_of.kind, src.tokens) {
        (CallKind::FnLike { .. }, crate::nested_input::CallTokens::Expansion(tokens)) => Some(tokens.written(alloc)),
        (CallKind::FnLike { .. }, _) => None,
        (CallKind::Attr { item, .. } | CallKind::Derive { item, .. }, crate::nested_input::CallTokens::Expansion(tokens)) => {
            let whole = match tokens.copy_in(alloc) {
                Ok(whole) => whole,
                Err(error) => {
                    note(ExpandNote::Expand { macro_name: mac.name, error: ExpandError::Stream { stream: None, error } });
                    return None;
                }
            };
            let words = tokens.written(alloc);
            let aligned = blaze_expand::origin_column::align_suffix(item, &whole, alloc);
            let mut map = AVec::with_capacity_in(aligned.len(), alloc);
            map.extend(aligned.iter().map(|at| at.and_then(|at| words.get(at as usize).copied().flatten())));
            Some(map)
        }
        (CallKind::Attr { item, .. } | CallKind::Derive { item, .. }, _) => {
            let written = lexed(src.input, Lexed::Input)?;
            Some(blaze_expand::origin_column::align_suffix(item, &written, alloc))
        }
    };
    part(ExpandPart::Render, mac.name, expansion.tokens.len());
    // Each placed piece as the nested input keeps it, `(start, end, token)`, made once at its
    // final size (at most one a token), and the `crate` written for another crate's `$crate`
    // beside it, sparse, by its place's index: no wide row per token is held or grown (the
    // frame's largest stretch in tempo's core, `scope frame by stretch`).
    // `placed` and `words` are kept (the nested input reads them), sized once in `home`; the
    // rendering, `roots` and the column grow in the call's work.
    let mut placed: BVec<'a, (u32, u32, u32)> = BVec::with_capacity_in(expansion.tokens.len(), home_bump);
    let mut roots: AVec<'_, (u32, CrateItemKey)> = AVec::new_in(alloc);
    let mut too_large = false;
    let mut words: BVec<'a, Option<u32>> = BVec::with_capacity_in(expansion.tokens.len(), home_bump);
    let rendered = render_with(
        &expansion.tokens,
        Some(here),
        |at| expansion.hygiene(at).mark.map(|mark| mark.site.krate),
        def_crate,
        &mut |token, start, end, root| match (u32::try_from(token), u32::try_from(start), u32::try_from(end), u32::try_from(placed.len())) {
            (Ok(token), Ok(start), Ok(end), Ok(place)) => {
                if let Some(root) = root {
                    roots.push((place, root));
                }
                placed.push((start, end, token));
            }
            _ => too_large = true,
        },
        Some(&mut words),
        alloc,
    );
    let rendered = match rendered {
        Ok(rendered) => rendered,
        Err(foreign) => {
            note(ExpandNote::ForeignDollarCrate { macro_name: mac.name, krate: foreign.krate });
            return None;
        }
    };
    if too_large {
        note(ExpandNote::NestedInput { macro_name: mac.name, error: crate::nested_input::NestedInputError::TooLarge });
        return None;
    }
    // How many `crate`s the text writes for another crate's `$crate`: such a text is read only
    // with its marks (crate::crate_roots).
    let roots_written = roots.iter().filter(|(_, k)| *k != here).count();
    // Rendered in the call's work (the text grows as it is written), kept at its exact size.
    let text: &'a str = keep(rendered.as_str(), home_bump);
    let input_index = match written_input.as_deref() {
        Some(map) => blaze_expand::origin_column::InputIndex::Aligned(map),
        None => blaze_expand::origin_column::InputIndex::AsWritten,
    };
    let shape = match src.kind {
        CallShape::FnLike => blaze_expand::origin_column::InputShape::Inside,
        CallShape::Attr | CallShape::Derive => blaze_expand::origin_column::InputShape::Whole,
    };
    let mark_of = |at: usize| expansion.hygiene(at).mark.map(blaze_expand::origin_column::TextMark::of);
    // The column and its bytes are made in the call's work; the bytes are kept at their size.
    let origin_column = crate::expansion_origins::column_of(call, &placed, &roots, &expansion.origins, input_index, &mark_of, alloc);
    let origins: Result<&'a [u8], crate::expansion_origins::OriginsError> = origin_column
        .and_then(|column| crate::expansion_origins::write_bytes(call, src.place, shape, src.name, src.implicit, &column, alloc))
        .map(|bytes| &*home_bump.alloc_slice_copy(&bytes));
    // What a call written in this text takes its input from (crate::nested_input): the
    // expansion's tokens sealed with its key, each placed piece's bytes, each token's word. The
    // stream was made in the call's work: it is kept as one exact copy in `home` (each table
    // sized once, `TokenStream::clone_in`), sealed there.
    let expansion_tokens = {
        let mut sealed = expansion.tokens.clone_in(home);
        sealed.seal(own_key.0);
        let sealed: &'a TokenStream<'a> = home_bump.alloc_val(sealed);
        let placed: &'a [(u32, u32, u32)] = placed.into_bump_slice();
        let words: &'a [Option<u32>] = words.into_bump_slice();
        crate::nested_input::ExpansionTokens { stream: sealed, text, places: placed, words }
    };
    part(ExpandPart::Outline, mac.name, text.len());
    let made: &'a ExpansionText<'a> = home_bump.alloc_val(ExpansionText::new(text, outline(text, edition, home), origins));
    part(ExpandPart::Lower, mac.name, text.len());
    // The marks a lowered path's first segment resolves by (token-mark-2): the text's column,
    // opened once in `ExpansionText::new`.
    let column = match made.column {
        Ok(column) => Some(column),
        Err(lost) if roots_written > 0 => {
            note(ExpandNote::RootsLost { macro_name: mac.name, lost });
            return None;
        }
        // No root of another crate: every `crate` in it is this crate's (or a `macro` item's
        // own, read as before this piece).
        Err(_) => None,
    };
    // With the tokens that place the text lowered: the rendered text's own, or, for statements
    // read as items, the same tokens moved with the slices that text copied
    // ([`crate::nested_input::ExpansionTokens::moved`]), so a call there takes its input as the
    // expansion's tokens, never as its text lexed again.
    #[allow(clippy::type_complexity)]
    let (text, made, column, fragment_tokens): (&'a str, &'a ExpansionText<'a>, Option<blaze_expand::origin_column::OriginsRef<'a>>, crate::nested_input::ExpansionTokens<'a>) = match made.outline.errors().first() {
        None => (text, made, column, expansion_tokens),
        Some(error) => {
            // A call a block's module holds ([`crate::blocks`]) stands in STATEMENT position:
            // the Reference, "Macro invocation", "Statements": its expansion is statements of
            // the calling block. One that is an expression or a `let` (`assert!(..)`,
            // `println!(..)`) declares nothing: nothing is lowered from it. One whose
            // statements hold items or statement calls beside what is not an item, or that is
            // a block expression declaring items (`tokio::select!`), is read as statements
            // (crate::expanded_blocks `statements_as_items`): its top-level items and calls,
            // and a module per block inside it that holds one, are the call's fragment, lowered
            // below like any expansion's items (one key, the call's; one record, its
            // expansion). An expansion nested in it is a call of that fragment: the same step
            // again, driven by the solve, `src.depth + 1` deep.
            let in_block = held(run.made.read(), "made modules")
                .get(&src.module)
                .is_some_and(|m| m.info.name.is_some_and(crate::blocks::is_block_module));
            if !in_block {
                note(ExpandNote::NotItems { macro_name: mac.name, message: made.outline.message(error) });
                (text, made, column, expansion_tokens)
            } else {
                match crate::expanded_blocks::statements_as_items(text, call, alloc) {
                    Ok(None) => return Some((DeclsBuilder::new_in(home), None)),
                    Ok(Some((items, copies))) => {
                        let items: &'a str = keep(items.as_str(), home_bump);
                        // The column moved with the slices the items text copied, written with
                        // the call's own part (its call, place, shape and macro, as the
                        // rendered text's): an item of the items text walks from its place to
                        // the call that wrote it (blaze-cli body_types/covered.rs
                        // `written_path`), as an item of any expansion does. The rows are those
                        // `remapped` carried before (one remap, read back in place).
                        let origins: Result<&'a [u8], crate::expansion_origins::OriginsError> = match made.column {
                            Ok(column) => match crate::crate_roots::remap(column, &copies, alloc) {
                                Ok(moved) => crate::expansion_origins::write_bytes(call, src.place, shape, src.name, src.implicit, &moved, alloc)
                                    .map(|bytes| &*home_bump.alloc_slice_copy(&bytes)),
                                Err(error) if roots_written > 0 => {
                                    note(ExpandNote::RootsLost { macro_name: mac.name, lost: crate::crate_roots::RootsLost::Column { error } });
                                    return None;
                                }
                                Err(_) => Err(crate::expansion_origins::OriginsError::Rewritten { what: "a call's statements read as items" }),
                            },
                            Err(_) => Err(crate::expansion_origins::OriginsError::Rewritten { what: "a call's statements read as items" }),
                        };
                        let again: &'a ExpansionText<'a> = home_bump.alloc_val(ExpansionText::new(items, outline(items, edition, home), origins));
                        // The marks a lowered path reads: the moved column, as before; none
                        // when the rendered text had none (refused above when it wrote
                        // another crate's root).
                        let column = match (column, again.column) {
                            (Some(_), Ok(moved)) => Some(moved),
                            (Some(_), Err(lost)) if roots_written > 0 => {
                                note(ExpandNote::RootsLost { macro_name: mac.name, lost });
                                return None;
                            }
                            _ => None,
                        };
                        if let Some(error) = again.outline.errors().first() {
                            note(ExpandNote::NotItems { macro_name: mac.name, message: again.outline.message(error) });
                            return Some((DeclsBuilder::new_in(home), None));
                        }
                        (items, again, column, expansion_tokens.moved(items, &copies, home_bump))
                    }
                    Err(refused) => {
                        note(ExpandNote::NotItems { macro_name: mac.name, message: refused.message() });
                        return Some((DeclsBuilder::new_in(home), None));
                    }
                }
            }
        }
    };
    // An `include!`'s file is the current file of what it writes ([`crate::included`]): the
    // calls in it read from it, and a `mod x;` at its top is found from its folder. A file
    // outside the package (a build script's output) has no such path: the calling file stays.
    let included: Option<&'a str> = match mac.body {
        MacroBody::BuiltinFn("include") => {
            let seen = base.sources.crate_env(base.index).map(|env| env.seen());
            crate::included::included_read(base.shown_from, seen, calling, &expansion.reads, alloc).map(|path| keep(&path, home_bump))
        }
        _ => None,
    };
    let written = included.unwrap_or(calling);
    let fragment_tokens = Some(fragment_tokens);
    let fragment = Fragment {
        tokens: fragment_tokens,
        module: src.module,
        owner: call,
        text,
        outline: &made.outline,
        items: None,
        at: Where::Expansion(src.loc),
        depth: src.depth + 1,
        made: false,
        members_of: src.owner,
        written,
        kept: None,
        marks: column.map(|column| crate::crate_roots::TextMarks { text, column }),
    };
    // The lowering is kept (the call's declarations and modules): in `home`.
    let mut out = lower(LowerCx { krate: base.key, tools: &base.root.tools, edition: base.edition, cfg: base.cfg, walked: base.tree, procs: base.expansion_procs(), entry: false, harness: false, order: base.sources.attr_order() }, fragment, home_bump);
    // Entered only by a call that makes modules: one that makes none stays in `Lower` for the
    // few moves below (what the part costs is what `make_modules` does: a module's file read,
    // outlined and lowered).
    if !out.mods_made.is_empty() {
        part(ExpandPart::Modules, mac.name, out.mods_made.len());
    }
    let mods = core::mem::replace(&mut out.mods_made, AVec::new_in(home));
    // An `include!`'s file is the current file of what it writes ([`crate::included`]): a
    // `mod x;` in it is found from the INCLUDED file's folder, not the calling module's.
    let included_dir: Option<&'a str> = included.filter(|_| !mods.is_empty()).map(|path| {
        let mut dir = AString::from_str_in(path, alloc);
        pop_dir(&mut dir);
        keep(&dir, home_bump)
    });
    let place = Made {
        text,
        outline: &made.outline,
        at: Where::Expansion(src.loc),
        place: At::Expansion(call),
        dir: included_dir,
        written,
        marks: column.map(|column| crate::crate_roots::TextMarks { text, column }),
        tokens: fragment_tokens,
    };
    let mut bodies = core::mem::replace(&mut out.bodies, AVec::new_in(home));
    make_modules(base, run, src.module, &mods, place, src.depth + 1, &mut bodies);
    let decls = core::mem::replace(&mut out.decls, DeclsBuilder::new_in(home));
    part(ExpandPart::Tables, mac.name, 0);
    add_lowered(run, call, At::Expansion(call), out);
    part(ExpandPart::Blocks, mac.name, bodies.len());
    blocks_of(base, run, &bodies, Some(made));
    Some((decls, Some(made)))
}

/// What [`ExpandNote`]s and part marks name a keep call's expansion by: no macro made it.
const KEPT_ITEM_NAME: &str = "kept item";

/// The builtin function-like macros whose expansion holds no token of their input (blaze-surface
/// macro_def.rs `BuiltinFn`): a literal, `Option`'s path with one, or nothing. A call of one in a
/// block declares nothing there, whatever its input ([`expand_call`]). `format_args!`,
/// `assert!`, `panic!`, `asm!`, `cfg_select!` and the rest place their input's expressions in
/// their output, and are asked by it.
const LITERAL_BUILTINS: [&str; 15] = [
    "concat",
    "concat_bytes",
    "stringify",
    "env",
    "option_env",
    "cfg",
    "line",
    "column",
    "file",
    "module_path",
    "compile_error",
    "include_str",
    "include_bytes",
    "trace_macros",
    "log_syntax",
];

/// The expansion of keep call `call` (`src`), its item kept after the attributes written after
/// its derives ([`crate::cells::KEEP_ITEM`], attr-bang-kind-2): the call's input, the item as
/// written, lowered as the call's fragment with [`Fragment::kept`] set to the names the calls
/// before it answered ([`CallSrc::answered`]), so what the calling fragment sent is not sent
/// again and the item's next stage is (attr-order-1: its later derives, its later names).
/// Its names are declared as an attribute macro's that returned the item would be; the item's
/// key is under the call's (as any expansion's).
fn keep_item<'a, S: SourceTexts<'a>>(
    base: &Base<'_, 'a, S>,
    run: &Run<'a>,
    call: ItemKey,
    src: CallSrc<'a>,
    part: PartMark<'_>,
    keep: KeepAs,
    note: &dyn Fn(ExpandNote<'a>),
) -> Option<(DeclsBuilder<'a, &'a str>, Option<&'a ExpansionText<'a>>)> {
    let alloc = base.alloc;
    let bump = base.bump;
    let text = src.input;
    let name = match keep {
        KeepAs::Item => KEPT_ITEM_NAME,
        KeepAs::Harness => HARNESS_NAME,
    };
    let kept_tokens = match src.tokens {
        crate::nested_input::CallTokens::Text => None,
        crate::nested_input::CallTokens::Expansion(input) => Some(input.of),
        crate::nested_input::CallTokens::Refused(error) => {
            note(ExpandNote::NestedInput { macro_name: name, error });
            return None;
        }
    };
    // Its origins: the identity column and the entry the call is at, the item itself
    // (expansion_origins.rs `kept`), so a head it declares is covered by that entry's check.
    // The harness's text is blaze's own, no slice of any written text: no column, its tokens
    // read at the module with no marks (front2 item_site.rs `marks_of_column`: `Rewritten` is
    // `None`), a finding in it filed at its own place.
    let origins = match keep {
        KeepAs::Item => crate::expansion_origins::kept(call, src.place, text, src.name, base.edition, bump),
        KeepAs::Harness => Err(crate::expansion_origins::OriginsError::Rewritten { what: "a proc-macro crate's harness (`_DECLS`)" }),
    };
    let made: &'a ExpansionText<'a> = bump.alloc_val(ExpansionText::new(text, outline(text, base.edition, alloc), origins));
    if let Some(error) = made.outline.errors().first() {
        note(ExpandNote::NotItems { macro_name: name, message: made.outline.message(error) });
        return None;
    }
    let fragment = Fragment {
        module: src.module,
        owner: call,
        text,
        outline: &made.outline,
        items: None,
        at: Where::Expansion(src.loc),
        depth: src.depth + 1,
        made: false,
        members_of: src.owner,
        written: src.written,
        // The names of the item the calls before this one answered (attr-order-1): the item's
        // attribute walk goes on from the next. The harness is no item's: a plain fragment.
        kept: match keep {
            KeepAs::Item => Some(src.answered),
            KeepAs::Harness => None,
        },
        // A kept item's text: the item as written at the call; its column (expansion_origins.rs
        // `kept`) holds no marks, and its tokens are read at the module, as this says.
        marks: None,
        // A slice of the calling text: written in an expansion, its calls take their inputs
        // from that expansion's tokens, placed in the same text.
        tokens: kept_tokens,
    };
    let mut out = lower(
        LowerCx { krate: base.key, tools: &base.root.tools, edition: base.edition, cfg: base.cfg, walked: base.tree, procs: base.expansion_procs(), entry: false, harness: false, order: base.sources.attr_order() },
        fragment,
        bump,
    );
    // A struct, an enum or a union makes no module; moved as `expand_decls` moves them so a
    // change there stays one shape.
    let mods = core::mem::replace(&mut out.mods_made, AVec::new_in(alloc));
    let place = Made { text, outline: &made.outline, at: Where::Expansion(src.loc), place: At::Expansion(call), dir: None, written: src.written, marks: None, tokens: kept_tokens };
    let mut bodies = core::mem::replace(&mut out.bodies, AVec::new_in(alloc));
    make_modules(base, run, src.module, &mods, place, src.depth + 1, &mut bodies);
    let decls = core::mem::replace(&mut out.decls, DeclsBuilder::new_in(alloc));
    // The harness declares no name: rustc writes `_DECLS` def-site, so no path of the crate
    // reaches it. Its text, key and body are the call's (`add_lowered` below), named by its
    // key alone (task/lower.rs `proc_macro_decls_key`).
    let decls = match keep {
        KeepAs::Item => decls,
        KeepAs::Harness => DeclsBuilder::new_in(alloc),
    };
    part(ExpandPart::Tables, name, 0);
    add_lowered(run, call, At::Expansion(call), out);
    part(ExpandPart::Blocks, name, bodies.len());
    blocks_of(base, run, &bodies, Some(made));
    Some((decls, Some(made)))
}

/// What [`keep_item`] lowers: a derived item kept after its derives, or a proc-macro crate's
/// harness (D175).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum KeepAs {
    Item,
    Harness,
}

/// What [`ExpandNote`]s and part marks name a proc-macro harness's expansion by.
const HARNESS_NAME: &str = "proc-macro harness";

/// The most bodies deep (with expansions) a block's module is made: a body in a block of a
/// body in a block, each level one more (rule 18).
const MAX_BLOCK_DEPTH: u16 = 64;

/// Makes the modules of the blocks of `bodies` that hold an `impl` ([`crate::blocks`]): each
/// body's text is scanned once by syntax; one that writes an impl in a block gets a fragment
/// under the key `(body, "blocks")`, lowered like an expansion's, whose modules are made
/// modules of the body's module: their impls land in the crate's tables with every other
/// impl, their members' bodies are bodies. The fragment's own declarations (its outermost
/// block's module) are given to no module: no name reaches a block from outside it. A body
/// lowered inside a block's module is scanned in turn: a work list, no recursion.
fn make_blocks<'a, S: SourceTexts<'a>>(
    base: &Base<'_, 'a, S>,
    run: &Run<'a>,
    bodies: &[crate::task::BodySrc<'a>],
    writer: Option<&ExpansionText<'a>>,
) {
    use crate::expansion_origins::OriginsError;

    let alloc = base.alloc;
    let bump = base.bump;
    let mut work: AVec<'a, crate::task::BodySrc<'a>> = AVec::from_slice_in(bodies, alloc);
    while let Some(body) = work.pop() {
        let nested = blaze_file_items::nested::scan(body.text, alloc);
        // An unbalanced body is the parser's error to report; nothing is read from it.
        // A block is a module when it holds an item that makes one (its head is made in the
        // block's module by the ordinary code; front2's `Locals::keep` derives the same module
        // key with `blocks::block_modules`) or a statement call (what it declares is known
        // once it is expanded: crate::expanded_blocks), or a local fn, const or static whose
        // own body writes a block (`NestedItem::body_writes`: the item is then the block
        // module's, and its body comes back to this loop with the module's items' bodies).
        if nested.end != blaze_file_items::nested::NestedEnd::Whole || !nested.writes() {
            continue;
        }
        if body.depth >= MAX_BLOCK_DEPTH {
            run.note(NamesNote::Made { name: crate::blocks::BLOCK_MODULE, at: body.loc, note: MadeNote::TooMany });
            continue;
        }
        let mut copies: AVec<'a, crate::crate_roots::Copied> = AVec::new_in(alloc);
        let rendered = crate::blocks::module_text(body.text, &nested, &mut |c| copies.push(c), alloc);
        let text: &'a str = bump.alloc_str(&rendered);
        let call = crate::task::key_of(base.key, body.key, "blocks", "", 0);
        // Copied tokens keep their indexes in the writing call's input, even when this
        // body came from another block module on the work list. Remap only their places.
        let origins = (|| {
            let of = body.marks.ok_or(OriginsError::Rewritten { what: "a block's module text" })?;
            let writer = writer.ok_or(OriginsError::Unplaced { call })?;
            let (call_part, _) = crate::expansion_origins::split(writer.origins?)?;
            let at = crate::expansion_origins::offset_in(of.text, body.text).ok_or(OriginsError::InputNotInText { call })?;
            let shifted = AVec::from_iter_in(copies.iter().map(|c| crate::crate_roots::Copied { from: at + c.from, ..*c }), alloc);
            let column_error = |error| OriginsError::Column { call: Some(call), error };
            let column = crate::crate_roots::remap(of.column, &shifted, alloc).map_err(column_error)?;
            let mut bytes = AVec::from_slice_in(call_part, alloc);
            column.write(&mut bytes).map_err(column_error)?;
            Ok(&*bump.alloc_slice_copy(&bytes))
        })();
        let made: &'a ExpansionText<'a> = bump.alloc_val(ExpansionText::new(text, outline(text, base.edition, alloc), origins));
        let marks = match (body.marks, made.column) {
            (None, _) => None,
            (Some(_), Ok(column)) => Some(crate::crate_roots::TextMarks { text, column }),
            (Some(_), Err(lost)) => {
                run.note(NamesNote::Expand {
                    call,
                    at: Some(body.loc),
                    note: ExpandNote::RootsLost { macro_name: crate::blocks::BLOCK_MODULE, lost },
                });
                continue;
            }
        };
        // A body a macro wrote: the tokens that placed its text move with the same slices, so a
        // call in the block's module takes its input as the expansion's tokens (bitflags'
        // `fn all() { .. $crate::__bitflags_expr_safe_attrs!(..) .. }`, written through
        // `__impl_bitflags!`'s argument). A body outside the text they place is no
        // expansion's: none.
        let tokens = body.tokens.and_then(|of| {
            let at = crate::expansion_origins::offset_in(of.text, body.text)?;
            let mut shifted: AVec<'a, crate::crate_roots::Copied> = AVec::with_capacity_in(copies.len(), alloc);
            shifted.extend(copies.iter().map(|c| crate::crate_roots::Copied { from: c.from + at, ..*c }));
            Some(of.moved(text, &shifted, bump))
        });
        let fragment = Fragment {
            module: body.module,
            owner: call,
            text,
            outline: &made.outline,
            items: None,
            at: Where::Expansion(body.loc),
            depth: body.depth + 1,
            made: false,
            members_of: None,
            written: body.written,
            kept: None,
            marks,
            tokens,
        };
        let mut out = lower(LowerCx { krate: base.key, tools: &base.root.tools, edition: base.edition, cfg: base.cfg, walked: base.tree, procs: base.expansion_procs(), entry: false, harness: false, order: base.sources.attr_order() }, fragment, bump);
        let mods = core::mem::replace(&mut out.mods_made, AVec::new_in(alloc));
        let place = Made { text, outline: &made.outline, at: Where::Expansion(body.loc), place: At::Expansion(call), dir: None, written: body.written, marks, tokens };
        {
            let mut places = held(run.block_places.lock(), "block places");
            for m in mods.iter() {
                places.insert(m.key, body.place);
            }
        }
        make_modules(base, run, body.module, &mods, place, body.depth + 1, &mut work);
        // The outermost block's `mod` declaration: dropped, so the body's module binds nothing.
        out.decls = DeclsBuilder::new_in(alloc);
        add_lowered(run, call, At::Expansion(call), out);
        held(run.expansions.lock(), "expansions").insert(call, Expansion { decls: DeclsBuilder::new_in(alloc), made: Some(made), refused: None, refused_by: None });
    }
}

/// Drives an eager builtin's nested calls: `include!(concat!(env!("OUT_DIR"), "/x.rs"))`.
///
/// The rule, as the pinned library documents each builtin (core/src/macros/mod.rs): `include!`
/// takes "a string literal" path, `concat!` "any number of comma-separated literals",
/// `env!`'s argument is the variable's name; the compiler expands macro calls written in those
/// positions FIRST (the Reference's "Macros by example" has no eager form: these are compiler
/// builtins, and blaze-expand answers [`Expanded::NeedsExpanded`] listing them). Here they are
/// expanded inside-out by a worklist (no recursion), each answer sealed by its call's key, and
/// the outer builtin re-entered with them, at most [`EAGER_DEPTH`] calls deep. A nested call
/// must name a library builtin (`concat`, `core::concat`, `std::env`, ...) that the crate does not
/// shadow with a macro of its own: anything else is a typed refusal naming it ([`EagerRefusal`]).
/// The nested calls' reads (`env!`'s variable) are the crate's script outputs, already in its key.
struct EagerDriver<'s, 'a, 'w> {
    macro_name: &'a str,
    /// Where the builtins the nested calls name are defined.
    def_at: DefAt,
    shadowed: &'s dyn Fn(&str) -> bool,
    /// Where a note's text is kept: the crate's bump arena (a note outlives the call).
    keep: Bump<'a>,
    /// Where the work is done: the call's scratch ([`crate::task::Par::call_scratch`]), or the
    /// crate's allocator when the Par gave none.
    alloc: Alloc<'w>,
}

/// A nested call on the worklist: its builtin, its arguments, the calls nested in them once
/// it asked, and their answers so far.
struct EagerFrame<'a> {
    def: MacroDefinition<'a>,
    name: &'static str,
    input: TokenStream<'a>,
    nested: Option<AVec<'a, NestedCall>>,
    answers: AVec<'a, TokenStream<'a>>,
}

impl<'a, 'w> EagerDriver<'_, 'a, 'w> {
    fn refused(&self, nested: &str, why: EagerRefusal) -> ExpandNote<'a> {
        ExpandNote::Eager { macro_name: self.macro_name, nested: keep(nested, self.keep), why }
    }

    /// `call` of `def`, its nested calls expanded first when it is eager.
    fn expand(&self, def: Kept<'_>, call: &Call<'_>, cx: &impl ExpandCx) -> Result<blaze_expand::Expansion<'w>, ExpandNote<'a>> {
        let failed = |error| ExpandNote::Expand { macro_name: self.macro_name, error };
        let nested = match def.expand(call, cx, self.alloc).map_err(failed)? {
            Expanded::Expansion(expansion) => return Ok(expansion),
            Expanded::NeedsExpanded(nested) => nested,
        };
        // Only a function-like builtin is eager.
        let CallKind::FnLike { args } = call.kind else { return Err(self.refused(self.macro_name, EagerRefusal::NotACall)) };
        let answers = self.answers(args, nested, call.site, cx)?;
        let mut refs: AVec<'_, &TokenStream<'_>> = AVec::with_capacity_in(answers.len(), self.alloc);
        refs.extend(answers.iter());
        let again = Call { kind: CallKind::FnLike { args }, site: call.site, eager: blaze_expand::Eager { inside: call.eager.inside, answers: &refs } };
        match def.expand(&again, cx, self.alloc).map_err(failed)? {
            Expanded::Expansion(expansion) => Ok(expansion),
            Expanded::NeedsExpanded(_) => Err(self.refused(self.macro_name, EagerRefusal::AskedAgain)),
        }
    }

    /// The answers of the outer call's `nested` calls, in order.
    fn answers(&self, outer: &TokenStream<'_>, nested: AVec<'w, NestedCall>, site: CallSite, cx: &impl ExpandCx) -> Result<AVec<'w, TokenStream<'w>>, ExpandNote<'a>> {
        let alloc = self.alloc;
        let mut root: AVec<'w, TokenStream<'w>> = AVec::new_in(alloc);
        let mut stack: AVec<'w, EagerFrame<'w>> = AVec::new_in(alloc);
        loop {
            // The top frame, once its nested calls are answered (or before it asked): expanded.
            if let Some(top) = stack.last_mut()
                && top.nested.as_ref().is_none_or(|n| top.answers.len() == n.len())
            {
                let (key, result) = {
                    let mut refs: AVec<'_, &TokenStream<'_>> = AVec::with_capacity_in(top.answers.len(), alloc);
                    refs.extend(top.answers.iter());
                    let call = Call { kind: CallKind::FnLike { args: &top.input }, site, eager: blaze_expand::Eager { inside: Inside::EagerInput, answers: &refs } };
                    (call_key(&top.def, &call).0, expand(&top.def, &call, cx, alloc))
                };
                let name = top.name;
                match result.map_err(|error| ExpandNote::Expand { macro_name: self.macro_name, error })? {
                    Expanded::Expansion(expansion) => {
                        // The nested call failed (`env!` of a variable not set, ..): its own
                        // reason, as a failed call's (rustc reports the nested call's error).
                        // The note outlives the call: kept in the crate's allocator.
                        if let Some(failed) = blaze_expand::Failed::of(&expansion, self.keep) {
                            return Err(ExpandNote::Failed { path: keep(name, self.keep), implicit: false, failed });
                        }
                        let mut tokens = expansion.tokens;
                        tokens.seal(key);
                        stack.pop();
                        match stack.last_mut() {
                            Some(parent) => parent.answers.push(tokens),
                            None => root.push(tokens),
                        }
                    }
                    Expanded::NeedsExpanded(asked) if top.nested.is_none() => top.nested = Some(asked),
                    Expanded::NeedsExpanded(_) => return Err(self.refused(name, EagerRefusal::AskedAgain)),
                }
                continue;
            }
            // The next nested call of the top frame (or of the outer call): pushed.
            let (input, calls, done): (&TokenStream<'_>, &[NestedCall], usize) = match stack.last() {
                Some(top) => (&top.input, top.nested.as_deref().unwrap_or(&[]), top.answers.len()),
                None => (outer, &nested, root.len()),
            };
            let Some(call) = calls.get(done) else { return Ok(root) };
            let path = eager_path_text(input, call.path.start, call.path.end, alloc);
            if call.kind != NestedKind::FnLike {
                return Err(self.refused(&path, EagerRefusal::NotACall));
            }
            if stack.len() >= EAGER_DEPTH {
                return Err(self.refused(&path, EagerRefusal::TooDeep));
            }
            let segments: AVec<'_, &str> = {
                let mut it = AVec::new_in(alloc);
                it.extend(path.split("::").filter(|s| !s.is_empty()));
                it
            };
            let which = match segments[..] {
                [name] | ["core" | "std", name] => BuiltinFn::of_library(name),
                _ => None,
            };
            let Some(which) = which else { return Err(self.refused(&path, EagerRefusal::NotBuiltin)) };
            if (self.shadowed)(segments[segments.len() - 1]) {
                return Err(self.refused(&path, EagerRefusal::Shadowed));
            }
            let inner = call.inner();
            let Ok(args) = input.copy_in(inner.start as usize..inner.end as usize, alloc) else {
                return Err(self.refused(&path, EagerRefusal::Stream));
            };
            let def = MacroDefinition::builtin_fn(which, self.def_at, alloc);
            stack.push(EagerFrame { def, name: which.name(), input: args, nested: None, answers: AVec::new_in(alloc) });
        }
    }
}

/// The path `stream`'s tokens `start..end` write, as source text (`a::b`).
fn eager_path_text<'a>(stream: &TokenStream<'_>, start: u32, end: u32, alloc: Alloc<'a>) -> AString<'a> {
    let mut out = AString::new_in(alloc);
    for at in start..end {
        match stream.tokens()[at as usize].kind {
            TokenKind::Ident { .. } => out.push_str(stream.text_at(at as usize)),
            _ => out.push(':'),
        }
    }
    out
}

/// The one home of a macro's definition as blaze-expand takes it, from its lowered source:
/// `krate` and `edition` are the defining crate's (its `$crate`, its body's syntax), `shape`
/// how it is called (a builtin derive only as a derive). A proc macro's definition names its
/// crate by `runner`, the key its proc-macro runner knows it by ([`CrateNames::proc_key`]).
/// Every expansion, item-position here and in bodies ([`Names::macro_definition`]), builds its
/// definition here.
pub fn definition_of<'a>(
    mac: MacroSrc<'a>,
    krate: CrateItemKey,
    runner: Option<CrateItemKey>,
    edition: Edition,
    shape: CallShape,
    alloc: Alloc<'a>,
) -> Result<MacroDefinition<'a>, ExpandNote<'a>> {
    let module = matches!(mac.body, MacroBody::Macro2 { .. }).then_some(mac.module);
    let def_at = DefAt {
        site: DefSite { krate, module },
        edition,
        export: if mac.export { Export::Exported } else { Export::Local },
        local_inner: if mac.local_inner { LocalInner::Yes } else { LocalInner::No },
        transparency: None,
    };
    let lexed = |text: &str, what: Lexed| lex(text, edition, alloc).map_err(|error| ExpandNote::Lex { macro_name: mac.name, what, error });
    Ok(match mac.body {
        MacroBody::Rules(body) => MacroDefinition::rules(lexed(body, Lexed::Body)?, def_at),
        MacroBody::Macro2 { params, body } => {
            let params = match params {
                Some(p) => Some(lexed(p, Lexed::Parameters)?),
                None => None,
            };
            MacroDefinition::macro2(params, lexed(body, Lexed::Body)?, def_at)
        }
        MacroBody::BuiltinFn(name) => match BuiltinFn::of_library(name) {
            Some(which) => MacroDefinition::builtin_fn(which, def_at, alloc),
            None => return Err(ExpandNote::BuiltinUnknown { name }),
        },
        MacroBody::BuiltinDerive { .. } if shape == CallShape::Derive => {
            match BuiltinDerive::ALL.into_iter().find(|it| it.name() == mac.name) {
                Some(which) => MacroDefinition::builtin_derive(which, def_at, alloc),
                None => return Err(ExpandNote::BuiltinUnknown { name: mac.name }),
            }
        }
        MacroBody::BuiltinDerive { .. } => return Err(ExpandNote::BuiltinOtherCalled { shape: CallShape::Derive, macro_name: mac.name }),
        // `#[global_allocator]` is the one built-in attribute blaze-expand writes (its four
        // allocator symbols beside the static); the rest stay refused by name.
        MacroBody::BuiltinOther(CallShape::Attr) if shape == CallShape::Attr && mac.name == BuiltinAttr::GlobalAllocator.name() => {
            MacroDefinition::builtin_attr(BuiltinAttr::GlobalAllocator, def_at, alloc)
        }
        MacroBody::BuiltinOther(shape) => return Err(ExpandNote::BuiltinOtherCalled { shape, macro_name: mac.name }),
        MacroBody::Proc { kind, helpers } => {
            // No runner holds its crate: the call has no definition here.
            let Some(runner) = runner else { return Err(ExpandNote::NoDefinition { shape, name: mac.name }) };
            let mut names: AVec<'a, &str> = AVec::new_in(alloc);
            names.extend(ProcAttr::helper_names(helpers));
            let at = DefAt { site: DefSite { krate: runner, module: None }, ..def_at };
            MacroDefinition::proc_macro(runner, kind, mac.name, &names, at, alloc)
                .map_err(|error| ExpandNote::Expand { macro_name: mac.name, error: ExpandError::Stream { stream: None, error } })?
        }
    })
}

/// Why [`Names::macro_definition`] has no definition for a path.
#[derive(Clone, Copy, Debug)]
pub enum MacroDefError<'a> {
    /// The path names nothing usable ([`Names::item_at`]'s error).
    Path(PathError),
    /// It names a macro with no lowered source: a procedural one (its crate's proc-macro
    /// runner's) or one not read.
    NoSource { def: Def, krate: usize },
    /// The source does not make a definition (it does not lex, a builtin with no form).
    Refused(ExpandNote<'a>),
}

/// The planted bug's match ([`AttrPlant::Spelling`]): two paths' texts, spaces taken out. A
/// `$crate` an expansion wrote is `$crate` on the item and `crate` in the lowering's text, so
/// this never finds it.
fn same_spelling(a: &str, b: &str) -> bool {
    a.chars().filter(|c| !c.is_whitespace()).eq(b.chars().filter(|c| !c.is_whitespace()))
}

/// The files' lock (a function so a guard is never held across a call that takes it).
fn base_files<'r, 'a>(run: &'r Run<'a>) -> LockResult<std::sync::MutexGuard<'r, FileSet<'a>>> {
    run.files.lock()
}

/// A fragment's tables added to the run's.
fn add_lowered<'a>(run: &Run<'a>, owner: ItemKey, at: At, out: Lowered<'a, 'a>) {
    let notes = held(run.tables.write(), "tables").add(owner, at, out);
    let mut held_notes = held(run.notes.lock(), "notes");
    held_notes.extend(notes.into_iter().map(NamesNote::Lower));
}

/// The text a fragment that writes modules is: its text and outline, the lowering's `Where`
/// and the place its items are given.
#[derive(Clone, Copy)]
struct Made<'a> {
    text: &'a str,
    outline: &'a Outline<'a>,
    at: Where,
    place: At,
    /// The folder of the file an `include!` read this fragment from ([`crate::included`]):
    /// where a `mod x;` written at the fragment's top is found and what its `#[path]` is
    /// relative to, in place of the calling module's folder. `None` for any other fragment.
    dir: Option<&'a str>,
    /// [`Fragment::written`] of this text: what an inline module made from it is written in.
    written: &'a str,
    /// [`Fragment::marks`] of this text.
    marks: Option<crate::crate_roots::TextMarks<'a>>,
    /// [`Fragment::tokens`] of this text.
    tokens: Option<crate::nested_input::ExpansionTokens<'a>>,
}

/// Where a module of this crate is: its children's directory, what their `#[path]` is relative
/// to, its chain up to the root, and its path; walked or made by a macro.
fn place_of<'a, S>(base: &Base<'_, 'a, S>, run: &Run<'a>, module: ItemKey) -> Option<(&'a str, &'a str, AVec<'a, ItemKey>, &'a str)> {
    let alloc = base.alloc;
    let bump = base.bump;
    if let Some(&i) = base.tree.by_key.get(&module) {
        let m = &base.tree.modules[i];
        let chain = AVec::from_slice_in(base.chains.get(&module)?, alloc);
        return Some((keep(&m.dir, bump), keep(&m.path_dir, bump), chain, keep(&m.path, bump)));
    }
    if let Some(it) = base.made_before.and_then(|m| m.get(&module)) {
        return Some((it.dir, it.path_dir, it.chain.clone_in(alloc), it.path));
    }
    let made = held(run.made.read(), "made modules");
    made.get(&module).map(|it| (it.dir, it.path_dir, it.chain.clone_in(alloc), it.path))
}

/// The file at `path`, read and outlined the first time a module names it.
fn file_at<'a, S: SourceTexts<'a>>(base: &Base<'_, 'a, S>, run: &Run<'a>, path: &str) -> Option<(u32, &'a SourceFile<'a>)> {
    let mut files = held(base_files(run), "files");
    if let Some(at) = files.find(path) {
        return Some((at, files.at(at as usize)));
    }
    let path = keep(path, base.bump);
    let text = base.sources.read(base.index, path)?;
    let file = file_of(path, text, outline(text, base.edition, base.alloc), base.shown_from, base.bump);
    Some((files.add(file), file))
}

/// Makes the modules `mods`, written in a fragment of `parent` (`made`), and every module
/// inside them: each one's declarations lowered from its own items (an inline one's from its
/// body, a `mod x;` one's from its file, found as the walk finds one), its place recorded in
/// `run.made`, and its key queued for the fill. A worklist, not recursion: a module inside a
/// made one is pushed once its parent is recorded.
fn make_modules<'a, S: SourceTexts<'a>>(
    base: &Base<'_, 'a, S>,
    run: &Run<'a>,
    parent: ItemKey,
    mods: &[ModMade<'a>],
    made: Made<'a>,
    depth: u16,
    // The bodies lowered in the made modules that may write an impl in a block, added.
    bodies: &mut AVec<'a, crate::task::BodySrc<'a>>,
) {
    let alloc = base.alloc;
    let bump = base.bump;
    let top = parent;
    let mut work: AVec<'a, (ItemKey, ModMade<'a>, Made<'a>)> = AVec::with_capacity_in(mods.len(), alloc);
    work.extend(mods.iter().map(|m| (parent, *m, made)));
    while let Some((parent, m, made)) = work.pop() {
        let note = |note: MadeNote<'a>| run.note(NamesNote::Made { name: m.name, at: m.loc, note });
        {
            let all = held(run.made.read(), "made modules");
            if all.contains_key(&m.key) {
                continue;
            }
            if all.len() >= MAX_MADE_MODULES {
                drop(all);
                note(MadeNote::TooMany);
                continue;
            }
        }
        let Some((dir, path_dir, parent_chain, parent_path)) = place_of(base, run, parent) else {
            note(MadeNote::NoPlace);
            continue;
        };
        // A module written at the top of an included file ([`crate::included`]): the included
        // file's folder is its parent's folder. One inside an inline module of that file has
        // that module as its parent, whose folder was made from this one.
        let (dir, path_dir) = match made.dir {
            Some(included) if parent == top => (included, included),
            _ => (dir, path_dir),
        };
        let (fragment_of, item, mdir, mpdir): (Made<'a>, Option<ItemIndex>, &'a str, &'a str) = if m.inline {
            let d = keep(&joined(dir, &[m.path_attr.unwrap_or(m.name)], alloc), bump);
            (made, Some(m.item), d, d)
        } else {
            let sub = keep(&joined(dir, &[m.name], alloc), bump);
            let (file, mdir, mpdir) = if let Some(p) = m.path_attr {
                let target = joined(path_dir, &[p], alloc);
                let mut parent_dir = target.clone_in(alloc);
                pop_dir(&mut parent_dir);
                let parent_dir = keep(&parent_dir, bump);
                let Some(file) = file_at(base, run, &target) else {
                    note(MadeNote::FileNotFound { path: keep(&target, bump) });
                    continue;
                };
                (file, parent_dir, parent_dir)
            } else {
                let mut file_name = AString::new_in(alloc);
                file_name.push_str(m.name);
                file_name.push_str(".rs");
                let a = joined(dir, &[&*file_name], alloc);
                match file_at(base, run, &a) {
                    Some(file) => (file, sub, dir),
                    None => {
                        let b = joined(dir, &[m.name, "mod.rs"], alloc);
                        let Some(file) = file_at(base, run, &b) else {
                            note(MadeNote::FileNotFound { path: keep(&b, bump) });
                            continue;
                        };
                        (file, sub, sub)
                    }
                }
            };
            let (at, file) = file;
            if !file_attrs(base.cfg, &file.outline, file.text, alloc).cfg_ok {
                note(MadeNote::CfgOff);
                continue;
            }
            (Made { text: file.text, outline: &file.outline, at: Where::File(at), place: At::File(at), dir: None, written: file.path, marks: None, tokens: None }, None, mdir, mpdir)
        };
        let fragment = Fragment {
            module: m.key,
            owner: m.key,
            text: fragment_of.text,
            outline: fragment_of.outline,
            items: item,
            at: fragment_of.at,
            depth,
            made: true,
            members_of: None,
            written: fragment_of.written,
            kept: None,
            marks: fragment_of.marks,
            tokens: fragment_of.tokens,
        };
        let mut out = lower(LowerCx { krate: base.key, tools: &base.root.tools, edition: base.edition, cfg: base.cfg, walked: base.tree, procs: base.expansion_procs(), entry: false, harness: false, order: base.sources.attr_order() }, fragment, bump);
        bodies.extend(out.bodies.iter().copied());
        let mut chain = AVec::with_capacity_in(parent_chain.len() + 1, alloc);
        chain.push(m.key);
        chain.extend_from_slice(&parent_chain);
        let mut path = BString::from_str_in(parent_path, bump);
        path.push_str("::");
        path.push_str(m.name);
        let module = MadeModule {
            info: ModuleInfo { krate: base.key, parent: Some(parent), name: Some(m.name), no_implicit_prelude: m.no_implicit_prelude },
            decls: core::mem::replace(&mut out.decls, DeclsBuilder::new_in(alloc)),
            chain,
            path: path.into_bump_str(),
            dir: mdir,
            path_dir: mpdir,
            at: fragment_of.place,
            item,
        };
        held(run.made.write(), "made modules").insert(m.key, module);
        held(run.new_modules.lock(), "new modules").push(m.key);
        for inner in out.mods_made.iter() {
            work.push((m.key, *inner, fragment_of));
        }
        add_lowered(run, m.key, fragment_of.place, out);
    }
}
