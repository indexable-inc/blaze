//! The machine's vocabulary: what is asked and answered, what it needs and is given, a cell's
//! key and reads, and its typed errors. Every value here is `Copy` and holds borrowed slices of
//! what the driver handed in (`'w`), never a copy of them.

use core::fmt;

use blaze_id::{CrateItemKey, ItemKey};
use blaze_ids::{ContentId, Hasher};
use blaze_own::AllocFailure;

/// The domain of a [`CellKey`]: the recipe's name, so a change of it is a new domain.
pub const KEY_DOMAIN: &str = "blaze v2 names on demand cell key 1";
/// The domain of a cell's answer digest (what a [`Read::Cell`] holds).
pub const DIGEST_DOMAIN: &str = "blaze v2 names on demand cell answer 1";

/// A namespace of Rust's name resolution.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum Ns {
    Type,
    Value,
    Macro,
}

impl Ns {
    pub const ALL: [Ns; 3] = [Ns::Type, Ns::Value, Ns::Macro];

    /// The byte a key is made with.
    pub fn byte(self) -> u8 {
        match self {
            Ns::Type => 0,
            Ns::Value => 1,
            Ns::Macro => 2,
        }
    }
}

/// The namespaces an item binds its name in (a unit or tuple struct: type and value).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Spaces {
    pub ty: bool,
    pub value: bool,
    pub macros: bool,
}

impl Spaces {
    pub const TYPE: Spaces = Spaces { ty: true, value: false, macros: false };
    pub const VALUE: Spaces = Spaces { ty: false, value: true, macros: false };
    pub const MACRO: Spaces = Spaces { ty: false, value: false, macros: true };

    pub fn has(self, ns: Ns) -> bool {
        match ns {
            Ns::Type => self.ty,
            Ns::Value => self.value,
            Ns::Macro => self.macros,
        }
    }
}

/// A module: its crate and its key (a crate root's key is the root module's).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct ModRef {
    pub krate: CrateItemKey,
    pub key: ItemKey,
}

/// What a name is bound to: the end of every re-export chain.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum Def {
    /// An item that is not a module (a type, fn, const, macro, ..).
    Item(ItemKey),
    /// A module, a crate's root among them.
    Module(ModRef),
}

/// A binding's visibility. `In(m)` is visible in `m` and every module under it (`pub(self)` is
/// `In(self)`, `pub(super)` is `In(parent)`, `pub(in path)` is `In(path)`).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Vis {
    Pub,
    Crate(CrateItemKey),
    In(ItemKey),
}

impl Vis {
    /// Whether a binding of this visibility is visible from the module `from`.
    pub fn visible_from(self, from: &Outline<'_>) -> bool {
        self.breadth(from).is_some()
    }

    /// How wide the visibility is as seen from `from`, `None` when `from` cannot see it: `In(m)`
    /// counts the modules from `from` up to `m`, so a restriction to an ancestor is wider.
    fn breadth(self, from: &Outline<'_>) -> Option<usize> {
        match self {
            Vis::Pub => Some(usize::MAX),
            Vis::Crate(krate) => (krate == from.module.krate).then_some(usize::MAX - 1),
            Vis::In(key) if key == from.module.key => Some(0),
            Vis::In(key) => {
                let at = from.chain.iter().position(|k| *k == key)?;
                from.chain.len().checked_sub(at)
            }
        }
    }

    /// The narrower of two visibilities both visible from `from` (an import re-exports at most
    /// what it imports).
    pub fn narrower(self, other: Vis, from: &Outline<'_>) -> Vis {
        if self.breadth(from) <= other.breadth(from) { self } else { other }
    }

    /// The wider of two visibilities both visible from `from` (one definition bound twice keeps
    /// the wider, as rustc's `try_define` does).
    pub fn wider(self, other: Vis, from: &Outline<'_>) -> Vis {
        if self.breadth(from) >= other.breadth(from) { self } else { other }
    }
}

/// Where a path starts.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Anchor<'w> {
    /// `crate::`: the root of the module's crate.
    Crate,
    /// `self::`.
    SelfMod,
    /// `super::` written this many times.
    Super(usize),
    /// `::name`: a crate of the extern prelude.
    Extern(&'w str),
    /// `$crate::`: the root of the crate the macro was defined in, as the expansion names it.
    Root(ModRef),
    /// A `use` path's first segment (Rust 2018): the module's name or an extern crate, the two
    /// ambiguous when both exist and differ.
    Uniform,
    /// A path's first segment outside `use`: the module's name, else the extern prelude, else
    /// the standard prelude.
    Lexical,
}

/// A path to a module: its anchor and the module names after it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Path<'w> {
    pub anchor: Anchor<'w>,
    pub segs: &'w [&'w str],
}

/// The key a call is known by: blaze-expand's `call_key` (the macro's definition content and the
/// call's tokens), item-relative.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct CallRef(pub ContentId);

/// What an item-position macro call may write (the soundness rule, crate docs).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Writes<'w> {
    /// Anything: any name, globs, further calls.
    Any,
    /// Bindings of these names only, no glob, no call summarised wider; `exports`: it may write
    /// a `#[macro_export]` macro (of one of these names).
    Only { names: &'w [&'w str], exports: bool },
}

impl Writes<'_> {
    pub fn may_write(self, name: &str) -> bool {
        match self {
            Writes::Any => true,
            Writes::Only { names, .. } => names.contains(&name),
        }
    }

    pub fn may_export(self, name: &str) -> bool {
        match self {
            Writes::Any => true,
            Writes::Only { names, exports } => exports && names.contains(&name),
        }
    }

    /// Whether `inner` claims no more than `self`.
    pub fn covers(self, inner: Writes<'_>) -> bool {
        match (self, inner) {
            (Writes::Any, _) => true,
            (Writes::Only { .. }, Writes::Any) => false,
            (Writes::Only { names, exports }, Writes::Only { names: inner_names, exports: inner_exports }) => {
                (exports || !inner_exports) && inner_names.iter().all(|n| names.contains(n))
            }
        }
    }
}

/// One entry of a module, as its outline (or an expansion in it) writes it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Entry<'w> {
    /// An item; `exported`: a `#[macro_export]` macro, bound at the crate root, not here.
    Item { name: &'w str, spaces: Spaces, vis: Vis, def: Def, exported: bool },
    /// `use path::target as name;` (`name == target` without `as`).
    Use { name: &'w str, path: Path<'w>, target: &'w str, vis: Vis },
    /// `use path::*;`.
    Glob { path: Path<'w>, vis: Vis },
    /// `extern crate krate as name;`, `root` its root module's key.
    ExternCrate { name: &'w str, krate: CrateItemKey, root: ItemKey, vis: Vis },
    /// An item-position macro call.
    Call { call: CallRef, writes: Writes<'w> },
}

/// One module as the machine reads it: the outline of its file (or inline body).
#[derive(Clone, Copy, Debug)]
pub struct Outline<'w> {
    pub module: ModRef,
    /// The digest of the module's own items (blaze-file-items' names digest of the module).
    pub digest: ContentId,
    /// Its ancestors, the crate root first, itself not included (empty for a root).
    pub chain: &'w [ItemKey],
    /// `#![no_implicit_prelude]` on it or an ancestor.
    pub no_implicit_prelude: bool,
    pub entries: &'w [Entry<'w>],
}

impl Outline<'_> {
    /// The key of its crate's root module.
    pub fn root(&self) -> ItemKey {
        self.chain.first().copied().unwrap_or(self.module.key)
    }
}

/// A crate of the extern prelude.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Extern<'w> {
    pub name: &'w str,
    pub krate: CrateItemKey,
    pub root: ItemKey,
}

/// A crate's root facts: what is read of it beyond its root module's outline.
#[derive(Clone, Copy, Debug)]
pub struct CrateRoot<'w> {
    pub krate: CrateItemKey,
    /// The digest of these facts (the key part a cell that read them carries).
    pub digest: ContentId,
    pub extern_prelude: &'w [Extern<'w>],
    /// The standard prelude's module, as a path from the crate root (`::std::prelude::rust_2024`).
    pub prelude: Option<Path<'w>>,
    /// Every module with a `#[macro_export]` macro or a call that may write one, from the files'
    /// outlines (no expansion): the root's macro namespace reads these (soundness rule 4).
    pub export_modules: &'w [ItemKey],
}

/// A question: the binding of `name` in namespace `ns` of the module `path` leads to, asked
/// from the module `from` (whose visibility decides `Private`). A bare name is `Lexical` with no
/// segments.
#[derive(Clone, Copy, Debug)]
pub struct Ask<'w> {
    pub from: ModRef,
    pub path: Path<'w>,
    pub name: &'w str,
    pub ns: Ns,
}

/// Why a name is ambiguous.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Ambiguity {
    /// Two explicit entries bind it (rustc E0428, E0252).
    Duplicate,
    /// It leads to more than one definition (globs, or a local name against an extern crate).
    Many,
    /// In the macro namespace, an expansion's binding against a glob's (rustc `GlobVsExpanded`).
    GlobVsExpanded,
}

/// The answer to an [`Ask`]. `at` is the segment the answer is about: `0..segs.len()` a module of
/// the path, `segs.len()` the name.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Answer {
    Found { def: Def, vis: Vis },
    Absent { at: usize },
    Ambiguous { at: usize, why: Ambiguity },
    Private { at: usize, def: Def },
    NotAModule { at: usize, def: Def },
    /// `::name` names no crate of the extern prelude.
    NoCrate,
    /// More `super` than ancestors.
    NoParent,
}

/// What the machine needs before it can go on.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Need {
    Outline(ModRef),
    Expand { module: ModRef, call: CallRef },
    Root(CrateItemKey),
}

/// What the driver gives back, one for each [`Need`].
#[derive(Clone, Copy, Debug)]
pub enum Given<'w> {
    Outline(&'w Outline<'w>),
    Expansion { module: ModRef, call: CallRef, entries: &'w [Entry<'w>] },
    Root(&'w CrateRoot<'w>),
}

/// A cell's key, before the work (rule 65): the module, its items digest, the namespace, the
/// name.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct CellKey(pub ContentId);

impl CellKey {
    pub fn of(module: &Outline<'_>, ns: Ns, name: &str) -> CellKey {
        let mut h = Hasher::new(KEY_DOMAIN);
        h.update(&module.module.krate.0.0).update(&module.module.key.0.0).id(&module.digest);
        h.update(&[ns.byte()]).part(name.as_bytes());
        CellKey(h.finish())
    }
}

/// One thing a cell's answer consulted beyond its own key: what a later command checks today's
/// values of before it takes the record.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Read {
    /// Another cell, and the digest of its answer as read.
    Cell { key: CellKey, answer: ContentId },
    /// A macro call of the module: its expansion or its summary.
    Call(CallRef),
    /// A crate's root facts.
    Root { krate: CrateItemKey, digest: ContentId },
}

/// Which bound a refusal is about (rule 18).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Limit {
    Modules,
    Cells,
    Passes,
    Rounds,
    Depth,
}

/// Why the machine stopped.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Error {
    Memory(AllocFailure),
    /// A table or loop past its cap.
    Bound { limit: Limit, most: usize },
    /// `step` with no ask, or `ask` while one is open.
    NoAsk,
    Busy,
    /// A given twice.
    Twice(Need),
    /// An expansion for a call the module does not hold unexpanded.
    Unasked { module: ModRef, call: CallRef },
    /// An expansion that writes what its call's summary said it could not.
    SummaryBroken { module: ModRef, call: CallRef },
    /// A round shadowed by an import that the next round found unresolved: the shadowing set
    /// shrank (rustc's indeterminate import).
    Unstable { module: ModRef, ns: Ns },
    /// A cell asked of the record writer that this request did not resolve.
    NotResolved { module: ModRef, ns: Ns },
    /// The machine's own invariant broken (an index it made, missing): a bug here.
    Internal(&'static str),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Memory(why) => write!(f, "names on demand: {why}"),
            Error::Bound { limit, most } => write!(f, "names on demand: past the bound of {most} {limit:?} (rule 18)"),
            Error::NoAsk => f.write_str("names on demand: a step with no ask open"),
            Error::Busy => f.write_str("names on demand: an ask while another is open"),
            Error::Twice(need) => write!(f, "names on demand: {need:?} given twice"),
            Error::Unasked { module, call } => write!(f, "names on demand: an expansion of {call:?} in {module:?}, which holds no such unexpanded call"),
            Error::SummaryBroken { module, call } => write!(f, "names on demand: the expansion of {call:?} in {module:?} writes what its summary excluded (soundness rule 3)"),
            Error::Unstable { module, ns } => write!(f, "names on demand: an import of {module:?} in {ns:?} resolved in one round and not in the next (an indeterminate import)"),
            Error::NotResolved { module, ns } => write!(f, "names on demand: a record of a cell of {module:?} in {ns:?} this request did not resolve"),
            Error::Internal(what) => write!(f, "names on demand: internal: {what}"),
        }
    }
}

impl core::error::Error for Error {}

impl From<AllocFailure> for Error {
    fn from(why: AllocFailure) -> Error {
        Error::Memory(why)
    }
}

/// The digest bytes of a definition and visibility, for an answer digest.
pub(crate) fn hash_def(h: &mut Hasher, def: Def, vis: Vis) {
    match def {
        Def::Item(k) => h.update(&[0]).update(&k.0.0),
        Def::Module(m) => h.update(&[1]).update(&m.krate.0.0).update(&m.key.0.0),
    };
    match vis {
        Vis::Pub => h.update(&[0]),
        Vis::Crate(k) => h.update(&[1]).update(&k.0.0),
        Vis::In(k) => h.update(&[2]).update(&k.0.0),
    };
}
