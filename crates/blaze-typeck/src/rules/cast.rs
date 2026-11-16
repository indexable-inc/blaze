//! Casts: checked with the target as a castable expectation, then deferred past fallback.
//!
//! Follows rustc_hir_typeck/src/expr.rs (`check_expr_cast`) and cast.rs (`CastCheck::check`,
//! `try_coercion_cast`, `do_check`, `CastTy::from_ty`); Relax maps them in typeck/cast.rs.
//!
//! rustc defers every cast to `check_casts`, which runs after integer and float fallback: an
//! unsuffixed literal under `ExpectCastableToType(u8)` is `u8` already (`check_lit`'s hint),
//! and the variables left are resolved by fallback before the cast is judged. A cast is
//! first tried as a coercion (a trivial cast); then by the kinds of both sides: numeric to
//! numeric, `bool`/`char`/C-like enum to integer, `u8` to `char` (other integers are E0604),
//! pointer to pointer or address, address to pointer, fn item or pointer to pointer or address,
//! `&[T; N]` to `*const T`. A non-primitive side is E0605, anything else E0606.
//!
//! Omitted for now: the metadata rules of wide-pointer casts (`*const [T]` to `*const U`
//! keeps or drops length), `dyn` upcasts, and the trivial-cast lint.

use crate::diag::{At, DiagCode};
use crate::error::Stop;
use crate::ids::{ExprId, UintTy};
use crate::machine::{CastCheck, Expectation, Flow, Frame, Machine, Then};
use crate::portable::PTy;
use crate::ty::{Ty, TyView, TypeCtx};
use crate::world::{AdtKind, CtorKind, World};

/// rustc's `CastTy`, coarsened to what the rules need.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// An integer; `true` for `u8`.
    Int(bool),
    Float,
    /// `bool`, `char` or a C-like enum: castable to an integer only.
    IntLike,
    Char,
    Ptr,
    FnPtr,
    /// A reference to an array: castable to a pointer.
    RefArray,
    Other,
}

impl<W: World, C: TypeCtx> Machine<'_, '_, '_, '_, W, C> {
    pub(crate) fn expr_cast(&mut self, mut f: Frame, e: ExprId, operand: ExprId, ty: PTy) -> Result<Flow, Stop> {
        if f.state == 0 {
            let to = self.lower_body_ty(ty, At::Expr(e))?;
            f.t = to;
            self.resume(f, 1)?;
            self.push_expr(operand, Expectation::Castable(to), Then::Nothing)?;
            return Ok(Flow::Wait);
        }
        let from = self.ret;
        let to = f.t;
        if !self.is_err(from) && !self.is_err(to) {
            self.casts.push(CastCheck { expr: e, operand, from, to });
        }
        Ok(Flow::Done(to))
    }

    fn cast_kind(&mut self, t: Ty) -> Result<Kind, Stop> {
        Ok(match self.view(t) {
            TyView::Int(_) => Kind::Int(false),
            TyView::Uint(u) => Kind::Int(u == UintTy::U8),
            TyView::Float(_) => Kind::Float,
            TyView::Bool => Kind::IntLike,
            TyView::Char => Kind::Char,
            TyView::Ptr(..) => Kind::Ptr,
            TyView::FnPtr(..) | TyView::FnDef(..) => Kind::FnPtr,
            TyView::Ref(_, inner) => match self.view(inner) {
                TyView::Array(..) => Kind::RefArray,
                _ => Kind::Other,
            },
            TyView::Adt(k, _) => match self.reads.adt(k)? {
                Some(adt)
                    if adt.kind == AdtKind::Enum
                        && !adt.variants.is_empty()
                        && adt.variants.iter().all(|v| v.ctor == CtorKind::Unit) =>
                {
                    Kind::IntLike
                }
                _ => Kind::Other,
            },
            _ => Kind::Other,
        })
    }

    /// `check_casts`: every deferred cast, in the order the walk met them.
    pub(crate) fn check_casts(&mut self) -> Result<(), Stop> {
        let mut i = 0;
        while i < self.casts.len() {
            let c = self.casts[i];
            i += 1;
            let from = self.ty.resolve(c.from)?;
            let to = self.ty.resolve(c.to)?;
            if self.is_err(from) || self.is_err(to) {
                continue;
            }
            // A trivial cast is a coercion.
            if self.commit_if(|m| -> Result<Option<()>, Stop> {
                Ok(if m.coerce(Some(c.operand), from, to, false)? { Some(()) } else { None })
            })?
            .is_some()
            {
                continue;
            }
            let ok = match (self.cast_kind(from)?, self.cast_kind(to)?) {
                (Kind::Int(_) | Kind::Float, Kind::Int(_) | Kind::Float) => Ok(()),
                (Kind::IntLike | Kind::Char, Kind::Int(_)) => Ok(()),
                (Kind::Int(true), Kind::Char) => Ok(()),
                (Kind::Int(false), Kind::Char) => Err(DiagCode::CharCast),
                (Kind::Ptr | Kind::FnPtr, Kind::Ptr | Kind::Int(_)) => Ok(()),
                (Kind::Int(_), Kind::Ptr) => Ok(()),
                (Kind::RefArray, Kind::Ptr) => Ok(()),
                (Kind::Other, _) | (_, Kind::Other) => Err(DiagCode::NonPrimitiveCast),
                _ => Err(DiagCode::InvalidCast),
            };
            if let Err(code) = ok {
                self.diag(code, At::Expr(c.expr), Some(to), Some(from), None);
            }
        }
        Ok(())
    }
}
