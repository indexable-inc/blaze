//! Paths as written, resolved over finished scope records (bottom.md, section 6, item 6): what a
//! later stage asks of the names once every module's scope is done. A path written in a fn
//! body, a type, a macro call: [`path`] walks it segment by segment through the records,
//! [`macro_path`] is the same for a macro call's path, and [`traits_in_scope`] lists what method
//! resolution may use. Pure: the records come in through [`World`], nothing is cached here.
//!
//! What a first segment means (Rust's lexical layers, outermost last): the module's own scope
//! (its bindings and, through its glob edges, the globbed modules'); for a macro, the module's
//! textual `macro_rules!` scope and its ancestors' up to their `mod` lines, then the
//! `#[macro_use]` prelude (each `#[macro_use] extern crate`'s `#[macro_export]`s, std's or
//! core's injected one first); for a type, the extern prelude; then the standard prelude
//! (unless `#[no_implicit_prelude]` is on the module or an ancestor); for a type, last, the
//! builtin types. `macro_rules!` written inside a body are the caller's: it resolves those first.

#![blaze::sans_io]

use core::fmt;

use blaze_id::{CrateItemKey, ItemKey};
use blaze_own::{AVec, Alloc};
use blaze_surface::{
    BindingState, Def, DefKind, Edition, Namespace, Node, Passed, PathKind, RefusedWhy, Sources, Through, ThroughScratch,
    Visibility, through_in,
};

use crate::builtin::builtin_type_in;
use crate::facts::Name;

mod seam;
pub use seam::{RootRead, ScopeRead, VariantHit, variant_in};

/// The finished records a path is read through: the working forms of a fresh solve
/// (`&ScopeBuilder`, `&CrateRootBuilder`) or a names record's views of its own bytes
/// (`ScopeView`, `CrateRootView`, made in O(1) and searched in place, D114 rule 1). One walk
/// reads both ([`seam`]).
pub trait World<N> {
    /// A module's scope as this world holds it.
    type Scope<'w>: ScopeRead<'w, N>
    where
        Self: 'w;
    /// A crate's root as this world holds it.
    type Root<'w>: RootRead<N>
    where
        Self: 'w;
    /// The module's finished scope, or `None` when no record of it is held.
    fn scope(&self, module: ItemKey) -> Option<Self::Scope<'_>>;
    /// The module's chain up to its crate's root, itself first.
    fn chain(&self, module: ItemKey) -> Option<&[ItemKey]>;
    /// The root record of the crate `key` (any item of it) is of.
    fn root_of(&self, key: ItemKey) -> Option<Self::Root<'_>>;
    /// The root record of the crate.
    fn root(&self, krate: CrateItemKey) -> Option<Self::Root<'_>>;
    /// The variant of the enum `owner` named `name`: `None` when `owner` is no enum held,
    /// `Some(None)` when it is one with no such variant.
    fn variant(&self, owner: ItemKey, name: N) -> Option<Option<VariantHit>>;
    /// Whether `module` or an ancestor is `#[no_implicit_prelude]`.
    fn no_implicit_prelude(&self, module: ItemKey) -> bool;
    /// Whether `module` is a block's module ([`crate::blocks::is_block_module`]): anonymous,
    /// passed over by a written `self` and `super` ([`written_up`]).
    fn anonymous(&self, module: ItemKey) -> bool;
}

/// The module a written `self` (`n == 0`) or `super` written `n` times names from the module
/// whose chain is `chain`: the `n`th module of the chain that is no block's module (the
/// Reference, `paths.qualifiers.mod-self` and `paths.qualifiers.super`; a block is an anonymous
/// scope). The cells' `Cells::up` is the same function. `None`: past the crate's root.
fn written_up<N: Name, W: World<N>>(world: &W, chain: &[ItemKey], n: u8) -> Option<ItemKey> {
    chain.iter().copied().filter(|m| !world.anonymous(*m)).nth(usize::from(n))
}

/// The module a path head `self` (`n == 0`) or `super` written `n` times names from `module`,
/// as [`path`] reads the head ([`written_up`]: a block's module is passed over). For a reader
/// that holds no segment to walk: a glob `use self::*;` or `use super::*;` (blaze-ladder
/// front2 block_traits.rs). `NotRead`: `module`'s chain is not held; `SuperPastRoot`: past
/// the crate's root.
pub fn written_module<N: Name, W: World<N>>(world: &W, module: ItemKey, n: u8) -> Result<ItemKey, PathError> {
    let chain = world.chain(module).ok_or(PathError { module, at: 0, why: Why::NotRead { module, layer: Unheld::Step } })?;
    written_up::<N, W>(world, chain, n).ok_or(PathError { module, at: 0, why: Why::SuperPastRoot })
}

/// Whether `module` itself binds `name` in `ns`: its own binding, then its glob edges, the
/// table [`crate::resolve::Names::lookup`] reads, for any module a block's included. Not a
/// written `self::name`: a written `self` passes over a block's module ([`written_up`]), so
/// that walk never reads the block's own items (the lazy run's "a struct in a block .. in a
/// local item's body" for every local struct, 2026-11-17). `Ok(false)`: no binding; an error:
/// the records the answer needs are not held, or the walk was refused.
pub fn binds<N: Name, W: World<N>>(world: &W, module: ItemKey, ns: Namespace, name: N, alloc: Alloc<'_>) -> Result<bool, PathError> {
    binds_in(world, module, ns, name, &mut ThroughScratch::new_in(alloc))
}

/// [`binds`] with the glob walk's working memory the caller's: a caller asking many names keeps
/// one [`ThroughScratch`] for all of them, so no lookup allocates once it is warm.
pub fn binds_in<N: Name, W: World<N>>(
    world: &W,
    module: ItemKey,
    ns: Namespace,
    name: N,
    scratch: &mut ThroughScratch<'_>,
) -> Result<bool, PathError> {
    match in_source(world, Def::module(module), ns, name, scratch) {
        Ok(found) => Ok(found.is_some()),
        Err(why) => Err(PathError { module, at: 0, why }),
    }
}

/// Why a path did not resolve; `at` is the index of the segment that did not.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Why {
    /// No binding of the segment where it was looked for.
    NotFound,
    /// A binding, not visible from the module the path is written in.
    Private,
    /// Two bindings the language refuses to choose between.
    Ambiguous,
    /// A segment before the last named something with no names under it (a fn, a struct).
    NotAModule,
    /// `super` past the crate's root.
    SuperPastRoot,
    /// The records a step needs are not held: a crate whose source is not read yet (a
    /// dependency not fetched), never the user's error. `module` is the one not held; `layer`
    /// is which of the walk's layers it was.
    NotRead { module: ItemKey, layer: Unheld },
    /// A lookup through globs past its bound (rule 18).
    Unbounded,
    /// The path has no segments.
    Empty,
    /// No binding of the segment in `module`, whose macro call `call` was refused (its scope
    /// holds [`blaze_surface::ScopeError::Refused`]): the name may be one the call would have written, so it
    /// is unknown because of that call, never "not found" (rules 3 and 19).
    Refused { module: ItemKey, call: ItemKey, why: RefusedWhy },
}

/// Which layer of a walk met records not held ([`Why::NotRead`]). A reader that knows what a
/// layer can bind decides by it what the answer would have been (the items record's writer:
/// the standard prelude binds no type named like a builtin, buckets.rs `filing`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Unheld {
    /// A module or enum the walk stands in or steps into: the module the path is written in, a
    /// module or enum a segment names, a `$crate`'s root.
    Step,
    /// A module a glob import reaches (`use dep::*;`, `dep` not read): it may bind any name, so
    /// nothing at or after this layer is known.
    Glob,
    /// A `#[macro_use]` crate's root (a macro path's layer).
    MacroUse,
    /// The standard prelude (the crate root's prelude module, or a module its globs reach),
    /// with every nearer layer read and binding nothing of the name. `builtin`: the builtin
    /// type the name is in the one layer after the prelude, the builtin types (a first segment
    /// looked up in the type namespace that names one), else `None`.
    Prelude { builtin: Option<ItemKey> },
}

/// A path that did not resolve: the module it is written in, the failing segment, and why.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct PathError {
    pub module: ItemKey,
    pub at: usize,
    pub why: Why,
}

impl fmt::Display for PathError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let why = match self.why {
            Why::NotFound => "not found",
            Why::Private => "private",
            Why::Ambiguous => "ambiguous",
            Why::NotAModule => "names no module, enum or trait",
            Why::SuperPastRoot => "`super` past the crate's root",
            Why::NotRead { .. } => "dependency source not read",
            Why::Unbounded => "a lookup through globs past its bound",
            Why::Empty => "no segments",
            Why::Refused { why, .. } => why.text(),
        };
        // No module or call key in the words (D191: one body's note in two views, whose block
        // modules have two keys, prints once; rules 19 and 25: the note names the body and the
        // path as written, and a key names nothing a reader can find and differs per unit).
        write!(f, "a path in module where it is written: segment {}: {why}", self.at)?;
        if let Why::Refused { .. } = self.why {
            f.write_str(" (a macro call of a module the path passes through was refused)")?;
        }
        Ok(())
    }
}

/// The first refused macro call of `module`'s scope (none held: `None`), as [`Why::Refused`].
fn refused_in<N: Name, W: World<N>>(world: &W, module: ItemKey) -> Option<Why> {
    let (call, why) = world.scope(module)?.refused()?;
    Some(Why::Refused { module, call, why })
}

/// A path as written: how it begins and its segments.
#[derive(Clone, Copy, Debug)]
pub struct Written<'p, N> {
    pub kind: PathKind,
    pub segments: &'p [N],
}

/// What a macro call's path names.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MacroRes {
    /// The definition (a `macro_rules!` or a `macro`): its crate and its key. Whether it is
    /// `#[rustc_builtin_macro]` is in its source, the caller's.
    Found { krate: CrateItemKey, item: ItemKey },
    NotFound,
    Ambiguous,
    /// The records are not held (a dependency not read).
    NotRead { module: ItemKey },
}

/// What a path's longest resolvable prefix names: `def`, reached after `consumed` segments
/// (`consumed == segments.len()`: the whole path). The walk stops at the first definition with
/// no names under it in the records (a struct, a trait, a fn, an enum whose next segment is no
/// variant); the segments after it (`Type::new`, `Trait::method`, `Enum::assoc`) are the type
/// checker's.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Resolved {
    pub def: Def,
    pub consumed: usize,
}

impl Resolved {
    /// The definition when the whole path of `len` segments resolved here, else `None`.
    pub fn whole(self, len: usize) -> Option<Def> {
        (self.consumed == len).then_some(self.def)
    }
}

/// `path` written in `module`: its longest prefix that names a definition, its last segment
/// looked up in `ns` and every other in the type namespace. An error is a segment that names
/// nothing before any definition with names under it ends the walk.
///
/// A plain path whose first segment is rustc's `{{root}}` (the `::` of `::x::y`, as the type
/// checker's lowering writes it) is the global path `x::y`; positions in the result still count
/// the `{{root}}` segment.
pub fn path<N: Name, W: World<N>>(world: &W, module: ItemKey, ns: Namespace, path: Written<'_, N>, alloc: Alloc<'_>) -> Result<Resolved, PathError> {
    path_in(world, module, ns, path, &mut ThroughScratch::new_in(alloc))
}

/// [`path`] with the glob walk's working memory the caller's: every layer and segment of the
/// walk reuses `scratch` (one per walk at least, where each lookup through globs made its own),
/// and a caller resolving many paths keeps one for all of them. The answer is [`path`]'s: the
/// scratch is cleared per lookup and holds no answer between them.
pub fn path_in<N: Name, W: World<N>>(
    world: &W,
    module: ItemKey,
    ns: Namespace,
    path: Written<'_, N>,
    scratch: &mut ThroughScratch<'_>,
) -> Result<Resolved, PathError> {
    let rooted = path.kind == PathKind::Plain && path.segments.first().is_some_and(|s| s.as_ref() == "{{root}}");
    if !rooted {
        return walk(world, module, ns, path, scratch);
    }
    let global = Written { kind: PathKind::Abs, segments: &path.segments[1..] };
    match walk(world, module, ns, global, scratch) {
        Ok(r) => Ok(Resolved { def: r.def, consumed: r.consumed + 1 }),
        Err(e) if e.why == Why::Empty => Err(PathError { module, at: 0, why: Why::Empty }),
        Err(e) => Err(PathError { at: e.at + 1, ..e }),
    }
}

fn walk<N: Name, W: World<N>>(
    world: &W,
    module: ItemKey,
    ns: Namespace,
    path: Written<'_, N>,
    scratch: &mut ThroughScratch<'_>,
) -> Result<Resolved, PathError> {
    let fail = |at: usize, why: Why| PathError { module, at, why };
    let last = path.segments.len().checked_sub(1).ok_or(fail(0, Why::Empty))?;
    let chain = world.chain(module).ok_or(fail(0, Why::NotRead { module, layer: Unheld::Step }))?;
    let root = world.root_of(module).ok_or(fail(0, Why::NotRead { module, layer: Unheld::Step }))?;
    let head = root.head();
    // Where the walk starts: a module named by the path's head, or the lexical layers.
    let (mut current, first) = match path.kind {
        // `::x` in edition 2015 is the crate root's `x`.
        PathKind::Abs if head.edition == Edition::E2015 => (Some(head.root_module), 0),
        PathKind::Crate => (Some(head.root_module), 0),
        // A written `self` or `super` passes over blocks' modules ([`written_up`]).
        PathKind::SelfMod => match written_up::<N, W>(world, chain, 0) {
            Some(m) => (Some(m), 0),
            None => return Err(fail(0, Why::SuperPastRoot)),
        },
        PathKind::Super(n) => match written_up::<N, W>(world, chain, n) {
            Some(m) => (Some(m), 0),
            None => return Err(fail(0, Why::SuperPastRoot)),
        },
        PathKind::DollarCrate(krate) => match world.root(krate) {
            Some(r) => (Some(r.head().root_module), 0),
            None => return Err(fail(0, Why::NotRead { module, layer: Unheld::Step })),
        },
        PathKind::Abs => match root.extern_crate(path.segments[0]) {
            Some(krate_root) if last > 0 => (Some(krate_root), 1),
            Some(krate_root) => return Ok(Resolved { def: Def::module(krate_root), consumed: 1 }),
            None => return Err(fail(0, Why::NotFound)),
        },
        PathKind::Plain => (None, 0),
    };
    for at in first..=last {
        let name = path.segments[at];
        let ns_here = if at == last { ns } else { Namespace::Types };
        let def = match current {
            None => {
                let def = lexical(world, module, chain, root, ns_here, name, scratch).map_err(|why| fail(at, why))?;
                // A built-in type's name in type position, bound lexically to a module (`use
                // core::str;` then `&str`): the built-in type, since a module is never a type. Only
                // a whole one-segment path in the type namespace is a type position; as a prefix
                // (`str::from_utf8`) the module stands. A struct, enum, alias or trait of the name
                // still shadows the built-in (Reference, "Preludes": prelude names may be shadowed).
                match builtin_type_in(name.as_ref(), head.arbitrary_ints) {
                    Some(builtin) if last == 0 && ns == Namespace::Types && def.kind == DefKind::Module => builtin,
                    _ => def,
                }
            }
            Some(m) => match name.as_ref() {
                "super" => {
                    let c = world.chain(m).ok_or(fail(at, Why::NotRead { module: m, layer: Unheld::Step }))?;
                    Def::module(written_up::<N, W>(world, c, 1).ok_or(fail(at, Why::SuperPastRoot))?)
                }
                "self" => Def::module(m),
                _ => {
                    let found = in_source(world, Def::module(m), ns_here, name, scratch).map_err(|why| fail(at, why))?;
                    let p = match found {
                        Some(p) => p,
                        None => return Err(fail(at, refused_in(world, m).unwrap_or(Why::NotFound))),
                    };
                    if !visible(p.vis, chain, head.krate) {
                        return Err(fail(at, Why::Private));
                    }
                    p.def
                }
            },
        };
        let here = Resolved { def, consumed: at + 1 };
        if at == last {
            return Ok(here);
        }
        current = match def.kind {
            DefKind::Module => Some(def.key),
            DefKind::Enum => {
                // `Enum::Variant` when the next segment is a variant; else `Enum::assoc`, the
                // type checker's.
                let hit = world.variant(def.key, path.segments[at + 1]).ok_or(fail(at, Why::NotRead { module: def.key, layer: Unheld::Step }))?;
                let next_ns = if at + 1 == last { ns } else { Namespace::Types };
                return Ok(match hit.and_then(|v| fits(v, next_ns)) {
                    Some(v) => Resolved { def: v, consumed: at + 2 },
                    None => here,
                });
            }
            // An associated item (`Trait::method`, `Type::new`) is the type checker's.
            _ => return Ok(here),
        };
    }
    Err(fail(last, Why::NotFound))
}

/// A macro call's path, written in `module`.
pub fn macro_path<N: Name, W: World<N>>(world: &W, module: ItemKey, written: Written<'_, N>, alloc: Alloc<'_>) -> MacroRes {
    match path(world, module, Namespace::Macros, written, alloc).map(|r| r.whole(written.segments.len())) {
        Ok(None) => MacroRes::NotFound,
        Ok(Some(def)) => match world.root_of(def.key) {
            Some(r) => MacroRes::Found { krate: r.head().krate, item: def.key },
            None => MacroRes::NotRead { module: def.key },
        },
        Err(PathError { why: Why::Ambiguous, .. }) => MacroRes::Ambiguous,
        Err(PathError { why: Why::NotRead { module, .. }, .. }) => MacroRes::NotRead { module },
        Err(_) => MacroRes::NotFound,
    }
}

/// The traits method resolution may use in `module`: its own trait bindings and `use .. as _`
/// imports, the traits its globs reach (each glob source once), and the standard prelude's;
/// sorted, without repeats.
pub fn traits_in_scope<'a, N: Name, W: World<N>>(world: &W, module: ItemKey, alloc: Alloc<'a>) -> Result<AVec<'a, ItemKey>, PathError> {
    let mut out: AVec<'a, ItemKey> = AVec::new_in(alloc);
    let mut seen: AVec<'a, ItemKey> = AVec::new_in(alloc);
    let mut stack: AVec<'a, ItemKey> = AVec::new_in(alloc);
    stack.push(module);
    if !world.no_implicit_prelude(module) {
        if let Some(prelude) = world.root_of(module).and_then(|r| r.head().prelude) {
            stack.push(prelude);
        }
    }
    while let Some(m) = stack.pop() {
        if seen.contains(&m) {
            continue;
        }
        seen.push(m);
        let Some(scope) = world.scope(m) else {
            if m == module {
                return Err(PathError { module, at: 0, why: Why::NotRead { module, layer: Unheld::Step } });
            }
            continue;
        };
        scope.each_trait(&mut |key| out.push(key));
        for edge in scope.globs().iter() {
            if edge.source.kind == DefKind::Module {
                stack.push(edge.source.key);
            }
        }
    }
    out.sort_unstable();
    out.dedup();
    Ok(out)
}

/// The first segment of a plain path: the lexical layers, in order.
fn lexical<'w, N: Name, W: World<N>>(
    world: &'w W,
    module: ItemKey,
    chain: &[ItemKey],
    root: W::Root<'w>,
    ns: Namespace,
    name: N,
    scratch: &mut ThroughScratch<'_>,
) -> Result<Def, Why> {
    let head = root.head();
    match name.as_ref() {
        "crate" => return Ok(Def::module(head.root_module)),
        "self" => return written_up::<N, W>(world, chain, 0).map(Def::module).ok_or(Why::SuperPastRoot),
        "super" => return written_up::<N, W>(world, chain, 1).map(Def::module).ok_or(Why::SuperPastRoot),
        _ => {}
    }
    if ns == Namespace::Macros {
        // Textual scope: the module's own `macro_rules!`, latest first, then each ancestor's
        // before the `mod` line of the child the chain came through.
        //
        // A body's outermost written block has no `mod` line: its module is an expansion of
        // the body, its `mod` dropped (blocks.rs, resolve.rs `make_blocks`), so its parent (the
        // body's module, or the block around a local item's body) marks no place for it. The
        // block is inside the body and the body is one of the parent's items, so the parent's
        // `macro_rules!` are in the block's textual scope (the Reference, "Macros by example",
        // "Textual scope": a definition's scope "extends to the end of the enclosing module or
        // block"), read as the parent's whole sequence: what every call of a body read before
        // the call was read at its block (the body's module, `child` `None`). Rust's own place
        // is the body item's, so a `macro_rules!` written after the body is in scope here too;
        // a mark at the item's place is the owed record change. An unmarked child that is no
        // block's module is a missing record and stays the empty prefix.
        let mut child: Option<ItemKey> = None;
        for m in chain.iter() {
            if let Some(scope) = world.scope(*m) {
                let upto = match child {
                    Some(c) if !scope.marked(c) && world.anonymous(c) => None,
                    other => other,
                };
                if let Some((key, shapes)) = scope.textual(upto, name) {
                    return Ok(Def { key, kind: DefKind::Macro(shapes) });
                }
            }
            child = Some(*m);
        }
    }
    if let Some(p) = in_source(world, Def::module(module), ns, name, scratch)? {
        return Ok(p.def);
    }
    if ns == Namespace::Macros {
        // The `#[macro_use]` prelude: a `#[macro_use] extern crate` imports the macros the
        // crate exports at its ROOT in the macro namespace, re-exports (`pub use`) included
        // (rustc's `process_macro_use_imports`, every macro child of the crate's root module).
        // std's `vec` is one: `pub use alloc_crate::vec;` (std lib.rs), so a `#![no_std]`
        // crate's `#[macro_use] extern crate std;` (proptest) has `vec!`. A macro of a module
        // under the root without `#[macro_export]` is no binding of the root: not imported.
        //
        // THE SAME RULE is the names step's layer (lookup.rs `lexical_layers`, the
        // `self.macro_use` loop: `layer_name(root, Macros, ..)`), over live modules; this is it
        // over records. Both walk the root's macro namespace by one walk, blaze-surface
        // `through`: `in_source` here, `read_name` there; the `#[macro_export]` rows first are
        // the records' index of the root's own macros (an exported macro is the root's binding
        // for other crates, crate_scope.rs), read without a walk.
        let mut i = 0;
        while let Some(krate_root) = root.macro_use(i) {
            let Some(other) = world.root_of(krate_root) else { return Err(Why::NotRead { module: krate_root, layer: Unheld::MacroUse }) };
            if let Some((key, shapes)) = other.macro_export(name) {
                return Ok(Def { key, kind: DefKind::Macro(shapes) });
            }
            let found = in_source(world, Def::module(krate_root), ns, name, scratch).map_err(|why| match why {
                // The crate's root, or a module behind its globs, not held is this layer's.
                Why::NotRead { module, .. } => Why::NotRead { module, layer: Unheld::MacroUse },
                why => why,
            })?;
            if let Some(p) = found
                && matches!(p.def.kind, DefKind::Macro(_))
                && visible(p.vis, chain, head.krate)
            {
                return Ok(p.def);
            }
            i += 1;
        }
    }
    if ns == Namespace::Types {
        if let Some(krate_root) = root.extern_crate(name) {
            return Ok(Def::module(krate_root));
        }
    }
    let builtin = if ns == Namespace::Types { builtin_type_in(name.as_ref(), head.arbitrary_ints) } else { None };
    if !world.no_implicit_prelude(module) {
        if let Some(prelude) = head.prelude {
            let unheld = Unheld::Prelude { builtin: builtin.map(|d| d.key) };
            if world.scope(prelude).is_none() {
                return Err(Why::NotRead { module: prelude, layer: unheld });
            }
            let found = in_source(world, Def::module(prelude), ns, name, scratch).map_err(|why| match why {
                // A glob of the prelude's into a module not held is still the prelude's layer.
                Why::NotRead { module, .. } => Why::NotRead { module, layer: unheld },
                why => why,
            })?;
            if let Some(p) = found {
                return Ok(p.def);
            }
        }
    }
    if ns == Namespace::Types {
        if let Some(def) = builtin {
            return Ok(def);
        }
    }
    // Found in no layer: unknown when the module's own refused call could have written it.
    Err(refused_in(world, module).unwrap_or(Why::NotFound))
}

/// `name` in `source` (a module, through its globs, or an enum).
fn in_source<N: Name, W: World<N>>(
    world: &W,
    source: Def,
    ns: Namespace,
    name: N,
    scratch: &mut ThroughScratch<'_>,
) -> Result<Option<Passed>, Why> {
    struct Records<'w, N, W> {
        world: &'w W,
        ns: Namespace,
        name: N,
    }
    impl<'w, N: Name + 'w, W: World<N>> Sources<'w> for Records<'w, N, W> {
        type Stop = ItemKey;
        fn node(&mut self, source: Def) -> Result<Node<'w>, ItemKey> {
            let world: &'w W = self.world;
            if source.kind == DefKind::Enum {
                let hit = world.variant(source.key, self.name).ok_or(source.key)?;
                let found = hit.and_then(|v| fits(v, self.ns));
                return Ok(Node::Value(found.map(|def| Passed { def, vis: Visibility::Public, state: BindingState::Determined })));
            }
            let scope = world.scope(source.key).ok_or(source.key)?;
            if let Some(b) = scope.get(self.ns, self.name) {
                return Ok(Node::Value(Some(Passed { def: b.def, vis: b.vis, state: b.state })));
            }
            let chain = world.chain(source.key).ok_or(source.key)?;
            let krate = world.root_of(source.key).ok_or(source.key)?.head().krate;
            Ok(Node::Edges { edges: scope.globs(), krate, chain })
        }
    }
    let mut sources = Records { world, ns, name };
    match through_in(&mut sources, source, scratch) {
        Ok(Some(p)) if matches!(p.state, BindingState::Ambiguous(_)) => Err(Why::Ambiguous),
        Ok(found) => Ok(found),
        // `source` itself not held is the walk's own step; a module behind its globs is a glob's.
        Err(Through::Stop(module)) => Err(Why::NotRead { module, layer: if module == source.key { Unheld::Step } else { Unheld::Glob } }),
        Err(Through::Unbounded) => Err(Why::Unbounded),
    }
}

/// A variant found by name, as a definition in `ns`: a record variant is a type only, any other
/// a value too.
fn fits(v: VariantHit, ns: Namespace) -> Option<Def> {
    let fits = match ns {
        Namespace::Types => true,
        Namespace::Values => !v.record,
        Namespace::Macros => false,
    };
    fits.then_some(Def { key: v.key, kind: DefKind::Other })
}

/// Whether a binding of visibility `vis` is visible from the module whose chain is `chain`.
fn visible(vis: Visibility, chain: &[ItemKey], krate: CrateItemKey) -> bool {
    match vis {
        Visibility::Public => true,
        Visibility::Crate(k) => k == krate,
        Visibility::Restricted(m) => chain.contains(&m),
    }
}
