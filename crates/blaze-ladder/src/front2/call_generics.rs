//! Each callee's declared type-outlives bounds (`T: 'a`, `T: 'static`, and whether its
//! container's header declares one): with BR4's generic shape (blaze-flow-lower
//! `read_shape`, filed by borrows.rs as `shapes`), what blaze-flow-lower's
//! `call_by_generic_signature` needs to relate a call whose generic arguments carry lifetimes
//! or whose callee's signature WRITES one (BR3 and BR4; flow.md "Census 3": 189 bstd bodies
//! refused `CallWritesLifetimes`). Without a row here no call takes that path.
//!
//! Read from what the borrow check already holds, never lowered again:
//! - the fn's own `T: 'a` from the lifetime part `read_signature` read from its text
//!   ([`SigBuf::type_outlives`]: the same reading, so a lifetime has the same `Named` number
//!   as in the signature's regions), each name resolved through the head's `params` (the
//!   names its signature was read with, the container's then its own, `Param` indices);
//!   a name that is not a parameter, or a bound on something other than a bare name, makes
//!   every parameter `unread_below`;
//! - whether the container's header declares any type-outlives bound
//!   (`container_declares_type_outlives` over the container's item text): if it does, the
//!   container's parameters (`Param(0..parent)`) are `unread_below`: a call whose argument for
//!   one of them carries a lifetime stays refused, by name (`CallOutlivesUnread`).
//!
//! - which parameters carry a trait bound (the head's `bounds`): the body's own row says
//!   which of its type parameters' outlives are not fully read (blaze-flow-lower
//!   param_outlives.rs: a failure there is a refusal, not E0310). The body's own row is
//!   asked too: `Facts::call_bounds(body.owner)` gives a caller's declared `U: 'r`.
//!
//! RULE 21: the borrow verdict now reads these, so each callee's answer is a read row of the
//! body ([`call_bounds_read`]: `FactKind::BorrowHead` with `detail` [`DETAIL`], beside the row
//! borrow_reads.rs makes with detail 0): the digest of the resolved bounds and
//! `unread_below`, or why none was filed. The bounds and the container's answer come from text
//! the detail-0 row does not digest, so this row is what moves when a callee gains
//! `T: 'static` or its impl gains `where T: 'a`. (The shape is a function of the head's types,
//! a row of the type check's list.)

use blaze_flow_lower::{CallBounds, Region, SigBuf};
use blaze_own::{AVec, Alloc};
use blaze_typeck as tc;
use blaze_typeck::StableHasher;

use super::heads::FnHead;
use super::reads::{Fact, FactKind, Read};

/// The `detail` of this module's `BorrowHead` rows (borrow_reads.rs files detail 0).
pub const DETAIL: u64 = 1;

/// The domain of this module's row answers.
/// v2 (lane B15): a fn pointer in a bound's types has positions and a lifetime its binder
/// binds is `Region::Bound` (tag 5).
const DOMAIN: &str = "blaze-ladder front2 borrow call bounds read v2";

/// Why no bounds were filed for a callee: its calls through `call_by_generic_signature` are
/// not made, and stay refused (`CallWritesLifetimes`) or on `unify_call`'s one origin.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CallBoundsRefusal {
    /// The store made no head for it.
    NoHead,
    /// Its signature's text was not read (`read_signature` refused it).
    SigNotRead,
    /// It is a member whose container's text is not known, or whose header did not lex.
    ContainerText,
}

/// One callee's bounds, owned.
pub struct CallBoundsBuf<'a> {
    outlives: AVec<'a, (u32, Region)>,
    unread_below: u32,
    /// The parameters with a trait bound (the head's `bounds`, the container's and its own),
    /// sorted, once each: what such a bound implies on their lifetimes is not read.
    trait_bounded: AVec<'a, u32>,
}

/// Every callee's, sorted by checker key, and the rows read for them.
pub struct CallBoundsTable<'a> {
    rows: AVec<'a, (u128, CallBoundsBuf<'a>)>,
    reads: AVec<'a, Read>,
}

impl<'a> CallBoundsTable<'a> {
    pub fn new_in(alloc: Alloc<'a>) -> CallBoundsTable<'a> {
        CallBoundsTable { rows: AVec::new_in(alloc), reads: AVec::new_in(alloc) }
    }

    /// The callee `item`'s, if filed.
    pub fn get(&self, item: tc::ItemKey) -> Option<CallBounds<'_>> {
        let at = self.rows.binary_search_by_key(&item.0, |(k, _)| *k).ok()?;
        self.rows.get(at).map(|(_, b)| CallBounds { outlives: &b.outlives, unread_below: b.unread_below, trait_bounded: &b.trait_bounded })
    }

    /// One `BorrowHead` row (detail [`DETAIL`]) per callee asked, for the body's read list.
    pub fn reads(&self) -> &[Read] {
        &self.reads
    }

    /// Files the callee `k`'s answer (and its read row). `head`: its head, if made; `sig`: the
    /// lifetime part read from its text, if read; `container`: `None` for a free fn, else the
    /// container's item text if known. Call in ascending `k`.
    pub fn gather(&mut self, k: u128, head: Option<&FnHead<'_>>, sig: Option<&SigBuf<Alloc<'a>>>, container: Option<Option<&str>>, alloc: Alloc<'a>) {
        let made = match (head, sig) {
            (None, _) => Err(CallBoundsRefusal::NoHead),
            (_, None) => Err(CallBoundsRefusal::SigNotRead),
            (Some(h), Some(s)) => make(h, s, container, alloc),
        };
        self.reads.push(call_bounds_read(k, made.as_ref().map_err(|e| *e)));
        if let Ok(buf) = made
            && self.rows.last().is_none_or(|(last, _)| *last < k)
        {
            self.rows.push((k, buf));
        }
    }
}

fn make<'a>(h: &FnHead<'_>, sig: &SigBuf<Alloc<'a>>, container: Option<Option<&str>>, alloc: Alloc<'a>) -> Result<CallBoundsBuf<'a>, CallBoundsRefusal> {
    let mut unread_below = 0u32;
    if let Some(text) = container {
        let text = text.ok_or(CallBoundsRefusal::ContainerText)?;
        if blaze_flow_lower::sig::container_declares_type_outlives(text, alloc).map_err(|_| CallBoundsRefusal::ContainerText)? {
            unread_below = h.parent;
        }
    }
    let mut outlives: AVec<'a, (u32, Region)> = AVec::new_in(alloc);
    for &(name, r) in sig.type_outlives.iter() {
        match h.params.iter().find(|(n, _)| *n == name) {
            Some(&(_, i)) => outlives.push((i, r)),
            // A bound on a name that is not a parameter in scope: not resolved, so every
            // parameter is under it as far as this knows.
            None => unread_below = u32::MAX,
        }
    }
    if sig.type_outlives_unread {
        unread_below = u32::MAX;
    }
    outlives.sort_unstable();
    let mut trait_bounded: AVec<'a, u32> = AVec::new_in(alloc);
    for b in h.bounds.iter() {
        trait_bounded.push(b.param);
    }
    trait_bounded.sort_unstable();
    trait_bounded.dedup();
    Ok(CallBoundsBuf { outlives, unread_below, trait_bounded })
}

fn region(h: &mut StableHasher, r: Region) {
    let (tag, n) = match r {
        Region::Static => (0, 0),
        Region::Named(n) => (1, n),
        Region::Anon(n) => (2, n),
        Region::SelfTy(n) => (3, n),
        Region::Hidden(n) => (4, n),
        Region::Bound(n) => (5, n),
    };
    h.tag(tag);
    h.u32(n);
}

/// The row of what the borrow check took about the callee `item`'s type-outlives bounds.
pub fn call_bounds_read(item: u128, made: Result<&CallBoundsBuf<'_>, CallBoundsRefusal>) -> Read {
    let mut h = StableHasher::new(DOMAIN);
    match made {
        Ok(b) => {
            h.tag(1);
            h.u32(b.outlives.len() as u32);
            for &(p, r) in b.outlives.iter() {
                h.u32(p);
                region(&mut h, r);
            }
            h.u32(b.unread_below);
            h.u32(b.trait_bounded.len() as u32);
            for &p in b.trait_bounded.iter() {
                h.u32(p);
            }
        }
        Err(why) => {
            h.tag(2);
            h.tag(match why {
                CallBoundsRefusal::NoHead => 0,
                CallBoundsRefusal::SigNotRead => 1,
                CallBoundsRefusal::ContainerText => 2,
            });
        }
    }
    Read { fact: Fact { kind: FactKind::BorrowHead, key: item, detail: DETAIL }, answer: Some(h.finish()) }
}
