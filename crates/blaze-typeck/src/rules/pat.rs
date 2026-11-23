//! Patterns and default binding modes.
//!
//! Follows rustc_hir_typeck/src/pat.rs (`check_pat_top`, `check_pat`, `calc_adjust_mode`,
//! `calc_default_binding_mode`, `peel_off_references`, `check_pat_ident`, `check_pat_tuple`,
//! `check_pat_ref`, `check_pat_lit`, `check_pat_path`, `check_pat_tuple_struct`,
//! `check_pat_struct`, `check_struct_pat_fields`, `check_pat_slice`); Relax maps them in
//! typeck/pat.rs.
//!
//! A pattern is checked against an expected type with a default binding mode (move, `ref`,
//! `ref mut`). Non-reference patterns (tuples, structs, tuple structs, slices, paths other than
//! constants of a reference type, literals other than string literals) first peel references off the expected type, each `&` making the
//! mode `ref` and each `&mut` making a move mode `ref mut`. Wildcards, bindings, `&` patterns
//! and or-patterns pass the type through. A binding's type is the expected type, behind a
//! reference when its mode is by-reference; an explicit `mut` resets the mode to move, and from
//! edition 2024 an explicit `mut`, `ref` or `&` under a by-reference default mode is an error.
//! A pattern's children are pushed all at once, in reverse so they run left to right; the
//! pattern's type needs no result of theirs.
//!
//! Omitted for now: recording the peeled references (`pat_adjustments`, which MIR building
//! reads), `box` and deref patterns, the
//! or-pattern check that every alternative binds a name at one type, and the 2024 rule's exact
//! diagnostic split (one code here).

use blaze_own::{Alloc, Vec};

use crate::body::{Lit, Pat};
use crate::diag::{About, At, DiagCode};
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

impl<'a, W: World, C: TypeCtx> Machine<'_, '_, '_, 'a, W, C> {
    /// One pattern. `f.expect` holds the expected type, `f.a` the default binding mode.
    pub(crate) fn step_pat(&mut self, f: Frame, p: PatId) -> Result<(), Stop> {
        let pat = self.body.pat(p)?;
        let mut expected = f.expect.to_option().unwrap_or(self.common.err);
        let mut mode = f.a;
        // A path pattern's type is known before it meets the expected type, and a constant of
        // a reference type is no non-reference pattern (the Reference, "Binding modes":
        // "Non-reference patterns include all patterns except bindings, wildcard patterns
        // (`_`), const patterns of reference types, and reference patterns"): it does not
        // peel. jsonwebtoken's `match s.as_str() { ENC_A128GCM => .. }` with `const
        // ENC_A128GCM: &str` (census: `found &str, expected str`).
        let path_ty = match pat {
            Pat::Path(path) => Some(self.value_path(At::Pat(p), Cause::Pat(p), path)?),
            _ => None,
        };
        let peel = match pat {
            Pat::Path(_) => match path_ty {
                Some(t) => {
                    let t = self.try_structurally_resolve(t)?;
                    !matches!(self.view(t), TyView::Ref(..))
                }
                None => true,
            },
            Pat::Tuple { .. }
            | Pat::TupleStruct { .. }
            | Pat::Struct { .. }
            | Pat::Record { .. }
            | Pat::Slice { .. }
            | Pat::Range { .. } => true,
            Pat::Lit(l) | Pat::NegLit(l) => !matches!(l, Lit::Str | Lit::ByteStr(_) | Lit::CStr),
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
        let ty = self.pat_rule(p, pat, expected, mode, path_ty)?;
        self.pat_ty[p.ix()] = ty;
        Ok(())
    }

    /// `path_ty`: a path pattern's type, found before the peeling (`step_pat`).
    fn pat_rule(&mut self, p: PatId, pat: Pat, expected: Ty, mode: u32, path_ty: Option<Ty>) -> Result<Ty, Stop> {
        let at = At::Pat(p);
        match pat {
            Pat::Missing => Ok(self.common.err),
            Pat::Wild => Ok(expected),
            Pat::Bind { ann, name, sub, ty: None } => {
                let explicit = ann.mutable || ann.by_ref.is_some();
                // By the edition of the pattern's own words, not the body's (rustc
                // migration.rs `emit`: a hard error when a label's span is
                // `at_least_rust_2024()`): a 2021 macro's `mut x` in a 2024 body is the
                // migration lint's, which says nothing at check.
                if explicit && mode != 0 && self.body.pat_edition(p) >= Edition::E2024 {
                    self.diag(DiagCode::BindingModifier2024, at, About::found_named(expected, name));
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
            Pat::Bind { ann, name, sub, ty: Some(pty) } => {
                let ann_ty = self.lower_body_ty(pty, at)?;
                let mut cur_expected = self.try_structurally_resolve(expected)?;
                let mut cur_mode = mode;
                // Peel references on `expected` when it points to a structural union and the
                // written type annotation itself is not a reference (so matching `&err` with
                // `e: FsError` binds `e: &FsError`).
                if !matches!(self.view(ann_ty), TyView::Ref(..)) && self.peels_to_union(cur_expected)? {
                    for _ in 0..crate::machine::RECURSION_LIMIT {
                        let t = self.try_structurally_resolve(cur_expected)?;
                        match self.view(t) {
                            TyView::Ref(m, inner) => {
                                cur_mode = peeled_mode(cur_mode, m);
                                cur_expected = inner;
                            }
                            _ => {
                                cur_expected = t;
                                break;
                            }
                        }
                    }
                }
                let explicit = ann.mutable || ann.by_ref.is_some();
                if explicit && cur_mode != 0 && self.body.pat_edition(p) >= Edition::E2024 {
                    self.diag(DiagCode::BindingModifier2024, at, About::found_named(cur_expected, name));
                }
                let by_ref = match ann.by_ref {
                    Some(m) => Some(m),
                    None if ann.mutable => None,
                    None => match cur_mode {
                        1 => Some(Mutbl::Not),
                        2 => Some(Mutbl::Mut),
                        _ => None,
                    },
                };
                let resolved = self.try_structurally_resolve(cur_expected)?;
                if let TyView::Union(members) = self.view(resolved) {
                    let n = self.ty.list_len(members);
                    let mut matched: Option<u32> = None;
                    let mut count = 0u32;
                    for v in 0..n {
                        let mv = self.ty.list_get(members, v);
                        if self.probe(|m| m.unify(mv, ann_ty))? {
                            matched = Some(v);
                            count += 1;
                            if count > 1 {
                                break;
                            }
                        }
                    }
                    if count == 1
                        && let Some(v) = matched
                        && self.unify(self.ty.list_get(members, v), ann_ty)?
                    {
                        self.union_pats.push((p, crate::columns::UnionPat::Variant(v)));
                        let local = match by_ref {
                            Some(m) => self.mk(TyView::Ref(m, ann_ty))?,
                            None => ann_ty,
                        };
                        if let Some(slot) = self.local_ty.get_mut(p.ix()) {
                            *slot = local;
                        }
                        if let Some(sub) = sub {
                            self.push_pat(sub, ann_ty, cur_mode)?;
                        }
                    } else {
                        self.diag(DiagCode::Mismatch, at, About::types(resolved, ann_ty));
                        if let Some(slot) = self.local_ty.get_mut(p.ix()) {
                            *slot = self.common.err;
                        }
                        if let Some(sub) = sub {
                            self.push_pat(sub, self.common.err, cur_mode)?;
                        }
                    }
                } else {
                    self.demand_eq(at, cur_expected, ann_ty)?;
                    let local = match by_ref {
                        Some(m) => self.mk(TyView::Ref(m, ann_ty))?,
                        None => ann_ty,
                    };
                    if let Some(slot) = self.local_ty.get_mut(p.ix()) {
                        *slot = local;
                    }
                    if let Some(sub) = sub {
                        self.push_pat(sub, ann_ty, cur_mode)?;
                    }
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
                if mode != 0 && self.body.pat_edition(p) >= Edition::E2024 {
                    self.diag(DiagCode::BindingModifier2024, at, About::found(expected));
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
                let mut lt = self.lit_ty(lit, crate::machine::Expectation::None, At::Pat(p))?;
                // `check_pat_lit`: a byte string against `&[_]` is `&[u8]`, not `&[u8; N]`
                // (`match dir { b"/" => .. }` with `dir: &[u8]`).
                if let Lit::ByteStr(_) = lit {
                    let e = self.try_structurally_resolve(expected)?;
                    if let TyView::Ref(_, inner) = self.view(e) {
                        let inner = self.try_structurally_resolve(inner)?;
                        if let TyView::Slice(_) = self.view(inner) {
                            let s = self.mk(TyView::Slice(self.common.u8_))?;
                            lt = self.mk(TyView::Ref(Mutbl::Not, s))?;
                        }
                    }
                }
                self.demand_eq(at, expected, lt)?;
                Ok(expected)
            }
            Pat::Path(_) => {
                let Some(t) = path_ty else { return Err(crate::error::CheckError::Malformed(crate::error::Malformed::Pat(p)).into()) };
                if !self.is_err(t) {
                    self.demand_eq(at, expected, t)?;
                }
                Ok(expected)
            }
            Pat::TupleStruct { path, elems, rest } => {
                let pats = self.body.pat_list(elems)?;
                let t = self.value_path(at, Cause::Pat(p), path)?;
                let sig = match self.view(t) {
                    TyView::FnDef(k, args) => self.fn_def_sig(k, args, Cause::Pat(p))?,
                    _ => None,
                };
                let Some((inputs, output)) = sig else {
                    if !self.is_err(t) {
                        self.diag(DiagCode::NotAValue, at, About::found(t));
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
                    let (expected_n, got) = (self.ty.list_len(inputs), pats.len() as u32);
                    self.diag(DiagCode::PatternArity, at, About::Count { expected: expected_n, got, found: Some(output) });
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
                let mut tys: Vec<Ty, Alloc<'a>> = Vec::with_capacity_in(fps.len(), self.alloc);
                for fp in fps {
                    match declared.iter().find(|d| d.name == fp.name) {
                        Some(d) => tys.push(self.lower(adt.types, d.ty, Lower::Args(args), Cause::Pat(fp.pat))?),
                        None => {
                            self.diag(DiagCode::NoSuchFieldPat, At::Pat(fp.pat), About::name(fp.name));
                            tys.push(self.common.err);
                        }
                    }
                }
                if adt.kind == crate::world::AdtKind::Union {
                    // The Reference, "Struct patterns": "A struct pattern used to match a union
                    // must specify exactly one field", and it names no other (E0784).
                    if fps.len() != 1 {
                        self.diag(DiagCode::UnionPatternFields, at, About::Count { expected: 1, got: fps.len() as u32, found: Some(expected) });
                    }
                } else if !rest && let Some(missing) = declared.iter().find(|d| !fps.iter().any(|fp| fp.name == d.name)) {
                    self.diag(DiagCode::MissingFieldPats, at, About::found_named(expected, missing.name));
                }
                for (fp, &t) in fps.iter().zip(tys.iter()).rev() {
                    self.push_pat(fp.pat, t, mode)?;
                }
                Ok(expected)
            }
            Pat::Record { fields, rest } => {
                let fps = self.body.field_pat_list(fields)?;
                let t = self.structurally_resolve(expected, at)?;
                let (schema, field_tys) = match self.view(t) {
                    TyView::Record(schema, field_tys) => (schema, field_tys),
                    TyView::Error => {
                        for fp in fps.iter().rev() {
                            self.push_pat(fp.pat, self.common.err, mode)?;
                        }
                        return Ok(expected);
                    }
                    _ => {
                        self.diag(DiagCode::Mismatch, at, About::expected(expected));
                        for fp in fps.iter().rev() {
                            self.push_pat(fp.pat, self.common.err, mode)?;
                        }
                        return Ok(expected);
                    }
                };
                let n = self.ty.list_len(field_tys);
                let mut tys: Vec<Ty, Alloc<'a>> = Vec::with_capacity_in(fps.len(), self.alloc);
                for fp in fps {
                    match self.record_field_index(schema, fp.name) {
                        Some(idx) if idx < n => tys.push(self.ty.list_get(field_tys, idx)),
                        _ => {
                            self.diag(DiagCode::NoField, At::Pat(fp.pat), About::found_named(t, fp.name));
                            tys.push(self.common.err);
                        }
                    }
                }
                if !rest {
                    let missing = self.record_names(schema).and_then(|names| {
                        names.iter().copied().find(|&nm| !fps.iter().any(|fp| fp.name == nm))
                    });
                    if let Some(nm) = missing {
                        self.diag(DiagCode::MissingFields, at, About::found_named(t, nm));
                    }
                }
                for (fp, &ft) in fps.iter().zip(tys.iter()).rev() {
                    self.push_pat(fp.pat, ft, mode)?;
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
            Pat::Slice { elems, rest } => {
                // `check_pat_slice`: against `[T; N]` the elements other than `..` number `N`
                // exactly with no `..` and at most `N` with one, which then is `[T; N - k]`;
                // against `[T]`, any number, and the `..` is `[T]`. Each element is `T`.
                let pats = self.body.pat_list(elems)?;
                let fixed = pats.len() as u64 - u64::from(rest.is_some());
                // After peeling known references, an exact pattern constrains an unknown
                // scrutinee to [T; k]. This supplies the target in `let [a, b] = v.try_into()?`.
                // A rest pattern leaves the length open.
                if rest.is_none() && self.is_ty_var(expected) {
                    let elem = self.fresh()?;
                    let len = self.len_value(fixed)?;
                    let array = self.mk(TyView::Array(elem, len))?;
                    self.demand_eq(at, expected, array)?;
                }
                let t = self.structurally_resolve(expected, at)?;
                let (elem, rest_ty) = match self.view(t) {
                    // A length that is no value yet (a variable, a parameter): without `..` the
                    // array must be `[T; k]`, so the length is unified with the count (E0308
                    // when it cannot be, as for a parameter); with `..`, E0730.
                    TyView::Array(e, len) => match (self.len_of(len), rest) {
                        (Some(n), _) if (rest.is_some() && fixed <= n) || fixed == n => {
                            let rest_len = self.len_value(n - fixed)?;
                            (e, self.mk(TyView::Array(e, rest_len))?)
                        }
                        (None, None) => {
                            let count = self.len_value(fixed)?;
                            if !self.unify(len, count)? {
                                self.diag(DiagCode::Mismatch, at, About::expected(expected));
                            }
                            (e, self.common.err)
                        }
                        (None, Some(_)) => {
                            self.diag(DiagCode::ArrayLenUnknown, at, About::found(t));
                            (e, self.common.err)
                        }
                        (Some(_), _) => {
                            self.diag(DiagCode::PatternArity, at, About::found(t));
                            (e, self.common.err)
                        }
                    },
                    TyView::Slice(e) => (e, t),
                    TyView::Error => (t, t),
                    _ => {
                        self.diag(DiagCode::Mismatch, at, About::expected(expected));
                        (self.common.err, self.common.err)
                    }
                };
                for (i, &sp) in pats.iter().enumerate().rev() {
                    let ty = if rest == Some(i as u32) { rest_ty } else { elem };
                    self.push_pat(sp, ty, mode)?;
                }
                Ok(expected)
            }
            Pat::Range { lo, hi, inclusive: _ } => {
                // `check_pat_range` (the Reference, "Range patterns"): each end is typed alone
                // (a literal, a negated literal or a path to a constant), must be an integer, a
                // float or a `char` (E0029), and its type is the expected type (both ends one
                // type). `lo <= hi` is a constant's check, made where the pattern is lowered.
                for end in [lo, hi].into_iter().flatten() {
                    let end_ty = match self.body.pat(end)? {
                        Pat::Lit(l) | Pat::NegLit(l) => self.lit_ty(l, crate::machine::Expectation::None, At::Pat(end))?,
                        Pat::Path(path) => self.value_path(At::Pat(end), Cause::Pat(end), path)?,
                        _ => {
                            return Err(crate::error::CheckError::Malformed(crate::error::Malformed::Pat(end)).into());
                        }
                    };
                    let r = self.try_structurally_resolve(end_ty)?;
                    let numeric = matches!(
                        self.view(r),
                        TyView::Int(_) | TyView::Uint(_) | TyView::Float(_) | TyView::Char | TyView::Error | TyView::Infer(_)
                    );
                    if !numeric {
                        self.diag(DiagCode::RangePatType, At::Pat(end), About::found(r));
                        self.pat_ty[end.ix()] = self.common.err;
                        continue;
                    }
                    self.demand_eq(At::Pat(end), expected, end_ty)?;
                    self.pat_ty[end.ix()] = expected;
                }
                Ok(expected)
            }
        }
    }

    /// Whether peeling zero or more references off `ty` reaches a structural union.
    fn peels_to_union(&mut self, mut ty: Ty) -> Result<bool, Stop> {
        for _ in 0..crate::machine::RECURSION_LIMIT {
            let t = self.try_structurally_resolve(ty)?;
            match self.view(t) {
                TyView::Ref(_, inner) => ty = inner,
                TyView::Union(_) => return Ok(true),
                _ => return Ok(false),
            }
        }
        Ok(false)
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
