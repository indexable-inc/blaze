//! The words of name resolution and what it writes: namespaces, visibilities and their
//! classes, a binding with its origin and state, a module's scope, its exports by class, its
//! `macro_rules!` sequence, and the typed errors a scope holds.

use core::alloc::Allocator;

use alloc::vec::Vec;
use blaze_id::{ContentId, CrateItemKey, ItemKey};

use crate::facts::Path;

/// One of a module's three name tables. The order is the order a scope is sorted in.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub enum Namespace {
    Types,
    Values,
    Macros,
}

impl Namespace {
    /// The three, in order.
    pub const ALL: [Namespace; 3] = [Namespace::Types, Namespace::Values, Namespace::Macros];

    /// Its place in [`Namespace::ALL`].
    pub(crate) fn index(self) -> usize {
        match self {
            Namespace::Types => 0,
            Namespace::Values => 1,
            Namespace::Macros => 2,
        }
    }
}

/// Which macro sub-namespace a macro is in: a path written `m!()` finds only bang macros,
/// `#[m]` only attribute macros, `#[derive(m)]` only derives (rustc's `sub_namespace_match`).
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub enum MacroKind {
    Bang,
    Attr,
    Derive,
}

/// What a binding's target is, as far as resolving a path through it needs: a module or an
/// enum can be descended into, a macro has a sub-namespace, anything else ends a path.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub enum DefKind {
    Module,
    Enum,
    Trait,
    Macro(MacroKind),
    Other,
}

/// A binding's target: the item's key and its kind.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct Def {
    pub key: ItemKey,
    pub kind: DefKind,
}

impl Def {
    /// A module's def.
    pub fn module(key: ItemKey) -> Def {
        Def { key, kind: DefKind::Module }
    }

    /// Whether a path can go on through it (a module or an enum).
    pub fn has_scope(&self) -> bool {
        matches!(self.kind, DefKind::Module | DefKind::Enum)
    }
}

/// A resolved visibility: who may name a binding.
///
/// `pub(self)`, `pub(super)` and `pub(in path)` are all `Restricted` to the module they name; a
/// restriction to the crate root is written `Crate`, so one visibility has one form.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub enum Visibility {
    /// `pub`: anyone.
    Public,
    /// `pub(crate)`: the modules of this crate.
    Crate(CrateItemKey),
    /// The module with this key and the modules under it.
    Restricted(ItemKey),
}

/// Who is asking: a module, its crate and its chain of modules from itself up to its crate's
/// root (itself first). Visibility is a question about this chain.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Asker<'q> {
    pub(crate) krate: CrateItemKey,
    pub(crate) chain: &'q [ItemKey],
}

impl Visibility {
    /// Whether a module with the chain `from` may name a binding of this visibility.
    pub(crate) fn visible_from(self, from: Asker<'_>) -> bool {
        match self {
            Visibility::Public => true,
            Visibility::Crate(krate) => krate == from.krate,
            Visibility::Restricted(module) => from.chain.contains(&module),
        }
    }

    /// The class of this visibility, which [`ModuleScope::exports`] groups a scope's bindings by.
    pub fn class(self) -> VisibilityClass {
        match self {
            Visibility::Public => VisibilityClass::Public,
            Visibility::Crate(_) => VisibilityClass::Crate,
            Visibility::Restricted(module) => VisibilityClass::Restricted(module),
        }
    }

    /// The narrower of `a` and `b`, both seen from a module with the chain `from`: what a
    /// re-export of a `b` binding through an `a` import may be named by. A visibility `from`
    /// cannot see is the narrower one, so the result is never wider than what `from` sees.
    pub(crate) fn narrower(a: Visibility, b: Visibility, from: Asker<'_>) -> Visibility {
        match (a, b) {
            (Visibility::Public, other) | (other, Visibility::Public) => other,
            (Visibility::Crate(_), other @ Visibility::Restricted(_))
            | (other @ Visibility::Restricted(_), Visibility::Crate(_)) => other,
            (Visibility::Crate(x), Visibility::Crate(y)) => {
                if x == from.krate {
                    Visibility::Crate(y)
                } else {
                    Visibility::Crate(x)
                }
            }
            (Visibility::Restricted(x), Visibility::Restricted(y)) => {
                // The deeper of two ancestors is the narrower; one `from` cannot see is narrowest.
                match (from.chain.iter().position(|&m| m == x), from.chain.iter().position(|&m| m == y)) {
                    (Some(i), Some(j)) => {
                        if i <= j {
                            a
                        } else {
                            b
                        }
                    }
                    (None, _) => a,
                    (_, None) => b,
                }
            }
        }
    }

    /// The wider of two visibilities that `from` both sees.
    pub(crate) fn wider(a: Visibility, b: Visibility, from: Asker<'_>) -> Visibility {
        if Visibility::narrower(a, b, from) == a { b } else { a }
    }
}

/// A class of visibility, the unit a glob reads a module's names in: a reader asks for the
/// classes it can see, so a name added in a class it cannot see moves none of its reads.
///
/// - `Public`: every reader.
/// - `Crate`: readers in the module's crate.
/// - `Restricted(r)`: readers under `r`, an ancestor of the module or the module itself (a
///   private item is `Restricted` to its own module). A reader outside `r` never asks for it.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub enum VisibilityClass {
    Public,
    Crate,
    Restricted(ItemKey),
}

/// Where a binding came from (rustc's binding kinds).
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub enum Origin {
    /// An item, `mod`, `struct`, `macro`, ... the module declares (itself, or through a macro
    /// expansion: see [`Binding::expansion`]).
    Declared,
    /// An `extern crate` item.
    ExternCrate,
    /// A `#[macro_export]` `macro_rules!` of the crate, which lives in the crate root's scope.
    MacroExport,
    /// A named `use`.
    Import,
    /// A `use ..::*`.
    Glob,
    /// A prelude (the extern prelude, the `#[macro_use]` prelude, the standard prelude): never
    /// a binding of a scope, only what a path's first segment was found through.
    Prelude,
}

/// rustc's ambiguity kinds that a scope can hold or an early resolution can be refused with
/// (rustc dev guide, "Name resolution"; `rustc_resolve::AmbiguityKind`).
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub enum AmbiguityKind {
    /// Two glob imports bring one name with different targets: an error only where the name
    /// is used.
    GlobVsGlob,
    /// A macro-expanded name shadows a glob-imported one that an early resolution of this
    /// module used.
    GlobVsExpanded,
    /// A glob-imported name and a name of an outer scope (a prelude) differ, and an early
    /// resolution went through one of them.
    GlobVsOuter,
    /// A macro-expanded name and a name of an outer scope differ, and an early resolution went
    /// through one of them.
    MoreExpandedVsOuter,
    /// A macro-expanded `macro_rules!` and a path-based macro of the same name.
    MacroRulesVsModularized,
}

/// Whether a binding is final.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub enum BindingState {
    /// Final.
    Determined,
    /// Not final yet: something still pending may change it. Only a member of a group that is
    /// still being solved answers with it; a finished scope holds none.
    Undetermined,
    /// Final, and ambiguous: naming it is an error of this kind.
    Ambiguous(AmbiguityKind),
}

/// A name's binding in a scope.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct Binding {
    pub def: Def,
    pub vis: Visibility,
    pub origin: Origin,
    /// How deep in macro expansions it was written: 0 in the module's own text, `n` in the
    /// expansion of a call made at depth `n - 1`. A binding at depth above 0 is
    /// "macro-expanded" in rustc's ambiguity rules.
    pub expansion: u16,
    pub state: BindingState,
}

/// One entry of a scope: a binding by namespace and name.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct ScopeEntry<N> {
    pub ns: Namespace,
    pub name: N,
    pub binding: Binding,
}

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
/// [`ModuleScope::textual`].
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct TextualMark {
    pub child: ItemKey,
    pub len: u32,
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
    /// A macro of the wrong sub-namespace (a bang macro used as an attribute, ...).
    WrongMacroKind,
}

/// An error of user code, kept in the scope as a value (the machine never panics on one).
#[derive(Clone, Debug)]
pub enum ScopeError<N, A: Allocator> {
    /// A `use` did not resolve: its path, the index of the segment that failed, and why.
    Import { site: UseSite, path: Path<N, A>, segment: u32, why: Unresolved },
    /// A macro call's path did not resolve.
    Macro { call: ItemKey, path: Path<N, A>, segment: u32, why: Unresolved },
    /// An `extern crate` of a crate that is no dependency.
    ExternCrate { name: N },
    /// A `pub(in path)` whose path is not an ancestor of the module.
    RestrictedNotAncestor { path: Path<N, A> },
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

impl<N: PartialEq, A: Allocator> PartialEq for ScopeError<N, A> {
    fn eq(&self, other: &Self) -> bool {
        use ScopeError as E;
        match (self, other) {
            (
                E::Import { site: a, path: p, segment: s, why: w },
                E::Import { site: b, path: q, segment: t, why: v },
            ) => a == b && p == q && s == t && w == v,
            (
                E::Macro { call: a, path: p, segment: s, why: w },
                E::Macro { call: b, path: q, segment: t, why: v },
            ) => a == b && p == q && s == t && w == v,
            (E::ExternCrate { name: a }, E::ExternCrate { name: b }) => a == b,
            (E::RestrictedNotAncestor { path: a }, E::RestrictedNotAncestor { path: b }) => a == b,
            (
                E::Duplicate { ns: a, name: n, first: f, second: s },
                E::Duplicate { ns: b, name: m, first: g, second: t },
            ) => a == b && n == m && f == g && s == t,
            (
                E::WiderReexport { site: a, ns: x, name: n, def: d },
                E::WiderReexport { site: b, ns: y, name: m, def: e },
            ) => a == b && x == y && n == m && d == e,
            (
                E::Ambiguous { site: a, ns: x, name: n, kind: k, first: f, second: s },
                E::Ambiguous { site: b, ns: y, name: m, kind: l, first: g, second: t },
            ) => a == b && x == y && n == m && k == l && f == g && s == t,
            (
                E::Inconsistent { site: a, ns: x, name: n, early: e, last: l },
                E::Inconsistent { site: b, ns: y, name: m, early: f, last: k },
            ) => a == b && x == y && n == m && e == f && l == k,
            (E::RecursionLimit { call: a }, E::RecursionLimit { call: b }) => a == b,
            _ => false,
        }
    }
}

/// A module's scope: what `ModuleScope` records hold (docs/v2/plan/design/bottom.md).
///
/// `bindings` is sorted by namespace then name, one entry per pair, so its serialized bytes are
/// a function of the scope alone. `errors` is in the order the module's text and expansions
/// are written in. A member of a cycle names its fellows, sorted.
#[derive(Clone, Debug)]
pub struct ModuleScope<N, A: Allocator> {
    pub module: ItemKey,
    pub bindings: Vec<ScopeEntry<N>, A>,
    pub errors: Vec<ScopeError<N, A>, A>,
    /// The `macro_rules!` defined in this module and in its `#[macro_use]` children, in the
    /// order they are visible: what a `#[macro_use]` on this module carries out to the parent
    /// (its `TextualOut`), and, by [`ModuleScope::marks`], each child's textual entry.
    pub textual: Vec<TextualDef<N>, A>,
    /// For each `mod` child (sorted by child key), where its `mod` line falls in `textual`.
    pub marks: Vec<TextualMark, A>,
    pub fellows: Vec<ItemKey, A>,
}

impl<N: Ord + Copy, A: Allocator> ModuleScope<N, A> {
    /// The binding of `name` in `ns`, by one binary search.
    pub fn get(&self, ns: Namespace, name: N) -> Option<&Binding> {
        self.bindings
            .binary_search_by(|e| (e.ns, e.name).cmp(&(ns, name)))
            .ok()
            .map(|at| &self.bindings[at].binding)
    }

    /// What a `(module, namespace, name)` read of this scope answers.
    pub fn answer(&self, ns: Namespace, name: N) -> crate::read::NameAnswer {
        match self.get(ns, name) {
            Some(b) => crate::read::NameAnswer::Found { def: b.def, vis: b.vis, state: b.state },
            None => crate::read::NameAnswer::Absent,
        }
    }

    /// The bindings of one visibility class, in scope order: what an `Exports` read of this
    /// scope answers. The record writer hashes them into the class's digest.
    pub fn exports(&self, class: VisibilityClass) -> impl Iterator<Item = ExportEntry<N>> + '_ {
        self.bindings.iter().filter(move |e| e.binding.vis.class() == class).map(|e| ExportEntry {
            ns: e.ns,
            name: e.name,
            def: e.binding.def,
            vis: e.binding.vis,
            state: e.binding.state,
        })
    }

    /// The textual mark of `child`, if it is a child of this module.
    pub fn mark(&self, child: ItemKey) -> Option<u32> {
        self.marks.binary_search_by(|m| m.child.cmp(&child)).ok().map(|at| self.marks[at].len)
    }
}

impl<N: PartialEq, A: Allocator> PartialEq for ModuleScope<N, A> {
    fn eq(&self, other: &Self) -> bool {
        self.module == other.module
            && self.bindings == other.bindings
            && self.errors == other.errors
            && self.textual == other.textual
            && self.marks == other.marks
            && self.fellows == other.fellows
    }
}

/// What a read saw: a name's answer, or the digest of an exports class or a textual list.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Saw {
    Name(crate::read::NameAnswer),
    Digest(ContentId),
}

/// One read a scope was determined by, and what it saw.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct ReadEntry<N> {
    pub read: crate::read::Read<N>,
    pub saw: Saw,
}

/// Exactly the reads of other modules' records that determined a scope, the negative ones
/// included (a name looked up and not found), sorted by read; a record is valid while every
/// one of them still sees what it saw. Reads of a fellow of a cycle are not in it: the
/// members are written together and are valid together.
#[derive(Clone, Debug)]
pub struct ReadList<N, A: Allocator> {
    pub entries: Vec<ReadEntry<N>, A>,
}

impl<N: Ord + Copy, A: Allocator> ReadList<N, A> {
    /// Whether every read still sees what it saw, `now` answering each as today's records do.
    pub fn holds(&self, mut now: impl FnMut(&crate::read::Read<N>) -> Saw) -> bool {
        self.entries.iter().all(|e| now(&e.read) == e.saw)
    }

    /// Whether the list holds a read of `read`.
    pub fn contains(&self, read: &crate::read::Read<N>) -> bool {
        self.entries.binary_search_by(|e| e.read.cmp(read)).is_ok()
    }
}

impl<N: PartialEq, A: Allocator> PartialEq for ReadList<N, A> {
    fn eq(&self, other: &Self) -> bool {
        self.entries == other.entries
    }
}

/// A finished module: its scope and the reads it is valid by.
#[derive(Clone, Debug)]
pub struct Resolved<N, A: Allocator> {
    pub scope: ModuleScope<N, A>,
    pub reads: ReadList<N, A>,
}

impl<N: PartialEq, A: Allocator> PartialEq for Resolved<N, A> {
    fn eq(&self, other: &Self) -> bool {
        self.scope == other.scope && self.reads == other.reads
    }
}
