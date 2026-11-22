//! Calls of a fn under `#[rustc_legacy_const_generics(i, ..)]`: stdarch's intrinsics, declared
//! `#[rustc_legacy_const_generics(1)] pub fn vget_lane_u64<const IMM5: i32>(v: uint64x1_t) -> u64`
//! and called as C's intrinsics are, `vget_lane_u64(v, 0)` (hashbrown's `Group::match_full`).
//!
//! The rule, from rustc (rustc_ast_lowering/src/expr.rs `lower_legacy_const_generics`, reached
//! from `ExprKind::Call` when rustc_resolve's `legacy_const_generic_args` names the callee): a
//! call whose callee is a plain path (no qualified self) to such a fn, whose last segment writes
//! no generic arguments, is the call `f::<{a_i}, ..>(rest..)`: the argument at each listed
//! position is taken out of the value arguments and becomes, in argument order, the fn's next
//! own const generic argument, an anonymous constant. A callee written with `::<..>`, through
//! parentheses or a reference, or a call with fewer arguments than a listed position, is
//! checked as written (and a wrong count is E0061 there). The attribute's own checks (one index
//! per generic parameter, only const generics, only on a free fn) are blaze-diagnostics'
//! (checks/attrs/check.rs `legacy_const_generics`).
//!
//! Here the arguments stay where they are written, so every expression of the body is typed:
//! the formal inputs get the const parameter's declared type at each listed position
//! ([`Machine::legacy_const_inputs`]), so the count matches and the argument is coerced to that
//! type as a value argument would be; once every argument is typed, each one's constant is the
//! const generic argument ([`Machine::bind_legacy_consts`]), and the record's callee is
//! `vget_lane_u64::<0>`. The code builder passes only the arguments at the other positions.
//!
//! The constant of an argument: an integer literal's value (`BodyView::call_arg_ints`, the
//! lowering's), `-` of one at a fixed-width signed type, `true`/`false`, a const parameter of
//! the body's item, or a constant item with no generics. Any other constant expression (a
//! block, arithmetic, a `const fn` call) is an anonymous constant of the body, valued by the
//! lowering's caller, the one home of written constants (blaze-lower `Types::legacy_term`,
//! `BodyView::legacy_args`): its term, or a hole with the gap listed. A caller that resolves no
//! names gives none, and that argument is refused by name. A value that is no constant (a
//! local) is E0435 in rustc, a named gap here too until the checker has that code.

use crate::body::{Expr, Lit, UnOp};
use crate::diag::{About, At, DiagCode};
use crate::error::{CheckError, Stop};
use crate::ids::{ExprId, IntTy, ItemKey};
use crate::machine::{Lower, Machine};
use crate::ty::{Cause, Ty, TyList, TyView, TypeCtx};
use crate::world::{Def, FnSig, World};
use blaze_own::Vec;

/// The position, among the listed ones, of argument `i`: how many listed positions are before
/// it (the const parameters are taken in argument order). `None` when `i` is not listed.
fn rank(listed: &[u32], i: u32) -> Option<u32> {
    if !listed.contains(&i) {
        return None;
    }
    Some(listed.iter().filter(|&&x| x < i).count() as u32)
}

/// The bits of `-value` at the signed type `t`, two's complement in its width (as consteval
/// writes a const value, little-endian in the low bits). `None` for `isize`, whose width is the
/// target's.
fn negated_bits(t: IntTy, value: u128) -> Option<u128> {
    let width = match t {
        IntTy::I8 => 8,
        IntTy::I16 => 16,
        IntTy::I32 => 32,
        IntTy::I64 => 64,
        IntTy::I128 => 128,
        IntTy::Isize => return None,
    };
    let neg = value.wrapping_neg();
    Some(if width == 128 { neg } else { neg & ((1u128 << width) - 1) })
}

impl<'w, 'a, W: World, C: TypeCtx> Machine<'_, 'w, '_, 'a, W, C> {
    /// The signature of fn `k` when the call of it through `callee` with `n` arguments passes
    /// const generics by position (module doc): `k` lists positions, `callee` is a plain path
    /// whose last segment writes no arguments, every position is distinct and below `n`, and
    /// each of the first (number of positions) own parameters of `k` is a const parameter.
    fn legacy_sig(&mut self, callee: ExprId, k: ItemKey, n: usize) -> Result<Option<FnSig<'w>>, Stop> {
        let Some(sig) = self.reads.fn_sig(k)? else { return Ok(None) };
        let listed = sig.legacy_const_generics;
        if listed.is_empty() {
            return Ok(None);
        }
        let Expr::Path(p) = self.body.expr(callee)? else { return Ok(None) };
        let path = self.body.path(p)?;
        if path.anchor.is_some() || path.lang.is_some() {
            return Ok(None);
        }
        let segs = self.body.segment_list(path.segments)?;
        match segs.last() {
            Some(last) if !last.has_args => {}
            _ => return Ok(None),
        }
        // The path itself names `k` (not a value whose type derefs to it).
        if !self.defs.iter().any(|(at, def)| matches!(at, At::Expr(y) if *y == callee) && *def == Def::Fn(k)) {
            return Ok(None);
        }
        let distinct = listed.iter().enumerate().all(|(j, x)| !listed[..j].contains(x));
        let total = sig.inputs.len() + listed.len();
        if !distinct || listed.iter().any(|&i| i as usize >= n || i as usize >= total) || listed.len() > sig.own_count as usize {
            return Ok(None);
        }
        let consts = self.reads.const_params(k)?;
        if (0..listed.len() as u32).any(|r| consts.type_of(sig.parent_count + r).is_none()) {
            return Ok(None);
        }
        Ok(Some(sig))
    }

    /// The declared type of `k`'s own const parameter `r` (its `r`th own parameter).
    fn legacy_param_ty(&mut self, k: ItemKey, sig: &FnSig<'w>, r: u32, cause: Cause) -> Result<Ty, Stop> {
        let consts = self.reads.const_params(k)?;
        let index = sig.parent_count + r;
        let Some(pt) = consts.type_of(index) else {
            let site = crate::error::ParamSite::ConstParamType { owner: k };
            return Err(CheckError::Malformed(crate::error::Malformed::Param { index, args: 0, site }).into());
        };
        let empty = self.mk_list(&[])?;
        self.lower(consts.types, pt, Lower::Args(empty), cause)
    }

    /// The formal inputs of a call of fn item `k` through `callee` with `n` arguments: for a
    /// call that passes const generics by position ([`Self::legacy_sig`]), `inputs` with the
    /// declared type of the matching const parameter inserted at each listed position;
    /// otherwise `inputs` as they are.
    pub(crate) fn legacy_const_inputs(&mut self, callee: ExprId, k: ItemKey, inputs: TyList, n: usize) -> Result<TyList, Stop> {
        let Some(sig) = self.legacy_sig(callee, k, n)? else { return Ok(inputs) };
        let listed = sig.legacy_const_generics;
        let declared = self.list(inputs);
        let total = declared.len() + listed.len();
        let mut all: Vec<Ty, _> = Vec::with_capacity_in(total, self.alloc);
        let mut next = declared.iter().copied();
        for i in 0..total as u32 {
            let t = match rank(listed, i) {
                Some(r) => self.legacy_param_ty(k, &sig, r, Cause::Expr(callee))?,
                None => match next.next() {
                    Some(t) => t,
                    None => self.common.err,
                },
            };
            all.push(t);
        }
        self.mk_list(&all)
    }

    /// After every argument of call `e` (callee `callee`) is typed: for a call that passes const
    /// generics by position, each listed argument's constant ([`Self::legacy_term`]) is unified
    /// with the callee's matching const generic argument (a const variable the path made), so
    /// the record's callee holds the value (`vget_lane_u64::<0>`).
    pub(crate) fn bind_legacy_consts(&mut self, e: ExprId, callee: ExprId, args: &[ExprId]) -> Result<(), Stop> {
        let Some(&(_, k, fargs)) = self.callees.iter().rev().find(|c| c.0 == e) else {
            return Ok(());
        };
        let Some(sig) = self.legacy_sig(callee, k, args.len())? else { return Ok(()) };
        let listed = sig.legacy_const_generics;
        for (i, &arg) in args.iter().enumerate() {
            let Some(r) = rank(listed, i as u32) else { continue };
            let index = sig.parent_count + r;
            if index >= self.ty.list_len(fargs) {
                let site = crate::error::ParamSite::ExplicitArgs { item: k };
                let args_len = self.ty.list_len(fargs);
                return Err(CheckError::Malformed(crate::error::Malformed::Param { index, args: args_len, site }).into());
            }
            let param = self.ty.list_get(fargs, index);
            let declared = self.legacy_param_ty(k, &sig, r, Cause::Expr(arg))?;
            let term = self.legacy_term(arg, declared)?;
            if !self.unify(param, term)? {
                self.diag(DiagCode::Mismatch, At::Expr(arg), About::types(param, term));
            }
        }
        Ok(())
    }

    /// The constant of the argument `arg` at a listed position, as a const term of type `d`
    /// (module doc for the forms taken).
    fn legacy_term(&mut self, arg: ExprId, d: Ty) -> Result<Ty, Stop> {
        let mut x = arg;
        while let Expr::Paren(inner) = self.body.expr(x)? {
            x = inner;
        }
        match self.body.expr(x)? {
            Expr::Lit(Lit::Int(_)) => {
                let Some(v) = self.body.call_arg_int(x) else {
                    return Err(CheckError::Unported("an integer literal passed as a legacy const generic whose value the lowering did not give (BodyView::call_arg_ints)").into());
                };
                self.mk(TyView::ConstValue { ty: d, bits: v })
            }
            Expr::Lit(Lit::Bool(b)) => self.mk(TyView::ConstValue { ty: d, bits: u128::from(b) }),
            Expr::Unary { op: UnOp::Neg, operand } => {
                let mut o = operand;
                while let Expr::Paren(inner) = self.body.expr(o)? {
                    o = inner;
                }
                let (Expr::Lit(Lit::Int(_)), Some(v)) = (self.body.expr(o)?, self.body.call_arg_int(o)) else {
                    return Err(CheckError::Unported("a negated legacy const generic argument that is no integer literal (an anonymous constant in a body)").into());
                };
                let TyView::Int(t) = self.view(d) else {
                    // `-` of an unsigned (or any non-integer) value was refused where the `-` was
                    // checked (rules/op.rs `expr_unary`, suite/neg_unsigned.rs).
                    return Ok(self.common.err);
                };
                let Some(bits) = negated_bits(t, v) else {
                    return Err(CheckError::Unported("a negative `isize` legacy const generic argument (its width is the target's)").into());
                };
                self.mk(TyView::ConstValue { ty: d, bits })
            }
            Expr::Path(_) => {
                let def = self.defs.iter().rev().find(|(at, _)| matches!(at, At::Expr(y) if *y == x)).map(|&(_, def)| def);
                match def {
                    Some(Def::TyParam(j)) => self.mk(TyView::ConstParam(j)),
                    Some(Def::Const(item)) if !self.const_args.iter().any(|c| c.0 == At::Expr(x)) => match self.reads.const_value(item)? {
                        Some(v) => self.mk(TyView::ConstValue { ty: d, bits: u128::from(v) }),
                        None => Err(CheckError::Unported("a constant passed as a legacy const generic whose value the world does not give").into()),
                    },
                    // A path that did not resolve as a value is reported where it is written.
                    None => Ok(self.common.err),
                    Some(_) => Err(CheckError::Unported("a legacy const generic argument naming a generic constant or no constant (rustc: E0435 or an anonymous constant)").into()),
                }
            }
            Expr::Missing => Ok(self.common.err),
            // An anonymous constant (rustc's `lower_legacy_const_generics`: `f(a, 32 - 12)` is
            // `f::<{ 32 - 12 }>(a)`; chacha20's neon `rotate_left!`), valued by the lowering's
            // caller at the parameter's type (blaze-lower `Types::legacy_term`), keyed by the
            // argument as written. `lower_len` reads any lowered const term: a hole (a gap the
            // caller listed in `const_gaps`) is a fresh const variable. The caller unifies it
            // with the parameter (`bind_legacy_consts`).
            _ => match self.body.legacy_arg(arg) {
                Some(t) => self.lower_len(t, At::Expr(arg)),
                None => Err(CheckError::Unported("a legacy const generic argument that is no literal, const parameter or constant (an anonymous constant in a body the lowering's caller did not value)").into()),
            },
        }
    }
}
