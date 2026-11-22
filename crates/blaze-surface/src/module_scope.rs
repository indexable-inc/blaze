//! [`ModuleScope`]: a module's resolved scope, read per name (docs/v2/plan/design/bottom.md,
//! "Name resolution without resolving a crate"); and [`ScopeBuilder`], the same scope as a
//! working form, which a name resolver fills and [`ScopeBuilder::write`] writes.
//!
//! It is found by its own `ModuleDecls` and is valid by its read list (read-addressed), never
//! keyed by the scopes it imports from. A reader asks one `(namespace, name)` ([`ScopeView::get`])
//! or the module's glob edges ([`ScopeView::globs`], read with [`globs_digest`]), and its read
//! list holds exactly that, so an item added to a module moves no reader that did not ask for
//! its name. The read list itself is the store's, not this record's.
//!
//! The record holds everything the working form does, so [`ScopeBuilder::from`] reads back an
//! equal scope: the module's own bindings with their origin, depth and state, its glob edges
//! (a name reached through a glob is found at lookup, never copied), the errors of user code, the
//! module's `macro_rules!` sequence and its children's marks in it, the `use .. as _` traits
//! and the macro-expanded `#[macro_export]`s. Nothing of how it was solved is in it (a cycle's
//! members are solved together, and which modules that was depends on the schedule): two
//! builders of one module write equal bytes whatever their order.

use core::fmt;
use core::mem::offset_of;

use blaze_format::{FormatError, Record, Slice, Writer, WriterOpts};
use blaze_id::{ContentId, CrateItemKey, ItemKey, Portable};
use blaze_own::{AVec, Alloc, Vec};

use crate::column::{
    Column, NONE, RecordBytes, Strings, Text, at_of, at_of_record, check_increasing, check_text,
    finish_with, key_of_word, key_word, text_of,
};
use crate::error::BuildError;
use crate::kind::{
    AmbiguityKind, Binding, BindingState, Def, DefKind, DefKindTag, MacroShapes, Namespace, Origin,
    ScopeEntry, StateTag, Visibility, VisibilityTag, byte_enum,
};
use crate::module_decls::NO_TEXT;
use crate::path::{Path, PathRow, PathTable, PathView, Paths};

mod in_place;
pub use in_place::Globs;

/// One `macro_rules!` of a textual scope, in source order (later shadows earlier).
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct TextualDef<N> {
    pub name: N,
    pub def: ItemKey,
    /// Expansion depth it was defined at (0 in source).
    pub expansion: u16,
    /// The call shapes its arms are written for.
    pub shapes: MacroShapes,
}

/// Where a child module's `mod` line falls in its parent's textual sequence: the child's
/// textual entry is the parent's entry followed by the first `len` defs of
/// [`ScopeBuilder::textual`].
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct TextualMark {
    pub child: ItemKey,
    pub len: u32,
}

/// A resolved `use path::*;` of a scope: the module or enum it imports from, the visibility of
/// what it imports (the narrower of this and each name's own), and how deep in macro
/// expansions it was written. A name reached through it is found at lookup, never copied
/// (docs/v2/plan/design/bottom.md, "Globs are edges, not copies").
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct GlobEdge {
    pub source: Def,
    pub vis: Visibility,
    pub expansion: u16,
}

/// A `use path as _;` that resolved: it binds no name, and brings its target (a trait) into
/// scope for method resolution.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct AnonImport {
    pub site: UseSite,
    pub def: Def,
    pub vis: Visibility,
}

/// A `use` item, named by the fragment it is written in (`None` for the module's own text, or
/// the macro call whose expansion wrote it) and its index among that fragment's `use`s.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct UseSite {
    pub expansion: Option<ItemKey>,
    pub index: u32,
}

/// Where an early resolution was made: an import or a macro call.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub enum Site {
    Import(UseSite),
    Call(ItemKey),
}

/// Why a path segment did not resolve.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub enum Unresolved {
    /// No binding of the name in any scope searched.
    NotFound,
    /// A binding exists and the importing module may not name it.
    Private,
    /// The segment before names something that is not a module or an enum.
    NotAModule,
    /// `super` above the crate root.
    NoParent,
    /// `$crate` of a crate that is not a dependency.
    NoCrate,
    /// The binding is ambiguous.
    Ambiguous(AmbiguityKind),
    /// A macro of the wrong call shape (a function-like macro used as an attribute, ...).
    WrongMacroKind,
    /// An attribute macro written after a derive on an item takes the item, and a `#[derive]`
    /// is written after the attribute on the same item: rustc runs that derive only on the
    /// macro's output; blaze sends it on the item as written (its helpers are in scope for the
    /// attributes before it), so it would run twice. Refused by name instead (attr-bang-kind-2,
    /// rule 3): move the `#[derive]` before the attribute macro, or into its output. The error's
    /// path is the attribute macro's, its place the item's.
    /// Only the `AttrOrder::DerivesAtOnce` control reaches this refusal.
    DeriveAfterAttrMacro,
}

byte_enum! {
    /// [`Unresolved`]'s byte in a record; an ambiguity's kind is a field beside it.
    pub(crate) enum UnresolvedTag, refused as "a byte that is no unresolved reason" {
        NotFound = 0 => "not found",
        Private = 1 => "private",
        NotAModule = 2 => "not a module",
        NoParent = 3 => "no parent",
        NoCrate = 4 => "no crate",
        Ambiguous = 6 => "ambiguous",
        WrongMacroKind = 7 => "wrong macro kind",
        DeriveAfterAttrMacro = 8 => "derive after attribute macro",
    }
}

impl Unresolved {
    /// The reason from its two record bytes (a finding of an `Imports` record); `None` for a
    /// byte that is no reason or no ambiguity kind.
    pub(crate) fn from_record_bytes(tag: u8, kind: u8) -> Option<Unresolved> {
        Some(Unresolved::from_record(UnresolvedTag::from_byte(tag)?, AmbiguityKind::from_byte(kind)?))
    }

    pub(crate) fn to_record(self) -> (UnresolvedTag, AmbiguityKind) {
        let none = AmbiguityKind::GlobVsGlob;
        match self {
            Unresolved::NotFound => (UnresolvedTag::NotFound, none),
            Unresolved::Private => (UnresolvedTag::Private, none),
            Unresolved::NotAModule => (UnresolvedTag::NotAModule, none),
            Unresolved::NoParent => (UnresolvedTag::NoParent, none),
            Unresolved::NoCrate => (UnresolvedTag::NoCrate, none),
            Unresolved::Ambiguous(kind) => (UnresolvedTag::Ambiguous, kind),
            Unresolved::WrongMacroKind => (UnresolvedTag::WrongMacroKind, none),
            Unresolved::DeriveAfterAttrMacro => (UnresolvedTag::DeriveAfterAttrMacro, none),
        }
    }

    fn from_record(tag: UnresolvedTag, kind: AmbiguityKind) -> Unresolved {
        match tag {
            UnresolvedTag::NotFound => Unresolved::NotFound,
            UnresolvedTag::Private => Unresolved::Private,
            UnresolvedTag::NotAModule => Unresolved::NotAModule,
            UnresolvedTag::NoParent => Unresolved::NoParent,
            UnresolvedTag::NoCrate => Unresolved::NoCrate,
            UnresolvedTag::Ambiguous => Unresolved::Ambiguous(kind),
            UnresolvedTag::WrongMacroKind => Unresolved::WrongMacroKind,
            UnresolvedTag::DeriveAfterAttrMacro => Unresolved::DeriveAfterAttrMacro,
        }
    }
}

/// Why a macro call's expansion was refused: what [`ScopeError::Refused`] holds, and what a
/// path that looks a name up behind it answers (blaze-scope `names::Why::Refused`).
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub enum RefusedWhy {
    /// `env!("OUT_DIR")` of a package whose build script's outputs are missing: cargo sets
    /// `OUT_DIR` for a package with a build script ("the folder in which all output and
    /// intermediate artifacts should be placed", the cargo book), and no outputs record is
    /// joined, so the script has not run here. The error's name is `OUT_DIR`.
    BuildScriptOutputs,
    /// An `env!` (or `option_env!`) of a variable this build could not answer; the error's name
    /// is the variable.
    UnknownEnv,
    /// Any other refusal (a definition not held, a lex error, a proc macro's failure); the
    /// error's name is the macro's path as written, the crate's notes hold the whole reason.
    Expansion,
}

impl RefusedWhy {
    /// What a name behind such a call is, in words.
    pub fn text(self) -> &'static str {
        match self {
            RefusedWhy::BuildScriptOutputs => {
                "unknown: a macro call that could write it was refused: `env!(\"OUT_DIR\")` of a package whose build script's outputs are missing (the build script has not run)"
            }
            RefusedWhy::UnknownEnv => "unknown: a macro call that could write it was refused: an `env!` of a variable this build could not answer",
            RefusedWhy::Expansion => "unknown: a macro call that could write it was refused (the crate's notes say why)",
        }
    }
}

/// An error of user code, kept in the scope as a value (a resolver never panics on one). `P`
/// is a path: [`Path`] in a working form, [`PathView`] read from a record.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum ScopeError<N, P> {
    /// A `use` did not resolve: its path, the index of the segment that failed, and why.
    Import { site: UseSite, path: P, segment: u32, why: Unresolved },
    /// A macro call's path did not resolve.
    Macro { call: ItemKey, path: P, segment: u32, why: Unresolved },
    /// An `extern crate` of a crate that is no dependency.
    ExternCrate { name: N },
    /// A `pub(in path)` whose path is not an ancestor of the module.
    RestrictedNotAncestor { path: P },
    /// Two different non-glob bindings of one name (rustc E0252, E0254, E0255, E0428).
    Duplicate { ns: Namespace, name: N, first: Def, second: Def },
    /// A `use` re-exports a binding wider than the binding itself is visible (E0364, E0365).
    WiderReexport { site: UseSite, ns: Namespace, name: N, def: Def },
    /// An early resolution (an import's or a macro path's first segment) that the final scope
    /// makes ambiguous.
    Ambiguous { site: Site, ns: Namespace, name: N, kind: AmbiguityKind, first: Def, second: Def },
    /// An early resolution that the final scope resolves to another target, with no rustc
    /// ambiguity kind to name it.
    Inconsistent { site: Site, ns: Namespace, name: N, early: Def, last: Def },
    /// A macro call nested deeper than the recursion limit.
    RecursionLimit { call: ItemKey },
    /// A macro call of the module whose expansion was refused: what it might have written is
    /// unknown, never absent. `name`: the variable for an `env!` refusal, else the macro's path.
    Refused { call: ItemKey, name: N, why: RefusedWhy },
}

impl<N, P> ScopeError<N, P> {
    /// The same error with each path through `f`.
    pub fn map_path<Q>(self, f: impl FnOnce(P) -> Q) -> ScopeError<N, Q> {
        use ScopeError as E;
        match self {
            E::Import { site, path, segment, why } => E::Import { site, path: f(path), segment, why },
            E::Macro { call, path, segment, why } => E::Macro { call, path: f(path), segment, why },
            E::ExternCrate { name } => E::ExternCrate { name },
            E::RestrictedNotAncestor { path } => E::RestrictedNotAncestor { path: f(path) },
            E::Duplicate { ns, name, first, second } => E::Duplicate { ns, name, first, second },
            E::WiderReexport { site, ns, name, def } => E::WiderReexport { site, ns, name, def },
            E::Ambiguous { site, ns, name, kind, first, second } => E::Ambiguous { site, ns, name, kind, first, second },
            E::Inconsistent { site, ns, name, early, last } => E::Inconsistent { site, ns, name, early, last },
            E::RecursionLimit { call } => E::RecursionLimit { call },
            E::Refused { call, name, why } => E::Refused { call, name, why },
        }
    }
}

/// A module's scope as a working form: what a resolver writes, and what a [`ModuleScope`]
/// record holds (docs/v2/plan/design/bottom.md).
///
/// `bindings` is sorted by namespace then name, one entry per pair; `marks` by child. `errors`
/// is in the order the module's text and expansions are written in.
#[derive(Clone, Debug)]
pub struct ScopeBuilder<'a, N> {
    pub module: ItemKey,
    /// The module's own bindings: no name reached through a glob is here.
    pub bindings: AVec<'a, ScopeEntry<N>>,
    /// The module's glob edges, sorted by source, one per source.
    pub globs: AVec<'a, GlobEdge>,
    pub errors: AVec<'a, ScopeError<N, Path<'a, N>>>,
    /// The `macro_rules!` defined in this module and in its `#[macro_use]` children, in the
    /// order they are visible: what a `#[macro_use]` on this module carries out to the parent
    /// (its `TextualOut`), and, by [`ScopeBuilder::marks`], each child's textual entry.
    pub textual: AVec<'a, TextualDef<N>>,
    /// For each `mod` child (sorted by child key), where its `mod` line falls in `textual`.
    pub marks: AVec<'a, TextualMark>,
    /// The `use .. as _` imports, in the module's tree order: the traits they bring into scope.
    /// (A glob does not carry them on yet.)
    pub traits: AVec<'a, AnonImport>,
    /// The `#[macro_export]` `macro_rules!` a macro expansion of this module wrote, in tree
    /// order. They are bindings of the crate root for other crates: the writer of the crate's
    /// records adds them to the root's answers once every module of the crate is done. In
    /// their own crate they are not reachable by path (rustc refuses that too).
    pub macro_exports: AVec<'a, TextualDef<N>>,
}

impl<'a, N> ScopeBuilder<'a, N> {
    /// The empty scope of `module`.
    pub fn new_in(module: ItemKey, alloc: Alloc<'a>) -> ScopeBuilder<'a, N> {
        ScopeBuilder {
            module,
            bindings: AVec::new_in(alloc),
            errors: AVec::new_in(alloc),
            textual: AVec::new_in(alloc),
            marks: AVec::new_in(alloc),
            globs: AVec::new_in(alloc),
            traits: AVec::new_in(alloc),
            macro_exports: AVec::new_in(alloc),
        }
    }
}

impl<N: Ord + Copy> ScopeBuilder<'_, N> {
    /// The binding of `name` in `ns`, by one binary search.
    pub fn get(&self, ns: Namespace, name: N) -> Option<&Binding> {
        self.bindings
            .binary_search_by(|e| (e.ns, e.name).cmp(&(ns, name)))
            .ok()
            .map(|at| &self.bindings[at].binding)
    }

    /// The glob edges, by source.
    pub fn globs(&self) -> &[GlobEdge] {
        &self.globs
    }

    /// The textual mark of `child`, if it is a child of this module.
    pub fn mark(&self, child: ItemKey) -> Option<u32> {
        self.marks.binary_search_by(|m| m.child.cmp(&child)).ok().map(|at| self.marks[at].len)
    }
}

impl<'b, N: PartialEq> PartialEq<ScopeBuilder<'b, N>> for ScopeBuilder<'_, N> {
    fn eq(&self, other: &ScopeBuilder<'b, N>) -> bool {
        self.module == other.module
            && self.bindings[..] == other.bindings[..]
            && self.globs[..] == other.globs[..]
            && self.errors.len() == other.errors.len()
            && self.errors.iter().zip(other.errors.iter()).all(|(a, b)| {
                a.clone_shape() == b.clone_shape() && a.path().zip(b.path()).is_none_or(|(p, q)| p == q)
            })
            && self.textual[..] == other.textual[..]
            && self.marks[..] == other.marks[..]
            && self.traits[..] == other.traits[..]
            && self.macro_exports[..] == other.macro_exports[..]
    }
}

impl<N, P> ScopeError<N, P> {
    /// Its path, if it has one.
    pub fn path(&self) -> Option<&P> {
        match self {
            ScopeError::Import { path, .. }
            | ScopeError::Macro { path, .. }
            | ScopeError::RestrictedNotAncestor { path } => Some(path),
            ScopeError::ExternCrate { .. }
            | ScopeError::Duplicate { .. }
            | ScopeError::WiderReexport { .. }
            | ScopeError::Ambiguous { .. }
            | ScopeError::Inconsistent { .. }
            | ScopeError::RecursionLimit { .. }
            | ScopeError::Refused { .. } => None,
        }
    }

    /// The error with its path left out: what two errors of differently allocated paths are
    /// compared by, beside their paths.
    fn clone_shape(&self) -> ScopeError<&N, ()> {
        use ScopeError as E;
        match self {
            E::Import { site, segment, why, .. } => E::Import { site: *site, path: (), segment: *segment, why: *why },
            E::Macro { call, segment, why, .. } => E::Macro { call: *call, path: (), segment: *segment, why: *why },
            E::ExternCrate { name } => E::ExternCrate { name },
            E::RestrictedNotAncestor { .. } => E::RestrictedNotAncestor { path: () },
            E::Duplicate { ns, name, first, second } => E::Duplicate { ns: *ns, name, first: *first, second: *second },
            E::WiderReexport { site, ns, name, def } => E::WiderReexport { site: *site, ns: *ns, name, def: *def },
            E::Ambiguous { site, ns, name, kind, first, second } => {
                E::Ambiguous { site: *site, ns: *ns, name, kind: *kind, first: *first, second: *second }
            }
            E::Inconsistent { site, ns, name, early, last } => {
                E::Inconsistent { site: *site, ns: *ns, name, early: *early, last: *last }
            }
            E::RecursionLimit { call } => E::RecursionLimit { call: *call },
            E::Refused { call, name, why } => E::Refused { call: *call, name, why: *why },
        }
    }
}

byte_enum! {
    /// [`ScopeError`]'s byte in a record.
    pub(crate) enum ErrorTag, refused as "a byte that is no scope error" {
        Import = 0 => "import",
        Macro = 1 => "macro",
        ExternCrate = 2 => "extern crate",
        RestrictedNotAncestor = 3 => "restricted to no ancestor",
        Duplicate = 4 => "duplicate",
        WiderReexport = 5 => "wider re-export",
        Ambiguous = 6 => "ambiguous",
        Inconsistent = 7 => "inconsistent",
        RecursionLimit = 8 => "recursion limit",
        RefusedScript = 9 => "refused: build script outputs",
        RefusedEnv = 10 => "refused: unknown env",
        RefusedExpansion = 11 => "refused expansion",
    }
}

byte_enum! {
    /// [`Site`]'s byte in a record, `None` for an error made at no site.
    pub(crate) enum SiteTag, refused as "a byte that is no resolution site" {
        None = 0 => "none",
        Import = 1 => "import",
        Call = 2 => "call",
    }
}

blaze_format::record! {
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub(crate) struct BindingRow: 1 {
        pub(crate) name: Text,
        pub(crate) def: ContentId,
        pub(crate) vis_key: ContentId,
        pub(crate) expansion: u16,
        pub(crate) pad: u16,
        pub(crate) ns: Namespace,
        pub(crate) origin: Origin,
        pub(crate) def_kind: DefKindTag,
        pub(crate) shape: MacroShapes,
        pub(crate) vis: VisibilityTag,
        pub(crate) state: StateTag,
        pub(crate) ambiguity: AmbiguityKind,
        pub(crate) pad2: u8,
    }
}

blaze_format::record! {
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub(crate) struct ErrorRow: 2 {
        pub(crate) name: Text,
        pub(crate) path: u32,
        pub(crate) segment: u32,
        pub(crate) site_index: u32,
        pub(crate) pad: u16,
        pub(crate) site_key: ContentId,
        pub(crate) first: ContentId,
        pub(crate) second: ContentId,
        pub(crate) tag: ErrorTag,
        pub(crate) site: SiteTag,
        pub(crate) ns: Namespace,
        pub(crate) why: UnresolvedTag,
        pub(crate) ambiguity: AmbiguityKind,
        pub(crate) first_kind: DefKindTag,
        pub(crate) first_shape: MacroShapes,
        pub(crate) second_kind: DefKindTag,
        pub(crate) second_shape: MacroShapes,
        pub(crate) pad2: u8,
    }
}

blaze_format::record! {
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub(crate) struct TextualRow: 1 {
        pub(crate) name: Text,
        pub(crate) def: ContentId,
        pub(crate) expansion: u16,
        pub(crate) shapes: MacroShapes,
        pub(crate) pad: u8,
    }
}

blaze_format::record! {
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub(crate) struct MarkRow: 1 {
        pub(crate) len: u32,
        pub(crate) child: ContentId,
    }
}

blaze_format::record! {
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub(crate) struct TraitRow: 1 {
        pub(crate) index: u32,
        pub(crate) expansion: ContentId,
        pub(crate) def: ContentId,
        pub(crate) vis_key: ContentId,
        pub(crate) def_kind: DefKindTag,
        pub(crate) shape: MacroShapes,
        pub(crate) vis: VisibilityTag,
        pub(crate) pad: u8,
    }
}

blaze_format::record! {
    /// One glob edge: the module or enum a `use ..::*` of the scope imports from.
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub(crate) struct GlobRow: 1 {
        pub(crate) source: ContentId,
        pub(crate) vis_key: ContentId,
        pub(crate) expansion: u16,
        pub(crate) def_kind: DefKindTag,
        pub(crate) shape: MacroShapes,
        pub(crate) vis: VisibilityTag,
        pub(crate) pad: u8,
    }
}

blaze_format::record! {
    /// Form 3: the bindings are the module's own (declared, named imports, extern crates,
    /// macro exports); a name reached through a glob is not copied, the glob is an edge. No
    /// fellows (form 2 named them): a record says nothing of how it was solved.
    #[derive(Clone, Copy, Debug)]
    pub(crate) struct ModuleScopeRoot: 3 {
        pub(crate) module: ContentId,
        pub(crate) bindings: Slice<BindingRow>,
        pub(crate) globs: Slice<GlobRow>,
        pub(crate) errors: Slice<ErrorRow>,
        pub(crate) textual: Slice<TextualRow>,
        pub(crate) marks: Slice<MarkRow>,
        pub(crate) traits: Slice<TraitRow>,
        pub(crate) macro_exports: Slice<TextualRow>,
        pub(crate) paths: Slice<PathRow>,
        pub(crate) segments: Slice<Text>,
        pub(crate) strings: Slice<u8>,
    }
}

const TABLE: &str = "ModuleScope";
const ZERO: ContentId = ContentId([0; 32]);

/// A def where a row has none: so two builders write equal bytes.
fn def_parts(def: Option<Def>) -> (ContentId, DefKindTag, MacroShapes) {
    match def {
        Some(def) => {
            let (kind, shape) = def.kind.to_record();
            (def.key.0, kind, shape)
        }
        None => (ZERO, DefKindTag::Other, MacroShapes::FN_LIKE),
    }
}

fn def_of(key: ContentId, kind: DefKindTag, shape: MacroShapes) -> Def {
    Def { key: ItemKey(key), kind: DefKind::from_record(kind, shape) }
}

fn site_parts(site: Option<Site>) -> (SiteTag, ContentId, u32) {
    match site {
        None => (SiteTag::None, ZERO, 0),
        Some(Site::Import(site)) => (SiteTag::Import, key_word(site.expansion), site.index),
        Some(Site::Call(call)) => (SiteTag::Call, call.0, 0),
    }
}

fn use_site_of(row: &ErrorRow) -> UseSite {
    UseSite { expansion: key_of_word(row.site_key), index: row.site_index }
}

/// The domain of a `Globs` digest.
pub const GLOBS_DOMAIN: &str = "blaze surface globs 2";

/// The digest of a module's glob edges (in order), its crate and its chain of modules from
/// itself up to its crate's root: everything a lookup through its edges reads of it besides
/// its names. A finished scope holds no undetermined binding, so there is none to digest.
pub fn globs_digest(edges: &[GlobEdge], krate: CrateItemKey, chain: &[ItemKey], alloc: Alloc<'_>) -> ContentId {
    let mut bytes = AVec::new_in(alloc);
    for e in edges {
        let (kind, shape) = e.source.kind.to_record();
        let (vis, vis_key) = e.vis.to_record();
        bytes.extend_from_slice(&e.source.key.0.0);
        bytes.extend_from_slice(&[kind.byte(), shape.byte(), vis.byte()]);
        bytes.extend_from_slice(&vis_key.0);
        bytes.extend_from_slice(&e.expansion.to_le_bytes());
    }
    bytes.push(0xff);
    bytes.extend_from_slice(&krate.0.0);
    for m in chain {
        bytes.extend_from_slice(&m.0.0);
    }
    ContentId::of(GLOBS_DOMAIN, &bytes)
}

/// The domain of a `TextualEntry` or `TextualOut` digest.
pub const TEXTUAL_DOMAIN: &str = "blaze surface textual 2";

/// The digest of a textual scope (`macro_rules!` in the order they are visible): what a
/// `TextualEntry` or `TextualOut` read saw. One function for every answerer, so a read list
/// written by one driver holds against another's answers.
pub fn textual_digest<N: AsRef<str>>(defs: &[TextualDef<N>], alloc: Alloc<'_>) -> ContentId {
    let mut bytes = AVec::new_in(alloc);
    for d in defs {
        let name = d.name.as_ref();
        bytes.extend_from_slice(&u64::try_from(name.len()).expect("a name fits a u64").to_le_bytes());
        bytes.extend_from_slice(name.as_bytes());
        bytes.extend_from_slice(&d.def.0.0);
        bytes.extend_from_slice(&d.expansion.to_le_bytes());
        bytes.push(d.shapes.byte());
    }
    ContentId::of(TEXTUAL_DOMAIN, &bytes)
}

impl<N: AsRef<str> + Copy> ScopeBuilder<'_, N> {
    /// The [`ModuleScope`] record of this scope, in `alloc`. The bindings are sorted by
    /// namespace and name and the marks by child, so the order they are in moves no byte; two
    /// bindings of one slot are refused.
    pub fn write<'b>(&self, alloc: Alloc<'b>) -> Result<RecordBytes<'b>, BuildError> {
        self.write_sorted(true, alloc)
    }

    /// [`ScopeBuilder::write`], with the planted bug of the order test: `sort` off.
    pub(crate) fn write_sorted<'b>(&self, sort: bool, alloc: Alloc<'b>) -> Result<RecordBytes<'b>, BuildError> {
        let mut strings = Strings::new_in(alloc);
        let mut paths = PathTable::new_in(alloc);

        let mut order = Vec::from_iter_in(0..self.bindings.len(), alloc);
        let bindings = &self.bindings;
        let slot = |at: &usize| (bindings[*at].ns, bindings[*at].name.as_ref());
        if sort {
            order.sort_unstable_by(|a, b| slot(a).cmp(&slot(b)));
        }
        if let Some(pair) = order.windows(2).find(|pair| slot(&pair[0]) == slot(&pair[1])) {
            return Err(BuildError::Twice { table: "ModuleScope bindings", key: bindings[pair[1]].binding.def.key.0 });
        }
        let mut rows = Vec::with_capacity_in(order.len(), alloc);
        for &at in order.iter() {
            let e = bindings[at];
            let (def, def_kind, shape) = def_parts(Some(e.binding.def));
            let (vis, vis_key) = e.binding.vis.to_record();
            let (state, ambiguity) = e.binding.state.to_record();
            rows.push(BindingRow {
                name: strings.push(e.name.as_ref(), TABLE)?,
                def,
                vis_key,
                expansion: e.binding.expansion,
                pad: 0,
                ns: e.ns,
                origin: e.binding.origin,
                def_kind,
                shape,
                vis,
                state,
                ambiguity,
                pad2: 0,
            });
        }

        let mut errors = Vec::with_capacity_in(self.errors.len(), alloc);
        for error in self.errors.iter() {
            errors.push(error_row(error, &mut paths, &mut strings)?);
        }
        let textual = textual_rows(&self.textual, &mut strings, alloc)?;
        let macro_exports = textual_rows(&self.macro_exports, &mut strings, alloc)?;

        let mut globs = Vec::with_capacity_in(self.globs.len(), alloc);
        for edge in self.globs.iter() {
            let (source, def_kind, shape) = def_parts(Some(edge.source));
            let (vis, vis_key) = edge.vis.to_record();
            globs.push(GlobRow { source, vis_key, expansion: edge.expansion, def_kind, shape, vis, pad: 0 });
        }
        if sort {
            globs.sort_unstable_by_key(|row| row.source);
        }
        if let Some(pair) = globs.windows(2).find(|pair| pair[0].source == pair[1].source) {
            return Err(BuildError::Twice { table: "ModuleScope globs", key: pair[1].source });
        }
        let mut marks = Vec::from_iter_in(self.marks.iter().map(|m| MarkRow { len: m.len, child: m.child.0 }), alloc);
        if sort {
            marks.sort_unstable_by_key(|m| m.child);
        }
        if let Some(pair) = marks.windows(2).find(|pair| pair[0].child == pair[1].child) {
            return Err(BuildError::Twice { table: "ModuleScope marks", key: pair[1].child });
        }
        let traits = Vec::from_iter_in(
            self.traits.iter().map(|t| {
                let (def, def_kind, shape) = def_parts(Some(t.def));
                let (vis, vis_key) = t.vis.to_record();
                TraitRow {
                    index: t.site.index,
                    expansion: key_word(t.site.expansion),
                    def,
                    vis_key,
                    def_kind,
                    shape,
                    vis,
                    pad: 0,
                }
            }),
            alloc,
        );

        let mut out = Writer::new(WriterOpts { schema: <ModuleScopeRoot as Record>::SCHEMA, alloc: alloc, capacity: None });
        let text = Column::push(&mut out, strings.bytes());
        let bindings = Column::push(&mut out, &rows);
        let globs = Column::push(&mut out, &globs);
        let errors = Column::push(&mut out, &errors);
        let textual = Column::push(&mut out, &textual);
        let marks = Column::push(&mut out, &marks);
        let traits = Column::push(&mut out, &traits);
        let macro_exports = Column::push(&mut out, &macro_exports);
        let path_rows = Column::push(&mut out, &paths.rows);
        let segments = Column::push(&mut out, &paths.segments);
        Ok(finish_with(out, |at| ModuleScopeRoot {
            module: self.module.0,
            bindings: bindings.slice(at, offset_of!(ModuleScopeRoot, bindings)),
            globs: globs.slice(at, offset_of!(ModuleScopeRoot, globs)),
            errors: errors.slice(at, offset_of!(ModuleScopeRoot, errors)),
            textual: textual.slice(at, offset_of!(ModuleScopeRoot, textual)),
            marks: marks.slice(at, offset_of!(ModuleScopeRoot, marks)),
            traits: traits.slice(at, offset_of!(ModuleScopeRoot, traits)),
            macro_exports: macro_exports.slice(at, offset_of!(ModuleScopeRoot, macro_exports)),
            paths: path_rows.slice(at, offset_of!(ModuleScopeRoot, paths)),
            segments: segments.slice(at, offset_of!(ModuleScopeRoot, segments)),
            strings: text.slice(at, offset_of!(ModuleScopeRoot, strings)),
        }))
    }
}

fn textual_rows<'b, N: AsRef<str>>(
    defs: &[TextualDef<N>],
    strings: &mut Strings<'_>,
    alloc: Alloc<'b>,
) -> Result<AVec<'b, TextualRow>, BuildError> {
    let mut rows = AVec::with_capacity_in(defs.len(), alloc);
    for d in defs {
        rows.push(TextualRow { name: strings.push(d.name.as_ref(), TABLE)?, def: d.def.0, expansion: d.expansion, shapes: d.shapes, pad: 0 });
    }
    Ok(rows)
}

fn error_row<N: AsRef<str>>(
    error: &ScopeError<N, Path<'_, N>>,
    paths: &mut PathTable<'_>,
    strings: &mut Strings<'_>,
) -> Result<ErrorRow, BuildError> {
    use ScopeError as E;
    let (tag, site, name, ns, why, ambiguity, first, second) = match error {
        E::Import { site, why, .. } => (ErrorTag::Import, Some(Site::Import(*site)), None, Namespace::Types, Some(*why), None, None, None),
        E::Macro { call, why, .. } => (ErrorTag::Macro, Some(Site::Call(*call)), None, Namespace::Types, Some(*why), None, None, None),
        E::ExternCrate { name } => (ErrorTag::ExternCrate, None, Some(name), Namespace::Types, None, None, None, None),
        E::RestrictedNotAncestor { .. } => (ErrorTag::RestrictedNotAncestor, None, None, Namespace::Types, None, None, None, None),
        E::Duplicate { ns, name, first, second } => {
            (ErrorTag::Duplicate, None, Some(name), *ns, None, None, Some(*first), Some(*second))
        }
        E::WiderReexport { site, ns, name, def } => {
            (ErrorTag::WiderReexport, Some(Site::Import(*site)), Some(name), *ns, None, None, Some(*def), None)
        }
        E::Ambiguous { site, ns, name, kind, first, second } => {
            (ErrorTag::Ambiguous, Some(*site), Some(name), *ns, None, Some(*kind), Some(*first), Some(*second))
        }
        E::Inconsistent { site, ns, name, early, last } => {
            (ErrorTag::Inconsistent, Some(*site), Some(name), *ns, None, None, Some(*early), Some(*last))
        }
        E::RecursionLimit { call } => (ErrorTag::RecursionLimit, Some(Site::Call(*call)), None, Namespace::Types, None, None, None, None),
        E::Refused { call, name, why } => {
            let tag = match why {
                RefusedWhy::BuildScriptOutputs => ErrorTag::RefusedScript,
                RefusedWhy::UnknownEnv => ErrorTag::RefusedEnv,
                RefusedWhy::Expansion => ErrorTag::RefusedExpansion,
            };
            (tag, Some(Site::Call(*call)), Some(name), Namespace::Types, None, None, None, None)
        }
    };
    let segment = match error {
        E::Import { segment, .. } | E::Macro { segment, .. } => *segment,
        _ => 0,
    };
    let (why, why_kind) = why.map_or((UnresolvedTag::NotFound, AmbiguityKind::GlobVsGlob), Unresolved::to_record);
    let ambiguity = ambiguity.unwrap_or(why_kind);
    let (site, site_key, site_index) = site_parts(site);
    let (first, first_kind, first_shape) = def_parts(first);
    let (second, second_kind, second_shape) = def_parts(second);
    let name = match name {
        Some(name) => strings.push(name.as_ref(), TABLE)?,
        None => NO_TEXT,
    };
    let path = paths.push_opt(error.path(), strings, TABLE)?;
    Ok(ErrorRow {
        name,
        path,
        segment,
        site_index,
        pad: 0,
        site_key,
        first,
        second,
        tag,
        site,
        ns,
        why,
        ambiguity,
        first_kind,
        first_shape,
        second_kind,
        second_shape,
        pad2: 0,
    })
}

impl<'a, 'b> ScopeBuilder<'b, &'a str> {
    /// The working form of a [`ModuleScope`] record, its names borrowed from the record, in
    /// `alloc`: equal to the scope that wrote it.
    pub fn from(view: &ScopeView<'a>, alloc: Alloc<'b>) -> ScopeBuilder<'b, &'a str> {
        let mut out = ScopeBuilder::new_in(view.module(), alloc);
        out.bindings.extend(view.bindings());
        out.globs.extend(view.globs());
        out.errors.extend(view.errors().map(|e| e.map_path(|p| p.to_path_in(alloc))));
        out.textual.extend(view.textual());
        out.marks.extend(view.marks());
        out.traits.extend(view.traits());
        out.macro_exports.extend(view.macro_exports());
        out
    }
}

/// A [`ModuleScope`] record read in place.
#[derive(Clone, Copy, Debug)]
pub struct ScopeView<'a> {
    module: ItemKey,
    rows: &'a [BindingRow],
    globs: &'a [GlobRow],
    errors: &'a [ErrorRow],
    textual: &'a [TextualRow],
    marks: &'a [MarkRow],
    traits: &'a [TraitRow],
    macro_exports: &'a [TextualRow],
    paths: Paths<'a>,
    strings: &'a str,
}

impl<'a> ScopeView<'a> {
    /// The record in `buffer`, checked once: its layout, every name and path, and that its
    /// bindings are in strictly increasing order of namespace and name, which
    /// [`ScopeView::get`] probes by (its glob edges and marks likewise by their keys).
    pub fn read(buffer: &'a [u8]) -> Result<ScopeView<'a>, FormatError> {
        let root = blaze_format::open::<ModuleScopeRoot>(buffer)?;
        let strings = root.strings.as_str(buffer)?;
        let paths = Paths::read(root.paths.follow(buffer)?, root.segments.follow(buffer)?, strings, buffer)?;
        let view = ScopeView {
            module: ItemKey(root.module),
            rows: root.bindings.follow(buffer)?,
            globs: root.globs.follow(buffer)?,
            errors: root.errors.follow(buffer)?,
            textual: root.textual.follow(buffer)?,
            marks: root.marks.follow(buffer)?,
            traits: root.traits.follow(buffer)?,
            macro_exports: root.macro_exports.follow(buffer)?,
            paths,
            strings,
        };
        for row in view.rows {
            check_text(strings, row.name, row, buffer)?;
        }
        check_increasing(view.rows, buffer, "bindings out of order or two of one slot", |row| {
            (row.ns, text_of(strings, row.name))
        })?;
        check_increasing(view.globs, buffer, "glob edges out of order or two of one source", |row| row.source)?;
        for row in view.errors {
            check_text(strings, row.name, row, buffer)?;
            let has_path = matches!(row.tag, ErrorTag::Import | ErrorTag::Macro | ErrorTag::RestrictedNotAncestor);
            match has_path {
                true => paths.check(row.path, false, row, buffer)?,
                false if row.path == NONE => {}
                false => {
                    return Err(FormatError::Invalid { at: at_of_record(row, buffer), what: "a path on an error that has none" });
                }
            }
        }
        for row in view.textual.iter().chain(view.macro_exports) {
            check_text(strings, row.name, row, buffer)?;
        }
        check_increasing(view.marks, buffer, "textual marks out of order or repeated", |row| row.child)?;
        Ok(view)
    }

    pub fn module(&self) -> ItemKey {
        self.module
    }

    fn decode(&self, row: &BindingRow) -> ScopeEntry<&'a str> {
        ScopeEntry {
            ns: row.ns,
            name: text_of(self.strings, row.name),
            binding: Binding {
                def: def_of(row.def, row.def_kind, row.shape),
                vis: Visibility::from_record(row.vis, row.vis_key),
                origin: row.origin,
                expansion: row.expansion,
                state: BindingState::from_record(row.state, row.ambiguity),
            },
        }
    }

    /// The binding of `name` in `ns`, by one binary search. A probe compares the namespace and
    /// the first eight bytes of the names as one word each (blaze-format's `rows::prefix_in`,
    /// one load from the string table), and the names' bytes only where those tie
    /// (`rows::order_by_prefix`: the same order as `str`'s, so the same probes and the same
    /// answer as comparing `(ns, &str)`): most names differ in their first eight bytes, so
    /// `memcmp` runs about once a lookup, where it ran once a probe, and no probe re-checks a
    /// name's character boundaries, which `read` checked once (rule 74).
    pub fn get(&self, ns: Namespace, name: &str) -> Option<Binding> {
        let strings = self.strings.as_bytes();
        let (want, name) = (blaze_format::rows::prefix(name.as_bytes()), name.as_bytes());
        let at = self
            .rows
            .binary_search_by(|row| {
                row.ns.cmp(&ns).then_with(|| {
                    // Inside `strings`: `read` checked every binding's name (`check_text`).
                    let range = row.name.range();
                    let probe_prefix = blaze_format::rows::prefix_in(strings, range.start, range.len());
                    blaze_format::rows::order_by_prefix(probe_prefix, &strings[range], want, name)
                })
            })
            .ok()?;
        Some(self.decode(&self.rows[at]).binding)
    }

    /// Every binding, by namespace and name.
    pub fn bindings(&self) -> impl ExactSizeIterator<Item = ScopeEntry<&'a str>> + use<'a> {
        let view = *self;
        self.rows.iter().map(move |row| view.decode(row))
    }

    /// The glob edges, by source.
    pub fn globs(&self) -> impl ExactSizeIterator<Item = GlobEdge> + use<'a> {
        self.globs.iter().map(|row| GlobEdge {
            source: def_of(row.source, row.def_kind, row.shape),
            vis: Visibility::from_record(row.vis, row.vis_key),
            expansion: row.expansion,
        })
    }


    /// The errors of user code, in the order the module's text and expansions are written in.
    pub fn errors(&self) -> impl ExactSizeIterator<Item = ScopeError<&'a str, PathView<'a>>> + use<'a> {
        let (strings, paths) = (self.strings, self.paths);
        self.errors.iter().map(move |row| {
            use ScopeError as E;
            let name = text_of(strings, row.name);
            let why = Unresolved::from_record(row.why, row.ambiguity);
            let first = def_of(row.first, row.first_kind, row.first_shape);
            let second = def_of(row.second, row.second_kind, row.second_shape);
            let site = match row.site {
                SiteTag::Call => Site::Call(ItemKey(row.site_key)),
                SiteTag::Import | SiteTag::None => Site::Import(use_site_of(row)),
            };
            match row.tag {
                ErrorTag::Import => E::Import { site: use_site_of(row), path: paths.get(row.path), segment: row.segment, why },
                ErrorTag::Macro => E::Macro { call: ItemKey(row.site_key), path: paths.get(row.path), segment: row.segment, why },
                ErrorTag::ExternCrate => E::ExternCrate { name },
                ErrorTag::RestrictedNotAncestor => E::RestrictedNotAncestor { path: paths.get(row.path) },
                ErrorTag::Duplicate => E::Duplicate { ns: row.ns, name, first, second },
                ErrorTag::WiderReexport => E::WiderReexport { site: use_site_of(row), ns: row.ns, name, def: first },
                ErrorTag::Ambiguous => E::Ambiguous { site, ns: row.ns, name, kind: row.ambiguity, first, second },
                ErrorTag::Inconsistent => E::Inconsistent { site, ns: row.ns, name, early: first, last: second },
                ErrorTag::RecursionLimit => E::RecursionLimit { call: ItemKey(row.site_key) },
                ErrorTag::RefusedScript => E::Refused { call: ItemKey(row.site_key), name, why: RefusedWhy::BuildScriptOutputs },
                ErrorTag::RefusedEnv => E::Refused { call: ItemKey(row.site_key), name, why: RefusedWhy::UnknownEnv },
                ErrorTag::RefusedExpansion => E::Refused { call: ItemKey(row.site_key), name, why: RefusedWhy::Expansion },
            }
        })
    }

    /// The module's `macro_rules!` sequence, in the order they are visible.
    pub fn textual(&self) -> impl ExactSizeIterator<Item = TextualDef<&'a str>> + use<'a> {
        textual_of(self.strings, self.textual)
    }

    /// Where each `mod` child's line falls in [`ScopeView::textual`], by child.
    pub fn marks(&self) -> impl ExactSizeIterator<Item = TextualMark> + use<'a> {
        self.marks.iter().map(|row| TextualMark { child: ItemKey(row.child), len: row.len })
    }

    /// The textual mark of `child`, by one binary search.
    pub fn mark(&self, child: ItemKey) -> Option<u32> {
        let at = self.marks.binary_search_by(|row| row.child.cmp(&child.0)).ok()?;
        Some(self.marks[at].len)
    }

    /// The `use .. as _` imports, in the module's tree order.
    pub fn traits(&self) -> impl ExactSizeIterator<Item = AnonImport> + use<'a> {
        self.traits.iter().map(|row| AnonImport {
            site: UseSite { expansion: key_of_word(row.expansion), index: row.index },
            def: def_of(row.def, row.def_kind, row.shape),
            vis: Visibility::from_record(row.vis, row.vis_key),
        })
    }

    /// The `#[macro_export]` `macro_rules!` this module's expansions wrote, in tree order.
    pub fn macro_exports(&self) -> impl ExactSizeIterator<Item = TextualDef<&'a str>> + use<'a> {
        textual_of(self.strings, self.macro_exports)
    }
}

fn textual_of<'a>(strings: &'a str, rows: &'a [TextualRow]) -> impl ExactSizeIterator<Item = TextualDef<&'a str>> + use<'a> {
    rows.iter().map(move |row| TextualDef { name: text_of(strings, row.name), def: ItemKey(row.def), expansion: row.expansion, shapes: row.shapes })
}

/// `ScopeView::get`'s prefix-first probe against its scalar twin, the probe it replaced.
#[cfg(test)]
mod prefix_probe {
    use blaze_format::AlignedIn;
    use blaze_id::{ContentId, ItemKey};
    use blaze_sim::{Broke, Config, Tape};

    use super::{ScopeBuilder, ScopeView};
    use crate::kind::{Binding, BindingState, Def, DefKind, Namespace, Origin, ScopeEntry, Visibility};

    impl ScopeView<'_> {
        /// The twin: the probe `get` made before, `(ns, &str)` compared whole at every step.
        fn get_whole(&self, ns: Namespace, name: &str) -> Option<Binding> {
            let strings = self.strings;
            let at = self.rows.binary_search_by(|row| (row.ns, super::text_of(strings, row.name)).cmp(&(ns, name))).ok()?;
            Some(self.decode(&self.rows[at]).binding)
        }
    }

    /// A name of up to twelve characters from a small alphabet with a two-byte one in it: names
    /// share their first eight bytes often, end inside another's prefix, and some are empty.
    fn drawn_name(tape: &Tape) -> String {
        tape.vec(0..=12, "name", |tape| ['a', 'b', '_', 'é'][tape.index(4, "char")]).into_iter().collect()
    }

    /// Every drawn binding is found by both probes, and every drawn query (bound or not, in each
    /// namespace) gets one answer from both. Prints nothing.
    #[test]
    fn get_answers_what_the_whole_name_probe_does() {
        let heap = blaze_own::testing::heap().alloc();
        blaze_sim::check(Config::cases(400), |tape| {
            let names: Vec<String> = tape.vec(0..=40, "names", drawn_name);
            let spaces = [Namespace::Types, Namespace::Values, Namespace::Macros];
            let mut builder = ScopeBuilder::<&str>::new_in(ItemKey(ContentId([9; 32])), heap);
            let mut bound: Vec<(Namespace, &str)> = Vec::new();
            for (n, name) in names.iter().enumerate() {
                let ns = spaces[tape.index(3, "ns")];
                if bound.contains(&(ns, name.as_str())) {
                    continue;
                }
                bound.push((ns, name.as_str()));
                let key = ItemKey(ContentId([u8::try_from(n).expect("at most 40 names"); 32]));
                let binding = Binding { def: Def { key, kind: DefKind::Other }, vis: Visibility::Public, origin: Origin::Declared, expansion: 0, state: BindingState::Determined };
                builder.bindings.push(ScopeEntry { ns, name: name.as_str(), binding });
            }
            let bytes = builder.write_sorted(true, heap).map_err(|why| Broke::Oracle(format!("{why:?}")))?;
            let buffer = AlignedIn::copy_from(&bytes, heap);
            let view = ScopeView::read(&buffer).map_err(|why| Broke::Oracle(format!("{why}")))?;
            let mut queries: Vec<String> = tape.vec(0..=12, "queries", drawn_name);
            queries.extend(names.iter().cloned());
            for ns in spaces {
                for query in &queries {
                    let (fast, whole) = (view.get(ns, query), view.get_whole(ns, query));
                    if fast != whole {
                        return Err(Broke::Oracle(format!("{ns:?} {query:?}: prefix-first {fast:?}, whole {whole:?}")));
                    }
                    if bound.contains(&(ns, query.as_str())) != fast.is_some() {
                        return Err(Broke::Oracle(format!("{ns:?} {query:?}: bound is not found, or unbound is")));
                    }
                }
            }
            Ok(())
        });
    }
}
