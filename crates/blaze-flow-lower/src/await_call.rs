//! Step 1 of an `.await` whose future is not its operand, `fut = into_future(op)`, related
//! through the head the type check recorded at the `.await` (piece await-intofuture).
//!
//! The type check selects the operand's `IntoFuture` impl with the body's solver and records
//! that impl's `into_future` at the impl's arguments as the `.await`'s callee
//! (blaze-typeck rules/await_impl.rs); a parameter's bound keeps the trait's `into_future`. So
//! the call is one of a KNOWN head, and is related as `call_done` relates any known head
//! (lib.rs), in the same order and by the same pieces: a declared type-outlives bound is
//! checked by the generic signature or refused by name; the head's lifetime part when it lines up
//! (`call_by_signature`), or instantiated with the generic arguments
//! (`call_by_generic_signature`, generic_call.rs); a head that writes lifetimes neither could
//! line up is refused by name (`Form::CallWritesLifetimes`: it may demand `'static`, which one
//! origin for all cannot say); else every position through one origin (`unify_call`), the
//! superset of any relation a signature that writes no lifetime can state. No head at all (a
//! record without the row) is refused by name (`Form::AwaitOperand`): never related as an
//! unknown call by default.
//!
//! The Reference, "Await expressions": "Create a future by calling `IntoFuture::into_future` on
//! the future operand"; the head's own signature (`fn into_future(self) -> Self::IntoFuture`)
//! is what its call may relate.

use blaze_flow::{Local, Place};
use core::alloc::Allocator;

use blaze_typeck::{ExprId, ItemKey, List, Name, PNode, PTy};

use crate::generic_call::{CallBounds, ShapeBuf};
use crate::{AwaitUnrelated, CallValue, Facts, Form, Lower, Plant, Refusal, Region, SigLife, TyD, Val};

impl<'b, 'c, F: Facts, A: Allocator + Copy, R: crate::one_call::CallRule> Lower<'b, 'c, F, A, R> {
    /// Relates `dest = into_future(v)` (the call statement just added) through the callee the
    /// record names at the `.await` `e` (the module doc).
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
        let facts: &'b F = self.facts;
        let sig = facts.callee(item);
        let mut generic_origins = false;
        for &a in self.list(g, e)? {
            generic_origins |= self.npos(a, e)? > 0;
        }
        // A head whose regions were not read (D148: `Facts::sig` is the one source): where the
        // operand or the future carries a lifetime, refused by name with why, as `call_done`
        // refuses any call of such a head; never related under a guess.
        if let Err(unread) = facts.sig(item)
            && self.plant != Plant::UnknownUnified
            && (self.npos_d(v.ty, e)? > 0 || self.info(dest).len > 0)
        {
            return self.refuse(Form::RegionsUnread { item: unread.item, why: unread.why }, e);
        }
        let owes = if facts.sig(item).is_ok() {
            match facts.call_bounds(item) {
                Some(b) => self.owes_type_outlives(b, g, e)?,
                None => self.owes_type_outlives(CallBounds { outlives: &[], unread_below: u32::MAX, trait_bounded: &[] }, g, e)?,
            }
        } else {
            None
        };
        if !generic_origins && owes.is_none()
            && let Ok(life) = facts.sig(item)
            && self.call_by_signature(life, Some((item, g)), &vs, dest, dest_ty, e)?
        {
            self.stats.calls_by_signature += 1;
            return Ok(());
        }
        if sig.is_some_and(|s| generic_origins || owes.is_some() || s.writes_lifetimes)
            && let Ok(life) = facts.sig(item)
            && let Some(shape) = facts.shape(item)
            && let Some(bounds) = facts.call_bounds(item)
            && self.call_by_generic_signature(item, life, shape, bounds, g, &vs, dest, dest_ty, e)?
        {
            self.stats.calls_by_signature += 1;
            return Ok(());
        }
        if let Some(param) = owes {
            return self.refuse(Form::CallOutlivesUnread { callee: item, param, why: crate::OutlivesUnreadWhy::Owed { lacks: self.call_lacks(item) } }, e);
        }
        if let Ok(life) = facts.sig(item)
            && !self.sig_laid(life, e)?
        {
            self.stats.calls_hidden += 1;
        }
        if sig.is_some_and(|s| s.writes_lifetimes) {
            return self.refuse(Form::CallWritesLifetimes { callee: item, lacks: self.call_lacks(item) }, e);
        }
        // A known head no signature related: the one origin, a STRICTER relation than its
        // signature's, noted as `Form::CallGenericUnaligned` (lib.rs `call_old`'s last arm,
        // `Lowered::coarse`): the body is refused by it only if it has a finding.
        if self.npos_d(v.ty, e)? > 0 || self.info(dest).len > 0 {
            self.coarse_step(Form::CallGenericUnaligned { callee: item, lacks: self.call_lacks(item) }, e);
        }
        self.unify_call(&vs, dest, e)?;
        self.stats.calls_unified += 1;
        Ok(())
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
    /// output's `Output = T` binding ([`Self::opaque_output`]), at the call's
    /// instantiation `g`, as `call_old` relates a call's output: by the head's lifetime part
    /// when no type argument carries an origin and no type-outlives bound is owed, else by its
    /// generic signature. `None` when related; else why nothing was related (the head is not
    /// an `async fn`'s, is not read, or does not line up), one [`AwaitUnrelated`] per exit.
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
        let value = CallValue::Awaited { ty: written, slots };
        let args = self.list(g, e)?;
        let mut generic_origins = false;
        for &a in args {
            generic_origins |= self.npos(a, e)? > 0;
        }
        let owes = match facts.call_bounds(item) {
            Some(b) => self.owes_type_outlives(b, g, e)?,
            None => self.owes_type_outlives(CallBounds { outlives: &[], unread_below: u32::MAX, trait_bounded: &[] }, g, e)?,
        };
        if !generic_origins && owes.is_none() {
            let related = self.call_by_signature_to(life, value, Some((item, g)), vs, dest, dest_ty, e)?;
            return Ok((!related).then_some(AwaitUnrelated::SignatureUnaligned));
        }
        let Some(bounds) = facts.call_bounds(item) else { return Ok(Some(AwaitUnrelated::NoBounds)) };
        // A type argument with positions under a bound not read: the generic path refuses it
        // by name. The call itself was related (or refused) by `call_old` already; its value
        // is refused by `await_done` under this name when it carries a lifetime.
        for i in 0..(bounds.unread_below as usize).min(args.len()) {
            if self.npos(args[i], e)? > 0 {
                return Ok(Some(AwaitUnrelated::BoundUnread { param: i as u32 }));
            }
        }
        let mut shape = ShapeBuf::new_in(self.alloc);
        if !self.awaited_shape(life, written, &mut shape, e)? {
            return Ok(Some(AwaitUnrelated::ShapeUnread));
        }
        let related = self.call_by_generic_signature_to(item, life, value, shape.shape(), bounds, g, vs, dest, dest_ty, e)?;
        Ok((!related).then_some(AwaitUnrelated::GenericUnaligned(self.misfit)))
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
            return Ok(Err(AwaitUnrelated::NoWrittenReturn));
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
}
