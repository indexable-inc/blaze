//! Step 1 of an `.await` whose future is not its operand, `fut = into_future(op)`, and
//! awaited-value shadow lowering, both routed through D156's unified `one_call.rs` seam.
//!
//! The type check selects the operand's `IntoFuture` impl with the body's solver and records
//! that impl's `into_future` at the impl's arguments as the `.await`'s callee
//! (blaze-typeck rules/await_impl.rs); a parameter's bound keeps the trait's `into_future`. No
//! head at all (a record without the row) is refused by name (`Form::AwaitOperand`): never
//! related as an unknown call by default.
//!
//! The Reference, "Await expressions": "Create a future by calling `IntoFuture::into_future` on
//! the future operand"; the head's own signature (`fn into_future(self) -> Self::IntoFuture`)
//! is what its call may relate.

use blaze_borrow::{Columns, RegionTypes, SlotCount};
use blaze_flow::{Local, Place};
use core::alloc::Allocator;

use blaze_typeck::{ExprId, ItemKey, List, Name, PNode, PTy};

use crate::one_call::FactsArity;
use crate::{AwaitUnrelated, Facts, Form, Lower, Refusal, Region, SigLife, TyD, Val};

impl<'b, 'c, F: Facts, A: Allocator + Copy, R: crate::one_call::CallRule> Lower<'b, 'c, F, A, R> {
    /// Relates `dest = into_future(v)` (the call statement just added) through the callee the
    /// record names at the `.await` `e` via D156's unified [`crate::one_call::CallRule`] seam.
    pub(crate) fn relate_into_future(&mut self, v: Val, dest: Place, dest_ty: TyD, e: ExprId) -> Result<(), Refusal> {
        let Some((item, g)) = self.callee_of(e) else {
            return self.refuse(Form::AwaitOperand, e);
        };
        // The witness lowering (one_call.rs `Erased`): the call statement is made; nothing is
        // related and no head is read.
        if !R::RELATES {
            return Ok(());
        }
        let vs = [v];
        let out = self.info(dest);
        let site = crate::one_call::CallSite { e, vs: &vs, ins: &[], tp: dest, ty: dest_ty, out, callee: Some((item, g)) };
        R::call(self, &site)
    }

    /// The call `site` (its destination the local `t`) is the operand of the innermost
    /// `.await` being lowered: makes the local whose positions are the `.await`'s value, and,
    /// where the callee's written return type relates them to the arguments
    /// ([`Self::relate_awaited`]), carries them on the future to the poll (coroutine.rs
    /// `await_done` relates the poll's result to them).
    ///
    /// The rule (the Reference, "Await expressions" and "Async functions"; RFC 2394): the value
    /// of `f(x).await` for `async fn f<'a, P>(x: &'a mut S<P>) -> W<P>` is a `W<P>`, its
    /// regions those `W<P>` names at the call's instantiation, never `'a`: the future holds the
    /// borrow of `x` until it is done, and the value it yields does not. Relating the poll's
    /// result to the future's positions (an opaque type is ONE position here, lib.rs `npos`,
    /// holding every capture) gave the value the argument's borrow: a later use of what was
    /// borrowed then conflicted with it (tempo tip20_channel_reserve.rs:229,
    /// `ChannelEnv::open(&mut payer, ..).await`). The relation is made HERE, at the call's own
    /// statement, where the arguments' origins are live; the local is never used, so the
    /// call's destination, then the future, mention its origins until the poll (origin
    /// liveness is per local, blaze-flow loans.rs `facts`).
    ///
    /// Both lowerings make the local (the oracle compares locals); only the one that relates
    /// regions relates it. A future not its own `IntoFuture` (an impl's `into_future`) is left
    /// to `await_done`, which sees it by itself. A callee value, or a head whose written return
    /// type does not relate, is recorded with why (`AwaitUnrelated`). `await_done` then refuses
    /// the `.await` by that name when its value carries a lifetime (rule 3).
    pub(crate) fn awaited_shadow(&mut self, site: &crate::one_call::CallSite<'_>, t: Local) -> Result<(), Refusal> {
        let Some(&(_, ae)) = self.awaiting.last() else { return Ok(()) };
        // `await_done` refuses an `.await` with no row by name; the identity only here.
        let Some(held) = self.ck.awaited_of(ae) else { return Ok(()) };
        if site.ty.wrap.is_some() || held != site.ty.ty {
            return Ok(());
        }
        let out = TyD::of(self.expr_ty(ae)?);
        let w = self.new_local_with(out, false, None, site.e)?;
        if !R::RELATES {
            return Ok(());
        }
        let Some((item, g)) = site.callee else {
            self.awaited.push((site.tp, Err((AwaitUnrelated::NoHead, None))));
            return Ok(());
        };
        let wp = self.place_of(w);
        if let Some(why) = self.relate_awaited(item, g, site.vs, wp, out, site.e)? {
            self.awaited.push((site.tp, Err((why, Some(item)))));
            return Ok(());
        }
        let (start, n) = self.local_origs[w.0 as usize];
        for k in start..start + n {
            let o = self.origs[k as usize];
            self.b.mentions(t.0, o);
        }
        self.awaited.push((site.tp, Ok(w)));
        Ok(())
    }

    /// Relates `dest`, of the awaited value's type, to the call's arguments `vs` through the
    /// written return type of `item`'s head (`SigRegions::written_output`), or else its opaque
    /// output's `Output = T` binding ([`Self::opaque_output`]), at the call's instantiation `g`
    /// via D156's unified `call_one`. `None` when related; else why nothing was related (the
    /// head is not an `async fn`'s, is not read, or does not line up), one [`AwaitUnrelated`]
    /// per exit.
    fn relate_awaited(&mut self, item: ItemKey, g: List, vs: &[Val], dest: Place, dest_ty: TyD, e: ExprId) -> Result<Option<AwaitUnrelated>, Refusal> {
        let facts: &'b F = self.facts;
        let Ok(life) = facts.sig(item) else { return Ok(Some(AwaitUnrelated::HeadUnread)) };
        let (written, slots) = match life.regions().written_output {
            Some(c) => (c.ty(), c.as_slice()),
            None => match self.opaque_output(life, e)? {
                Ok(w) => w,
                Err(why) => return Ok(Some(why)),
            },
        };
        let out = self.info(dest);
        let site = crate::one_call::CallSite { e, vs, ins: &[], tp: dest, ty: dest_ty, out, callee: Some((item, g)) };
        let output_override = Some(blaze_borrow::Typed { ty: written, col: slots });
        match self.call_one(&site, output_override, false, true) {
            Ok(()) => Ok(None),
            Err(r @ Refusal { form: Form::Malformed | Form::Bound | Form::Builder, .. }) => Err(r),
            Err(_) => Ok(Some(AwaitUnrelated::SignatureUnaligned)),
        }
    }

    /// The awaited value of a call of head `life` whose output is an opaque type, `fn f(..) ->
    /// impl Future<Output = T>` (a trait's method too: reth's `EthBlocks::rpc_block_header`,
    /// jsonrpsee's generated `fn uninstall_filter(..) -> impl Future<Output = RpcResult<bool>>`,
    /// tempo's `ConsensusFeed::get_latest`): `T`'s type (the head records' table) and its slots
    /// in the fn's numbering. The Reference, "Await expressions": the value is the future's
    /// `Future::Output`, which for an opaque type is what its bound `Output = T` says ("Impl
    /// trait": only the bounds are known of the hidden type), at the call's instantiation; the
    /// lifetimes the opaque captures and `T` does not name are not in it.
    ///
    /// Read from the opaque's own head: the `Output` binding clause with the opaque as `Self`
    /// (its column laid over `ExProj { args: [Self], term: T }`, children `Self` then `T`). Its
    /// columns are in the fn's numbering (blaze-ladder heads.rs `opaque_filed`: "its clause
    /// columns are in the fn's numbering"), which is checked, not assumed: the clause's `Self`
    /// child must begin with the signature's own slots of the opaque (its captures). Else why
    /// not, by name.
    ///
    /// UNSURE: the clause's trait is not compared with `Future` (no lang item in the facts):
    /// exactly one `Output` binding on the opaque is required instead, and the caller lays `T`
    /// over the `.await`'s type, which refuses a type that does not fit.
    fn opaque_output(&self, life: SigLife<'b>, e: ExprId) -> Result<Result<(PTy, &'b [Region]), AwaitUnrelated>, Refusal> {
        let facts: &'b F = self.facts;
        let out_ty = self.sig_declared(life, life.inputs(), e)?;
        let PNode::Opaque(k, _) = self.head_node(out_ty, e)? else {
            return self.dyn_future_output(life, out_ty, e);
        };
        let regions = facts.regions();
        let (Ok(captures), Ok(clauses)) = (regions.opaque_captures(k), regions.clauses(k)) else {
            return Ok(Err(AwaitUnrelated::OpaqueOutputUnread { opaque: k }));
        };
        let Some(own) = life.output().and_then(|o| o.get(..captures as usize)) else {
            return Ok(Err(AwaitUnrelated::OpaqueOutputUnaligned { opaque: k }));
        };
        let mut found: Option<(PTy, &'b [Region])> = None;
        for cv in clauses.iter() {
            let PNode::ExProj { name, args, term, .. } = self.head_node(cv.col.ty(), e)? else { continue };
            if name != Name::of("Output") {
                continue;
            }
            let args = self.head_list(args, e)?;
            let [me] = args else { continue };
            if !matches!(self.head_node(*me, e)?, PNode::Opaque(m, _) if m == k) {
                continue;
            }
            if found.is_some() {
                return Ok(Err(AwaitUnrelated::OpaqueOutputUnread { opaque: k }));
            }
            let me_slots = cv.col.child(0);
            let term_slots = cv.col.child(1);
            let aligned = cv.binders == 0 && cv.col.children() == 2 && me_slots.and_then(|s| s.get(..own.len())) == Some(own);
            match term_slots {
                Some(t) if aligned => found = Some((term, t)),
                _ => return Ok(Err(AwaitUnrelated::OpaqueOutputUnaligned { opaque: k })),
            }
        }
        Ok(found.ok_or(AwaitUnrelated::OpaqueOutputUnread { opaque: k }))
    }

    /// The awaited value of a sync call whose return type is a trait-object future wrapped in
    /// transparent pointer layers (`Pin<Box<dyn Future<Output = T> + Send + 'a>>`,
    /// `Box<dyn Future<Output = T> + 'a>`, `Pin<&mut dyn Future<Output = T>>`, or
    /// `&mut dyn Future<Output = T>`): extracts `T` and its exact slot slice from the
    /// `ExProj { name: "Output", args: [], term: T }` predicate inside `PNode::Dynamic`, so the
    /// awaited value carries only the lifetimes `T` names and never the future object's `'a`.
    fn dyn_future_output(
        &self,
        life: SigLife<'b>,
        out_ty: PTy,
        e: ExprId,
    ) -> Result<Result<(PTy, &'b [Region]), AwaitUnrelated>, Refusal> {
        let Some(mut cur_slots) = life.output() else {
            return Ok(Err(AwaitUnrelated::NoWrittenReturn));
        };
        let facts: &'b F = self.facts;
        let arity = FactsArity { facts };
        let heads = Columns::new(facts.region_tys().unwrap_or(self.tys), &arity, &arity);
        let mut cur_ty = out_ty;
        for _ in 0..4 {
            match self.head_node(cur_ty, e)? {
                PNode::Dynamic(preds) => {
                    let Ok(own) = heads.own(cur_ty) else { return Ok(Err(AwaitUnrelated::NoWrittenReturn)) };
                    let Some(mut rest) = cur_slots.get(own.len()..) else { return Ok(Err(AwaitUnrelated::NoWrittenReturn)) };
                    let mut found: Option<(PTy, &'b [Region])> = None;
                    for &pred in self.head_list(preds, e)? {
                        let Ok(pc) = heads.count(pred) else { return Ok(Err(AwaitUnrelated::NoWrittenReturn)) };
                        let Some((pslots, next_rest)) = rest.split_at_checked(pc.len()) else {
                            return Ok(Err(AwaitUnrelated::NoWrittenReturn));
                        };
                        rest = next_rest;
                        if let PNode::ExProj { name, args, term, .. } = self.head_node(pred, e)?
                            && name == Name::of("Output")
                        {
                            if !self.head_list(args, e)?.is_empty()
                                || heads.own(pred).ok() != Some(SlotCount::ZERO)
                                || found.is_some()
                            {
                                return Ok(Err(AwaitUnrelated::NoWrittenReturn));
                            }
                            let Ok(tc) = heads.count(term) else { return Ok(Err(AwaitUnrelated::NoWrittenReturn)) };
                            if pslots.len() != tc.len() {
                                return Ok(Err(AwaitUnrelated::NoWrittenReturn));
                            }
                            found = Some((term, pslots));
                        }
                    }
                    return Ok(found.ok_or(AwaitUnrelated::NoWrittenReturn));
                }
                PNode::Ref(_, inner) => {
                    let Ok(own) = heads.own(cur_ty) else { return Ok(Err(AwaitUnrelated::NoWrittenReturn)) };
                    let Some(next) = cur_slots.get(own.len()..) else { return Ok(Err(AwaitUnrelated::NoWrittenReturn)) };
                    cur_ty = inner;
                    cur_slots = next;
                }
                PNode::Adt(k, args) => {
                    let Ok(own) = heads.own(cur_ty) else { return Ok(Err(AwaitUnrelated::NoWrittenReturn)) };
                    if own != SlotCount::ZERO {
                        return Ok(Err(AwaitUnrelated::NoWrittenReturn));
                    }
                    let list = self.head_list(args, e)?;
                    let is_box = facts.adt(k).is_some_and(|a| a.is_box);
                    // Only peel `Pin<P>` (single-param zero-lifetime wrapper) or `Box<T, A>`
                    // (`is_box`); `call_one` still checks `term` against the `.await` expr's type.
                    if (is_box && !list.is_empty()) || list.len() == 1 {
                        let inner = list[0];
                        let Ok(ic) = heads.count(inner) else { return Ok(Err(AwaitUnrelated::NoWrittenReturn)) };
                        let Some(next) = cur_slots.get(..ic.len()) else { return Ok(Err(AwaitUnrelated::NoWrittenReturn)) };
                        cur_ty = inner;
                        cur_slots = next;
                    } else {
                        return Ok(Err(AwaitUnrelated::NoWrittenReturn));
                    }
                }
                _ => return Ok(Err(AwaitUnrelated::NoWrittenReturn)),
            }
        }
        Ok(Err(AwaitUnrelated::NoWrittenReturn))
    }
}
