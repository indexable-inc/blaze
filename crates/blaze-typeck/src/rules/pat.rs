//! Patterns and default binding modes.
//!
//! Follows rustc_hir_typeck/src/pat.rs (`check_pat_top`, `check_pat`, `calc_adjust_mode`,
//! `calc_default_binding_mode`, `peel_off_references`, `check_pat_ident`, `check_pat_tuple`,
//! `check_pat_ref`, `check_pat_lit`, `check_pat_path`, `check_pat_tuple_struct`,
//! `check_pat_struct`, `check_struct_pat_fields`, `check_pat_slice`); Relax maps them in
//! typeck/pat.rs.
//!
//! A pattern is checked against an expected type with a default binding mode (move, `ref`,
//! `ref mut`). Non-reference patterns (tuples, structs, tuple structs, slices, paths, literals
//! other than string literals) first peel references off the expected type, each `&` making the
//! mode `ref` and each `&mut` making a move mode `ref mut`. Wildcards, bindings, `&` patterns
//! and or-patterns pass the type through. A binding's type is the expected type, behind a
//! reference when its mode is by-reference; an explicit `mut` resets the mode to move, and from
//! edition 2024 an explicit `mut`, `ref` or `&` under a by-reference default mode is an error.
//! A pattern's children are pushed all at once, in reverse so they run left to right; the
//! pattern's type needs no result of theirs.
//!
//! Omitted for now: recording the peeled references (`pat_adjustments`, which MIR building
//! reads), range patterns, `box` and deref patterns, subslice `..` in slice patterns, the
//! or-pattern check that every alternative binds a name at one type, and the 2024 rule's exact
//! diagnostic split (one code here).

use blaze_own::Vec;
use core::alloc::Allocator;

use crate::body::{Lit, Pat};
use crate::diag::{At, DiagCode};
use crate::error::Stop;
use crate::ids::{Edition, Mutbl, PatId};
use crate::machine::{Frame, Lower, Machine};
use crate::ty::{Cause, Ty, TyList, TyView, TypeCtx};
use crate::world::World;

/// The mode code a `ref` default gives to a reference of mutability `m` (`peel_off_references`).
fn peeled_mode(mode: u32, m: Mutbl) -> u32 {
    match (m, mode) {
        (Mutbl::Not, _) => 1,
        (Mutbl::Mut, 0) => 2,
        (Mutbl::Mut, other) => other,
    }
}

impl<W: World, C: TypeCtx, A: Allocator + Clone> Machine<'_, '_, '_, W, C, A> {
    /// One pattern. `f.expect` holds the expected type, `f.a` the default binding mode.
    pub(crate) fn step_pat(&mut self, f: Frame, p: PatId) -> Result<(), Stop> {
        let pat = self.body.pat(p)?;
        let mut expected = f.expect.to_option().unwrap_or(self.common.err);
        let mut mode = f.a;
        let peel = match pat {
            Pat::Tuple { .. } | Pat::TupleStruct { .. } | Pat::Struct { .. } | Pat::Slice { .. } | Pat::Path(_) => true,
            Pat::Lit(l) | Pat::NegLit(l) => !matches!(l, Lit::Str | Lit::ByteStr(_)),
            Pat::Missing | Pat::Wild | Pat::Bind { .. } | Pat::Ref { .. } | Pat::Or { .. } => false,
        };
        if peel {
            // Bounded by the recursion limit, as rustc's autoderef of patterns is.
            for _ in 0..crate::machine::RECURSION_LIMIT {
                let t = self.try_structurally_resolve(expected)?;
                match self.view(t) {
                    TyView::Ref(m, inner) => {
                        mode = peeled_mode(mode, m);
                        expected = inner;
                    }
                    _ => {
                        expected = t;
                        break;
                    }
                }
            }
        }
        let ty = self.pat_rule(p, pat, expected, mode)?;
        self.pat_ty[p.ix()] = ty;
        Ok(())
    }

    fn pat_rule(&mut self, p: PatId, pat: Pat, expected: Ty, mode: u32) -> Result<Ty, Stop> {
        let at = At::Pat(p);
        match pat {
            Pat::Missing => Ok(self.common.err),
            Pat::Wild => Ok(expected),
            Pat::Bind { ann, name: _, sub } => {
                let explicit = ann.mutable || ann.by_ref.is_some();
                if explicit && mode != 0 && self.body.edition >= Edition::E2024 {
                    self.diag(DiagCode::BindingModifier2024, at, None, None, None);
                }
                let by_ref = match ann.by_ref {
                    Some(m) => Some(m),
                    None if ann.mutable => None,
                    None => match mode {
                        1 => Some(Mutbl::Not),
                        2 => Some(Mutbl::Mut),
                        _ => None,
                    },
                };
                let local = match by_ref {
                    Some(m) => self.mk(TyView::Ref(m, expected))?,
                    None => expected,
                };
                self.local_ty[p.ix()] = local;
                if let Some(sub) = sub {
                    self.push_pat(sub, expected, mode)?;
                }
                Ok(expected)
            }
            Pat::Tuple { elems, rest } => {
                let pats = self.body.pat_list(elems)?;
                let t = self.try_structurally_resolve(expected)?;
                let known = match self.view(t) {
                    TyView::Tuple(l) => Some(self.ty.list_len(l) as usize),
                    _ => None,
                };
                let len = match (rest, known) {
                    (Some(_), Some(n)) => n.max(pats.len()),
                    _ => pats.len(),
                };
                let tys = self.fresh_args(len as u32)?;
                let tuple = self.mk(TyView::Tuple(tys))?;
                let elem_tys = if self.demand_eq(at, expected, tuple)? { tys } else { self.err_list(len)? };
                self.push_sub_pats(pats, rest, elem_tys, mode)?;
                Ok(expected)
            }
            Pat::Ref { mutbl, inner } => {
                if mode != 0 && self.body.edition >= Edition::E2024 {
                    self.diag(DiagCode::BindingModifier2024, at, None, None, None);
                }
                let t = self.try_structurally_resolve(expected)?;
                let inner_ty = match self.view(t) {
                    TyView::Ref(m, inner_ty) if m == mutbl => inner_ty,
                    TyView::Error => t,
                    _ => {
                        let fresh = self.fresh()?;
                        let r = self.mk(TyView::Ref(mutbl, fresh))?;
                        if self.demand_eq(at, expected, r)? { fresh } else { self.common.err }
                    }
                };
                self.push_pat(inner, inner_ty, 0)?;
                Ok(expected)
            }
            Pat::Lit(lit) | Pat::NegLit(lit) => {
                let lt = self.lit_ty(lit, crate::machine::Expectation::None)?;
                self.demand_eq(at, expected, lt)?;
                Ok(expected)
            }
            Pat::Path(path) => {
                let t = self.value_path(at, Cause::Pat(p), path)?;
                if !self.is_err(t) {
                    self.demand_eq(at, expected, t)?;
                }
                Ok(expected)
            }
            Pat::TupleStruct { path, elems, rest } => {
                let pats = self.body.pat_list(elems)?;
                let t = self.value_path(at, Cause::Pat(p), path)?;
                let sig = match self.view(t) {
                    TyView::FnDef(k, args) => self.fn_def_sig(k, args)?,
                    _ => None,
                };
                let Some((inputs, output)) = sig else {
                    if !self.is_err(t) {
                        self.diag(DiagCode::NotAValue, at, None, Some(t), None);
                    }
                    let errs = self.err_list(pats.len())?;
                    self.push_sub_pats(pats, None, errs, mode)?;
                    return Ok(expected);
                };
                self.demand_eq(at, expected, output)?;
                let n = self.ty.list_len(inputs) as usize;
                let fits = match rest {
                    Some(_) => pats.len() <= n,
                    None => pats.len() == n,
                };
                if !fits {
                    self.diag(DiagCode::PatternArity, at, None, None, None);
                    let errs = self.err_list(pats.len())?;
                    self.push_sub_pats(pats, None, errs, mode)?;
                    return Ok(expected);
                }
                self.push_sub_pats(pats, rest, inputs, mode)?;
                Ok(expected)
            }
            Pat::Struct { path, fields, rest } => {
                let fps = self.body.field_pat_list(fields)?;
                let Some((adt_key, variant, args)) = self.struct_path(at, path)? else {
                    for fp in fps.iter().rev() {
                        self.push_pat(fp.pat, self.common.err, mode)?;
                    }
                    return Ok(expected);
                };
                let adt_ty = self.mk(TyView::Adt(adt_key, args))?;
                self.demand_eq(at, expected, adt_ty)?;
                let Some(adt) = self.reads.adt(adt_key)? else { return Ok(expected) };
                let Some(v) = adt.variants.get(variant as usize).copied() else { return Ok(expected) };
                let declared = adt.fields.get(v.fields.range()).unwrap_or(&[]);
                let mut tys: Vec<Ty, A> = Vec::with_capacity_in(fps.len(), self.alloc.clone());
                for fp in fps {
                    match declared.iter().find(|d| d.name == fp.name) {
                        Some(d) => tys.push(self.lower(adt.types, d.ty, Lower::Args(args))?),
                        None => {
                            self.diag(DiagCode::NoSuchFieldPat, At::Pat(fp.pat), None, None, Some(fp.name));
                            tys.push(self.common.err);
                        }
                    }
                }
                if !rest && declared.iter().any(|d| !fps.iter().any(|fp| fp.name == d.name)) {
                    self.diag(DiagCode::MissingFieldPats, at, None, None, None);
                }
                for (fp, &t) in fps.iter().zip(tys.iter()).rev() {
                    self.push_pat(fp.pat, t, mode)?;
                }
                Ok(expected)
            }
            Pat::Or { alts } => {
                let pats = self.body.pat_list(alts)?;
                for &alt in pats.iter().rev() {
                    self.push_pat(alt, expected, mode)?;
                }
                Ok(expected)
            }
            Pat::Slice { elems } => {
                let pats = self.body.pat_list(elems)?;
                let t = self.structurally_resolve(expected, at)?;
                let elem = match self.view(t) {
                    TyView::Array(e, n) => {
                        if n != pats.len() as u64 {
                            self.diag(DiagCode::PatternArity, at, None, Some(t), None);
                        }
                        e
                    }
                    TyView::Slice(e) => e,
                    TyView::Error => t,
                    _ => {
                        self.diag(DiagCode::Mismatch, at, Some(expected), None, None);
                        self.common.err
                    }
                };
                for &sp in pats.iter().rev() {
                    self.push_pat(sp, elem, mode)?;
                }
                Ok(expected)
            }
        }
    }

    /// Pushes `pats` against the element types `tys`, `..` at `rest` skipping the middle
    /// (`check_pat_tuple`'s and `check_pat_tuple_struct`'s index mapping).
    fn push_sub_pats(&mut self, pats: &[PatId], rest: Option<u32>, tys: TyList, mode: u32) -> Result<(), Stop> {
        let n = self.ty.list_len(tys) as usize;
        for (i, &sp) in pats.iter().enumerate().rev() {
            let at = match rest {
                Some(r) if i >= r as usize => n - (pats.len() - i),
                _ => i,
            };
            let t = if at < n { self.ty.list_get(tys, at as u32) } else { self.common.err };
            self.push_pat(sp, t, mode)?;
        }
        Ok(())
    }
}
