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
    BindingState, CrateRootBuilder, Def, DefKind, Edition, Namespace, Node, Passed, PathKind, RefusedWhy, ScopeBuilder, ScopeError,
    Sources, Through, Visibility, through,
};

use crate::builtin::builtin_type_in;
use crate::facts::Name;
use crate::task::Variant;

/// The finished records a path is read through.
pub trait World<N> {
    /// The module's finished scope, or `None` when no record of it is held.
    fn scope(&self, module: ItemKey) -> Option<&ScopeBuilder<'_, N>>;
    /// The module's chain up to its crate's root, itself first.
    fn chain(&self, module: ItemKey) -> Option<&[ItemKey]>;
    /// The root record of the crate `key` (any item of it) is of.
    fn root_of(&self, key: ItemKey) -> Option<&CrateRootBuilder<'_, N>>;
    /// The root record of the crate.
    fn root(&self, krate: CrateItemKey) -> Option<&CrateRootBuilder<'_, N>>;
    /// An enum's variants, or `None` when `owner` is no enum held.
    fn variants(&self, owner: ItemKey) -> Option<&[Variant<N>]>;
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
    /// dependency not fetched), never the user's error.
    NotRead { module: ItemKey },
    /// A lookup through globs past its bound (rule 18).
    Unbounded,
    /// The path has no segments.
    Empty,
    /// No binding of the segment in `module`, whose macro call `call` was refused (its scope
    /// holds [`ScopeError::Refused`]): the name may be one the call would have written, so it
    /// is unknown because of that call, never "not found" (rules 3 and 19).
    Refused { module: ItemKey, call: ItemKey, why: RefusedWhy },
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
        write!(f, "a path in module {:x}: segment {}: {why}", self.module, self.at)?;
        if let Why::Refused { module, call, .. } = self.why {
            write!(f, " (macro call {call:x} in module {module:x})")?;
        }
        Ok(())
    }
}

/// The first refused macro call of `module`'s scope (none held: `None`), as [`Why::Refused`].
fn refused_in<N: Name, W: World<N>>(world: &W, module: ItemKey) -> Option<Why> {
    world.scope(module)?.errors.iter().find_map(|e| match e {
        ScopeError::Refused { call, why, .. } => Some(Why::Refused { module, call: *call, why: *why }),
        _ => None,
    })
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
    let rooted = path.kind == PathKind::Plain && path.segments.first().is_some_and(|s| s.as_ref() == "{{root}}");
    if !rooted {
        return walk(world, module, ns, path, alloc);
    }
    let global = Written { kind: PathKind::Abs, segments: &path.segments[1..] };
    match walk(world, module, ns, global, alloc) {
        Ok(r) => Ok(Resolved { def: r.def, consumed: r.consumed + 1 }),
        Err(e) if e.why == Why::Empty => Err(PathError { module, at: 0, why: Why::Empty }),
        Err(e) => Err(PathError { at: e.at + 1, ..e }),
    }
}

fn walk<N: Name, W: World<N>>(world: &W, module: ItemKey, ns: Namespace, path: Written<'_, N>, alloc: Alloc<'_>) -> Result<Resolved, PathError> {
    let fail = |at: usize, why: Why| PathError { module, at, why };
    let last = path.segments.len().checked_sub(1).ok_or(fail(0, Why::Empty))?;
    let chain = world.chain(module).ok_or(fail(0, Why::NotRead { module }))?;
    let root = world.root_of(module).ok_or(fail(0, Why::NotRead { module }))?;
    // Where the walk starts: a module named by the path's head, or the lexical layers.
    let (mut current, first) = match path.kind {
        // `::x` in edition 2015 is the crate root's `x`.
        PathKind::Abs if root.head.edition == Edition::E2015 => (Some(root.head.root_module), 0),
        PathKind::Crate => (Some(root.head.root_module), 0),
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
            Some(r) => (Some(r.head.root_module), 0),
            None => return Err(fail(0, Why::NotRead { module })),
        },
        PathKind::Abs => {
            let name = path.segments[0];
            match root.extern_prelude.binary_search_by(|(n, _)| n.cmp(&name)) {
                Ok(at) if last > 0 => (Some(root.extern_prelude[at].1), 1),
                Ok(at) => return Ok(Resolved { def: Def::module(root.extern_prelude[at].1), consumed: 1 }),
                Err(_) => return Err(fail(0, Why::NotFound)),
            }
        }
        PathKind::Plain => (None, 0),
    };
    for at in first..=last {
        let name = path.segments[at];
        let ns_here = if at == last { ns } else { Namespace::Types };
        let def = match current {
            None => {
                let def = lexical(world, module, chain, root, ns_here, name, alloc).map_err(|why| fail(at, why))?;
                // A built-in type's name in type position, bound lexically to a module (`use
                // core::str;` then `&str`): the built-in type, since a module is never a type. Only
                // a whole one-segment path in the type namespace is a type position; as a prefix
                // (`str::from_utf8`) the module stands. A struct, enum, alias or trait of the name
                // still shadows the built-in (Reference, "Preludes": prelude names may be shadowed).
                match builtin_type_in(name.as_ref(), root.head.arbitrary_ints) {
                    Some(builtin) if last == 0 && ns == Namespace::Types && def.kind == DefKind::Module => builtin,
                    _ => def,
                }
            }
            Some(m) => match name.as_ref() {
                "super" => {
                    let c = world.chain(m).ok_or(fail(at, Why::NotRead { module: m }))?;
                    Def::module(written_up::<N, W>(world, c, 1).ok_or(fail(at, Why::SuperPastRoot))?)
                }
                "self" => Def::module(m),
                _ => {
                    let found = in_source(world, Def::module(m), ns_here, name, alloc).map_err(|why| fail(at, why))?;
                    let p = match found {
                        Some(p) => p,
                        None => return Err(fail(at, refused_in(world, m).unwrap_or(Why::NotFound))),
                    };
                    if !visible(p.vis, chain, root.head.krate) {
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
                let variants = world.variants(def.key).ok_or(fail(at, Why::NotRead { module: def.key }))?;
                let next_ns = if at + 1 == last { ns } else { Namespace::Types };
                return Ok(match variant(variants, next_ns, path.segments[at + 1]) {
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
            Some(r) => MacroRes::Found { krate: r.head.krate, item: def.key },
            None => MacroRes::NotRead { module: def.key },
        },
        Err(PathError { why: Why::Ambiguous, .. }) => MacroRes::Ambiguous,
        Err(PathError { why: Why::NotRead { module }, .. }) => MacroRes::NotRead { module },
        Err(_) => MacroRes::NotFound,
    }
}

/// The traits method resolution may use in `module`: its own trait bindings and `use .. as _`
/// imports, the traits its globs reach (each glob source once), and the standard prelude's;
/// sorted, without repeats.
pub fn traits_in_scope<'a, N: Name, W: World<N>>(world: &W, module: ItemKey, alloc: Alloc<'a>) -> Result<AVec<'a, ItemKey>, PathError> {
    let mut out: AVec<'a, ItemKey> = AVec::new_in(alloc.clone());
    let mut seen: AVec<'a, ItemKey> = AVec::new_in(alloc.clone());
    let mut stack: AVec<'a, ItemKey> = AVec::new_in(alloc.clone());
    stack.push(module);
    if !world.no_implicit_prelude(module) {
        if let Some(prelude) = world.root_of(module).and_then(|r| r.head.prelude) {
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
                return Err(PathError { module, at: 0, why: Why::NotRead { module } });
            }
            continue;
        };
        for entry in scope.bindings.iter() {
            if entry.ns == Namespace::Types && entry.binding.def.kind == DefKind::Trait {
                out.push(entry.binding.def.key);
            }
        }
        for anon in scope.traits.iter() {
            out.push(anon.def.key);
        }
        for edge in scope.globs.iter() {
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
fn lexical<N: Name, W: World<N>>(
    world: &W,
    module: ItemKey,
    chain: &[ItemKey],
    root: &CrateRootBuilder<'_, N>,
    ns: Namespace,
    name: N,
    alloc: Alloc<'_>,
) -> Result<Def, Why> {
    match name.as_ref() {
        "crate" => return Ok(Def::module(root.head.root_module)),
        "self" => return written_up::<N, W>(world, chain, 0).map(Def::module).ok_or(Why::SuperPastRoot),
        "super" => return written_up::<N, W>(world, chain, 1).map(Def::module).ok_or(Why::SuperPastRoot),
        _ => {}
    }
    if ns == Namespace::Macros {
        // Textual scope: the module's own `macro_rules!`, latest first, then each ancestor's
        // before the `mod` line of the child the chain came through.
        let mut child: Option<ItemKey> = None;
        for m in chain.iter() {
            if let Some(scope) = world.scope(*m) {
                let end = match child {
                    None => scope.textual.len(),
                    Some(c) => scope.marks.iter().find(|mark| mark.child == c).map_or(0, |mark| mark.len as usize),
                };
                if let Some(def) = scope.textual[..end.min(scope.textual.len())].iter().rev().find(|d| d.name == name) {
                    return Ok(Def { key: def.def, kind: DefKind::Macro(def.shapes) });
                }
            }
            child = Some(*m);
        }
    }
    if let Some(p) = in_source(world, Def::module(module), ns, name, alloc.clone())? {
        return Ok(p.def);
    }
    if ns == Namespace::Macros {
        for krate_root in root.macro_use.iter() {
            let Some(other) = world.root_of(*krate_root) else { return Err(Why::NotRead { module: *krate_root }) };
            if let Ok(at) = other.macro_exports.binary_search_by(|(n, _, _)| n.as_ref().cmp(name.as_ref())) {
                let (_, key, shapes) = other.macro_exports[at];
                return Ok(Def { key, kind: DefKind::Macro(shapes) });
            }
        }
    }
    if ns == Namespace::Types {
        if let Ok(at) = root.extern_prelude.binary_search_by(|(n, _)| n.as_ref().cmp(name.as_ref())) {
            return Ok(Def::module(root.extern_prelude[at].1));
        }
    }
    if !world.no_implicit_prelude(module) {
        if let Some(prelude) = root.head.prelude {
            if world.scope(prelude).is_none() {
                return Err(Why::NotRead { module: prelude });
            }
            if let Some(p) = in_source(world, Def::module(prelude), ns, name, alloc)? {
                return Ok(p.def);
            }
        }
    }
    if ns == Namespace::Types {
        if let Some(def) = builtin_type_in(name.as_ref(), root.head.arbitrary_ints) {
            return Ok(def);
        }
    }
    // Found in no layer: unknown when the module's own refused call could have written it.
    Err(refused_in(world, module).unwrap_or(Why::NotFound))
}

/// `name` in `source` (a module, through its globs, or an enum).
fn in_source<N: Name, W: World<N>>(world: &W, source: Def, ns: Namespace, name: N, alloc: Alloc<'_>) -> Result<Option<Passed>, Why> {
    struct Records<'w, N, W> {
        world: &'w W,
        ns: Namespace,
        name: N,
    }
    impl<'w, N: Name + 'w, W: World<N>> Sources<'w> for Records<'w, N, W> {
        type Stop = ItemKey;
        fn node(&mut self, source: Def) -> Result<Node<'w>, ItemKey> {
            if source.kind == DefKind::Enum {
                let variants = self.world.variants(source.key).ok_or(source.key)?;
                let found = variant(variants, self.ns, self.name);
                return Ok(Node::Value(found.map(|def| Passed { def, vis: Visibility::Public, state: BindingState::Determined })));
            }
            let scope = self.world.scope(source.key).ok_or(source.key)?;
            if let Some(b) = scope.get(self.ns, self.name) {
                return Ok(Node::Value(Some(Passed { def: b.def, vis: b.vis, state: b.state })));
            }
            let chain = self.world.chain(source.key).ok_or(source.key)?;
            let krate = self.world.root_of(source.key).ok_or(source.key)?.head.krate;
            Ok(Node::Edges { edges: &scope.globs, krate, chain })
        }
    }
    let mut sources = Records { world, ns, name };
    match through(&mut sources, source, alloc) {
        Ok(Some(p)) if matches!(p.state, BindingState::Ambiguous(_)) => Err(Why::Ambiguous),
        Ok(found) => Ok(found),
        Err(Through::Stop(module)) => Err(Why::NotRead { module }),
        Err(Through::Unbounded) => Err(Why::Unbounded),
    }
}

/// A variant of `variants` by name: a record variant is a type only, any other a value too.
fn variant<N: Name>(variants: &[Variant<N>], ns: Namespace, name: N) -> Option<Def> {
    let v = variants.iter().find(|v| v.name.as_ref() == name.as_ref())?;
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
