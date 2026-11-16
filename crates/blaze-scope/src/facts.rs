//! What the machine reads of the module it resolves, as plain data: [`ScopeFacts`], which
//! blaze-surface implements over its records (`ModuleDecls`, `TextualEntry`'s inputs, the crate
//! root record, `Expansion`), and the types it hands back.
//!
//! Reads of another module's scope are not here: the machine yields them as asks
//! ([`crate::Read`]) and takes their answers, so it never computes another module's scope.

use core::alloc::Allocator;
use core::fmt;
use core::hash::Hash;

use alloc::vec::Vec;
use blaze_id::{CrateItemKey, ItemKey};

use crate::scope::{Def, MacroKind, Namespace};

/// What a name is to the machine: a small value that compares, orders and hashes. Its order
/// must be the order of the names' text, since a scope is sorted by it; blaze-surface's
/// interned symbol orders by its text, and the tests use `&'static str`.
pub trait Name: Copy + Ord + Hash + fmt::Debug {}

impl<T: Copy + Ord + Hash + fmt::Debug> Name for T {}

/// The edition of a crate, as far as names care: 2015 resolves an import's first segment
/// from the crate root, later editions lexically.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub enum Edition {
    E2015,
    E2018,
    E2021,
    E2024,
}

/// How a path begins.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub enum PathKind {
    /// `a::b`: the first segment is looked up lexically (or, in a 2015 `use`, in the crate root).
    Plain,
    /// `crate::a`.
    Crate,
    /// `self::a`.
    SelfMod,
    /// `super::a` (`Super(1)`), `super::super::a` (`Super(2)`), ...
    Super(u8),
    /// `::a`: the extern prelude (2018 on), or the crate root then the extern prelude (2015).
    Abs,
    /// `$crate::a`, with the crate the `$crate` token's hygiene names.
    DollarCrate(CrateItemKey),
}

/// A path as written: how it begins and its segments.
#[derive(Clone, Debug)]
pub struct Path<N, A: Allocator> {
    pub kind: PathKind,
    pub segments: Vec<N, A>,
}

impl<N: PartialEq, A: Allocator> PartialEq for Path<N, A> {
    fn eq(&self, other: &Self) -> bool {
        self.kind == other.kind && self.segments == other.segments
    }
}

impl<N: Copy, A: Allocator> Path<N, A> {
    /// The same path in `alloc`.
    pub fn copy_in<B: Allocator>(&self, alloc: B) -> Path<N, B> {
        let mut segments = Vec::with_capacity_in(self.segments.len(), alloc);
        segments.extend_from_slice(&self.segments);
        Path { kind: self.kind, segments }
    }
}

/// A visibility as written.
#[derive(Clone, Debug)]
pub enum VisDecl<N, A: Allocator> {
    /// No visibility written, or `pub(self)`.
    Private,
    Public,
    /// `pub(crate)` (and `crate` in 2015's `crate fn`).
    Crate,
    /// `pub(super)`.
    Super,
    /// `pub(in path)`.
    In(Path<N, A>),
}

/// An item the module declares itself, in one namespace (a tuple struct is two of these).
#[derive(Clone, Debug)]
pub struct DeclaredItem<N, A: Allocator> {
    pub name: N,
    pub ns: Namespace,
    pub def: Def,
    pub vis: VisDecl<N, A>,
}

/// A `mod` child. `#[path]` and the `foo.rs` / `foo/mod.rs` choice are already decided: the
/// child is named by its key, and which file it is in is the `ModuleFile` record's question.
#[derive(Clone, Debug)]
pub struct ModDecl<N, A: Allocator> {
    pub name: N,
    pub module: ItemKey,
    pub vis: VisDecl<N, A>,
    /// `#[macro_use] mod child;`: the child's `macro_rules!` stay visible after this line.
    pub macro_use: bool,
    /// Its place in the fragment's source order (unique among the fragment's ordered entries).
    pub pos: u32,
}

/// What a `use` leaf binds its target as.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub enum Rename<N> {
    /// `use a::b;`: as its last segment.
    Same,
    /// `use a::b as c;`.
    To(N),
    /// `use a::b as _;`: as nothing nameable (it still brings a trait's methods into scope).
    Underscore,
}

/// A `use` leaf: named, or a glob.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub enum UseLeaf<N> {
    /// `use a::b` (`types_only` for `use a::{self}`, which imports only the module).
    Named { rename: Rename<N>, types_only: bool },
    /// `use a::*`.
    Glob,
}

/// One leaf of a `use` tree: `use a::{b, c::*}` is two.
#[derive(Clone, Debug)]
pub struct UseDecl<N, A: Allocator> {
    pub path: Path<N, A>,
    pub leaf: UseLeaf<N>,
    pub vis: VisDecl<N, A>,
    pub pos: u32,
}

/// Which crate an `extern crate` names.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum ExternTarget<N> {
    /// A dependency by its name.
    Dep(N),
    /// `extern crate self as name;`.
    SelfCrate,
}

/// An `extern crate` item. What it adds to the extern prelude and the `#[macro_use]` prelude
/// at the crate root is in [`CrateRoot`]; here it is only the binding in its own module.
#[derive(Clone, Debug)]
pub struct ExternCrateDecl<N, A: Allocator> {
    pub target: ExternTarget<N>,
    pub rename: Rename<N>,
    pub vis: VisDecl<N, A>,
}

/// A `macro_rules!` definition, in the fragment's source order. A `#[macro_export]` one is in
/// the crate root's scope through [`CrateRoot::macro_exports`].
#[derive(Clone, Copy, Debug)]
pub struct MacroRulesDecl<N> {
    pub name: N,
    pub def: ItemKey,
    pub pos: u32,
}

/// An item-position macro call: a bang call, an attribute macro on an item (the item is not a
/// declared name until the call expands), or a derive.
#[derive(Clone, Debug)]
pub struct MacroCallDecl<N, A: Allocator> {
    /// The call's identity: the key its expansion's items are keyed under.
    pub call: ItemKey,
    pub path: Path<N, A>,
    pub kind: MacroKind,
    pub pos: u32,
}

/// What one fragment of a module declares: the module's own text (with `cfg` already decided:
/// an item `cfg`'d out is not here), or one macro call's expansion.
#[derive(Clone, Debug)]
pub struct ModuleDecls<N, A: Allocator> {
    pub items: Vec<DeclaredItem<N, A>, A>,
    pub mods: Vec<ModDecl<N, A>, A>,
    pub uses: Vec<UseDecl<N, A>, A>,
    pub extern_crates: Vec<ExternCrateDecl<N, A>, A>,
    pub macro_rules: Vec<MacroRulesDecl<N>, A>,
    pub calls: Vec<MacroCallDecl<N, A>, A>,
}

impl<N, A: Allocator + Clone> ModuleDecls<N, A> {
    /// Nothing declared.
    pub fn new_in(alloc: A) -> ModuleDecls<N, A> {
        ModuleDecls {
            items: Vec::new_in(alloc.clone()),
            mods: Vec::new_in(alloc.clone()),
            uses: Vec::new_in(alloc.clone()),
            extern_crates: Vec::new_in(alloc.clone()),
            macro_rules: Vec::new_in(alloc.clone()),
            calls: Vec::new_in(alloc),
        }
    }
}

/// What a macro call's expansion adds to its module: the same shape as the module's own text.
pub type ExpandedDecls<N, A> = ModuleDecls<N, A>;

/// A module's place: a function of its key (the key hashes its parent and its name), so it is
/// never a read of a read list.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct ModuleInfo<N> {
    pub krate: CrateItemKey,
    /// `None` for a crate root.
    pub parent: Option<ItemKey>,
    /// Its name in its parent (`None` for a crate root).
    pub name: Option<N>,
    /// `#[no_implicit_prelude]` on this module (it holds for the modules under it too).
    pub no_implicit_prelude: bool,
}

/// A dependency of a crate, by the name it is known by.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Dep<N> {
    pub name: N,
    pub krate: CrateItemKey,
    pub root: ItemKey,
}

/// The crate-root record: what every module of a crate reads of its crate.
#[derive(Clone, Debug)]
pub struct CrateRoot<N, A: Allocator> {
    pub krate: CrateItemKey,
    pub root: ItemKey,
    pub edition: Edition,
    pub no_std: bool,
    pub no_core: bool,
    /// The extern prelude, sorted by name: the `--extern` crates and the root's `extern crate`
    /// items (`extern crate self as name` too), each to its root module.
    pub extern_prelude: Vec<(N, ItemKey), A>,
    /// Every dependency by name: what an `extern crate` and a `$crate` resolve against.
    pub deps: Vec<Dep<N>, A>,
    /// The root modules of the crates under `#[macro_use] extern crate` (std's or core's
    /// injected one included), whose exported macros are the `#[macro_use]` prelude, in order.
    pub macro_use: Vec<ItemKey, A>,
    /// The standard prelude module to import (`std::prelude::rust_2021`, or core's under
    /// `no_std`), or none under `no_core`.
    pub prelude: Option<ItemKey>,
    /// The crate's `#[macro_export]` `macro_rules!` written in source (not macro-expanded),
    /// sorted by name: they are bindings of the root's scope.
    pub macro_exports: Vec<(N, ItemKey), A>,
    /// The tool names (`rustfmt`, `clippy`, `register_tool`'s): an attribute path beginning
    /// with one is inert.
    pub tools: Vec<N, A>,
}

/// Everything the machine reads of the module it resolves and of its crate. Each method is a
/// lookup of a record read in place; `None` is a missing record, which the machine stops on
/// with a typed error naming it, except for [`ScopeFacts::expansion`], whose `None` means the
/// call has not been expanded yet.
pub trait ScopeFacts {
    type Name: Name;
    type Alloc: Allocator + Clone;

    /// The module's place in its crate.
    fn module(&self, module: ItemKey) -> Option<ModuleInfo<Self::Name>>;

    /// What the module's own text declares.
    fn decls(&self, module: ItemKey) -> Option<&ModuleDecls<Self::Name, Self::Alloc>>;

    /// The crate-root record.
    fn crate_root(&self, krate: CrateItemKey) -> Option<&CrateRoot<Self::Name, Self::Alloc>>;

    /// What the call's expansion adds to its module, or `None` while it is not expanded.
    fn expansion(&self, call: ItemKey) -> Option<&ExpandedDecls<Self::Name, Self::Alloc>>;
}
