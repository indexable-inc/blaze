//! [`crate::HasSource`] over views: each def's node found from its pointer under the tree the
//! database keeps (`HirFileId::root`), with no handle made and nothing reparsed.

use base_db::Db;
use either::Either;
use hir_def::{
    CallableDefId, ExpressionStoreOwnerId, Lookup, MacroId, VariantId,
    expr_store::{Body, ExpressionStore},
    src::{HasChildSourceView, HasSource as _},
};
use hir_expand::{EditionedFileId, HirFileId, InFile};
use hir_ty::{db::InternedClosure, next_solver::AnyImplId};
use syntax::{
    SyntaxKind, SyntaxNodePtr, TextRange,
    view::{self as ast, AstNode, HasName as _, Node, Typed},
};

pub use hir_def::nameres::ModuleSourceView;

use crate::{
    Adt, AnyClosureId, AnyFunctionId, Callee, Closure, Const, Enum, EnumVariant, ExternCrateDecl,
    Field, Function, Impl, InlineAsmOperand, Label, LifetimeParam, Local, Macro, Module, Param,
    SelfParam, Static, Struct, Trait, TypeAlias, TypeOrConstParam, Union, UseDecl, Variant,
};

/// [`crate::HasSource`] giving views borrowed from the database.
pub trait HasSourceView: Sized {
    type Ast<'db>: AstNode<'db>;

    /// The definition's source node, as [`crate::HasSource::source`] finds it.
    fn source_view<'db>(self, db: &'db Db) -> Option<InFile<Self::Ast<'db>>>;

    /// [`crate::HasSource::source_with_range`] over views.
    fn source_with_range_view<'db>(
        self,
        db: &'db Db,
    ) -> Option<InFile<(TextRange, Option<Self::Ast<'db>>)>> {
        let source = self.source_view(db)?;
        Some(source.map(|node| (node.syntax().text_range(), Some(node))))
    }
}

/// The node a def's location points at, as a view.
fn loc_view<'db, L>(db: &'db Db, loc: &L) -> InFile<<L::Value as Typed>::Of<'db>>
where
    L: hir_def::src::HasSource,
    L::Value: Typed,
{
    let InFile { file_id, value } = loc.ast_ptr(db);
    InFile::new(file_id, value.to_view(file_id.root(db)))
}

/// The node `ptr` points at in `file_id`, cast to `N`: what `AstPtr<Either<..>>::to_node`
/// gives, for pointers whose kind tag has no single view type.
///
/// # Panics
///
/// When the node is not there, as `AstPtr::to_node` panics.
pub(crate) fn ptr_view<'db, N: AstNode<'db>>(
    db: &'db Db,
    file_id: HirFileId,
    ptr: SyntaxNodePtr,
) -> Option<N> {
    N::cast(ast::to_node(ptr, file_id.root(db)))
}

/// [`crate::FieldSource`] over views.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FieldSourceView<'db> {
    Named(ast::RecordField<'db>),
    Pos(ast::TupleField<'db>),
}

impl<'db> AstNode<'db> for FieldSourceView<'db> {
    fn can_cast(kind: SyntaxKind) -> bool {
        ast::RecordField::can_cast(kind) || ast::TupleField::can_cast(kind)
    }

    fn cast(syntax: Node<'db>) -> Option<Self> {
        if ast::RecordField::can_cast(syntax.kind()) {
            ast::RecordField::cast(syntax).map(FieldSourceView::Named)
        } else if ast::TupleField::can_cast(syntax.kind()) {
            ast::TupleField::cast(syntax).map(FieldSourceView::Pos)
        } else {
            None
        }
    }

    fn syntax(&self) -> Node<'db> {
        match self {
            FieldSourceView::Named(it) => it.syntax(),
            FieldSourceView::Pos(it) => it.syntax(),
        }
    }
}

/// [`crate::LocalSource`] over views.
#[derive(Debug, Clone, Copy)]
pub struct LocalSourceView<'db> {
    pub local: Local<'db>,
    pub source: InFile<Either<ast::IdentPat<'db>, ast::SelfParam<'db>>>,
}

impl<'db> LocalSourceView<'db> {
    pub fn as_ident_pat(&self) -> Option<ast::IdentPat<'db>> {
        self.source.value.left()
    }

    pub fn into_ident_pat(self) -> Option<ast::IdentPat<'db>> {
        self.source.value.left()
    }

    pub fn original_file(&self, db: &Db) -> EditionedFileId {
        self.source.file_id.original_file(db)
    }

    pub fn file(&self) -> HirFileId {
        self.source.file_id
    }

    pub fn name(&self) -> Option<InFile<ast::Name<'db>>> {
        self.source.map(|it| it.name()).transpose()
    }

    pub fn syntax(&self) -> Node<'db> {
        self.source.value.syntax()
    }

    pub fn syntax_ptr(self) -> InFile<SyntaxNodePtr> {
        self.source.map(|it| ast::ptr(it.syntax()))
    }
}

impl<'db> Local<'db> {
    /// [`Local::sources`] over views.
    pub fn sources_view(self, db: &'db Db) -> Vec<LocalSourceView<'db>> {
        let b;
        let source_map = match self.parent {
            ExpressionStoreOwnerId::Signature(generic_def_id) => {
                ExpressionStore::with_source_map(db, generic_def_id.into()).1
            }
            ExpressionStoreOwnerId::Body(def_with_body_id) => {
                b = Body::with_source_map(db, def_with_body_id);
                if let Some(source) = self.self_param_view(db, &b.0, &b.1) {
                    // alloc: the `Vec` the handle `sources` returns; a local has few sources.
                    return vec![source];
                }
                &b.1.store
            }
            ExpressionStoreOwnerId::VariantFields(def) => {
                ExpressionStore::with_source_map(db, def.into()).1
            }
        };
        source_map
            .patterns_for_binding(self.binding_id)
            .iter()
            .map(|&definition| self.pat_source_view(db, source_map, definition))
            .collect() // alloc: the local's definitions, what `sources` gives
    }

    /// [`Local::primary_source`] over views.
    pub fn primary_source_view(self, db: &'db Db) -> LocalSourceView<'db> {
        let b;
        let source_map = match self.parent {
            ExpressionStoreOwnerId::Signature(generic_def_id) => {
                ExpressionStore::with_source_map(db, generic_def_id.into()).1
            }
            ExpressionStoreOwnerId::Body(def_with_body_id) => {
                b = Body::with_source_map(db, def_with_body_id);
                if let Some(source) = self.self_param_view(db, &b.0, &b.1) {
                    return source;
                }
                &b.1.store
            }
            ExpressionStoreOwnerId::VariantFields(def) => {
                ExpressionStore::with_source_map(db, def.into()).1
            }
        };
        let definition = *source_map
            .patterns_for_binding(self.binding_id)
            .first()
            .expect("a local has a definition");
        self.pat_source_view(db, source_map, definition)
    }

    /// The `self` parameter this local is, when it is one.
    fn self_param_view(
        self,
        db: &'db Db,
        body: &Body,
        source_map: &hir_def::expr_store::BodySourceMap,
    ) -> Option<LocalSourceView<'db>> {
        if !body.is_any_self_param(self.binding_id) {
            return None;
        }
        let source = source_map.self_param_syntax()?;
        let root = source.file_id.root(db);
        Some(LocalSourceView {
            local: self,
            source: source.map(|ptr| Either::Right(ptr.to_view(root))),
        })
    }

    fn pat_source_view(
        self,
        db: &'db Db,
        source_map: &hir_def::expr_store::ExpressionStoreSourceMap,
        definition: hir_def::hir::PatId,
    ) -> LocalSourceView<'db> {
        let src = source_map.pat_syntax(definition).expect("a local's pattern has syntax");
        let node = ast::to_node(src.value.syntax_node_ptr(), src.file_id.root(db));
        let pat = ast::IdentPat::cast(node)
            .unwrap_or_else(|| unreachable!("local with non ident-pattern"));
        LocalSourceView { local: self, source: InFile::new(src.file_id, Either::Left(pat)) }
    }
}

impl Module {
    /// [`Module::definition_source`] over views.
    pub fn definition_source_view(self, db: &Db) -> InFile<ModuleSourceView<'_>> {
        let def_map = self.id.def_map(db);
        def_map.module(db, self.id).definition_source_view(db)
    }

    /// [`Module::declaration_source`] over views.
    pub fn declaration_source_view(self, db: &Db) -> Option<InFile<ast::Module<'_>>> {
        let def_map = self.id.def_map(db);
        def_map.module(db, self.id).declaration_source_view(db)
    }
}

impl EnumVariant {
    /// The discriminant expression written for the variant.
    pub fn value_view(self, db: &Db) -> Option<ast::Expr<'_>> {
        self.source_view(db)?.value.const_arg()?.expr()
    }
}

impl Const {
    /// The const's written body.
    pub fn value_view(self, db: &Db) -> Option<ast::Expr<'_>> {
        self.source_view(db)?.value.body()
    }
}

impl Static {
    /// The static's written initializer.
    pub fn value_view(self, db: &Db) -> Option<ast::Expr<'_>> {
        self.source_view(db)?.value.body()
    }
}

impl<'db> Param<'db> {
    /// The pattern of a non-`self` parameter.
    pub fn pattern_source_view<'v>(self, db: &'v Db) -> Option<ast::Pat<'v>> {
        self.source_view(db).and_then(|p| p.value.right()?.pat())
    }
}

impl UseDecl {
    /// The `use` item.
    pub fn source_view(self, db: &Db) -> InFile<ast::Use<'_>> {
        loc_view(db, self.id.lookup(db))
    }
}

impl HasSourceView for Field {
    type Ast<'db> = FieldSourceView<'db>;
    fn source_view<'db>(self, db: &'db Db) -> Option<InFile<FieldSourceView<'db>>> {
        let var = VariantId::from(self.parent);
        let src = var.child_source_view(db, self.id);
        Some(src.map(|it| match it.expect("a field of a variant has a source") {
            Either::Left(it) => FieldSourceView::Pos(it),
            Either::Right(it) => FieldSourceView::Named(it),
        }))
    }
}
impl HasSourceView for Adt {
    type Ast<'db> = ast::Adt<'db>;
    fn source_view<'db>(self, db: &'db Db) -> Option<InFile<Self::Ast<'db>>> {
        match self {
            Adt::Struct(s) => Some(s.source_view(db)?.map(ast::Adt::Struct)),
            Adt::Union(u) => Some(u.source_view(db)?.map(ast::Adt::Union)),
            Adt::Enum(e) => Some(e.source_view(db)?.map(ast::Adt::Enum)),
        }
    }
}
impl HasSourceView for Variant {
    type Ast<'db> = ast::VariantDef<'db>;
    fn source_view<'db>(self, db: &'db Db) -> Option<InFile<Self::Ast<'db>>> {
        match self {
            Variant::Struct(s) => Some(s.source_view(db)?.map(ast::VariantDef::Struct)),
            Variant::Union(u) => Some(u.source_view(db)?.map(ast::VariantDef::Union)),
            Variant::EnumVariant(v) => Some(v.source_view(db)?.map(ast::VariantDef::Variant)),
        }
    }
}
impl HasSourceView for Struct {
    type Ast<'db> = ast::Struct<'db>;
    fn source_view<'db>(self, db: &'db Db) -> Option<InFile<Self::Ast<'db>>> {
        Some(loc_view(db, self.id.lookup(db)))
    }
}
impl HasSourceView for Union {
    type Ast<'db> = ast::Union<'db>;
    fn source_view<'db>(self, db: &'db Db) -> Option<InFile<Self::Ast<'db>>> {
        Some(loc_view(db, self.id.lookup(db)))
    }
}
impl HasSourceView for Enum {
    type Ast<'db> = ast::Enum<'db>;
    fn source_view<'db>(self, db: &'db Db) -> Option<InFile<Self::Ast<'db>>> {
        Some(loc_view(db, self.id.lookup(db)))
    }
}
impl HasSourceView for EnumVariant {
    type Ast<'db> = ast::Variant<'db>;
    fn source_view<'db>(self, db: &'db Db) -> Option<InFile<Self::Ast<'db>>> {
        Some(loc_view(db, self.id.lookup(db)))
    }
}
impl HasSourceView for Function {
    type Ast<'db> = ast::Fn<'db>;
    fn source_view<'db>(self, db: &'db Db) -> Option<InFile<Self::Ast<'db>>> {
        match self.id {
            AnyFunctionId::FunctionId(id) => Some(loc_view(db, id.loc(db))),
            // As `HasSource::source`: the trait method's source here, none in
            // `source_with_range_view`.
            AnyFunctionId::BuiltinDeriveImplMethod { method, impl_ } => method
                .trait_method(db, impl_)
                .and_then(|trait_method| Function::from(trait_method).source_view(db)),
        }
    }

    fn source_with_range_view<'db>(
        self,
        db: &'db Db,
    ) -> Option<InFile<(TextRange, Option<Self::Ast<'db>>)>> {
        match self.id {
            AnyFunctionId::FunctionId(id) => Some(
                loc_view(db, id.loc(db))
                    .map(|source| (source.syntax().text_range(), Some(source))),
            ),
            AnyFunctionId::BuiltinDeriveImplMethod { impl_, .. } => {
                Some(impl_.loc(db).source(db).map(|range| (range, None)))
            }
        }
    }
}
impl HasSourceView for Const {
    type Ast<'db> = ast::Const<'db>;
    fn source_view<'db>(self, db: &'db Db) -> Option<InFile<Self::Ast<'db>>> {
        Some(loc_view(db, self.id.lookup(db)))
    }
}
impl HasSourceView for Static {
    type Ast<'db> = ast::Static<'db>;
    fn source_view<'db>(self, db: &'db Db) -> Option<InFile<Self::Ast<'db>>> {
        Some(loc_view(db, self.id.lookup(db)))
    }
}
impl HasSourceView for Trait {
    type Ast<'db> = ast::Trait<'db>;
    fn source_view<'db>(self, db: &'db Db) -> Option<InFile<Self::Ast<'db>>> {
        Some(loc_view(db, self.id.lookup(db)))
    }
}
impl HasSourceView for TypeAlias {
    type Ast<'db> = ast::TypeAlias<'db>;
    fn source_view<'db>(self, db: &'db Db) -> Option<InFile<Self::Ast<'db>>> {
        Some(loc_view(db, self.id.lookup(db)))
    }
}
impl HasSourceView for Macro {
    type Ast<'db> = Either<ast::Macro<'db>, ast::Fn<'db>>;
    fn source_view<'db>(self, db: &'db Db) -> Option<InFile<Self::Ast<'db>>> {
        match self.id {
            MacroId::Macro2Id(it) => {
                Some(loc_view(db, it.lookup(db)).map(ast::Macro::MacroDef).map(Either::Left))
            }
            MacroId::MacroRulesId(it) => {
                Some(loc_view(db, it.lookup(db)).map(ast::Macro::MacroRules).map(Either::Left))
            }
            MacroId::ProcMacroId(it) => Some(loc_view(db, it.lookup(db)).map(Either::Right)),
        }
    }
}
impl HasSourceView for Impl {
    type Ast<'db> = ast::Impl<'db>;
    fn source_view<'db>(self, db: &'db Db) -> Option<InFile<Self::Ast<'db>>> {
        match self.id {
            AnyImplId::ImplId(id) => Some(loc_view(db, id.loc(db))),
            AnyImplId::BuiltinDeriveImplId(_) => None,
        }
    }

    fn source_with_range_view<'db>(
        self,
        db: &'db Db,
    ) -> Option<InFile<(TextRange, Option<Self::Ast<'db>>)>> {
        match self.id {
            AnyImplId::ImplId(id) => Some(
                loc_view(db, id.loc(db))
                    .map(|source| (source.syntax().text_range(), Some(source))),
            ),
            AnyImplId::BuiltinDeriveImplId(impl_) => {
                Some(impl_.loc(db).source(db).map(|range| (range, None)))
            }
        }
    }
}

impl HasSourceView for TypeOrConstParam {
    type Ast<'db> = Either<ast::TypeOrConstParam<'db>, ast::Trait<'db>>;
    fn source_view<'db>(self, db: &'db Db) -> Option<InFile<Self::Ast<'db>>> {
        self.id.parent.child_source_view(db, self.id.local_id).transpose()
    }
}

impl HasSourceView for LifetimeParam {
    type Ast<'db> = ast::LifetimeParam<'db>;
    fn source_view<'db>(self, db: &'db Db) -> Option<InFile<Self::Ast<'db>>> {
        self.id.parent.child_source_view(db, self.id.local_id).transpose()
    }
}

impl HasSourceView for Param<'_> {
    type Ast<'db> = Either<ast::SelfParam<'db>, ast::Param<'db>>;

    fn source_view<'db>(self, db: &'db Db) -> Option<InFile<Self::Ast<'db>>> {
        match self.func {
            Callee::Def(CallableDefId::FunctionId(func)) => {
                let InFile { file_id, value } = Function::from(func).source_view(db)?;
                let params = value.param_list()?;
                if let Some(self_param) = params.self_param() {
                    if let Some(idx) = self.idx.checked_sub(1) {
                        params.params().nth(idx).map(Either::Right)
                    } else {
                        Some(Either::Left(self_param))
                    }
                } else {
                    params.params().nth(self.idx).map(Either::Right)
                }
                .map(|value| InFile { file_id, value })
            }
            Callee::Closure(closure, _) => {
                let InternedClosure { owner, expr: expr_id, .. } = closure.loc(db);
                let (_, source_map) =
                    ExpressionStore::with_source_map(db, owner.expression_store_owner(db));
                let InFile { file_id, value } = source_map.expr_syntax(expr_id).ok()?;
                let closure: ast::ClosureExpr<'db> =
                    ptr_view(db, file_id, value.syntax_node_ptr())?;
                closure
                    .param_list()?
                    .params()
                    .nth(self.idx)
                    .map(Either::Right)
                    .map(|value| InFile { file_id, value })
            }
            _ => None,
        }
    }
}

impl HasSourceView for Closure<'_> {
    type Ast<'db> = ast::ClosureExpr<'db>;

    fn source_view<'db>(self, db: &'db Db) -> Option<InFile<Self::Ast<'db>>> {
        let InternedClosure { owner, expr, .. } = match self.id {
            AnyClosureId::ClosureId(it) => it.loc(db),
            AnyClosureId::CoroutineClosureId(it) => it.loc(db),
        };
        let (_, source_map) =
            ExpressionStore::with_source_map(db, owner.expression_store_owner(db));
        let InFile { file_id, value } = source_map.expr_syntax(expr).ok()?;
        let value = ptr_view(db, file_id, value.syntax_node_ptr())?;
        Some(InFile { file_id, value })
    }
}

impl HasSourceView for SelfParam {
    type Ast<'db> = ast::SelfParam<'db>;

    fn source_view<'db>(self, db: &'db Db) -> Option<InFile<Self::Ast<'db>>> {
        let InFile { file_id, value } = self.func.source_view(db)?;
        value
            .param_list()
            .and_then(|params| params.self_param())
            .map(|value| InFile { file_id, value })
    }
}

impl HasSourceView for Label {
    type Ast<'db> = ast::Label<'db>;

    fn source_view<'db>(self, db: &'db Db) -> Option<InFile<Self::Ast<'db>>> {
        let src = ExpressionStore::with_source_map(db, self.parent).1.label_syntax(self.label_id);
        let value = ptr_view(db, src.file_id, src.value.syntax_node_ptr())?;
        Some(InFile::new(src.file_id, value))
    }
}

impl HasSourceView for ExternCrateDecl {
    type Ast<'db> = ast::ExternCrate<'db>;

    fn source_view<'db>(self, db: &'db Db) -> Option<InFile<Self::Ast<'db>>> {
        Some(loc_view(db, self.id.lookup(db)))
    }
}

impl HasSourceView for InlineAsmOperand {
    type Ast<'db> = ast::AsmOperandNamed<'db>;
    fn source_view<'db>(self, db: &'db Db) -> Option<InFile<Self::Ast<'db>>> {
        let (_, source_map) = ExpressionStore::with_source_map(db, self.owner);
        let src = source_map.expr_syntax(self.expr).ok()?;
        let asm: ast::AsmExpr<'db> = ptr_view(db, src.file_id, src.value.syntax_node_ptr())?;
        let value = asm
            .asm_pieces()
            .filter_map(|it| match it {
                ast::AsmPiece::AsmOperandNamed(it) => Some(it),
                _ => None,
            })
            .nth(self.index)?;
        Some(InFile::new(src.file_id, value))
    }
}
