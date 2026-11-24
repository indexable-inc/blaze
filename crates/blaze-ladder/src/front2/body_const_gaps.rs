//! Post-check resolution of body const-generic argument gaps whose parameter type was not
//! known when the body was lowered (`Gap::BodyConst` with [`ConstGapWhy::AnonInBody`] or
//! [`ConstGapWhy::NegativeNoSuffix`]).
//!
//! When a body writes `reader.read::<{ 256 / 8 }>()` (a method call) or
//! `<U12>::from_be_bytes::<{ 24 / 2 }>(b)` (an associated fn after an anchor), `to_typeck`
//! does not yet know the callee or its const parameter's declared type (`usize`, `u8`, `i8`,
//! ...), so an expression with unsuffixed integer literals cannot be evaluated safely at
//! lowering time (`anon::written_ty(PNode::Infer, text)` returns `None`).
//!
//! Once `blaze-typeck` resolves the callee on an initial check pass (with the gap left as
//! `PNode::Infer`), the callee's declared const parameter type is known from the store
//! (`by_origin::const_params_of`). This module evaluates the anonymous constant at that
//! declared type via the normal `anon::Ask` + `check::eval_len` path, patches a local copy
//! of the body's `type_lists` and `OwnTys` overlay with the resulting `ConstValue`, and lets
//! `check_body` re-check the body with the real constant value.

use blaze_lower::typeck::{ConstGapWhy, ConstSite};
use blaze_own::{AVec, Alloc};
use blaze_typeck as tc;
use blaze_typeck::{List, PNode, PTy, PTys};

use super::body::{Body, identifiers};
use super::check::{CheckFail, Records, const_eval_fail, const_nested_fail, eval_len};
use super::locals::LocalNs;
use super::source::NamesSource;
use super::types::Heads as _;

/// Local patched copies of `body.tys` and `body.typeck.type_lists` held across rounds of
/// `check_body` when one or more deferred `BodyConst` gaps have been evaluated.
pub(super) struct PatchedConsts {
    pub(super) tys: tc::OwnTys<'static>,
    pub(super) type_lists: AVec<'static, PTy>,
    pub(super) resolved: AVec<'static, bool>,
}

/// Whether `body` still has at least one unresolved `ConstSite::Arg` gap whose parameter type
/// can be supplied after `blaze-typeck` resolves the callee.
pub(super) fn has_resolvable_gap(body: &Body<'_>, patched: Option<&PatchedConsts>) -> bool {
    body.typeck.const_gaps.iter().enumerate().any(|(idx, gap)| {
        !patched.is_some_and(|p| p.resolved.get(idx).copied().unwrap_or(false))
            && gap.site == ConstSite::Arg
            && matches!(gap.why, ConstGapWhy::AnonInBody(_) | ConstGapWhy::NegativeNoSuffix)
    })
}

/// Resolves every deferred `BodyConst` argument gap whose callee and declared const parameter
/// type `done` now knows. Returns `Ok(true)` if at least one gap was patched into `patched`.
pub(super) fn resolve<'n, S: NamesSource<'n>>(
    r: &mut Records<'_, 'n, S>,
    body: &Body<'_>,
    done: &tc::Checked<'_>,
    patched: &mut Option<PatchedConsts>,
    alloc: Alloc<'static>,
) -> Result<bool, CheckFail> {
    if !has_resolvable_gap(body, patched.as_ref()) {
        return Ok(false);
    }
    let lowered = body.lowered.view();
    let types_at = r.store.locals.head(body.owner).map_or(body.module, |h| h.names_at);
    let mut progress = false;

    for (gap_idx, &gap) in body.typeck.const_gaps.iter().enumerate() {
        if patched.as_ref().is_some_and(|p| p.resolved.get(gap_idx).copied().unwrap_or(false)) {
            continue;
        }
        if gap.site != ConstSite::Arg || !matches!(gap.why, ConstGapWhy::AnonInBody(_) | ConstGapWhy::NegativeNoSuffix) {
            continue;
        }
        let want_range = tc::Range { start: gap.range.start, end: gap.range.end };
        let slot = {
            let cur_lists = match patched.as_ref() {
                Some(p) => p.type_lists.as_slice(),
                None => body.typeck.type_lists.as_slice(),
            };
            let cur_tys = match patched.as_ref() {
                Some(p) => p.tys.view(&r.store.tys),
                None => body.tys.view(&r.store.tys),
            }
            .map_err(|error| CheckFail::Check { owner: body.owner, error })?;
            match find_gap_slot(cur_lists, &body.typeck.type_list_ranges, cur_tys, want_range) {
                Some(slot) => slot,
                None => continue,
            }
        };
        let Some((ty_node, expr_scope)) = find_param_ty(r, body, done, slot) else {
            continue;
        };
        let Some(at) = find_body_range(&lowered, gap.range) else {
            continue;
        };
        let Ok(source) = lowered.text_at(at) else { continue };
        let Ok(mut marks) = lowered.marks_in(at, alloc) else { continue };
        let text = source.trim();
        let inner = text.strip_prefix('{').and_then(|t| t.strip_suffix('}')).map_or(text, str::trim);

        let self_ = super::const_open::ground_self_in(r.store.tys.view(), body.self_, &body.params, inner);
        if self_.is_none() && super::anon::mentions(inner, &body.params) {
            continue;
        }
        let local = expr_scope.is_some_and(|scope| {
            identifiers(inner).any(|ident| {
                r.store.locals.find(body.owner, Some(scope.0), ident, LocalNs::Value).is_some()
                    || r.store.locals.find(body.owner, Some(scope.0), ident, LocalNs::Type).is_some()
            })
        });
        let use_body = local || (!marks.is_empty() && identifiers(inner).next().is_some());
        let ask = if use_body {
            let Some(scope) = expr_scope else { continue };
            let Ok(leading) = u32::try_from(source.len().saturating_sub(source.trim_start().len())) else { continue };
            let Ok(text_len) = u32::try_from(text.len()) else { continue };
            marks.retain_mut(|(start, _)| {
                if *start < leading || *start - leading >= text_len {
                    return false;
                }
                *start -= leading;
                true
            });
            super::anon::Ask::in_body(types_at, ty_node, text, body.owner, scope.0, self_).map(|a| a.carry_marks(marks))
        } else if let Some((self_ty, self_node)) = self_ {
            super::anon::Ask::of_self(types_at, ty_node, inner, self_ty, self_node)
        } else {
            super::anon::Ask::of(types_at, ty_node, inner)
        };
        let Some(ask) = ask else { continue };
        let need = ask.need;

        let bits = if let Some(bits) = r.store.maps.const_bits(need) {
            bits
        } else if r.store.maps.anon_failed.contains_key(&need) {
            continue;
        } else {
            r.store.maps.ask_anon(ask);
            match eval_len(r, need) {
                Ok(()) => match r.store.maps.const_bits(need) {
                    Some(bits) => bits,
                    None => continue,
                },
                Err(super::consts::ConstFail::Nested { .. }) => {
                    if r.consts.is_none() && r.store.maps.evaluating.get() > 0 {
                        return Err(const_nested_fail(r, body.owner, need));
                    }
                    continue;
                }
                Err(error) => {
                    if r.store.maps.anon_failed.contains_key(&need) {
                        continue;
                    }
                    return Err(const_eval_fail(r, body.owner, need, error));
                }
            }
        };

        let p = patched.get_or_insert_with(|| {
            let mut type_lists = AVec::with_capacity_in(body.typeck.type_lists.len(), alloc);
            type_lists.extend_from_slice(&body.typeck.type_lists);
            let mut resolved = AVec::with_capacity_in(body.typeck.const_gaps.len(), alloc);
            resolved.resize(body.typeck.const_gaps.len(), false);
            PatchedConsts { tys: body.tys.clone_in(alloc), type_lists, resolved }
        });
        let ty_pty = p.tys.push_node(&mut r.store.tys, ty_node).map_err(|error| CheckFail::Check { owner: body.owner, error })?;
        let val_pty = p
            .tys
            .push_node(&mut r.store.tys, PNode::ConstValue { ty: ty_pty, bits })
            .map_err(|error| CheckFail::Check { owner: body.owner, error })?;
        if let Some(cell) = p.type_lists.get_mut(slot) {
            *cell = val_pty;
        }
        if let Some(flag) = p.resolved.get_mut(gap_idx) {
            *flag = true;
        }
        progress = true;
    }

    Ok(progress)
}

fn find_gap_slot(lists: &[PTy], ranges: &[tc::Range], tys: PTys<'_>, want: tc::Range) -> Option<usize> {
    for (slot, (&ty, &r)) in lists.iter().zip(ranges.iter()).enumerate() {
        if r == want && tys.get(ty) == Some(&PNode::Infer) {
            return Some(slot);
        }
    }
    None
}

fn list_offset(slot: usize, list: List) -> Option<u32> {
    let start = usize::try_from(list.start).ok()?;
    let len = usize::try_from(list.len).ok()?;
    let rel = slot.checked_sub(start)?;
    if rel < len { u32::try_from(rel).ok() } else { None }
}

fn find_param_ty<'n, S: NamesSource<'n>>(
    r: &Records<'_, 'n, S>,
    body: &Body<'_>,
    done: &tc::Checked<'_>,
    slot: usize,
) -> Option<(PNode, Option<tc::ScopeId>)> {
    for (ix, expr) in body.typeck.exprs.iter().enumerate() {
        let expr_id = tc::ExprId(u32::try_from(ix).ok()?);
        match expr {
            tc::Expr::MethodCall { generic_args, scope, .. } => {
                let Some(arg_idx) = list_offset(slot, *generic_args) else { continue };
                let pos = done.callees.column::<0>().iter().position(|&e| e == expr_id)?;
                let callee = *done.callees.column::<1>().get(pos)?;
                let parent = r.store.fns.get(&callee)?.parent;
                let consts = super::by_origin::const_params_of(r.names, r.keys, r.store, callee).ok()?;
                let (_, ty_pty) = consts.iter().find(|&&(i, _)| i == parent + arg_idx)?;
                return Some((r.store.tys.view().get(*ty_pty).copied()?, Some(*scope)));
            }
            tc::Expr::Path(path_idx) | tc::Expr::Struct { path: path_idx, .. } => {
                let bp = body.typeck.paths.get(usize::try_from(path_idx.0).ok()?)?;
                let segs = body.typeck.segments.get(bp.segments.range())?;
                for (seg_ix, seg) in segs.iter().enumerate() {
                    let Some(arg_idx) = list_offset(slot, seg.generic_args) else { continue };
                    let pos = done.expr_defs.column::<0>().iter().position(|&e| e == expr_id)?;
                    let def = *done.expr_defs.column::<1>().get(pos)?;
                    let (target, param_idx) = if seg_ix + 1 == segs.len() {
                        match def {
                            tc::Def::Fn(callee) => {
                                let parent = r.store.fns.get(&callee)?.parent;
                                (callee, parent + arg_idx)
                            }
                            tc::Def::Adt(adt) | tc::Def::Ctor { adt, .. } => (adt, arg_idx),
                            _ => return None,
                        }
                    } else if seg_ix + 2 == segs.len() {
                        match def {
                            tc::Def::Ctor { adt, .. } => (adt, arg_idx),
                            tc::Def::Fn(callee) => {
                                let container = r.store.fns.get(&callee)?.container?;
                                if let Some(imp) = r.store.impls.get(&container)
                                    && let Some(PNode::Adt(adt, _)) = r.store.tys.view().get(imp.self_ty).copied()
                                {
                                    (adt, arg_idx)
                                } else if r.keys.library_in(r.names, container).is_some_and(|lib| r.store.maps.traits.contains_key(&lib)) {
                                    (container, arg_idx + 1)
                                } else {
                                    return None;
                                }
                            }
                            _ => return None,
                        }
                    } else {
                        return None;
                    };
                    let consts = super::by_origin::const_params_of(r.names, r.keys, r.store, target).ok()?;
                    let (_, ty_pty) = consts.iter().find(|&&(i, _)| i == param_idx)?;
                    return Some((r.store.tys.view().get(*ty_pty).copied()?, Some(bp.scope)));
                }
            }
            _ => {}
        }
    }
    None
}

fn find_body_range(view: &blaze_lower::View<'_>, want: blaze_surface::Range) -> Option<blaze_lower::BodyRange> {
    for at in view.consts() {
        if view.site(at).ok() == Some(want) {
            return Some(at);
        }
    }
    for i in 0..view.types.len() {
        let Ok(idx) = u32::try_from(i) else { break };
        let t = blaze_lower::form::TypeId(idx);
        if let Ok(at) = view.type_range(t)
            && view.site(at).ok() == Some(want)
        {
            return Some(at);
        }
    }
    None
}
