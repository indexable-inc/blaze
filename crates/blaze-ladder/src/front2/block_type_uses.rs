//! A written type in a body whose first segment a block's named `use` binds: bstd's
//! `rt::write_out` writes `use std::{io::{IoSlice, Write as _}, ..};` in the fn's block and
//! then `let mut left: &mut [IoSlice<'_>] = ..`.
//!
//! The Reference, "Use declarations": "Use declarations may appear in modules and blocks,
//! usually at the top."; "Statements", "Item declarations": "Declaring an item within a
//! statement block restricts its scope to the block containing the statement." So a name
//! written in a block is, in this order (super::scope_table, "The rules"): the items and
//! named imports of its block, then that block's globs, then the same of each block around
//! it, then the body's module. It is the same order for a path in an expression and for a
//! path in a written type: a named import binds the name in every namespace its path
//! resolves in.
//!
//! A path in an expression reads a block's named import (super::world_paths). A written type
//! is converted by the type builder (super::types), which read the body's module, or the
//! module of a block that declares a struct, enum, union, alias, trait, `mod` or `extern
//! crate` (body.rs `BodyTypes::env_at`): a block that holds only `use` items (and `fn`s,
//! `const`s, `static`s) is no module of the names library (blaze-file-items
//! `NestedKind::makes_module`), so its import was never asked and the type named nothing
//! there: the body was refused E0412. Here the builder asks the block chain for a written
//! path's first segment first ([`through_use`], called by `Builder::resolve_at`): when
//! [`Locals::find`] answers with a named import, the import's path stands for the name and is
//! read where the `use` is written, exactly as world_paths reads it for an expression: at the
//! site the written path's mark gives (the body's module for a path written in its text), or
//! at the block module of a block's item the import's path starts with; a `$crate` the import
//! starts with is its mark's crate. Any other answer (a block's item, nothing) leaves the path
//! to the reading the builder already did.
//!
//! Owed, refused by name (never read at the module, rule 3): a glob of a block that is nearer
//! than the named import ([`super::glob_uses::TYPE_THROUGH_GLOB`]: the type builder reads no
//! glob yet); an import whose path does not resolve where it is written, or stops before its
//! binding ([`NAMED_TYPE_TARGET`]).

use blaze_id::ItemKey as LibKey;
use blaze_lower::View;
use blaze_own::{AVec, Alloc};
use blaze_scope::crate_roots::TextMarks;
use blaze_surface::Range;
use blaze_typeck::{ItemKey, PathIdx};

use super::glob_uses::{MAX_STEPS, TYPE_THROUGH_GLOB, dollar_of, is_glob};
use super::locals::{LocalKind, LocalNs, Locals, Row};
use super::source::NamesSource;
use super::types::Why;
use crate::resolve::{Ns, Paths, ResolveError, Target};

/// A written type whose first segment a block's named import binds, where the import's path
/// does not resolve where it is written, or names nothing past its own segments.
pub const NAMED_TYPE_TARGET: &str = "a written type through a use in a block whose path does not resolve";

/// The blocks around a written type nest deeper than [`MAX_STEPS`].
pub const TOO_DEEP: &str = "a written type through a use in a block under more blocks than the lookup walks";

/// The body a written type stands in, as the type builder asks its blocks' names: the items
/// and imports kept for the command (super::locals), whose body this is, the body's module,
/// and the allocator a path through an import is spelled in. `None` in [`super::types::Env`]
/// for a type no body's block holds (a signature, a head).
#[derive(Clone, Copy)]
pub struct BodyScope<'e> {
    pub locals: &'e Locals<'e>,
    pub owner: ItemKey,
    pub module: LibKey,
    pub alloc: Alloc<'e>,
}

/// Whether two rows are the one kept row (a `use` tree of several leaves is several rows at
/// one range: the name tells them apart).
fn same(a: &Row<'_>, b: &Row<'_>) -> bool {
    a.scope == b.scope && a.kind == b.kind && a.name == b.name && a.key == b.key && a.range == b.range
}

/// Whether a glob of a block between scope `scope` of the body `owner` and the block that
/// declares `named` (exclusive: a named import shadows its own block's globs) may give the
/// name first: then the type is refused by name ([`TYPE_THROUGH_GLOB`]), as the builder reads
/// no glob. The walk is glob_uses.rs `before`'s: out of a local item's body into the blocks
/// around its declaration.
fn glob_nearer(locals: &Locals<'_>, owner: ItemKey, scope: u32, named: &Row<'_>) -> Result<(), &'static str> {
    let mut body = owner;
    let mut at = Some(scope);
    for _ in 0..MAX_STEPS {
        let (b, here) = match (locals.body(body), at) {
            (Some(b), Some(here)) => (b, here),
            _ => match locals.head(body) {
                Some(h) => {
                    body = h.enclosing;
                    at = Some(h.scope);
                    continue;
                }
                // Out of every block: `named` was found by `Locals::find` through a body
                // this walk does not enter (an anonymous constant's): no glob was met.
                None => return Ok(()),
            },
        };
        if b.rows.iter().any(|r| r.scope == here && same(r, named)) {
            return Ok(());
        }
        if b.rows.iter().any(|r| r.scope == here && is_glob(r)) {
            return Err(TYPE_THROUGH_GLOB);
        }
        at = b.parents.get(here as usize).copied().flatten();
    }
    Err(TOO_DEEP)
}

/// What the written path `p` of `v` (segments `texts`, from `::` when `abs`), standing in a
/// block of `body`, names in `ns` through a block's named import of its first segment: the
/// target, its `consumed` counted in the segments as WRITTEN (the import's path stands for its
/// one binding). `None`: no block's named import binds the first segment (a block's item
/// does, or nothing does): the caller reads the path as before. `marks`: the builder's marks
/// of the text the path is read from, for the written path's site
/// ([`super::item_site::written_site`]). `scope`: the block of `body` the path stands in, as
/// super::scope_table `written` decides it (a local item's signature path: its declaring
/// block, in its enclosing body).
#[expect(clippy::too_many_arguments, reason = "one resolve of one path: the path, its view, its marks and the body")]
pub fn through_use<'n, S: NamesSource<'n>>(
    resolver: &Paths<S>,
    marks: Option<TextMarks<'_>>,
    body: &BodyScope<'_>,
    scope: u32,
    v: &View<'_>,
    p: PathIdx,
    abs: bool,
    texts: &[&str],
    ns: Ns,
    r: Range,
) -> Result<Option<Target>, Why> {
    let Some((&first, rest)) = texts.split_first() else { return Ok(None) };
    // A path from the root, or one whose first segment is a path head, names no block's
    // import (`self`, `super` and `crate` are the module's; `Self` the builder's).
    if abs || matches!(first, "self" | "super" | "crate" | "Self" | "<qualified>") || first.starts_with('$') {
        return Ok(None);
    }
    let Some(row) = body.locals.find(body.owner, Some(scope), first, LocalNs::Type) else { return Ok(None) };
    if row.kind != LocalKind::Use {
        return Ok(None);
    }
    glob_nearer(body.locals, body.owner, scope, &row).map_err(|form| Why::Unsupported(form, r))?;
    // Read where the written path's mark says (as world_paths reads an expression's path
    // through a block import), unless the import's path starts with a block's item: then at
    // that item's block module (super::scope_table).
    let site = super::item_site::written_site(resolver, marks, body.module, v, p, abs, first, r)?;
    let mut at_module = site.module;
    if let Some(&head) = row.path.first()
        && !row.abs
        && let Some(item) = body.locals.find(body.owner, Some(row.scope), head, LocalNs::Type).filter(|i| i.kind.in_block_module())
    {
        at_module = item.module.ok_or(Why::Unsupported(item.kind.unplaced(), r))?;
    }
    let dollar = if row.abs { None } else { dollar_of(row.path.first().copied(), row.first_mark) };
    let mut through: AVec<'_, &str> = AVec::new_in(body.alloc);
    through.extend_from_slice(row.path);
    through.extend_from_slice(rest);
    let target = match resolver.path_with(at_module, dollar, row.abs, &through, ns) {
        Ok(t) => t,
        Err(ResolveError::Path(_)) => return Err(Why::Unsupported(NAMED_TYPE_TARGET, r)),
        Err(e @ ResolveError::NoText { .. }) => return Err(Why::Resolve(e, r)),
    };
    // The import's segments stand for the written first one: a resolution that stops inside
    // them names no item by the binding.
    let lead = (row.path.len() as u32).saturating_sub(1);
    let consumed = target.consumed.checked_sub(lead).filter(|c| *c > 0).ok_or(Why::Unsupported(NAMED_TYPE_TARGET, r))?;
    Ok(Some(Target { consumed, ..target }))
}
