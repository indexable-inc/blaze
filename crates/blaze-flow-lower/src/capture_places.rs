//! Closure captures as PLACES in the borrow lowering (stage 2 of RFC 2229, "Closures capture
//! disjoint fields"; the Reference, "Closure types", "Capture modes"): a closure's environment
//! holds one field per captured PLACE (blaze-typeck env_fields.rs `EnvField`), so
//!
//! - creating the closure borrows or moves each place, not its root: `&mut x.a` held by the
//!   closure and a later `&x.b` are disjoint loans, a later use of `x.a` is not
//!   ([`Lower::capture_source`]);
//! - a by-value capture of `x.a` moves `x.a` only: the owner sees `x.a` moved and `x.b` live, and
//!   `x`'s scope end drops what remains as for `let a = x.a;` (the partial move is the owner's
//!   ordinary move of a projected place);
//! - inside the closure, a use of `x.a.b` is field `x.a`'s place projected by `.b`; a use of
//!   `r` whose field is `*r` by shared reference (`truncate_capture_for_optimization`) is the
//!   field's reference itself ([`Lower::place_capture_use`]); a use of a place only partly held
//!   (`x` when `x.a` and `x.b` are the fields) is refused by name.
//!
//! NOT DECLARED YET (written 2026-11-21 by a write-only lane): `mod capture_places;` in lib.rs,
//! the two `Lower` fields this file reads (`env_now`, `field_places`), the `Form::CapturePlace`
//! variant and the call sites are the SWITCH's, one save with typeck's `ActiveEnv = Places`
//! (the lane's report names each edit).

use blaze_flow::{Local, Place, Proj};
use blaze_typeck::env_fields::{Cover, cover, use_path};
use blaze_typeck::{CaptureKind, CaptureProj, ClosureKind, EnvField, ExprId, Mutbl, PNode};
use core::alloc::Allocator;

use crate::{Facts, Form, Lower, Refusal, TyD};

impl<F: Facts, A: Allocator + Copy, R: crate::one_call::CallRule> Lower<'_, '_, F, A, R> {
    /// How a closure of `kind` reaches field `how` (closure.rs `run_closure`'s table): owned
    /// (`None`), or through a reference of this mutability, the environment's own when the bool
    /// is set.
    pub(crate) fn field_through(how: CaptureKind, kind: ClosureKind) -> (Option<Mutbl>, bool) {
        match (how, kind) {
            (CaptureKind::ByValue, ClosureKind::FnOnce) => (None, false),
            (CaptureKind::ByValue, ClosureKind::FnMut) => (Some(Mutbl::Mut), true),
            (CaptureKind::ByValue, ClosureKind::Fn) => (Some(Mutbl::Not), true),
            (CaptureKind::ByRef, _) => (Some(Mutbl::Not), false),
            (CaptureKind::ByMutRef, ClosureKind::FnOnce) => (Some(Mutbl::Mut), false),
            (CaptureKind::ByMutRef, _) => (Some(Mutbl::Mut), true),
        }
    }

    /// The upvar of field `k` of the closure being lowered, a PROJECTED place `f` (a whole
    /// binding's is closure.rs `run_closure`'s): argument `Local(1 + k)` holds the place's
    /// value (`through` none) or a reference to it, and the place the body reaches is that
    /// local or `*local`, of the place's type. Filed in `field_places[k]`.
    pub(crate) fn place_upvar(&mut self, closure: ExprId, k: u32, f: EnvField, through: Option<Mutbl>) -> Result<(), Refusal> {
        let ck = self.ck;
        let Some(pt) = f.place_ty(ck) else { return self.refuse(Form::Malformed, closure) };
        let l = Local(1 + k);
        let place = match through {
            None => {
                self.new_local_with(TyD::of(pt), true, Some(l), closure)?;
                self.place_of(l)
            }
            Some(m) => {
                let rt = TyD { wrap: Some(m), ty: pt };
                self.new_local_with(rt, true, Some(l), closure)?;
                let at = self.place_of(l);
                self.project(at, rt, Proj::Deref, TyD::of(pt), false, closure)?
            }
        };
        while self.field_places.len() < k as usize {
            // Whole-binding fields before this one reach their place through `upvar_places`;
            // their slot here is never read (`place_capture_use` matches projected fields only).
            self.field_places.push(None);
        }
        self.field_places.push(Some((place, TyD::of(pt), through)));
        Ok(())
    }

    /// The closure body's use `e` of a captured place, when its root is held by PROJECTED
    /// fields (blaze-typeck env_fields.rs `cover`): `Ok(true)` with the place pushed, `Ok(false)`
    /// when the ordinary descent lowers it (a place under a field: the descent meets the
    /// field's own place on its way down; a root held whole; no captured root), a refusal by
    /// name when only parts of the place are held. Called from `eval` after its `Adjust` task,
    /// so the expression's own adjustments apply to what is pushed.
    pub(crate) fn place_capture_use(&mut self, e: ExprId) -> Result<bool, Refusal> {
        if !self.env_now.iter().any(|f| !f.is_whole()) {
            return Ok(false);
        }
        let path = match use_path(self.body, self.ck, e, 0) {
            Ok(Some(p)) => p,
            Ok(None) => return Ok(false),
            Err(_) => return self.refuse(Form::Malformed, e),
        };
        if !self.env_now.iter().any(|f| f.base == path.root && !f.is_whole()) {
            return Ok(false);
        }
        match cover(self.ck, &self.env_now, &path) {
            Cover::Exact(j) => match self.field_places.get(j).copied().flatten() {
                Some((place, ty, _)) => {
                    self.push_place(place, ty);
                    Ok(true)
                }
                None => self.refuse(Form::Malformed, e),
            },
            Cover::Under { .. } => Ok(false),
            // The field is `&*r` or `&mut *r`: `r`'s value is the field's pointer, the
            // argument local itself (`&T` / `&mut T` as the lowering lays a reference, `wrap`).
            Cover::RefOf(j) => match self.field_places.get(j).copied().flatten() {
                Some((_, ty, Some(m))) => {
                    let l = Local(1 + j as u32);
                    let at = self.place_of(l);
                    self.push_place(at, TyD { wrap: Some(m), ty: ty.ty });
                    Ok(true)
                }
                _ => self.refuse(Form::CapturePlace, e),
            },
            Cover::Above | Cover::NotCaptured => self.refuse(Form::CapturePlace, e),
        }
    }

    /// The owner's place of environment field `f` of the closure (or block) made at `e`, and
    /// its type: the root binding's place projected by the field's projections, each as the
    /// owner's own expression would project it (`field_done`, `deref`). What `closure_value`
    /// borrows (by reference) or moves (by value): a by-value field moves THAT place only.
    ///
    /// Refused by name: a root that is itself held by projected fields of the closure being
    /// lowered (a nested closure's place of a place-captured variable), a field of an enum
    /// variant or a union, an index (the analysis truncates at one; a row with one is
    /// malformed).
    pub(crate) fn capture_source(&mut self, f: EnvField, e: ExprId) -> Result<(Place, TyD), Refusal> {
        let ck = self.ck;
        let (mut at, mut ty, start_i) = if self.env_now.iter().any(|g| g.base == f.base && !g.is_whole()) {
            // Inside an outer closure that holds projected fields of `f.base`: find the
            // enclosing projected field (prefix of `f`) or a `RefOf` field (`&*f`), and
            // project any remaining steps from it.
            let mut found = None;
            for (j, g) in self.env_now.iter().enumerate() {
                if g.base != f.base || g.is_whole() {
                    continue;
                }
                let n = g.len as usize;
                let m = f.len as usize;
                let common = (0..n.min(m)).take_while(|&i| g.proj(ck, i as u32).map(|(p, _)| p) == f.proj(ck, i as u32).map(|(p, _)| p)).count();
                if common == n {
                    if let Some((place, pty, _)) = self.field_places.get(j).copied().flatten() {
                        found = Some((place, pty, g.len));
                        break;
                    }
                } else if common == m
                    && n == m + 1
                    && matches!(g.how, CaptureKind::ByRef | CaptureKind::ByMutRef)
                    && g.proj(ck, m as u32).is_some_and(|(p, _)| p == CaptureProj::Deref)
                    && let Some((_, pty, Some(mutbl))) = self.field_places.get(j).copied().flatten()
                {
                    let l = Local(1 + j as u32);
                    found = Some((self.place_of(l), TyD { wrap: Some(mutbl), ty: pty.ty }, f.len));
                    break;
                }
            }
            match found {
                Some(x) => x,
                None => return self.refuse(Form::CapturePlace, e),
            }
        } else {
            let (p, t) = self.binding_place(f.base, e)?;
            (p, t, 0)
        };
        for i in start_i..f.len {
            let Some((proj, after)) = f.proj(ck, i) else { return self.refuse(Form::Malformed, e) };
            let child = TyD::of(after);
            at = match proj {
                CaptureProj::Deref => {
                    if ty.wrap.is_some() {
                        self.project(at, ty, Proj::Deref, child, false, e)?
                    } else {
                        match self.node(ty.ty, e)? {
                            PNode::Ref(..) | PNode::Ptr(..) => self.project(at, ty, Proj::Deref, child, false, e)?,
                            PNode::Adt(k, _) if self.adt_facts(k, e)?.is_box => self.project(at, ty, Proj::DerefBox, child, false, e)?,
                            _ => return self.refuse(Form::Malformed, e),
                        }
                    }
                }
                CaptureProj::Field { variant, field } => {
                    if ty.wrap.is_some() {
                        return self.refuse(Form::Malformed, e);
                    }
                    let drop_type = match self.node(ty.ty, e)? {
                        PNode::Tuple(_) | PNode::Record(..) if variant == 0 => false,
                        PNode::Adt(k, _) => {
                            let facts = self.adt_facts(k, e)?;
                            if facts.union {
                                return self.refuse(Form::Union, e);
                            }
                            if facts.is_enum || variant != 0 {
                                return self.refuse(Form::CapturePlace, e);
                            }
                            facts.drop_impl
                        }
                        _ => return self.refuse(Form::Malformed, e),
                    };
                    self.project(at, ty, Proj::Field(field), child, drop_type, e)?
                }
                CaptureProj::Index => return self.refuse(Form::Malformed, e),
            };
            ty = child;
        }
        Ok((at, ty))
    }
}
