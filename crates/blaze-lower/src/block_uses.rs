//! The `use` declarations of a body's blocks, as an identifier pattern sees them.
//!
//! The Reference, "Use declarations": "Use declarations may appear in modules and blocks,
//! usually at the top." and, "Glob imports": "The `*` character may be used as the last segment
//! of a `use` path to import all importable entities from the entity of the preceding segment."
//! So after `use E::*;` in a block, a pattern `A` in that block is the VARIANT `E::A`, never a
//! binding (the Reference, "Identifier patterns": "Path patterns take precedence over
//! identifier patterns"). The lowering does not read a module's names, so it hands the world
//! the `use` items around the pattern, innermost block first, and the world answers
//! ([`ValueNames::value_name_by_uses`]).
//!
//! The order is the Reference's ("Name resolution", [names.resolution.expansion.imports.shadowing]):
//! "Names introduced via use declarations in an outer scope are shadowed by candidates in the
//! same namespace with the same name from an inner scope"; and ("Use declarations", "Glob
//! imports") "Items and named imports are allowed to shadow names from glob imports in the same
//! namespace." So: a block's own item, then that block's `use`s, then the block around it. The
//! `use`s handed over are therefore those of the blocks INSIDE the nearest block that declares
//! a value item of the name (that block's item wins over its own globs), or of every block
//! around the pattern when no block declares one.
//!
//! Every question is a row of the record's read list (`value_reads`, flagged [`BY_USES`]):
//! [`reads_hold`] asks each again.

use blaze_own::{AVec, Alloc, Vec};
use parser::SyntaxKind::USE;

use crate::columns::{TextAt, View};
use crate::form::{BadRow, NameIdx, Node, RowIdx, ScopeId};
use crate::{ValueName, ValueNames};

/// A read-list row's flag: the name was asked through the `use`s of the blocks around the
/// pattern; `b` is the pattern's block. A row with no flag is the module's answer.
pub const BY_USES: u16 = 1;
/// With [`BY_USES`]: no `use` binds the name (the row's answer is [`ValueName::Nothing`], and
/// the block's item or the module was asked next).
pub const UNBOUND: u16 = 2;
/// With [`BY_USES`]: a block around the pattern declares a value item of the name, and only
/// the `use`s of the blocks inside that block were asked.
pub const OWN_ITEM: u16 = 4;

/// One `use` item declared in a block around a pattern.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BlockUse<'a> {
    /// How many blocks out from the pattern's own block it is declared (0: the same block).
    pub depth: u32,
    /// The item's text, attributes and `;` included.
    pub text: &'a str,
    /// The block it is declared in: where its path is read is that block's (the world's scope
    /// table, by the scope's [`crate::scope_origin::ScopeOrigin`]).
    pub scope: ScopeId,
    /// Where it is shown in the enclosing item ([`View::site`]), for a refusal's words only:
    /// never a key, never a lookup.
    pub shown: blaze_surface::Range,
    /// Its range in the view: the marks of its words ([`View::marks_in`], a start in
    /// [`Self::text`] each), so a `$crate` a macro wrote in it is read as the MACRO's crate
    /// (clap_builder's `value_parser!` writes `use $crate::builder::impl_prelude::*;`).
    pub range: crate::columns::BodyRange,
}

/// What the `use`s around a pattern make of its name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Asked {
    /// No `use` is declared in the blocks asked: nothing was asked.
    NoUses,
    /// A `use` binds the name in the value namespace.
    Bound(ValueName),
    /// None does.
    Unbound,
}

/// The `use` items of the blocks around `scope`, innermost first, and the names of the other
/// items those blocks declare.
pub struct InScope<'a, 'b> {
    pub uses: AVec<'a, BlockUse<'b>>,
    /// The names of the blocks' own items (any namespace): a `use` path that starts with one
    /// names an item of a block, which the world reads at the block's module or refuses.
    pub locals: AVec<'a, &'b str>,
}

/// The `use`s and item names of the blocks around `scope`. With `own_item`, only those inside
/// the nearest block that declares an item named `name` (the module doc's order).
pub fn in_scope<'a, 'b>(v: &View<'b>, scope: ScopeId, name: u64, own_item: bool, alloc: Alloc<'a>) -> Result<InScope<'a, 'b>, TextAt> {
    let mut found = InScope { uses: Vec::new_in(alloc.clone()), locals: Vec::new_in(alloc) };
    let mut at = Some(scope);
    let mut depth = 0u32;
    // A chain is at most as long as the scopes column: a cycle is a malformed record.
    for _ in 0..=v.scopes.len() {
        let Some(s) = at else { return Ok(found) };
        let block = v.scope(s)?;
        let uses_before = found.uses.len();
        let mut declares = false;
        for i in block.items.start..block.items.start + block.items.len {
            let item = v.local_item(v.row(RowIdx(i))?)?;
            if item.syntax == USE as u16 {
                found.uses.push(BlockUse { depth, text: v.text_at(item.range)?, scope: s, shown: v.site(item.range)?, range: item.range });
            } else if let Some(n) = item.name {
                declares |= v.name(n)? == name;
                found.locals.push(v.name_text(n)?);
            }
        }
        if own_item && declares {
            // The block's own item wins over the block's `use`s.
            found.uses.truncate(uses_before);
            return Ok(found);
        }
        at = block.parent;
        depth += 1;
    }
    Err(TextAt::Row(BadRow { column: "scopes", tag: u16::MAX }))
}

/// Asks `names` what `name`, an identifier pattern written in `scope`, is through the `use`s
/// around it. `own_item`: an item of one of the blocks answers for the name in the value
/// namespace (the lowering's own `local_value`).
pub fn ask<'a>(v: &View<'_>, scope: ScopeId, name: NameIdx, own_item: bool, names: &mut dyn ValueNames, alloc: Alloc<'a>) -> Result<Asked, TextAt> {
    let found = in_scope(v, scope, v.name(name)?, own_item, alloc)?;
    if found.uses.is_empty() {
        return Ok(Asked::NoUses);
    }
    Ok(match names.value_name_by_uses(v, v.name_text(name)?, &found.uses, &found.locals) {
        Some(answer) => Asked::Bound(answer),
        None => Asked::Unbound,
    })
}

/// The read-list row of one question [`ask`] answered; `None` for [`Asked::NoUses`].
pub fn read_row(name: NameIdx, scope: ScopeId, own_item: bool, asked: Asked) -> Option<Node> {
    let own = if own_item { OWN_ITEM } else { 0 };
    match asked {
        Asked::NoUses => None,
        Asked::Bound(answer) => Some(Node { a: name.0, b: scope.0, flags: BY_USES | own, ..Node::of(answer as u16) }),
        Asked::Unbound => Some(Node { a: name.0, b: scope.0, flags: BY_USES | UNBOUND | own, ..Node::of(ValueName::Nothing as u16) }),
    }
}

/// Whether every answer of `view`'s read list still holds: the module's
/// ([`ValueNames::value_name`]) and those through the blocks' `use`s. When one does not (a glob
/// import's enum gained a variant of a name the body binds), the body is lowered again.
pub fn reads_hold<'a>(view: &View<'_>, names: &mut dyn ValueNames, alloc: Alloc<'a>) -> Result<bool, TextAt> {
    for row in view.value_reads.iter() {
        let Some(answer) = ValueName::of_code(row.tag) else {
            return Err(TextAt::Row(BadRow { column: "value_reads", tag: row.tag }));
        };
        let name = NameIdx(row.a);
        if row.flags & BY_USES == 0 {
            if names.value_name(view.name_text(name)?) != answer {
                return Ok(false);
            }
            continue;
        }
        let was = if row.flags & UNBOUND != 0 { Asked::Unbound } else { Asked::Bound(answer) };
        if ask(view, ScopeId(row.b), name, row.flags & OWN_ITEM != 0, names, alloc.clone())? != was {
            return Ok(false);
        }
    }
    Ok(true)
}
