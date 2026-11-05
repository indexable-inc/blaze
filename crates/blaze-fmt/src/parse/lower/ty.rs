//! Types, with rustc's `parse_ty_common` span: the type's first token to its last.

use syntax::view::{self as ra, AstNode};

use super::Lower;
use crate::ast::{self, DUMMY_NODE_ID, ThinVec};

impl Lower<'_> {
    pub(crate) fn ty(&self, ty: ra::Type<'_>) -> Box<ast::Ty> {
        let span = self.nsp(ty.syntax());
        let kind = match ty {
            ra::Type::ArrayType(t) => {
                let elem = self.opt_ty(t.ty(), span);
                match t.const_arg().and_then(|c| c.expr()) {
                    Some(len) => ast::TyKind::Array(elem, self.anon_const(len)),
                    None => ast::TyKind::Slice(elem),
                }
            }
            ra::Type::SliceType(t) => ast::TyKind::Slice(self.opt_ty(t.ty(), span)),
            ra::Type::DynTraitType(t) => {
                let bounds = t
                    .type_bound_list()
                    .map(|b| self.bounds(b))
                    .unwrap_or_default();
                let syntax = if t.dyn_token().is_some() {
                    ast::TraitObjectSyntax::Dyn
                } else {
                    ast::TraitObjectSyntax::None
                };
                ast::TyKind::TraitObject(bounds, syntax)
            }
            ra::Type::ImplTraitType(t) => {
                let bounds = t
                    .type_bound_list()
                    .map(|b| self.bounds(b))
                    .unwrap_or_default();
                ast::TyKind::ImplTrait(DUMMY_NODE_ID, bounds)
            }
            ra::Type::FnPtrType(t) => ast::TyKind::FnPtr(self.fn_ptr(t, ThinVec::new())),
            ra::Type::ForType(t) => {
                let params: ThinVec<ast::GenericParam> = t
                    .for_binder()
                    .and_then(|b| b.generic_param_list())
                    .map(|l| l.generic_params().map(|g| self.generic_param(g)).collect())
                    .unwrap_or_default();
                match t.ty() {
                    Some(ra::Type::FnPtrType(f)) => ast::TyKind::FnPtr(self.fn_ptr(f, params)),
                    _ => {
                        self.unsupported(span, "a `for<..>` type that is not a function pointer");
                        ast::TyKind::Err(ast::ErrorGuaranteed::unchecked_error_guaranteed())
                    }
                }
            }
            ra::Type::UnsafeBinderType(t) => {
                let generic_params: ThinVec<ast::GenericParam> = t
                    .generic_param_list()
                    .map(|l| l.generic_params().map(|g| self.generic_param(g)).collect())
                    .unwrap_or_default();
                ast::TyKind::UnsafeBinder(Box::new(ast::UnsafeBinderTy {
                    generic_params,
                    inner_ty: self.opt_ty(t.ty(), span),
                }))
            }
            ra::Type::InferType(_) => ast::TyKind::Infer,
            ra::Type::MacroType(t) => match t.macro_call() {
                Some(mc) => ast::TyKind::MacCall(self.mac_call(mc)),
                None => self.ty_kind_err(span),
            },
            ra::Type::NeverType(_) => ast::TyKind::Never,
            ra::Type::ParenType(t) => ast::TyKind::Paren(self.opt_ty(t.ty(), span)),
            ra::Type::PathType(t) => match t.path() {
                Some(p) => {
                    let (qself, path) = self.path(p);
                    ast::TyKind::Path(qself, path)
                }
                None => self.ty_kind_err(span),
            },
            ra::Type::PtrType(t) => {
                let mutbl = if t.mut_token().is_some() {
                    ast::Mutability::Mut
                } else {
                    ast::Mutability::Not
                };
                ast::TyKind::Ptr(self.opt_ty(t.ty(), span), mutbl)
            }
            ra::Type::RefType(t) => {
                let mutbl = if t.mut_token().is_some() {
                    ast::Mutability::Mut
                } else {
                    ast::Mutability::Not
                };
                let lt = t.lifetime().map(|l| self.lifetime(l));
                ast::TyKind::Ref(lt, self.opt_ty(t.ty(), span), mutbl)
            }
            ra::Type::TupleType(t) => ast::TyKind::Tup(t.fields().map(|f| self.ty(f)).collect()),
            ra::Type::PatternType(_) => {
                self.unsupported(span, "a pattern type");
                ast::TyKind::Err(ast::ErrorGuaranteed::unchecked_error_guaranteed())
            }
        };
        Box::new(ast::Ty {
            id: DUMMY_NODE_ID,
            kind,
            span,
        })
    }

    pub(crate) fn opt_ty(&self, ty: Option<ra::Type<'_>>, span: crate::span::Span) -> Box<ast::Ty> {
        match ty {
            Some(ty) => self.ty(ty),
            None => {
                self.unsupported(span, "a type rust-analyzer did not parse");
                self.ty_err(span)
            }
        }
    }

    fn ty_kind_err(&self, span: crate::span::Span) -> ast::TyKind {
        self.unsupported(span, "a type rust-analyzer did not parse");
        ast::TyKind::Err(ast::ErrorGuaranteed::unchecked_error_guaranteed())
    }

    /// rustc's `parse_ty_fn_ptr`: `decl_span` runs from the qualifiers (after any `for<..>`)
    /// through the return type.
    fn fn_ptr(
        &self,
        t: ra::FnPtrType<'_>,
        generic_params: ThinVec<ast::GenericParam>,
    ) -> Box<ast::FnPtrTy> {
        let decl_span = self.nsp(t.syntax());
        let safety = match t.unsafe_token() {
            Some(u) => ast::Safety::Unsafe(self.tsp(u)),
            None => ast::Safety::Default,
        };
        let ext = self.abi(t.abi());
        let decl = Box::new(self.fn_decl(t.param_list(), t.ret_type(), decl_span));
        if let Some(c) = t.const_token().or(t.async_token()) {
            self.unsupported(self.tsp(c), "a `const` or `async` function pointer");
        }
        Box::new(ast::FnPtrTy {
            safety,
            ext,
            generic_params,
            decl,
            decl_span,
        })
    }

    /// `extern` or `extern "abi"` (rustc's `parse_extern`).
    pub(crate) fn abi(&self, abi: Option<ra::Abi<'_>>) -> ast::Extern {
        let Some(abi) = abi else {
            return ast::Extern::None;
        };
        let Some(kw) = abi.extern_token() else {
            return ast::Extern::None;
        };
        let kw_span = self.tsp(kw);
        match abi.string_token() {
            None => ast::Extern::Implicit(kw_span),
            Some(s) => match self.str_lit(s) {
                Some(lit) => ast::Extern::Explicit(lit, kw_span),
                None => ast::Extern::Implicit(kw_span),
            },
        }
    }

    /// A string literal as `ast::StrLit`, as rustc's `parse_str_lit` builds it for ABIs.
    pub(crate) fn str_lit(&self, token: ra::Token<'_>) -> Option<ast::StrLit> {
        let span = self.tsp(token);
        let lit = self.token_lit(token);
        match ast::LitKind::from_token_lit(lit) {
            Ok(ast::LitKind::Str(symbol_unescaped, style)) => Some(ast::StrLit {
                symbol: lit.symbol,
                suffix: lit.suffix,
                symbol_unescaped,
                style,
                span,
            }),
            _ => {
                self.unsupported(span, "an ABI that is not a plain string");
                None
            }
        }
    }

    /// Generics of an item that has them, with rustc's empty spans when written without.
    pub(crate) fn item_generics<'t>(
        &self,
        node: impl ra::HasGenericParams<'t>,
        after: crate::span::Span,
        where_after: crate::span::Span,
    ) -> ast::Generics {
        self.generics(
            node.generic_param_list(),
            node.where_clause(),
            after,
            where_after,
        )
    }

    pub(crate) fn opt_bounds<'t>(&self, node: impl ra::HasTypeBounds<'t>) -> ast::GenericBounds {
        node.type_bound_list()
            .map(|b| self.bounds(b))
            .unwrap_or_default()
    }
}
