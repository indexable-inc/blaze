//! The region slots of a type: ONE answer, slot for slot with rustc's region arguments
//! (end-state.md D156; the audit's C3, "one grain").
//!
//! THE ORDER. A type's slots are in its pre-order: a node's OWN slots, then each child's slots
//! in order. An ADT's own slots are its lifetime arguments, then its type arguments' slots; a
//! reference's own region, then its referent's; an unnormalized projection's own slots are its
//! trait's lifetime arguments (then a GAT's own), then its arguments' slots (rustc's rule for an
//! unnormalized alias: its regions are its arguments'); an opaque type's own slots are its
//! captured lifetimes, then its arguments'; `dyn Trait + 'r` is `'r`, then its predicates'; a
//! fn pointer has none of its own, its inputs' and output's are its children's, and the regions
//! its binder binds are marked in the column (`Region::Late`), not counted apart.
//!
//! TYPED INDEXES (states.md item 31): [`Slot`], [`SlotCount`] and [`SlotRange`] do no
//! arithmetic with any other integer type; the only way from a count to an index is through a
//! range.
//!
//! TWO IMPLEMENTATIONS. [`Columns`] answers from D148's data (blaze-typeck regions.rs:
//! `region_arity`, the one counter, over the `Arity` facts the head lowering filed). It answers
//! HONESTLY: an opaque with N captures has N own slots (today's flow lowering answers 1), and a
//! count not filed is [`Unknown`], never 0 or 1. The second implementation (lifetime arguments
//! inside the type nodes, end-state.md D156's first sentence) answers the same methods from the
//! node itself; nothing in inst.rs, relate.rs or call.rs reads a count any other way.
#![blaze::sans_io]

use blaze_typeck::regions::{self, Arity, region_arity};
use blaze_typeck::{ItemKey, Mutbl, PNode, PTy, PTys};

use crate::Variance;

/// One region slot: an index into a value's slot range or a column. No arithmetic.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Slot(u32);

impl Slot {
    /// The slot at index `i` of a caller's origin column.
    pub const fn new(i: u32) -> Slot {
        Slot(i)
    }

    /// The index, for reading a column.
    pub const fn index(self) -> usize {
        self.0 as usize
    }
}

/// How many region slots: of a node of its own, of a whole type, of a range.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SlotCount(u32);

impl SlotCount {
    pub const ZERO: SlotCount = SlotCount(0);

    pub const fn new(n: u32) -> SlotCount {
        SlotCount(n)
    }

    /// The count of a column of `len` entries; `None` past `u32::MAX`.
    pub fn of_len(len: usize) -> Option<SlotCount> {
        u32::try_from(len).ok().map(SlotCount)
    }

    pub const fn get(self) -> u32 {
        self.0
    }

    pub const fn checked_add(self, other: SlotCount) -> Option<SlotCount> {
        match self.0.checked_add(other.0) {
            Some(n) => Some(SlotCount(n)),
            None => None,
        }
    }

    pub const fn checked_sub(self, other: SlotCount) -> Option<SlotCount> {
        match self.0.checked_sub(other.0) {
            Some(n) => Some(SlotCount(n)),
            None => None,
        }
    }

    /// The count as a length, for splitting a column.
    pub const fn len(self) -> usize {
        self.0 as usize
    }
}

/// A run of slots `start..start + count` of a caller's origin column: one value's slots.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct SlotRange {
    start: Slot,
    count: SlotCount,
}

impl SlotRange {
    pub const EMPTY: SlotRange = SlotRange { start: Slot(0), count: SlotCount(0) };

    /// `count` slots from `start`; `None` if the end passes `u32::MAX`.
    pub const fn new(start: Slot, count: SlotCount) -> Option<SlotRange> {
        match start.0.checked_add(count.0) {
            Some(_) => Some(SlotRange { start, count }),
            None => None,
        }
    }

    pub const fn start(self) -> Slot {
        self.start
    }

    pub const fn count(self) -> SlotCount {
        self.count
    }

    /// The first `n` slots and the rest; `None` if the range has fewer than `n`.
    pub const fn split(self, n: SlotCount) -> Option<(SlotRange, SlotRange)> {
        if n.0 > self.count.0 {
            return None;
        }
        let head = SlotRange { start: self.start, count: n };
        let tail = SlotRange { start: Slot(self.start.0 + n.0), count: SlotCount(self.count.0 - n.0) };
        Some((head, tail))
    }

    /// The `k`-th slot of the range (`k` an own-slot index of the node the range is over).
    pub const fn nth(self, k: u32) -> Option<Slot> {
        if k >= self.count.0 {
            return None;
        }
        Some(Slot(self.start.0 + k))
    }

    /// The slots, in order.
    pub fn slots(self) -> impl Iterator<Item = Slot> {
        (self.start.0..self.start.0 + self.count.0).map(Slot)
    }
}

/// Why a count is not known.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Why {
    /// A fact the counter needs was not filed (an ADT's, opaque's, trait's or alias's lifetimes).
    Fact(regions::Unknown),
    /// A body's type (a closure, an async block's future, a coroutine): its slots are its
    /// captures', which a declared column does not hold.
    BodyType,
    /// A hole (`_`) where a type was expected.
    Hole,
    /// The table could not be read at the node.
    Table,
    /// The walk passed its bound (rule 18).
    Bound,
    /// The count passed `u32::MAX`.
    Overflow,
}

/// A node whose region count is not known, and why: never a guessed number (rule 3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Unknown {
    pub node: PTy,
    pub why: Why,
}

/// A node's type children whose slots follow its own, in order.
#[derive(Clone, Copy, Debug)]
pub enum Kids<'t> {
    None,
    One(PTy),
    List(&'t [PTy]),
    /// A projection predicate's arguments, then its term.
    ListThen(&'t [PTy], PTy),
}

impl Kids<'_> {
    pub fn len(&self) -> u32 {
        match self {
            Kids::None => 0,
            Kids::One(_) => 1,
            Kids::List(l) => l.len() as u32,
            Kids::ListThen(l, _) => l.len() as u32 + 1,
        }
    }

    /// The `k`-th child.
    pub fn get(&self, k: u32) -> Option<PTy> {
        match *self {
            Kids::None => None,
            Kids::One(t) => (k == 0).then_some(t),
            Kids::List(l) => l.get(k as usize).copied(),
            Kids::ListThen(l, last) => {
                if (k as usize) < l.len() {
                    l.get(k as usize).copied()
                } else if k as usize == l.len() {
                    Some(last)
                } else {
                    None
                }
            }
        }
    }
}

/// Where in a node a slot or a child is: for its variance.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pos {
    /// The node's own slot `k`.
    Own(u32),
    /// The node's child `k`.
    Kid(u32),
}

/// The most nodes one count visits, and the deepest it goes (rule 18).
pub const MOST_COUNT_STEPS: u32 = 4096;
pub const MOST_DEPTH: u32 = 256;

/// THE ONE ANSWER for a type's region slots. Every count, split and variance the call rule uses
/// comes from here; an implementation answers for the types of one table.
pub trait RegionTypes {
    /// The node of `ty`.
    fn node(&self, ty: PTy) -> Result<PNode, Unknown>;
    /// How many slots `ty`'s node has of its own (before its children's).
    fn own(&self, ty: PTy) -> Result<SlotCount, Unknown>;
    /// The children of `ty` whose slots follow its own, in order.
    fn kids(&self, ty: PTy) -> Result<Kids<'_>, Unknown>;
    /// The declared variance of each generic parameter of ADT `adt`, its lifetimes first, then
    /// its type parameters; `None` where not known (then every one is invariant: the sound
    /// answer, more relations refuse more, never less).
    fn adt_variances(&self, adt: ItemKey) -> Option<&[Variance]>;

    /// How many slots the whole of `ty` has: its own, then its children's, bounded.
    fn count(&self, ty: PTy) -> Result<SlotCount, Unknown> {
        count_in(self, ty)
    }

    /// The variance of `ty`'s own slot or child at `at`, within `ty`: [`rule_variance`].
    fn variance(&self, ty: PTy, at: Pos) -> Result<Variance, Unknown> {
        rule_variance(self, ty, at)
    }
}

#[derive(Clone, Copy)]
struct CountFrame<'t> {
    ty: PTy,
    kids: Kids<'t>,
    next_k: u32,
    acc: SlotCount,
}

fn count_in<T: RegionTypes + ?Sized>(types: &T, root: PTy) -> Result<SlotCount, Unknown> {
    const MAX_FRAMES: usize = 257; // MOST_DEPTH (256) + 1
    let mut budget = MOST_COUNT_STEPS;
    let mut stack: [Option<CountFrame<'_>>; MAX_FRAMES] = [None; MAX_FRAMES];
    let mut sp: usize = 0;

    let mut enter = Some(root);
    let mut finished: Option<SlotCount> = None;

    loop {
        if let Some(ty) = enter.take() {
            if budget == 0 || sp >= MAX_FRAMES {
                return Err(Unknown { node: ty, why: Why::Bound });
            }
            budget -= 1;
            let acc = types.own(ty)?;
            let kids = types.kids(ty)?;
            if kids.len() == 0 {
                finished = Some(acc);
            } else {
                let Some(slot) = stack.get_mut(sp) else {
                    return Err(Unknown { node: ty, why: Why::Bound });
                };
                *slot = Some(CountFrame { ty, kids, next_k: 0, acc });
                sp += 1;
            }
            continue;
        }

        let Some(top_idx) = sp.checked_sub(1) else {
            return finished.ok_or(Unknown { node: root, why: Why::Bound });
        };
        let Some(Some(top)) = stack.get_mut(top_idx) else {
            return Err(Unknown { node: root, why: Why::Bound });
        };

        if let Some(child_count) = finished.take() {
            top.acc = top.acc.checked_add(child_count).ok_or(Unknown { node: top.ty, why: Why::Overflow })?;
        }

        if top.next_k < top.kids.len() {
            let k = top.next_k;
            top.next_k += 1;
            let Some(c) = top.kids.get(k) else {
                return Err(Unknown { node: top.ty, why: Why::Table });
            };
            enter = Some(c);
        } else {
            let acc = top.acc;
            sp = top_idx;
            finished = Some(acc);
        }
    }
}

/// THE VARIANCE RULE (the Reference, "Subtyping and Variance", its table): `&'a T` covariant in
/// `'a` and `T`; `&'a mut T` covariant in `'a`, invariant in `T`; `*const T`, `[T]`, `[T; n]`,
/// tuples and pattern types covariant; `*mut T` invariant; `fn(A) -> R` contravariant in `A`,
/// covariant in `R`; `dyn Trait<T> + 'a` covariant in `'a`, invariant in its predicates; an ADT
/// as its declared variances say (not known: invariant). Everything else (a fn item's
/// arguments, an alias's, an opaque's, a trait reference's, a body type's) is invariant.
pub fn rule_variance<T: RegionTypes + ?Sized>(types: &T, ty: PTy, at: Pos) -> Result<Variance, Unknown> {
    let node = types.node(ty)?;
    Ok(match (node, at) {
        (PNode::Ref(..), Pos::Own(_)) | (PNode::Dynamic(_), Pos::Own(_)) => Variance::Co,
        (PNode::Ref(Mutbl::Not, _) | PNode::Ptr(Mutbl::Not, _), Pos::Kid(_)) => Variance::Co,
        (PNode::Ref(Mutbl::Mut, _) | PNode::Ptr(Mutbl::Mut, _), Pos::Kid(_)) => Variance::In,
        (PNode::Tuple(_) | PNode::Record(..) | PNode::Union(_) | PNode::Slice(_) | PNode::Array(..) | PNode::Pat(..), _) => Variance::Co,
        (PNode::Adt(adt, _), at) => {
            let i = match at {
                Pos::Own(k) => k as usize,
                Pos::Kid(k) => types.own(ty)?.len() + k as usize,
            };
            types.adt_variances(adt).and_then(|v| v.get(i).copied()).unwrap_or(Variance::In)
        }
        (PNode::FnPtr(..), Pos::Kid(k)) => {
            if k + 1 == types.kids(ty)?.len() {
                Variance::Co
            } else {
                Variance::Contra
            }
        }
        _ => Variance::In,
    })
}

/// The declared variances of ADTs, for [`Columns`] (the front end computes them from the
/// fields; whoever answers files the read, rule 21).
pub trait Variances {
    fn of(&self, adt: ItemKey) -> Option<&[Variance]>;
}

/// The FIRST implementation: D148's columns. Counts by blaze-typeck's one counter
/// (`region_arity`) over the `Arity` facts the head records filed; children in the column's own
/// order (regions.rs `kids`, the order `RegionCol::lay` checks).
#[derive(Clone, Copy)]
pub struct Columns<'a> {
    tys: PTys<'a>,
    arity: &'a dyn Arity,
    variances: &'a dyn Variances,
}

impl<'a> Columns<'a> {
    /// The answer for the types of `tys` (a body's record table, where its callees' columns are
    /// laid), counting by `arity`.
    pub fn new(tys: PTys<'a>, arity: &'a dyn Arity, variances: &'a dyn Variances) -> Columns<'a> {
        Columns { tys, arity, variances }
    }
}

impl RegionTypes for Columns<'_> {
    fn node(&self, ty: PTy) -> Result<PNode, Unknown> {
        self.tys.node(ty).map_err(|_| Unknown { node: ty, why: Why::Table })
    }

    fn own(&self, ty: PTy) -> Result<SlotCount, Unknown> {
        let node = self.node(ty)?;
        match node {
            // regions.rs counts none of their own here (a body type is the flow's, not a
            // column's): that is a count NOT KNOWN to a reader of columns, never 0.
            PNode::Closure { .. } | PNode::Future(..) | PNode::Coroutine { .. } => Err(Unknown { node: ty, why: Why::BodyType }),
            PNode::Infer => Err(Unknown { node: ty, why: Why::Hole }),
            _ => region_arity(ty, node, self.arity).map(SlotCount::new).map_err(|n| Unknown { node: n.node, why: Why::Fact(n.what) }),
        }
    }

    fn kids(&self, ty: PTy) -> Result<Kids<'_>, Unknown> {
        let node = self.node(ty)?;
        let list = |l| self.tys.list(l).map_err(|_| Unknown { node: ty, why: Why::Table });
        Ok(match node {
            PNode::Tuple(l)
            | PNode::Record(_, l)
            | PNode::Union(l)
            | PNode::Adt(_, l)
            | PNode::FnDef(_, l)
            | PNode::FnPtr(l, _)
            | PNode::Opaque(_, l)
            | PNode::Alias { args: l, .. }
            | PNode::Dynamic(l)
            | PNode::ExTrait(_, l) => Kids::List(list(l)?),
            PNode::Ref(_, c) | PNode::Ptr(_, c) | PNode::Slice(c) | PNode::Array(c, _) | PNode::Pat(c, _) => Kids::One(c),
            PNode::ExProj { args, term, .. } => Kids::ListThen(list(args)?, term),
            PNode::Closure { .. } | PNode::Future(..) | PNode::Coroutine { .. } => return Err(Unknown { node: ty, why: Why::BodyType }),
            PNode::Infer => return Err(Unknown { node: ty, why: Why::Hole }),
            PNode::Bool
            | PNode::Char
            | PNode::Int(_)
            | PNode::Uint(_)
            | PNode::Float(_)
            | PNode::Str
            | PNode::Never
            | PNode::Param(_)
            | PNode::Error
            | PNode::ConstValue { .. }
            | PNode::ConstParam(_)
            | PNode::ExAuto(_)
            | PNode::Foreign(_) => Kids::None,
        })
    }

    fn adt_variances(&self, adt: ItemKey) -> Option<&[Variance]> {
        self.variances.of(adt)
    }
}
