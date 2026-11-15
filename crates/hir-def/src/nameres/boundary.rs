//! Name resolution's fact boundaries: what a reader of a crate's finished def map lists for
//! what it looked up, one row per name, scope or table, never the crate's files.
//!
//! A crate's def map is the value of one query that reads its files whole
//! (`crate_resolution`, or `crate_served` from its pages). Read through that value alone, a
//! body's step would list every file of its crate (`base_db::VfsText`, `VfsFileRoot`,
//! `VfsRoot` rows): no record of it could be kept past an edit of any other body. So:
//!
//! - the two crate queries, `crate_shadows_primitive_type` and [`blaze_edition`] are fact
//!   boundaries (their reads stay behind them, `blaze_memo::Builder::mark_boundary`): a row
//!   of a crate's is what its def map holds outside its scopes ([`NameRead::Crate`], the
//!   crate's `CrateNames` fact, which holds the edition);
//! - every read of a scope of the finished map (`ScopeSlot::Final`) or of a paged crate's
//!   (`ScopeSlot::Paged`) goes through a boundary keyed by the scope and, for a lookup, the
//!   name: [`final_name`], [`final_legacy_macro`] ([`NameRead::Name`], lane items'
//!   `Resolves` fact of the module and the name), [`final_traits`] ([`NameRead::Traits`]),
//!   [`final_scope`] and [`final_children`] ([`NameRead::WholeScope`], a scan); the same for
//!   pages;
//! - a walk of every module of a crate's finished map lists [`all_modules`]
//!   ([`NameRead::AllModules`], a scan of the crate).
//!
//! A block's def map (`block_def_map`) is NOT a boundary: its reads are its reader's. A block
//! is the text of the body that holds it, and its def map a function of that text and of what
//! its resolution looked up outside it. The text is read through the holding body's syntax
//! (`block_item_tree_query` reads `body_syntax` of the block's owner, the boundaries
//! `item_body_syntax` / `placed_body_syntax`), so the body's own block lists a row its key
//! covers; what the block's imports, globs and macros looked up in modules outside it goes
//! through the boundaries above (the crate map's scopes are `ScopeSlot::Final`), so it is in
//! the reader's list as ordinary rows: `Resolves(m, name)`, `TraitsInScope(m)`,
//! `WholeScope(m)`, `CrateNames`. No fact stands for a block's scope as a whole, so nothing
//! has to make one.
//!
//! A block of ANOTHER body: its reads are listed the same way, and its text is a row of the
//! other body's syntax, which a reader refuses (`item_body_syntax` of another body): loud,
//! never stale. The only direct path is an item declared in a block (a nested `fn`, a
//! `const`, an `impl`'s method) reading the block it is in, and such an item has no key of its
//! own (`blaze_item_hash::identity::early_parts`), so no list of it is filed. Every other
//! reader of another body reads what that body's maker made (its steps' facts, lane items'
//! facts of the items in its blocks), whose own reads stay behind those boundaries.
//!
//! The floor (rule 67): an edit of one body's text moves no row of another body (no row is of
//! a file); a new item in module `m` moves the rows of `m`'s names it binds or shadows, of a
//! scan of `m` and of a walk of the crate's modules, and no other. A name that is a keyed
//! (hygienic) name is [`NameRead::Keyed`]: no fact spells its key, so a reader refuses it.
//!
//! Cost: one memo entry per (scope, name) a finished map is asked about, each a `PerNs` or a
//! reference; the views collection reads keep `ScopeSlot::Fact` and list nothing new.

use base_db::{Crate, Db};
use blaze_memo::Read;
use hir_expand::name::Name;

use crate::{
    FxIndexMap, MacroId, ModuleId, TraitId,
    item_scope::ItemScope,
    nameres::{
        ModulesSlot, ScopeSlot, crate_def_map, crate_resolution, crate_served,
        crate_shadows_primitive_type,
        facts::{
            self,
            queries::{Scope, ScopeRef},
        },
        frozen::pages::{self, PagedScope},
    },
    per_ns::PerNs,
};

/// What `name` means in the finished scope `scope`.
#[blaze_memo::tracked(returns(copy))]
pub(crate) fn final_name(db: &Db, scope: ScopeRef, name: Name) -> PerNs {
    facts::fact_scope(db, scope).scope.get(&name)
}

/// The textual (`macro_rules!`) macros of `name` in the finished scope `scope`.
#[blaze_memo::tracked(returns(copy))]
pub(crate) fn final_legacy_macro<'db>(
    db: &'db Db,
    scope: ScopeRef,
    name: Name,
) -> Option<&'db [MacroId]> {
    facts::fact_scope(db, scope).scope.get_legacy_macro(&name)
}

/// The finished scope `scope`, for its traits in scope alone: a reader iterates its traits and
/// nothing else of it (`ModuleData::traits_in_scope`), so its row is the module's
/// `TraitsInScope` fact.
#[blaze_memo::tracked(returns(copy))]
pub(crate) fn final_traits<'db>(db: &'db Db, scope: ScopeRef) -> &'db ItemScope {
    &facts::fact_scope(db, scope).scope
}

/// The whole of the finished scope `scope`.
#[blaze_memo::tracked(returns(copy))]
pub(crate) fn final_scope<'db>(db: &'db Db, scope: ScopeRef) -> &'db ItemScope {
    &facts::fact_scope(db, scope).scope
}

/// The modules declared in the finished scope `scope`'s module, by name: names of its scope,
/// each a type, so a read of them is a read of the scope.
#[blaze_memo::tracked(returns(copy))]
pub(crate) fn final_children<'db>(db: &'db Db, scope: ScopeRef) -> &'db FxIndexMap<Name, ModuleId> {
    &facts::fact_scope(db, scope).children
}

/// What `name` means in the paged scope `paged`.
#[blaze_memo::tracked(returns(copy))]
pub(crate) fn paged_name(db: &Db, paged: PagedScope, name: Name) -> PerNs {
    pages::name(db, paged, name)
}

/// The textual macros of `name` in the paged scope `paged`.
#[blaze_memo::tracked(returns(copy))]
pub(crate) fn paged_legacy_macro<'db>(
    db: &'db Db,
    paged: PagedScope,
    name: Name,
) -> Option<&'db [MacroId]> {
    pages::legacy_macro(db, paged, name)
}

/// The traits in scope in the paged scope `paged`.
#[blaze_memo::tracked(returns(copy))]
pub(crate) fn paged_traits<'db>(db: &'db Db, paged: PagedScope) -> &'db [TraitId] {
    pages::traits(db, paged)
}

/// The whole of the paged scope `paged`.
#[blaze_memo::tracked(returns(copy))]
pub(crate) fn paged_scope<'db>(db: &'db Db, paged: PagedScope) -> &'db ItemScope {
    pages::scope(db, paged)
}

/// The modules declared in the paged scope `paged`'s module, by name.
#[blaze_memo::tracked(returns(copy))]
pub(crate) fn paged_children<'db>(
    db: &'db Db,
    paged: PagedScope,
) -> &'db FxIndexMap<Name, ModuleId> {
    pages::children(db, paged)
}

/// The read of every module of `krate`'s finished def map (`DefMap::table`): a row and nothing
/// else. It reads nothing, so it is asked from inside the crate's own resolution without a
/// cycle; what the row stands for is the crate's modules as its def map lists them now
/// ([`NameRead::AllModules`]), valued by the reader of the row, never by this entry.
#[blaze_memo::tracked(returns(copy))]
pub(crate) fn all_modules(db: &Db, krate: Crate) {
    let _ = (db, krate);
}

/// The blaze edition of `krate` (`base_db::Crate::blaze_edition`), as name resolution reads it:
/// which dependency is the crate's `std` and which prelude its modules and blocks see. The
/// edition is said in the crate root's text (`#![blaze::edition]`), so read bare it lists the
/// root FILE (`VfsText`) in whoever reads it; behind this boundary a reader lists one row of
/// the crate ([`NameRead::Crate`], its `CrateNames` fact, which holds the edition). A block's
/// collection reads it (the dependency table, `collect::deps`, and the prelude it injects,
/// `collect::inject_prelude`), and a block's reads are its reader's: without this, every body
/// with a block of items in a crate's root file read that file whole.
///
/// Of a crate read from pages (a crate given by its address, `deps::give_keyed`, among them),
/// its root module's attribute fact `blaze edition` (`deps::facts::module_place`: keyed by the
/// root's inner attributes, how the root is read, which is the crate's Rust edition, and each
/// command-line crate attribute's content: all the edition is read from), so it is read with
/// no text of the root. Not kept (a store that never asked it), the root's text is read, and of
/// a crate given by its address whose root has no text here that is a typed refusal naming the
/// crate and its root (`frozen::pages::demand_root_text`). A crate resolved here reads its root:
/// its def map, which finding the root module's fact reads, is being made from it.
///
/// Floor and cost: `deps::facts`'s, one entry a crate (one byte of value).
#[blaze_memo::tracked(returns(copy))]
pub fn blaze_edition(db: &Db, krate: Crate) -> Option<span::BlazeEdition> {
    match super::frozen::pages::is_paged(db, krate) {
        true => blaze_edition_fact(db, krate),
        false => blaze_edition_read(db, krate),
    }
}

/// [`blaze_edition`] as its root module's fact, kept where it is made: what a paged crate
/// reads, and what `deps::facts::keep_all` asks of a crate it images (resolved here, so its
/// def map is there to find the root module in).
pub(crate) fn blaze_edition_fact(db: &Db, krate: Crate) -> Option<span::BlazeEdition> {
    let root = crate::AttrDefId::ModuleId(super::crate_def_map(db, krate).root_module_id());
    crate::deps::facts::of_attrs(db, "blaze edition", root, || {
        (blaze_edition_read(db, krate), true)
    })
}

/// [`blaze_edition`] off the root's text, refused first where it has none here.
fn blaze_edition_read(db: &Db, krate: Crate) -> Option<span::BlazeEdition> {
    super::frozen::pages::demand_root_text(db, krate);
    krate.blaze_edition(db)
}

/// Every kind of name resolution's fact boundaries, as `blaze_memo::Builder::mark_boundary`
/// takes them: whoever marks them answers each row by [`name_read`].
pub const KINDS: &[&str] = &[
    final_name::KIND_NAME,
    final_legacy_macro::KIND_NAME,
    final_traits::KIND_NAME,
    final_scope::KIND_NAME,
    final_children::KIND_NAME,
    paged_name::KIND_NAME,
    paged_legacy_macro::KIND_NAME,
    paged_traits::KIND_NAME,
    paged_scope::KIND_NAME,
    paged_children::KIND_NAME,
    all_modules::KIND_NAME,
    crate_resolution::KIND_NAME,
    crate_served::KIND_NAME,
    crate_shadows_primitive_type::KIND_NAME,
    blaze_edition::KIND_NAME,
];

/// What one row of name resolution's boundaries ([`KINDS`]) was a read of.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NameRead {
    /// One name looked up in a module's scope, in its namespaces or among its textual macros;
    /// found or not. Lane items' `Resolves` fact of the module and the name's text.
    Name(ModuleId, Name),
    /// As [`NameRead::Name`], of a name a `macro` 2.0 or a proc macro's `Span::def_site` keyed:
    /// no fact spells the key, so its reader refuses it.
    Keyed(ModuleId),
    /// The traits in scope in a module.
    Traits(ModuleId),
    /// The whole of a module's scope, or the modules declared in it: an iteration of it.
    WholeScope(ModuleId),
    /// Every module of a crate: an iteration of all of them.
    AllModules(Crate),
    /// What a crate's def map holds outside its modules' scopes: its preludes, the names and
    /// flags its root's attributes decide, its features, recursion limit, exported macros and
    /// blaze edition (`super::crate_names`). A row of the crate's resolution, of
    /// `crate_shadows_primitive_type` or of [`blaze_edition`]. The rows of the module table itself (each module's origin,
    /// visibility and parent) are structure a name's row already moves with: a module's
    /// visibility is its parent's binding's, its parent and origin its declaration's.
    Crate(Crate),
    /// A row of a scope that is not the one its crate's finished def map or pages hold for its
    /// module: no fact stands for it.
    Unplaced,
}

/// What `read` was of: `None` for a row of no kind of [`KINDS`].
pub fn name_read(db: &Db, read: Read) -> Option<NameRead> {
    let memo = blaze_memo::Database::memo(db);
    if let Some((scope, name)) =
        final_name::key_of(memo, read).or_else(|| final_legacy_macro::key_of(memo, read))
    {
        return Some(named(final_module(db, scope), name));
    }
    if let Some((scope,)) = final_traits::key_of(memo, read) {
        return Some(final_module(db, scope).map_or(NameRead::Unplaced, NameRead::Traits));
    }
    if let Some((scope,)) =
        final_scope::key_of(memo, read).or_else(|| final_children::key_of(memo, read))
    {
        return Some(final_module(db, scope).map_or(NameRead::Unplaced, NameRead::WholeScope));
    }
    if let Some((paged, name)) =
        paged_name::key_of(memo, read).or_else(|| paged_legacy_macro::key_of(memo, read))
    {
        return Some(named(paged_module(db, paged), name));
    }
    if let Some((paged,)) = paged_traits::key_of(memo, read) {
        return Some(paged_module(db, paged).map_or(NameRead::Unplaced, NameRead::Traits));
    }
    if let Some((paged,)) =
        paged_scope::key_of(memo, read).or_else(|| paged_children::key_of(memo, read))
    {
        return Some(paged_module(db, paged).map_or(NameRead::Unplaced, NameRead::WholeScope));
    }
    if let Some((krate,)) = all_modules::key_of(memo, read) {
        return Some(NameRead::AllModules(krate));
    }
    if let Some((krate,)) = crate_resolution::key_of(memo, read)
        .or_else(|| crate_served::key_of(memo, read))
        .or_else(|| crate_shadows_primitive_type::key_of(memo, read))
        .or_else(|| blaze_edition::key_of(memo, read))
    {
        return Some(NameRead::Crate(krate));
    }
    None
}

/// A lookup's row: of a keyed name, [`NameRead::Keyed`].
fn named(module: Option<ModuleId>, name: Name) -> NameRead {
    match (module, name.key()) {
        (None, _) => NameRead::Unplaced,
        (Some(module), Some(_)) => NameRead::Keyed(module),
        (Some(module), None) => NameRead::Name(module, name),
    }
}

/// The module whose scope in its crate's finished def map is `scope`: the module its fact
/// names, when that map holds `scope` for it (`ScopeSlot::Final`); `None` for a scope of a
/// round before the last, or of a crate its pages serve.
fn final_module(db: &Db, scope: ScopeRef) -> Option<ModuleId> {
    let module = match scope.scope(db) {
        Scope::Fact(key) => key.module(db),
        Scope::Decl(key) => key.decl(db).module,
    };
    let def_map = crate_def_map(db, module.krate(db));
    let ModulesSlot::Collected(modules) = &def_map.modules else { return None };
    let held = modules.get(&module)?;
    (held.scope == ScopeSlot::Final(scope)).then_some(module)
}

/// The module whose scope in its crate's pages is `paged` (its place in the crate's table),
/// when the crate's def map holds `paged` for it.
fn paged_module(db: &Db, paged: PagedScope) -> Option<ModuleId> {
    let def_map = crate_def_map(db, paged.krate(db));
    let ModulesSlot::Collected(modules) = &def_map.modules else { return None };
    let (&module, held) = modules.get_index(paged.module(db) as usize)?;
    (held.scope == ScopeSlot::Paged(paged)).then_some(module)
}
