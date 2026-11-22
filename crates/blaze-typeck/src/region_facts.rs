//! What a head's lowering says about its lifetimes, and what a reader asks of it (D148,
//! docs/v2/plan/design/flow.md "Lifetimes are produced once and carried beside the types";
//! piece L2 makes this file exactly the contract written there).
//!
//! The producer (front2 heads.rs, heads_regions.rs: pieces L1a, L1b) fills one
//! [`HeadRegions`] per head, in the same walk that interns the erased types; the head record
//! (front2 head_record.rs, form 22) stores it; every reader (the flow lowering's `Facts`:
//! pieces L3, L4, L5) asks through [`RegionFacts`] and reads borrowed [`ColView`]s,
//! [`ParamsView`]s and [`ClauseView`]s, never a signature's text. Numbering, counting and the
//! column itself have their one home in [`crate::regions`]; this file adds only what a head
//! holds and how it is read.
//!
//! A region of a head that cannot be read is a typed [`RegionsUnread`] naming the item and why
//! (rule 19): never a guess, never a fresh origin in place of a declared one (rule 3).

use core::alloc::Allocator;

use blaze_own::Vec;

use crate::ids::ItemKey;
use crate::portable::PTy;
use crate::regions::{LifetimeParams, Region, RegionCol, RegionError};

/// What one head's lowering produced (pieces L1a, L1b fill it; L2 stores it).
#[derive(Debug)]
pub struct HeadRegions<A: Allocator> {
    pub params: LifetimeParams<A>,
    /// A fn: laid over `FnPtr(inputs.., output)`; `child(k)` is input k, the last child the output.
    pub sig: Option<RegionCol<A>>,
    /// An async fn's WRITTEN return type (the future's output).
    pub written_output: Option<RegionCol<A>>,
    /// One per where-clause or bound, in the head's clause order (`nth`).
    pub clauses: Vec<ClauseRegions<A>, A>,
    pub outlives: Vec<Outlives, A>,
    /// An ADT: (variant, field, the field type's column).
    pub fields: Vec<(u32, u32, RegionCol<A>), A>,
    /// An impl: its self type's column and its trait reference's.
    pub impl_header: Option<(RegionCol<A>, Option<RegionCol<A>>)>,
    /// An impl: each `type Name = U;` member's value, U's column in the impl's numbering
    /// (`Param(i)` its lifetimes), in strict name order (each name once; the head record
    /// refuses another order) ([`RegionFacts::impl_assoc`]).
    pub impl_assoc: Vec<(crate::ids::Name, RegionCol<A>), A>,
    /// An opaque type: how many lifetimes it captures (edition rules and `use<..>` applied).
    pub opaque_captures: Option<u32>,
}

/// One where-clause's or bound's column: `nth` its place in the head's clause order, `binders`
/// how many lifetimes its own `for<..>` binds (0 when it has none).
#[derive(Debug)]
pub struct ClauseRegions<A: Allocator> {
    pub nth: u32,
    pub binders: u32,
    pub col: RegionCol<A>,
}

/// A declared outlives clause: `T: 'r` (the head's clause `clause`, whose subject is the type)
/// or `'long: 'short`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Outlives {
    Type { clause: u32, region: Region },
    Region { long: Region, short: Region },
}

/// What a reader asks (the flow lowering's `Facts` supplies it from head records).
pub trait RegionFacts {
    fn sig(&self, item: ItemKey) -> Result<SigRegions<'_>, RegionsUnread>;
    fn clauses(&self, item: ItemKey) -> Result<&[ClauseView<'_>], RegionsUnread>;
    fn outlives(&self, item: ItemKey) -> Result<&[Outlives], RegionsUnread>;
    fn field(&self, adt: ItemKey, variant: u32, field: u32) -> Result<ColView<'_>, RegionsUnread>;
    fn impl_header(&self, impl_: ItemKey) -> Result<(ColView<'_>, Option<ColView<'_>>), RegionsUnread>;
    fn opaque_captures(&self, opaque: ItemKey) -> Result<u32, RegionsUnread>;
    fn lifetime_params(&self, item: ItemKey) -> Result<ParamsView<'_>, RegionsUnread>;
    /// An impl's `type name = U;` value: U's column, numbered as the impl's (`Param(i)` its lifetimes); `NoHead` where none is filed.
    fn impl_assoc(&self, impl_: ItemKey, name: crate::ids::Name) -> Result<ColView<'_>, RegionsUnread> { let _ = name; Err(RegionsUnread { item: impl_, why: UnreadWhy::NoHead }) }
}

/// A fn's signature regions, borrowed: its lifetime parameters, the column laid over
/// `FnPtr(inputs.., output)`, and an async fn's written output.
#[derive(Clone, Copy, Debug)]
pub struct SigRegions<'a> {
    pub params: ParamsView<'a>,
    pub col: ColView<'a>,
    pub written_output: Option<ColView<'a>>,
}

/// Why a head's regions were not read: the item named, the cause typed (rule 19); never a guess.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RegionsUnread {
    pub item: ItemKey,
    pub why: UnreadWhy,
}

/// The cause of a [`RegionsUnread`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum UnreadWhy {
    /// No head of the item is filed, or its head holds no such part (a `sig` asked of an ADT,
    /// a field the ADT does not have).
    NoHead,
    /// The item's head was refused (its own error is the head's, reported there: E0106 is one).
    HeadRefused,
    /// The item's head record is of another form (`found` its version): reported and rebuilt,
    /// never migrated (rule 26).
    RecordForm { found: u32 },
    /// The item's head is filed whole, its regions refused by name (`what`): its record's
    /// regions and lifetime part did not line up (blaze-ladder heads.rs `file_heads`).
    Refused { what: &'static str },
}

impl core::fmt::Display for RegionsUnread {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let item = self.item.0;
        match self.why {
            UnreadWhy::NoHead => write!(f, "regions of item {item:#034x}: no head holds them (D148)"),
            UnreadWhy::HeadRefused => write!(f, "regions of item {item:#034x}: its head was refused (D148)"),
            UnreadWhy::RecordForm { found } => {
                write!(f, "regions of item {item:#034x}: its head record is form {found}, not this reader's: rebuild it (rule 26)")
            }
            UnreadWhy::Refused { what } => write!(f, "regions of item {item:#034x}: refused, {what} (D148)"),
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Views: borrowed reads of a stored column and parameter set, the same reads the owned types
// give, with no allocator in their type (a reader is not generic over the store's allocator).

/// What a [`ColView`] reads of a [`RegionCol`] of any allocator.
trait ColRead {
    fn ty(&self) -> PTy;
    fn slots(&self) -> &[Region];
    fn own(&self) -> &[Region];
    fn children(&self) -> u32;
    fn child(&self, k: u32) -> Option<&[Region]>;
}

impl<A: Allocator> ColRead for RegionCol<A> {
    fn ty(&self) -> PTy {
        RegionCol::ty(self)
    }
    fn slots(&self) -> &[Region] {
        self.as_slice()
    }
    fn own(&self) -> &[Region] {
        RegionCol::own(self)
    }
    fn children(&self) -> u32 {
        RegionCol::children(self)
    }
    fn child(&self, k: u32) -> Option<&[Region]> {
        RegionCol::child(self, k)
    }
}

/// A borrowed read of a stored [`RegionCol`]: slots by index, `own()`, `child(k)`.
#[derive(Clone, Copy)]
pub struct ColView<'a> {
    col: &'a dyn ColRead,
}

impl<'a> ColView<'a> {
    /// The view of `col`.
    pub fn of<A: Allocator + 'a>(col: &'a RegionCol<A>) -> ColView<'a> {
        ColView { col }
    }

    /// The type the column was laid over.
    pub fn ty(&self) -> PTy {
        self.col.ty()
    }

    /// The number of slots.
    pub fn len(&self) -> u32 {
        self.col.slots().len() as u32
    }

    pub fn is_empty(&self) -> bool {
        self.col.slots().is_empty()
    }

    /// Every slot, in slot order.
    pub fn as_slice(&self) -> &'a [Region] {
        let col: &'a dyn ColRead = self.col;
        col.slots()
    }

    /// The slot `i`; `None` past the column.
    pub fn slot(&self, i: u32) -> Option<Region> {
        self.as_slice().get(i as usize).copied()
    }

    /// The root node's own slots (before any child's).
    pub fn own(&self) -> &'a [Region] {
        let col: &'a dyn ColRead = self.col;
        col.own()
    }

    /// The number of the root's children.
    pub fn children(&self) -> u32 {
        self.col.children()
    }

    /// The slots of the root's child `k`; `None` past its children.
    pub fn child(&self, k: u32) -> Option<&'a [Region]> {
        let col: &'a dyn ColRead = self.col;
        col.child(k)
    }

    /// Whether two views read the same column: one type (both in one table), the same slots
    /// and the same split among the root's children.
    pub fn same(&self, other: &ColView<'_>) -> bool {
        self.ty() == other.ty()
            && self.as_slice() == other.as_slice()
            && self.own() == other.own()
            && self.children() == other.children()
            && (0..self.children()).all(|k| self.child(k) == other.child(k))
    }
}

impl core::fmt::Debug for ColView<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ColView").field("ty", &self.ty()).field("slots", &self.as_slice()).field("own", &self.own().len()).finish()
    }
}

/// What a [`ParamsView`] reads of a [`LifetimeParams`] of any allocator.
trait ParamsRead {
    fn count(&self) -> u32;
    fn is_late(&self, i: u32) -> Result<bool, RegionError>;
    fn outlives(&self) -> &[(Region, Region)];
}

impl<A: Allocator> ParamsRead for LifetimeParams<A> {
    fn count(&self) -> u32 {
        LifetimeParams::count(self)
    }
    fn is_late(&self, i: u32) -> Result<bool, RegionError> {
        LifetimeParams::is_late(self, i)
    }
    fn outlives(&self) -> &[(Region, Region)] {
        LifetimeParams::outlives(self)
    }
}

/// A borrowed read of a stored [`LifetimeParams`]: count, `is_late(i)`, outlives pairs.
#[derive(Clone, Copy)]
pub struct ParamsView<'a> {
    params: &'a dyn ParamsRead,
}

impl<'a> ParamsView<'a> {
    /// The view of `params`.
    pub fn of<A: Allocator + 'a>(params: &'a LifetimeParams<A>) -> ParamsView<'a> {
        ParamsView { params }
    }

    /// The number of lifetime parameters.
    pub fn count(&self) -> u32 {
        self.params.count()
    }

    /// Whether `Param(i)` is late-bound; [`RegionError::LifetimePast`] past the count.
    pub fn is_late(&self, i: u32) -> Result<bool, RegionError> {
        self.params.is_late(i)
    }

    /// The declared outlives clauses among the parameters, `(longer, shorter)`.
    pub fn outlives(&self) -> &'a [(Region, Region)] {
        let p: &'a dyn ParamsRead = self.params;
        p.outlives()
    }

    /// Whether two views read the same parameters.
    pub fn same(&self, other: &ParamsView<'_>) -> bool {
        self.count() == other.count() && self.outlives() == other.outlives() && (0..self.count()).all(|i| self.is_late(i) == other.is_late(i))
    }
}

impl core::fmt::Debug for ParamsView<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ParamsView").field("count", &self.count()).field("outlives", &self.outlives()).finish()
    }
}

/// A borrowed read of a stored [`ClauseRegions`].
#[derive(Clone, Copy, Debug)]
pub struct ClauseView<'a> {
    pub nth: u32,
    pub binders: u32,
    pub col: ColView<'a>,
}

impl<'a> ClauseView<'a> {
    /// The view of `clause`.
    pub fn of<A: Allocator + 'a>(clause: &'a ClauseRegions<A>) -> ClauseView<'a> {
        ClauseView { nth: clause.nth, binders: clause.binders, col: ColView::of(&clause.col) }
    }
}

impl<A: Allocator + Clone> HeadRegions<A> {
    /// A head with `params` and no other part: what a producer starts from and fills.
    pub fn new_in(params: LifetimeParams<A>, alloc: A) -> HeadRegions<A> {
        HeadRegions {
            params,
            sig: None,
            written_output: None,
            clauses: Vec::new_in(alloc.clone()),
            outlives: Vec::new_in(alloc.clone()),
            fields: Vec::new_in(alloc.clone()),
            impl_header: None,
            impl_assoc: Vec::new_in(alloc),
            opaque_captures: None,
        }
    }
}

impl<A: Allocator> HeadRegions<A> {
    /// A fn's signature regions as a reader reads them; `None` when the head has no `sig`.
    pub fn sig_view(&self) -> Option<SigRegions<'_>> {
        let col = self.sig.as_ref()?;
        Some(SigRegions { params: ParamsView::of(&self.params), col: ColView::of(col), written_output: self.written_output.as_ref().map(ColView::of) })
    }

    /// Whether two heads' regions are one, part by part (both in one type table).
    pub fn same<B: Allocator>(&self, other: &HeadRegions<B>) -> bool {
        let col = |a: &RegionCol<A>, b: &RegionCol<B>| ColView::of(a).same(&ColView::of(b));
        let opt = |a: Option<&RegionCol<A>>, b: Option<&RegionCol<B>>| match (a, b) {
            (None, None) => true,
            (Some(a), Some(b)) => col(a, b),
            _ => false,
        };
        ParamsView::of(&self.params).same(&ParamsView::of(&other.params))
            && opt(self.sig.as_ref(), other.sig.as_ref())
            && opt(self.written_output.as_ref(), other.written_output.as_ref())
            && self.clauses.len() == other.clauses.len()
            && self.clauses.iter().zip(other.clauses.iter()).all(|(a, b)| a.nth == b.nth && a.binders == b.binders && col(&a.col, &b.col))
            && self.outlives.as_slice() == other.outlives.as_slice()
            && self.fields.len() == other.fields.len()
            && self.fields.iter().zip(other.fields.iter()).all(|(a, b)| a.0 == b.0 && a.1 == b.1 && col(&a.2, &b.2))
            && match (&self.impl_header, &other.impl_header) {
                (None, None) => true,
                (Some((s1, t1)), Some((s2, t2))) => col(s1, s2) && opt(t1.as_ref(), t2.as_ref()),
                _ => false,
            }
            && self.impl_assoc.len() == other.impl_assoc.len()
            && self.impl_assoc.iter().zip(other.impl_assoc.iter()).all(|(a, b)| a.0 == b.0 && col(&a.1, &b.1))
            && self.opaque_captures == other.opaque_captures
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::{FnPtrHeader, Mutbl, UintTy};
    use crate::portable::{PNode, PortableTys};
    use crate::regions::{Arity, Count};
    use crate::ids::Name;
    use blaze_own::Alloc;

    /// No ADT, opaque or alias is in the toy types.
    struct NoNominal;

    impl Arity for NoNominal {
        fn adt_lifetimes(&self, _: ItemKey) -> Count {
            Count::NotKnown
        }
        fn opaque_captures(&self, _: ItemKey) -> Count {
            Count::NotKnown
        }
        fn alias_lifetimes(&self, _: ItemKey, _: Name) -> Count {
            Count::NotKnown
        }
        fn trait_lifetimes(&self, _: ItemKey) -> Count {
            Count::NotKnown
        }
    }

    /// `fn(&'b u8, &'a u8) -> &'a u8` of `fn f<'a, 'b>`: [Param(1), Param(0), Param(0)].
    fn sig_of(t: &mut PortableTys<'static>, slots: &[Region]) -> RegionCol<Alloc<'static>> {
        let u = t.intern(PNode::Uint(UintTy::U8)).unwrap();
        let r = t.intern(PNode::Ref(Mutbl::Not, u)).unwrap();
        let io = t.intern_list(&[r, r, r]).unwrap();
        let f = t.intern(PNode::FnPtr(io, FnPtrHeader::RUST)).unwrap();
        RegionCol::lay(t.view(), f, &NoNominal, slots, Alloc::global()).unwrap()
    }

    /// A view reads what the owned column and parameters read; two columns that differ in one
    /// slot are not the same (the planted control).
    #[test]
    fn views_read_what_the_owned_types_read() {
        let mut t = PortableTys::new_in(Alloc::global());
        let (a, b) = (Region::Param(0), Region::Param(1));
        let col = sig_of(&mut t, &[b, a, a]);
        // The same column laid again: the head below owns one, the view reads the other.
        let filed = sig_of(&mut t, &[b, a, a]);
        let v = ColView::of(&col);
        assert_eq!(v.ty(), col.ty());
        assert_eq!(v.len(), 3);
        assert_eq!(v.as_slice(), col.as_slice());
        assert_eq!(v.own(), col.own());
        assert_eq!(v.children(), 3);
        for k in 0..4 {
            assert_eq!(v.child(k), col.child(k));
        }
        assert_eq!(v.slot(0), Some(b));
        assert_eq!(v.slot(3), None);

        let mut params = LifetimeParams::new_in(2, Alloc::global());
        params.set_late(1).unwrap();
        params.add_outlives(b, a).unwrap();
        let p = ParamsView::of(&params);
        assert_eq!(p.count(), 2);
        assert_eq!(p.is_late(1), Ok(true));
        assert_eq!(p.is_late(0), Ok(false));
        assert_eq!(p.is_late(2), Err(RegionError::LifetimePast { index: 2, count: 2 }));
        assert_eq!(p.outlives(), [(b, a)]);

        let mut head = HeadRegions::new_in(params, Alloc::global());
        head.sig = Some(filed);
        let sig = head.sig_view().expect("a sig");
        assert!(sig.col.same(&v));
        assert!(sig.written_output.is_none());

        // CONTROL: numbering by first appearance (`'b` first) lays [Param(0), Param(1), Param(1)].
        let first_seen = sig_of(&mut t, &[a, b, b]);
        assert!(!ColView::of(&first_seen).same(&v), "CONTROL: two columns differing in every slot read the same");
    }

    /// Two heads are the same part by part; one outlives clause more is not (the control).
    #[test]
    fn heads_compare_part_by_part() {
        let mut t = PortableTys::new_in(Alloc::global());
        let a = Region::Param(0);
        let make = |t: &mut PortableTys<'static>| {
            let mut h = HeadRegions::new_in(LifetimeParams::new_in(1, Alloc::global()), Alloc::global());
            h.sig = Some(sig_of(t, &[a, a, a]));
            h.outlives.push(Outlives::Type { clause: 0, region: Region::Static });
            h
        };
        let x = make(&mut t);
        let mut y = make(&mut t);
        assert!(x.same(&y));
        y.outlives.push(Outlives::Region { long: a, short: Region::Static });
        assert!(!x.same(&y), "CONTROL: a head with one clause more is the same");
    }

    /// An unread names its item and its cause.
    #[test]
    fn unread_names_the_item() {
        let u = RegionsUnread { item: ItemKey(0xab), why: UnreadWhy::RecordForm { found: 21 } };
        let text = std::format!("{u}");
        assert!(text.contains("0x000000000000000000000000000000ab"), "{text}");
        assert!(text.contains("form 21"), "{text}");
    }
}
