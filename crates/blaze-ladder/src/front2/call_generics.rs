//! Each callee's declared type-outlives bounds (`T: 'a`, `T: 'static`, `where I::Item: 'a`):
//! with BR4's generic shape (blaze-flow-lower `read_shape`, filed by borrows.rs as `shapes`),
//! what blaze-flow-lower's `call_by_generic_signature` needs to relate a call whose generic
//! arguments carry lifetimes or whose callee's signature WRITES one (BR3 and BR4; flow.md
//! "Census 3"). Without a row here no call takes that path.
//!
//! D148 ("Lifetimes are produced once and carried beside the types"): read from what the head
//! lowering produced, never from text.
//! - the bounds are the head's clauses `PClause::TypeOutlives { ty, region }` (the fn's
//!   `Preds`, its own and its container's inherited, so an impl's `impl<T: 'static>` is read
//!   as the fn's own), the region in the head's one numbering (container's lifetimes first,
//!   then the fn's, then the elided: `regions::Numbering`), the same the signature's column
//!   writes. Each subject is read from the type: a bare `Param(p)`; the resolved Future's
//!   `Output` over a `Param(p)` with a Future bound; another projection over a `Param(p)` is
//!   kept as unread (`OutlivesType::UnreadProjection`); any other subject (`&'b U: 'static`)
//!   makes every parameter `unread_below`, so a call carrying a lifetime is refused by name;
//! - the head's region facts must be read (`RegionFacts::outlives` of the callee answers):
//!   every `Outlives::Type` row there names a clause of the list above with its region, and
//!   one that does not is a malformed head, refused ([`CallBoundsRefusal::Malformed`]). A
//!   head whose regions are not read files no bounds ([`CallBoundsRefusal::Regions`]), and its
//!   calls stay refused: an unknown is never an empty list (an outlives bound dropped accepts
//!   a dangling borrow);
//! - which parameters carry a trait bound (the head's `bounds`): the body's own row says
//!   which of its type parameters' outlives are not fully read (blaze-flow-lower
//!   param_outlives.rs: a failure there is a refusal, not E0310). The body's own row is
//!   asked too: `Facts::call_bounds(body.owner)` gives a caller's declared `U: 'r`.
//!
//! Before D148 the bounds came from the lifetime part `read_signature` read from the fn's
//! text (`SigBuf::type_outlives`) and the container's header text
//! (`container_declares_type_outlives`), whose bounds were never resolved: the container's
//! parameters were all `unread_below`. Both are gone.
//!
//! RULE 21: the borrow verdict reads these, so each callee's answer is a read row of the body
//! ([`call_bounds_read`]: `FactKind::BorrowHead` with `detail` [`DETAIL`]): the digest of the
//! resolved bounds, `unread_below` and the trait-bounded parameters, or why none was filed.

use blaze_flow_lower::{CallBounds, OutlivesType};
use blaze_own::{AVec, Alloc};
use blaze_typeck as tc;
use blaze_typeck::StableHasher;
use blaze_typeck::region_facts::{Outlives, RegionFacts, UnreadWhy};
use blaze_typeck::regions::Region;

use super::heads::FnHead;
use super::reads::{Fact, FactKind, Read};
use super::types::ParamBound;

/// The `detail` of this module's `BorrowHead` rows (borrow_reads.rs files detail 0).
pub const DETAIL: u64 = 1;

/// The domain of this module's row answers.
/// v4 (D148): bounds from the head's clauses and region facts, regions as
/// `regions::Region` (Static 0, Param 1, Late 2).
const DOMAIN: &str = "blaze-ladder front2 borrow call bounds read v4";

/// Why no bounds were filed for a callee: its calls through `call_by_generic_signature` are
/// not made, and stay refused (`CallWritesLifetimes`) or on `unify_call`'s one origin.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CallBoundsRefusal {
    /// The store made no head for it.
    NoHead,
    /// Its clauses were not filed (`Store::preds` holds no row for it).
    NoClauses,
    /// Its head's regions were not read (`RegionFacts::outlives` answered `RegionsUnread`).
    Regions(UnreadWhy),
    /// Its region facts name clause `clause` as a `T: 'r` that its clause list does not hold
    /// with that region: a malformed head.
    Malformed { clause: u32 },
    /// A subject's type could not be read from the head's type table.
    Table { clause: u32 },
}

/// One callee's bounds, owned.
pub struct CallBoundsBuf<'a> {
    outlives: AVec<'a, (OutlivesType, Region)>,
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

    /// Files the callee `k`'s answer (and its read row). `head`: its head, if made; `clauses`:
    /// its `Preds::clauses` (own and inherited), if filed, their types in `tys`; `regions`:
    /// the head records' region facts (head_record.rs `RegionTable::reader`); `future`: the
    /// `Future` lang item. Call in ascending `k`.
    #[expect(clippy::too_many_arguments, reason = "one callee's facts and the table's allocator")]
    pub fn gather(
        &mut self,
        k: u128,
        head: Option<&FnHead<'_>>,
        clauses: Option<&[tc::PClause]>,
        tys: tc::PTys<'_>,
        regions: &dyn RegionFacts,
        future: Option<blaze_id::ItemKey>,
        extra_static: &[u32],
        alloc: Alloc<'a>,
    ) {
        let made = match (head, clauses) {
            (None, _) => Err(CallBoundsRefusal::NoHead),
            (_, None) => Err(CallBoundsRefusal::NoClauses),
            (Some(h), Some(c)) => make(tc::ItemKey(k), &h.bounds, c, tys, regions, future, extra_static, alloc),
        };
        self.reads.push(call_bounds_read(k, made.as_ref().map_err(|e| *e)));
        if let Ok(buf) = made
            && self.rows.last().is_none_or(|(last, _)| *last < k)
        {
            self.rows.push((k, buf));
        }
    }
}

/// What the subject of a `T: 'r` is, as [`OutlivesType`]; `None`: no parameter's (then every
/// parameter is `unread_below`).
fn subject(tys: tc::PTys<'_>, ty: tc::PTy, bounds: &[ParamBound], future: Option<tc::ItemKey>, clause: u32) -> Result<Option<OutlivesType>, CallBoundsRefusal> {
    let table = |_| CallBoundsRefusal::Table { clause };
    Ok(match tys.node(ty).map_err(table)? {
        tc::PNode::Param(p) => Some(OutlivesType::Param(p)),
        tc::PNode::Alias { trait_, name, args } => {
            let first = tys.list(args).map_err(table)?.first().copied();
            let Some(first) = first else { return Ok(None) };
            match tys.node(first).map_err(table)? {
                tc::PNode::Param(p) => {
                    let is_future = Some(trait_) == future
                        && name == tc::Name::of("Output")
                        && bounds.iter().any(|b| b.param == p && Some(super::keys::checker_of(b.trait_)) == future);
                    Some(if is_future { OutlivesType::FutureOutput(p) } else { OutlivesType::UnreadProjection(p) })
                }
                _ => None,
            }
        }
        _ => None,
    })
}

#[expect(clippy::too_many_arguments, reason = "one callee's facts")]
fn make<'a>(
    item: tc::ItemKey,
    bounds: &[ParamBound],
    clauses: &[tc::PClause],
    tys: tc::PTys<'_>,
    regions: &dyn RegionFacts,
    future: Option<blaze_id::ItemKey>,
    extra_static: &[u32],
    alloc: Alloc<'a>,
) -> Result<CallBoundsBuf<'a>, CallBoundsRefusal> {
    let future = future.map(super::keys::checker_of);
    let declared = regions.outlives(item).map_err(|u| CallBoundsRefusal::Regions(u.why))?;
    // Every row the region facts give names a `T: 'r` clause of the list with its region.
    for o in declared.iter() {
        if let Outlives::Type { clause, region } = *o {
            match clauses.get(clause as usize) {
                Some(&tc::PClause::TypeOutlives { region: r, .. }) if r == region => {}
                _ => return Err(CallBoundsRefusal::Malformed { clause }),
            }
        }
    }
    let mut unread_below = 0u32;
    let mut outlives = AVec::new_in(alloc);
    // Every `T: 'r` of the list, the inherited too: none is dropped.
    for (n, c) in clauses.iter().enumerate() {
        let tc::PClause::TypeOutlives { ty, region } = *c else { continue };
        match subject(tys, ty, bounds, future, n as u32)? {
            Some(t) => outlives.push((t, region)),
            None => unread_below = u32::MAX,
        }
    }
    // Transitive supertrait `Self: 'static` bounds on the owner's type parameters
    // (`R: Relation` where `trait Relation: 'static`).
    for &p in extra_static {
        outlives.push((OutlivesType::Param(p), Region::Static));
    }
    outlives.sort_unstable();
    outlives.dedup();
    let mut trait_bounded: AVec<'a, u32> = AVec::new_in(alloc);
    for b in bounds.iter() {
        trait_bounded.push(b.param);
    }
    trait_bounded.sort_unstable();
    trait_bounded.dedup();
    Ok(CallBoundsBuf { outlives, unread_below, trait_bounded })
}

fn region(h: &mut StableHasher, r: Region) {
    match r {
        Region::Static => h.tag(0),
        Region::Param(n) => {
            h.tag(1);
            h.u32(n);
        }
        Region::Late { binder, var } => {
            h.tag(2);
            h.u32(binder);
            h.u32(var);
        }
    }
}

/// The row of what the borrow check took about the callee `item`'s type-outlives bounds.
pub fn call_bounds_read(item: u128, made: Result<&CallBoundsBuf<'_>, CallBoundsRefusal>) -> Read {
    let mut h = StableHasher::new(DOMAIN);
    match made {
        Ok(b) => {
            h.tag(1);
            h.u32(b.outlives.len() as u32);
            for &(subject, r) in b.outlives.iter() {
                h.tag(match subject {
                    OutlivesType::Param(_) => 0,
                    OutlivesType::FutureOutput(_) => 1,
                    OutlivesType::UnreadProjection(_) => 2,
                });
                h.u32(subject.param());
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
            match why {
                CallBoundsRefusal::NoHead => h.tag(0),
                CallBoundsRefusal::NoClauses => h.tag(1),
                CallBoundsRefusal::Regions(w) => {
                    h.tag(2);
                    match w {
                        UnreadWhy::NoHead => h.tag(0),
                        UnreadWhy::HeadRefused => h.tag(1),
                        UnreadWhy::RecordForm { found } => {
                            h.tag(2);
                            h.u32(found);
                        }
                        UnreadWhy::Refused { what } => {
                            h.tag(3);
                            super::borrow_reads::hash_text(&mut h, what);
                        }
                    }
                }
                CallBoundsRefusal::Malformed { clause } => {
                    h.tag(3);
                    h.u32(clause);
                }
                CallBoundsRefusal::Table { clause } => {
                    h.tag(4);
                    h.u32(clause);
                }
            }
        }
    }
    Read { fact: Fact { kind: FactKind::BorrowHead, key: item, detail: DETAIL }, answer: Some(h.finish()) }
}

#[cfg(test)]
mod tests {
    //! The bounds read from a head's clauses and region facts (D148), with planted controls.
    //! `g<T: 'static>(t: T)`: its one clause `T: 'static` gives `(Param(0), Static)`; a head
    //! whose regions are unread files nothing (never an empty list); a region-facts row that
    //! names a clause the list does not hold is refused.

    use super::*;
    use blaze_typeck::region_facts::{ClauseView, ColView, ParamsView, RegionsUnread, SigRegions};

    /// Region facts that answer `outlives` only.
    struct Facts<'r> {
        outlives: Result<&'r [Outlives], UnreadWhy>,
    }

    impl RegionFacts for Facts<'_> {
        fn sig(&self, item: tc::ItemKey) -> Result<SigRegions<'_>, RegionsUnread> {
            Err(RegionsUnread { item, why: UnreadWhy::NoHead })
        }
        fn clauses(&self, item: tc::ItemKey) -> Result<&[ClauseView<'_>], RegionsUnread> {
            Err(RegionsUnread { item, why: UnreadWhy::NoHead })
        }
        fn outlives(&self, item: tc::ItemKey) -> Result<&[Outlives], RegionsUnread> {
            self.outlives.map_err(|why| RegionsUnread { item, why })
        }
        fn field(&self, adt: tc::ItemKey, _: u32, _: u32) -> Result<ColView<'_>, RegionsUnread> {
            Err(RegionsUnread { item: adt, why: UnreadWhy::NoHead })
        }
        fn impl_header(&self, item: tc::ItemKey) -> Result<(ColView<'_>, Option<ColView<'_>>), RegionsUnread> {
            Err(RegionsUnread { item, why: UnreadWhy::NoHead })
        }
        fn opaque_captures(&self, item: tc::ItemKey) -> Result<u32, RegionsUnread> {
            Err(RegionsUnread { item, why: UnreadWhy::NoHead })
        }
        fn lifetime_params(&self, item: tc::ItemKey) -> Result<ParamsView<'_>, RegionsUnread> {
            Err(RegionsUnread { item, why: UnreadWhy::NoHead })
        }
    }

    const G: u128 = 0x67;

    fn a() -> Alloc<'static> {
        Alloc::global()
    }

    /// `fn g<T: 'static>(t: T)`'s bounds, read through `make` as `gather` reads them, with the
    /// region facts `facts`.
    fn g(facts: &Facts<'_>) -> Result<CallBoundsBuf<'static>, CallBoundsRefusal> {
        let mut tys = tc::PortableTys::new_in(a());
        let t = tys.intern(tc::PNode::Param(0)).unwrap();
        let clauses = [tc::PClause::TypeOutlives { ty: t, region: Region::Static }];
        make(tc::ItemKey(G), &[], &clauses, tys.view(), facts, None, &[], a())
    }

    #[test]
    fn static_bound_is_read_from_the_clause() {
        let rows = [Outlives::Type { clause: 0, region: Region::Static }];
        let b = g(&Facts { outlives: Ok(&rows) }).expect("g's bounds are filed");
        assert_eq!(&b.outlives[..], &[(OutlivesType::Param(0), Region::Static)]);
        assert_eq!(b.unread_below, 0);
    }

    /// CONTROL: the same head whose regions are not read files no bounds, so its calls stay
    /// refused (never an empty list, which would accept `g(&local)`); and its read row is the
    /// refusal's, not the bounds'.
    #[test]
    fn unread_regions_file_nothing() {
        let got = g(&Facts { outlives: Err(UnreadWhy::HeadRefused) });
        assert!(matches!(got, Err(CallBoundsRefusal::Regions(UnreadWhy::HeadRefused))));
        let rows = [Outlives::Type { clause: 0, region: Region::Static }];
        let filed = g(&Facts { outlives: Ok(&rows) }).expect("filed");
        assert_ne!(call_bounds_read(G, Ok(&filed)).answer, call_bounds_read(G, Err(CallBoundsRefusal::Regions(UnreadWhy::HeadRefused))).answer);
    }

    /// A region-facts row naming a clause the list does not hold with its region: malformed.
    #[test]
    fn a_row_naming_another_region_is_malformed() {
        let rows = [Outlives::Type { clause: 0, region: Region::Param(0) }];
        assert!(matches!(g(&Facts { outlives: Ok(&rows) }), Err(CallBoundsRefusal::Malformed { clause: 0 })));
    }

    /// A subject that is no parameter (`&'b U: 'static`) makes every parameter unread.
    #[test]
    fn a_non_param_subject_reads_every_parameter_unread() {
        let mut tys = tc::PortableTys::new_in(a());
        let u = tys.intern(tc::PNode::Param(0)).unwrap();
        let r = tys.intern(tc::PNode::Ref(tc::Mutbl::Not, u)).unwrap();
        let clauses = [tc::PClause::TypeOutlives { ty: r, region: Region::Static }];
        let b = make(tc::ItemKey(G), &[], &clauses, tys.view(), &Facts { outlives: Ok(&[]) }, None, &[], a()).expect("filed");
        assert_eq!(b.unread_below, u32::MAX);
        assert!(b.outlives.is_empty());
    }
}
