//! A glob `use` declared in a block (`use Enum::*;`, `use self::Kind::*;`, `use super::*;`):
//! a name of the body is looked up through it lazily, by the questions a path through the
//! glob's module or enum asks ([`Paths::path`] over any [`NamesSource`], so it reads the same
//! over the solved names and over records).
//!
//! The Reference, "Use declarations": "Use declarations may appear in modules and blocks,
//! usually at the top."; "Glob imports": "The `*` character may be used as the last segment of
//! a `use` path to import all importable entities from the entity of the preceding segment."
//! and "Items and named imports are allowed to shadow names from glob imports in the same
//! namespace." "Name resolution" [names.resolution.expansion.imports.shadowing]: "Names
//! introduced via use declarations in an outer scope are shadowed by candidates in the same
//! namespace with the same name from an inner scope"; [..ambiguity.glob-vs-glob]: "Names may
//! not be resolved through ambiguous glob imports. Glob imports are allowed to import
//! conflicting names in the same namespace as long as the name is not used. [..] The errors
//! occur at time of use, not time of import." and "Multiple glob imports are allowed to import
//! the same name, and that name is allowed to be used if the imports are of the same item".
//!
//! So a name written in a block is, in this order: the block's own item or named import
//! ([`Locals::find`], the lowering's `local_value`), then what the block's globs import, then
//! the same of the block around it, then the module's. A glob imports `name` when the path
//! `<the glob's path>::name` resolves whole, read where the `use` is written: that walk is the
//! library's, so what is private to the body's module is not imported, and an enum's variants
//! answer in both namespaces. Two globs of one block that give a USED name two different items
//! are refused ([`AMBIGUOUS`]); unused, nothing is asked.
//!
//! Why not a block module (blaze-scope blocks.rs, as a block holding an impl or a type is):
//! `self::` and `super::` in a block's `use` name the body's module and its parent, where a
//! block module's are the block and the body's module (blocks.rs, "Known differences"); the
//! block module's own `use super::*` and the block's glob would be two globs of one module, so
//! a name in both would be ambiguous where the rule above says the inner one shadows; and a
//! `use` an expansion wrote, or one in a local item's body, has no block module at all.
//!
//! A glob's own path: `::a`, `crate::`, `self::`, `super::` are read at the body's module. A
//! path that starts with a block's struct, enum, union or alias is read at that block's module
//! (where the type is an item); one that starts with a named import of a block stands for the
//! import's path; one that starts with a name another glob around it imports (`use super::*;
//! use Kind::*;`) stands for that path. Owed, each refused by name or read at the module:
//! a first segment found only through a glob that itself needs a glob; a macro imported by a
//! block's glob (the macro resolver asks the module); a written TYPE whose first segment only a
//! block's glob imports ([`TYPE_THROUGH_GLOB`]: the type builder reads at one module).

use blaze_file_items::nested::{Nested, NestedEnd};
use blaze_id::CrateItemKey;
use blaze_id::ItemKey as LibKey;
use blaze_lower::Range;
use blaze_lower::block_uses::BlockUse;
use blaze_own::{AVec, Alloc};
use blaze_surface::{Rename, UseForm};
use blaze_typeck::ItemKey;

use super::locals::{BodyLocals, LocalGap, LocalKind, LocalNs, Locals, Row};
use super::source::NamesSource;
use crate::resolve::{Kind, Ns, Paths, ResolveError, Target};

/// The name a glob import's row is kept under ([`Row::name`]): no identifier, so no lookup by
/// name finds it.
pub const GLOB: &str = "*";

/// Two globs of one block give the used name two different items (rustc's E0659).
pub const AMBIGUOUS: &str = "a name that two glob uses of one block import as two different items (ambiguous, E0659)";
/// The glob's own path does not resolve where it is written.
pub const TARGET: &str = "a glob use in a block whose path does not resolve";
/// The glob's path names something with no importable entities.
pub const NOT_A_MODULE: &str = "a glob use in a block of what is neither a module nor an enum";
/// The path starts with an item of a block that the names library has no module for.
pub const THROUGH_LOCAL: &str = "a use in a block whose path starts with a block's item that has no module";
/// A named import of a block, asked by a pattern, whose path does not resolve.
pub const NAMED_TARGET: &str = "a use in a block whose path does not resolve (asked by a pattern)";
/// The `use` item's tree does not read.
pub const UNREAD: &str = "a use tree in a block that does not read";
/// The marks of a block `use`'s words do not read from the body's record, or its path's first
/// segment is not in its text: a `$crate` a macro wrote could not be placed.
pub const MARKS_UNREAD: &str = "a use in a block whose words' marks do not read";
/// The blocks around a name nest deeper than [`MAX_STEPS`].
pub const TOO_DEEP: &str = "a use in a block under more blocks than the lookup walks";
/// A written type in a body some block of which holds a glob import, whose path does not
/// resolve at the body's module: it may be the glob's to give.
pub const TYPE_THROUGH_GLOB: &str = "a written type in a body with a glob use in a block, not found at the body's module";

/// The most blocks (and bodies) a lookup walks outward.
pub const MAX_STEPS: u32 = 4096;

/// Whether `segment` is a path head the names library reads itself.
fn is_head(segment: &str) -> bool {
    matches!(segment, "self" | "super" | "crate" | "{{root}}") || segment.starts_with('$')
}

/// A copy of `segments` in `alloc`.
fn copy<'a>(segments: &[&'a str], alloc: Alloc<'a>) -> AVec<'a, &'a str> {
    let mut path: AVec<'a, &'a str> = AVec::new_in(alloc);
    path.extend_from_slice(segments);
    path
}

/// Where a `use`'s path is read: the module, whether from the global root, the path, and the
/// crate a first segment `$crate` (or an opaque `crate`) names.
pub struct Start<'a> {
    pub abs: bool,
    pub at: LibKey,
    pub path: AVec<'a, &'a str>,
    /// The crate of the macro that wrote the path's first segment when that segment is a
    /// `$crate` (or an opaque `crate`): its token's mark's, by the one resolver-by-mark
    /// (blaze-scope `crate_roots::root_by_mark`, [`Row::first_mark`]). `None`: the body's
    /// crate. The Reference, "Macros By Example", "Hygiene": "`$crate` [..] refers to the crate
    /// that defines the macro". clap_builder 4.6.7's `value_parser!` writes `use
    /// $crate::builder::impl_prelude::*;` in the block it expands to; read as the calling
    /// crate's `crate::builder`, every clap derive's method body was refused [`TARGET`]
    /// (port27/mini/clap-root, MX 2026-11-17).
    pub dollar: Option<CrateItemKey>,
}

impl<'a> Start<'a> {
    fn again(&self, alloc: Alloc<'a>) -> Start<'a> {
        Start { abs: self.abs, at: self.at, path: copy(&self.path, alloc), dollar: self.dollar }
    }

    /// [`Paths::path_with`] of `path` from this start.
    fn walk<'n, S: NamesSource<'n>>(&self, paths: &Paths<S>, path: &[&str], ns: Ns) -> Result<Target, ResolveError> {
        paths.path_with(self.at, self.dollar, self.abs, path, ns)
    }
}

/// The crate a path whose first segment is `first`, of mark `mark`, names by `$crate` (or an
/// opaque `crate`) when it is another crate's ([`Start::dollar`]).
pub fn dollar_of(first: Option<&str>, mark: Option<blaze_expand::origin_column::TextMark>) -> Option<CrateItemKey> {
    match blaze_scope::crate_roots::root_by_mark(first?, mark) {
        blaze_scope::crate_roots::Root::Crate(k) => Some(k),
        blaze_scope::crate_roots::Root::Here | blaze_scope::crate_roots::Root::NotARoot => None,
    }
}

/// One import of a block around a name.
pub struct UseAt<'a> {
    /// How many blocks out from the name's block it is declared.
    pub depth: u32,
    /// `None`: a glob; else the name a named import binds.
    pub bound: Option<&'a str>,
    /// Where its path is read; `None`: a plain first segment no block's item takes, read
    /// through the globs around it, else at the body's module.
    pub start: Option<Start<'a>>,
    /// The path as written (without the `*` of a glob).
    pub written: AVec<'a, &'a str>,
    /// What a refusal names: the body's item and the `use`'s place in it.
    pub item: LibKey,
    pub range: Range,
}

/// What a name is through a block's import: the path that stands for it (read at `at`), and
/// the item.
pub struct Hit<'a> {
    pub abs: bool,
    pub at: LibKey,
    /// The import's path with the name as its last segment (a named import's: its path).
    pub path: AVec<'a, &'a str>,
    pub target: Target,
    /// The crate [`Self::path`]'s first segment `$crate` names ([`Start::dollar`]).
    pub dollar: Option<CrateItemKey>,
}

/// Whether `t`, the answer to a path asked in `ns` (which [`Paths::path`] also looks up in the
/// other namespace), is an entity of `ns` itself.
fn of_namespace<'n, S: NamesSource<'n>>(paths: &Paths<S>, t: &Target, ns: Ns) -> bool {
    let value_only = matches!(t.kind, Kind::Fn | Kind::Const | Kind::Static);
    match ns {
        Ns::Type => !value_only,
        Ns::Value => value_only || matches!(t.kind, Kind::Variant { .. }) || (matches!(t.kind, Kind::Struct) && paths.names.is_ctor(t.key)),
    }
}

/// What `name` is through the globs `globs` of ONE block (each with its start): the one item
/// they import under it in `ns`, `None` when none does, [`AMBIGUOUS`] when two differ.
fn through<'n, 'a, S: NamesSource<'n>>(paths: &Paths<S>, globs: &[(&UseAt<'a>, Start<'a>)], name: &'a str, ns: Ns, alloc: Alloc<'a>) -> Result<Option<Hit<'a>>, LocalGap> {
    let mut hit: Option<(Hit<'a>, bool)> = None;
    for (glob, start) in globs {
        let gap = |form: &'static str| LocalGap { item: glob.item, form, range: glob.range };
        // The entity whose names are imported: a module or an enum. A path of heads alone
        // (`super::*`, `crate::*`) names a module.
        if !start.path.iter().all(|s| is_head(s)) {
            match start.walk(paths, &start.path, Ns::Type) {
                Ok(t) if t.consumed as usize == start.path.len() && matches!(t.kind, Kind::Module | Kind::Enum) => {}
                Ok(t) if t.consumed as usize == start.path.len() => return Err(gap(NOT_A_MODULE)),
                Ok(_) | Err(_) => return Err(gap(TARGET)),
            }
        }
        let mut path: AVec<'a, &'a str> = AVec::new_in(alloc);
        path.extend_from_slice(&start.path);
        path.push(name);
        let target = match start.walk(paths, &path, ns) {
            Ok(t) if t.consumed as usize == path.len() => t,
            // The walk stopped before the name (an enum's associated item is no import), or
            // the name is not there or not visible from here: this glob does not import it.
            Ok(_) | Err(ResolveError::Path(_)) => continue,
            Err(ResolveError::NoText { .. }) => return Err(gap(TARGET)),
        };
        let own = of_namespace(paths, &target, ns);
        let takes = match &hit {
            // The same item through two globs is one import.
            Some((h, _)) if h.target.key == target.key => false,
            Some((_, had)) if *had == own => return Err(gap(AMBIGUOUS)),
            // An entity of the asked namespace is the answer over one of the other.
            Some((_, true)) => false,
            _ => true,
        };
        if takes {
            hit = Some((Hit { abs: start.abs, at: start.at, path, target, dollar: start.dollar }, own));
        }
    }
    Ok(hit.map(|(h, _)| h))
}

/// Where `u`'s path is read: its own start, else its first segment through the globs around
/// it that have a start of their own (nearest block first), else the body's module as written.
fn start_of<'n, 'a, S: NamesSource<'n>>(paths: &Paths<S>, module: LibKey, uses: &[UseAt<'a>], u: &UseAt<'a>, alloc: Alloc<'a>) -> Result<Start<'a>, LocalGap> {
    if let Some(s) = &u.start {
        return Ok(s.again(alloc));
    }
    let as_written = |alloc: Alloc<'a>| Start { abs: false, at: module, path: copy(&u.written, alloc), dollar: None };
    let Some(&first) = u.written.first() else { return Ok(as_written(alloc)) };
    let mut depth = u.depth;
    let mut at = 0usize;
    while at < uses.len() {
        // `uses` is sorted by depth: one block's imports at a time, from `u`'s outward.
        if uses[at].depth < depth {
            at += 1;
            continue;
        }
        depth = uses[at].depth;
        let mut group: AVec<'a, (&UseAt<'a>, Start<'a>)> = AVec::new_in(alloc);
        while at < uses.len() && uses[at].depth == depth {
            if let (None, Some(s)) = (uses[at].bound, &uses[at].start) {
                group.push((&uses[at], s.again(alloc)));
            }
            at += 1;
        }
        if let Some(hit) = through(paths, &group, first, Ns::Type, alloc)? {
            let mut path = hit.path;
            path.extend_from_slice(&u.written[1..]);
            return Ok(Start { abs: hit.abs, at: hit.at, path, dollar: hit.dollar });
        }
    }
    Ok(as_written(alloc))
}

/// What `name` is in `ns` through `uses` (sorted by depth, nearest block first), read in a
/// body of `module`: each block's named imports of it, then its globs, then the next block
/// out. `None`: none of them binds it.
pub fn lookup<'n, 'a, S: NamesSource<'n>>(paths: &Paths<S>, module: LibKey, uses: &[UseAt<'a>], name: &'a str, ns: Ns, alloc: Alloc<'a>) -> Result<Option<Hit<'a>>, LocalGap> {
    if is_head(name) {
        return Ok(None);
    }
    let mut at = 0usize;
    while at < uses.len() {
        let depth = uses[at].depth;
        let from = at;
        while at < uses.len() && uses[at].depth == depth {
            at += 1;
        }
        let block = &uses[from..at];
        // A named import of the block shadows its globs.
        for u in block.iter().filter(|u| u.bound == Some(name)) {
            let start = start_of(paths, module, uses, u, alloc)?;
            match start.walk(paths, &start.path, ns) {
                Ok(target) if target.consumed as usize == start.path.len() => {
                    if of_namespace(paths, &target, ns) {
                        return Ok(Some(Hit { abs: start.abs, at: start.at, dollar: start.dollar, path: start.path, target }));
                    }
                }
                Ok(_) | Err(_) => return Err(LocalGap { item: u.item, form: NAMED_TARGET, range: u.range }),
            }
        }
        let mut globs: AVec<'a, (&UseAt<'a>, Start<'a>)> = AVec::new_in(alloc);
        for u in block.iter().filter(|u| u.bound.is_none()) {
            globs.push((u, start_of(paths, module, uses, u, alloc)?));
        }
        if let Some(hit) = through(paths, &globs, name, ns, alloc)? {
            return Ok(Some(hit));
        }
    }
    Ok(None)
}

/// Whether `row` is a glob import's row.
pub fn is_glob(row: &Row<'_>) -> bool {
    row.kind == LocalKind::Use && row.name == GLOB
}

/// Whether the body `owner`, or a body around it, declares a glob import in any block.
pub fn any_glob(locals: &Locals<'_>, owner: ItemKey) -> bool {
    let mut body = owner;
    for _ in 0..MAX_STEPS {
        if locals.body(body).is_some_and(|b| b.rows.iter().any(is_glob)) {
            return true;
        }
        match locals.head(body) {
            Some(h) => body = h.enclosing,
            None => return false,
        }
    }
    false
}

/// A glob row's own start, when its path says where it is read (the module doc): `None` for a
/// plain first segment no block's item or named import takes.
fn row_start<'n, 'a>(locals: &Locals<'n>, module: LibKey, body: ItemKey, row: &Row<'n>, alloc: Alloc<'a>) -> Result<Option<Start<'a>>, LocalGap>
where
    'n: 'a,
{
    let mut path: AVec<'a, &'a str> = AVec::new_in(alloc);
    let Some(&first) = row.path.first() else { return Ok(Some(Start { abs: row.abs, at: module, path, dollar: None })) };
    if row.abs || is_head(first) {
        path.extend_from_slice(row.path);
        // A `$crate` a macro wrote: the macro's crate, by the row's first segment's mark.
        let dollar = if row.abs { None } else { dollar_of(Some(first), row.first_mark) };
        return Ok(Some(Start { abs: row.abs, at: module, path, dollar }));
    }
    let gap = LocalGap { item: row.key, form: THROUGH_LOCAL, range: row.range };
    match locals.find(body, Some(row.scope), first, LocalNs::Type) {
        None => Ok(None),
        // A block's struct, enum, union or alias: an item of its block's module.
        Some(local) if local.kind.in_block_module() => match local.module {
            Some(m) => {
                path.extend_from_slice(row.path);
                Ok(Some(Start { abs: false, at: m, path, dollar: None }))
            }
            None => Err(gap),
        },
        // A named import: its path stands for the name (its first segment's mark with it).
        Some(local) if local.kind == LocalKind::Use => {
            path.extend_from_slice(local.path);
            path.extend_from_slice(&row.path[1..]);
            let dollar = if local.abs { None } else { dollar_of(local.path.first().copied(), local.first_mark) };
            Ok(Some(Start { abs: local.abs, at: module, path, dollar }))
        }
        Some(_) => Err(gap),
    }
}

/// What `name`, the first segment of a path written in block `scope` of the body `owner` (of
/// `module`), names before reaching `explicit`, the item or named import [`Locals::find`]
/// found in the original syntax. At each block a direct binding from an attribute replacement
/// wins over that block's globs. Inner blocks are searched first. With no hit, the caller
/// continues with `explicit` or the body's module.
pub fn before<'n, 'l, 'a, S: NamesSource<'n>>(
    locals: &Locals<'l>,
    paths: &Paths<S>,
    module: LibKey,
    owner: ItemKey,
    scope: u32,
    name: &'a str,
    ns: Ns,
    explicit: Option<&Row<'l>>,
    alloc: Alloc<'a>,
) -> Result<Option<Hit<'a>>, LocalGap>
where
    'l: 'a,
{
    if is_head(name) {
        return Ok(None);
    }
    let mut uses: AVec<'a, UseAt<'a>> = AVec::new_in(alloc);
    let mut body = owner;
    let mut at = Some(scope);
    let mut depth = 0u32;
    let mut steps = 0u32;
    let mut ended = false;
    while steps < MAX_STEPS {
        steps += 1;
        let (b, here): (&BodyLocals<'l>, u32) = match (locals.body(body), at) {
            (Some(b), Some(here)) => (b, here),
            // Out of this body's blocks: the blocks around a local item's declaration.
            _ => match locals.head(body) {
                Some(h) => {
                    body = h.enclosing;
                    at = Some(h.scope);
                    continue;
                }
                None => {
                    ended = true;
                    break;
                }
            },
        };
        let declares = |e: &Row<'l>| b.rows.iter().any(|r| r.scope == here && r.kind == e.kind && r.name == e.name && r.key == e.key && r.range == e.range);
        if explicit.is_some_and(declares) {
            ended = true;
            break;
        }
        if let Some(binding) = super::scope_table::direct_name_in(b, here, name, ns, depth, alloc, paths.names, b.names_of_blocks)? {
            uses.push(binding);
            ended = true;
            break;
        }
        for row in b.rows.iter().filter(|r| r.scope == here && is_glob(r)) {
            let mut written: AVec<'a, &'a str> = AVec::new_in(alloc);
            written.extend_from_slice(row.path);
            uses.push(UseAt { depth, bound: None, start: row_start(locals, module, body, row, alloc)?, written, item: row.key, range: row.range });
        }
        at = b.parents.get(here as usize).copied().flatten();
        depth += 1;
    }
    if uses.is_empty() {
        return Ok(None);
    }
    if !ended {
        return Err(LocalGap { item: uses[0].item, form: TOO_DEEP, range: uses[0].range });
    }
    lookup(paths, module, &uses, name, ns, alloc)
}

/// A body's blocks as the names library made them modules, for a pattern's question (asked
/// while the body is lowered, before its local items are kept): the scan of the body's text
/// and each written block's module ([`blaze_scope::blocks::block_modules`]). No modules when
/// the body is no item or member of the library (a local item's own body).
pub struct PatternBlocks<'a> {
    nested: Nested<'a>,
    modules: Option<AVec<'a, Option<LibKey>>>,
}

impl<'a> PatternBlocks<'a> {
    /// Of the body `owner` whose text is `text`, the item `item` (a member's: its container)
    /// of crate `krate`.
    pub fn of<'n, S: NamesSource<'n>>(names: S, locals: &super::locals::Locals<'n>, keys: &super::keys::Keys, krate: usize, item: LibKey, owner: ItemKey, text: &str, alloc: Alloc<'a>) -> PatternBlocks<'a> {
        let nested = blaze_file_items::nested::scan(text, alloc);
        // The library's key of the body, the one answer `Locals::keep` takes too.
        let body = locals.library_body(names, keys, item, owner, text);
        let modules = match (body, names.crate_key(krate)) {
            (Some(body), Some(crate_key)) if nested.end == NestedEnd::Whole => Some(blaze_scope::blocks::block_modules(crate_key, body, &nested, alloc)),
            _ => None,
        };
        PatternBlocks { nested, modules }
    }
}

/// What `name`, an identifier pattern, is in the value namespace through the `use` items
/// `block_uses` of the blocks around it (the lowering's list, nearest first; `local_names`:
/// the names of those blocks' own items), in a body of `module` whose item is `item`, lowered
/// to `view` (as far as it is lowered: every scope around the pattern is in it):
/// [`blaze_lower::ValueNames::value_name_by_uses`]'s answer before its shape. `None`: no `use`
/// binds it as a value. `planted`: the scope table's planted bug (`Planted::None` but in its
/// test).
#[allow(clippy::too_many_arguments, reason = "the world's one question: the body, its lowering and the uses")]
pub fn by_uses<'n, 'a, S: NamesSource<'n>>(
    paths: &Paths<S>,
    module: LibKey,
    item: LibKey,
    blocks: &PatternBlocks<'_>,
    view: &blaze_lower::View<'_>,
    name: &'a str,
    block_uses: &[BlockUse<'a>],
    local_names: &[&str],
    planted: super::scope_table::Planted,
    alloc: Alloc<'a>,
) -> Result<Option<Target>, LocalGap> {
    // The body's scope table (super::scope_table), built at the first `use` whose path starts
    // with an item of a block.
    let mut table: Option<super::scope_table::ScopeTable<'a>> = None;
    let mut uses: AVec<'a, UseAt<'a>> = AVec::new_in(alloc);
    // (the name a named import binds, its path, whether from the root): what a later `use`
    // path may start with.
    // (.., and the crate its first segment `$crate` names: [`Start::dollar`]).
    let mut named: AVec<'a, (&'a str, AVec<'a, &'a str>, bool, Option<CrateItemKey>)> = AVec::new_in(alloc);
    // Each import's (its `use`'s index, where shown, its first segment's `$crate`).
    let mut leaves_of: AVec<'a, (usize, Range, Option<CrateItemKey>)> = AVec::new_in(alloc);
    for (i, u) in block_uses.iter().enumerate() {
        // Where the `use` is shown in the body's item, for a refusal's words.
        let range: Range = u.shown;
        let Some(leaves) = blaze_scope::task::use_leaves(super::locals::use_tree(u.text), alloc) else {
            return Err(LocalGap { item, form: UNREAD, range });
        };
        // The marks of the `use`'s words, each at its start in `u.text`: a `$crate` (or opaque
        // `crate`) a macro wrote names that macro's crate (blaze-scope `root_by_mark`, as a
        // module's `use` and a block's kept row are read, locals.rs `Row::first_mark`).
        let marks = view.marks_in(u.range, alloc).map_err(|_| LocalGap { item, form: MARKS_UNREAD, range })?;
        for leaf in leaves.iter() {
            let first = leaf.segments.first().copied();
            let mark = match first {
                Some(first) if !leaf.abs => {
                    let Some(at) = blaze_scope::expansion_origins::offset_in(u.text, first) else {
                        return Err(LocalGap { item, form: MARKS_UNREAD, range });
                    };
                    marks.iter().find(|(start, _)| *start == at).map(|(_, m)| *m)
                }
                _ => None,
            };
            let dollar = if leaf.abs { None } else { dollar_of(first, mark) };
            let bound = match leaf.form {
                UseForm::Named { rename: Rename::Same, .. } => Some(leaf.segments.last().copied().unwrap_or("")),
                UseForm::Named { rename: Rename::To(n), .. } => Some(n),
                // `as _` binds no name.
                UseForm::Named { rename: Rename::Underscore, .. } => continue,
                UseForm::Glob => None,
            };
            if let Some(b) = bound {
                named.push((b, copy(&leaf.segments, alloc), leaf.abs, dollar));
            }
            // `::a::*` is read from the global root, whatever the blocks declare.
            let start = leaf.abs.then(|| Start { abs: true, at: module, path: copy(&leaf.segments, alloc), dollar: None });
            uses.push(UseAt { depth: u.depth, bound, start, written: copy(&leaf.segments, alloc), item, range });
            leaves_of.push((i, range, dollar));
        }
    }
    // Each import's start, where its own path says it (the module doc).
    for n in 0..uses.len() {
        if uses[n].start.is_some() {
            continue;
        }
        let Some(&first) = uses[n].written.first() else {
            uses[n].start = Some(Start { abs: false, at: module, path: AVec::new_in(alloc), dollar: None });
            continue;
        };
        if is_head(first) {
            let path = copy(&uses[n].written, alloc);
            uses[n].start = Some(Start { abs: false, at: module, path, dollar: leaves_of[n].2 });
        } else if local_names.contains(&first) {
            // An item of a block: read at the module of the block the `use` is written in, the
            // scope table's for the `use`'s scope (super::scope_table, where the order is
            // stated): a hand block's or an expansion's, by the block, never by a site.
            let (i, range, _) = leaves_of[n];
            if table.is_none() {
                let Some(crate_key) = paths.names.crate_of(module).and_then(|k| paths.names.crate_key(k)) else {
                    return Err(LocalGap { item, form: THROUGH_LOCAL, range });
                };
                let built = super::scope_table::build(crate_key, module, view, &blocks.nested, blocks.modules.as_deref(), planted, alloc)
                    .map_err(|r| LocalGap { item, form: r.form(), range })?;
                table = Some(built);
            }
            // A block the library filed no module for: its items are nowhere to read (refused).
            let m = match table.as_ref().and_then(|t| t.at(block_uses[i].scope.0)) {
                Some(super::scope_table::ScopeAt::Own(m) | super::scope_table::ScopeAt::Inherits(m)) => m,
                Some(super::scope_table::ScopeAt::Unfiled(_)) | None => return Err(LocalGap { item, form: THROUGH_LOCAL, range }),
            };
            let path = copy(&uses[n].written, alloc);
            uses[n].start = Some(Start { abs: false, at: m, path, dollar: None });
        } else if uses[n].bound != Some(first)
            && let Some((_, through, abs, dollar)) = named.iter().find(|(b, _, _, _)| *b == first)
        {
            // A named import of a block: its path stands for the name (one step: an import
            // through another block import is read as written there; `use x::x;` names the
            // `x` outside), its first segment's `$crate` with it.
            let mut path = copy(through, alloc);
            path.extend_from_slice(&uses[n].written[1..]);
            uses[n].start = Some(Start { abs: *abs, at: module, path, dollar: *dollar });
        }
    }
    Ok(lookup(paths, module, &uses, name, Ns::Value, alloc)?.map(|hit| hit.target))
}
