//! A call of a head with a hidden input (`fn take<T: Tr>(x: T::Out)`) whose type arguments
//! carry origins but name none of the caller's generics (`take::<Wrap<'_>>(&s)`): the hidden
//! input is read through the impl the TYPE CHECKER selected for the projection at this call
//! (`Checked::selected`, piece impl-per-call), never selected again here (rule 77), and its
//! `type Out = ..;` text is lined up by the one reader of impl texts, sig/impl_life.rs
//! (`read_impl_signature`, rule 30).
//!
//! THE GAP it closes (hidden_call.rs, "NOT KNOWN AT THE CALL"): with `T = Wrap<'x>` the impl may
//! write `type Out = &'a u8` (an origin of `T`) or `&'static u8`; without the impl's text the
//! call was REFUSED (`Form::CallHiddenInput`). tempo meets this at every iterator adapter and
//! `IntoIterator` call over a borrowed collection.
//!
//! THE RULES. The Reference, "Associated Items", "Associated types": the selected impl's
//! `type Out = U;` defines `<T as Tr>::Out` as `U` at this call. "Implementations", "Generic
//! implementations" (E0207): every lifetime of `U` is `'static` or one of the impl's, and each
//! of the impl's appears in its self type (`impl<'a> Tr for Wrap<'a>`), so it is the call's
//! `T`'s position there: impl_life.rs maps it to `Region::SelfTy(k)`. "Lifetime bounds":
//! `'static` "outlives all other lifetimes".
//!
//! WHAT IS RELATED, and why it is sound:
//! - a `'static` at a hidden position: the argument's origin there must outlive `'static`
//!   (EXACT: the demand the coarse path missed, E0597 for `&local`);
//! - a `SelfTy(k)` (one of `T`'s lifetimes): left to the call's older path, which `call_done`
//!   takes next: the type arguments carry origins, so it is the one-hub relation (lib.rs
//!   `unify_call`), where every operand's positions flow into the hub and the hub into every
//!   position but a reference operand's first. That is a SUPERSET of `arg: T_k` with `T_k`
//!   related to `T`'s other occurrences (more relations refuse more, never fewer): no error is
//!   missed; at worst a false one, as before this piece for every call with origins in its
//!   type arguments.
//! - an impl whose header bounds a mapped lifetime by `'static` (`impl Tr for Wrap<'static>`,
//!   a `SelfTy(k): 'static`) is REFUSED by name: the demand falls on `T`'s positions, which
//!   this path holds no origins for.
//!
//! WHICH ROW: the checker's rows at a call are in the order the signature was lowered (inputs
//! left to right, then the output), one per projection; the reader numbers hidden positions in
//! the same text order (`Region::Hidden(h)`). Row `h` is used only when the call's row count
//! equals its hidden position count; else (an opaque type among them, a projection nested in
//! another's arguments) the call is refused by name. A hidden input must be the WHOLE parameter
//! (`x: T::Out`, not `x: Vec<T::Out>`): its erased type is then the argument's own type.
//!
//! Keys (rule 21): the rows are a column of the caller's checked record; the impl's text is
//! what `Facts::impl_source` gives, filed in the body's read list keyed by the IMPL (front2's
//! `sig_impl::impl_read` form, `Want::Impl`: its header and every non-fn member's text).

use blaze_flow::Origin;
use blaze_own::Vec;
use blaze_typeck::{ExprId, ItemKey, Name, Selected};
use core::alloc::Allocator;

use crate::hidden_call::HiddenCall;
use crate::sig::{ImplText, Region, SigBuf, SigLife, read_impl_signature};
use crate::{At, Facts, Form, Lower, Refusal, Val};

/// An impl's texts, as the borrow check reads them at a call that selected it: the impl's
/// item text (its header is read to its first `{`) and each associated type member's
/// `(name, text)`.
#[derive(Clone, Copy, Debug)]
pub struct ImplSource<'t> {
    pub text: &'t str,
    pub assoc: &'t [(&'t str, &'t str)],
}

/// The longest associated type name the synthetic member text holds (rule 18).
const MOST_NAME: usize = 1024;

impl<'b, 'c, F: Facts, A: Allocator + Clone> Lower<'b, 'c, F, A> {
    /// A call of `item` (lifetime part `life`, holding a hidden position) whose type arguments
    /// carry origins and name none of the caller's generics: each hidden input whose argument
    /// carries origins is read through the selected impl (the module doc). `Coarse`: the
    /// `'static` demands are in; the caller relates the rest by its older path.
    pub(crate) fn call_hidden_by_impl(&mut self, item: ItemKey, life: SigLife<'_>, vs: &[Val], e: ExprId) -> Result<HiddenCall, Refusal> {
        let rows = self.ck.selected_at(e);
        let hidden = life.regions.iter().filter(|r| matches!(r, Region::Hidden(_))).count();
        let numbered = rows.len() == hidden && !life.regions.contains(&Region::Hidden(u32::MAX));
        let facts: &'b F = self.facts;
        let adt_lifetimes = |k: ItemKey| facts.adt(k).map(|a| a.lifetimes);
        let mut inst: Vec<(Region, Origin), A> = Vec::new_in(self.alloc.clone());
        let mut text: Vec<u8, A> = Vec::new_in(self.alloc.clone());
        let mut lo = 0usize;
        for (i, v) in vs.iter().enumerate() {
            let Some(&end) = life.ends.get(i) else {
                return self.refuse(Form::CallHiddenInput { callee: item, input: u32::MAX }, e);
            };
            let hi = end as usize;
            let refused = Form::CallHiddenInput { callee: item, input: i as u32 };
            if hi < lo || hi > life.regions.len() {
                return self.refuse(refused, e);
            }
            let part = &life.regions[lo..hi];
            lo = hi;
            let Some(h) = part.iter().find_map(|r| match r {
                Region::Hidden(h) => Some(*h),
                _ => None,
            }) else {
                continue;
            };
            let At::Place(p) = v.at else { continue };
            let info = self.info(p);
            if info.len == 0 {
                continue;
            }
            if !numbered || part.len() != 1 || !info.exact || v.ty.wrap.is_some() {
                return self.refuse(refused, e);
            }
            let Some(row) = rows.get(h as usize).copied() else { return self.refuse(refused, e) };
            let Selected::Impl { imp, .. } = row.selected else { return self.refuse(refused, e) };
            let Some(src) = facts.impl_source(imp) else { return self.refuse(refused, e) };
            let Some(&self_ty) = self.list(row.args, e)?.first() else { return self.refuse(refused, e) };
            let Some(&(name, _)) = src.assoc.iter().find(|(n, _)| Name::of(n) == row.name) else {
                return self.refuse(refused, e);
            };
            if name.len() > MOST_NAME {
                return self.refuse(refused, e);
            }
            // The member as a fn of the impl would name it: `Self::Out` is lined up against
            // the argument's erased type through the impl's `type Out` text, its lifetimes
            // through the impl's header (impl_life.rs); the output repeats the input so no
            // other type is needed, and is not read below.
            text.clear();
            text.extend_from_slice(b"fn hidden(x: Self::");
            text.extend_from_slice(name.as_bytes());
            text.extend_from_slice(b") -> Self::");
            text.extend_from_slice(name.as_bytes());
            let Ok(t) = core::str::from_utf8(&text) else { return self.refuse(refused, e) };
            let imp_text = ImplText { text: src.text, self_ty, assoc: src.assoc };
            let mut buf: SigBuf<A> = SigBuf::new_in(self.alloc.clone());
            let tys = self.ck.types.view();
            if read_impl_signature(t, imp_text, tys, &[v.ty.ty], v.ty.ty, &adt_lifetimes, &mut buf, self.alloc.clone()).is_err() {
                return self.refuse(refused, e);
            }
            let n = buf.ends.first().copied().unwrap_or(0);
            if n != info.len || buf.bounds.iter().any(|&(a, b)| a == Region::Static || b == Region::Static) {
                return self.refuse(refused, e);
            }
            for k in 0..n {
                match buf.regions[k as usize] {
                    Region::Static => {
                        let here = self.origs[(info.start + k) as usize];
                        let there = self.instance(&mut inst, Region::Static);
                        self.b.subset(here, there);
                    }
                    // One of `T`'s lifetimes: the hub relates it (the module doc).
                    Region::SelfTy(_) => {}
                    // An impl's `type` text elides nothing (E0106) and names only its own
                    // lifetimes: anything else was not read as this file assumes.
                    Region::Named(_) | Region::Anon(_) | Region::Hidden(_) | Region::Bound(_) => return self.refuse(refused, e),
                }
            }
        }
        Ok(HiddenCall::Coarse)
    }
}
