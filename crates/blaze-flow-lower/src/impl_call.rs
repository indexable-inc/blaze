//! A call of a head with a hidden input (`fn take<T: Tr>(x: T::Out)`) whose type arguments
//! carry origins but name none of the caller's generics (`take::<Wrap<'_>>(&s)`): the hidden
//! input is read through the impl the TYPE CHECKER selected for the projection at this call
//! (`Checked::selected`, piece impl-per-call), never selected again here (rule 77), and its
//! `type Out = ..;` value's regions are the impl's head record's (`RegionFacts::impl_assoc`,
//! D148): no impl text is read here.
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
//! `T`'s position there: the impl's column numbers it `Region::Param(i)`, a slot of the
//! impl's self type column. "Lifetime bounds":
//! `'static` "outlives all other lifetimes".
//!
//! WHAT IS RELATED, and why it is sound:
//! - a `'static` at a hidden position: the argument's origin there must outlive `'static`
//!   (EXACT: the demand the coarse path missed, E0597 for `&local`);
//! - a `Param(i)` (one of `T`'s lifetimes), EXACTLY when the call is CLOSED (below): the
//!   whole call goes by the signature, the hidden input laid as the impl's value column with
//!   each impl lifetime one fresh region of the call ([`Lower::call_laid`]): `arg: T_k`, and
//!   `T_k` free, since `T` appears nowhere else the call relates.
//! - otherwise left to the call's older path, which `call_done` takes next: the type
//!   arguments carry origins, so it is the one-hub relation (lib.rs `unify_call`), a
//!   SUPERSET of `arg: T_k` with `T_k` related to `T`'s other occurrences, noted as stricter
//!   (`Form::CallGenericUnaligned` in `Lowered::coarse`).
//!
//! CLOSED: every hidden position is an input's (or the output's, into a destination of no
//! position: nothing is related there), each read through a selected impl row as above, and
//! the expanded column lines up with every operand and the destination ([`Lower::call_laid`]
//! counts them). `T` (a type argument with origins) appearing anywhere else in the signature
//! (`t: T`, `-> Vec<T>`) is laid there as no position while the operand or destination holds
//! `T`'s, so such a call never lines up and keeps the older path: the impl lifetimes' fresh
//! regions are then tied to nothing but the hidden inputs, which is what `T`'s lifetimes are
//! when it appears only there (one hidden row's impl lifetimes are its own regions: two rows
//! of two projections are two type arguments' lifetimes, related by nothing the call says).
//! A callee whose bounds tie trait lifetimes (`Facts::ties` rows) keeps the older path too:
//! those rows are tied through type arguments that hold no position (trait_ties.rs).
//! CONTROL (rule 23): `impl_call_tests.rs` `a_static_impl_type_still_errors` was refused
//! `CallGenericUnaligned { lacks: Shape }` (the older path's note) with only the demand path.
//! - an impl whose header bounds a mapped lifetime by `'static` (`impl Tr for Wrap<'static>`,
//!   or `impl<'a: 'static>`: a `'static` slot in its self type column or a `'static` in its
//!   declared outlives) is REFUSED by name: the demand falls on `T`'s positions, which this
//!   path holds no origins for.
//!
//! WHICH ROW: the checker's rows at a call are in the order the signature was lowered (inputs
//! left to right, then the output), one per projection; the head record's column laid as the
//! flow's positions numbers hidden positions in the same order (hidden_call.rs `LaidSig`, D148).
//! Row `h` is used only when the call's row count
//! equals its hidden position count; else (an opaque type among them, a projection nested in
//! another's arguments) the call is refused by name. A hidden input must be the WHOLE parameter
//! (`x: T::Out`, not `x: Vec<T::Out>`): its erased type is then the argument's own type.
//!
//! Keys (rule 21): the rows are a column of the caller's checked record; the impl's regions
//! are its head record's, whose digest is in the head rows the body files (D148).

use blaze_flow::{Origin, Place};
use blaze_own::Vec;
use blaze_typeck::{ExprId, ItemKey, Selected};
use core::alloc::Allocator;

use crate::hidden_call::{HiddenCall, HiddenWhy, LaidSig};
use crate::{At, Facts, Form, Laid, Lower, Refusal, Region, SigLife, TyD, Val};

impl<'b, 'c, F: Facts, A: Allocator + Copy, R: crate::one_call::CallRule> Lower<'b, 'c, F, A, R> {
    /// A call of `item` (its column `life`, laid as `laid`, holding a hidden position) whose
    /// type arguments carry origins and name none of the caller's generics: each hidden input
    /// whose argument carries origins is read through the selected impl (the module doc).
    /// `BySignature`: the call is CLOSED and related whole by its column, the hidden inputs
    /// laid as the impls' values. `Coarse`: the `'static` demands are in; the caller relates
    /// the rest by its older path.
    #[expect(clippy::too_many_arguments, reason = "the call's parts as call_through_hidden holds them")]
    pub(crate) fn call_hidden_by_impl(
        &mut self,
        item: ItemKey,
        life: SigLife<'_>,
        laid: &LaidSig<A>,
        vs: &[Val],
        dest: Place,
        dest_ty: TyD,
        e: ExprId,
    ) -> Result<HiddenCall, Refusal> {
        let rows = self.ck.selected_at(e);
        // The checker's rows are one per hidden position in the laid order (`LaidSig`); a call
        // whose counts differ (an opaque type among them) is refused at its hidden input below.
        let numbered = rows.len() == laid.hidden();
        let facts: &'b F = self.facts;
        let mut inst: Vec<(Region, Origin), A> = Vec::new_in(self.alloc);
        // The CLOSED call's column (the module doc): each input's laid regions, a hidden
        // position replaced by its impl value's column, each impl lifetime renumbered past
        // every region the callee's column and bounds name (`next`), one block per row.
        let mut closed = facts.ties(item).is_none_or(|t| t.rows.is_empty());
        let mut wide: Vec<Region, A> = Vec::new_in(self.alloc);
        let mut ends: Vec<u32, A> = Vec::new_in(self.alloc);
        let named = life.slots().iter().chain(life.bounds().iter().flat_map(|(a, b)| [a, b]));
        let mut next = named.filter_map(|r| match *r {
            Region::Param(i) => i.checked_add(1),
            _ => None,
        }).max().unwrap_or(0);
        // The number of hidden positions before input `i`'s: its first hidden one's row.
        let mut before = 0usize;
        for (i, v) in vs.iter().enumerate() {
            let Some(part) = laid.input(i) else {
                return self.refuse(Form::CallHiddenInput { callee: item, input: i as u32, why: HiddenWhy::NoInputPart }, e);
            };
            let input = i as u32;
            let refused = |why| Form::CallHiddenInput { callee: item, input, why };
            let first = before;
            before += part.iter().filter(|r| r.is_none()).count();
            if !part.iter().any(Option::is_none) {
                wide.extend(part.iter().flatten().copied());
                ends.push(wide.len() as u32);
                continue;
            }
            let h = first;
            // An operand whose positions owe no demand (a constant, a place of none): the
            // demand path skips it; the closed column still needs its hidden part laid.
            let owes = match v.at {
                At::Place(p) => self.info(p).len > 0,
                At::Const => false,
            };
            if !owes {
                let n = match v.at {
                    At::Place(p) => self.info(p).len,
                    At::Const => self.npos_d(v.ty, e)?,
                };
                if n == 0 && part.len() == 1 {
                    // `x: T::Out` given a value of no position: the hidden part is none.
                    ends.push(wide.len() as u32);
                } else {
                    closed = false;
                }
                continue;
            }
            let At::Place(p) = v.at else { continue };
            let info = self.info(p);
            if !numbered || part.len() != 1 || !info.exact || v.ty.wrap.is_some() {
                return self.refuse(refused(HiddenWhy::RowsUnnumbered), e);
            }
            let Some(row) = rows.get(h).copied() else { return self.refuse(refused(HiddenWhy::NoImplRow), e) };
            let Selected::Impl { imp, .. } = row.selected else { return self.refuse(refused(HiddenWhy::NoImplRow), e) };
            let Some(name) = row.name else { return self.refuse(refused(HiddenWhy::NoImplRow), e) };
            // The impl's head record (D148): its self type's column, its declared outlives,
            // and the `type name` value's column, all numbered as the impl's lifetimes.
            let regions = facts.regions();
            let Ok((self_col, _)) = regions.impl_header(imp) else { return self.refuse(refused(HiddenWhy::ImplUnread), e) };
            let Ok(params) = regions.lifetime_params(imp) else { return self.refuse(refused(HiddenWhy::ImplUnread), e) };
            let Ok(value) = regions.impl_assoc(imp, name) else { return self.refuse(refused(HiddenWhy::ImplUnread), e) };
            // A `'static` the header puts on `T`'s positions is a demand this path holds no
            // origins for (the module doc): refused by name.
            let header_static = self_col.as_slice().contains(&Region::Static) || params.outlives().iter().any(|&(a, b)| a == Region::Static || b == Region::Static);
            // The value's column is laid over its declared type (the impl's own table): it is
            // the argument's positions only when it has exactly as many slots.
            if header_static || value.len() != info.len {
                return self.refuse(refused(HiddenWhy::ImplUnaligned), e);
            }
            for (k, &r) in value.as_slice().iter().enumerate() {
                match r {
                    Region::Static => {
                        let here = self.origs[info.start as usize + k];
                        let there = self.instance(&mut inst, Region::Static);
                        self.b.subset(here, there);
                        wide.push(Region::Static);
                    }
                    // One of `T`'s lifetimes (E0207: a lifetime of the impl appears in its self
                    // type): a fresh region of the call when it is closed, else the hub (the
                    // module doc).
                    Region::Param(j) if self_col.as_slice().contains(&r) => match next.checked_add(j) {
                        Some(x) => wide.push(Region::Param(x)),
                        None => closed = false,
                    },
                    // Any other region was not laid as this file assumes: refused, never guessed.
                    Region::Param(_) | Region::Late { .. } => return self.refuse(refused(HiddenWhy::ImplRegionForeign), e),
                }
            }
            match next.checked_add(params.count()) {
                Some(x) => next = x,
                None => closed = false,
            }
            ends.push(wide.len() as u32);
        }
        // The output: closed only with no hidden position, or one into a destination of no
        // position (it relates nothing, hidden_call.rs `expand`'s `output_empty`).
        let d = self.info(dest);
        let output_empty = d.exact && d.len == 0 && laid.output().iter().all(Option::is_none);
        if laid.output().iter().any(Option::is_none) && !output_empty {
            closed = false;
        }
        if !closed {
            return Ok(HiddenCall::Coarse);
        }
        wide.extend(laid.output().iter().flatten().copied());
        // `call_laid` relates nothing and answers `false` when a part does not line up (`T`
        // laid elsewhere as no position: the module doc): the older path then.
        if self.call_laid(Laid { regions: &wide, ends: &ends, bounds: life.bounds() }, None, &[], vs, dest, dest_ty, e)? {
            self.stats.calls_hidden_static += 1;
            return Ok(HiddenCall::BySignature);
        }
        Ok(HiddenCall::Coarse)
    }
}
