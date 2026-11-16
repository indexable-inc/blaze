//! [`ModuleScope`]: a module's resolved scope, read per name (docs/v2/plan/design/bottom.md,
//! "Name resolution without resolving a crate"); and [`ScopeBuilder`], the same scope as a
//! working form, which a name resolver fills and [`ScopeBuilder::write`] writes.
//!
//! It is found by its own `ModuleDecls` and is valid by its read list (read-addressed), never
//! keyed by the scopes it imports from. A reader asks one `(namespace, name)` ([`ScopeView::get`])
//! or the digest of one [`VisibilityClass`] a glob reads ([`ScopeView::exports`]), and its read
//! list holds exactly that, so a private fn added to a module moves no reader that did not ask
//! for its name and no glob outside it. The read list itself is the store's, not this record's.
//!
//! The record holds everything the working form does, so [`ScopeBuilder::from`] reads back an
//! equal scope: the bindings with their origin, depth and state, the errors of user code, the
//! module's `macro_rules!` sequence and its children's marks in it, the fellows of its group,
//! the `use .. as _` traits and the macro-expanded `#[macro_export]`s.

use core::alloc::Allocator;
use core::fmt;
use core::mem::offset_of;

use blaze_format::{FormatError, Record, Slice, Writer};
use blaze_id::{ContentId, ItemKey, Portable};
use blaze_own::Vec;

use crate::column::{
    Column, NONE, RecordBytes, Strings, Text, at_of, at_of_record, check_increasing, check_text,
    finish_with, key_of_word, key_word, text_of,
};
use crate::error::BuildError;
use crate::kind::{
    AmbiguityKind, Binding, BindingState, CallShape, Def, DefKind, DefKindTag, Namespace, Origin,
    ScopeEntry, StateTag, Visibility, VisibilityClass, VisibilityTag, byte_enum,
};
use crate::module_decls::NO_TEXT;
use crate::path::{Path, PathRow, PathTable, PathView, Paths};

/// The domain of an `Exports` digest.
pub const EXPORTS_DOMAIN: &str = "blaze surface exports 2";

/// One entry of an `Exports` read: a binding a glob can import, by namespace and name.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct ExportEntry<N> {
    pub ns: Namespace,
    pub name: N,
    pub def: Def,
    pub vis: Visibility,
    pub state: BindingState,
}

/// One `macro_rules!` of a textual scope, in source order (later shadows earlier).
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct TextualDef<N> {
    pub name: N,
    pub def: ItemKey,
    /// Expansion depth it was defined at (0 in source).
    pub expansion: u16,
}

/// Where a child module's `mod` line falls in its parent's textual sequence: the child's
/// textual entry is the parent's entry followed by the first `len` defs of
/// [`ScopeBuilder::textual`].
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct TextualMark {
    pub child: ItemKey,
    pub len: u32,
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
    /// Still undetermined when nothing could move any more: a cycle of imports that only
    /// justify each other, or a macro whose path depends on its own expansion.
    Undetermined,
    /// The binding is ambiguous.
    Ambiguous(AmbiguityKind),
    /// A macro of the wrong call shape (a function-like macro used as an attribute, ...).
    WrongMacroKind,
}

byte_enum! {
    /// [`Unresolved`]'s byte in a record; an ambiguity's kind is a field beside it.
    pub(crate) enum UnresolvedTag, refused as "a byte that is no unresolved reason" {
        NotFound = 0 => "not found",
        Private = 1 => "private",
        NotAModule = 2 => "not a module",
        NoParent = 3 => "no parent",
        NoCrate = 4 => "no crate",
        Undetermined = 5 => "undetermined",
        Ambiguous = 6 => "ambiguous",
        WrongMacroKind = 7 => "wrong macro kind",
    }
}

impl Unresolved {
    fn to_record(self) -> (UnresolvedTag, AmbiguityKind) {
        let none = AmbiguityKind::GlobVsGlob;
        match self {
            Unresolved::NotFound => (UnresolvedTag::NotFound, none),
            Unresolved::Private => (UnresolvedTag::Private, none),
            Unresolved::NotAModule => (UnresolvedTag::NotAModule, none),
            Unresolved::NoParent => (UnresolvedTag::NoParent, none),
            Unresolved::NoCrate => (UnresolvedTag::NoCrate, none),
            Unresolved::Undetermined => (UnresolvedTag::Undetermined, none),
            Unresolved::Ambiguous(kind) => (UnresolvedTag::Ambiguous, kind),
            Unresolved::WrongMacroKind => (UnresolvedTag::WrongMacroKind, none),
        }
    }

    fn from_record(tag: UnresolvedTag, kind: AmbiguityKind) -> Unresolved {
        match tag {
            UnresolvedTag::NotFound => Unresolved::NotFound,
            UnresolvedTag::Private => Unresolved::Private,
            UnresolvedTag::NotAModule => Unresolved::NotAModule,
            UnresolvedTag::NoParent => Unresolved::NoParent,
            UnresolvedTag::NoCrate => Unresolved::NoCrate,
            UnresolvedTag::Undetermined => Unresolved::Undetermined,
            UnresolvedTag::Ambiguous => Unresolved::Ambiguous(kind),
            UnresolvedTag::WrongMacroKind => Unresolved::WrongMacroKind,
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
        }
    }
}

/// A module's scope as a working form: what a resolver writes, and what a [`ModuleScope`]
/// record holds (docs/v2/plan/design/bottom.md).
///
/// `bindings` is sorted by namespace then name, one entry per pair; `marks` by child and
/// `fellows` by key. `errors` is in the order the module's text and expansions are written in.
/// A member of a cycle names its fellows.
#[derive(Clone, Debug)]
pub struct ScopeBuilder<N, A: Allocator> {
    pub module: ItemKey,
    pub bindings: Vec<ScopeEntry<N>, A>,
    pub errors: Vec<ScopeError<N, Path<N, A>>, A>,
    /// The `macro_rules!` defined in this module and in its `#[macro_use]` children, in the
    /// order they are visible: what a `#[macro_use]` on this module carries out to the parent
    /// (its `TextualOut`), and, by [`ScopeBuilder::marks`], each child's textual entry.
    pub textual: Vec<TextualDef<N>, A>,
    /// For each `mod` child (sorted by child key), where its `mod` line falls in `textual`.
    pub marks: Vec<TextualMark, A>,
    pub fellows: Vec<ItemKey, A>,
    /// The `use .. as _` imports, in the module's tree order: the traits they bring into scope.
    /// (A glob does not carry them on yet.)
    pub traits: Vec<AnonImport, A>,
    /// The `#[macro_export]` `macro_rules!` a macro expansion of this module wrote, in tree
    /// order. They are bindings of the crate root for other crates: the writer of the crate's
    /// records adds them to the root's answers once every module of the crate is done. In
    /// their own crate they are not reachable by path (rustc refuses that too).
    pub macro_exports: Vec<TextualDef<N>, A>,
}

impl<N, A: Allocator + Clone> ScopeBuilder<N, A> {
    /// The empty scope of `module`.
    pub fn new_in(module: ItemKey, alloc: A) -> ScopeBuilder<N, A> {
        ScopeBuilder {
            module,
            bindings: Vec::new_in(alloc.clone()),
            errors: Vec::new_in(alloc.clone()),
            textual: Vec::new_in(alloc.clone()),
            marks: Vec::new_in(alloc.clone()),
            fellows: Vec::new_in(alloc.clone()),
            traits: Vec::new_in(alloc.clone()),
            macro_exports: Vec::new_in(alloc),
        }
    }
}

impl<N: Ord + Copy, A: Allocator> ScopeBuilder<N, A> {
    /// The binding of `name` in `ns`, by one binary search.
    pub fn get(&self, ns: Namespace, name: N) -> Option<&Binding> {
        self.bindings
            .binary_search_by(|e| (e.ns, e.name).cmp(&(ns, name)))
            .ok()
            .map(|at| &self.bindings[at].binding)
    }

    /// The bindings of one visibility class, in scope order: what an `Exports` read of this
    /// scope answers, and what the record hashes into the class's digest.
    pub fn exports(&self, class: VisibilityClass) -> impl Iterator<Item = ExportEntry<N>> + '_ {
        self.bindings.iter().filter(move |e| e.binding.vis.class() == class).map(|e| export_of(*e))
    }

    /// The textual mark of `child`, if it is a child of this module.
    pub fn mark(&self, child: ItemKey) -> Option<u32> {
        self.marks.binary_search_by(|m| m.child.cmp(&child)).ok().map(|at| self.marks[at].len)
    }
}

fn export_of<N>(e: ScopeEntry<N>) -> ExportEntry<N> {
    ExportEntry { ns: e.ns, name: e.name, def: e.binding.def, vis: e.binding.vis, state: e.binding.state }
}

impl<N: PartialEq, A: Allocator, B: Allocator> PartialEq<ScopeBuilder<N, B>> for ScopeBuilder<N, A> {
    fn eq(&self, other: &ScopeBuilder<N, B>) -> bool {
        self.module == other.module
            && self.bindings[..] == other.bindings[..]
            && self.errors.len() == other.errors.len()
            && self.errors.iter().zip(other.errors.iter()).all(|(a, b)| {
                a.clone_shape() == b.clone_shape() && a.path().zip(b.path()).is_none_or(|(p, q)| p == q)
            })
            && self.textual[..] == other.textual[..]
            && self.marks[..] == other.marks[..]
            && self.fellows[..] == other.fellows[..]
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
            | ScopeError::RecursionLimit { .. } => None,
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
        pub(crate) shape: CallShape,
        pub(crate) vis: VisibilityTag,
        pub(crate) state: StateTag,
        pub(crate) ambiguity: AmbiguityKind,
        pub(crate) pad2: u8,
    }
}

blaze_format::record! {
    /// One visibility class a binding of the scope is in, and the digest of its bindings.
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub(crate) struct ExportRow: 1 {
        pub(crate) module: ContentId,
        pub(crate) digest: ContentId,
        pub(crate) class: VisibilityTag,
    }
}

blaze_format::record! {
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub(crate) struct ErrorRow: 1 {
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
        pub(crate) first_shape: CallShape,
        pub(crate) second_kind: DefKindTag,
        pub(crate) second_shape: CallShape,
        pub(crate) pad2: u8,
    }
}

blaze_format::record! {
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub(crate) struct TextualRow: 1 {
        pub(crate) name: Text,
        pub(crate) def: ContentId,
        pub(crate) expansion: u16,
        pub(crate) pad: u16,
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
        pub(crate) shape: CallShape,
        pub(crate) vis: VisibilityTag,
        pub(crate) pad: u8,
    }
}

blaze_format::record! {
    #[derive(Clone, Copy, Debug)]
    pub(crate) struct ModuleScopeRoot: 1 {
        pub(crate) module: ContentId,
        pub(crate) bindings: Slice<BindingRow>,
        pub(crate) exports: Slice<ExportRow>,
        pub(crate) errors: Slice<ErrorRow>,
        pub(crate) textual: Slice<TextualRow>,
        pub(crate) marks: Slice<MarkRow>,
        pub(crate) fellows: Slice<ContentId>,
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
fn def_parts(def: Option<Def>) -> (ContentId, DefKindTag, CallShape) {
    match def {
        Some(def) => {
            let (kind, shape) = def.kind.to_record();
            (def.key.0, kind, shape)
        }
        None => (ZERO, DefKindTag::Other, CallShape::FnLike),
    }
}

fn def_of(key: ContentId, kind: DefKindTag, shape: CallShape) -> Def {
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

/// The digest of one class's exports, in scope order: everything a glob of that class reads.
pub fn exports_digest<N: AsRef<str>, B: Allocator>(
    entries: impl Iterator<Item = ExportEntry<N>>,
    alloc: B,
) -> ContentId {
    let mut bytes = Vec::new_in(alloc);
    for e in entries {
        let name = e.name.as_ref();
        let (def, kind, shape) = def_parts(Some(e.def));
        let (vis, vis_key) = e.vis.to_record();
        let (state, ambiguity) = e.state.to_record();
        bytes.push(e.ns.byte());
        bytes.extend_from_slice(&u64::try_from(name.len()).expect("a name fits a u64").to_le_bytes());
        bytes.extend_from_slice(name.as_bytes());
        bytes.extend_from_slice(&def.0);
        bytes.extend_from_slice(&[kind.byte(), shape.byte(), vis.byte(), state.byte(), ambiguity.byte()]);
        bytes.extend_from_slice(&vis_key.0);
    }
    ContentId::of(EXPORTS_DOMAIN, &bytes)
}

impl<N: AsRef<str> + Copy, A: Allocator> ScopeBuilder<N, A> {
    /// The [`ModuleScope`] record of this scope, in `alloc`. The bindings are sorted by
    /// namespace and name, the marks by child and the fellows by key, so the order they are
    /// in moves no byte; two bindings of one slot are refused.
    pub fn write<B: Allocator + Clone>(&self, alloc: B) -> Result<RecordBytes<B>, BuildError> {
        self.write_sorted(true, alloc)
    }

    /// [`ScopeBuilder::write`], with the planted bug of the order test: `sort` off.
    pub(crate) fn write_sorted<B: Allocator + Clone>(&self, sort: bool, alloc: B) -> Result<RecordBytes<B>, BuildError> {
        let mut strings = Strings::new_in(alloc.clone());
        let mut paths = PathTable::new_in(alloc.clone());

        let mut order = Vec::from_iter_in(0..self.bindings.len(), alloc.clone());
        let bindings = &self.bindings;
        let slot = |at: &usize| (bindings[*at].ns, bindings[*at].name.as_ref());
        if sort {
            order.sort_unstable_by(|a, b| slot(a).cmp(&slot(b)));
        }
        if let Some(pair) = order.windows(2).find(|pair| slot(&pair[0]) == slot(&pair[1])) {
            return Err(BuildError::Twice { table: "ModuleScope bindings", key: bindings[pair[1]].binding.def.key.0 });
        }
        let mut rows = Vec::with_capacity_in(order.len(), alloc.clone());
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

        // Each class a binding is in, by its bytes, then its digest over the sorted bindings.
        let mut classes = Vec::with_capacity_in(order.len(), alloc.clone());
        classes.extend(order.iter().map(|&at| bindings[at].binding.vis.class()));
        classes.sort_unstable_by_key(|class| class.to_record());
        classes.dedup();
        let mut exports = Vec::with_capacity_in(classes.len(), alloc.clone());
        for &class in classes.iter() {
            let entries = order.iter().map(|&at| bindings[at]).filter(|e| e.binding.vis.class() == class).map(export_of);
            let (tag, module) = class.to_record();
            exports.push(ExportRow { module, digest: exports_digest(entries, alloc.clone()), class: tag });
        }

        let mut errors = Vec::with_capacity_in(self.errors.len(), alloc.clone());
        for error in self.errors.iter() {
            errors.push(error_row(error, &mut paths, &mut strings)?);
        }
        let textual = textual_rows(&self.textual, &mut strings, alloc.clone())?;
        let macro_exports = textual_rows(&self.macro_exports, &mut strings, alloc.clone())?;

        let mut marks = Vec::from_iter_in(self.marks.iter().map(|m| MarkRow { len: m.len, child: m.child.0 }), alloc.clone());
        let mut fellows = Vec::from_iter_in(self.fellows.iter().map(|key| key.0), alloc.clone());
        if sort {
            marks.sort_unstable_by_key(|m| m.child);
            fellows.sort_unstable();
        }
        fellows.dedup();
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
            alloc.clone(),
        );

        let mut out = Writer::new_in(<ModuleScopeRoot as Record>::SCHEMA, alloc);
        let text = Column::push(&mut out, strings.bytes());
        let bindings = Column::push(&mut out, &rows);
        let exports = Column::push(&mut out, &exports);
        let errors = Column::push(&mut out, &errors);
        let textual = Column::push(&mut out, &textual);
        let marks = Column::push(&mut out, &marks);
        let fellows = Column::push(&mut out, &fellows);
        let traits = Column::push(&mut out, &traits);
        let macro_exports = Column::push(&mut out, &macro_exports);
        let path_rows = Column::push(&mut out, &paths.rows);
        let segments = Column::push(&mut out, &paths.segments);
        Ok(finish_with(out, |at| ModuleScopeRoot {
            module: self.module.0,
            bindings: bindings.slice(at, offset_of!(ModuleScopeRoot, bindings)),
            exports: exports.slice(at, offset_of!(ModuleScopeRoot, exports)),
            errors: errors.slice(at, offset_of!(ModuleScopeRoot, errors)),
            textual: textual.slice(at, offset_of!(ModuleScopeRoot, textual)),
            marks: marks.slice(at, offset_of!(ModuleScopeRoot, marks)),
            fellows: fellows.slice(at, offset_of!(ModuleScopeRoot, fellows)),
            traits: traits.slice(at, offset_of!(ModuleScopeRoot, traits)),
            macro_exports: macro_exports.slice(at, offset_of!(ModuleScopeRoot, macro_exports)),
            paths: path_rows.slice(at, offset_of!(ModuleScopeRoot, paths)),
            segments: segments.slice(at, offset_of!(ModuleScopeRoot, segments)),
            strings: text.slice(at, offset_of!(ModuleScopeRoot, strings)),
        }))
    }
}

fn textual_rows<N: AsRef<str>, B: Allocator, C: Allocator>(
    defs: &[TextualDef<N>],
    strings: &mut Strings<C>,
    alloc: B,
) -> Result<Vec<TextualRow, B>, BuildError> {
    let mut rows = Vec::with_capacity_in(defs.len(), alloc);
    for d in defs {
        rows.push(TextualRow { name: strings.push(d.name.as_ref(), TABLE)?, def: d.def.0, expansion: d.expansion, pad: 0 });
    }
    Ok(rows)
}

fn error_row<N: AsRef<str>, A: Allocator, B: Allocator + Clone, C: Allocator>(
    error: &ScopeError<N, Path<N, A>>,
    paths: &mut PathTable<B>,
    strings: &mut Strings<C>,
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

impl<'a, A: Allocator + Clone> ScopeBuilder<&'a str, A> {
    /// The working form of a [`ModuleScope`] record, its names borrowed from the record, in
    /// `alloc`: equal to the scope that wrote it.
    pub fn from(view: &ScopeView<'a>, alloc: A) -> ScopeBuilder<&'a str, A> {
        let mut out = ScopeBuilder::new_in(view.module(), alloc.clone());
        out.bindings.extend(view.bindings());
        out.errors.extend(view.errors().map(|e| e.map_path(|p| p.to_path_in(alloc.clone()))));
        out.textual.extend(view.textual());
        out.marks.extend(view.marks());
        out.fellows.extend(view.fellows());
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
    exports: &'a [ExportRow],
    errors: &'a [ErrorRow],
    textual: &'a [TextualRow],
    marks: &'a [MarkRow],
    fellows: &'a [ContentId],
    traits: &'a [TraitRow],
    macro_exports: &'a [TextualRow],
    paths: Paths<'a>,
    strings: &'a str,
}

impl<'a> ScopeView<'a> {
    /// The record in `buffer`, checked once: its layout, every name and path, and that its
    /// bindings are in strictly increasing order of namespace and name, which
    /// [`ScopeView::get`] probes by (its classes, marks and fellows likewise by their keys).
    pub fn read(buffer: &'a [u8]) -> Result<ScopeView<'a>, FormatError> {
        let root = blaze_format::open::<ModuleScopeRoot>(buffer)?;
        let strings = root.strings.as_str(buffer)?;
        let paths = Paths::read(root.paths.follow(buffer)?, root.segments.follow(buffer)?, strings, buffer)?;
        let view = ScopeView {
            module: ItemKey(root.module),
            rows: root.bindings.follow(buffer)?,
            exports: root.exports.follow(buffer)?,
            errors: root.errors.follow(buffer)?,
            textual: root.textual.follow(buffer)?,
            marks: root.marks.follow(buffer)?,
            fellows: root.fellows.follow(buffer)?,
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
        check_increasing(view.exports, buffer, "visibility classes out of order or repeated", |row| (row.class, row.module))?;
        for row in view.exports {
            if row.class != VisibilityTag::Restricted && row.module != ZERO {
                return Err(FormatError::Invalid { at: at_of_record(row, buffer), what: "a module on a class that has none" });
            }
        }
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
        check_increasing(view.fellows, buffer, "fellow modules out of order or repeated", |key| *key)?;
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

    /// The binding of `name` in `ns`, by one binary search.
    pub fn get(&self, ns: Namespace, name: &str) -> Option<Binding> {
        let strings = self.strings;
        let at = self
            .rows
            .binary_search_by(|row| (row.ns, text_of(strings, row.name)).cmp(&(ns, name)))
            .ok()?;
        Some(self.decode(&self.rows[at]).binding)
    }

    /// Every binding, by namespace and name.
    pub fn bindings(&self) -> impl ExactSizeIterator<Item = ScopeEntry<&'a str>> + use<'a> {
        let view = *self;
        self.rows.iter().map(move |row| view.decode(row))
    }

    /// The digest of the bindings a glob of `class` reads: of none when no binding is in it.
    pub fn exports(&self, class: VisibilityClass) -> ContentId {
        let (tag, module) = class.to_record();
        match self.exports.binary_search_by(|row| (row.class, row.module).cmp(&(tag, module))) {
            Ok(at) => self.exports[at].digest,
            Err(_) => ContentId::of(EXPORTS_DOMAIN, &[]),
        }
    }

    /// The bindings of `class`, in scope order: what an `Exports` read answers.
    pub fn exported(&self, class: VisibilityClass) -> impl Iterator<Item = ExportEntry<&'a str>> + use<'a> {
        self.bindings().filter(move |e| e.binding.vis.class() == class).map(export_of)
    }

    /// The classes a binding of the scope is in, each with its digest, by class.
    pub fn classes(&self) -> impl ExactSizeIterator<Item = (VisibilityClass, ContentId)> + use<'a> {
        self.exports.iter().map(|row| (VisibilityClass::from_record(row.class, row.module), row.digest))
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

    /// The other modules of the strongly connected set this scope was solved in; none for a
    /// module alone.
    pub fn fellows(&self) -> impl ExactSizeIterator<Item = ItemKey> + use<'a> {
        self.fellows.iter().map(|key| ItemKey(*key))
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
    rows.iter().map(move |row| TextualDef { name: text_of(strings, row.name), def: ItemKey(row.def), expansion: row.expansion })
}
