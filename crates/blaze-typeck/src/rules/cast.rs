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
//! A target written with holes (`x as _`, `p as *const _`; the Reference, "Type cast
//! expressions", and "Inferred type": `_` "asks the compiler to infer the type if possible")
//! is lowered with a fresh type variable per hole, and the cast expression has that type, so
//! the body's later uses decide it (`let y = 7u8 as _; let z: u16 = y;`). When the casts are
//! judged, a side whose head is still a variable is E0282 (rustc's `structurally_resolve_type`
//! in `CastCheck::check`): a hole is never given the operand's type by the trivial cast.
//!
//! A fn item source (rustc's `do_check`, its `FnDef` case): an intrinsic has no address
//! (core/src/intrinsics/mod.rs: "The actual implementations live in the compiler."), so any
//! cast of it is E0606 (the coercion's `IntrinsicCast` becomes the cast's `IllegalCast`), a fn
//! pointer target included; a fn pointer type is no cast target past a coercion (rustc's
//! `(_, Int(CEnum) | FnPtr) => NonScalar`, E0605), so a safe `#[target_feature]` fn cast to a
//! safe fn pointer outside its features (rules/target_feature.rs) is E0605.
//!
//! Omitted for now: the metadata rules of wide-pointer casts (`*const [T]` to `*const U`
//! keeps or drops length), `dyn` upcasts, and the trivial-cast lint.

use crate::diag::{About, At, DiagCode};
use crate::error::Stop;
use crate::ids::{ExprId, Safety, UintTy};
use crate::machine::{CastCheck, Expectation, Flow, Frame, Machine, Then};
use crate::portable::PTy;
use crate::rules::target_feature::Gate;
use crate::ty::{Ty, TyView, TypeCtx};
use crate::world::{AdtKind, CtorKind, TypeAttrs, World};

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
            // rustc's `CastCheck::check` first `structurally_resolve_type`s both sides: a cast
            // decides its kind on known types only. A side whose head is still a type variable
            // here (after fallback: `let w = 7u8 as _;` that nothing constrains) is E0282 at
            // the cast, never settled by the trivial-cast coercion below: that coercion would
            // unify the variable with the other side, giving `_` the operand's own type
            // (`E::V as _` typed `E`, so `u32 == (E::V as _)` became `u32: PartialEq<E>`). A
            // head that is known with holes inside (`&x as *const _`) is the coercion's to
            // fill, as rustc's `try_coercion_cast` fills it.
            let from = self.structurally_resolve(c.from, At::Expr(c.operand))?;
            let to = self.structurally_resolve(c.to, At::Expr(c.expr))?;
            let from = self.ty.resolve(from)?;
            let to = self.ty.resolve(to)?;
            if self.is_err(from) || self.is_err(to) {
                continue;
            }
            // A fn item's own rules (the module doc), before the trivial cast: an intrinsic is
            // E0606 whatever the target; a gated `#[target_feature]` fn to a safe fn pointer is
            // E0605. A body with features of its own goes on to the coercion, which refuses it
            // by name (rules/reify.rs).
            if let TyView::FnDef(k, _) = self.view(from) {
                let attrs = self.reads.attrs(k)?;
                if attrs.0 & TypeAttrs::INTRINSIC != 0 {
                    self.diag(DiagCode::InvalidCast, At::Expr(c.expr), About::types(to, from));
                    continue;
                }
                if let TyView::FnPtr(_, hb) = self.view(to)
                    && hb.safety == Safety::Safe
                {
                    let own = match self.reads.fn_sig(k)? {
                        Some(sig) => Some(sig.header.safety),
                        None => None,
                    };
                    if let Some(own) = own
                        && self.feature_gate(attrs, own)? == Gate::Unsafe
                    {
                        self.diag(DiagCode::NonPrimitiveCast, At::Expr(c.expr), About::types(to, from));
                        continue;
                    }
                }
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
                // A fn pointer is no cast target past a coercion (rustc's `do_check`,
                // `(_, Int(CEnum) | FnPtr) => NonScalar`).
                (_, Kind::FnPtr) => Err(DiagCode::NonPrimitiveCast),
                _ => Err(DiagCode::InvalidCast),
            };
            if let Err(code) = ok {
                self.diag(code, At::Expr(c.expr), About::types(to, from));
            }
        }
        Ok(())
    }
}
