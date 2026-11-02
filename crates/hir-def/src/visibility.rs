//! Defines hir-level representation of visibility (e.g. `pub` and `pub(crate)`).

use std::iter;

use base_db::{Db, Crate};
use hir_expand::{
    AstId, InFile, Lookup,
    mod_path::{ModPath, PathKind},
};
use la_arena::ArenaMap;
use span::{AstIdNode, Edition};
use syntax::view as ast;

use crate::{
    AssocItemId, BlockIdLt, HasModule, ItemContainerId, LocalFieldId, ModuleDefId, ModuleId,
    ModuleIdLt, TraitId, VariantId, nameres::DefMap, resolver::HasResolver,
    signatures::VariantFields,
};

pub use crate::item_tree::{RawVisibility, VisibilityExplicitness};

/// Visibility of an item, with the path resolved.
#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
pub enum Visibility {
    /// Visibility is restricted to a certain module.
    Module(ModuleId, VisibilityExplicitness),
    /// Visibility is restricted to the crate.
    PubCrate(Crate),
    /// Visibility is unrestricted.
    Public,
}

// A module id (4 bytes) and the explicitness, whose spare values tag the other two.
const _: () = assert!(size_of::<Visibility>() <= 8);

/// Why a `pub(in path)` (or `pub(super)`) gives no visibility: rustc's `VisResolutionError`,
/// from `rustc_resolve::build_reduced_graph`'s `try_resolve_visibility`.
#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
pub enum VisibilityError {
    /// The path does not resolve in the type namespace (rustc's E0433, "failed to resolve").
    Unresolved,
    /// The path names `found`, which is not a module (rustc's E0577, "expected module").
    NotAModule { found: ModuleDefId },
    /// The path names `module`, which is not the item's module or one above it (rustc's E0742,
    /// "visibilities can only be restricted to ancestor modules").
    NotAncestor { module: ModuleId },
    /// The path starts with a name in Rust 2018 or later (rustc's "relative paths are not
    /// supported in visibilities in 2018 edition or later").
    Relative2018,
}

/// The path rustc resolves for `pub(in path)` (`try_resolve_visibility`): one that starts with a
/// name is from the crate root in Rust 2015 and an error after it.
pub(crate) fn visibility_path(path: &ModPath, edition: Edition) -> Result<ModPath, VisibilityError> {
    match path.kind {
        PathKind::Plain if edition == Edition::Edition2015 => {
            Ok(ModPath::from_segments(PathKind::Abs, path.segments().iter().copied()))
        }
        PathKind::Plain => Err(VisibilityError::Relative2018),
        _ => Ok(*path),
    }
}

/// rustc's error for `ast_vis`, the visibility written on an item in `scope`: what
/// [`visibility_from_ast_view`] and the def collectors recover from.
pub fn visibility_error_from_ast_view(
    db: &Db,
    scope: impl HasResolver,
    ast_vis: InFile<Option<ast::Visibility<'_>>>,
) -> Option<VisibilityError> {
    let mut span_map = None;
    let raw_vis = crate::item_tree::visibility_from_ast(db, ast_vis.value, &mut |range| {
        span_map.get_or_insert_with(|| ast_vis.file_id.span_map(db)).span_for_range(range).ctx
    });
    match raw_vis {
        RawVisibility::Module(..) => Visibility::try_resolve(db, &scope.resolver(db), &raw_vis).err(),
        RawVisibility::PubSelf(_) | RawVisibility::PubCrate | RawVisibility::Public => None,
    }
}

impl VisibilityError {
    /// The visibility the item has after the error is reported, as rustc's `resolve_visibility`
    /// gives it (`ty::Visibility::Public`); the build stops on the error.
    pub const RECOVERED: Visibility = Visibility::Public;
}

/// Whether `ancestor` is `module` or a module above it in their `DefMap`, by the parent each
/// module id carries.
fn is_ancestor_in_def_map<'db>(
    db: &'db Db,
    ancestor: ModuleIdLt<'db>,
    module: ModuleIdLt<'db>,
) -> bool {
    iter::successors(Some(module), |&m| m.containing_module_inside_def_map(db))
        .any(|m| m == ancestor)
}

impl Visibility {
    /// [`Self::try_resolve`], with an error's [`VisibilityError::RECOVERED`]: for a reader of
    /// the item whose error the item's own check reports.
    pub fn resolve(
        db: &Db,
        resolver: &crate::resolver::Resolver<'_>,
        raw_vis: &RawVisibility,
    ) -> Self {
        Self::try_resolve(db, resolver, raw_vis).unwrap_or(VisibilityError::RECOVERED)
    }

    /// The visibility `raw_vis` gives an item in `resolver`'s scope, or why it gives none.
    pub fn try_resolve(
        db: &Db,
        resolver: &crate::resolver::Resolver<'_>,
        raw_vis: &RawVisibility,
    ) -> Result<Self, VisibilityError> {
        resolver.resolve_visibility(db, raw_vis)
    }

    pub(crate) fn is_visible_from_other_crate(self) -> bool {
        matches!(self, Visibility::Public)
    }

    pub fn is_visible_from<'db>(
        self,
        db: &'db Db,
        from_module: ModuleIdLt<'db>,
    ) -> bool {
        let to_module = match self {
            // A `Visibility` is resolved against `db`'s def maps and asked with the same `db`.
            Visibility::Module(m, _) => m.to_db(db),
            Visibility::PubCrate(krate) => return from_module.krate(db) == krate,
            Visibility::Public => return true,
        };
        if from_module == to_module {
            // if the modules are the same, visibility is trivially satisfied
            return true;
        }
        // if they're not in the same crate, it can't be visible
        if from_module.krate(db) != to_module.krate(db) {
            return false;
        }
        Self::is_visible_from_def_map_(db, from_module.block(db), to_module, from_module)
    }

    pub(crate) fn is_visible_from_def_map<'db>(
        self,
        db: &'db Db,
        def_map: &'db DefMap,
        from_module: ModuleIdLt<'db>,
    ) -> bool {
        if cfg!(debug_assertions) {
            _ = def_map.module(db, from_module);
        }
        let to_module = match self {
            Visibility::Module(m, _) => m,
            Visibility::PubCrate(krate) => return from_module.krate(db) == krate,
            Visibility::Public => return true,
        };
        if from_module == to_module {
            // if the modules are the same, visibility is trivially satisfied
            return true;
        }
        // if they're not in the same crate, it can't be visible
        if def_map.krate() != to_module.krate(db) {
            return false;
        }

        if from_module == to_module && def_map.block_id() == to_module.block(db) {
            // if the modules are the same, visibility is trivially satisfied
            return true;
        }
        Self::is_visible_from_def_map_(db, def_map.block_id(), to_module, from_module)
    }

    /// Whether `from_module`, in the `DefMap` of `def_map_block`, sees what is visible in
    /// `to_module`. Each step up reads the module id's own fields (its parent in its `DefMap`, or
    /// the module its block is in), never a `DefMap`: `to_module` may be in the one being
    /// computed.
    fn is_visible_from_def_map_<'db>(
        db: &'db Db,
        def_map_block: Option<BlockIdLt<'db>>,
        mut to_module: ModuleIdLt<'db>,
        mut from_module: ModuleIdLt<'db>,
    ) -> bool {
        // `to_module` might be the root module of a block expression. Those have the same
        // visibility as the containing module (even though no items are directly nameable from
        // there, getting this right is important for method resolution).
        // In that case, we adjust the visibility of `to_module` to point to the containing module.
        loop {
            match (to_module.block(db), def_map_block) {
                // `to_module` is not a block, so there is no parent def map to use.
                (None, _) => (),
                // `to_module` is at `def_map`'s block, no need to move further.
                (Some(a), Some(b)) if a == b => {}
                // The module the block is in is the parent of the block's `DefMap`.
                (Some(block), _) => {
                    to_module = block.module(db);
                    continue;
                }
            }
            break;
        }

        // from_module needs to be a descendant of to_module
        loop {
            if from_module == to_module {
                return true;
            }
            match from_module.containing_module(db) {
                Some(parent) => from_module = parent,
                // Reached the root module, nothing left to check.
                None => return false,
            }
        }
    }

    /// Returns the most permissive visibility of `self` and `other`.
    ///
    /// If there is no subset relation between `self` and `other`, returns `None` (i.e. they're only
    /// visible in unrelated modules).
    pub(crate) fn max(
        self,
        db: &Db,
        other: Visibility,
        def_map: &DefMap,
    ) -> Option<Visibility> {
        match (self, other) {
            (_, Visibility::Public) | (Visibility::Public, _) => Some(Visibility::Public),
            (Visibility::PubCrate(krate), Visibility::PubCrate(krateb)) => {
                if krate == krateb {
                    Some(Visibility::PubCrate(krate))
                } else {
                    None
                }
            }
            (Visibility::Module(mod_, _), Visibility::PubCrate(krate))
            | (Visibility::PubCrate(krate), Visibility::Module(mod_, _)) => {
                if mod_.krate(db) == krate { Some(Visibility::PubCrate(krate)) } else { None }
            }
            (Visibility::Module(mod_a, expl_a), Visibility::Module(mod_b, expl_b)) => {
                if mod_a == mod_b {
                    // Most module visibilities are `pub(self)`, and assuming no errors
                    // this will be the common and thus fast path.
                    return Some(Visibility::Module(
                        mod_a,
                        match (expl_a, expl_b) {
                            (VisibilityExplicitness::Explicit, _)
                            | (_, VisibilityExplicitness::Explicit) => {
                                VisibilityExplicitness::Explicit
                            }
                            _ => VisibilityExplicitness::Implicit,
                        },
                    ));
                }

                if mod_a.krate(db) != def_map.krate() || mod_b.krate(db) != def_map.krate() {
                    return None;
                }

                let def_block = def_map.block_id();
                if mod_a.block(db) != def_block || mod_b.block(db) != def_block {
                    return None;
                }

                if is_ancestor_in_def_map(db, mod_b, mod_a) {
                    // B is above A
                    return Some(Visibility::Module(mod_b, expl_b));
                }

                if is_ancestor_in_def_map(db, mod_a, mod_b) {
                    // A is above B
                    return Some(Visibility::Module(mod_a, expl_a));
                }

                None
            }
        }
    }

    /// `self`, a restriction to the crate root spelled `pub(crate)`: the same visibility, which
    /// a glob passes on in one spelling whichever of a private root import and a `pub(crate)`
    /// item reached it first.
    pub(crate) fn root_as_pub_crate(self, db: &Db, def_map: &DefMap) -> Visibility {
        match self {
            Visibility::Module(module, _) if module == def_map.crate_root(db) => {
                Visibility::PubCrate(def_map.krate())
            }
            other => other,
        }
    }

    /// Returns the least permissive visibility of `self` and `other`.
    ///
    /// If there is no subset relation between `self` and `other`, returns `None` (i.e. they're only
    /// visible in unrelated modules).
    pub(crate) fn min(
        self,
        db: &Db,
        other: Visibility,
        def_map: &DefMap,
    ) -> Option<Visibility> {
        match (self, other) {
            (vis, Visibility::Public) | (Visibility::Public, vis) => Some(vis),
            (Visibility::PubCrate(krate), Visibility::PubCrate(krateb)) => {
                if krate == krateb {
                    Some(Visibility::PubCrate(krate))
                } else {
                    None
                }
            }
            (Visibility::Module(mod_, exp), Visibility::PubCrate(krate))
            | (Visibility::PubCrate(krate), Visibility::Module(mod_, exp)) => {
                if mod_.krate(db) == krate { Some(Visibility::Module(mod_, exp)) } else { None }
            }
            (Visibility::Module(mod_a, expl_a), Visibility::Module(mod_b, expl_b)) => {
                if mod_a == mod_b {
                    // Most module visibilities are `pub(self)`, and assuming no errors
                    // this will be the common and thus fast path.
                    return Some(Visibility::Module(
                        mod_a,
                        match (expl_a, expl_b) {
                            (VisibilityExplicitness::Explicit, _)
                            | (_, VisibilityExplicitness::Explicit) => {
                                VisibilityExplicitness::Explicit
                            }
                            _ => VisibilityExplicitness::Implicit,
                        },
                    ));
                }

                if mod_a.krate(db) != def_map.krate() || mod_b.krate(db) != def_map.krate() {
                    return None;
                }

                let def_block = def_map.block_id();
                if mod_a.block(db) != def_block || mod_b.block(db) != def_block {
                    return None;
                }

                if is_ancestor_in_def_map(db, mod_b, mod_a) {
                    // B is above A
                    return Some(Visibility::Module(mod_a, expl_a));
                }

                if is_ancestor_in_def_map(db, mod_a, mod_b) {
                    // A is above B
                    return Some(Visibility::Module(mod_b, expl_b));
                }

                None
            }
        }
    }

    #[inline]
    pub fn is_public(&self) -> bool {
        matches!(self, Visibility::Public)
    }
}

#[blaze_query::tracked]
impl VariantFields {
    /// Resolve visibility of all specific fields of a struct or union variant.
    #[blaze_query::tracked(returns(ref))]
    pub fn field_visibilities(
        db: &Db,
        variant_id: VariantId,
    ) -> ArenaMap<LocalFieldId, Visibility> {
        let variant_fields = variant_id.fields(db);
        let fields = variant_fields.fields();
        if fields.is_empty() {
            return ArenaMap::default();
        }
        let resolver = variant_id.module(db).resolver(db);
        let mut res = ArenaMap::with_capacity(fields.len());
        for (field_id, field_data) in fields.iter() {
            res.insert(field_id, Visibility::resolve(db, &resolver, &field_data.visibility));
        }
        res
    }
}

pub fn visibility_from_ast_view(
    db: &Db,
    has_resolver: impl HasResolver + HasModule,
    ast_vis: InFile<Option<ast::Visibility<'_>>>,
) -> Visibility {
    let mut span_map = None;
    let raw_vis = crate::item_tree::visibility_from_ast(db, ast_vis.value, &mut |range| {
        span_map.get_or_insert_with(|| ast_vis.file_id.span_map(db)).span_for_range(range).ctx
    });
    resolve_raw(db, has_resolver, &raw_vis)
}

/// The visibility written on the item at `id`, resolved for `has_resolver`: read off its file's
/// outline, so an edit of a body the outline leaves out never changes it
/// ([`crate::signatures::written_visibility`]).
pub fn written_visibility<N: AstIdNode>(
    db: &Db,
    has_resolver: impl HasResolver + HasModule,
    id: AstId<N>,
) -> Visibility {
    let module = has_resolver.module(db);
    resolve_raw(db, has_resolver, &crate::signatures::written_visibility(db, id, module))
}

fn resolve_raw(db: &Db, has_resolver: impl HasResolver + HasModule, raw_vis: &RawVisibility) -> Visibility {
    match raw_vis {
        &RawVisibility::PubSelf(explicitness) => {
            Visibility::Module(has_resolver.module(db), explicitness)
        }
        RawVisibility::PubCrate => Visibility::PubCrate(has_resolver.krate(db)),
        RawVisibility::Public => Visibility::Public,
        RawVisibility::Module(..) => Visibility::resolve(db, &has_resolver.resolver(db), raw_vis),
    }
}

#[blaze_query::tracked]
impl AssocItemId {
    /// Resolve visibility of an assoc item.
    #[blaze_query::tracked(returns(copy))]
    pub fn assoc_visibility(self, db: &Db) -> Visibility {
        match self {
            AssocItemId::FunctionId(function_id) => {
                let loc = function_id.lookup(db);
                trait_item_visibility(db, loc.container)
                    .unwrap_or_else(|| written_visibility(db, function_id, loc.id))
            }
            AssocItemId::ConstId(const_id) => {
                let loc = const_id.lookup(db);
                trait_item_visibility(db, loc.container)
                    .unwrap_or_else(|| written_visibility(db, const_id, loc.id))
            }
            AssocItemId::TypeAliasId(type_alias_id) => {
                let loc = type_alias_id.lookup(db);
                trait_item_visibility(db, loc.container)
                    .unwrap_or_else(|| written_visibility(db, type_alias_id, loc.id))
            }
        }
    }
}

fn trait_item_visibility(
    db: &Db,
    container: ItemContainerId,
) -> Option<Visibility> {
    match container {
        ItemContainerId::TraitId(trait_) => Some(trait_visibility(db, trait_)),
        _ => None,
    }
}

fn trait_visibility(db: &Db, def: TraitId) -> Visibility {
    written_visibility(db, def, def.lookup(db).id)
}

#[cfg(test)]
mod view_tests {
    use syntax::view::{HasName as _, HasVisibility as _};
    use hir_expand::fixture;

    use super::*;
    use crate::{
        AssocItemId, ModuleDefId,
        nameres::{
            assoc::{ImplItems, TraitItems},
            crate_def_map,
        },
        src::HasSource as _,
        test_db::{TestDB, TestDBExt},
    };

    /// Every way to write a visibility, on every kind of item that reads one from its syntax.
    const CODE: &str = r#"
pub mod m {
    pub(crate) fn a() {}
    pub(super) const B: u8 = 0;
    pub(in crate::m) type C = u8;
    pub(self) fn d() {}
    fn e() {}
    pub trait T { fn f(&self); const G: u8; type H; }
    pub(crate) trait U {}
    pub struct S;
    impl S { pub fn i() {} pub(crate) const J: u8 = 1; fn l() {} }
    impl T for S { fn f(&self) {} const G: u8 = 2; type H = u8; }
}
pub use m::*;
"#;

    /// A visibility with its module named as `CODE` names it.
    fn described(db: &TestDB, vis: Visibility) -> String {
        let root = crate_def_map(db, db.fetch_test_crate()).root_module_id();
        match vis {
            Visibility::Public => "pub".to_owned(),
            Visibility::PubCrate(_) => "pub(crate)".to_owned(),
            Visibility::Module(module, explicitness) => {
                let module = if module == root { "crate" } else { "m" };
                format!("{module} {explicitness:?}")
            }
        }
    }

    /// The visibility an item's own syntax gives, by the item's name.
    macro_rules! seen {
        ($db:ident, $seen:ident, $id:expr) => {{
            let id = $id;
            let source = id.lookup(&$db).source_view(&$db);
            let name = source.value.name().map_or("", |it| it.text()).to_owned();
            let vis = visibility_from_ast_view(&$db, id, source.map(|it| it.visibility()));
            $seen.push((name, described(&$db, vis)));
        }};
    }

    /// `visibility_from_ast_view` of every function, const, type alias and trait of `CODE`, the
    /// items of its traits and impls too: `pub(super)` is the crate, `pub(self)` and no
    /// visibility are the item's module (explicit and not), and an item of a trait or impl reads
    /// only its own syntax.
    #[test]
    fn visibility_of_each_way_to_write_one() {
        let fx = fixture::db(CODE);
        let _ = fx.only_file();
        let db = fx.into_db();
        let mut seen = Vec::new();
        let mut items = Vec::new();
        let mut traits = 0;
        for (_, module) in crate_def_map(&db, db.fetch_test_crate()).modules(&db) {
            let scope = module.scope(&db);
            for decl in scope.declarations() {
                match decl {
                    ModuleDefId::FunctionId(it) => seen!(db, seen, it),
                    ModuleDefId::ConstId(it) => seen!(db, seen, it),
                    ModuleDefId::TypeAliasId(it) => seen!(db, seen, it),
                    ModuleDefId::TraitId(it) => {
                        seen!(db, seen, it);
                        traits += 1;
                        items.extend(TraitItems::query(&db, it).items.iter().map(|&(_, it)| it));
                    }
                    _ => {}
                }
            }
            for imp in scope.impls() {
                items.extend(ImplItems::of(&db, imp).0.items.iter().map(|&(_, it)| it));
            }
        }
        assert_eq!(traits, 2, "the traits of CODE");
        assert_eq!(items.len(), 9, "the trait and impl items of CODE");
        for item in items {
            match item {
                AssocItemId::FunctionId(it) => seen!(db, seen, it),
                AssocItemId::ConstId(it) => seen!(db, seen, it),
                AssocItemId::TypeAliasId(it) => seen!(db, seen, it),
            }
        }
        let mut want: Vec<(String, String)> = [
            ("a", "pub(crate)"),
            ("B", "crate Explicit"),
            ("C", "m Explicit"),
            ("d", "m Explicit"),
            ("e", "m Implicit"),
            ("T", "pub"),
            ("U", "pub(crate)"),
            // `T`'s items, `impl T for S`'s, then `impl S`'s.
            ("f", "m Implicit"),
            ("G", "m Implicit"),
            ("H", "m Implicit"),
            ("f", "m Implicit"),
            ("G", "m Implicit"),
            ("H", "m Implicit"),
            ("i", "pub"),
            ("J", "pub(crate)"),
            ("l", "m Implicit"),
        ]
        .map(|(name, vis)| (name.to_owned(), vis.to_owned()))
        .into();
        want.sort();
        seen.sort();
        assert_eq!(seen, want);
    }

    /// `Visibility::try_resolve` of each function's visibility, by name: rustc's E0433, E0577
    /// and E0742 where the path does not name the item's module or one above it, never a
    /// visibility guessed for it.
    #[test]
    fn restricted_visibility_errors() {
        let fx = fixture::db(
            r#"
pub mod m {
    pub(in crate::m) fn ok() {}
    pub(in crate::nope) fn unresolved() {}
    pub(in crate::S) fn not_a_module() {}
    pub(in crate::n) fn not_ancestor() {}
    pub(in m) fn relative() {}
}
pub mod n {}
pub struct S;
pub(super) fn too_far() {}
"#,
        );
        assert_eq!(restricted_visibilities(fx), [
            "not_a_module E0577",
            "not_ancestor E0742",
            "ok ok",
            "relative 2018",
            "too_far E0433",
            "unresolved E0433",
        ]);
    }

    /// In Rust 2015 a `pub(in path)` that starts with a name is from the crate root, as rustc
    /// resolves it.
    #[test]
    fn restricted_visibility_2015_is_from_the_root() {
        let fx = fixture::db(
            r#"
//- /main.rs edition:2015
pub mod m {
    pub mod k {
        pub(in m) fn ok() {}
        pub(in k) fn not_from_here() {}
    }
}
"#,
        );
        assert_eq!(restricted_visibilities(fx), ["not_from_here E0433", "ok ok"]);
    }

    /// `Visibility::try_resolve` of each function's visibility of `fx`, by name.
    fn restricted_visibilities(fx: fixture::Fixture) -> Vec<String> {
        let _ = fx.only_file();
        let db = fx.into_db();
        let mut seen = Vec::new();
        for (_, module) in crate_def_map(&db, db.fetch_test_crate()).modules(&db) {
            for decl in module.scope(&db).declarations() {
                let ModuleDefId::FunctionId(f) = decl else { continue };
                let source = f.lookup(&db).source_view(&db);
                let name = source.value.name().map_or("", |it| it.text()).to_owned();
                let vis = source.map(|it| it.visibility());
                let code = match visibility_error_from_ast_view(&db, f, vis) {
                    None => "ok",
                    Some(VisibilityError::Unresolved) => "E0433",
                    Some(VisibilityError::NotAModule { .. }) => "E0577",
                    Some(VisibilityError::NotAncestor { .. }) => "E0742",
                    Some(VisibilityError::Relative2018) => "2018",
                };
                seen.push(format!("{name} {code}"));
            }
        }
        seen.sort();
        seen
    }
}
