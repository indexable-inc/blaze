//! Utilities for mapping between hir IDs and the surface syntax.

use base_db::Db;
use either::Either;
use hir_expand::{AstId, InFile};
use la_arena::{Arena, ArenaMap, Idx};
use syntax::{
    AstNode, AstPtr, ast,
    view::{self, HasGenericParams as _, Typed},
};

use crate::{
    AstIdLoc, GenericDefId, LocalFieldId, LocalLifetimeParamId, LocalTypeOrConstParamId, Lookup,
    ModuleId, UseId, VariantId, attrs::AttrFlags, hir::generics::GenericParams,
};

#[cfg(test)]
mod tests;

pub trait HasSource {
    type Value: AstNode;
    fn source(&self, db: &Db) -> InFile<Self::Value> {
        let InFile { file_id, value } = self.ast_ptr(db);
        InFile::new(file_id, value.to_node(&file_id.parse_or_expand(db)))
    }
    /// [`Self::source`] as a view of the database's tree: the same node.
    fn source_view<'db>(&self, db: &'db Db) -> InFile<<Self::Value as Typed>::Of<'db>>
    where
        Self::Value: Typed,
    {
        let InFile { file_id, value } = self.ast_ptr(db);
        InFile::new(file_id, value.to_view(file_id.root(db)))
    }
    fn ast_ptr(&self, db: &Db) -> InFile<AstPtr<Self::Value>>;
}

impl<T> HasSource for T
where
    T: AstIdLoc,
{
    type Value = T::Ast;
    fn ast_ptr(&self, db: &Db) -> InFile<AstPtr<Self::Value>> {
        let id = self.ast_id();
        let ast_id_map = id.file_id.ast_id_map(db);
        InFile::new(id.file_id, ast_id_map.get(id.value))
    }
}

/// One child's node, as a view of the database's tree: found by walking the children up to
/// `child`, `None` where the owner has no such child.
pub trait HasChildSourceView<ChildId> {
    type Value<'db>: view::AstNode<'db>;
    fn child_source_view<'db>(
        &self,
        db: &'db Db,
        child: ChildId,
    ) -> InFile<Option<Self::Value<'db>>>;
}

/// Maps a `UseTree` contained in this import back to its node, a view of the database's tree.
pub fn use_tree_to_view<'db>(
    db: &'db Db,
    use_ast_id: AstId<ast::Use>,
    index: Idx<ast::UseTree>,
) -> view::UseTree<'db> {
    use_trees_view(db, use_ast_id)[Idx::from_raw(index.into_raw())]
}

/// Every `UseTree` of this import as a view, by the index the item tree gives it: the import
/// lowered again.
pub fn use_trees_view<'db>(
    db: &'db Db,
    use_ast_id: AstId<ast::Use>,
) -> Arena<view::UseTree<'db>> {
    // Note: The AST unwraps are fine, since if they fail we should have never obtained `index`.
    let use_tree = use_ast_id.to_view(db).use_tree().expect("missing `use_tree`");
    let mut span_map = None;
    crate::item_tree::lower_use_tree(db, use_tree, &mut |range| {
        span_map.get_or_insert_with(|| use_ast_id.file_id.span_map(db)).span_for_range(range).ctx
    })
    .expect("failed to lower use tree")
}

/// The use trees of a `UseId` as views of the database's tree: every `UseTree` of the import,
/// by the index the item tree gives it.
pub fn use_tree_sources_view<'db>(
    db: &'db Db,
    id: UseId,
) -> InFile<ArenaMap<Idx<ast::UseTree>, view::UseTree<'db>>> {
    let loc = id.lookup(db);
    let map = use_trees_view(db, loc.id)
        .iter()
        .map(|(idx, &tree)| (Idx::from_raw(idx.into_raw()), tree))
        // alloc: a use's trees by item-tree index, for diagnostics
        .collect();
    InFile::new(loc.id.file_id, map)
}

impl HasChildSourceView<LocalTypeOrConstParamId> for GenericDefId {
    type Value<'db> = Either<view::TypeOrConstParam<'db>, view::Trait<'db>>;
    fn child_source_view<'db>(
        &self,
        db: &'db Db,
        child: LocalTypeOrConstParamId,
    ) -> InFile<Option<Self::Value<'db>>> {
        let generic_params = GenericParams::of(db, *self);
        let mut idx_iter = generic_params.iter_type_or_consts().map(|(idx, _)| idx);
        // For traits the first type index is `Self`, the trait itself.
        let (file_id, list) = match *self {
            GenericDefId::TraitId(id) => {
                let source = id.lookup(db).source_view(db);
                if idx_iter.next() == Some(child) {
                    return InFile::new(source.file_id, Some(Either::Right(source.value)));
                }
                (source.file_id, source.value.generic_param_list())
            }
            _ => self.file_id_and_params_of_view(db),
        };
        let param = list.and_then(|list| {
            idx_iter.zip(list.type_or_const_params()).find(|&(idx, _)| idx == child)
        });
        InFile::new(file_id, param.map(|(_, it)| Either::Left(it)))
    }
}

impl HasChildSourceView<LocalLifetimeParamId> for GenericDefId {
    type Value<'db> = view::LifetimeParam<'db>;
    fn child_source_view<'db>(
        &self,
        db: &'db Db,
        child: LocalLifetimeParamId,
    ) -> InFile<Option<view::LifetimeParam<'db>>> {
        let generic_params = GenericParams::of(db, *self);
        let idx_iter = generic_params.iter_lt().map(|(idx, _)| idx);
        let (file_id, list) = self.file_id_and_params_of_view(db);
        let param = list
            .and_then(|list| idx_iter.zip(list.lifetime_params()).find(|&(idx, _)| idx == child));
        InFile::new(file_id, param.map(|(_, it)| it))
    }
}

/// The fields of `variant` as written, with the module whose crate's cfg says which are there.
fn variant_kind_view(db: &Db, variant: VariantId) -> (InFile<view::StructKind<'_>>, ModuleId) {
    match variant {
        VariantId::EnumVariantId(it) => {
            let lookup = it.lookup(db);
            (lookup.source_view(db).map(|it| it.kind()), lookup.parent.lookup(db).container)
        }
        VariantId::StructId(it) => {
            let lookup = it.lookup(db);
            (lookup.source_view(db).map(|it| it.kind()), lookup.container)
        }
        VariantId::UnionId(it) => {
            let lookup = it.lookup(db);
            (lookup.source_view(db).map(|it| it.kind()), lookup.container)
        }
    }
}

/// The fields of `variant` its crate's cfg keeps, in order: what the `LocalFieldId`s count.
pub fn enabled_fields_view(
    db: &Db,
    variant: VariantId,
) -> InFile<impl Iterator<Item = Either<view::TupleField<'_>, view::RecordField<'_>>> + use<'_>> {
    let (src, container) = variant_kind_view(db, variant);
    let cfg_options = container.krate(db).cfg_options(db);
    let fields = match src.value {
        view::StructKind::Tuple(fl) => Either::Left(
            fl.fields()
                .filter(move |&fd| AttrFlags::is_cfg_enabled_for(fd, cfg_options).is_ok())
                .map(Either::Left),
        ),
        view::StructKind::Record(fl) => Either::Right(Either::Left(
            fl.fields()
                .filter(move |&fd| AttrFlags::is_cfg_enabled_for(fd, cfg_options).is_ok())
                .map(Either::Right),
        )),
        view::StructKind::Unit => Either::Right(Either::Right(std::iter::empty())),
    };
    InFile::new(src.file_id, fields)
}

impl HasChildSourceView<LocalFieldId> for VariantId {
    type Value<'db> = Either<view::TupleField<'db>, view::RecordField<'db>>;
    fn child_source_view<'db>(
        &self,
        db: &'db Db,
        child: LocalFieldId,
    ) -> InFile<Option<Self::Value<'db>>> {
        let nth = u32::from(child.into_raw()) as usize;
        enabled_fields_view(db, *self).map(|mut fields| fields.nth(nth))
    }
}
