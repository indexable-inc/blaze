//! The traits in scope at each block of a body: the module's ([`super::body::Body::traits`])
//! and the traits the body's blocks bring in by `use` (named, `as _`, a glob) or declare
//! (`trait SpecWriteFmt { .. }` in core's `Write::write_fmt`: `Reader::declared`), each
//! block's inherited by the blocks inside it and by the bodies of items declared there.
//!
//! The Reference, "Use declarations" [items.use.intro]: "These declarations may appear in
//! modules and blocks, usually at the top." [items.use.as-underscore.intro]: "Items can be
//! imported without binding to a name by using an underscore with the form use path as _.
//! This is particularly useful to import a trait so that its methods may be used without
//! importing the trait's symbol". "Method call expressions": "Any of the methods provided by
//! a visible trait implemented by T. [..] Then all remaining methods in scope are looked up."
//!
//! tracing's `trace!`/`info!`/`debug!` write `use $crate::__macro_support::Callsite as _;` in
//! the block they expand to and then call `__CALLSITE.metadata()`; tempo_alloy writes `use
//! std::os::unix::fs::OpenOptionsExt;` inside a fn and calls `options.mode(0o600)`. Both are
//! method calls through a trait only a block imports.
//!
//! How each `use` row of a block ([`super::locals`]) is read: its path, at the body's module
//! (as [`super::world_paths`] reads a block's named import), after its first segment is taken
//! through the block imports around it (a named import's path stands for it; a block's
//! struct, enum, union or alias has only variants under it, never a trait). Then:
//! - a named or `as _` import: the trait its path names in the type namespace; a path that
//!   names no trait (a module, a type, a fn, a macro: asked in the value and macro namespaces
//!   too) brings none;
//! - a glob: the traits the names library's `traits_in_scope` gives at the glob's module (its
//!   own, its imports', its globs', its prelude's: the library reads a module's glob the same
//!   way, visibility aside); an enum's glob brings none.
//!
//! Refused by name, lazily, at the first method call in a block where the answer depends on
//! it ([`InScope::Refused`], the body's [`super::world_items::Gap::Local`]): a `use` whose path
//! does not resolve or resolves only in part ([`UNRESOLVED`], [`PARTIAL`]), a trait alias
//! imported ([`TRAIT_ALIAS`]) or declared ([`LOCAL_TRAIT_ALIAS`]), a trait declared in a
//! block whose block module the names library did not make or does not read it in
//! ([`LocalKind::unplaced`], [`LOCAL_TRAIT`]), a glob of something that is no module or enum ([`GLOB_NOT_MODULE`]), a glob
//! of a module whose prelude is larger than the body's ([`GLOB_LATER_EDITION`]: its prelude's
//! traits would be in scope here though the body's prelude has not them), a path through
//! another block's item that is no type or import ([`THROUGH_LOCAL`]), and import chains or
//! nesting past their bounds ([`CHAIN`], [`NESTING`]). Known difference, not refused: a glob
//! read at a body whose module is `#![no_implicit_prelude]` brings the glob module's prelude.

use blaze_id::ItemKey as LibKey;
use blaze_own::{AVec, Alloc};
use blaze_scope::names::{Why as PathWhy, Written};
use blaze_scope::task::to_path_marked;
use blaze_surface::{DefKind, Edition, Namespace, PathKind};
use blaze_typeck::ItemKey;

use super::glob_uses::GLOB;
use super::keys::checker_of;
use super::locals::{BodyLocals, LocalGap, LocalKind, LocalNs, Locals, Row};
use super::source::NamesSource;
use crate::resolve::{Kind, Paths};

/// The name a `use path as _;` row of a block is kept under: no identifier is it (it holds a
/// space), so [`Locals::find`] never answers it for a written name.
pub const UNDERSCORE: &str = "as _";

/// The most items a body is nested in (a fn in a block of a fn in a block ..) whose blocks'
/// imports are read for it (rule 18).
pub const MOST_NESTING: u32 = 64;

/// The most block imports one `use` path is taken through before its module reads it.
pub const MOST_CHAIN: u32 = 64;

pub const UNRESOLVED: &str = "a use declaration in a block whose path the names library does not resolve (the traits it brings into scope are not known)";
pub const PARTIAL: &str = "a use declaration in a block whose path the names library resolves only in part";
pub const TRAIT_ALIAS: &str = "a trait alias imported in a block (its traits' methods would be in scope there)";
pub const LOCAL_TRAIT: &str = "a trait declared in a block whose block's module does not read its name as a trait";
pub const LOCAL_TRAIT_ALIAS: &str = "a trait alias declared in a block (its traits' methods would be in scope there)";
pub const GLOB_NOT_MODULE: &str = "a glob use in a block of an item that is no module or enum";
pub const GLOB_LATER_EDITION: &str = "a glob use in a block of a module whose crate's edition has a larger prelude than the body's";
pub const THROUGH_LOCAL: &str = "a use declaration in a block whose path starts at a block's item that is no type or import";
pub const CHAIN: &str = "a use declaration in a block whose path goes through more block imports than the bound";
pub const NESTING: &str = "a body nested in more items than the bound, whose outer blocks' imports are not read";

/// One block's answer, as kept.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum At {
    /// `keys[start..end]`, sorted.
    Traits { start: u32, end: u32 },
    Refused(LocalGap),
}

/// What [`BlockTraits::at`] answers for a block.
#[derive(Clone, Copy, Debug)]
pub enum InScope<'a> {
    /// The module's traits: the body's `traits`.
    Module,
    /// The block's own list (the module's and its imports'), sorted.
    Traits(&'a [ItemKey]),
    /// The block's traits are not known: the body's gap.
    Refused(LocalGap),
}

/// The traits in scope at each block of one body that differ from its module's.
pub struct BlockTraits {
    /// A block with no entry of its own; `None`: the module's.
    default: Option<At>,
    /// Sorted by block (`ScopeId`).
    scopes: AVec<'static, (u32, At)>,
    keys: AVec<'static, ItemKey>,
}

impl BlockTraits {
    /// Every block answers the module's traits.
    pub fn module(alloc: Alloc<'static>) -> BlockTraits {
        BlockTraits { default: None, scopes: AVec::new_in(alloc), keys: AVec::new_in(alloc) }
    }

    /// The traits in scope at block `scope` of the body.
    pub fn at(&self, scope: u32) -> InScope<'_> {
        let at = match self.scopes.binary_search_by_key(&scope, |e| e.0) {
            Ok(i) => Some(self.scopes[i].1),
            Err(_) => self.default,
        };
        match at {
            None => InScope::Module,
            Some(At::Traits { start, end }) => InScope::Traits(&self.keys[start as usize..end as usize]),
            Some(At::Refused(gap)) => InScope::Refused(gap),
        }
    }

    /// `own` is in scope in every block too (a member's own trait, [`super::own_trait`]): each
    /// kept list gets it, still sorted; lists shared by blocks stay shared.
    pub fn add(&mut self, own: ItemKey, alloc: Alloc<'static>) {
        let mut keys: AVec<'static, ItemKey> = AVec::new_in(alloc);
        let mut scopes: AVec<'static, (u32, At)> = AVec::with_capacity_in(self.scopes.len(), alloc);
        // Old range to new, for each distinct range met.
        let mut moved: AVec<'_, (u32, u32, u32, u32)> = AVec::new_in(alloc);
        let mut redo = |at: At, keys: &mut AVec<'static, ItemKey>| -> At {
            let At::Traits { start, end } = at else { return at };
            if let Some(&(_, _, s, e)) = moved.iter().find(|m| m.0 == start && m.1 == end) {
                return At::Traits { start: s, end: e };
            }
            let s = keys.len() as u32;
            let old = &self.keys[start as usize..end as usize];
            keys.extend_from_slice(old);
            if old.binary_search(&own).is_err() {
                keys.push(own);
                keys[s as usize..].sort_unstable();
            }
            let e = keys.len() as u32;
            moved.push((start, end, s, e));
            At::Traits { start: s, end: e }
        };
        let default = self.default.map(|d| redo(d, &mut keys));
        for &(s, at) in self.scopes.iter() {
            scopes.push((s, redo(at, &mut keys)));
        }
        self.default = default;
        self.scopes = scopes;
        self.keys = keys;
    }
}

/// The rank of an edition's standard prelude: 2015 and 2018 share one, 2021 adds to it, 2024
/// adds again (the Reference, "Preludes", [names.preludes.std]).
fn prelude_rank(e: Edition) -> u8 {
    match e {
        Edition::E2015 | Edition::E2018 => 0,
        Edition::E2021 => 1,
        Edition::E2024 => 2,
    }
}

/// Whether `segment` is a path head the names library reads itself.
fn is_head(segment: &str) -> bool {
    matches!(segment, "self" | "super" | "crate" | "$crate")
}

/// Where a body is, for reading its blocks' imports.
struct Reader<'l, 'n, S> {
    names: S,
    locals: &'l Locals<'n>,
    module: LibKey,
    /// The body's crate's prelude rank; `None`: its edition is not known.
    rank: Option<u8>,
}

impl<'l, 'n, S: NamesSource<'n>> Reader<'l, 'n, S> {
    fn gap(row: &Row<'_>, form: &'static str) -> LocalGap {
        LocalGap { item: row.key, form, range: row.range }
    }

    /// The traits `row`, an item of a block of the body `owner`, brings into scope there,
    /// appended to `out` (checker keys, unsorted).
    fn row(&self, owner: ItemKey, row: &Row<'n>, out: &mut AVec<'_, ItemKey>, alloc: Alloc<'_>) -> Result<(), LocalGap> {
        match row.kind {
            LocalKind::Trait => return self.declared(row, out),
            LocalKind::Use => {}
            _ => return Ok(()),
        }
        // The path, its first segment taken through the block imports around the row.
        let mut abs = row.abs;
        // The mark of the path's first segment: the row's, or, once that segment is taken
        // through a block import, that import's (its path's first segment is first then).
        let mut first_mark = row.first_mark;
        let mut path: AVec<'_, &str> = AVec::new_in(alloc);
        path.extend_from_slice(row.path);
        // Where the path is walked: the body's module, or the block module of the block `mod`
        // or `extern crate` its first segment names (the scope table's, `Row::module`).
        let mut at = self.module;
        let mut steps = 0u32;
        loop {
            let first = match path.first() {
                Some(&f) if !abs && !is_head(f) => f,
                _ => break,
            };
            let Some(local) = self.locals.find(owner, Some(row.scope), first, LocalNs::Type) else { break };
            // The row's own statement binds the name (`use log::log;`): read at the module.
            if local.kind == LocalKind::Use && local.scope == row.scope && local.range == row.range {
                break;
            }
            match local.kind {
                LocalKind::Use if local.name != GLOB && local.name != UNDERSCORE => {
                    steps += 1;
                    if steps > MOST_CHAIN {
                        return Err(Self::gap(row, CHAIN));
                    }
                    let mut through: AVec<'_, &str> = AVec::new_in(alloc);
                    through.extend_from_slice(local.path);
                    through.extend_from_slice(&path[1..]);
                    path = through;
                    abs = local.abs;
                    first_mark = local.first_mark;
                }
                // A block's type: only its variants are under it, never a trait. A block's
                // trait: `use Tr as _;` names it, and it is in scope here already (its block
                // is this row's or one around it: [`Self::declared`]); its associated items
                // are no traits. A block's `mod` or `extern crate` may hold a trait: the path is
                // walked at the block's module, where it is an item (super::scope_table); one
                // whose block the library filed no module for is refused by name.
                LocalKind::Mod | LocalKind::ExternCrate => match local.module {
                    Some(m) => {
                        at = m;
                        break;
                    }
                    None => return Err(LocalGap { item: local.key, form: local.kind.unplaced(), range: row.range }),
                },
                k if k.in_block_module() => return Ok(()),
                _ => return Err(LocalGap { item: local.key, form: THROUGH_LOCAL, range: row.range }),
            }
        }
        let krate = self.names.crate_of(self.module).ok_or(Self::gap(row, UNRESOLVED))?;
        let krate_key = self.names.crate_key(krate).ok_or(Self::gap(row, UNRESOLVED))?;
        // A `$crate` (or an opaque `crate`) a macro wrote names the macro's crate, by its mark
        // (`Row::first_mark`): read as the names library reads a module's `use`.
        let written = to_path_marked(abs, &path, first_mark, krate_key, alloc);
        let w = Written { kind: written.kind, segments: &written.segments };
        if row.name == GLOB {
            return self.glob(row, w, krate, at, out, alloc);
        }
        let len = w.segments.len();
        match self.names.walk(at, Namespace::Types, w) {
            Ok(r) if r.consumed == len => match r.def.kind {
                DefKind::Trait => out.push(checker_of(r.def.key)),
                DefKind::Other => {
                    if let Ok(Kind::TraitAlias) = (Paths { names: self.names }).kind_of(r.def) {
                        return Err(Self::gap(row, TRAIT_ALIAS));
                    }
                }
                DefKind::Module | DefKind::Enum | DefKind::Macro(_) => {}
            },
            // `use E::V;`: the walk stops at the enum; a variant is no trait.
            Ok(r) if r.def.kind == DefKind::Enum && r.consumed + 1 == len => {}
            Ok(_) => return Err(Self::gap(row, PARTIAL)),
            // No type of the name: a fn, a const, a macro (`use tracing::info;`) brings no trait.
            // Nor does a type of the name that is not visible here: an import takes, in each
            // namespace, only a binding it may name (the Reference, "Use declarations"; "Visibility
            // and privacy"), so core's `mod poll_fn;` (private) beside `pub use
            // poll_fn::poll_fn;` leaves `use std::future::poll_fn;` (tokio's `Receiver::recv`)
            // the fn alone. The walk answers `Private` for that module at the last segment; when
            // no namespace resolves it either, the `use` is refused below as before.
            Err(e) if matches!(e.why, PathWhy::NotFound | PathWhy::Private) && e.at + 1 == len => {
                let value = self.names.walk(at, Namespace::Values, w).is_ok_and(|r| r.consumed == len);
                if !value && !self.names.walk(at, Namespace::Macros, w).is_ok_and(|r| r.consumed == len) {
                    return Err(Self::gap(row, UNRESOLVED));
                }
            }
            Err(_) => return Err(Self::gap(row, UNRESOLVED)),
        }
        Ok(())
    }

    /// The trait a block declares (`trait SpecWriteFmt { .. }` in core's `Write::write_fmt`),
    /// in scope in its block and the blocks inside it. The Reference, "Statements", "Item
    /// declarations": "Declaring an item within a statement block restricts its scope to the
    /// block containing the statement"; "Method call expressions": "Then all remaining methods
    /// in scope are looked up." It is an item of its block's module in the names library
    /// ([`LocalKind::in_block_module`], [`Row::module`]): its one key is the one the name
    /// solver answers for its name there, the key its head is made under and the impls of the
    /// block that name it are filed under, as for a module's trait. Refused by name: a block
    /// whose module the library did not make ([`LocalKind::unplaced`]), a trait alias
    /// ([`LOCAL_TRAIT_ALIAS`]), and a name the module does not read as a trait
    /// ([`LOCAL_TRAIT`]).
    fn declared(&self, row: &Row<'n>, out: &mut AVec<'_, ItemKey>) -> Result<(), LocalGap> {
        let Some(module) = row.module else { return Err(Self::gap(row, row.kind.unplaced())) };
        let segments = [row.name];
        let w = Written { kind: PathKind::Plain, segments: &segments };
        match self.names.walk(module, Namespace::Types, w) {
            Ok(r) if r.consumed == 1 => match r.def.kind {
                DefKind::Trait => {
                    out.push(checker_of(r.def.key));
                    Ok(())
                }
                DefKind::Other if matches!((Paths { names: self.names }).kind_of(r.def), Ok(Kind::TraitAlias)) => Err(Self::gap(row, LOCAL_TRAIT_ALIAS)),
                _ => Err(Self::gap(row, LOCAL_TRAIT)),
            },
            _ => Err(Self::gap(row, LOCAL_TRAIT)),
        }
    }

    /// The traits a glob row `use <w>::*;` brings into scope.
    fn glob(&self, row: &Row<'n>, w: Written<'_, &str>, krate: usize, at: LibKey, out: &mut AVec<'_, ItemKey>, alloc: Alloc<'_>) -> Result<(), LocalGap> {
        let target = if w.segments.is_empty() {
            // `self::*`, `super::*`, `crate::*`: the module the head names. `self` and `super`
            // as the names library reads a path's head: a block's module is passed over (the
            // Reference, `paths.qualifiers.mod-self`, `paths.qualifiers.super`).
            match w.kind {
                PathKind::SelfMod => self.names.written_module(self.module, 0).map_err(|_| Self::gap(row, UNRESOLVED))?,
                PathKind::Crate => self.names.root_module(krate).ok_or(Self::gap(row, UNRESOLVED))?,
                PathKind::Super(n) => self.names.written_module(self.module, n).map_err(|_| Self::gap(row, UNRESOLVED))?,
                _ => return Err(Self::gap(row, UNRESOLVED)),
            }
        } else {
            let len = w.segments.len();
            match self.names.walk(at, Namespace::Types, w) {
                Ok(r) if r.consumed == len => match r.def.kind {
                    DefKind::Module => r.def.key,
                    DefKind::Enum => return Ok(()),
                    _ => return Err(Self::gap(row, GLOB_NOT_MODULE)),
                },
                Ok(_) => return Err(Self::gap(row, PARTIAL)),
                Err(_) => return Err(Self::gap(row, UNRESOLVED)),
            }
        };
        // The glob's module's prelude must add nothing the body's lacks.
        let theirs = self.names.crate_of(target).and_then(|k| self.names.edition_of(k)).map(prelude_rank);
        match (theirs, self.rank) {
            (Some(t), Some(ours)) if t <= ours => {}
            (Some(_), Some(_)) => return Err(Self::gap(row, GLOB_LATER_EDITION)),
            _ => return Err(Self::gap(row, UNRESOLVED)),
        }
        let traits = self.names.traits_in_scope(target, alloc).map_err(|_| Self::gap(row, UNRESOLVED))?;
        for &k in traits.iter() {
            out.push(checker_of(k));
        }
        Ok(())
    }

    /// The traits the rows of block `scope` of `body` and of every block around it bring in,
    /// appended to `out`; the innermost refusal, if any.
    fn chain(&self, owner: ItemKey, body: &BodyLocals<'n>, scope: u32, out: &mut AVec<'_, ItemKey>, alloc: Alloc<'_>) -> Option<LocalGap> {
        let mut refused = None;
        let mut here = Some(scope);
        // A chain is at most as long as the body's blocks.
        for _ in 0..=body.parents.len() {
            let Some(h) = here else { break };
            for row in body.rows.iter().filter(|r| r.scope == h) {
                if let Err(g) = self.row(owner, row, out, alloc) {
                    refused = refused.or(Some(g));
                }
            }
            here = body.parents.get(h as usize).copied().flatten();
        }
        refused
    }
}

/// Appends `list` to `keys` unless the last kept list is equal: its range.
fn keep_list(keys: &mut AVec<'static, ItemKey>, last: &mut Option<(u32, u32)>, list: &[ItemKey]) -> At {
    if let Some((s, e)) = *last {
        if &keys[s as usize..e as usize] == list {
            return At::Traits { start: s, end: e };
        }
    }
    let s = keys.len() as u32;
    keys.extend_from_slice(list);
    let e = keys.len() as u32;
    *last = Some((s, e));
    At::Traits { start: s, end: e }
}

/// The traits in scope at each block of the body `owner` (whose module is `module`, of crate
/// `krate`) whose list is not `module_traits` (sorted checker keys, the body's `traits`).
/// Every kept list is in `alloc`; the scratch too.
pub fn of<'n, S: NamesSource<'n>>(names: S, locals: &Locals<'n>, owner: ItemKey, module: LibKey, krate: usize, module_traits: &[ItemKey], alloc: Alloc<'static>) -> BlockTraits {
    let mut out = BlockTraits::module(alloc);
    let body = locals.body(owner);
    if body.is_none() && locals.head(owner).is_none() {
        return out;
    }
    let reader = Reader { names, locals, module, rank: names.edition_of(krate).map(prelude_rank) };
    // The base: the module's traits and those of the blocks around the body's item, when the
    // body is a local item's (its declaration's block, then each block and item outward).
    let mut base: AVec<'static, ItemKey> = AVec::new_in(alloc);
    base.extend_from_slice(module_traits);
    let mut base_refused: Option<LocalGap> = None;
    let mut cur = owner;
    let mut nested = 0u32;
    while let Some(h) = locals.head(cur) {
        nested += 1;
        if nested > MOST_NESTING {
            base_refused = base_refused.or(Some(LocalGap { item: h.lib, form: NESTING, range: h.range }));
            break;
        }
        if let Some(b) = locals.body(h.enclosing) {
            if let Some(g) = reader.chain(h.enclosing, b, h.scope, &mut base, alloc) {
                base_refused = base_refused.or(Some(g));
            }
        }
        cur = h.enclosing;
    }
    base.sort_unstable();
    base.dedup();
    let mut last: Option<(u32, u32)> = None;
    out.default = match base_refused {
        Some(g) => Some(At::Refused(g)),
        None if &base[..] == module_traits => None,
        None => Some(keep_list(&mut out.keys, &mut last, &base)),
    };
    let Some(b) = body else { return out };
    // Each block's own rows, read once: `own[s]` is its range in `own_keys` and its refusal.
    let blocks = b.parents.len();
    let mut own_keys: AVec<'_, ItemKey> = AVec::new_in(alloc);
    let mut own: AVec<'_, (u32, u32, Option<LocalGap>)> = AVec::with_capacity_in(blocks, alloc);
    for s in 0..blocks {
        let start = own_keys.len() as u32;
        let mut refused = None;
        for row in b.rows.iter().filter(|r| r.scope == s as u32) {
            if let Err(g) = reader.row(owner, row, &mut own_keys, alloc) {
                refused = refused.or(Some(g));
            }
        }
        own.push((start, own_keys.len() as u32, refused));
    }
    // Each block's answer: `None` is the default's. A block that adds nothing answers as its
    // parent does.
    let mut answer: AVec<'_, Option<At>> = AVec::with_capacity_in(blocks, alloc);
    let mut list: AVec<'_, ItemKey> = AVec::new_in(alloc);
    for s in 0..blocks {
        let (start, end, refused) = own[s];
        let parent = b.parents[s];
        if start == end && refused.is_none() {
            match parent {
                None => {
                    answer.push(None);
                    continue;
                }
                Some(p) if (p as usize) < s => {
                    let a = answer[p as usize];
                    answer.push(a);
                    continue;
                }
                Some(_) => {}
            }
        }
        list.clear();
        list.extend_from_slice(&base);
        let mut refused = None;
        let mut here = Some(s as u32);
        for _ in 0..=blocks {
            let Some(h) = here else { break };
            let (a, e, r) = own[h as usize];
            list.extend_from_slice(&own_keys[a as usize..e as usize]);
            if refused.is_none() {
                refused = r;
            }
            here = b.parents.get(h as usize).copied().flatten();
        }
        let a = match refused.or(base_refused) {
            Some(g) => Some(At::Refused(g)),
            None => {
                list.sort_unstable();
                list.dedup();
                if list[..] == base[..] { None } else { Some(keep_list(&mut out.keys, &mut last, &list)) }
            }
        };
        answer.push(a);
    }
    for (s, a) in answer.iter().enumerate() {
        if let Some(a) = *a {
            if Some(a) != out.default {
                out.scopes.push((s as u32, a));
            }
        }
    }
    out
}
