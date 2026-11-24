//! Each fn's trait bounds with lifetime arguments, as blaze-flow-lower's `Facts::ties` asks
//! them (piece RB8, blaze-flow-lower trait_ties.rs): a callee's tie its trait lifetimes to the
//! caller's regions at a call, the body's own say which of its regions answer them.
//!
//! D148 ("Lifetimes are produced once and carried beside the types"): read from what the head
//! lowering produced, never from text.
//! - which bounds there are: the head's clauses (`Preds::clauses`, own and inherited), each
//!   `PClause::Trait { trait_, args }` whose self type `args[0]` is a bare `Param(p)`;
//! - each bound's lifetime arguments: its clause's column (`RegionFacts::clauses`, the view
//!   whose `nth` is the clause's place), whose OWN slots are the trait reference's lifetime
//!   arguments in the trait's order, in the fn's one numbering (the same its signature's
//!   column writes). A `for<..>` the clause binds is `Region::Late` there (trait_ties.rs ties
//!   no such pair);
//! - each trait's lifetime count: `RegionFacts::lifetime_params` of the trait. A trait with
//!   none has no row. A column whose own slots are another count, two clauses for one
//!   `(param, trait)`, a clause with no column, or a head or trait whose regions are not read:
//!   the row is kept unread, never guessed;
//! - a trait method's implicit `Self: Trait<'params>` (param 0, the head's `SelfTy::Trait`):
//!   the trait's lifetimes are the container's group of the method's numbering, numbered
//!   first (`regions::Numbering`), so its row is `Param(0..count)`.
//!
//! Before D148 the trait's lifetimes came from its header text (`trait_header`) and each
//! bound's arguments from the fn's text (`SigBuf::trait_bounds`), matched by name; the
//! `assoc_read` answer generic_call.rs took for projections is gone too (the projection's
//! trait lifetimes are its column's slots).
//!
//! RULE 21: the borrow verdict reads these, so each fn's answer is a read row of the body
//! (`FactKind::BorrowHead`, detail [`DETAIL`], keyed by the fn), and each trait's lifetime
//! count one more (detail [`TRAIT_DETAIL`], keyed by the trait).

use blaze_flow_lower::{Tie, Ties};
use blaze_own::{AVec, Alloc};
use blaze_typeck as tc;
use blaze_typeck::StableHasher;
use blaze_typeck::region_facts::{RegionFacts, UnreadWhy};
use blaze_typeck::regions::Region;

use super::super::heads::FnHead;
use super::super::keys::checker_of;
use super::super::reads::{Fact, FactKind, Read};
use super::super::types::SelfTy;

/// The `detail` of a fn's ties row (call_generics.rs files 1; 2 and 3 were the deleted impl
/// and trait text rows, piece L6).
pub const DETAIL: u64 = 4;
/// The `detail` of a trait's lifetime-count row read for a tie.
pub const TRAIT_DETAIL: u64 = 5;

/// v3 (D148): rows from clause columns, regions as `regions::Region`.
const DOMAIN: &str = "blaze-ladder front2 borrow trait ties read v3";
const TRAIT_DOMAIN: &str = "blaze-ladder front2 borrow tie trait lifetimes read v3";

/// Every fn's rows, sorted by checker key, and the rows read for them.
pub struct TiesTable<'a> {
    /// `(fn, first row, end)` into `ties`.
    index: AVec<'a, (u128, u32, u32)>,
    ties: AVec<'a, Tie>,
    /// Every row's regions; a [`Tie`]'s `lo..hi` index here.
    regions: AVec<'a, Region>,
    reads: AVec<'a, Read>,
    /// The traits whose lifetime-count row is filed.
    traits: AVec<'a, u128>,
}

impl<'a> TiesTable<'a> {
    pub fn new_in(alloc: Alloc<'a>) -> TiesTable<'a> {
        TiesTable { index: AVec::new_in(alloc), ties: AVec::new_in(alloc), regions: AVec::new_in(alloc), reads: AVec::new_in(alloc), traits: AVec::new_in(alloc) }
    }

    /// Fn `item`'s rows, if filed.
    pub fn get(&self, item: tc::ItemKey) -> Option<Ties<'_>> {
        let at = self.index.binary_search_by_key(&item.0, |&(k, _, _)| k).ok()?;
        let (_, lo, hi) = self.index[at];
        Some(Ties { rows: self.ties.get(lo as usize..hi as usize)?, regions: &self.regions })
    }

    /// The read rows, for the body's read list.
    pub fn reads(&self) -> &[Read] {
        &self.reads
    }

    /// Files fn `k`'s rows and their read rows. `clauses`: its `Preds::clauses`, their types in
    /// `tys`; `regions`: the head records' region facts (head_record.rs `RegionTable::reader`).
    /// Call in ascending `k`.
    pub fn gather(&mut self, k: u128, head: Option<&FnHead<'_>>, clauses: Option<&[tc::PClause]>, tys: tc::PTys<'_>, regions: &dyn RegionFacts) {
        let (Some(h), Some(clauses)) = (head, clauses) else {
            let mut hasher = StableHasher::new(DOMAIN);
            hasher.tag(0);
            self.reads.push(Read { fact: Fact { kind: FactKind::BorrowHead, key: k, detail: DETAIL }, answer: Some(hasher.finish()) });
            return;
        };
        let self_trait = match h.self_ {
            SelfTy::Trait(lib) => Some(checker_of(lib)),
            _ => None,
        };
        self.rows(k, self_trait, clauses, tys, regions);
    }

    /// [`TiesTable::gather`] past the head: `self_trait` the trait of a trait method (its
    /// implicit `Self: Trait<'params>` row), `None` for any other fn.
    fn rows(&mut self, k: u128, self_trait: Option<tc::ItemKey>, clauses: &[tc::PClause], tys: tc::PTys<'_>, regions: &dyn RegionFacts) {
        let start = self.ties.len();
        if let Some(tk) = self_trait {
            match self.trait_count(tk, regions) {
                Some(0) => {}
                Some(n) => {
                    let lo = self.regions.len() as u32;
                    for i in 0..n {
                        self.regions.push(Region::Param(i));
                    }
                    self.ties.push(Tie { param: 0, trait_: tk, lo, hi: self.regions.len() as u32, read: true });
                }
                None => self.unread(start, 0, tk),
            }
        }
        let views = regions.clauses(tc::ItemKey(k));
        for (nth, c) in clauses.iter().enumerate() {
            let tc::PClause::Trait { trait_, args } = *c else { continue };
            // A bound on a bare parameter of the fn; any other self type is no tie row.
            let first = tys.list(args).ok().and_then(|l| l.first().copied());
            let p = match first.map(|f| tys.node(f)) {
                Some(Ok(tc::PNode::Param(p))) => p,
                Some(Ok(_)) | None => continue,
                Some(Err(_)) => {
                    self.unread(start, u32::MAX, trait_);
                    continue;
                }
            };
            let Some(count) = self.trait_count(trait_, regions) else {
                self.unread(start, p, trait_);
                continue;
            };
            if count == 0 {
                continue;
            }
            let own = match &views {
                Ok(vs) => vs.iter().find(|v| v.nth == nth as u32).map(|v| v.col.own()),
                Err(_) => None,
            };
            match own {
                Some(own) if own.len() == count as usize => {
                    if let Some(t) = self.ties[start..].iter().find(|t| t.param == p && t.trait_ == trait_) {
                        // The same bound again (a trait method's inherited `Self: Trait<'a..>`
                        // beside its implicit row) is one row; two that differ
                        // (`T: Tr<'a> + Tr<'b>`) are not: which answers is unread.
                        let same = t.read && self.regions.get(t.lo as usize..t.hi as usize) == Some(own);
                        if !same {
                            self.unread(start, p, trait_);
                        }
                        continue;
                    }
                    let lo = self.regions.len() as u32;
                    self.regions.extend_from_slice(own);
                    self.ties.push(Tie { param: p, trait_, lo, hi: self.regions.len() as u32, read: true });
                }
                _ => self.unread(start, p, trait_),
            }
        }
        let mut hasher = StableHasher::new(DOMAIN);
        hasher.tag(1);
        hasher.u32((self.ties.len() - start) as u32);
        for t in self.ties[start..].iter() {
            hasher.u32(t.param);
            hasher.u128(t.trait_.0);
            hasher.tag(u8::from(t.read));
            hasher.u32(t.hi - t.lo);
            for &r in self.regions[t.lo as usize..t.hi as usize].iter() {
                region(&mut hasher, r);
            }
        }
        self.reads.push(Read { fact: Fact { kind: FactKind::BorrowHead, key: k, detail: DETAIL }, answer: Some(hasher.finish()) });
        if self.index.last().is_none_or(|&(last, _, _)| last < k) {
            self.index.push((k, start as u32, self.ties.len() as u32));
        }
    }

    /// The row for `(param, trait_)` of the fn whose rows start at `start` is unread: an
    /// existing one is turned unread, else an unread one is filed.
    fn unread(&mut self, start: usize, param: u32, trait_: tc::ItemKey) {
        if let Some(t) = self.ties[start..].iter_mut().find(|t| t.param == param && t.trait_ == trait_) {
            t.read = false;
            t.lo = t.hi;
            return;
        }
        let at = self.regions.len() as u32;
        self.ties.push(Tie { param, trait_, lo: at, hi: at, read: false });
    }

    /// Trait `t`'s lifetime count, its row filed once per trait; `None`: not read. Also the
    /// one home of a trait object's principal's count (borrows.rs `object_traits`).
    pub(super) fn trait_count(&mut self, t: tc::ItemKey, regions: &dyn RegionFacts) -> Option<u32> {
        let got = regions.lifetime_params(t).map(|p| p.count()).map_err(|u| u.why);
        if !self.traits.contains(&t.0) {
            self.traits.push(t.0);
            let mut h = StableHasher::new(TRAIT_DOMAIN);
            match got {
                Ok(n) => {
                    h.tag(1);
                    h.u32(n);
                }
                Err(UnreadWhy::NoHead) => h.tag(2),
                Err(UnreadWhy::HeadRefused) => h.tag(3),
                Err(UnreadWhy::RecordForm { found }) => {
                    h.tag(4);
                    h.u32(found);
                }
                Err(UnreadWhy::Refused { what }) => {
                    h.tag(5);
                    // This file is `borrows`'s child (`#[path]`), so front2 is two levels up.
                    super::super::borrow_reads::hash_text(&mut h, what);
                }
            }
            self.reads.push(Read { fact: Fact { kind: FactKind::BorrowHead, key: t.0, detail: TRAIT_DETAIL }, answer: Some(h.finish()) });
        }
        got.ok()
    }
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

#[cfg(test)]
mod tests {
    //! The rows read from a head's clauses and their columns (D148), with planted controls.
    //! `fn f<'a, T: Tr<'a>>`: its clause `T: Tr<'a>` with the column whose own slot is
    //! `Param(0)` gives the read row `(T, Tr, ['a])`. CONTROLS: the same clause with a column of
    //! another count, with no column, or with the trait's regions unread gives an UNREAD row
    //! (never a guessed one, never no row: a call through it is refused, trait_ties.rs).

    use super::*;
    use blaze_typeck::region_facts::{ClauseView, ColView, Outlives, ParamsView, RegionsUnread, SigRegions};
    use blaze_typeck::regions::{Arity, Count, LifetimeParams, RegionCol};

    const F: u128 = 0xf;
    const TR: tc::ItemKey = tc::ItemKey(0x7a);

    fn a() -> Alloc<'static> {
        Alloc::global()
    }

    /// No nominal type in these columns.
    struct NoNominal;
    impl Arity for NoNominal {
        fn adt_lifetimes(&self, _: tc::ItemKey) -> Count {
            Count::NotKnown
        }
        fn opaque_captures(&self, _: tc::ItemKey) -> Count {
            Count::NotKnown
        }
        fn alias_lifetimes(&self, _: tc::ItemKey, _: tc::Name) -> Count {
            Count::NotKnown
        }
        fn trait_lifetimes(&self, _: tc::ItemKey) -> Count {
            Count::NotKnown
        }
    }

    /// Region facts answering `clauses` of `F` and `lifetime_params` of `TR` only.
    struct Facts<'r> {
        clauses: &'r [ClauseView<'r>],
        tr: Option<&'r LifetimeParams<Alloc<'static>>>,
    }

    impl RegionFacts for Facts<'_> {
        fn sig(&self, item: tc::ItemKey) -> Result<SigRegions<'_>, RegionsUnread> {
            Err(RegionsUnread { item, why: UnreadWhy::NoHead })
        }
        fn clauses(&self, item: tc::ItemKey) -> Result<&[ClauseView<'_>], RegionsUnread> {
            if item == tc::ItemKey(F) { Ok(self.clauses) } else { Err(RegionsUnread { item, why: UnreadWhy::NoHead }) }
        }
        fn outlives(&self, item: tc::ItemKey) -> Result<&[Outlives], RegionsUnread> {
            Err(RegionsUnread { item, why: UnreadWhy::NoHead })
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
            match self.tr {
                Some(p) if item == TR => Ok(ParamsView::of(p)),
                _ => Err(RegionsUnread { item, why: UnreadWhy::NoHead }),
            }
        }
    }

    /// The one row filed for `fn f<'a, T: Tr<'a>>` (its clause 0, `T` = `Param(0)`), whose
    /// clause column holds `own` as its own slots (laid over `&T`, a node with one own slot,
    /// or over `T` for none), with `Tr` declaring `tr_count` lifetimes (`None`: unread).
    fn row(own: Option<&[Region]>, tr_count: Option<u32>) -> (Tie, std::vec::Vec<Region>) {
        let mut tys = tc::PortableTys::new_in(a());
        let p0 = tys.intern(tc::PNode::Param(0)).unwrap();
        let args = tys.intern_list(&[p0]).unwrap();
        let clauses = [tc::PClause::Trait { trait_: TR, args }];
        let laid = own.map(|own| {
            let over = if own.is_empty() { p0 } else { tys.intern(tc::PNode::Ref(tc::Mutbl::Not, p0)).unwrap() };
            RegionCol::lay(tys.view(), over, &NoNominal, own, a()).unwrap()
        });
        let views: std::vec::Vec<ClauseView<'_>> = laid.iter().map(|col| ClauseView { nth: 0, binders: 0, col: ColView::of(col), outlives: None }).collect();
        let params = tr_count.map(|n| LifetimeParams::new_in(n, a()));
        let facts = Facts { clauses: &views, tr: params.as_ref() };
        let mut table = TiesTable::new_in(a());
        table.rows(F, None, &clauses, tys.view(), &facts);
        assert_eq!(table.index.len(), 1, "the fn's rows are filed");
        let got = table.get(tc::ItemKey(F)).unwrap();
        assert_eq!(got.rows.len(), 1, "one row for the one bound: {:?}", got.rows);
        let t = got.rows[0];
        (t, got.regions[t.lo as usize..t.hi as usize].to_vec())
    }

    #[test]
    fn a_bound_is_read_from_its_clause_column() {
        let (t, regions) = row(Some(&[Region::Param(0)]), Some(1));
        std::println!("a_bound_is_read_from_its_clause_column: {t:?} {regions:?}");
        assert_eq!((t.param, t.trait_, t.read), (0, TR, true));
        assert_eq!(regions, [Region::Param(0)]);
    }

    /// CONTROLS: each unknown keeps the row, unread.
    #[test]
    fn an_unknown_part_keeps_the_row_unread() {
        for (what, own, count) in [
            ("a column of another count", Some(&[][..]), Some(1)),
            ("no column", None, Some(1)),
            ("the trait's regions unread", Some(&[Region::Param(0)][..]), None),
        ] {
            let (t, regions) = row(own, count);
            std::println!("an_unknown_part_keeps_the_row_unread {what}: {t:?}");
            assert_eq!((t.param, t.trait_, t.read), (0, TR, false), "{what}");
            assert!(regions.is_empty(), "{what}");
        }
    }

    /// A trait with no lifetimes has no row (nothing to tie).
    #[test]
    fn a_trait_without_lifetimes_has_no_row() {
        let mut tys = tc::PortableTys::new_in(a());
        let p0 = tys.intern(tc::PNode::Param(0)).unwrap();
        let args = tys.intern_list(&[p0]).unwrap();
        let clauses = [tc::PClause::Trait { trait_: TR, args }];
        let params = LifetimeParams::new_in(0, a());
        let facts = Facts { clauses: &[], tr: Some(&params) };
        let mut table = TiesTable::new_in(a());
        table.rows(F, None, &clauses, tys.view(), &facts);
        assert_eq!(table.get(tc::ItemKey(F)).map(|t| t.rows.len()), Some(0));
    }
}
