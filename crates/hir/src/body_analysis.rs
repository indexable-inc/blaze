//! [`BodyAnalysis`]: one body's types and resolutions, asked by pointers into the tree it is
//! written in ([`body_syntax`]'s, or its expansion's for a body a macro writes), so that a query
//! asking them of one body reads that tree and never the file's whole one.

#[cfg(test)]
mod tests;

use std::{cell::OnceCell, iter::once, mem, sync::Arc};

use base_db::Db;
use blaze_hash::HashMap;
use either::Either;
use hir_def::{
    AdtId, AssocItemId, BuiltinDeriveImplId, CallableDefId, DefWithBodyId, EnumVariantId,
    FunctionId, GenericDefId, HasModule as _, Lookup as _, ModuleDefId, VariantId,
    builtin_derive::BuiltinDeriveImplMethod,
    expr_store::{
        Body, BodySourceMap, ExpressionStore, ExpressionStoreSourceMap, HygieneId,
        body::{BodySyntax, body_syntax},
        path::Path,
        scope::{ExprScopes, ScopeId},
    },
    hir::{BindingId, Expr, ExprId, ExprOrPatId, Pat, generics::GenericParams},
    item_scope::ItemScope,
    nameres::MacroSubNs,
    resolver::resolver_for_scope,
    signatures::{
        ConstSignature, EnumSignature, FunctionSignature, ImplSignature, StaticSignature,
        StructSignature, TraitSignature, TypeAliasSignature, UnionSignature, VariantFields,
    },
};
use hir_expand::{
    AstId, HirFileId, InFile, MacroCallId,
    mod_path::{ModPath, PathKind},
    name::Name,
};
use hir_ty::{
    InferBodyId, InferenceResult, ParamEnvAndCrate,
    infer::closure::analysis::expr_use_visitor::moved_locals,
    next_solver::{DbInterner, EarlyBinder, GenericArgs, Ty, TyKind},
};
use rustc_type_ir::inherent::IntoKind as _;
use span::{AstIdMap, ErasedFileAstId, RealSpanMap, SyntaxContext};
use syntax::{
    AstPtr, FlatTree, SyntaxKind, SyntaxNodePtr, TextRange, ast as tag,
    view::{self as ast, AstNode as _},
};

use crate::{
    Adjust, Adjustment, AnyFunctionId, DefWithBody, Function, Local, Macro, Type,
    db::HirDatabase,
    semantics::{PathResolution, TypeInfo},
    source_analyzer::{BodyOrSig, SourceAnalyzer, resolve_hir_path},
};

/// One body's types and resolutions by pointer into [`BodyAnalysis::tree`]: what `Semantics`
/// answers of a node, read off [`Body::relative`]'s source map (for a body a macro writes,
/// [`Body::with_source_map`]'s, of its expansion) instead of the file's. The body is inferred
/// the first time an answer needs it.
pub struct BodyAnalysis<'db> {
    db: &'db Db,
    def: DefWithBodyId,
    /// The file of the tree the pointers are into: the anchor's file, or the macro file the
    /// body's item is in.
    file_id: HirFileId,
    syntax: Option<&'db BodySyntax>,
    body: &'db Body,
    source_map: &'db BodySourceMap,
    infer: OnceCell<&'db InferenceResult<'db>>,
    anchor: OnceCell<Arc<FlatTree>>,
    /// Where each macro call of the body's store is, by its call.
    calls: OnceCell<HashMap<MacroCallId, InFile<SyntaxNodePtr>>>,
    nested: OnceCell<Nested>,
    moved: OnceCell<Option<Box<[BindingId]>>>,
}

/// The items written in the body's blocks and the body's own item, by where each is, and
/// where the macro calls among them are.
#[derive(Default)]
struct Nested {
    items: HashMap<InFile<SyntaxNodePtr>, Either<GenericDefId, EnumVariantId>>,
    calls: HashMap<MacroCallId, InFile<SyntaxNodePtr>>,
}

/// The store a node is lowered in.
#[derive(Clone, Copy)]
enum Store {
    Body(DefWithBodyId),
    Signature(GenericDefId),
    Fields(VariantId),
}

impl<'db> BodyAnalysis<'db> {
    /// Reads [`body_syntax`] and [`Body::relative`] (for a body a macro writes, the expansion's
    /// tree and [`Body::with_source_map`]); never infers.
    ///
    /// # Panics
    ///
    /// For a method of a built-in derive, which has no body.
    pub fn new(db: &'db Db, def: DefWithBody) -> BodyAnalysis<'db> {
        let Ok(def) = DefWithBodyId::try_from(def) else {
            panic!("`BodyAnalysis` of {def:?}, a built-in derive's method, which has no body")
        };
        let syntax = body_syntax(db, def).as_ref();
        let ((body, source_map), file_id) = match syntax {
            Some(syntax) => {
                let relative = Body::relative(db, def).as_ref();
                (relative.expect("a body with its syntax has a relative map"), syntax.file_id)
            }
            None => (Body::with_source_map(db, def), def.erased_ast_id(db).file_id),
        };
        BodyAnalysis {
            db,
            def,
            file_id,
            syntax,
            body,
            source_map,
            infer: OnceCell::new(),
            anchor: OnceCell::new(),
            calls: OnceCell::new(),
            nested: OnceCell::new(),
            moved: OnceCell::new(),
        }
    }

    /// The tree every pointer is into, and its file: the root of [`BodySyntax::tree`] and its
    /// `file_id`, or the expansion's root and its macro file.
    pub fn tree(&self) -> (ast::Node<'_>, HirFileId) {
        let root = match self.syntax {
            Some(syntax) => ast::Node::root(self.anchor.get_or_init(|| syntax.tree(self.db))),
            None => self.file_id.root(self.db),
        };
        (root, self.file_id)
    }

    pub fn def(&self) -> DefWithBody {
        self.def.into()
    }

    /// The same body as [`Body::of`], with the ids [`BodyAnalysis::infer`] keys.
    pub fn body(&self) -> &'db Body {
        self.body
    }

    pub fn source_map(&self) -> &'db BodySourceMap {
        self.source_map
    }

    /// Infers the body the first time it is called.
    pub fn infer(&self) -> &'db InferenceResult<'db> {
        self.infer.get_or_init(|| InferenceResult::of(self.db, self.def))
    }

    pub fn type_of_expr(&self, expr: InFile<AstPtr<tag::Expr>>) -> Option<TypeInfo<'db>> {
        let id = self.expr_id(expr)?;
        let infer = self.infer();
        let adjusted = id
            .as_expr()
            .and_then(|id| infer.expr_adjustment(id))
            .and_then(|adjusts| adjusts.last())
            .map(|adjust| self.ty(adjust.target.as_ref()));
        Some(TypeInfo { original: self.ty(infer.expr_or_pat_ty(id)), adjusted })
    }

    pub fn type_of_pat(&self, pat: InFile<AstPtr<tag::Pat>>) -> Option<TypeInfo<'db>> {
        let id = self.pat_id(pat)?;
        let infer = self.infer();
        let adjusted = match id {
            ExprOrPatId::ExprId(id) => infer
                .expr_adjustment(id)
                .and_then(|adjusts| adjusts.last())
                .map(|adjust| adjust.target.as_ref()),
            ExprOrPatId::PatId(id) => infer
                .pat_adjustment(id)
                .and_then(|adjusts| adjusts.last())
                .map(|adjust| adjust.source.as_ref()),
        };
        let adjusted = adjusted.map(|it| self.ty(it));
        Some(TypeInfo { original: self.ty(infer.expr_or_pat_ty(id)), adjusted })
    }

    /// `Semantics::expr_adjustments`: each adjustment of `expr`, from the type before it.
    pub fn expr_adjustments(
        &self,
        expr: InFile<AstPtr<tag::Expr>>,
    ) -> Option<Vec<Adjustment<'db>>> {
        let id = self.expr_id(expr)?;
        let infer = self.infer();
        let adjusts = infer.expr_adjustment(id.as_expr()?)?;
        let mut source = self.ty(infer.expr_or_pat_ty(id));
        let adjustment = |adjust: &hir_ty::Adjustment| {
            let target = self.ty(adjust.target.as_ref());
            let kind = Adjust::from(adjust.kind);
            // clone: the next adjustment's source, as `Semantics::expr_adjustments` does
            let source = mem::replace(&mut source, target.clone());
            Adjustment { source, target, kind }
        };
        // alloc: the answer, what `Semantics::expr_adjustments` gives
        Some(adjusts.iter().map(adjustment).collect())
    }

    pub fn resolve_method_call(
        &self,
        call: InFile<AstPtr<tag::MethodCallExpr>>,
    ) -> Option<Function> {
        let id = self.expr_id(call.map(AstPtr::upcast))?.as_expr()?;
        let (func, args) = self.infer().method_resolution(id)?;
        Some(match self.impl_method(func, args).0 {
            Either::Left(func) => func.into(),
            Either::Right((impl_, method)) => {
                Function { id: AnyFunctionId::BuiltinDeriveImplMethod { method, impl_ } }
            }
        })
    }

    /// `Semantics::resolve_path`, in the scope of the block the path is in.
    pub fn resolve_path(&self, path: InFile<AstPtr<tag::Path>>) -> Option<PathResolution<'db>> {
        let node = self.node(path.map(|it| it.syntax_node_ptr()))?;
        let analyzer = self.analyzer(path.file_id, node, true)?;
        analyzer.resolve_path_view(self.db, ast::Path::cast(node)?).map(|(it, _)| it)
    }

    pub fn binding(&self, pat: InFile<AstPtr<tag::IdentPat>>) -> Option<Local<'db>> {
        let id = self.pat_id(pat.map(AstPtr::upcast))?.as_pat()?;
        match self.body.store.pat(id) {
            Pat::Bind { id, .. } => Some(Local::from((self.def, id))),
            _ => None,
        }
    }

    /// The callee of a call or method call: the impl's function when the arguments say which
    /// (`lookup_impl_method`), else the trait's.
    pub fn callee(&self, expr: ExprId) -> Option<(FunctionId, GenericArgs<'db>)> {
        let infer = self.infer();
        let (func, args) = match self.body.store.expr(expr) {
            Expr::MethodCall { .. } => infer.method_resolution(expr)?,
            Expr::Call { callee, .. } => match infer.type_of_expr_or_pat(callee.into())?.kind() {
                TyKind::FnDef(def, args) => match def.0 {
                    CallableDefId::FunctionId(func) => (func, args.skip_binder()),
                    CallableDefId::StructId(_) | CallableDefId::EnumVariantId(_) => return None,
                },
                _ => return None,
            },
            _ => return None,
        };
        Some(match self.impl_method(func, args) {
            (Either::Left(func), args) => (func, args),
            (Either::Right(_), _) => (func, args),
        })
    }

    /// `ty` with the body's environment, for [`Type`]'s methods.
    pub fn ty(&self, ty: Ty<'db>) -> Type<'db> {
        let owner = self.def.generic_def(self.db).into();
        Type { owner, ty: EarlyBinder::bind(DbInterner::new_no_crate(self.db), ty) }
    }

    /// The expansion of a macro call of the body's store.
    pub fn expansion(&self, call: InFile<AstPtr<tag::MacroCall>>) -> Option<MacroCallId> {
        let node = ast::MacroCall::cast(self.node(call.map(|it| it.syntax_node_ptr()))?)?;
        self.source_map.store.expansion_view(InFile::new(call.file_id, node))
    }

    /// Where `expr` is written in [`BodyAnalysis::tree`]: its own range, or that of the
    /// outermost macro call of the tree that wrote it; `None` for what a macro made up with no
    /// call in the tree.
    pub fn expr_range(&self, expr: ExprId) -> Option<TextRange> {
        let InFile { mut file_id, value } = self.source_map.store.expr_syntax(expr).ok()?;
        let mut range = value.syntax_node_ptr().text_range();
        while file_id != self.file_id {
            let HirFileId::MacroFile(call) = file_id else { return None };
            let at = self.call_site(call)?;
            file_id = at.file_id;
            range = at.value.text_range();
        }
        Some(range)
    }

    /// A written type lowered in its scope (`Semantics::resolve_type`), with no inference.
    pub fn resolve_type(&self, ty: InFile<AstPtr<tag::Type>>) -> Option<Type<'db>> {
        let node = self.node(ty.map(|it| it.syntax_node_ptr()))?;
        let analyzer = self.analyzer(ty.file_id, node, false)?;
        analyzer.type_of_type_view(self.db, ast::Type::cast(node)?)
    }

    /// The macro a call calls (`Semantics::resolve_macro_call`), with no inference.
    pub fn resolve_macro_call(&self, call: InFile<AstPtr<tag::MacroCall>>) -> Option<Macro> {
        let db = self.db;
        let node = self.node(call.map(|it| it.syntax_node_ptr()))?;
        let path = ast::MacroCall::cast(node)?.path()?;
        let analyzer = self.analyzer(call.file_id, node, false)?;
        let path = match call.file_id {
            HirFileId::MacroFile(call) => {
                let map = call.expansion_span_map(db);
                ModPath::from_src_view(db, path, &mut |range| map.span_at(range.start()).ctx)?
            }
            HirFileId::FileId(file) => {
                let root = SyntaxContext::root(file.edition(db));
                ModPath::from_src_view(db, path, &mut |_| root)?
            }
        };
        let found = analyzer.resolver.resolve_path_as_macro(db, &path, Some(MacroSubNs::Bang))?;
        Some(Macro { id: found.def })
    }

    /// `name` as a path of one segment in the scope at `at`
    /// (`SemanticsScope::speculative_resolve`), with no inference.
    pub fn resolve_name(
        &self,
        at: InFile<SyntaxNodePtr>,
        name: &Name,
    ) -> Option<PathResolution<'db>> {
        let node = self.node(at)?;
        let analyzer = self.analyzer(at.file_id, node, false)?;
        // alloc: an interned `Name`'s count and a one-segment path, as `speculative_resolve`
        let path = ModPath::from_segments(PathKind::Plain, once(name.clone()));
        resolve_hir_path(
            self.db,
            &analyzer.resolver,
            analyzer.infer_body,
            Path::BarePath(&path),
            HygieneId::ROOT,
            None,
        )
    }

    /// Whether the body moves out of a place whose base is `local`: rustc's
    /// `ExprUseVisitor::for_clippy(..).consume_body(body)` with clippy's `MovedVariablesCtxt`
    /// ([`moved_locals`]). `true` for a body whose walk stops at a type error, which rustc lints
    /// nothing of; `false` for a local of another body, which this one cannot move.
    pub fn consumes(&self, local: Local<'db>) -> bool {
        if local.parent_infer != InferBodyId::from(self.def) {
            return false;
        }
        let moved = self.moved.get_or_init(|| moved_locals(self.db, self.def, self.infer()));
        let Some(moved) = moved else { return true };
        let key = |it: &BindingId| it.into_raw().into_u32();
        moved.binary_search_by_key(&key(&local.binding_id), key).is_ok()
    }

    fn expr_id(&self, expr: InFile<AstPtr<tag::Expr>>) -> Option<ExprOrPatId> {
        let node = ast::Expr::cast(self.node(expr.map(|it| it.syntax_node_ptr()))?)?;
        self.source_map.store.node_expr_view(InFile::new(expr.file_id, node))
    }

    fn pat_id(&self, pat: InFile<AstPtr<tag::Pat>>) -> Option<ExprOrPatId> {
        let node = ast::Pat::cast(self.node(pat.map(|it| it.syntax_node_ptr()))?)?;
        self.source_map.store.node_pat_view(InFile::new(pat.file_id, node))
    }

    /// `lookup_impl_method` of `func` in the body's environment.
    fn impl_method(
        &self,
        func: FunctionId,
        args: GenericArgs<'db>,
    ) -> (Either<FunctionId, (BuiltinDeriveImplId, BuiltinDeriveImplMethod)>, GenericArgs<'db>) {
        let db = self.db;
        let param_env = db.trait_environment(self.def.generic_def(db));
        db.lookup_impl_method(ParamEnvAndCrate { param_env, krate: self.def.krate(db) }, func, args)
    }

    /// The node `ptr` points at, in the body's tree or an expansion.
    fn node(&self, InFile { file_id, value }: InFile<SyntaxNodePtr>) -> Option<ast::Node<'_>> {
        let root = match (file_id, self.syntax) {
            (HirFileId::MacroFile(_), _) => file_id.root(self.db),
            (HirFileId::FileId(_), _) if file_id != self.file_id => return None,
            (HirFileId::FileId(_), Some(syntax)) => {
                ast::Node::root(self.anchor.get_or_init(|| syntax.tree(self.db)))
            }
            (HirFileId::FileId(_), None) => file_id.root(self.db),
        };
        ast::try_to_node(value, root)
    }

    /// Where the call of the expansion `call` is: of the body's store, or among its items.
    fn call_site(&self, call: MacroCallId) -> Option<InFile<SyntaxNodePtr>> {
        let calls = self.calls.get_or_init(|| {
            let calls = self.source_map.store.expansions();
            // alloc: once per body whose expansions are climbed out of
            calls.map(|(at, &call)| (call, at.map(|it| it.syntax_node_ptr()))).collect()
        });
        calls.get(&call).or_else(|| self.nested().calls.get(&call)).copied()
    }

    /// The analyzer of the store `node` is lowered in, at the scope of the block it is in.
    fn analyzer<'t>(
        &'t self,
        file: HirFileId,
        node: ast::Node<'t>,
        infer: bool,
    ) -> Option<SourceAnalyzer<'db>> {
        let db = self.db;
        Some(match self.store_of(file, node)? {
            Store::Body(def) => {
                let (body, source_map) = match def == self.def {
                    true => (self.body, self.source_map),
                    false => Body::unmoved(db, def),
                };
                let scope = self.scope_in(&source_map.store, ExprScopes::of(db, def), file, node);
                let infer = infer.then(|| match def == self.def {
                    true => self.infer(),
                    false => InferenceResult::of(db, def),
                });
                SourceAnalyzer {
                    resolver: resolver_for_scope(db, def, scope),
                    body_or_sig: Some(BodyOrSig::Body { def, body, source_map, infer }),
                    file_id: file,
                    type_owner: def.generic_def(db).into(),
                    infer_body: Some(def.into()),
                    maps: self.maps(file),
                }
            }
            Store::Signature(def) => {
                let (store, source_map) = signature_maps(db, def);
                let scope = self.scope_in(source_map, ExprScopes::of(db, def), file, node);
                let generics = GenericParams::of(db, def);
                let sig = BodyOrSig::Sig { def, store, source_map, generics, infer: None };
                SourceAnalyzer {
                    resolver: resolver_for_scope(db, def, scope),
                    body_or_sig: Some(sig),
                    file_id: file,
                    type_owner: def.into(),
                    infer_body: None,
                    maps: self.maps(file),
                }
            }
            Store::Fields(def) => {
                let (store, source_map) = match VariantFields::relative(db, def) {
                    Some((fields, source_map)) => (&fields.store, source_map),
                    None => {
                        let (fields, source_map) = def.fields_with_source_map(db);
                        (&fields.store, source_map)
                    }
                };
                let scope = self.scope_in(source_map, ExprScopes::of(db, def), file, node);
                let fields = BodyOrSig::VariantFields { def, store, source_map, infer: None };
                SourceAnalyzer {
                    resolver: resolver_for_scope(db, def, scope),
                    body_or_sig: Some(fields),
                    file_id: file,
                    type_owner: GenericDefId::from(def.adt_id(db)).into(),
                    infer_body: None,
                    maps: self.maps(file),
                }
            }
        })
    }

    /// The maps a path of `file` read from this analysis' tree is lowered again with: the
    /// anchor's ([`BodySyntax::span_map`], [`BodySyntax::ast_id_map`]) for the body's own file,
    /// whose pointers are into the anchor's tree, and the file's own for a macro file.
    fn maps(&self, file: HirFileId) -> Option<(&'db RealSpanMap, &'db AstIdMap)> {
        match self.syntax {
            Some(syntax) if file == self.file_id && !file.is_macro() => {
                Some((&*syntax.span_map, &*syntax.ast_id_map))
            }
            _ => None,
        }
    }

    /// The store `node` is lowered in: of the innermost item around it, of the body's own
    /// item, or of an item nested in the body, climbing out of expansions.
    fn store_of<'t>(&'t self, mut file: HirFileId, mut node: ast::Node<'t>) -> Option<Store> {
        let db = self.db;
        let owner = self.owner();
        loop {
            let mut in_fields = false;
            let mut child: Option<ast::Node<'t>> = None;
            for it in node.ancestors() {
                let kind = it.kind();
                in_fields |=
                    matches!(kind, SyntaxKind::RECORD_FIELD_LIST | SyntaxKind::TUPLE_FIELD_LIST);
                if !ast::Item::can_cast(kind) && kind != SyntaxKind::VARIANT {
                    child = Some(it);
                    continue;
                }
                let at = InFile::new(file, ast::ptr(it));
                let def = match owner {
                    Some((ptr, def)) if ptr == at => Some(def),
                    _ => self.nested().items.get(&at).copied(),
                };
                if let Some(def) = def {
                    return Some(store(db, def, it, child, in_fields));
                }
                child = Some(it);
            }
            let HirFileId::MacroFile(call) = file else { return None };
            let at = self.call_site(call)?;
            file = at.file_id;
            node = self.node(at)?;
        }
    }

    /// `scope_for_view`: the scope of the innermost expression of `source_map` above `node`,
    /// climbing out of expansions, stopping at the item around it.
    fn scope_in<'t>(
        &'t self,
        source_map: &ExpressionStoreSourceMap,
        scopes: &ExprScopes,
        mut file: HirFileId,
        mut node: ast::Node<'t>,
    ) -> Option<ScopeId> {
        loop {
            for it in node.ancestors() {
                let kind = it.kind();
                if ast::Item::can_cast(kind)
                    && !ast::MacroCall::can_cast(kind)
                    && !ast::Use::can_cast(kind)
                    && !ast::AsmExpr::can_cast(kind)
                {
                    return None;
                }
                let Some(expr) = ast::Expr::cast(it) else { continue };
                let expr = source_map.node_expr_view(InFile::new(file, expr));
                if let Some(scope) = expr.and_then(|it| scopes.scope_for(it.as_expr()?)) {
                    return Some(scope);
                }
            }
            let HirFileId::MacroFile(call) = file else { return None };
            let at = source_map
                .expansions()
                .find(|&(_, &it)| it == call)
                .map(|(at, _)| at.map(|it| it.syntax_node_ptr()))
                .or_else(|| self.call_site(call))?;
            file = at.file_id;
            node = self.node(at)?;
        }
    }

    /// Where the body's own item is, and what it is.
    fn owner(&self) -> Option<(InFile<SyntaxNodePtr>, Either<GenericDefId, EnumVariantId>)> {
        let def = match self.def {
            DefWithBodyId::FunctionId(it) => Either::Left(it.into()),
            DefWithBodyId::ConstId(it) => Either::Left(it.into()),
            DefWithBodyId::StaticId(it) => Either::Left(it.into()),
            DefWithBodyId::VariantId(it) => Either::Right(it),
        };
        let at = match self.syntax {
            Some(syntax) => InFile::new(self.file_id, syntax.item),
            None => {
                let id = self.def.erased_ast_id(self.db);
                self.key(id.file_id, id.value)?
            }
        };
        Some((at, def))
    }

    /// The items of the body's blocks, the variants of their enums and the items of their
    /// traits and impls, and the macro calls among them.
    fn nested(&self) -> &Nested {
        self.nested.get_or_init(|| {
            let db = self.db;
            let mut nested = Nested::default();
            for (_, block) in self.body.blocks(db) {
                for (_, module) in block.modules(db) {
                    self.index(&mut nested, module.scope(db));
                }
            }
            nested
        })
    }

    fn index(&self, nested: &mut Nested, scope: &ItemScope) {
        let db = self.db;
        let item = |nested: &mut Nested, id: InFile<ErasedFileAstId>, def| {
            if let Some(at) = self.key(id.file_id, id.value) {
                nested.items.insert(at, def);
            }
        };
        let call = |nested: &mut Nested, id: InFile<ErasedFileAstId>, call| {
            if let Some(at) = self.key(id.file_id, id.value) {
                nested.calls.insert(call, at);
            }
        };
        let assoc = |nested: &mut Nested,
                         items: &[(Name, AssocItemId)],
                         calls: &[(AstId<tag::Item>, MacroCallId)]| {
            for &(_, it) in items {
                let (id, def) = match it {
                    AssocItemId::FunctionId(it) => (erased(it.lookup(db).id), it.into()),
                    AssocItemId::ConstId(it) => (erased(it.lookup(db).id), it.into()),
                    AssocItemId::TypeAliasId(it) => (erased(it.lookup(db).id), it.into()),
                };
                item(nested, id, Either::Left(def));
            }
            for &(at, it) in calls {
                call(nested, erased(at), it);
            }
        };
        for it in scope.declarations() {
            let (id, def) = match it {
                ModuleDefId::FunctionId(it) => (erased(it.lookup(db).id), it.into()),
                ModuleDefId::AdtId(AdtId::StructId(it)) => (erased(it.lookup(db).id), it.into()),
                ModuleDefId::AdtId(AdtId::UnionId(it)) => (erased(it.lookup(db).id), it.into()),
                ModuleDefId::AdtId(AdtId::EnumId(it)) => {
                    for &(variant, _) in it.enum_variants(db).variants.values() {
                        item(nested, erased(variant.lookup(db).id), Either::Right(variant));
                    }
                    (erased(it.lookup(db).id), it.into())
                }
                ModuleDefId::ConstId(it) => (erased(it.lookup(db).id), it.into()),
                ModuleDefId::StaticId(it) => (erased(it.lookup(db).id), it.into()),
                ModuleDefId::TraitId(it) => {
                    let items = it.trait_items(db);
                    assoc(nested, &items.items, &items.macro_calls);
                    (erased(it.lookup(db).id), it.into())
                }
                ModuleDefId::TypeAliasId(it) => (erased(it.lookup(db).id), it.into()),
                _ => continue,
            };
            item(nested, id, Either::Left(def));
        }
        for it in scope.unnamed_consts() {
            item(nested, erased(it.lookup(db).id), Either::Left(it.into()));
        }
        for it in scope.impls() {
            let items = it.impl_items(db);
            assoc(nested, &items.items, &items.macro_calls);
            item(nested, erased(it.lookup(db).id), Either::Left(it.into()));
        }
        for (&at, &it) in scope.iter_macro_invoc() {
            call(nested, erased(at), it);
        }
        for (at, it) in scope.attr_macro_invocs() {
            call(nested, erased(at), it);
        }
        for (at, derives) in scope.derive_macro_invocs() {
            let at = InFile::new(at.file_id, at.value.erase());
            for (_, attr_call, each) in derives {
                call(nested, at, attr_call);
                for it in each.iter().filter_map(|it| it.and_then(|it| it.left())) {
                    call(nested, at, it);
                }
            }
        }
    }

    /// Where `id` of `file` is in a tree [`BodyAnalysis::node`] reads: the anchor's for the
    /// body's own file, an expansion's for a macro file.
    fn key(&self, file: HirFileId, id: ErasedFileAstId) -> Option<InFile<SyntaxNodePtr>> {
        let ptr = match (file, self.syntax) {
            (HirFileId::MacroFile(_), _) => file.ast_id_map(self.db).try_get_erased(id)?,
            (HirFileId::FileId(_), _) if file != self.file_id => return None,
            (HirFileId::FileId(_), Some(syntax)) => syntax.ast_id_map.try_get_erased(id)?,
            (HirFileId::FileId(_), None) => file.ast_id_map(self.db).try_get_erased(id)?,
        };
        Some(InFile::new(file, ptr))
    }
}

/// The store of `def`, found at `item` with `child` the node under it the walk came from.
fn store(
    db: &Db,
    def: Either<GenericDefId, EnumVariantId>,
    item: ast::Node<'_>,
    child: Option<ast::Node<'_>>,
    in_fields: bool,
) -> Store {
    let is = |body: Option<ast::Node<'_>>| {
        body.zip(child).is_some_and(|(body, child)| ast::ptr(body) == ast::ptr(child))
    };
    match def {
        Either::Right(variant) if in_fields => Store::Fields(variant.into()),
        Either::Right(variant)
            if is(ast::Variant::cast(item).and_then(|it| it.const_arg()).map(|it| it.syntax())) =>
        {
            Store::Body(variant.into())
        }
        Either::Right(variant) => {
            Store::Signature(GenericDefId::AdtId(variant.lookup(db).parent.into()))
        }
        Either::Left(GenericDefId::AdtId(AdtId::StructId(it))) if in_fields => {
            Store::Fields(it.into())
        }
        Either::Left(GenericDefId::AdtId(AdtId::UnionId(it))) if in_fields => {
            Store::Fields(it.into())
        }
        Either::Left(GenericDefId::FunctionId(it))
            if is(ast::Fn::cast(item).and_then(|it| it.body()).map(|it| it.syntax())) =>
        {
            Store::Body(it.into())
        }
        Either::Left(GenericDefId::ConstId(it))
            if is(ast::Const::cast(item).and_then(|it| it.body()).map(|it| it.syntax())) =>
        {
            Store::Body(it.into())
        }
        Either::Left(GenericDefId::StaticId(it))
            if is(ast::Static::cast(item).and_then(|it| it.body()).map(|it| it.syntax())) =>
        {
            Store::Body(it.into())
        }
        Either::Left(def) => Store::Signature(def),
    }
}

/// [`Body::unmoved`] of a signature (a body's store and the source map of the tree it is
/// written in: the anchor's for one written in a file, its expansion's for one a macro
/// writes; an item nested in a body is anchored where the body is, so the two share the
/// anchor's coordinates): [`GenericParams::with_source_map`] over the signature's
/// relative map.
fn signature_maps(db: &Db, def: GenericDefId) -> (&ExpressionStore, &ExpressionStoreSourceMap) {
    macro_rules! maps {
        ($Sig:ident, $id:expr) => {
            match $Sig::relative(db, $id) {
                Some((sig, source_map)) => (&sig.store, source_map),
                None => {
                    let (sig, source_map) = $Sig::with_source_map(db, $id);
                    (&sig.store, source_map)
                }
            }
        };
    }
    match def {
        GenericDefId::AdtId(AdtId::EnumId(id)) => maps!(EnumSignature, id),
        GenericDefId::AdtId(AdtId::StructId(id)) => maps!(StructSignature, id),
        GenericDefId::AdtId(AdtId::UnionId(id)) => maps!(UnionSignature, id),
        GenericDefId::ConstId(id) => maps!(ConstSignature, id),
        GenericDefId::FunctionId(id) => maps!(FunctionSignature, id),
        GenericDefId::ImplId(id) => maps!(ImplSignature, id),
        GenericDefId::StaticId(id) => maps!(StaticSignature, id),
        GenericDefId::TraitId(id) => maps!(TraitSignature, id),
        GenericDefId::TypeAliasId(id) => maps!(TypeAliasSignature, id),
    }
}

fn erased<N: span::AstIdNode>(id: AstId<N>) -> InFile<ErasedFileAstId> {
    InFile::new(id.file_id, id.value.erase())
}
