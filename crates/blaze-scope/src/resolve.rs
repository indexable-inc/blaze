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
    BuiltinDerive, Call, CallSite, Ctx, DefAt, ItemAttrs, ItemAttrsError, NextAttr, DefSite, Delim, Diagnostic, ExpandCx, ExpandError, Expanded,
    Export, HostAsk, HostError, HostWhy, IncludeAs, Included, LexError, LitKind, LocalInner,
    MacroDefinition, Place as HostPlace, PlaceOf, ProcCall, Produced, Raw, Spacing, TokenKind,
    TokenStream, expand, lex, CallKind, Inside, NestedCall, NestedKind, PathScope, call_key,
};
use blaze_file_items::{Outline, outline};
use blaze_format::FormatError;
use blaze_hash::HashMap;
use blaze_id::{ContentId, CrateItemKey, ItemKey};
use blaze_own::{AString, AVec, Alloc, Box};
use blaze_surface::{
    Binding, BindingState, BuildError, BuiltinFn, CallShape, CrateRootBuilder, CrateRootView,
    DeclsBuilder, Def, DefKind, Edition, ExternCrateDecl, ExternTarget, ItemIndex, MacroShapes, ModuleDeclsView,
    ModuleInfo, Namespace, Origin, Rename, ScopeBuilder, SurfaceError, Visibility, WrittenVis,
};
use blaze_build_script::{cx::ScriptCx, outputs::OutputsRef};
use blaze_unit_graph::Implicit;

use crate::graph::{CrateGraph, CrateId, ExternGone, GraphCrate, GraphDep, GraphError, RootAttrs, Sysroot};
use crate::names::{PathError, Why, World, Written};
use crate::task::{
    CallSrc, Cfg, DollarCrate, Fill, FillEnv, FillReport, Files, First, Fragment, Held, ImplSrc,
    ItemText, LangSrc, Loc, LowerCx, LowerNote, Lowered, MacroBody, MacroSrc, Member, ModMade, Par,
    PreludeImport, RootInput, RootNote, RootPlan, TaskError, Tree, Variant, WalkNote, Where,
    crate_root, file_attrs, lower, walk,
};
use crate::task::{joined, pop_dir};
use crate::task::ExpandPart;
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
    /// The text of the file at `path` (absolute, `/`-separated, without `.` or `..`) of the
    /// crate at `krate` (its index in the inputs), or `None` when it has no such file.
    fn read(&self, krate: usize, path: &str) -> Option<&'a str>;

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

    /// The environment the crate at `krate` is compiled in ([`crate::crate_env::CrateEnv`]:
    /// one per unit, made by `UnitCrates::env_of`): what `env!` and `option_env!` read, the
    /// same for its items and its bodies. `None`: this source has none, and every `env!` of
    /// the crate is refused by name, never answered unset.
    fn crate_env(&self, krate: usize) -> Option<&crate::crate_env::CrateEnv<'a>> {
        let _ = krate;
        None
    }
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
    pub text: &'a str,
    pub id: ContentId,
    pub outline: Outline<'a>,
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
}

impl<'a> Tables<'a> {
    fn new_in(alloc: Alloc<'a>) -> Tables<'a> {
        Tables {
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
        let Lowered { macros, calls, enums, impls, langs, use_locs, texts, members, ctors, notes, .. } = part;
        for (owner, list) in members {
            self.join_members(owner, list);
        }
        self.ctors.extend(ctors.into_iter().map(|k| (k, ())));
        self.macros.extend(macros);
        self.calls.extend(calls);
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
        let Tables { macros, calls, enums, impls, langs, use_locs, texts, by_place, members, ctors } = other;
        self.by_place.extend(by_place);
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

/// An expansion's text, outlined.
#[derive(Debug)]
pub struct ExpansionText<'a> {
    pub text: &'a str,
    pub outline: Outline<'a>,
}

/// A macro call's expansion: what it adds to its module.
#[derive(Debug)]
pub struct Expansion<'a> {
    /// Empty when the call is not expanded (the note says why).
    pub decls: DeclsBuilder<'a, &'a str>,
    /// `None` when no text was made.
    pub made: Option<&'a ExpansionText<'a>>,
}

/// Which text of a call did not lex.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Lexed {
    Body,
    Parameters,
    Input,
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
    /// A diagnostic of the macro's (its expansion is kept).
    Diagnostic { macro_name: &'a str, diagnostic: Diagnostic },
    /// The expansion does not read as items: the outline's first syntax error.
    NotItems { macro_name: &'a str, message: &'a str },
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
    /// The nested call's expansion has an error diagnostic.
    Failed,
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
    /// The walk kept finding files past [`MOST_WALKS`] passes.
    WalkUnbounded { krate: &'a str },
    RootRecord { krate: &'a str, error: RecordError },
    /// A crate the graph's waves never reached.
    NotResolved { krate: &'a str },
    /// The cell core, the names records' writer, refused the crate.
    Cells { krate: &'a str, error: crate::cells::Unwritten },
}

impl fmt::Display for NamesError<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            NamesError::Graph(error) => write!(f, "the crate graph: {error}"),
            NamesError::NoSuchDep { krate, name, index } => {
                write!(f, "crate `{krate}`: dependency `{name}` is crate {index}, past the inputs")
            }
            NamesError::RootNotRead { krate, path } => write!(f, "crate `{krate}`: its root file {path} is not read"),
            NamesError::WalkUnbounded { krate } => {
                write!(f, "crate `{krate}`: the module walk found new files past {MOST_WALKS} passes")
            }
            NamesError::RootRecord { krate, error } => write!(f, "crate `{krate}`: its CrateRoot record: {error:?}"),
            NamesError::NotResolved { krate } => write!(f, "crate `{krate}`: not reached by the graph's waves"),
            NamesError::Cells { krate, error } => write!(f, "crate `{krate}`: {error}"),
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
    /// The modules whose task failed, and why.
    pub failed: HashMap<ItemKey, TaskError, Alloc<'a>>,
    pub report: FillReport,
    pub notes: AVec<'a, NamesNote<'a>>,
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

    /// A module's place, walked or made.
    pub(crate) fn info(&self, module: ItemKey) -> Option<ModuleInfo<&'a str>> {
        self.decls.get(&module).map(|it| it.0).or_else(|| self.made.get(&module).map(|m| m.info))
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
    alloc: Alloc<'a>,
}

impl<'a> Names<'a> {
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
        Ok(Names { graph, crates: AVec::from_slice_in(all, alloc), slots, by_prefix, alloc })
    }

    pub fn graph(&self) -> &CrateGraph<'a, &'a str> {
        &self.graph
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
        let krate = self.crate_of(def.key).ok_or(PathError { module, at: len - 1, why: Why::NotRead { module: def.key } })?;
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

    fn variants(&self, owner: ItemKey) -> Option<&[Variant<&'a str>]> {
        Names::variants(self, owner)
    }

    fn no_implicit_prelude(&self, module: ItemKey) -> bool {
        let Some(at) = self.crate_of(module) else { return false };
        let c = self.crates[at];
        let Some(chain) = c.chains.get(&module) else { return false };
        chain.iter().any(|m| c.info(*m).is_some_and(|info| info.no_implicit_prelude))
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
    alloc: Alloc<'a>,
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
    alloc: Alloc<'a>,
) -> Result<Names<'a>, NamesError<'a>> {
    let graph = graph_of(crates, sysroot, alloc)?;
    let mut finished: AVec<'a, Option<Dep<'a>>> = AVec::with_capacity_in(crates.len(), alloc);
    finished.extend(crates.iter().map(|_| None));
    let mut whole: AVec<'a, Option<&'a CrateNames<'a>>> = AVec::with_capacity_in(crates.len(), alloc);
    whole.extend(crates.iter().map(|_| None));
    let mut by_prefix = HashMap::new_in(alloc);
    let prepares = Prepares::new_in(alloc);
    for wave in graph.waves() {
        for &id in wave {
            let done = {
                let built = Built { crates: &finished, by_prefix: &by_prefix, asked: None };
                resolve_one::<S, P>(&graph, &crates[id.index()], id, id.index(), &built, sources, par, first, prepares, &crate::expansions::KeptExpansions::none(alloc), alloc)?
            };
            let done: &'a CrateNames<'a> = Box::leak(Box::new_in(done, alloc));
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
    Ok(Names { graph, crates: all, slots: finished, by_prefix, alloc })
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
                    let found = other.enum_of(source.key).and_then(|v| match variant_answer(v, self.ns, self.name) {
                        NameAnswer::Found { def, vis, state, .. } => Some(Passed { def, vis, state }),
                        NameAnswer::Absent => None,
                    });
                    return Ok(Node::Value(found));
                };
                if let Some(b) = scope.get(self.ns, self.name) {
                    return Ok(Node::Value(Some(Passed { def: b.def, vis: b.vis, state: b.state })));
                }
                let Some(chain) = other.chain(source.key) else { return Err(()) };
                Ok(Node::Edges { edges: &scope.globs, krate: other.key(), chain })
            }
        }
        let other = self.of(module)?;
        if let Some(b) = other.scope(module).and_then(|s| s.get(ns, name).copied()) {
            return Some(b);
        }
        let mut sources = Records { built: self, ns, name };
        let p = blaze_surface::through_in(&mut sources, Def::module(module), scratch).ok()??;
        Some(Binding { def: p.def, vis: p.vis, origin: Origin::Glob, expansion: 0, state: p.state })
    }
}

/// A `Name` read of an enum: its variant of the name (a record variant has no value).
fn variant_answer(variants: &[Variant<&str>], ns: Namespace, name: &str) -> NameAnswer {
    let hit = variants.iter().find(|v| v.name == name && (ns == Namespace::Types || (ns == Namespace::Values && !v.record)));
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

/// A crate's files: a list by index and an index by path.
struct FileSet<'a> {
    files: AVec<'a, &'a SourceFile<'a>>,
    index: HashMap<&'a str, u32, Alloc<'a>>,
}

impl<'a> FileSet<'a> {
    fn new_in(alloc: Alloc<'a>) -> FileSet<'a> {
        FileSet { files: AVec::new_in(alloc), index: HashMap::new_in(alloc) }
    }

    fn add(&mut self, file: &'a SourceFile<'a>) -> u32 {
        let at = u32::try_from(self.files.len()).expect("a crate's files are counted in u32");
        self.files.push(file);
        self.index.insert(file.path, at);
        at
    }
}

impl<'a> Files<'a> for FileSet<'a> {
    fn find(&self, path: &str) -> Option<u32> {
        self.index.get(path).copied()
    }

    fn text(&self, file: u32) -> &'a str {
        self.files[file as usize].text
    }

    fn outline(&self, file: u32) -> &Outline<'_> {
        &self.files[file as usize].outline
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
fn keep<'a>(text: &str, alloc: Alloc<'a>) -> &'a str {
    Box::leak(AString::from_str_in(text, alloc).into_boxed_str())
}

/// A file read and outlined, kept as long as `alloc`.
fn file_of<'a>(path: &'a str, text: &'a str, outlined: Outline<'a>, from: &str, alloc: Alloc<'a>) -> &'a SourceFile<'a> {
    let package = crate::package_path::PackagePath::of(path, from, alloc);
    Box::leak(Box::new_in(SourceFile { path, package, shown: shown(path, from), text, id: file_id(text), outline: outlined }, alloc))
}

/// A record's bytes copied to blocks aligned as blaze-format reads them in place.
#[repr(C, align(16))]
#[derive(Clone, Copy)]
struct Block([u8; 16]);

const _: () = assert!(core::mem::align_of::<Block>() == blaze_format::ALIGN);

pub fn aligned<'a>(bytes: &[u8], alloc: Alloc<'a>) -> &'a [u8] {
    let mut blocks = AVec::with_capacity_in(bytes.len().div_ceil(16), alloc);
    for chunk in bytes.chunks(16) {
        let mut block = [0u8; 16];
        block[..chunk.len()].copy_from_slice(chunk);
        blocks.push(Block(block));
    }
    let blocks: &'a [Block] = blocks.leak();
    // SAFETY: `Block` is `repr(C)` around `[u8; 16]`, so `blocks` is `16 * len` initialized
    // bytes with no padding, borrowed for `'a` and never written again.
    let all: &'a [u8] = unsafe { core::slice::from_raw_parts(blocks.as_ptr().cast::<u8>(), blocks.len() * 16) };
    &all[..bytes.len()]
}

// ---------------------------------------------------------------------------------------------
// One crate.

/// What the lowering of the walked modules gives.
struct Lowering<'a> {
    /// By module index: what each module's record is written from.
    decls: AVec<'a, DeclsBuilder<'a, &'a str>>,
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
    alloc: Alloc<'a>,
) -> Result<CrateNames<'a>, NamesError<'a>> {
    // The crate's index for `sources`: its graph id in a whole-set call, the caller's index
    // in a per-crate one ([`crate::crate_scope::resolve_crate`]).
    let index = source;
    let edition = input.edition;
    // `input.cfg` already holds the build script's `cargo:rustc-cfg`s: the caller joins them
    // (blaze-world's `ScriptOutputs`), as the crate's key does.
    let cfg = Cfg::from_atoms(input.cfg, alloc);
    let mut notes: AVec<'a, NamesNote<'a>> = AVec::new_in(alloc);

    // The files: the root's, then each the walk names, until it names no file not read.
    par.stage(crate::task::Stage::Files);
    let root_text = sources.read(index, input.root).ok_or(NamesError::RootNotRead { krate: input.name, path: input.root })?;
    let mut set = FileSet::new_in(alloc);
    let root_file = set.add(file_of(input.root, root_text, outline(root_text, edition, alloc), input.shown_from, alloc));
    let mut root_dir = AString::from_str_in(input.root, alloc);
    pop_dir(&mut root_dir);
    let mut tried: HashMap<&'a str, (), Alloc<'a>> = HashMap::new_in(alloc);
    let mut walked = None;
    for _ in 0..MOST_WALKS {
        let tree = walk(&set, &cfg, input.key, input.name, root_file, &root_dir, edition, alloc);
        let mut read: AVec<'a, (&'a str, &'a str)> = AVec::new_in(alloc);
        for path in tree.missing.iter() {
            if set.find(path).is_some() || tried.contains_key(path.as_str()) {
                continue;
            }
            let path = keep(path, alloc);
            tried.insert(path, ());
            if let Some(text) = sources.read(index, path) {
                read.push((path, text));
            }
        }
        if read.is_empty() {
            walked = Some(tree);
            break;
        }
        let mut outlines = AVec::with_capacity_in(read.len(), alloc);
        par.map(read.len(), |at| outline(read[at].1, edition, alloc), &mut outlines);
        for (&(path, text), outlined) in read.iter().zip(outlines) {
            set.add(file_of(path, text, outlined, input.shown_from, alloc));
        }
    }
    let tree = walked.ok_or(NamesError::WalkUnbounded { krate: input.name })?;

    // Each walked module's own text, lowered.
    par.stage(crate::task::Stage::Lower);
    let listed = if input.implicit.proc_macro { sources.proc_exports(index) } else { None };
    let mut low = lower_modules(input.key, edition, &cfg, &set, &tree, listed, &mut notes, par, alloc);
    let root = set.files[root_file as usize];
    let attrs = file_attrs(&cfg, &root.outline, root.text, alloc);
    let root_attrs = RootAttrs { no_std: attrs.no_std, no_core: attrs.no_core, arbitrary_ints: attrs.arbitrary_ints };
    let plan = root_plan(graph, id, root_attrs, built, &mut notes, alloc);
    if let Some(name) = plan.injected {
        low.decls[0].extern_crates.push(ExternCrateDecl { target: ExternTarget::Dep(name), rename: Rename::Same, vis: WrittenVis::Private });
    }
    let root_input = RootInput {
        krate: input.key,
        root_file: root.id,
        edition,
        attrs: &attrs,
        tree: &tree,
        prelude_import: low.prelude.as_ref(),
        extern_crates: &low.decls[0].extern_crates,
        macro_use: &low.macro_use,
        macro_exports: &low.macro_exports,
    };
    let mut root_notes = AVec::new_in(alloc);
    let cr = crate_root(&root_input, &plan, &mut root_notes, alloc);
    notes.extend(root_notes.into_iter().map(NamesNote::Root));

    // The records, written and read back: what the fill and later crates read.
    par.stage(crate::task::Stage::Decls);
    let mut decls = HashMap::new_in(alloc);
    for (i, m) in tree.modules.iter().enumerate() {
        let info = ModuleInfo {
            krate: input.key,
            parent: m.parent.map(|p| tree.modules[p].key),
            name: m.name,
            no_implicit_prelude: m.no_implicit_prelude,
        };
        let bytes = match low.decls[i].write(m.key, &info, alloc) {
            Ok(bytes) => aligned(&bytes, alloc),
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
    let bytes = aligned(&cr.write(alloc).map_err(|e| root_record(RecordError::Refused(e)))?, alloc);
    let view = CrateRootView::read(bytes).map_err(|e| root_record(RecordError::NotRead(e)))?;
    let mut crate_root_record = CrateRootBuilder::from(&view, alloc);
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
    let all: AVec<'a, ItemKey> = {
        let mut all = AVec::with_capacity_in(tree.modules.len(), alloc);
        all.extend(tree.modules.iter().map(|m| m.key));
        all
    };
    let run = Run::new_in(set, prepares, alloc);
    let mut fill: Fill<'a, &'a str> = Fill::new_in(alloc);
    fill.first = first;
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
            built,
            sources,
            listed: listed.is_some(),
            kept,
            alloc,
        };
        let mark = |stage: crate::task::Stage| par.stage(stage);
        let part = |part: ExpandPart, macro_name: &str, size: usize| par.expand_part(part, macro_name, size);
        let seen = |def: ItemKey, input: &str| par.expand_call(def, input);
        let cx = FillCx { base: &base, run: &run, facts: Facts { base: &base, run: &run }, mark: &mark, part: &part, seen: &seen };
        // The cell core solves and writes the scopes. The files' blocks that hold impls are
        // modules before it starts: it is told of them as of any made module.
        par.stage(crate::task::Stage::Solve);
        make_blocks(&base, &run, &low.bodies);
        let places = |m: ItemKey| held(run.block_places.lock(), "block places").get(&m).copied();
        let expand = |call: ItemKey, def: Def| {
            let _ = crate::task::Expand::expand(&cx, ExpandRequest { call, def });
        };
        let made = |out: &mut AVec<'a, ItemKey>| out.extend(held(run.made.read(), "made modules").keys().copied());
        fill.done = crate::cells::solve_and_write(&cx.facts, built, &cx.facts, &places, tree.modules[0].key, &expand, &made, alloc)
            .map_err(|error| NamesError::Cells { krate: input.name, error })?;
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
    let failed = core::mem::replace(&mut fill.failed, HashMap::new_in(alloc));
    let report = fill.report;

    let Run { files, tables, expansions, made, notes: run_notes, new_modules: _, block_places: _, prepares: _ } = run;
    let files = held(files.into_inner(), "files").files;
    let mut tables_all = low.tables;
    tables_all.merge(held(tables.into_inner(), "tables"));
    let made = held(made.into_inner(), "made modules");
    for (&m, module) in made.iter() {
        chains.insert(m, module.chain.clone());
    }
    notes.extend(held(run_notes.into_inner(), "notes"));
    Ok(CrateNames {
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
        decls,
        crate_root: crate_root_record,
        chains,
        tables: tables_all,
        made,
        expansions: held(expansions.into_inner(), "expansions"),
        done,
        failed,
        report,
        notes,
    })
}

/// A lock's value; a lock poisoned is a fill thread that panicked holding it, and that panic
/// is this crate's failure (rule 20), named.
fn held<T>(value: LockResult<T>, what: &str) -> T {
    match value {
        Ok(value) => value,
        Err(_) => panic!("a fill thread panicked holding the {what} lock"),
    }
}

fn lower_modules<'a, P: Par>(
    krate: CrateItemKey,
    edition: Edition,
    cfg: &Cfg<'a, 'a>,
    set: &FileSet<'a>,
    tree: &Tree<'a, 'a>,
    listed: Option<&'a [ProcExport<'a>]>,
    notes: &mut AVec<'a, NamesNote<'a>>,
    par: &P,
    alloc: Alloc<'a>,
) -> Lowering<'a> {
    let mut parts = AVec::with_capacity_in(tree.modules.len(), alloc);
    par.map(
        tree.modules.len(),
        |i| {
            let m = &tree.modules[i];
            let file = set.files[m.file as usize];
            let fragment = Fragment {
                module: m.key,
                owner: m.key,
                text: file.text,
                outline: &file.outline,
                items: m.item,
                at: Where::File(m.file),
                depth: 0,
                made: false,
                members_of: None,
            };
            // The root's fragment (the walk's first module) declares the module's list.
            let procs = match listed {
                None => ProcDecls::Text,
                Some(list) => ProcDecls::Module { here: if i == 0 { list } else { &[] } },
            };
            lower(LowerCx { krate, edition, cfg, walked: tree, procs }, fragment, alloc)
        },
        &mut parts,
    );
    let mut out = Lowering {
        decls: AVec::with_capacity_in(parts.len(), alloc),
        tables: Tables::new_in(alloc),
        prelude: None,
        macro_use: AVec::new_in(alloc),
        macro_exports: AVec::new_in(alloc),
        bodies: AVec::new_in(alloc),
    };
    for (i, mut part) in parts.into_iter().enumerate() {
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
    out
}

/// The crate root's plan from the graph and the finished crates.
fn root_plan<'a>(
    graph: &CrateGraph<'a, &'a str>,
    id: CrateId,
    attrs: RootAttrs,
    built: &Built<'_, 'a>,
    notes: &mut AVec<'a, NamesNote<'a>>,
    alloc: Alloc<'a>,
) -> RootPlan<'a, 'a> {
    let externs = graph.externs(id, attrs);
    let krate = graph.get(id).name;
    notes.extend(externs.dropped.iter().map(|d| NamesNote::ExternDropped { krate, name: d.name, why: d.why }));
    let root_of = |c: CrateId| built.at(c.index()).map(|f| (f.key(), f.root()));
    let mut extern_prelude = AVec::new_in(alloc);
    for d in externs.prelude.iter() {
        if let Some((_, root)) = root_of(d.krate) {
            extern_prelude.push((d.name, root));
        }
    }
    let mut deps = AVec::new_in(alloc);
    for d in externs.deps.iter() {
        if let Some((krate, root)) = root_of(d.krate) {
            deps.push(blaze_surface::Dep { name: d.name, krate, root });
        }
    }
    let macro_use = externs.macro_use.and_then(|c| root_of(c).map(|it| it.1));
    // rustc's injected `#[macro_use] extern crate std;` (`core` under `#![no_std]`) is an item
    // of the root: `crate::std` names it, and in 2015 `use std::..` reads it there.
    let injected = externs.macro_use.filter(|c| root_of(*c).is_some()).map(|_| if attrs.no_std { "core" } else { "std" });
    let prelude = externs.std_prelude.map(|p| {
        let mut why = AString::new_in(alloc);
        let Some(other) = built.at(p.krate.index()) else {
            push_fmt(&mut why, format_args!("crate {} not finished", p.krate.0));
            return Err(&*Box::leak(why.into_boxed_str()));
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
                    return Err(&*Box::leak(why.into_boxed_str()));
                }
            }
        }
        Ok(at)
    });
    RootPlan { extern_prelude, deps, macro_use, prelude, injected }
}

fn push_fmt(out: &mut AString<'_>, args: fmt::Arguments<'_>) {
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
    built: &'r Built<'r, 'a>,
    sources: &'r S,
    /// A macro crate whose module's list declared its macros: an expansion declares none.
    listed: bool,
    /// The crate's expansions kept between solves ([`crate::expansions`]): asked and told by
    /// [`expand_decls`] alone.
    kept: &'r crate::expansions::KeptExpansions<'r, 'a>,
    alloc: Alloc<'a>,
}

impl<'a, S> Base<'_, 'a, S> {
    /// Where an expansion's procedural macros are declared from: never from its text when
    /// the module's list declared the crate's.
    fn expansion_procs(&self) -> ProcDecls<'a> {
        if self.listed { ProcDecls::Module { here: &[] } } else { ProcDecls::Text }
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
        self.base.tables.enums.contains_key(&of) || held(self.run.tables.read(), "tables").enums.contains_key(&of)
    }

    fn variant(&self, of: ItemKey, ns: Namespace, name: &'a str) -> crate::cells::DepAnswer {
        let answer = |variants: &[Variant<&str>]| match variant_answer(variants, ns, name) {
            NameAnswer::Found { def, vis, .. } => crate::cells::DepAnswer::Found(def, vis),
            NameAnswer::Absent => crate::cells::DepAnswer::NotFound,
        };
        if let Some(v) = self.base.tables.enums.get(&of) {
            return answer(v);
        }
        match held(self.run.tables.read(), "tables").enums.get(&of) {
            Some(v) => answer(v),
            None => crate::cells::DepAnswer::Unknown,
        }
    }
}

impl<'a, S> ScopeFacts for Facts<'_, 'a, S> {
    type Name = &'a str;

    fn module(&self, module: ItemKey) -> Result<ModuleInfo<&'a str>, SurfaceError> {
        if let Some(it) = self.base.decls.get(&module) {
            return Ok(it.0);
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
}

/// [`crate::task::Par::expand_part`] as [`expand_decls`] takes it.
type PartMark<'r> = &'r dyn Fn(ExpandPart, &str, usize);

impl<'a, S: SourceTexts<'a>> crate::task::Expand for FillCx<'_, 'a, S> {
    fn expand(&self, request: ExpandRequest) -> crate::task::Expanded {
        (self.mark)(crate::task::Stage::Expand);
        (self.part)(ExpandPart::Find, "", 0);
        let (decls, made) = expand_decls(self.base, self.run, request, self.part, self.seen).unwrap_or_else(|| (DeclsBuilder::new_in(self.base.alloc), None));
        (self.part)(ExpandPart::Store, "", 0);
        held(self.run.expansions.lock(), "expansions").insert(request.call, Expansion { decls, made });
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
        let Some(other) = self.base.built.of(key) else { return Held::Unknown };
        if let Some(scope) = other.scope(key) {
            return Held::Scope { scope, krate: other.key(), chain: other.chain(key) };
        }
        if let Some(v) = other.enum_of(key) {
            return Held::Variants { variants: AVec::from_slice_in(v, alloc), krate: other.key() };
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
        // A proc macro is declared by the lowering of its crate's text, never by an expansion,
        // so this crate's lowered tables or the defining crate's hold it.
        let src = match self.base.tables.macros.get(&derive) {
            Some(src) => *src,
            None => {
                let src = self.base.built.of(derive).and_then(|d| d.macro_of(derive));
                self.base.built.ask(|| crate::asked::Question::macro_src(derive, src, self.base.alloc));
                src?
            }
        };
        match src.body {
            MacroBody::Proc { kind: ProcKind::Derive, helpers } => Some(helpers),
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
    file: &'a str,
    /// The calling file's path (absolute): where `include!`'s path starts.
    path: &'a str,
    /// The package's folder: the one folder an `include!` may read in (no build script output
    /// is held here: `OUT_DIR` is no environment this host gives).
    package: &'a str,
    /// The calling crate's index.
    krate: usize,
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
        Ok(self.cfg.eval(&render_with(predicate, None, |_| None, |_| false, self.alloc)).unwrap_or(false))
    }

    /// The Reference ("Macros", built-in `include!`; core's `include_str!`): "The file is
    /// located relative to the current file (similarly to how modules are found)": the calling
    /// file's folder joined with the written path (`blaze_expand::include_target`), read from
    /// the crate's texts. These texts hold the package alone, so the bound here is the package
    /// (`SourceOrigin::Package`); a member's item-position read under its source root (D60)
    /// needs the root from `SourceTexts`, not given yet.
    fn include(&self, written: &str, _: IncludeAs) -> Result<Included<'_>, HostError> {
        // The one place the path is made: the modules the file writes are found from it too.
        let seen = self.sources.crate_env(self.krate).map(|env| env.seen());
        let Some(path) = crate::included::included_file(self.package, seen, self.path, written, self.alloc) else {
            return Ok(Included::Outside);
        };
        Ok(self.sources.read(self.krate, &path).map_or(Included::Missing, |text| Included::Bytes(text.as_bytes())))
    }

    fn place(&self, what: PlaceOf) -> Result<HostPlace<'_>, HostError> {
        Ok(match what {
            PlaceOf::Line | PlaceOf::Column => HostPlace::Number(1),
            PlaceOf::File => HostPlace::Text(self.file),
            PlaceOf::ModulePath => HostPlace::Text(""),
        })
    }

    fn run_proc_macro<'b>(&self, def: &MacroDefinition<'_>, call: ProcCall<'_>, alloc: Alloc<'b>) -> Result<Produced<'b>, HostError> {
        self.sources.run_proc_macro(self.krate, def, call, alloc)
    }
}

/// A token stream as source text: a space between tokens unless the first is joint; a
/// `$crate` at token `at` written for the crate `dollar(at)` names ([`DollarCrate`]) when that
/// is another crate than `here`, the one expanding, or when it sits inside a macro definition
/// the expansion writes (`macro_rules! m { .. }`, `macro m { .. }`), else `crate`; invisible
/// groups dropped.
///
/// The Reference, "Macros By Example", "Hygiene": `$crate` names the crate that defines the
/// macro wherever the macro is called, so a definition keeps it: alloy-primitives'
/// `fixed_bytes_macros!` writes `address!`, whose body calls `$crate::hex!`; written as
/// `crate::hex!` a call from another crate would name the caller's `hex`.
///
/// A token `def_crate(at)` says begins a single-name call the body of a `local_inner_macros`
/// macro wrote ([`blaze_expand::PathScope::DefCrate`]) is rendered with the defining crate's
/// `$crate::` before it. The Reference, "Macros By Example", "Hygiene": "Additionally, the
/// `local_inner_macros` key [of `macro_export`] can be used to automatically prefix all
/// contained macro invocations with `$crate::`."
fn render_with<'a>(
    stream: &TokenStream<'_>,
    here: Option<CrateItemKey>,
    dollar: impl Fn(usize) -> Option<CrateItemKey>,
    def_crate: impl Fn(usize) -> bool,
    alloc: Alloc<'a>,
) -> AString<'a> {
    let tokens = stream.tokens();
    let mut out = AString::new_in(alloc);
    let mut glued = true;
    let mut at = 0;
    // Groups open now; the depth a definition's body opened at while inside one; how far the
    // tokens read so far are into `macro_rules ! name` (1, 2, 3) or `macro name` (2, 3).
    let mut depth = 0u32;
    let mut in_def: Option<u32> = None;
    let mut def_head = 0u8;
    // A `$crate` no expansion wrote (tokio's `doc! { macro_rules! try_join { .. $crate .. } }`:
    // in the call's own tokens) names the crate whose text holds it, the one expanding.
    let crate_of = |out: &mut AString<'_>, krate: Option<CrateItemKey>, in_def: bool| match krate.or(here) {
        Some(k) if in_def || Some(k) != here => push_fmt(out, format_args!("{}", DollarCrate(k))),
        _ => out.push_str("crate"),
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
            at += 1;
            continue;
        }
        if !glued {
            out.push(' ');
        }
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
        if def_crate(at) {
            crate_of(&mut out, dollar(at), in_def.is_some());
            out.push_str("::");
        }
        match token.kind {
            TokenKind::Punct { char: '$', .. } if tokens.get(at + 1).is_some_and(|next| stream.text_of(next) == "crate") => {
                at += 1;
                crate_of(&mut out, dollar(at), in_def.is_some());
            }
            TokenKind::Ident { .. } if text == "$crate" => crate_of(&mut out, dollar(at), in_def.is_some()),
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
        glued = !matches!(token.kind.spacing(), Spacing::Alone);
        at += 1;
    }
    out
}

/// Expands one call the fill asked for: its declarations and text, or `None` (noted) when it
/// adds nothing.
fn expand_decls<'a, S: SourceTexts<'a>>(
    base: &Base<'_, 'a, S>,
    run: &Run<'a>,
    request: ExpandRequest,
    part: PartMark<'_>,
    seen: &dyn Fn(ItemKey, &str),
) -> Option<(DeclsBuilder<'a, &'a str>, Option<&'a ExpansionText<'a>>)> {
    let alloc = base.alloc;
    let call = request.call;
    let src = base.tables.calls.get(&call).copied().or_else(|| held(run.tables.read(), "tables").calls.get(&call).copied());
    let Some(src) = src else {
        run.note(NamesNote::Expand { call, at: None, note: ExpandNote::UnknownCall });
        return None;
    };
    let note = |note: ExpandNote<'a>| run.note(NamesNote::Expand { call, at: Some(src.loc), note });
    let def_key = request.def.key;
    let mac = base.tables.macros.get(&def_key).copied().or_else(|| held(run.tables.read(), "tables").macros.get(&def_key).copied());
    // A macro of another crate: its definition there, its `$crate` and edition that crate's.
    let (mac, def_krate, def_edition, runner) = match mac {
        Some(mac) => (Some(mac), base.key, base.edition, base.sources.proc_macro_key(base.index)),
        None => match base.built.of(def_key) {
            Some(other) => {
                let mac = other.macro_of(def_key);
                base.built.ask(|| crate::asked::Question::macro_src(def_key, mac, alloc));
                (mac, other.key(), other.edition(), other.proc_key())
            }
            None => (None, base.key, base.edition, None),
        },
    };
    let Some(mac) = mac else {
        note(ExpandNote::NoDefinition { shape: src.kind, name: src.name });
        return None;
    };
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
    let file = held(base_files(run), "files").files[src.loc.file as usize];
    let host = || Host {
        cfg: base.cfg,
        file: shown(file.path, base.shown_from),
        path: file.path,
        package: base.shown_from,
        krate: base.index,
        sources: base.sources,
        alloc,
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
            MacroBody::BuiltinOther(CallShape::Derive) => src.kind == CallShape::Derive,
            MacroBody::Proc { .. } => runner.is_some(),
            MacroBody::BuiltinFn(_) | MacroBody::BuiltinOther(_) => false,
        };
    part(ExpandPart::Define, mac.name, 0);
    let written;
    let def: Kept<'_> = match run.prepares.get(def_key).filter(|_| declarative) {
        Some(kept) => kept,
        None => match definition_of(mac, def_krate, runner, def_edition, src.kind, alloc) {
            Ok(def) if declarative => run.prepares.keep(def_key, def),
            Ok(def) => {
                written = def;
                Kept::Written(&written)
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
    // is the item, lexed as text of its own.
    let input = match src.kind {
        CallShape::FnLike => match blaze_expand::lex_inside(src.input, edition, alloc) {
            Ok(stream) => stream,
            Err(error) => {
                note(ExpandNote::Lex { macro_name: mac.name, what: Lexed::Input, error });
                return None;
            }
        },
        CallShape::Attr | CallShape::Derive => lexed(src.input, Lexed::Input)?,
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
    let (args, item): (Option<TokenStream<'a>>, TokenStream<'a>);
    let derive_facts;
    let call_of = match src.kind {
        CallShape::FnLike => Call::fn_like(&input, site),
        CallShape::Attr => {
            let mut walk = ItemAttrs::new(input);
            let attr = loop {
                match walk.next(atom) {
                    Ok(NextAttr::Call { path, attr }) if same_path(&path, src.name) => break attr,
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
                Ok(None) => return Some((DeclsBuilder::new_in(alloc), None)),
                Err(error) => {
                    item_note(error);
                    return None;
                }
            };
            let Some(used) = derives.uses.iter().find(|u| same_path(&u.path, src.name)) else {
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
    let call_key = keyed.then(|| match kept.planted() {
        crate::expansions::Planted::None => blaze_expand::call_key(def.def(), &call_of),
        crate::expansions::Planted::KeyWithoutTheDefinition => {
            blaze_expand::call_key_of(blaze_expand::MacroContent(blaze_id::ContentId([0; 32])), def.def().site(), &call_of)
        }
    });
    // A kept expansion is taken when its read list is answered today as it was: each cfg atom
    // the item walk asked, then each read the expansion made.
    let mut found: Option<blaze_expand::Expansion<'a>> = None;
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
    let eager = EagerDriver { macro_name: mac.name, def_at: nested_at, shadowed: &shadowed, alloc };
    let run_part = match mac.body {
        MacroBody::Rules(_) | MacroBody::Macro2 { .. } => ExpandPart::RunRules,
        MacroBody::BuiltinFn(_) => ExpandPart::RunBuiltin,
        MacroBody::BuiltinOther(_) => ExpandPart::RunDerive,
        MacroBody::Proc { .. } => ExpandPart::RunProc,
    };
    part(run_part, mac.name, input_tokens);
    let expanded = match base.script {
        Some(out) => eager.expand(def, &call_of, &ScriptCx { inner: host(), out }),
        None => eager.expand(def, &call_of, &host()),
    };
    let expansion = match expanded {
        Ok(expansion) => expansion,
        Err(why) => {
            note(why);
            return None;
        }
    };
    // Kept for the crate's next solve, unless a read of it cannot be asked again.
    match call_key {
        Some(key) => {
            part(ExpandPart::Keep, mac.name, expansion.tokens.len());
            let entry = match expansion.reads.askable() {
                Ok(()) if asked_fits => blaze_expand::kept::write_entry(&asked, &expansion, alloc).ok(),
                _ => None,
            };
            match entry {
                Some(entry) => kept.made(key, entry.leak()),
                None => kept.unkept(),
            }
        }
        None if kept.keeps() => kept.unkept(),
        None => {}
    }
    expansion
    }
    };
    for diagnostic in expansion.diagnostics.iter() {
        note(ExpandNote::Diagnostic { macro_name: mac.name, diagnostic: *diagnostic });
    }
    let here = base.key;
    let mut def_crate_starts: AVec<'_, u32> = AVec::new_in(alloc);
    def_crate_starts.extend(expansion.nested.iter().filter(|c| c.scope == PathScope::DefCrate).map(|c| c.path.start));
    def_crate_starts.sort_unstable();
    let def_crate = |at: usize| def_crate_starts.binary_search(&(at as u32)).is_ok();
    part(ExpandPart::Render, mac.name, expansion.tokens.len());
    let rendered = render_with(&expansion.tokens, Some(here), |at| expansion.hygiene(at).mark.map(|mark| mark.site.krate), def_crate, alloc);
    let text: &'a str = Box::leak(rendered.into_boxed_str());
    part(ExpandPart::Outline, mac.name, text.len());
    let made: &'a ExpansionText<'a> = Box::leak(Box::new_in(ExpansionText { text, outline: outline(text, edition, alloc) }, alloc));
    part(ExpandPart::Lower, mac.name, text.len());
    let (text, made): (&'a str, &'a ExpansionText<'a>) = match made.outline.errors().first() {
        None => (text, made),
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
                .is_some_and(|m| m.info.name.is_some_and(|name| name.starts_with(crate::blocks::BLOCK_MODULE)));
            if !in_block {
                note(ExpandNote::NotItems { macro_name: mac.name, message: made.outline.message(error) });
                (text, made)
            } else {
                match crate::expanded_blocks::statements_as_items(text, call, alloc) {
                    Ok(None) => return Some((DeclsBuilder::new_in(alloc), None)),
                    Ok(Some(items)) => {
                        let items: &'a str = Box::leak(items.into_boxed_str());
                        let again: &'a ExpansionText<'a> = Box::leak(Box::new_in(ExpansionText { text: items, outline: outline(items, edition, alloc) }, alloc));
                        if let Some(error) = again.outline.errors().first() {
                            note(ExpandNote::NotItems { macro_name: mac.name, message: again.outline.message(error) });
                            return Some((DeclsBuilder::new_in(alloc), None));
                        }
                        (items, again)
                    }
                    Err(refused) => {
                        note(ExpandNote::NotItems { macro_name: mac.name, message: refused.message() });
                        return Some((DeclsBuilder::new_in(alloc), None));
                    }
                }
            }
        }
    };
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
    };
    let mut out = lower(LowerCx { krate: base.key, edition: base.edition, cfg: base.cfg, walked: base.tree, procs: base.expansion_procs() }, fragment, alloc);
    // Entered only by a call that makes modules: one that makes none stays in `Lower` for the
    // few moves below (what the part costs is what `make_modules` does: a module's file read,
    // outlined and lowered).
    if !out.mods_made.is_empty() {
        part(ExpandPart::Modules, mac.name, out.mods_made.len());
    }
    let mods =core::mem::replace(&mut out.mods_made, AVec::new_in(alloc));
    // An `include!`'s file is the current file of what it writes ([`crate::included`]): a
    // `mod x;` in it is found from the INCLUDED file's folder, not the calling module's.
    let included_dir: Option<&'a str> = match mac.body {
        MacroBody::BuiltinFn("include") if !mods.is_empty() => {
            let seen = base.sources.crate_env(base.index).map(|env| env.seen());
            crate::included::included_dir(base.shown_from, seen, file.path, &expansion.reads, alloc).map(|dir| keep(&dir, alloc))
        }
        _ => None,
    };
    let place = Made { text, outline: &made.outline, at: Where::Expansion(src.loc), place: At::Expansion(call), dir: included_dir };
    let mut bodies = core::mem::replace(&mut out.bodies, AVec::new_in(alloc));
    make_modules(base, run, src.module, &mods, place, src.depth + 1, &mut bodies);
    let decls = core::mem::replace(&mut out.decls, DeclsBuilder::new_in(alloc));
    part(ExpandPart::Tables, mac.name, 0);
    add_lowered(run, call, At::Expansion(call), out);
    part(ExpandPart::Blocks, mac.name, bodies.len());
    make_blocks(base, run, &bodies);
    Some((decls, Some(made)))
}

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
fn make_blocks<'a, S: SourceTexts<'a>>(base: &Base<'_, 'a, S>, run: &Run<'a>, bodies: &[crate::task::BodySrc<'a>]) {
    let alloc = base.alloc;
    let mut work: AVec<'a, crate::task::BodySrc<'a>> = AVec::from_slice_in(bodies, alloc);
    while let Some(body) = work.pop() {
        let nested = blaze_file_items::nested::scan(body.text, alloc);
        // An unbalanced body is the parser's error to report; nothing is read from it.
        // A block is a module when it holds an item that makes one (its head is made in the
        // block's module by the ordinary code; front2's `Locals::keep` derives the same module
        // key with `blocks::block_modules`) or a statement call (what it declares is known
        // once it is expanded: crate::expanded_blocks).
        if nested.end != blaze_file_items::nested::NestedEnd::Whole || (!nested.makes_module() && nested.calls.is_empty()) {
            continue;
        }
        if body.depth >= MAX_BLOCK_DEPTH {
            run.note(NamesNote::Made { name: crate::blocks::BLOCK_MODULE, at: body.loc, note: MadeNote::TooMany });
            continue;
        }
        let rendered = crate::blocks::module_text(body.text, &nested, alloc);
        let text: &'a str = Box::leak(rendered.into_boxed_str());
        let made: &'a ExpansionText<'a> = Box::leak(Box::new_in(ExpansionText { text, outline: outline(text, base.edition, alloc) }, alloc));
        let call = crate::task::key_of(base.key, body.key, "blocks", "", 0);
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
        };
        let mut out = lower(LowerCx { krate: base.key, edition: base.edition, cfg: base.cfg, walked: base.tree, procs: base.expansion_procs() }, fragment, alloc);
        let mods = core::mem::replace(&mut out.mods_made, AVec::new_in(alloc));
        let place = Made { text, outline: &made.outline, at: Where::Expansion(body.loc), place: At::Expansion(call), dir: None };
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
        held(run.expansions.lock(), "expansions").insert(call, Expansion { decls: DeclsBuilder::new_in(alloc), made: Some(made) });
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
struct EagerDriver<'s, 'a> {
    macro_name: &'a str,
    /// Where the builtins the nested calls name are defined.
    def_at: DefAt,
    shadowed: &'s dyn Fn(&str) -> bool,
    alloc: Alloc<'a>,
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

impl<'a> EagerDriver<'_, 'a> {
    fn refused(&self, nested: &str, why: EagerRefusal) -> ExpandNote<'a> {
        let mut text = AString::new_in(self.alloc);
        text.push_str(nested);
        ExpandNote::Eager { macro_name: self.macro_name, nested: Box::leak(text.into_boxed_str()), why }
    }

    /// `call` of `def`, its nested calls expanded first when it is eager.
    fn expand(&self, def: Kept<'_>, call: &Call<'_>, cx: &impl ExpandCx) -> Result<blaze_expand::Expansion<'a>, ExpandNote<'a>> {
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
    fn answers(&self, outer: &TokenStream<'_>, nested: AVec<'a, NestedCall>, site: CallSite, cx: &impl ExpandCx) -> Result<AVec<'a, TokenStream<'a>>, ExpandNote<'a>> {
        let alloc = self.alloc;
        let mut root: AVec<'a, TokenStream<'a>> = AVec::new_in(alloc);
        let mut stack: AVec<'a, EagerFrame<'a>> = AVec::new_in(alloc);
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
                        if expansion.diagnostics.iter().any(Diagnostic::is_error) {
                            return Err(self.refused(name, EagerRefusal::Failed));
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
        MacroBody::BuiltinOther(CallShape::Derive) if shape == CallShape::Derive => {
            match BuiltinDerive::ALL.into_iter().find(|it| it.name() == mac.name) {
                Some(which) => MacroDefinition::builtin_derive(which, def_at, alloc),
                None => return Err(ExpandNote::BuiltinUnknown { name: mac.name }),
            }
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

/// Whether two paths as written are one (`a :: b` and `a::b`): spaces are not part of a path.
fn same_path(a: &str, b: &str) -> bool {
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
}

/// Where a module of this crate is: its children's directory, what their `#[path]` is relative
/// to, its chain up to the root, and its path; walked or made by a macro.
fn place_of<'a, S>(base: &Base<'_, 'a, S>, run: &Run<'a>, module: ItemKey) -> Option<(&'a str, &'a str, AVec<'a, ItemKey>, &'a str)> {
    let alloc = base.alloc;
    if let Some(&i) = base.tree.by_key.get(&module) {
        let m = &base.tree.modules[i];
        let chain = AVec::from_slice_in(base.chains.get(&module)?, alloc);
        return Some((keep(&m.dir, alloc), keep(&m.path_dir, alloc), chain, keep(&m.path, alloc)));
    }
    let made = held(run.made.read(), "made modules");
    made.get(&module).map(|it| (it.dir, it.path_dir, it.chain.clone(), it.path))
}

/// The file at `path`, read and outlined the first time a module names it.
fn file_at<'a, S: SourceTexts<'a>>(base: &Base<'_, 'a, S>, run: &Run<'a>, path: &str) -> Option<(u32, &'a SourceFile<'a>)> {
    let mut files = held(base_files(run), "files");
    if let Some(at) = files.find(path) {
        return Some((at, files.files[at as usize]));
    }
    let path = keep(path, base.alloc);
    let text = base.sources.read(base.index, path)?;
    let file = file_of(path, text, outline(text, base.edition, base.alloc), base.shown_from, base.alloc);
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
            let d = keep(&joined(dir, &[m.path_attr.unwrap_or(m.name)], alloc), alloc);
            (made, Some(m.item), d, d)
        } else {
            let sub = keep(&joined(dir, &[m.name], alloc), alloc);
            let (file, mdir, mpdir) = if let Some(p) = m.path_attr {
                let target = joined(path_dir, &[p], alloc);
                let mut parent_dir = target.clone();
                pop_dir(&mut parent_dir);
                let parent_dir = keep(&parent_dir, alloc);
                let Some(file) = file_at(base, run, &target) else {
                    note(MadeNote::FileNotFound { path: keep(&target, alloc) });
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
                            note(MadeNote::FileNotFound { path: keep(&b, alloc) });
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
            (Made { text: file.text, outline: &file.outline, at: Where::File(at), place: At::File(at), dir: None }, None, mdir, mpdir)
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
        };
        let mut out = lower(LowerCx { krate: base.key, edition: base.edition, cfg: base.cfg, walked: base.tree, procs: base.expansion_procs() }, fragment, alloc);
        bodies.extend(out.bodies.iter().copied());
        let mut chain = AVec::with_capacity_in(parent_chain.len() + 1, alloc);
        chain.push(m.key);
        chain.extend_from_slice(&parent_chain);
        let mut path = AString::from_str_in(parent_path, alloc);
        path.push_str("::");
        path.push_str(m.name);
        let module = MadeModule {
            info: ModuleInfo { krate: base.key, parent: Some(parent), name: Some(m.name), no_implicit_prelude: m.no_implicit_prelude },
            decls: core::mem::replace(&mut out.decls, DeclsBuilder::new_in(alloc)),
            chain,
            path: Box::leak(path.into_boxed_str()),
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
