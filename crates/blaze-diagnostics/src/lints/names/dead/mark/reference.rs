//! The reference the marks are checked against (`the_marks_equal_the_whole_pass`): the walk
//! of a signature, a `use` and an enum's discriminants over their syntax, as rustc's
//! `intravisit` walks its HIR, macro calls expanded and names resolved through
//! `hir::view::Semantics`, and of a body with the walk that marked as it went ([`body`]). This
//! is the pass as it was before its marks were recorded per store ([`super::body`]) and
//! replayed; it is kept only for the test, and reads syntax everywhere the records read none.
//!
//! Follows `compiler/rustc_passes/src/dead.rs` (`MarkSymbolVisitor`) (MIT or Apache-2.0).

mod body;

use either::Either;
use hir::{HirFileId, InFile, ModuleDef, PathResolution};
use hir_def::{
    AdtId, DefWithBodyId, ExpressionStoreOwnerId, FieldId,
    expr_store::{Body, ExpressionStore},
};
use syntax::{
    SyntaxKind,
    view::{self as ast, AstNode as _, BinaryOp, HasArgList as _, RangeItem as _},
};

use super::{
    super::{
        defs::{Anon, DefIdx, Id, Kind, reference::is_anon},
        res::Res,
    },
    Mark,
};
use crate::{
    lower::{Found, Phase},
    model::Diag,
};

impl<'db> Mark<'_, '_, '_, 'db> {
    /// The walk of the definition `def` as the pass did it before its marks were recorded:
    /// its signature over its syntax, then its body ([`body::Walk`]).
    pub(super) fn reference_walk(&mut self, def: DefIdx) {
        let k = self.k;
        if k.defs[def].kind == Kind::Enum {
            self.discriminants(def);
        }
        let Some(ptr) = k.defs.node(k.cx, def) else { return };
        let node = k.cx.syntax_view(ptr);
        if let Some(owner) = self.signature(node.file_id, node.value, k.defs[def].id) {
            let (body, source_map) = Body::with_source_map(k.cx.db, owner);
            let body: &Body = body;
            body::Walk::new(self, owner, body, source_map).run(body);
        }
    }

    /// The walk of a definition's signature, over its syntax. The body of a function, a
    /// constant or a static, when one is written, is left out and its owner given, for the
    /// body's walk ([`body::Walk`]).
    fn signature(
        &mut self,
        file: HirFileId,
        node: ast::Node<'db>,
        id: Id,
    ) -> Option<DefWithBodyId> {
        let written = |kind: fn(SyntaxKind) -> bool| node.children().find(|it| kind(it.kind()));
        let found = match id {
            Id::Fn(it) => written(|kind| kind == SyntaxKind::BLOCK_EXPR)
                .map(|body| (DefWithBodyId::FunctionId(it), body)),
            Id::Const(it) => {
                written(ast::Expr::can_cast).map(|body| (DefWithBodyId::ConstId(it), body))
            }
            Id::Static(it) => {
                written(ast::Expr::can_cast).map(|body| (DefWithBodyId::StaticId(it), body))
            }
            _ => None,
        };
        let Some((owner, body)) = found else {
            self.walk(file, node, true);
            return None;
        };
        for child in node.children() {
            if child == body {
                continue;
            }
            if child.kind() != SyntaxKind::PARAM_LIST {
                stdx::ensure_sufficient_stack(|| self.walk(file, child, false));
                continue;
            }
            for param in child.children() {
                if param.kind() != SyntaxKind::PARAM {
                    self.walk(file, param, false);
                    continue;
                }
                for part in param.children() {
                    if !ast::Pat::can_cast(part.kind()) {
                        stdx::ensure_sufficient_stack(|| self.walk(file, part, false));
                    }
                }
            }
        }
        Some(owner)
    }

    /// The discriminants of the enum `def`, walked over its syntax with it: each an anonymous
    /// constant, visited (and so the parent of the items in it live) by the walk.
    fn discriminants(&mut self, def: DefIdx) {
        let k = self.k;
        if !k.defs.in_anon {
            return;
        }
        let db = k.cx.db;
        let Id::Adt(AdtId::EnumId(it)) = k.defs[def].id else { return };
        let variants = it.enum_variants(db);
        for &(variant, _) in variants.variants.values() {
            if variants.has_explicit_discriminant(variant) {
                let owner = DefWithBodyId::VariantId(variant);
                let root = Body::of(db, owner).root_expr();
                self.live.anon.insert((ExpressionStoreOwnerId::Body(owner), root));
            }
        }
    }

    /// rustc's `intravisit` walk of `node` with the visitor's methods: the items in it are
    /// definitions of their own and not walked, nor attributes; macro calls are walked as what
    /// they expand to.
    fn walk(&mut self, file: HirFileId, node: ast::Node<'db>, root: bool) {
        let kind = node.kind();
        match kind {
            SyntaxKind::ATTR => return,
            SyntaxKind::MACRO_CALL => {
                let in_item_list = node.parent().is_some_and(|it| {
                    matches!(
                        it.kind(),
                        SyntaxKind::SOURCE_FILE
                            | SyntaxKind::ITEM_LIST
                            | SyntaxKind::ASSOC_ITEM_LIST
                            | SyntaxKind::EXTERN_ITEM_LIST
                            | SyntaxKind::MACRO_ITEMS
                    )
                });
                if in_item_list && !root {
                    return;
                }
                if let Some(call) = ast::MacroCall::cast(node)
                    && let Some(expansion) = self.k.cx.vsema.expand_macro_call(call)
                {
                    stdx::ensure_sufficient_stack(|| {
                        self.walk(expansion.file_id, expansion.value, false)
                    });
                }
                return;
            }
            _ if !root && ast::Item::can_cast(kind) => return,
            _ => {}
        }
        if is_anon(node) {
            self.anon_syntax(file, node);
            return;
        }
        if ast::Pat::can_cast(kind) {
            self.pat(file, node);
            return;
        }
        match kind {
            SyntaxKind::PATH => {
                if let Some(path) = ast::Path::cast(node) {
                    self.path(path);
                }
            }
            SyntaxKind::METHOD_CALL_EXPR => {
                if let Some(call) = ast::MethodCallExpr::cast(node)
                    && let Some(function) = self.k.cx.vsema.resolve_method_call(call)
                {
                    self.handle_res(Res::function(self.k.defs, function));
                }
            }
            SyntaxKind::FIELD_EXPR => {
                if let Some(field) = ast::FieldExpr::cast(node) {
                    self.field_read(field);
                }
            }
            SyntaxKind::RECORD_EXPR => {
                if let Some(expr) = ast::RecordExpr::cast(node) {
                    self.union_fields(expr);
                }
            }
            SyntaxKind::OFFSET_OF_EXPR => {
                if let Some(expr) = ast::OffsetOfExpr::cast(node) {
                    for name in expr.fields() {
                        if let Some((Either::Right(field), _)) =
                            self.k.cx.vsema.resolve_offset_of_field(name)
                            && let Some(field) = self.k.defs.get(Id::Field(FieldId::from(field)))
                        {
                            self.insert_def_id(field);
                        }
                    }
                }
            }
            SyntaxKind::BIN_EXPR => {
                if let Some(expr) = ast::BinExpr::cast(node)
                    && let Some(BinaryOp::Assignment { op: None }) = expr.op_kind()
                {
                    self.assign(file, expr);
                    return;
                }
            }
            SyntaxKind::MATCH_ARM => {
                let len = self.ignore_variants.len();
                if let Some(pat) = ast::MatchArm::cast(node).and_then(|it| it.pat()) {
                    let mut stack = std::mem::take(&mut self.ignore_variants);
                    self.necessary_variants(pat.syntax(), &mut stack, len);
                    self.ignore_variants = stack;
                }
                self.children(file, node);
                self.ignore_variants.truncate(len);
                return;
            }
            _ => {}
        }
        self.children(file, node);
    }

    fn children(&mut self, file: HirFileId, node: ast::Node<'db>) {
        for child in node.children() {
            stdx::ensure_sufficient_stack(|| self.walk(file, child, false));
        }
    }

    /// A closure or an anonymous constant of a signature, walked out of any pattern it is in:
    /// visited (unless it is a closure with an `allow` of its own that what reached it came
    /// with), by the expression the store of the definition being walked lowered it to, the
    /// key the numbering gives the items in it.
    fn anon_syntax(&mut self, file: HirFileId, node: ast::Node<'db>) {
        let k = self.k;
        if k.defs.in_anon
            && let Some(expr) = ast::Expr::cast(node)
        {
            let own = node.kind() == SyntaxKind::CLOSURE_EXPR
                && self.propagated
                && k.levels.at(k.cx, InFile::new(file, node)).is_allow_or_expect();
            if !own && let Some(key) = self.anon_of(InFile::new(file, expr)) {
                self.live.anon.insert(key);
            }
        }
        let in_pat = std::mem::replace(&mut self.in_pat, false);
        self.children(file, node);
        self.in_pat = in_pat;
    }

    /// The store (of the definition being walked) and expression `at` is lowered to.
    fn anon_of(&self, at: InFile<ast::Expr<'db>>) -> Option<Anon> {
        let db = self.k.cx.db;
        let mut found = None;
        super::super::defs::stores(db, self.k.defs[self.current].id, |owner| {
            if found.is_none() {
                let (_, source_map) = ExpressionStore::with_source_map(db, owner);
                found = source_map.node_expr_view(at).and_then(|it| it.as_expr()).map(|it| (owner, it));
            }
        });
        found
    }

    /// rustc's `visit_path` (and `visit_expr` of a type-relative path): what `path` resolves
    /// to is live. Gives what it resolved to, for the pattern that has it.
    fn path(&mut self, path: ast::Path<'db>) -> Res {
        let cx = self.k.cx;
        let parent = path.syntax().parent().map(|it| it.kind());
        if matches!(
            parent,
            Some(
                SyntaxKind::MACRO_CALL
                    | SyntaxKind::PATH_META
                    | SyntaxKind::KEY_VALUE_META
                    | SyntaxKind::TOKEN_TREE_META
            )
        ) {
            return Res::None;
        }
        let Some(res) = cx.vsema.resolve_path(path) else { return Res::None };
        let value_or_pat = matches!(
            parent,
            Some(SyntaxKind::PATH_EXPR | SyntaxKind::PATH_PAT | SyntaxKind::TUPLE_STRUCT_PAT)
        );
        let self_ctor = value_or_pat && path.qualifier().is_none() && is_self(path);
        let res = Res::path(cx, self.k.defs, res, self_ctor);
        self.handle_res(res);
        res
    }

    /// rustc's `handle_field_access`: a field of a struct or a union read.
    fn field_read(&mut self, field: ast::FieldExpr<'db>) {
        if let Some(Either::Left(field)) = self.k.cx.vsema.resolve_field(field)
            && let Some(field) = self.k.defs.get(Id::Field(FieldId::from(field)))
        {
            self.insert_def_id(field);
        }
    }

    /// rustc's `mark_as_used_if_union`: the fields a union expression names, of a union of
    /// more than one field, are read. A literal naming no field is no union's, and is not typed.
    fn union_fields(&mut self, expr: ast::RecordExpr<'db>) {
        let cx = self.k.cx;
        let Some(list) = expr.record_expr_field_list() else { return };
        if list.fields().next().is_none() {
            return;
        }
        let Some(ty) = cx.vsema.type_of_expr(ast::Expr::RecordExpr(expr)) else { return };
        let Some(hir::Adt::Union(union)) = ty.original.as_adt() else { return };
        if union.fields(cx.db).len() <= 1 {
            return;
        }
        for field in list.fields() {
            if let Some((field, _, _)) = cx.vsema.resolve_record_field(field)
                && let Some(field) = self.k.defs.get(Id::Field(FieldId::from(field)))
            {
                self.insert_def_id(field);
            }
        }
    }

    /// rustc's `visit_pat`: what a pattern's paths name, its ADT, and the fields it binds.
    fn pat(&mut self, file: HirFileId, node: ast::Node<'db>) {
        let cx = self.k.cx;
        self.in_pat = true;
        let Some(pat) = ast::Pat::cast(node) else {
            self.children(file, node);
            self.in_pat = false;
            return;
        };
        match pat {
            ast::Pat::RecordPat(it) => {
                self.pat_adt(pat);
                if let Some(path) = it.path() {
                    self.path(path);
                }
                if let Some(list) = it.record_pat_field_list() {
                    for field in list.fields() {
                        if matches!(field.pat(), Some(ast::Pat::WildcardPat(_))) {
                            continue;
                        }
                        if let Some((field, _)) = cx.vsema.resolve_record_pat_field(field)
                            && let Some(field) = self.k.defs.get(Id::Field(FieldId::from(field)))
                        {
                            self.insert_def_id(field);
                        }
                    }
                    self.children(file, list.syntax());
                }
            }
            ast::Pat::TupleStructPat(it) => {
                let adt = self.pat_adt(pat);
                let res = it.path().map_or(Res::None, |path| self.path(path));
                if let Some(variant) = self.variant_of(res, adt) {
                    let rest = it.fields().position(|it| matches!(it, ast::Pat::RestPat(_)));
                    let bound = it.fields().count() - usize::from(rest.is_some());
                    let wildcard = it
                        .fields()
                        .filter(|it| !matches!(it, ast::Pat::RestPat(_)))
                        .map(|it| matches!(it, ast::Pat::WildcardPat(_)));
                    self.tuple_fields(variant, wildcard, bound, rest);
                }
                for sub in it.fields() {
                    stdx::ensure_sufficient_stack(|| self.walk(file, sub.syntax(), false));
                    self.in_pat = true;
                }
            }
            ast::Pat::PathPat(_) => {
                self.pat_adt(pat);
                self.children(file, node);
            }
            ast::Pat::IdentPat(it) => {
                if let Some(def) = cx.vsema.resolve_bind_pat_to_const(it) {
                    self.pat_adt(pat);
                    self.handle_res(Res::def(self.k.defs, def));
                }
                self.children(file, node);
            }
            _ => self.children(file, node),
        }
        self.in_pat = false;
    }

    /// The variant a tuple struct pattern (or the call a destructuring assignment lowers to one)
    /// binds the fields of: rustc's `adt.variant_of_res(res)` of the pattern's type, the variant
    /// its path names, else the struct (also for `Self`).
    fn variant_of(&self, res: Res, adt: Option<DefIdx>) -> Option<DefIdx> {
        match res {
            Res::Variant(it) => Some(it),
            Res::Def(_) | Res::SelfCtor => adt.filter(|&it| self.k.defs[it].kind == Kind::Struct),
            Res::None => None,
        }
    }

    /// The fields of `variant` that the subpatterns of a tuple pattern bind: whether each of its
    /// `bound` subpatterns other than `..` is a `_`, in order, and how many come before its `..`.
    fn tuple_fields(
        &mut self,
        variant: DefIdx,
        wildcard: impl Iterator<Item = bool>,
        bound: usize,
        dotdot: Option<usize>,
    ) {
        let defs = self.k.defs;
        let fields = defs.children(variant);
        let dotdot = dotdot.unwrap_or(bound);
        let missing = fields.len().saturating_sub(bound);
        for (index, is_wildcard) in wildcard.enumerate() {
            if is_wildcard {
                continue;
            }
            let index = if index < dotdot { index } else { index + missing };
            if let Some(&field) = fields.get(index) {
                self.insert_def_id(field);
            }
        }
    }

    /// The ADT a pattern is of is live (rustc's `check_def_id(adt.did())` of a struct, tuple
    /// struct or path pattern).
    fn pat_adt(&mut self, pat: ast::Pat<'db>) -> Option<DefIdx> {
        let ty = self.k.cx.vsema.type_of_pat(pat)?;
        let def = self.k.defs.get(Id::Adt(AdtId::from(ty.original.as_adt()?)))?;
        self.check_def_id(def);
        Some(def)
    }

    /// rustc's `necessary_variants` of a match arm's pattern: the variants its paths name
    /// (resolved, not through a type), outside `|` patterns, added to `out` past `from` once
    /// each.
    fn necessary_variants(&self, node: ast::Node<'db>, out: &mut Vec<DefIdx>, from: usize) {
        let cx = self.k.cx;
        let path = match node.kind() {
            SyntaxKind::OR_PAT => return,
            SyntaxKind::PATH_PAT => ast::PathPat::cast(node).and_then(|it| it.path()),
            SyntaxKind::TUPLE_STRUCT_PAT => {
                ast::TupleStructPat::cast(node).and_then(|it| it.path())
            }
            SyntaxKind::RECORD_PAT => ast::RecordPat::cast(node).and_then(|it| it.path()),
            SyntaxKind::IDENT_PAT => {
                if let Some(pat) = ast::IdentPat::cast(node)
                    && let Some(ModuleDef::EnumVariant(it)) =
                        cx.vsema.resolve_bind_pat_to_const(pat)
                    && let Some(variant) = self.k.defs.get(Id::Variant(it.into()))
                    && !out[from..].contains(&variant)
                {
                    out.push(variant);
                }
                None
            }
            _ => None,
        };
        if let Some(path) = path
            && self.is_resolved(path)
            && let Some(res) = cx.vsema.resolve_path(path)
            && let Res::Variant(variant) = Res::path(cx, self.k.defs, res, false)
            && !out[from..].contains(&variant)
        {
            out.push(variant);
        }
        for child in node.children() {
            let kind = child.kind();
            if ast::Pat::can_cast(kind) || kind == SyntaxKind::RECORD_PAT_FIELD_LIST {
                self.necessary_variants(child, out, from);
            } else if kind == SyntaxKind::RECORD_PAT_FIELD {
                for it in child.children() {
                    self.necessary_variants(it, out, from);
                }
            }
        }
    }

    /// Whether rustc's resolver resolves `path` (`QPath::Resolved`), rather than typeck through
    /// a type (`QPath::TypeRelative`: `Self::A`, `Alias::A`, `<E>::A`).
    fn is_resolved(&self, path: ast::Path<'db>) -> bool {
        let Some(qualifier) = path.qualifier() else { return true };
        if is_self(qualifier) || qualifier.segments().any(|it| it.type_anchor().is_some()) {
            return false;
        }
        matches!(
            self.k.cx.vsema.resolve_path(qualifier),
            Some(PathResolution::Def(ModuleDef::Module(_) | ModuleDef::Adt(hir::Adt::Enum(_))))
        )
    }

    /// `lhs = rhs`: rustc's `handle_assign` of the place written (its fields not read), or the
    /// pattern a destructuring assignment is lowered to; then `rhs`, and the self-assignment
    /// lint.
    fn assign(&mut self, file: HirFileId, expr: ast::BinExpr<'db>) {
        let Some(lhs) = expr.lhs() else {
            if let Some(rhs) = expr.rhs() {
                self.walk(file, rhs.syntax(), false);
            }
            return;
        };
        let destructuring = destructures(lhs);
        match destructuring {
            true => self.assign_pattern(file, lhs),
            false => self.handle_assign(file, lhs),
        }
        if let Some(rhs) = expr.rhs() {
            if !destructuring {
                self.self_assign_syntax(file, expr, lhs, rhs);
            }
            stdx::ensure_sufficient_stack(|| self.walk(file, rhs.syntax(), false));
        }
    }

    fn handle_assign(&mut self, file: HirFileId, place: ast::Expr<'db>) {
        let through_field = matches!(place, ast::Expr::FieldExpr(_) | ast::Expr::ParenExpr(_));
        let derefs =
            through_field
                && self.k.cx.vsema.expr_adjustments(place).is_some_and(|it| {
                    it.iter().any(|it| matches!(it.kind, hir_ty::Adjust::Deref(_)))
                });
        match place {
            ast::Expr::FieldExpr(field) if !derefs => {
                if let Some(base) = field.expr() {
                    self.handle_assign(file, base);
                }
            }
            ast::Expr::ParenExpr(it) if !derefs => {
                if let Some(inner) = it.expr() {
                    self.handle_assign(file, inner);
                }
            }
            _ => stdx::ensure_sufficient_stack(|| self.walk(file, place.syntax(), false)),
        }
    }

    /// The left of a destructuring assignment as the pattern rustc lowers it to, each place in
    /// it assigned.
    fn assign_pattern(&mut self, file: HirFileId, lhs: ast::Expr<'db>) {
        let cx = self.k.cx;
        match lhs {
            ast::Expr::ParenExpr(it) => {
                if let Some(inner) = it.expr() {
                    self.assign_pattern(file, inner);
                }
            }
            ast::Expr::TupleExpr(it) => {
                for field in it.fields() {
                    self.assign_pattern(file, field);
                }
            }
            ast::Expr::ArrayExpr(it) => {
                for field in it.exprs() {
                    self.assign_pattern(file, field);
                }
            }
            ast::Expr::UnderscoreExpr(_) => {}
            ast::Expr::RangeExpr(it) if it.start().is_none() && it.end().is_none() => {}
            ast::Expr::RecordExpr(it) => {
                self.expr_adt(lhs);
                if let Some(path) = it.path() {
                    self.in_pat = true;
                    self.path(path);
                    self.in_pat = false;
                }
                for field in it.record_expr_field_list().into_iter().flat_map(|it| it.fields()) {
                    let value = field.expr();
                    if matches!(value, Some(ast::Expr::UnderscoreExpr(_))) {
                        continue;
                    }
                    if let Some((def, _, _)) = cx.vsema.resolve_record_field(field)
                        && let Some(def) = self.k.defs.get(Id::Field(FieldId::from(def)))
                    {
                        self.insert_def_id(def);
                    }
                    if let Some(value) = value {
                        self.assign_pattern(file, value);
                    }
                }
            }
            ast::Expr::CallExpr(it) => {
                let path = match it.expr() {
                    Some(ast::Expr::PathExpr(it)) => it.path(),
                    _ => None,
                };
                let Some(path) = path else {
                    self.handle_assign(file, lhs);
                    return;
                };
                let adt = self.expr_adt(lhs);
                self.in_pat = true;
                let res = self.path(path);
                self.in_pat = false;
                let args = || it.arg_list().into_iter().flat_map(|it| it.args());
                if let Some(variant) = self.variant_of(res, adt) {
                    let rest = args().position(is_rest);
                    let bound = args().count() - usize::from(rest.is_some());
                    let wildcard = args()
                        .filter(|&it| !is_rest(it))
                        .map(|it| matches!(it, ast::Expr::UnderscoreExpr(_)));
                    self.tuple_fields(variant, wildcard, bound, rest);
                }
                for arg in args() {
                    self.assign_pattern(file, arg);
                }
            }
            ast::Expr::PathExpr(it) => {
                let path = it.path();
                let constructs =
                    path.and_then(|path| cx.vsema.resolve_path(path)).is_some_and(|it| {
                        matches!(
                            it,
                            PathResolution::Def(ModuleDef::Adt(_) | ModuleDef::EnumVariant(_))
                        )
                    });
                match (constructs, path) {
                    (true, Some(path)) => {
                        self.expr_adt(lhs);
                        self.in_pat = true;
                        self.path(path);
                        self.in_pat = false;
                    }
                    _ => self.handle_assign(file, lhs),
                }
            }
            _ => self.handle_assign(file, lhs),
        }
    }

    fn expr_adt(&mut self, expr: ast::Expr<'db>) -> Option<DefIdx> {
        let ty = self.k.cx.vsema.type_of_expr(expr)?;
        let def = self.k.defs.get(Id::Adt(AdtId::from(ty.original.as_adt()?)))?;
        self.check_def_id(def);
        Some(def)
    }

    /// rustc's `check_for_self_assign`: `x = x` or `a.b = a.b`, not from a macro's expansion.
    /// The two sides' shapes and names are compared before anything is resolved.
    fn self_assign_syntax(
        &mut self,
        file: HirFileId,
        expr: ast::BinExpr<'db>,
        lhs: ast::Expr<'db>,
        rhs: ast::Expr<'db>,
    ) {
        let cx = self.k.cx;
        if !same_place(cx, lhs, rhs) {
            return;
        }
        if crate::lints::syntax::from_expansion(cx, InFile::new(file, expr.syntax())) {
            return;
        }
        let at = InFile::new(file, ast::ptr(expr.syntax()));
        if !self.self_assigns.insert(at) {
            return;
        }
        let Some(ty) = cx.vsema.type_of_expr(lhs) else { return };
        let what = match unparen(lhs) {
            ast::Expr::FieldExpr(_) => "field",
            _ => "variable",
        };
        // alloc: the edge, a lint that fires
        let message =
            format!("useless assignment of {what} of type `{}` to itself", cx.ty(&ty.original));
        let mut diag = Diag::lint("dead_code", message, cx.span_of(at));
        crate::lints::code_fact(cx, &mut diag, at);
        self.found.push(Found::lint(diag, Phase::Late, at));
    }
}

/// Whether the left of an assignment is a pattern rustc lowers a destructuring assignment to.
fn destructures(lhs: ast::Expr<'_>) -> bool {
    match lhs {
        ast::Expr::ParenExpr(it) => it.expr().is_some_and(destructures),
        ast::Expr::TupleExpr(_)
        | ast::Expr::ArrayExpr(_)
        | ast::Expr::RecordExpr(_)
        | ast::Expr::CallExpr(_)
        | ast::Expr::UnderscoreExpr(_) => true,
        _ => false,
    }
}

/// `..` as an argument of a destructuring assignment.
fn is_rest(expr: ast::Expr<'_>) -> bool {
    matches!(expr, ast::Expr::RangeExpr(it) if it.start().is_none() && it.end().is_none())
}

fn unparen(expr: ast::Expr<'_>) -> ast::Expr<'_> {
    match expr {
        ast::Expr::ParenExpr(it) => it.expr().map_or(expr, unparen),
        _ => expr,
    }
}

/// `check_for_self_assign_helper`: the same local (a one-segment path on each side with the same
/// name, resolved only then), or the same field of the same place.
fn same_place<'db>(cx: &crate::cx::Cx<'_, 'db>, lhs: ast::Expr<'db>, rhs: ast::Expr<'db>) -> bool {
    match (unparen(lhs), unparen(rhs)) {
        (ast::Expr::PathExpr(l), ast::Expr::PathExpr(r)) => {
            let (Some(l), Some(r)) = (l.path(), r.path()) else { return false };
            let same_name = match (l.as_single_name_ref(), r.as_single_name_ref()) {
                (Some(a), Some(b)) => unraw(a.text()) == unraw(b.text()),
                _ => false,
            };
            let local = |it: ast::Path<'db>| match cx.vsema.resolve_path(it)? {
                PathResolution::Local(it) => Some(it),
                _ => None,
            };
            same_name && matches!((local(l), local(r)), (Some(l), Some(r)) if l == r)
        }
        (ast::Expr::FieldExpr(l), ast::Expr::FieldExpr(r)) => {
            let same_name = match (l.name_ref(), r.name_ref()) {
                (Some(a), Some(b)) => unraw(a.text()) == unraw(b.text()),
                _ => false,
            };
            match (l.expr(), r.expr()) {
                (Some(l), Some(r)) if same_name => same_place(cx, l, r),
                _ => false,
            }
        }
        _ => false,
    }
}

/// A name's text as its symbol is: `r#` stripped.
fn unraw(text: &str) -> &str {
    text.strip_prefix("r#").unwrap_or(text)
}

/// Whether `path` is the one segment `Self`.
fn is_self(path: ast::Path<'_>) -> bool {
    path.segment().is_some_and(|it| it.self_type_token().is_some())
}
