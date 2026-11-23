//! Regions beside the erased types: the one home of how a declared type's lifetimes are
//! numbered and counted (design review RT1, verdict B, piece P0).
//!
//! Types stay erased in the portable table. A declared type's lifetimes live in a REGION
//! COLUMN beside it ([`RegionCol`]): one [`Region`] per region position of the type, in the
//! type's own node order (pre-order, a node's own slots before its children's, list children
//! in order). How many slots a node has of its own is [`region_arity`], the ONE counter: the
//! head lowering that lays a column and the flow lowering that reads one both count with it,
//! so the two can never disagree by construction (the refusal `SigPositionsUnaligned` was two
//! counters that differed by design).
//!
//! The facts the counter needs and the portable node does not hold (an ADT's lifetime
//! parameters, an opaque's captured lifetimes, an associated type's lifetime arguments) come
//! through [`Arity`]. Where a fact is not known, the count is a typed [`NotKnown`] naming the
//! node, never a guessed number (rule 3).

use core::alloc::Allocator;

use blaze_own::Vec;

use crate::error::CheckError;
use crate::ids::{ItemKey, Name};
use crate::portable::{PNode, PTy, PTys};

/// The most nodes one walk of [`slots`] or [`RegionCol::lay`] visits: the bound the flow
/// lowering's position walks keep (blaze-flow-lower `MOST_POSITIONS`, the same value; rule 18).
pub const MOST_REGION_STEPS: u32 = 4096;

/// One region of a declared type's column.
///
/// `Param(i)` is the item's `i`-th lifetime: the container's (an impl's or a trait's) first,
/// then the item's own in DECLARATION order, then one anonymous parameter per elided input
/// lifetime, in input slot order ([`Numbering`] is the one rule). Never by first appearance.
///
/// `Late { binder, var }` is a region bound by a binder inside the type (`for<'a> ..`, a fn
/// pointer's elided lifetimes): `binder` is the binding node's index among the binders of the
/// laid type in its pre-order (absolute, not by depth), `var` the variable within it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Region {
    Static,
    Param(u32),
    Late { binder: u32, var: u32 },
}

/// An answer of [`Arity`]: a count, or a typed "not known".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Count {
    Known(u32),
    NotKnown,
}

/// The facts [`region_arity`] needs that a portable node does not hold. The head lowering
/// supplies them from what it lowered; the flow lowering from its `Facts`.
pub trait Arity {
    /// How many lifetime parameters the ADT `adt` declares (one slot each, in declared order).
    fn adt_lifetimes(&self, adt: ItemKey) -> Count;
    /// How many lifetimes the opaque type `opaque` captures (edition rules and `use<..>`
    /// applied).
    fn opaque_captures(&self, opaque: ItemKey) -> Count;
    /// For `<_ as trait_<..>>::name`: the trait's lifetime arguments plus the associated
    /// type's own lifetime parameters (a GAT's), the trait's first.
    fn alias_lifetimes(&self, trait_: ItemKey, name: Name) -> Count;
    /// How many lifetime parameters the trait `trait_` declares: the own slots of a trait
    /// object's principal and of a clause's trait (`ExTrait`), one each in declared order.
    fn trait_lifetimes(&self, trait_: ItemKey) -> Count;
}

/// Which fact [`Arity`] did not know.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Unknown {
    AdtLifetimes(ItemKey),
    OpaqueCaptures(ItemKey),
    AliasLifetimes { trait_: ItemKey, name: Name },
    TraitLifetimes(ItemKey),
}

/// A node whose own region count [`Arity`] could not answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NotKnown {
    pub node: PTy,
    pub what: Unknown,
}

/// Which group of a [`Numbering`] an index was asked of.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Group {
    Container,
    Own,
    Elided,
}

/// Why a column could not be counted, laid or numbered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RegionError {
    /// A node's own count is not known (`NotKnown::what` says which fact).
    NotKnown(NotKnown),
    /// A column for `ty` with `got` slots where the type has `want` (`RegionCol::lay`).
    Length { ty: PTy, want: u32, got: u32 },
    /// The walk of `ty` passed [`MOST_REGION_STEPS`] nodes (rule 18).
    Bound { ty: PTy, limit: u32 },
    /// The slot count of `ty` passed `u32::MAX`.
    Overflow { ty: PTy },
    /// A body-local type (a hole, a closure, an async block's future, a coroutine) at `node`:
    /// a column is laid over declared types only; a body's positions are the flow lowering's.
    BodyType { node: PTy },
    /// The portable table could not be read (a node or a list past its columns).
    Table(CheckError),
    /// `index` asked of `group` of a [`Numbering`] that has `count` there, or the numbering's
    /// total passed `u32::MAX` (`index == count == u32::MAX`).
    ParamPast { group: Group, index: u32, count: u32 },
    /// A lifetime parameter `index` past the item's `count` ([`LifetimeParams`]).
    LifetimePast { index: u32, count: u32 },
}

impl From<NotKnown> for RegionError {
    fn from(n: NotKnown) -> RegionError {
        RegionError::NotKnown(n)
    }
}

impl core::fmt::Display for RegionError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            RegionError::NotKnown(n) => write!(f, "regions: the region count of type node {} is not known: {:?}", n.node.0, n.what),
            RegionError::Length { ty, want, got } => {
                write!(f, "regions: a column of {got} slots for type node {} which has {want} (RT1, rule 76)", ty.0)
            }
            RegionError::Bound { ty, limit } => write!(f, "regions: the walk of type node {} passed {limit} nodes (rule 18)", ty.0),
            RegionError::Overflow { ty } => write!(f, "regions: the slot count of type node {} passed u32::MAX", ty.0),
            RegionError::BodyType { node } => {
                write!(f, "regions: type node {} is a body's type; a column is laid over declared types only", node.0)
            }
            RegionError::Table(e) => write!(f, "regions: {e}"),
            RegionError::ParamPast { group, index, count } => {
                write!(f, "regions: lifetime {index} of the {group:?} group, which has {count}")
            }
            RegionError::LifetimePast { index, count } => write!(f, "regions: lifetime parameter {index} of an item with {count}"),
        }
    }
}

/// THE ONE COUNTER: how many region slots `node` (the node of `t`) has of its own; its
/// children's are counted by the walk ([`slots`]). `&T` and `dyn ..` one (the reference's, the
/// object's lifetime bound), an ADT its lifetime parameters, an opaque its captured lifetimes,
/// an alias the trait's lifetime arguments plus the associated type's own, a trait (`ExTrait`)
/// its lifetime arguments, a projection bound (`ExProj`) the same as its alias; a fn pointer
/// none of its own (its inputs' and output's are its children's); every other node none.
pub fn region_arity<F: Arity + ?Sized>(t: PTy, node: PNode, facts: &F) -> Result<u32, NotKnown> {
    let known = |c: Count, what: Unknown| match c {
        Count::Known(n) => Ok(n),
        Count::NotKnown => Err(NotKnown { node: t, what }),
    };
    match node {
        PNode::Ref(..) | PNode::Dynamic(_) => Ok(1),
        PNode::Adt(k, _) => known(facts.adt_lifetimes(k), Unknown::AdtLifetimes(k)),
        PNode::Opaque(k, _) => known(facts.opaque_captures(k), Unknown::OpaqueCaptures(k)),
        PNode::Alias { trait_, name, .. } => known(facts.alias_lifetimes(trait_, name), Unknown::AliasLifetimes { trait_, name }),
        // A principal or a clause's trait: the trait's lifetime arguments. A projection bound
        // (`Item = T` in `dyn` or a clause): the trait's and the associated type's, as `Alias`.
        PNode::ExTrait(k, _) => known(facts.trait_lifetimes(k), Unknown::TraitLifetimes(k)),
        PNode::ExProj { trait_, name, .. } => known(facts.alias_lifetimes(trait_, name), Unknown::AliasLifetimes { trait_, name }),
        // `FnPtr`: none of its own; its binder binds regions of its children's slots (`Late`).
        // A closure's, an async block's and a coroutine's positions are a body's (captures),
        // never a declared type's: none of their own here.
        PNode::FnPtr(..)
        | PNode::Bool
        | PNode::Char
        | PNode::Int(_)
        | PNode::Uint(_)
        | PNode::Float(_)
        | PNode::Str
        | PNode::Never
        | PNode::Tuple(_)
        | PNode::Record(..)
        | PNode::Union(_)
        | PNode::Ptr(..)
        | PNode::Array(..)
        | PNode::Slice(_)
        | PNode::Pat(..)
        | PNode::FnDef(..)
        | PNode::Closure { .. }
        | PNode::Future(..)
        | PNode::Coroutine { .. }
        | PNode::Param(_)
        | PNode::Infer
        | PNode::Error
        | PNode::ConstValue { .. }
        | PNode::ConstParam(_)
        | PNode::ExAuto(_)
        | PNode::Foreign(_) => Ok(0),
    }
}

/// The type children of a node whose slots follow its own, in order.
enum Kids<'a> {
    None,
    One(PTy),
    Many(&'a [PTy]),
    /// A projection's arguments, then its term.
    ManyThen(&'a [PTy], PTy),
}

/// The children [`slots`] walks under `node`: every type child in declaration order. An array's
/// length, a pattern type's ranges and a const value are const terms, no type positions.
fn kids<'a>(tys: PTys<'a>, t: PTy, node: PNode) -> Result<Kids<'a>, RegionError> {
    let list = |l| tys.list(l).map_err(RegionError::Table);
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
        | PNode::ExTrait(_, l) => Kids::Many(list(l)?),
        PNode::Ref(_, c) | PNode::Ptr(_, c) | PNode::Slice(c) | PNode::Array(c, _) | PNode::Pat(c, _) => Kids::One(c),
        PNode::ExProj { args, term, .. } => Kids::ManyThen(list(args)?, term),
        PNode::Infer | PNode::Closure { .. } | PNode::Future(..) | PNode::Coroutine { .. } => {
            return Err(RegionError::BodyType { node: t });
        }
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

/// The pre-order walk of `ty`'s slots, `visit(node, its node, its own count)` called in slot
/// order; the total. `budget` counts the nodes left to visit, shared by the walks of one lay.
fn walk<A, F, V>(tys: PTys<'_>, ty: PTy, facts: &F, alloc: A, budget: &mut u32, visit: &mut V) -> Result<u32, RegionError>
where
    A: Allocator,
    F: Arity + ?Sized,
    V: FnMut(PTy, PNode, u32) -> Result<(), RegionError>,
{
    let mut stack: Vec<PTy, A> = Vec::new_in(alloc);
    stack.push(ty);
    let mut total = 0u32;
    while let Some(t) = stack.pop() {
        if *budget == 0 {
            return Err(RegionError::Bound { ty, limit: MOST_REGION_STEPS });
        }
        *budget -= 1;
        let node = tys.node(t).map_err(RegionError::Table)?;
        let own = region_arity(t, node, facts)?;
        let children = kids(tys, t, node)?;
        visit(t, node, own)?;
        total = total.checked_add(own).ok_or(RegionError::Overflow { ty })?;
        // Reversed onto the stack, so the first child is popped (and laid) first.
        match children {
            Kids::None => {}
            Kids::One(c) => stack.push(c),
            Kids::Many(l) => l.iter().rev().for_each(|&c| stack.push(c)),
            Kids::ManyThen(l, last) => {
                stack.push(last);
                l.iter().rev().for_each(|&c| stack.push(c));
            }
        }
    }
    Ok(total)
}

/// The number of region slots of the whole declared type `ty`: [`region_arity`] of each node,
/// in pre-order, list children in order. Bounded by [`MOST_REGION_STEPS`] nodes.
pub fn slots<A: Allocator, F: Arity + ?Sized>(tys: PTys<'_>, ty: PTy, facts: &F, alloc: A) -> Result<u32, RegionError> {
    let mut budget = MOST_REGION_STEPS;
    walk(tys, ty, facts, alloc, &mut budget, &mut |_, _, _| Ok(()))
}

/// [`slots`]'s walk with `visit(node, its node, its own count)` called per node in slot order:
/// the order a producer pushes slots in, so a column it lays is in the order [`RegionCol::lay`]
/// checks.
pub fn each_slot_node<A, F, V>(tys: PTys<'_>, ty: PTy, facts: &F, alloc: A, visit: &mut V) -> Result<u32, RegionError>
where
    A: Allocator,
    F: Arity + ?Sized,
    V: FnMut(PTy, PNode, u32) -> Result<(), RegionError>,
{
    let mut budget = MOST_REGION_STEPS;
    walk(tys, ty, facts, alloc, &mut budget, visit)
}

/// The region column of one declared type: exactly [`slots`] of it, a checked length (rule
/// 76). Its only maker is [`RegionCol::lay`].
#[derive(Debug)]
pub struct RegionCol<A: Allocator> {
    ty: PTy,
    slots: Vec<Region, A>,
    /// Where each of the root's children's slots start, then the end: `kids[0]` is the root's
    /// own count, child `k`'s slots are `kids[k]..kids[k + 1]`.
    kids: Vec<u32, A>,
}

impl<A: Allocator + Clone> RegionCol<A> {
    /// The column `slots` of `ty`, refused by [`RegionError::Length`] unless it has exactly
    /// [`slots`] of `ty` entries. The slots are in pre-order, list children in order.
    pub fn lay<F: Arity + ?Sized>(tys: PTys<'_>, ty: PTy, facts: &F, slots: &[Region], alloc: A) -> Result<RegionCol<A>, RegionError> {
        let root = tys.node(ty).map_err(RegionError::Table)?;
        let mut budget = MOST_REGION_STEPS - 1;
        let own = region_arity(ty, root, facts)?;
        let mut starts: Vec<u32, A> = Vec::new_in(alloc.clone());
        starts.push(own);
        let mut total = own;
        let mut add = |c: PTy, starts: &mut Vec<u32, A>, total: &mut u32| -> Result<(), RegionError> {
            let n = walk(tys, c, facts, alloc.clone(), &mut budget, &mut |_, _, _| Ok(()))?;
            *total = total.checked_add(n).ok_or(RegionError::Overflow { ty })?;
            starts.push(*total);
            Ok(())
        };
        match kids(tys, ty, root)? {
            Kids::None => {}
            Kids::One(c) => add(c, &mut starts, &mut total)?,
            Kids::Many(l) => {
                for &c in l {
                    add(c, &mut starts, &mut total)?;
                }
            }
            Kids::ManyThen(l, last) => {
                for &c in l {
                    add(c, &mut starts, &mut total)?;
                }
                add(last, &mut starts, &mut total)?;
            }
        }
        let got = u32::try_from(slots.len()).map_err(|_| RegionError::Overflow { ty })?;
        if got != total {
            return Err(RegionError::Length { ty, want: total, got });
        }
        let mut col: Vec<Region, A> = Vec::with_capacity_in(slots.len(), alloc);
        col.extend_from_slice(slots);
        Ok(RegionCol { ty, slots: col, kids: starts })
    }
}

impl<A: Allocator> RegionCol<A> {
    /// The type the column was laid over.
    pub fn ty(&self) -> PTy {
        self.ty
    }

    /// The number of slots.
    pub fn len(&self) -> u32 {
        self.kids.last().copied().unwrap_or(0)
    }

    /// Every slot, in slot order.
    pub fn as_slice(&self) -> &[Region] {
        &self.slots
    }

    /// The slot `i`; `None` past the column.
    pub fn slot(&self, i: u32) -> Option<Region> {
        self.slots.get(i as usize).copied()
    }

    /// The root node's own slots (before any child's).
    pub fn own(&self) -> &[Region] {
        let end = self.kids.first().copied().unwrap_or(0) as usize;
        self.slots.get(..end).unwrap_or(&[])
    }

    /// The number of the root's children (a fn signature laid as a fn pointer: its inputs,
    /// then its output).
    pub fn children(&self) -> u32 {
        (self.kids.len() as u32).saturating_sub(1)
    }

    /// The slots of the root's child `k`, in slot order; `None` past its children.
    pub fn child(&self, k: u32) -> Option<&[Region]> {
        let k = k as usize;
        let start = *self.kids.get(k)? as usize;
        let end = *self.kids.get(k + 1)? as usize;
        self.slots.get(start..end)
    }
}

/// THE ONE NUMBERING of an item's `Param` regions: the container's lifetimes first, then the
/// item's own in declaration order, then one per elided input lifetime in input slot order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Numbering {
    /// The container's (impl's or trait's) lifetime parameters.
    pub container: u32,
    /// The item's own lifetime parameters, as declared.
    pub own: u32,
    /// The anonymous parameters elision made, one per elided input lifetime.
    pub elided: u32,
}

impl Numbering {
    /// The item's lifetime parameter count: all three groups.
    pub fn count(&self) -> Result<u32, RegionError> {
        self.container
            .checked_add(self.own)
            .and_then(|n| n.checked_add(self.elided))
            .ok_or(RegionError::ParamPast { group: Group::Elided, index: u32::MAX, count: u32::MAX })
    }

    /// The container's `i`-th lifetime.
    pub fn container(&self, i: u32) -> Result<Region, RegionError> {
        self.at(Group::Container, i, self.container, 0)
    }

    /// The item's own `i`-th lifetime, in declaration order.
    pub fn own(&self, i: u32) -> Result<Region, RegionError> {
        self.at(Group::Own, i, self.own, self.container)
    }

    /// The `i`-th anonymous parameter of elision, in input slot order.
    pub fn elided(&self, i: u32) -> Result<Region, RegionError> {
        let before = self.container.checked_add(self.own).ok_or(RegionError::ParamPast { group: Group::Own, index: u32::MAX, count: u32::MAX })?;
        self.at(Group::Elided, i, self.elided, before)
    }

    fn at(&self, group: Group, i: u32, count: u32, before: u32) -> Result<Region, RegionError> {
        if i >= count {
            return Err(RegionError::ParamPast { group, index: i, count });
        }
        before.checked_add(i).map(Region::Param).ok_or(RegionError::ParamPast { group, index: i, count })
    }
}

/// An item's lifetime parameters: how many (`Numbering::count`), which are late-bound (a
/// lifetime that appears in no where-clause or bound and does appear in the inputs: what a fn
/// item to fn pointer coercion re-binds and a turbofish may not name), and the declared
/// `'a: 'b` among them and `'static` (`(longer, shorter)`).
#[derive(Debug)]
pub struct LifetimeParams<A: Allocator> {
    count: u32,
    /// One bit per parameter, `Param(i)` at bit `i % 64` of word `i / 64`.
    late: Vec<u64, A>,
    outlives: Vec<(Region, Region), A>,
}

impl<A: Allocator + Clone> LifetimeParams<A> {
    /// `count` parameters, all early-bound, no outlives clause.
    pub fn new_in(count: u32, alloc: A) -> LifetimeParams<A> {
        let words = (count as usize).div_ceil(64);
        LifetimeParams { count, late: Vec::from_elem_in(0u64, words, alloc.clone()), outlives: Vec::new_in(alloc) }
    }
}

impl<A: Allocator> LifetimeParams<A> {
    /// The number of lifetime parameters.
    pub fn count(&self) -> u32 {
        self.count
    }

    /// Marks `Param(i)` late-bound.
    pub fn set_late(&mut self, i: u32) -> Result<(), RegionError> {
        self.check(i)?;
        if let Some(w) = self.late.get_mut((i / 64) as usize) {
            *w |= 1u64 << (i % 64);
        }
        Ok(())
    }

    /// Whether `Param(i)` is late-bound.
    pub fn is_late(&self, i: u32) -> Result<bool, RegionError> {
        self.check(i)?;
        Ok(self.late.get((i / 64) as usize).is_some_and(|w| w & (1u64 << (i % 64)) != 0))
    }

    /// Adds the clause `longer: shorter`; each a `Param` of this item, `'static`, or a `Late`.
    pub fn add_outlives(&mut self, longer: Region, shorter: Region) -> Result<(), RegionError> {
        for r in [longer, shorter] {
            if let Region::Param(i) = r {
                self.check(i)?;
            }
        }
        self.outlives.push((longer, shorter));
        Ok(())
    }

    /// The declared outlives clauses, `(longer, shorter)`, in the order added.
    pub fn outlives(&self) -> &[(Region, Region)] {
        &self.outlives
    }

    fn check(&self, i: u32) -> Result<(), RegionError> {
        if i >= self.count {
            return Err(RegionError::LifetimePast { index: i, count: self.count });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::{FnPtrHeader, List, Mutbl, UintTy};
    use crate::portable::PortableTys;
    use blaze_own::Alloc;

    const FOO: ItemKey = ItemKey(0xf00);
    const TR: ItemKey = ItemKey(0x7a);
    const S: ItemKey = ItemKey(0x5);
    const OPAQUE: ItemKey = ItemKey(0x0a);

    /// `struct Foo<'b, 'c, T>` and `struct S<'x>`; no opaque or alias is known.
    struct Toy;

    impl Arity for Toy {
        fn adt_lifetimes(&self, adt: ItemKey) -> Count {
            match adt {
                FOO => Count::Known(2),
                S => Count::Known(1),
                _ => Count::NotKnown,
            }
        }
        fn opaque_captures(&self, _: ItemKey) -> Count {
            Count::NotKnown
        }
        fn alias_lifetimes(&self, _: ItemKey, _: Name) -> Count {
            Count::NotKnown
        }
        /// `trait Tr` has no lifetime parameter.
        fn trait_lifetimes(&self, trait_: ItemKey) -> Count {
            if trait_ == TR { Count::Known(0) } else { Count::NotKnown }
        }
    }

    fn table() -> PortableTys<'static> {
        PortableTys::new_in(Alloc::global())
    }

    fn reference(t: &mut PortableTys<'_>, to: PTy) -> PTy {
        t.intern(PNode::Ref(Mutbl::Not, to)).unwrap()
    }

    fn u8_ty(t: &mut PortableTys<'_>) -> PTy {
        t.intern(PNode::Uint(UintTy::U8)).unwrap()
    }

    fn lay(t: &PortableTys<'_>, ty: PTy, col: &[Region]) -> Result<RegionCol<Alloc<'static>>, RegionError> {
        RegionCol::lay(t.view(), ty, &Toy, col, Alloc::global())
    }

    /// The node order of each slot of `ty`: which node laid it.
    fn slot_nodes(t: &PortableTys<'_>, ty: PTy) -> std::vec::Vec<PTy> {
        let mut out = std::vec::Vec::new();
        each_slot_node(t.view(), ty, &Toy, Alloc::global(), &mut |n, _, own| {
            for _ in 0..own {
                out.push(n);
            }
            Ok(())
        })
        .unwrap();
        out
    }

    /// `&'a Foo<'b, 'c, T>`: three slots, the reference's then Foo's two.
    #[test]
    fn ref_to_adt_with_two_lifetimes() {
        let mut t = table();
        let p = t.intern(PNode::Param(0)).unwrap();
        let args = t.intern_list(&[p]).unwrap();
        let foo = t.intern(PNode::Adt(FOO, args)).unwrap();
        let r = reference(&mut t, foo);
        assert_eq!(region_arity(r, PNode::Ref(Mutbl::Not, foo), &Toy), Ok(1));
        assert_eq!(region_arity(foo, PNode::Adt(FOO, args), &Toy), Ok(2));
        assert_eq!(region_arity(p, PNode::Param(0), &Toy), Ok(0));
        assert_eq!(slots(t.view(), r, &Toy, Alloc::global()), Ok(3));
        assert_eq!(slot_nodes(&t, r), [r, foo, foo]);
        let (a, b, c) = (Region::Param(0), Region::Param(1), Region::Param(2));
        let col = lay(&t, r, &[a, b, c]).unwrap();
        assert_eq!(col.own(), [a]);
        assert_eq!(col.child(0), Some(&[b, c][..]));
        assert_eq!(col.child(1), None);
    }

    /// `fn(&u8) -> &u8`: two slots, none the fn pointer's own.
    #[test]
    fn fn_pointer_has_none_of_its_own() {
        let mut t = table();
        let u = u8_ty(&mut t);
        let r = reference(&mut t, u);
        let io = t.intern_list(&[r, r]).unwrap();
        let f = t.intern(PNode::FnPtr(io, FnPtrHeader::RUST)).unwrap();
        assert_eq!(region_arity(f, PNode::FnPtr(io, FnPtrHeader::RUST), &Toy), Ok(0));
        assert_eq!(slots(t.view(), f, &Toy, Alloc::global()), Ok(2));
        assert_eq!(slot_nodes(&t, f), [r, r]);
        let late = Region::Late { binder: 0, var: 0 };
        let col = lay(&t, f, &[late, late]).unwrap();
        assert!(col.own().is_empty());
        assert_eq!(col.children(), 2);
        assert_eq!(col.child(0), Some(&[late][..]));
        assert_eq!(col.child(1), Some(&[late][..]));
    }

    /// `dyn Tr + 'a`: one slot, the object's lifetime bound.
    #[test]
    fn trait_object_has_one() {
        let mut t = table();
        let principal = t.intern(PNode::ExTrait(TR, List::EMPTY)).unwrap();
        let d = t.dynamic(&[principal]).unwrap();
        assert_eq!(slots(t.view(), d, &Toy, Alloc::global()), Ok(1));
        assert_eq!(slot_nodes(&t, d), [d]);
        assert_eq!(lay(&t, d, &[Region::Param(0)]).unwrap().own(), [Region::Param(0)]);
    }

    /// `(&'a u8, &'b u16)`: two slots, in the tuple's order.
    #[test]
    fn tuple_of_two_references_in_order() {
        let mut t = table();
        let u = u8_ty(&mut t);
        let w = t.intern(PNode::Uint(UintTy::U16)).unwrap();
        let ra = reference(&mut t, u);
        let rb = reference(&mut t, w);
        let l = t.intern_list(&[ra, rb]).unwrap();
        let tup = t.intern(PNode::Tuple(l)).unwrap();
        assert_eq!(slots(t.view(), tup, &Toy, Alloc::global()), Ok(2));
        assert_eq!(slot_nodes(&t, tup), [ra, rb]);
        let (a, b) = (Region::Param(0), Region::Param(1));
        let col = lay(&t, tup, &[a, b]).unwrap();
        assert_eq!(col.child(0), Some(&[a][..]));
        assert_eq!(col.child(1), Some(&[b][..]));
        assert_eq!(col.as_slice(), [a, b]);
    }

    /// A column one slot short and one slot long is refused by length, naming the type.
    #[test]
    fn lay_refuses_a_wrong_length() {
        let mut t = table();
        let u = u8_ty(&mut t);
        let ra = reference(&mut t, u);
        let l = t.intern_list(&[ra, ra]).unwrap();
        let tup = t.intern(PNode::Tuple(l)).unwrap();
        let a = Region::Param(0);
        assert_eq!(lay(&t, tup, &[a]).unwrap_err(), RegionError::Length { ty: tup, want: 2, got: 1 });
        assert_eq!(lay(&t, tup, &[a, a, a]).unwrap_err(), RegionError::Length { ty: tup, want: 2, got: 3 });
        assert!(lay(&t, tup, &[a, a]).is_ok());
    }

    /// A fact the counter does not know is a typed error naming the node, never a count.
    #[test]
    fn unknown_count_is_named() {
        let mut t = table();
        let o = t.intern(PNode::Opaque(OPAQUE, List::EMPTY)).unwrap();
        let r = reference(&mut t, o);
        let want = RegionError::NotKnown(NotKnown { node: o, what: Unknown::OpaqueCaptures(OPAQUE) });
        assert_eq!(slots(t.view(), r, &Toy, Alloc::global()), Err(want));
        assert_eq!(lay(&t, r, &[Region::Static]).unwrap_err(), want);
    }

    /// `impl<'x> S<'x> { fn f<'a, 'b>(&self, y: &'b u8, x: &'a u8) -> &'a u8 }`, laid as the
    /// fn pointer of its inputs then its output: `'x` is `Param(0)` (the container's first),
    /// `'a` `Param(1)` and `'b` `Param(2)` (declaration order, not first appearance: `'b` is
    /// written first), the elided `&self` lifetime `Param(3)`.
    #[test]
    fn param_numbering_by_declaration() {
        let n = Numbering { container: 1, own: 2, elided: 1 };
        assert_eq!(n.count(), Ok(4));
        let x = n.container(0).unwrap();
        let a = n.own(0).unwrap();
        let b = n.own(1).unwrap();
        let anon = n.elided(0).unwrap();
        assert_eq!([x, a, b, anon], [Region::Param(0), Region::Param(1), Region::Param(2), Region::Param(3)]);
        assert_eq!(n.own(2), Err(RegionError::ParamPast { group: Group::Own, index: 2, count: 2 }));

        let mut t = table();
        let p = t.intern(PNode::Param(0)).unwrap();
        let s_args = t.intern_list(&[p]).unwrap();
        let s = t.intern(PNode::Adt(S, s_args)).unwrap();
        let self_ = reference(&mut t, s);
        let u = u8_ty(&mut t);
        let ru = reference(&mut t, u);
        let io = t.intern_list(&[self_, ru, ru, ru]).unwrap();
        let sig = t.intern(PNode::FnPtr(io, FnPtrHeader::RUST)).unwrap();
        let col = lay(&t, sig, &[anon, x, b, a, a]).unwrap();
        assert_eq!(col.child(0), Some(&[Region::Param(3), Region::Param(0)][..]));
        assert_eq!(col.child(1), Some(&[Region::Param(2)][..]));
        assert_eq!(col.child(2), Some(&[Region::Param(1)][..]));
        assert_eq!(col.child(3), Some(&[Region::Param(1)][..]));

        let mut lp = LifetimeParams::new_in(n.count().unwrap(), Alloc::global());
        lp.set_late(1).unwrap();
        assert_eq!(lp.add_outlives(a, b), Ok(()));
        assert_eq!(lp.add_outlives(Region::Param(4), x), Err(RegionError::LifetimePast { index: 4, count: 4 }));
        assert_eq!(lp.outlives(), [(a, b)]);
        assert_eq!(lp.is_late(1), Ok(true));
        assert_eq!(lp.is_late(0), Ok(false));
        assert_eq!(lp.set_late(4), Err(RegionError::LifetimePast { index: 4, count: 4 }));
    }
}
