//! The region columns of a head's clauses, an ADT's fields, an impl's header and an opaque
//! type's captures (D148, docs/v2/plan/design/flow.md "Lifetimes are produced once and carried
//! beside the types"; piece L1b). The fn signature's own column is types.rs's and heads.rs's
//! (piece L1a); this file fills the rest of a [`HeadRegions`]: `clauses`, `outlives`,
//! `fields`, `impl_header`, `impl_assoc` (each `type X = U;` value of an impl) and
//! `opaque_captures`.
//!
//! The one home of counting, laying and numbering is blaze-typeck regions.rs; nothing here
//! counts a type's slots another way. A written type's slots come from the head lowering's
//! walk that interned it ([`Written`], L1a's `RegionSink`), in exactly the order
//! `regions::each_slot_node` walks; what this file adds is what the lifetimes NAME: a clause's
//! `for<..>` variables (`Late { binder: 0, var }`), the item's parameters (`Param(i)` by the
//! [`Scope`], declaration order), `'static`, and what an elided lifetime is where elision has a
//! rule (an impl header's fresh parameters, `Fn(..)` sugar's late inputs and its output).
//! Where none applies the language's error is a typed [`RegionsRefusal`] (E0106, E0261, E0637,
//! E0107), never a guess (rule 3).
//!
//! A clause's column is laid over the clause as one portable node: `T: Tr<A..>` over
//! `ExTrait(Tr, [T, A..])` (the trait's own lifetime arguments, then each argument's slots),
//! `<X as Tr>::N == U` over `ExProj { Tr, N, [X, A..], U }`, `T: 'r` over `T`. `'a: 'b` and a
//! `where const` have no type to lay: the first is in [`HeadRegions::outlives`] only.
//!
//! Binders. A clause's own variables (its where predicate's `for<..>`, then its bound's, then
//! one per elided input of `Fn(..)` sugar) are `Late { binder: 0, var }` and are counted in
//! `ClauseRegions::binders`; a binder inside the clause's types (a fn pointer's) is numbered
//! after it: each written type's inner binders, as the sink numbers them from 0 within that
//! type, are shifted past every binder before them in the column ([`St::next_binder`]).

use blaze_lower::{View, form as lf, head};
use blaze_own::{Alloc, Vec};
use blaze_surface::{Edition, Range};
use blaze_typeck::region_facts::{ClauseRegions, HeadRegions, Outlives};
use blaze_typeck::regions::{Arity, LifetimeParams, Region, RegionCol, RegionError, each_slot_node, region_arity, slots as slot_count};
use blaze_typeck::{ItemKey, List, PClause, PNode, PTy, PTys, PortableTys};

use super::preds::Preds;
use super::types::{NotLaidWhy, RegionRefusal, is_binder};

/// The allocator a head's regions are kept in: the store's.
type Mem = Alloc<'static>;

/// The most type nodes [`captures`] walks in one opaque type's bounds (rule 18).
const MOST_CAPTURE_STEPS: u32 = 4096;

// ---------------------------------------------------------------------------------------------
// What a clause was written as (recorded by preds.rs beside each clause).

/// The `for<..>` lists a clause is written under (each rows of [`lf::GenericParam`], lifetimes
/// only: preds.rs `lifetimes_only` refuses the rest): a where predicate's, then its bound's.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Binders {
    pub outer: lf::List,
    pub inner: lf::List,
}

impl Binders {
    /// No `for<..>`.
    pub const NONE: Binders = Binders { outer: List::EMPTY, inner: List::EMPTY };

    /// A where predicate's `for<..>`.
    pub fn outer(outer: lf::List) -> Binders {
        Binders { outer, inner: List::EMPTY }
    }

    /// These binders with a bound's own `for<..>` added: one bound under one where predicate
    /// has two lists at most. A third (a `for<..>` on an associated type bound nested in a
    /// bound that has one) is refused by name.
    pub fn with(self, bound: lf::List) -> Result<Binders, RegionsRefusal> {
        if bound.len == 0 {
            return Ok(self);
        }
        if self.inner.len == 0 {
            return Ok(Binders { outer: self.outer, inner: bound });
        }
        Err(RegionsRefusal::Unsupported { what: "a `for<..>` on an associated type bound inside a bound that has its own `for<..>`", at: Range::new(0, 0) })
    }
}

/// The `Self` of a clause, as written.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum On {
    /// A type written in the item's text: a where predicate's bounded type.
    Written(lf::TypeId),
    /// A type the lowering made with no text of its own: a type parameter, `Self`, an
    /// anonymous `impl Trait` parameter, the item's own opaque or associated type. Its column
    /// is empty, or the head's own ([`Cx::made`]); any other is refused by name.
    Made,
    /// `<Self as Owner>::Name` of the `Name: Bounds` written in the bound whose trait clause is
    /// clause `of`; `gat` the lifetime arguments written on `Name` (`Name<'a>: B`).
    Nested { of: u32, gat: lf::List },
}

/// What one clause of [`Preds::clauses`] was written as: what its column is laid from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Src {
    /// The trait clause of the trait bound whose path type is `bound`, on `on`.
    Trait { on: On, bound: lf::TypeId, binders: Binders },
    /// The projection of a `Name = T` (or `Fn(..) -> T`'s `Output`) of the bound whose trait
    /// clause is clause `of`: `term` the written type (`None`: the `()` of `Fn(..)` with no
    /// `->`), `gat` the lifetime arguments written on `Name`.
    Binding { of: u32, term: Option<lf::TypeId>, gat: lf::List },
    /// `on: 'lifetime`.
    TypeOutlives { on: On, lifetime: lf::NameIdx, binders: Binders },
    /// `'long: 'short`.
    RegionOutlives { long: lf::NameIdx, short: lf::NameIdx },
    /// A clause made from others with no text of its own (an implied `Sized`, a trait alias's
    /// expansion, a generic associated type binding with type arguments, a `where const`): its
    /// column is empty, and one whose types carry lifetimes is refused by name.
    Derived,
    /// The container's clause `nth` (an impl's, a trait's), inherited by a member: its column
    /// is the container's (the member's numbering puts the container's lifetimes first).
    Inherited { nth: u32 },
}

// ---------------------------------------------------------------------------------------------
// Refusals.

/// Why a head's regions were refused: the language's error by its code, or a construct by name
/// (rule 19). The caller names the item (types.rs `Why::Regions`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RegionsRefusal {
    /// E0106: a lifetime is elided where no elision rule gives one (a field's `&T`, `Fn(&A,
    /// &B) -> &C`, a bound on a trait with lifetime parameters written with none).
    Missing { at: Range },
    /// E0261: a lifetime no parameter, binder or `'static` declares.
    Undeclared { at: Range },
    /// E0637: `'_` or `&` with no lifetime where a name is required (a bound, a where clause).
    NotHere { at: Range },
    /// E0107: a trait path written with `got` lifetime arguments where it takes `want`.
    LifetimeArgs { at: Range, want: u32, got: u32 },
    /// Clause `nth` was made with no text of its own and its types carry lifetimes this head
    /// does not know (a trait alias's expansion, a defaulted argument, a GAT binding).
    Derived { nth: u32 },
    /// An impl header's defaulted trait argument whose type carries lifetimes.
    HeaderDefault { at: Range },
    /// Clause `nth`'s written arguments do not line up with its list (a producer's bug).
    Unaligned { nth: u32 },
    /// The producer's clause sources are out of step with its clauses (a producer's bug).
    Sources { clauses: u32, sources: u32 },
    /// A member's inherited clause `nth` that its container's regions hold no column of.
    Inherited { nth: u32 },
    /// An ADT's fields not in strict (variant, field) order (a producer's bug).
    FieldOrder { variant: u32, field: u32 },
    /// A construct this producer does not take yet, by its Rust name.
    Unsupported { what: &'static str, at: Range },
    /// The lowered text could not be read: what was being read.
    Text { what: &'static str },
    /// Counting, laying or numbering (blaze-typeck regions.rs).
    Region(RegionError),
    /// The type table is full.
    Table,
    /// A written type's slot names inner binder `binder` of a type with `binders` binder nodes:
    /// the walk's numbering and the column's disagree (a producer's bug).
    InnerBinder { binder: u32, binders: u32 },
    /// The head lowering's walk laid no slots for a written type this producer reads (types.rs
    /// `RegionSink`'s refusal, or the type was not walked).
    Walk(RegionRefusal),
    /// Where the head lowering's numbering (L1a's, heads.rs) and this file's meet, they
    /// disagree (a producer's bug): `what` names the place.
    Disagree { what: &'static str },
    /// E0201: an impl defines the associated type `name` twice.
    AssocTwice { name: blaze_typeck::Name },
}

impl From<RegionError> for RegionsRefusal {
    fn from(e: RegionError) -> RegionsRefusal {
        RegionsRefusal::Region(e)
    }
}

impl core::fmt::Display for RegionsRefusal {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            RegionsRefusal::Missing { at } => write!(f, "missing lifetime specifier at {at:?} (E0106; D148)"),
            RegionsRefusal::Undeclared { at } => write!(f, "use of an undeclared lifetime name at {at:?} (E0261; D148)"),
            RegionsRefusal::NotHere { at } => write!(f, "`'_` or `&` without a lifetime name cannot be used here, at {at:?} (E0637; D148)"),
            RegionsRefusal::LifetimeArgs { at, want, got } => {
                write!(f, "a trait path written with {got} lifetime arguments where it takes {want}, at {at:?} (E0107; D148)")
            }
            RegionsRefusal::Derived { nth } => {
                write!(f, "clause {nth} was made from other clauses and its types carry lifetimes this head does not know (D148)")
            }
            RegionsRefusal::HeaderDefault { at } => write!(f, "an impl header's defaulted trait argument carries lifetimes, at {at:?} (D148)"),
            RegionsRefusal::Unaligned { nth } => write!(f, "clause {nth}'s written arguments do not line up with its list (D148, a producer's bug)"),
            RegionsRefusal::Sources { clauses, sources } => {
                write!(f, "{clauses} clauses with {sources} sources: the producer's lists are out of step (D148, a producer's bug)")
            }
            RegionsRefusal::Inherited { nth } => write!(f, "inherited clause {nth} has no column in the container's regions (D148)"),
            RegionsRefusal::FieldOrder { variant, field } => {
                write!(f, "field {field} of variant {variant} is not in (variant, field) order (D148, a producer's bug)")
            }
            RegionsRefusal::Unsupported { what, at } => write!(f, "{what} at {at:?}: not taken by the region producer yet (D148)"),
            RegionsRefusal::Text { what } => write!(f, "the lowered text could not be read: {what} (D148)"),
            RegionsRefusal::Region(e) => write!(f, "{e}"),
            RegionsRefusal::Table => f.write_str("the type table is full (D148)"),
            RegionsRefusal::InnerBinder { binder, binders } => {
                write!(f, "a written type's slot names inner binder {binder} of a type with {binders} binder nodes (D148, a producer's bug)")
            }
            RegionsRefusal::Walk(why) => write!(f, "a written type's slots were not laid by the head's walk: {why} (D148)"),
            RegionsRefusal::Disagree { what } => write!(f, "the head's two region numberings disagree at {what} (D148, a producer's bug)"),
            RegionsRefusal::AssocTwice { name } => write!(f, "the associated type {name:?} is defined twice in one impl (E0201; D148)"),
        }
    }
}

fn read<T, E>(r: Result<T, E>, what: &'static str) -> Result<T, RegionsRefusal> {
    r.map_err(|_| RegionsRefusal::Text { what })
}

fn name_site(v: &View<'_>, n: lf::NameIdx) -> Result<Range, RegionsRefusal> {
    let r = read(v.name_range(n), "a lifetime's range")?;
    read(v.site(r), "a lifetime's range")
}

fn type_site(v: &View<'_>, t: lf::TypeId) -> Result<Range, RegionsRefusal> {
    let r = read(v.type_range(t), "a type's range")?;
    read(v.site(r), "a type's range")
}

fn table<T>(r: Result<T, blaze_typeck::CheckError>) -> Result<T, RegionsRefusal> {
    r.map_err(|e| RegionsRefusal::Region(RegionError::Table(e)))
}

fn index(i: usize) -> Result<u32, RegionsRefusal> {
    u32::try_from(i).map_err(|_| RegionsRefusal::Region(RegionError::LifetimePast { index: u32::MAX, count: u32::MAX }))
}

// ---------------------------------------------------------------------------------------------
// Names.

/// The lifetimes an item's clauses and types name: the container's, then the item's own in
/// declaration order (`regions::Numbering`'s first two groups), each its `Param`.
#[derive(Clone, Debug)]
pub struct Scope {
    /// (the name's hash, as `View::name` gives it, its region), innermost last.
    names: Vec<(u64, Region), Mem>,
    /// The `Param`s numbered so far: the container's and the item's own.
    params: u32,
    /// What `'_` names here: `None` where a name is required (a where clause, a generic
    /// parameter's bound: E0637, refused softly by name); the output rule's answer in a return
    /// opaque's bounds (`-> impl Trait + '_`, heads.rs `output_elided`), or why it gives none.
    elided: Option<Result<Region, RegionRefusal>>,
}

impl Scope {
    /// No lifetime in scope.
    pub fn new_in(alloc: Mem) -> Scope {
        Scope { names: Vec::new_in(alloc), params: 0, elided: None }
    }

    /// A copy of this scope in `alloc`.
    pub fn clone_in(&self, alloc: Mem) -> Scope {
        Scope { names: self.names.clone_in(alloc), params: self.params, elided: self.elided }
    }

    /// This scope, with `'_` naming `elided` (the position's elision rule's answer, or why
    /// it has none): a return opaque's bounds' scope.
    pub fn with_elided(&self, elided: Result<Region, RegionRefusal>) -> Scope {
        let mut s = self.clone_in(self.alloc());
        s.elided = Some(elided);
        s
    }

    /// `container`'s names (none for a free item), then the item's own lifetime parameters of
    /// `v` (its GENERICS) numbered from `base` in declaration order. `base` is the container's
    /// lifetime count: an impl's own and its header's elided ones (the impl's
    /// `HeadRegions::params.count()`), a trait's own.
    pub fn item(v: &View<'_>, container: Option<&Scope>, base: u32, alloc: Mem) -> Result<Scope, RegionsRefusal> {
        let mut s = match container {
            Some(c) => c.clone_in(alloc),
            None => Scope::new_in(alloc),
        };
        s.params = base;
        for &n in read(v.rows(v.list(head::GENERICS)), "an item's generics")? {
            if let lf::GenericParam::Lifetime { name, .. } = read(lf::GenericParam::decode(n), "a generic parameter")? {
                let h = read(v.name(name), "a lifetime's name")?;
                s.names.push((h, Region::Param(s.params)));
                s.params = s.params.checked_add(1).ok_or(RegionError::LifetimePast { index: u32::MAX, count: u32::MAX })?;
            }
        }
        Ok(s)
    }

    /// This scope extended with a GAT's own lifetime parameters of `v` (its GENERICS), each
    /// mapped to `Region::Late { binder: u32::MAX, var: j }` so `impl_assoc` can lay GAT values
    /// without altering the impl's own `Param` count.
    pub fn with_gat_lifetimes(&self, v: &View<'_>, alloc: Mem) -> Result<Scope, RegionsRefusal> {
        let mut s = self.clone_in(alloc);
        let mut gat = 0u32;
        for &n in read(v.rows(v.list(head::GENERICS)), "a GAT's generics")? {
            if let lf::GenericParam::Lifetime { name, .. } = read(lf::GenericParam::decode(n), "a GAT generic parameter")? {
                let h = read(v.name(name), "a GAT lifetime's name")?;
                s.names.push((h, Region::Late { binder: u32::MAX, var: gat }));
                gat = gat.checked_add(1).ok_or(RegionError::LifetimePast { index: u32::MAX, count: u32::MAX })?;
            }
        }
        Ok(s)
    }

    /// A container's scope from its filed group (heads.rs `ContainerRegions`): `names[i]` (a
    /// declared lifetime's hash, declaration order) is `Param(i)`, and `params` counts its
    /// declared and its header's elided lifetimes. More names than `params` is the producer's
    /// bug, refused.
    pub fn container(names: &[blaze_typeck::Name], params: u32, alloc: Mem) -> Result<Scope, RegionsRefusal> {
        let n = index(names.len())?;
        if n > params {
            return Err(RegionError::LifetimePast { index: n, count: params }.into());
        }
        let mut s = Scope { names: Vec::with_capacity_in(names.len(), alloc), params, elided: None };
        for (i, &h) in names.iter().enumerate() {
            s.names.push((h.0, Region::Param(index(i)?)));
        }
        Ok(s)
    }

    /// The number of `Param`s the scope numbers.
    pub fn params(&self) -> u32 {
        self.params
    }

    /// The allocator the scope is kept in.
    pub fn alloc(&self) -> Mem {
        *self.names.allocator()
    }

    fn find(&self, name: u64) -> Option<Region> {
        self.names.iter().rev().find(|e| e.0 == name).map(|e| e.1)
    }
}

/// A clause's own binder: its `for<..>` names, then the variables elision made (`Fn(..)`
/// sugar's elided inputs), each `Late { binder: 0, var }`.
#[derive(Clone, Debug)]
struct ClauseBinder {
    names: Vec<u64, Mem>,
    vars: u32,
}

impl ClauseBinder {
    fn none(alloc: Mem) -> ClauseBinder {
        ClauseBinder { names: Vec::new_in(alloc), vars: 0 }
    }

    fn clone_in(&self, alloc: Mem) -> ClauseBinder {
        ClauseBinder { names: self.names.clone_in(alloc), vars: self.vars }
    }

    fn of(v: &View<'_>, b: Binders, alloc: Mem) -> Result<ClauseBinder, RegionsRefusal> {
        let mut names = Vec::new_in(alloc);
        for list in [b.outer, b.inner] {
            for &n in read(v.rows(list), "a `for<..>` binder")? {
                match read(lf::GenericParam::decode(n), "a `for<..>` binder's parameter")? {
                    lf::GenericParam::Lifetime { name, .. } => names.push(read(v.name(name), "a binder's lifetime name")?),
                    _ => return Err(RegionsRefusal::Unsupported { what: "a type or const in a `for<..>` binder (non-lifetime binders)", at: Range::new(0, 0) }),
                }
            }
        }
        let vars = index(names.len())?;
        Ok(ClauseBinder { names, vars })
    }

    fn find(&self, name: u64) -> Option<Region> {
        let i = self.names.iter().position(|&n| n == name)?;
        u32::try_from(i).ok().map(|var| Region::Late { binder: 0, var })
    }

    fn fresh(&mut self) -> Result<Region, RegionsRefusal> {
        let var = self.vars;
        self.vars = var.checked_add(1).ok_or(RegionError::LifetimePast { index: u32::MAX, count: u32::MAX })?;
        Ok(Region::Late { binder: 0, var })
    }
}

/// What a written lifetime name is.
enum Named {
    Is(Region),
    /// `'_`: what it is depends on where it is written.
    Underscore(Range),
}

fn named(v: &View<'_>, scope: &Scope, b: &ClauseBinder, n: lf::NameIdx) -> Result<Named, RegionsRefusal> {
    let text = read(v.name_text(n), "a lifetime's text")?;
    if text == "'static" {
        return Ok(Named::Is(Region::Static));
    }
    if text == "'_" {
        return match scope.elided {
            Some(Ok(r)) => Ok(Named::Is(r)),
            Some(Err(why)) => Err(RegionsRefusal::Walk(why)),
            None => Ok(Named::Underscore(name_site(v, n)?)),
        };
    }
    let h = read(v.name(n), "a lifetime's name")?;
    if let Some(r) = b.find(h).or_else(|| scope.find(h)) {
        return Ok(Named::Is(r));
    }
    Err(RegionsRefusal::Undeclared { at: name_site(v, n)? })
}

/// The region a lifetime written in a clause names (`T: 'a`, `'a: 'b`, under `binders`): a
/// variable of the clause's binder, a parameter of `scope`, or `'static`. `'_` is E0637 there.
/// What preds.rs puts in `PClause::TypeOutlives` and `RegionOutlives`.
pub fn lifetime(v: &View<'_>, scope: &Scope, binders: Binders, n: lf::NameIdx) -> Result<Region, RegionsRefusal> {
    let b = ClauseBinder::of(v, binders, scope.alloc())?;
    match named(v, scope, &b, n)? {
        Named::Is(r) => Ok(r),
        Named::Underscore(at) => Err(RegionsRefusal::NotHere { at }),
    }
}

/// The written term and the lifetime arguments written on the name of each `Name = T` the
/// segment `seg` writes, in the order preds.rs `segment_args` gives its bindings: `Fn(..) ->
/// T`'s output only, or each `Name = T` whose arguments are lifetimes only (a binding with type
/// arguments is gat.rs's, made as a [`Src::Derived`] clause).
pub fn binding_terms(v: &View<'_>, seg: &lf::Segment, alloc: Mem) -> Result<Vec<(Option<lf::TypeId>, lf::List), Mem>, RegionsRefusal> {
    let mut out = Vec::new_in(alloc);
    if seg.parenthesized {
        out.push((seg.ty, List::EMPTY));
        return Ok(out);
    }
    if !seg.has_args {
        return Ok(out);
    }
    for &n in read(v.rows(seg.args), "a segment's arguments")? {
        if let lf::GenericArg::AssocType { args, ty, .. } = read(lf::GenericArg::decode(n), "a generic argument")?
            && lifetimes_only(v, args)?
        {
            out.push((Some(ty), args));
        }
    }
    Ok(out)
}

/// The lifetime arguments written on the name of each `Name: Bounds` the segment writes, in
/// the order preds.rs `segment_args` gives them (those whose arguments are lifetimes only).
pub fn nested_gats(v: &View<'_>, seg: &lf::Segment, alloc: Mem) -> Result<Vec<lf::List, Mem>, RegionsRefusal> {
    let mut out = Vec::new_in(alloc);
    if seg.parenthesized || !seg.has_args {
        return Ok(out);
    }
    for &n in read(v.rows(seg.args), "a segment's arguments")? {
        if let lf::GenericArg::AssocBound { args, .. } = read(lf::GenericArg::decode(n), "a generic argument")?
            && lifetimes_only(v, args)?
        {
            out.push(args);
        }
    }
    Ok(out)
}

fn lifetimes_only(v: &View<'_>, args: lf::List) -> Result<bool, RegionsRefusal> {
    for &n in read(v.rows(args), "generic arguments")? {
        if !matches!(read(lf::GenericArg::decode(n), "a generic argument")?, lf::GenericArg::Lifetime(_)) {
            return Ok(false);
        }
    }
    Ok(true)
}

fn last_segment(v: &View<'_>, path: lf::TypeId) -> Result<lf::Segment, RegionsRefusal> {
    let lf::TypeRef::Path(p) = read(v.ty(path), "a bound's type")? else {
        return Err(RegionsRefusal::Unsupported { what: "a bound that is no path", at: type_site(v, path)? });
    };
    let path_ref = read(v.path(p), "a bound's path")?;
    let segs = read(v.segments(path_ref), "a bound's segments")?;
    let Some(&last) = segs.last() else {
        return Err(RegionsRefusal::Unsupported { what: "a bound's path with no segment (a lang item's)", at: type_site(v, path)? });
    };
    read(lf::Segment::decode(last), "a bound's segment")
}

// ---------------------------------------------------------------------------------------------
// The seam to the head lowering's walk (L1a's `RegionSink`).

/// What the head lowering wrote at one region slot of a type it made from text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Slot {
    /// A lifetime written by a name other than `'static` and `'_`, `name` its hash
    /// (`View::name`, blaze-typeck's `Name.0`; L1a's `SlotKind::Named`): what it names is this
    /// file's.
    Named { name: u64, at: Range },
    /// A slot no lifetime is written at, or `'_` (`&T`, `&'_ T`, `Foo` of `struct Foo<'a>`),
    /// at `at`: elision's.
    Elided(Range),
    /// A variable of a binder inside the type (a fn pointer's `for<..>` or its elided input):
    /// `Late { binder, var }`, `binder` counted from 0 within this type.
    Inner(Region),
    /// The same region as this type's slot `index` (before this one): a trait object's default
    /// lifetime bound under a reference or a bounded ADT parameter (`&'a dyn Tr` is
    /// `&'a (dyn Tr + 'a)`).
    SameAs(u32),
    /// `'static` with no name written: a trait object's default bound elsewhere.
    Static,
}

/// The head lowering's slots of the types it made from text (L1a's `RegionSink`, kept per head
/// while the head is lowered).
pub trait Written {
    /// The slots of the type written at `t` in `v`, which the lowering made into `made`: exactly
    /// one per region slot of `made`, in `regions::each_slot_node` order (an alias expanded, a
    /// default filled in, as the type was made).
    fn slots(&self, v: &View<'_>, t: lf::TypeId, made: PTy, out: &mut Vec<Slot, Mem>) -> Result<(), RegionsRefusal>;
}

/// The [`Written`] of one head over L1a's walk: each written type a producer here reads,
/// walked by `Builder::ty_regions` with a sink of its own (types.rs [`RegionSink`]), its one
/// fragment kept in this file's terms. Kept per head and per view (`lf::TypeId`s of two views
/// are not comparable).
///
/// The two numberings meet here. The sink's: a slot's `Late` binder is its binding node's
/// pre-order index among the made type's binder nodes, from 0 (types.rs `SlotKind::Late`),
/// which is what [`Slot::Inner`] takes; an elided lifetime is an id the sink gives a site,
/// which [`Slot::Elided`] takes as the site, a second slot of one id ([`Slot::SameAs`]) the
/// first's region. A slot of no meaning here (`Bound`, a variable of an object not yet placed;
/// `Given` a bound variable) is refused by name, never mapped by a guess.
pub struct Laid {
    /// (the written type, what the walk made of it, its slots in `slots` or why none).
    rows: Vec<(lf::TypeId, Option<PTy>, Result<(u32, u32), RegionRefusal>), Mem>,
    slots: Vec<Slot, Mem>,
    /// Why the head's written types could not all be listed ([`written_types`] refused one):
    /// a type not laid is then refused with this, by name, when a producer reads it.
    unlisted: Option<RegionsRefusal>,
}

impl Laid {
    pub fn new_in(alloc: Mem) -> Laid {
        Laid { rows: Vec::new_in(alloc), slots: Vec::new_in(alloc), unlisted: None }
    }

    /// The written types were listed only in part, for `why`: every type not laid is refused
    /// with it (the first listing's refusal is kept).
    pub fn unlisted(&mut self, why: RegionsRefusal) {
        self.unlisted.get_or_insert(why);
    }

    /// Whether `t` is laid (or refused) already.
    pub fn has(&self, t: lf::TypeId) -> bool {
        self.rows.iter().any(|r| r.0 == t)
    }

    /// `t`, made into `made` by a walk whose sink is `sink` (fresh for this type: its top
    /// fragment is `t`'s).
    pub fn add(&mut self, t: lf::TypeId, made: PTy, sink: &super::types::RegionSink<'_>) -> Result<(), RegionsRefusal> {
        use super::types::SlotKind;
        if let Some(why) = sink.refused() {
            self.rows.push((t, Some(made), Err(why)));
            return Ok(());
        }
        let Some(&f) = sink.frags().last() else {
            self.rows.push((t, Some(made), Err(RegionRefusal::not_laid(NotLaidWhy::Stack, Range::new(0, 0)))));
            return Ok(());
        };
        if sink.frags().len() != 1 {
            self.rows.push((t, Some(made), Err(RegionRefusal::not_laid(NotLaidWhy::Stack, Range::new(0, 0)))));
            return Ok(());
        }
        let start = index(self.slots.len())?;
        let laid = sink.slots_of(f);
        // Each elided id's first slot, within this type.
        let mut first: Vec<(u32, u32), Mem> = Vec::new_in(*self.slots.allocator());
        let mut refusal = None;
        for (i, s) in laid.iter().enumerate() {
            let i = index(i)?;
            let slot = match s.kind {
                SlotKind::Static => Slot::Static,
                SlotKind::Named(n, at) => Slot::Named { name: n.0, at },
                SlotKind::Elided(id) => match first.iter().find(|e| e.0 == id) {
                    Some(&(_, j)) => Slot::SameAs(j),
                    None => {
                        first.push((id, i));
                        match sink.elided_at(id) {
                            Some(at) => Slot::Elided(at),
                            None => {
                                refusal = Some(RegionRefusal::not_laid(NotLaidWhy::ElidedOutput, Range::new(0, 0)));
                                break;
                            }
                        }
                    }
                },
                SlotKind::Late { binder, var } => Slot::Inner(Region::Late { binder, var }),
                SlotKind::Given(r @ (Region::Static | Region::Param(_))) => Slot::Inner(r),
                // A trait object's default bound its own type does not decide: the context of a
                // written type a producer here reads (a field, a bound's argument, a where
                // predicate's type, an impl header) is no reference and no bounded parameter,
                // so it is `'static` (the Reference, "Default trait object lifetimes": "If
                // neither of those rules apply ... `'static`").
                SlotKind::ObjectDefault(_) => Slot::Static,
                SlotKind::Given(_) | SlotKind::Bound(_) => {
                    refusal = Some(RegionRefusal::not_laid(NotLaidWhy::BoundVariable, Range::new(0, 0)));
                    break;
                }
            };
            self.slots.push(slot);
        }
        match refusal {
            Some(why) => {
                self.slots.truncate(start as usize);
                self.rows.push((t, Some(made), Err(why)));
            }
            None => self.rows.push((t, Some(made), Ok((start, index(self.slots.len())? - start)))),
        }
        Ok(())
    }

    /// `t` could not be walked again (the walk's own error, which the head's first walk of the
    /// same type did not meet): refused when read.
    pub fn refuse(&mut self, t: lf::TypeId, why: RegionRefusal) {
        self.rows.push((t, None, Err(why)));
    }
}

impl Written for Laid {
    fn slots(&self, _v: &View<'_>, t: lf::TypeId, made: PTy, out: &mut Vec<Slot, Mem>) -> Result<(), RegionsRefusal> {
        let not_laid = RegionsRefusal::Walk(RegionRefusal::not_laid(NotLaidWhy::Unlisted, Range::new(0, 0)));
        // A type not listed is refused with why the listing stopped, when it did.
        let (_, was, got) = *self.rows.iter().find(|r| r.0 == t).ok_or(self.unlisted.unwrap_or(not_laid))?;
        let (start, len) = got.map_err(RegionsRefusal::Walk)?;
        // The producer's type and the walk's must be one: else the slots are another type's.
        if was != Some(made) {
            return Err(RegionsRefusal::Walk(RegionRefusal::not_laid(NotLaidWhy::OutOfStep, Range::new(0, 0))));
        }
        let s = self.slots.get(start as usize..(start + len) as usize).ok_or(not_laid)?;
        out.extend_from_slice(s);
        Ok(())
    }
}

/// The written types `preds`' clauses were made from, which [`fill_clauses`] reads through
/// [`Written`]: each trait bound's `Self` when written and its last segment's type arguments
/// (`Fn(..)` sugar's inputs too), each binding's term, each `T: 'a`'s `T` when written. Into
/// `out`, each once.
pub fn written_types(v: &View<'_>, preds: &Preds, out: &mut Vec<lf::TypeId, Mem>) -> Result<(), RegionsRefusal> {
    let add = |t: lf::TypeId, out: &mut Vec<lf::TypeId, Mem>| {
        if !out.contains(&t) {
            out.push(t);
        }
    };
    for &src in preds.sources.iter() {
        match src {
            Src::Trait { on, bound, .. } => {
                if let On::Written(t) = on {
                    add(t, out);
                }
                let seg = last_segment(v, bound)?;
                if seg.has_args || seg.parenthesized {
                    for &n in read(v.rows(seg.args), "a bound's arguments")? {
                        if let lf::GenericArg::Type(t) = read(lf::GenericArg::decode(n), "a generic argument")? {
                            add(t, out);
                        }
                    }
                }
            }
            Src::Binding { term: Some(t), .. } => add(t, out),
            Src::TypeOutlives { on: On::Written(t), .. } => add(t, out),
            Src::Binding { term: None, .. } | Src::TypeOutlives { .. } | Src::RegionOutlives { .. } | Src::Derived | Src::Inherited { .. } => {}
        }
    }
    Ok(())
}

/// The written types of an impl header that [`impl_header_regions`] reads: the self type, then the
/// trait reference's type arguments.
pub fn header_types(v: &View<'_>, self_written: lf::TypeId, trait_path: Option<lf::TypeId>, out: &mut Vec<lf::TypeId, Mem>) -> Result<(), RegionsRefusal> {
    out.push(self_written);
    if let Some(p) = trait_path {
        let seg = last_segment(v, p)?;
        for &n in read(v.rows(seg.args), "a trait reference's arguments")? {
            if let lf::GenericArg::Type(t) = read(lf::GenericArg::decode(n), "a generic argument")?
                && !out.contains(&t)
            {
                out.push(t);
            }
        }
    }
    Ok(())
}

/// What a producer here reads.
pub struct Cx<'c, 'n> {
    pub v: &'c View<'n>,
    pub facts: &'c dyn Arity,
    pub written: &'c dyn Written,
    /// The item's lifetimes by name.
    pub scope: &'c Scope,
    /// The whole columns of types the lowering made with no text of their own whose regions
    /// the head knows: its own opaque type (its captures), an associated type in its own bounds
    /// (its own lifetimes, `Param(0..)`).
    pub made: &'c [(PTy, &'c [Region])],
    pub alloc: Mem,
}

/// How a slot with no lifetime written is filled.
#[derive(Clone, Copy, Debug)]
enum Elide {
    /// A field: E0106 (`&T` and `&'_ T` alike).
    Field,
    /// A bound's argument, a where predicate's type: E0637.
    Clause,
    /// An input of `Fn(..)` sugar: a fresh variable of the clause's binder.
    FreshLate,
    /// An impl header: a fresh anonymous parameter of the impl.
    FreshParam,
    /// `Fn(..)` sugar's output: the one input lifetime, E0106 when there is not exactly one.
    Output(Option<Region>),
}

/// The state of one column being laid.
struct St {
    binder: ClauseBinder,
    /// The next anonymous `Param` an impl header's elision makes.
    anon: u32,
    /// The binder index the next written type's inner binder 0 takes.
    next_binder: u32,
}

impl St {
    fn plain(alloc: Mem) -> St {
        St { binder: ClauseBinder::none(alloc), anon: 0, next_binder: 0 }
    }

    fn clause(binder: ClauseBinder) -> St {
        St { binder, anon: 0, next_binder: 1 }
    }
}

fn elided(st: &mut St, mode: Elide, at: Range) -> Result<Region, RegionsRefusal> {
    match mode {
        Elide::Field => Err(RegionsRefusal::Missing { at }),
        Elide::Clause => Err(RegionsRefusal::NotHere { at }),
        Elide::FreshLate => st.binder.fresh(),
        Elide::FreshParam => {
            let r = Region::Param(st.anon);
            st.anon = st.anon.checked_add(1).ok_or(RegionError::LifetimePast { index: u32::MAX, count: u32::MAX })?;
            Ok(r)
        }
        Elide::Output(Some(r)) => Ok(r),
        Elide::Output(None) => Err(RegionsRefusal::Missing { at }),
    }
}

/// The slots of the written type `t`, made into `made`, appended to `out`.
fn written(cx: &Cx<'_, '_>, tys: PTys<'_>, st: &mut St, mode: Elide, t: lf::TypeId, made: PTy, out: &mut Vec<Region, Mem>) -> Result<(), RegionsRefusal> {
    // The slots and the binder nodes (fn pointers, trait objects) of `made`, by the one walk:
    // the column's binders are numbered in its pre-order (regions.rs `Region::Late`), so the
    // types after this one start past ALL of its binder nodes, binding or not.
    let mut binders = 0u32;
    let n = each_slot_node(tys, made, cx.facts, cx.alloc, &mut |_, node, _| {
        binders += u32::from(is_binder(node));
        Ok(())
    })?;
    if n == 0 {
        st.next_binder = st.next_binder.checked_add(binders).ok_or(RegionError::Overflow { ty: made })?;
        return Ok(());
    }
    let mut raw: Vec<Slot, Mem> = Vec::new_in(cx.alloc);
    cx.written.slots(cx.v, t, made, &mut raw)?;
    let got = index(raw.len())?;
    if got != n {
        return Err(RegionError::Length { ty: made, want: n, got }.into());
    }
    let base = out.len();
    let mut deepest: Option<u32> = None;
    for (i, &s) in raw.iter().enumerate() {
        let r = match s {
            Slot::Static => Region::Static,
            Slot::Named { name, at } => st.binder.find(name).or_else(|| cx.scope.find(name)).ok_or(RegionsRefusal::Undeclared { at })?,
            // In a return opaque's bounds a bound's elided argument (`-> impl Iterator<Item =
            // &u8>`) is the output rule's ([`Scope::with_elided`]); `Fn(..)` sugar's inputs and
            // output keep their own rules.
            Slot::Elided(at) => match (mode, cx.scope.elided) {
                (Elide::Clause, Some(e)) => e.map_err(RegionsRefusal::Walk)?,
                _ => elided(st, mode, at)?,
            },
            Slot::Inner(Region::Late { binder, var }) => {
                deepest = Some(deepest.map_or(binder, |d| d.max(binder)));
                let binder = binder.checked_add(st.next_binder).ok_or(RegionError::Overflow { ty: made })?;
                Region::Late { binder, var }
            }
            Slot::Inner(r) => r,
            Slot::SameAs(j) if (j as usize) < i => out[base + j as usize],
            Slot::SameAs(_) => return Err(RegionError::Length { ty: made, want: n, got }.into()),
        };
        out.push(r);
    }
    // Where the walk's numbering (the sink's, within `made`) and the column's meet: a slot
    // naming a binder past `made`'s own binder nodes is the producer's bug, refused.
    if let Some(d) = deepest
        && d >= binders
    {
        return Err(RegionsRefusal::InnerBinder { binder: d, binders });
    }
    st.next_binder = st.next_binder.checked_add(binders).ok_or(RegionError::Overflow { ty: made })?;
    Ok(())
}

/// The slots of a type made with no text: none, or the head's own ([`Cx::made`]); any other is
/// refused (`nth` the clause it is in).
fn made(cx: &Cx<'_, '_>, tys: PTys<'_>, ty: PTy, nth: u32, out: &mut Vec<Region, Mem>) -> Result<(), RegionsRefusal> {
    let n = slot_count(tys, ty, cx.facts, cx.alloc)?;
    if n == 0 {
        return Ok(());
    }
    match cx.made.iter().find(|(p, col)| *p == ty && col.len() == n as usize) {
        Some((_, col)) => {
            out.extend_from_slice(col);
            Ok(())
        }
        None => Err(RegionsRefusal::Derived { nth }),
    }
}

fn intern(tys: &mut PortableTys<'_>, n: PNode) -> Result<PTy, RegionsRefusal> {
    tys.intern(n).map_err(|_| RegionsRefusal::Table)
}

fn lay(cx: &Cx<'_, '_>, tys: PTys<'_>, ty: PTy, slots: &[Region]) -> Result<RegionCol<Mem>, RegionsRefusal> {
    Ok(RegionCol::lay(tys, ty, cx.facts, slots, cx.alloc)?)
}

fn clause_name(cx: &Cx<'_, '_>, st: &St, n: lf::NameIdx) -> Result<Region, RegionsRefusal> {
    match named(cx.v, cx.scope, &st.binder, n)? {
        Named::Is(r) => Ok(r),
        Named::Underscore(at) => Err(RegionsRefusal::NotHere { at }),
    }
}

// ---------------------------------------------------------------------------------------------
// Clauses.

/// A trait clause's column as laid, kept for the clauses made from its bound (its bindings,
/// its nested bounds).
struct Parts {
    trait_: ItemKey,
    args: List,
    slots: Vec<Region, Mem>,
    /// The trait's own slots are `slots[..own]`, its arguments' `slots[own..]`.
    own: usize,
    binder: ClauseBinder,
    next_binder: u32,
    /// `Fn(..)` sugar: the inputs' slots, `slots[start..end]`.
    inputs: Option<(usize, usize)>,
}

/// The column of `on`, the clause's `Self` (made into `self_ty`), appended to `out`.
#[expect(clippy::too_many_arguments, reason = "the clause, its Self, and the column it goes in")]
fn self_col(cx: &Cx<'_, '_>, tys: PTys<'_>, st: &mut St, nth: u32, on: On, self_ty: PTy, parts: &[Option<Parts>], out: &mut Vec<Region, Mem>) -> Result<(), RegionsRefusal> {
    match on {
        On::Written(t) => written(cx, tys, st, Elide::Clause, t, self_ty, out),
        On::Made => made(cx, tys, self_ty, nth, out),
        On::Nested { of, gat } => {
            let parent = parts.get(of as usize).and_then(Option::as_ref).ok_or(RegionsRefusal::Unaligned { nth })?;
            let PNode::Alias { trait_: owner, args: oargs, .. } = table(tys.node(self_ty))? else {
                return Err(RegionsRefusal::Unaligned { nth });
            };
            let same = owner == parent.trait_ && table(tys.list(oargs))? == table(tys.list(parent.args))?;
            if same {
                out.extend_from_slice(&parent.slots[..parent.own]);
            }
            for &n in read(cx.v.rows(gat), "an associated type bound's arguments")? {
                if let lf::GenericArg::Lifetime(l) = read(lf::GenericArg::decode(n), "a generic argument")? {
                    out.push(clause_name(cx, st, l)?);
                }
            }
            if same {
                out.extend_from_slice(&parent.slots[parent.own..]);
            } else {
                for &a in table(tys.list(oargs))? {
                    made(cx, tys, a, nth, out)?;
                }
            }
            Ok(())
        }
    }
}

/// The trait clause `trait_<args>` written as the bound `bound` on `on`.
#[expect(clippy::too_many_arguments, reason = "the clause and what it was written as")]
fn trait_clause(
    cx: &Cx<'_, '_>,
    tys: &mut PortableTys<'_>,
    nth: u32,
    trait_: ItemKey,
    args: List,
    on: On,
    bound: lf::TypeId,
    binders: Binders,
    parts: &[Option<Parts>],
) -> Result<(RegionCol<Mem>, Parts), RegionsRefusal> {
    let v = cx.v;
    let node = PNode::ExTrait(trait_, args);
    let node_ty = intern(tys, node)?;
    let view = tys.view();
    let list = table(view.list(args))?;
    let own_n = region_arity(node_ty, node, cx.facts).map_err(RegionError::NotKnown)?;
    let seg = last_segment(v, bound)?;
    let at = type_site(v, bound)?;
    let mut st = St::clause(ClauseBinder::of(v, binders, cx.alloc)?);
    let mut out: Vec<Region, Mem> = Vec::new_in(cx.alloc);
    // The trait's own lifetime arguments (`Tr<'a, ..>`).
    let mut lts = 0u32;
    if !seg.parenthesized {
        for &n in read(v.rows(seg.args), "a bound's arguments")? {
            if let lf::GenericArg::Lifetime(l) = read(lf::GenericArg::decode(n), "a generic argument")? {
                out.push(clause_name(cx, &st, l)?);
                lts += 1;
            }
        }
    }
    if lts != own_n {
        return Err(if lts == 0 { RegionsRefusal::Missing { at } } else { RegionsRefusal::LifetimeArgs { at, want: own_n, got: lts } });
    }
    let own = out.len();
    let Some(&self_ty) = list.first() else { return Err(RegionsRefusal::Unaligned { nth }) };
    self_col(cx, view, &mut st, nth, on, self_ty, parts, &mut out)?;
    let self_end = out.len();
    let mut k = 0usize;
    let mut inputs = None;
    if seg.parenthesized {
        // `Fn(A, B) -> C`: the one argument is the tuple of the inputs (preds.rs `segment_args`).
        let &tuple = list.get(1).ok_or(RegionsRefusal::Unaligned { nth })?;
        let PNode::Tuple(l) = table(view.node(tuple))? else { return Err(RegionsRefusal::Unaligned { nth }) };
        let kids = table(view.list(l))?;
        let start = out.len();
        let mut j = 0usize;
        for &n in read(v.rows(seg.args), "a bound's inputs")? {
            let lf::GenericArg::Type(t) = read(lf::GenericArg::decode(n), "a generic argument")? else {
                return Err(RegionsRefusal::Unaligned { nth });
            };
            let &made_ty = kids.get(j).ok_or(RegionsRefusal::Unaligned { nth })?;
            written(cx, view, &mut st, Elide::FreshLate, t, made_ty, &mut out)?;
            j += 1;
        }
        if j != kids.len() {
            return Err(RegionsRefusal::Unaligned { nth });
        }
        inputs = Some((start, out.len()));
        k = 1;
    } else if seg.has_args {
        for &n in read(v.rows(seg.args), "a bound's arguments")? {
            match read(lf::GenericArg::decode(n), "a generic argument")? {
                lf::GenericArg::Type(t) => {
                    let &made_ty = list.get(1 + k).ok_or(RegionsRefusal::Unaligned { nth })?;
                    written(cx, view, &mut st, Elide::Clause, t, made_ty, &mut out)?;
                    k += 1;
                }
                lf::GenericArg::Const(_) => {
                    let &made_ty = list.get(1 + k).ok_or(RegionsRefusal::Unaligned { nth })?;
                    if slot_count(view, made_ty, cx.facts, cx.alloc)? != 0 {
                        return Err(RegionsRefusal::Unaligned { nth });
                    }
                    k += 1;
                }
                _ => {}
            }
        }
    }
    // The parameters not written take their defaults (types.rs `with_defaults`): `Self`'s
    // column for a default that is `Self` (`trait Add<Rhs = Self>`), none for one with no
    // regions; any other is refused by name.
    for &d in list.get(1 + k..).ok_or(RegionsRefusal::Unaligned { nth })? {
        if d == self_ty {
            let copy: Vec<Region, Mem> = Vec::from_slice_in(&out[own..self_end], cx.alloc);
            out.extend_from_slice(&copy);
        } else {
            made(cx, view, d, nth, &mut out)?;
        }
    }
    let col = lay(cx, view, node_ty, &out)?;
    let next_binder = st.next_binder;
    Ok((col, Parts { trait_, args, slots: out, own, binder: st.binder, next_binder, inputs }))
}

/// The projection `<args as owner>::name == term` of a binding of the bound whose trait clause
/// is `parent`.
#[expect(clippy::too_many_arguments, reason = "the clause and what it was written as")]
fn binding(
    cx: &Cx<'_, '_>,
    tys: &mut PortableTys<'_>,
    nth: u32,
    (owner, oargs, name, term): (ItemKey, List, blaze_typeck::Name, PTy),
    parent: &Parts,
    written_term: Option<lf::TypeId>,
    gat: lf::List,
) -> Result<(RegionCol<Mem>, u32), RegionsRefusal> {
    let node_ty = intern(tys, PNode::ExProj { trait_: owner, name, args: oargs, term })?;
    let view = tys.view();
    let mut st = St { binder: parent.binder.clone_in(cx.alloc), anon: 0, next_binder: parent.next_binder };
    let mut out: Vec<Region, Mem> = Vec::new_in(cx.alloc);
    // The owner's lifetimes are the bound's when the owner is the bound's trait at the same
    // arguments; else (`Fn(..)`'s `Output` is `FnOnce`'s) the owner's arguments must carry none
    // of their own beyond what `made` knows.
    let same = owner == parent.trait_ && table(view.list(oargs))? == table(view.list(parent.args))?;
    if same {
        out.extend_from_slice(&parent.slots[..parent.own]);
    }
    for &n in read(cx.v.rows(gat), "a binding's arguments")? {
        if let lf::GenericArg::Lifetime(l) = read(lf::GenericArg::decode(n), "a generic argument")? {
            out.push(clause_name(cx, &st, l)?);
        }
    }
    if same {
        out.extend_from_slice(&parent.slots[parent.own..]);
    } else {
        let parent_args = table(view.list(parent.args))?;
        let oargs_list = table(view.list(oargs))?;
        // `Fn(A) -> B`'s `FnOnce<A>`: the owner's arguments are the bound's (a supertrait at the
        // same arguments): each one's slots are the bound's argument's.
        if oargs_list.len() == parent_args.len() && oargs_list == parent_args {
            out.extend_from_slice(&parent.slots[parent.own..]);
        } else {
            for &a in oargs_list {
                made(cx, view, a, nth, &mut out)?;
            }
        }
    }
    match written_term {
        Some(t) => {
            let mode = match parent.inputs {
                // The Reference, "Lifetime elision", "Function elision rules", as for a fn
                // pointer: exactly one lifetime position among the inputs is the output's.
                Some((s, e)) => Elide::Output(if e - s == 1 { parent.slots.get(s).copied() } else { None }),
                None => Elide::Clause,
            };
            written(cx, view, &mut st, mode, t, term, &mut out)?;
        }
        None => made(cx, view, term, nth, &mut out)?,
    }
    let col = lay(cx, view, node_ty, &out)?;
    Ok((col, st.binder.vars))
}

// A clause with no text of its own ([`Src::Derived`]: an implied sizedness, a supertrait copy,
// a GAT binding, a trait alias's expansion) has NO column, ever. Its regions are not the head's
// to lay (they are the clause it was derived from, or none), and counting its node's slots
// would need the lifetime count of a trait this head may never have made (the implied
// `Sized`'s): a column laid only when that count happens to be filed would make the head's
// bytes depend on what was made before it (rule 25). A consumer finds columns by `nth` and has
// none for it.

/// The column of every clause of `preds` into `head.clauses` (one per written trait, projection
/// and type-outlives clause, by `nth`; a `where const`, a `'a: 'b` and a derived clause have
/// none, nor a copy of a derived clause a member inherits), and every outlives
/// clause into `head.outlives` (`'a: 'b` into `head.params` too). `container` is the regions of
/// the container a member inherits clauses from. `head.params` must already have the item's
/// count.
pub fn fill_clauses(cx: &Cx<'_, '_>, tys: &mut PortableTys<'_>, preds: &Preds, container: Option<&HeadRegions<Mem>>, head: &mut HeadRegions<Mem>) -> Result<(), RegionsRefusal> {
    if preds.sources.len() != preds.clauses.len() {
        return Err(RegionsRefusal::Sources { clauses: index(preds.clauses.len())?, sources: index(preds.sources.len())? });
    }
    // Sources the clauses' maker could not read as written (preds.rs `Preds::regions_gap`).
    if let Some(why) = preds.regions_gap {
        return Err(why);
    }
    let mut parts: Vec<Option<Parts>, Mem> = Vec::new_in(cx.alloc);
    for (i, (&c, &src)) in preds.clauses.iter().zip(preds.sources.iter()).enumerate() {
        let nth = index(i)?;
        match c {
            PClause::TypeOutlives { region, .. } => head.outlives.push(Outlives::Type { clause: nth, region }),
            PClause::RegionOutlives { long, short } => {
                head.outlives.push(Outlives::Region { long, short });
                head.params.add_outlives(long, short)?;
            }
            PClause::Trait { .. } | PClause::Projection { .. } | PClause::Const { .. } | PClause::ForFields { .. } => {}
        }
        let mut part = None;
        let laid = match (c, src) {
            (PClause::Const { .. } | PClause::RegionOutlives { .. } | PClause::ForFields { .. }, _) => None,
            (_, Src::Inherited { nth: k }) => {
                // The container's columns were filled by this same function (or
                // `trait_inherited`): a trait, projection or type-outlives clause there with no
                // column is a derived one ([`Src::Derived`]), and its copy here has none either. No
                // container at all is refused.
                let h = container.ok_or(RegionsRefusal::Inherited { nth: k })?;
                match h.clauses.iter().find(|r| r.nth == k) {
                    Some(from) => Some((lay(cx, tys.view(), from.col.ty(), from.col.as_slice())?, from.binders)),
                    None => None,
                }
            }
            // No column (see above `fill_clauses`).
            (_, Src::Derived) => None,
            (PClause::Trait { trait_, args }, Src::Trait { on, bound, binders }) => {
                let (col, p) = trait_clause(cx, tys, nth, trait_, args, on, bound, binders, &parts)?;
                let vars = p.binder.vars;
                part = Some(p);
                Some((col, vars))
            }
            (PClause::Projection { trait_, args, name, term }, Src::Binding { of, term: written_term, gat }) => {
                let parent = parts.get(of as usize).and_then(Option::as_ref).ok_or(RegionsRefusal::Unaligned { nth })?;
                Some(binding(cx, tys, nth, (trait_, args, name, term), parent, written_term, gat)?)
            }
            (PClause::TypeOutlives { ty, .. }, Src::TypeOutlives { on, binders, .. }) => {
                let mut st = St::clause(ClauseBinder::of(cx.v, binders, cx.alloc)?);
                let mut out: Vec<Region, Mem> = Vec::new_in(cx.alloc);
                let view = tys.view();
                self_col(cx, view, &mut st, nth, on, ty, &parts, &mut out)?;
                Some((lay(cx, view, ty, &out)?, st.binder.vars))
            }
            _ => return Err(RegionsRefusal::Unaligned { nth }),
        };
        if let Some((col, binders)) = laid {
            head.clauses.push(ClauseRegions { nth, binders, col });
        }
        parts.push(part);
    }
    Ok(())
}

/// A head with only clauses (a trait's, an associated type's or const's, a type alias's): `count`
/// lifetime parameters, all early-bound, its clauses' columns and outlives.
pub fn clause_regions(cx: &Cx<'_, '_>, tys: &mut PortableTys<'_>, preds: &Preds, count: u32, container: Option<&HeadRegions<Mem>>) -> Result<HeadRegions<Mem>, RegionsRefusal> {
    let mut head = HeadRegions::new_in(LifetimeParams::new_in(count, cx.alloc), cx.alloc);
    fill_clauses(cx, tys, preds, container, &mut head)?;
    Ok(head)
}

/// The columns a trait's members inherit, in the order heads.rs `Store::trait_members` hands
/// its members the trait's clauses: `self_clause` (`Self: Trait<params>`, over `ExTrait` its
/// own lifetimes `Param(0..count)` and its arguments', parameters with none) first, then each
/// of `trait_head`'s clause columns one place later, then `after` (an implied sizedness of
/// `Self`, made with no text: [`Src::Derived`], so no column, only its place). Its count is the
/// trait's.
pub fn trait_inherited(cx: &Cx<'_, '_>, tys: &mut PortableTys<'_>, self_clause: PClause, trait_head: &HeadRegions<Mem>, own_clauses: u32, after: &[PClause]) -> Result<HeadRegions<Mem>, RegionsRefusal> {
    let count = trait_head.params.count();
    let mut head = HeadRegions::new_in(LifetimeParams::new_in(count, cx.alloc), cx.alloc);
    let PClause::Trait { trait_, args } = self_clause else { return Err(RegionsRefusal::Unaligned { nth: 0 }) };
    let node_ty = intern(tys, PNode::ExTrait(trait_, args))?;
    let mut own: Vec<Region, Mem> = Vec::with_capacity_in(count as usize, cx.alloc);
    for i in 0..count {
        own.push(Region::Param(i));
    }
    head.clauses.push(ClauseRegions { nth: 0, binders: 0, col: lay(cx, tys.view(), node_ty, &own)? });
    for c in trait_head.clauses.iter() {
        if c.nth >= own_clauses {
            return Err(RegionsRefusal::Unaligned { nth: c.nth });
        }
        head.clauses.push(ClauseRegions { nth: c.nth + 1, binders: c.binders, col: lay(cx, tys.view(), c.col.ty(), c.col.as_slice())? });
    }
    // `after` sits after every clause of the trait, columned or not (`'a: 'b` and `where const`
    // have none): the caller's `inherited[1 + own_clauses + j]`. Each is derived, so it has no
    // column (above `fill_clauses`); only its numbering is checked.
    index(after.len())?.checked_add(1).and_then(|n| n.checked_add(own_clauses)).ok_or(RegionsRefusal::Unaligned { nth: u32::MAX })?;
    Ok(head)
}

// ---------------------------------------------------------------------------------------------
// ADTs.

/// One field of an ADT as the head lowered it: where it is, its written type and what it made.
#[derive(Clone, Copy, Debug)]
pub struct FieldAt {
    pub variant: u32,
    pub field: u32,
    pub written: lf::TypeId,
    pub ty: PTy,
}

/// An ADT's regions: its lifetime parameters (its own, all early-bound: [`Cx::scope`]), its
/// clauses, and one column per field (`fields` in strict (variant, field) order). A field's
/// elided lifetime is E0106 (`struct S { x: &u8 }`); an object default is the sink's.
pub fn adt_regions(cx: &Cx<'_, '_>, tys: &mut PortableTys<'_>, preds: &Preds, fields: &[FieldAt]) -> Result<HeadRegions<Mem>, RegionsRefusal> {
    let mut head = clause_regions(cx, tys, preds, cx.scope.params(), None)?;
    let view = tys.view();
    let mut last: Option<(u32, u32)> = None;
    for f in fields {
        if last.is_some_and(|l| l >= (f.variant, f.field)) {
            return Err(RegionsRefusal::FieldOrder { variant: f.variant, field: f.field });
        }
        last = Some((f.variant, f.field));
        let mut st = St::plain(cx.alloc);
        let mut out: Vec<Region, Mem> = Vec::new_in(cx.alloc);
        written(cx, view, &mut st, Elide::Field, f.written, f.ty, &mut out)?;
        head.fields.push((f.variant, f.field, lay(cx, view, f.ty, &out)?));
    }
    Ok(head)
}

// ---------------------------------------------------------------------------------------------
// Impls.

/// An impl's trait reference as the head lowered it: the trait's path type (head::TRAIT), the
/// trait's checker key and the clause arguments, `Self` first (heads.rs `Builder::trait_ref`).
#[derive(Clone, Copy, Debug)]
pub struct TraitRef {
    pub path: lf::TypeId,
    pub trait_: ItemKey,
    pub args: List,
}

/// An impl's header as the head lowered it.
#[derive(Clone, Copy, Debug)]
pub struct ImplAt {
    pub self_written: lf::TypeId,
    pub self_ty: PTy,
    pub trait_ref: Option<TraitRef>,
}

/// An impl's regions. Its lifetime parameters are its own (`Cx::scope`, declaration order),
/// then one per lifetime elided in its header, in slot order (the self type's first, then the
/// trait reference's): the Reference, "Lifetime elision", "Impl header lifetime elision"
/// (`impl Tr for &S` is `impl<'x> Tr for &'x S`; `'_` the same). Its members number the
/// container's lifetimes as `0..head.params.count()`. Then the self type's column and the
/// trait reference's (over `ExTrait(trait_, args)`); no clause. The header's columns come
/// FIRST and alone: the trait column is what a `Self::Name` of a trait with lifetimes reads
/// (types.rs `projection_slots`, `Through::SelfImpl`), in the impl's own where clauses and its
/// `type X = ..;` values as in its members, so those are walked after it, given it, and
/// filed into this head by the caller ([`fill_clauses`], [`file_impl_assoc`]; heads.rs
/// `impl_`). `cx.written` is the header types' walk ([`header_types`]).
pub fn impl_header_regions(cx: &Cx<'_, '_>, tys: &mut PortableTys<'_>, at: ImplAt) -> Result<HeadRegions<Mem>, RegionsRefusal> {
    let mut st = St { binder: ClauseBinder::none(cx.alloc), anon: cx.scope.params(), next_binder: 0 };
    let mut self_slots: Vec<Region, Mem> = Vec::new_in(cx.alloc);
    written(cx, tys.view(), &mut st, Elide::FreshParam, at.self_written, at.self_ty, &mut self_slots)?;
    let trait_slots = match at.trait_ref {
        Some(tr) => Some(header_trait(cx, tys, &mut st, tr, at.self_ty, &self_slots)?),
        None => None,
    };
    let mut head = HeadRegions::new_in(LifetimeParams::new_in(st.anon, cx.alloc), cx.alloc);
    let view = tys.view();
    let self_col = lay(cx, view, at.self_ty, &self_slots)?;
    let trait_col = match trait_slots {
        Some((node_ty, slots)) => Some(lay(cx, view, node_ty, &slots)?),
        None => None,
    };
    head.impl_header = Some((self_col, trait_col));
    Ok(head)
}

/// The trait reference's node and slots: the trait's own lifetime arguments (each elided one
/// fresh), `Self`'s (the self type's), then each written argument's, then the defaults'.
fn header_trait(cx: &Cx<'_, '_>, tys: &mut PortableTys<'_>, st: &mut St, tr: TraitRef, self_ty: PTy, self_slots: &[Region]) -> Result<(PTy, Vec<Region, Mem>), RegionsRefusal> {
    let v = cx.v;
    let node = PNode::ExTrait(tr.trait_, tr.args);
    let node_ty = intern(tys, node)?;
    let view = tys.view();
    let list = table(view.list(tr.args))?;
    let own_n = region_arity(node_ty, node, cx.facts).map_err(RegionError::NotKnown)?;
    let seg = last_segment(v, tr.path)?;
    let at = type_site(v, tr.path)?;
    if seg.parenthesized {
        return Err(RegionsRefusal::Unsupported { what: "`Fn(..)` sugar in an impl header", at });
    }
    let mut out: Vec<Region, Mem> = Vec::new_in(cx.alloc);
    let mut lts = 0u32;
    for &n in read(v.rows(seg.args), "a trait reference's arguments")? {
        if let lf::GenericArg::Lifetime(l) = read(lf::GenericArg::decode(n), "a generic argument")? {
            let r = match named(v, cx.scope, &st.binder, l)? {
                Named::Is(r) => r,
                Named::Underscore(at) => elided(st, Elide::FreshParam, at)?,
            };
            out.push(r);
            lts += 1;
        }
    }
    if lts == 0 {
        // `impl Tr for S` where `trait Tr<'a>`: each elided header lifetime is a fresh one.
        for _ in 0..own_n {
            let r = elided(st, Elide::FreshParam, at)?;
            out.push(r);
        }
    } else if lts != own_n {
        return Err(RegionsRefusal::LifetimeArgs { at, want: own_n, got: lts });
    }
    if list.first() != Some(&self_ty) {
        return Err(RegionsRefusal::Unaligned { nth: u32::MAX });
    }
    out.extend_from_slice(self_slots);
    let mut k = 0usize;
    if seg.has_args {
        for &n in read(v.rows(seg.args), "a trait reference's arguments")? {
            match read(lf::GenericArg::decode(n), "a generic argument")? {
                lf::GenericArg::Type(t) => {
                    let &made_ty = list.get(1 + k).ok_or(RegionsRefusal::Unaligned { nth: u32::MAX })?;
                    written(cx, view, st, Elide::FreshParam, t, made_ty, &mut out)?;
                    k += 1;
                }
                lf::GenericArg::Const(_) => {
                    let &made_ty = list.get(1 + k).ok_or(RegionsRefusal::Unaligned { nth: u32::MAX })?;
                    if slot_count(view, made_ty, cx.facts, cx.alloc)? != 0 {
                        return Err(RegionsRefusal::Unaligned { nth: u32::MAX });
                    }
                    k += 1;
                }
                _ => {}
            }
        }
    }
    for &d in list.get(1 + k..).ok_or(RegionsRefusal::Unaligned { nth: u32::MAX })? {
        if d == self_ty {
            out.extend_from_slice(self_slots);
        } else if slot_count(view, d, cx.facts, cx.alloc)? != 0 {
            return Err(RegionsRefusal::HeaderDefault { at });
        }
    }
    Ok((node_ty, out))
}

/// One `type Name = U;` member of an impl as the head lowered it: its name, `U` as written (in
/// the MEMBER's view: [`Cx::v`] of the call is that view) and what the lowering made of it.
#[derive(Clone, Copy, Debug)]
pub struct AssocAt {
    pub name: blaze_typeck::Name,
    pub written: lf::TypeId,
    pub ty: PTy,
}

/// An impl's `type Name = U;` value's column (`HeadRegions::impl_assoc`, read by
/// `RegionFacts::impl_assoc`), in the impl's numbering: `cx.scope` is the impl's lifetimes by
/// name (`Param(i)`, declaration order), `cx.v` and `cx.written` the member's view and its walk.
/// No elision rule gives a lifetime there (the Reference, "Lifetime elision": an associated
/// type's value is no fn signature and no impl header), so an elided one is refused as a
/// field's is (E0106; `'_` there is rustc's E0637, the same slot: [`Slot::Elided`]); a name the
/// impl does not declare is E0261. A trait object's default bound there is `'static` (the
/// walk's `ObjectDefault`, [`Laid::add`]).
pub fn impl_assoc(cx: &Cx<'_, '_>, tys: PTys<'_>, at: AssocAt) -> Result<(blaze_typeck::Name, RegionCol<Mem>), RegionsRefusal> {
    let mut st = St::plain(cx.alloc);
    let mut out: Vec<Region, Mem> = Vec::new_in(cx.alloc);
    written(cx, tys, &mut st, Elide::Field, at.written, at.ty, &mut out)?;
    Ok((at.name, lay(cx, tys, at.ty, &out)?))
}

/// `col` into `head`'s associated type columns, kept in strict name order (the head record's
/// one order, rule 25); a name filed twice is E0201.
pub fn file_impl_assoc(head: &mut HeadRegions<Mem>, name: blaze_typeck::Name, col: RegionCol<Mem>) -> Result<(), RegionsRefusal> {
    match head.impl_assoc.binary_search_by(|e| e.0.cmp(&name)) {
        Ok(_) => Err(RegionsRefusal::AssocTwice { name }),
        Err(i) => {
            head.impl_assoc.insert_shift(i, (name, col));
            Ok(())
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Opaque types.

/// Which lifetimes an opaque type captures when no `use<..>` is written.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Capture {
    /// Every lifetime parameter in scope, `Param(0..count)` (the fn's numbering: its
    /// container's, its own, its elided inputs'): edition 2024's rule for return-position
    /// `impl Trait`, and every edition's for an async fn's future and for `impl Trait` in a
    /// trait's or a trait impl's fn (RFC 3498, "Lifetime capture rules 2024"; RFC 3425).
    All,
    /// Before edition 2024, elsewhere: only the lifetimes the bounds name (RFC 3498, "Rust
    /// 2021 and earlier"), `'_` and an elided `&` there being the fn's elided output lifetime.
    Named,
}

impl Capture {
    /// The rule for an opaque type of a fn written in `edition`.
    pub fn rule(edition: Edition, in_trait_or_trait_impl: bool, async_future: bool) -> Capture {
        if async_future || in_trait_or_trait_impl || matches!(edition, Edition::E2024) { Capture::All } else { Capture::Named }
    }
}

/// The lifetimes the opaque type written at `bounds` (an `impl ..` type; `None` for an async
/// fn's future, which has no text of its own) captures, in ascending `Param` order, into `out`:
/// a written `use<..>`'s lifetimes exactly; else by `rule`. `count` is the fn's lifetime count,
/// `output_elided` what an elided output lifetime is (L1a's elision: `None` where none
/// applies). Its length is the opaque's `opaque_captures`, its slots the `Opaque` node's own in
/// the fn's signature column. It reads only the text (`v`), the fn's names (`scope`) and the
/// allocator, so a fn's head asks it before the signature's column is laid (heads.rs
/// `fn_head`, the opaques' captures) as well as after (`opaque_filed`).
#[expect(clippy::too_many_arguments, reason = "the opaque's text and names, its rule, and the out list")]
pub fn captures(v: &View<'_>, scope: &Scope, alloc: Mem, bounds: Option<lf::TypeId>, rule: Capture, count: u32, output_elided: Option<Region>, out: &mut Vec<Region, Mem>) -> Result<(), RegionsRefusal> {
    let start = out.len();
    let list = match bounds {
        Some(t) => match read(v.ty(t), "an opaque type")? {
            lf::TypeRef::Impl(l) => Some(l),
            _ => return Err(RegionsRefusal::Unsupported { what: "an opaque type that is no `impl Trait`", at: type_site(v, t)? }),
        },
        None => None,
    };
    let none = ClauseBinder::none(alloc);
    let elided_at = |at: Range| output_elided.ok_or(RegionsRefusal::Missing { at });
    let mut written_use = false;
    if let Some(l) = list {
        for &n in read(v.rows(l), "an opaque type's bounds")? {
            let lf::Bound::Use(ids) = read(lf::Bound::decode(n), "a bound")? else { continue };
            written_use = true;
            for &id in read(v.ids(ids), "a `use<..>` list")? {
                let name = lf::NameIdx(id);
                // A type or const parameter in `use<..>`: captured under every rule.
                if !read(v.name_text(name), "a `use<..>` name")?.starts_with('\'') {
                    continue;
                }
                match named(v, scope, &none, name)? {
                    Named::Is(Region::Static) => {
                        return Err(RegionsRefusal::Unsupported { what: "`'static` in `use<..>`", at: name_site(v, name)? });
                    }
                    Named::Is(r) => out.push(r),
                    Named::Underscore(at) => out.push(elided_at(at)?),
                }
            }
        }
    }
    if !written_use {
        match (rule, list) {
            (Capture::All, _) => {
                for i in 0..count {
                    out.push(Region::Param(i));
                }
            }
            (Capture::Named, Some(l)) => named_in_bounds((v, scope, alloc), l, output_elided, out)?,
            (Capture::Named, None) => {
                return Err(RegionsRefusal::Unsupported { what: "an opaque type with no text under the named-lifetimes capture rule", at: Range::new(0, 0) });
            }
        }
    }
    // One slot per captured lifetime, in `Param` order (the order a `use<..>` lists them in is
    // no input of what is captured).
    out[start..].sort_unstable();
    let mut w = start;
    for r in start..out.len() {
        if w == start || out[w - 1] != out[r] {
            out[w] = out[r];
            w += 1;
        }
    }
    out.truncate(w);
    for &r in &out[start..] {
        if let Region::Param(index) = r
            && index >= count
        {
            return Err(RegionError::LifetimePast { index, count }.into());
        }
    }
    Ok(())
}

/// What the bounds of an `impl ..` name, pre-2024: each lifetime written in them that the
/// scope declares (not one their own `for<..>` binds), and the elided output lifetime for a
/// `'_` or a `&` with none outside a fn pointer.
fn named_in_bounds((v, scope, alloc): (&View<'_>, &Scope, Mem), bounds: lf::List, output_elided: Option<Region>, out: &mut Vec<Region, Mem>) -> Result<(), RegionsRefusal> {
    let none = ClauseBinder::none(alloc);
    // The names `for<..>` binds anywhere in the bounds (a lifetime name may not shadow one in
    // scope: E0496), so those are not captured.
    let mut bound_here: Vec<u64, Mem> = Vec::new_in(alloc);
    // (a bounds list, or a type and whether it is under a fn pointer).
    enum Work {
        Bounds(lf::List),
        Ty(lf::TypeId, bool),
    }
    let mut todo: Vec<Work, Mem> = Vec::new_in(alloc);
    todo.push(Work::Bounds(bounds));
    let mut steps = 0u32;
    let note = |n: lf::NameIdx, bound_here: &[u64], out: &mut Vec<Region, Mem>| -> Result<(), RegionsRefusal> {
        let text = read(v.name_text(n), "a lifetime's text")?;
        if text == "'static" {
            return Ok(());
        }
        if text == "'_" {
            out.push(output_elided.ok_or(RegionsRefusal::Missing { at: name_site(v, n)? })?);
            return Ok(());
        }
        if bound_here.contains(&read(v.name(n), "a lifetime's name")?) {
            return Ok(());
        }
        match named(v, scope, &none, n)? {
            Named::Is(Region::Static) => {}
            Named::Is(r) => out.push(r),
            Named::Underscore(at) => out.push(output_elided.ok_or(RegionsRefusal::Missing { at })?),
        }
        Ok(())
    };
    let binder_names = |list: lf::List, into: &mut Vec<u64, Mem>| -> Result<(), RegionsRefusal> {
        for &n in read(v.rows(list), "a `for<..>` binder")? {
            if let lf::GenericParam::Lifetime { name, .. } = read(lf::GenericParam::decode(n), "a binder's parameter")? {
                into.push(read(v.name(name), "a binder's lifetime name")?);
            }
        }
        Ok(())
    };
    while let Some(w) = todo.pop() {
        steps += 1;
        if steps > MOST_CAPTURE_STEPS {
            return Err(RegionsRefusal::Unsupported { what: "an opaque type's bounds past 4096 nodes", at: Range::new(0, 0) });
        }
        match w {
            Work::Bounds(l) => {
                for &n in read(v.rows(l), "bounds")? {
                    match read(lf::Bound::decode(n), "a bound")? {
                        lf::Bound::Trait { ty, binder, .. } => {
                            binder_names(binder, &mut bound_here)?;
                            todo.push(Work::Ty(ty, false));
                        }
                        lf::Bound::Lifetime(n) => note(n, &bound_here, out)?,
                        lf::Bound::Use(_) => {}
                    }
                }
            }
            Work::Ty(t, in_fn) => match read(v.ty(t), "a type")? {
                lf::TypeRef::Ref { lifetime, inner, .. } => {
                    match lifetime {
                        Some(n) => note(n, &bound_here, out)?,
                        // An elided lifetime of a fn pointer's input or output is the fn
                        // pointer's own (late); elsewhere in return position it is the output's.
                        None if in_fn => {}
                        None => out.push(output_elided.ok_or(RegionsRefusal::Missing { at: type_site(v, t)? })?),
                    }
                    todo.push(Work::Ty(inner, in_fn));
                }
                lf::TypeRef::Ptr { inner, .. } | lf::TypeRef::Slice(inner) => todo.push(Work::Ty(inner, in_fn)),
                lf::TypeRef::Array { elem, .. } => todo.push(Work::Ty(elem, in_fn)),
                lf::TypeRef::Pat { base, .. } => todo.push(Work::Ty(base, in_fn)),
                lf::TypeRef::Record(fields) => {
                    for &n in read(v.rows(fields), "a record's fields")? {
                        todo.push(Work::Ty(read(lf::Field::decode(n), "a record's field")?.ty, in_fn));
                    }
                }
                lf::TypeRef::Tuple(l) | lf::TypeRef::Union(l) => {
                    for &id in read(v.ids(l), "a tuple's elements")? {
                        todo.push(Work::Ty(lf::TypeId(id), in_fn));
                    }
                }
                lf::TypeRef::For { binder, inner } => {
                    binder_names(binder, &mut bound_here)?;
                    todo.push(Work::Ty(inner, in_fn));
                }
                lf::TypeRef::FnPtr { params, ret, .. } => {
                    for &n in read(v.rows(params), "a fn pointer's parameters")? {
                        if let Some(pt) = read(lf::Param::decode(n), "a fn pointer's parameter")?.ty {
                            todo.push(Work::Ty(pt, true));
                        }
                    }
                    if let Some(r) = ret {
                        todo.push(Work::Ty(r, true));
                    }
                }
                lf::TypeRef::Dyn(l) | lf::TypeRef::Impl(l) => todo.push(Work::Bounds(l)),
                lf::TypeRef::Path(p) => {
                    let path = read(v.path(p), "a path")?;
                    for &s in read(v.segments(path), "a path's segments")? {
                        let seg = read(lf::Segment::decode(s), "a segment")?;
                        for &n in read(v.rows(seg.args), "a segment's arguments")? {
                            match read(lf::GenericArg::decode(n), "a generic argument")? {
                                lf::GenericArg::Type(a) => todo.push(Work::Ty(a, in_fn)),
                                lf::GenericArg::Lifetime(l) => note(l, &bound_here, out)?,
                                lf::GenericArg::AssocType { args, ty, .. } => {
                                    for &g in read(v.rows(args), "a binding's arguments")? {
                                        if let lf::GenericArg::Lifetime(l) = read(lf::GenericArg::decode(g), "a generic argument")? {
                                            note(l, &bound_here, out)?;
                                        }
                                    }
                                    todo.push(Work::Ty(ty, in_fn));
                                }
                                lf::GenericArg::AssocBound { bounds, .. } => todo.push(Work::Bounds(bounds)),
                                lf::GenericArg::Const(_) | lf::GenericArg::AssocConst { .. } => {}
                            }
                        }
                        // `Fn(A) -> B`'s inputs are in `args`, its output here, both late like a
                        // fn pointer's; an anchor's type and trait are types of the path.
                        if let Some(o) = seg.ty {
                            todo.push(Work::Ty(o, in_fn || seg.parenthesized));
                        }
                        if let Some(tr) = seg.trait_ {
                            todo.push(Work::Ty(tr, in_fn));
                        }
                    }
                }
                lf::TypeRef::Macro(_) => return Err(RegionsRefusal::Unsupported { what: "a type macro in an opaque type's bounds", at: type_site(v, t)? }),
                lf::TypeRef::Never | lf::TypeRef::Infer | lf::TypeRef::Error => {}
            },
        }
    }
    Ok(())
}

/// An opaque type's regions: the fn's `count` lifetimes (its clause columns are in the fn's
/// numbering), its captured lifetimes' count, and its own clauses (its bounds with it as
/// `Self`: [`Cx::made`] must hold it with `captured`).
pub fn opaque_regions(cx: &Cx<'_, '_>, tys: &mut PortableTys<'_>, preds: &Preds, count: u32, captured: &[Region]) -> Result<HeadRegions<Mem>, RegionsRefusal> {
    let mut head = clause_regions(cx, tys, preds, count, None)?;
    head.opaque_captures = Some(index(captured.len())?);
    Ok(head)
}

#[cfg(test)]
mod tests {
    use super::*;
    use blaze_lower::{Cfg, CfgAnswer, Lowered, lower_signature};
    use blaze_typeck::regions::Count;
    use blaze_typeck::{Mutbl, Name, UintTy};

    struct NoCfg;

    impl Cfg for NoCfg {
        fn atom(&mut self, name: &str, _value: Option<&str>) -> CfgAnswer {
            panic!("a signature in these tests asked the cfg atom `{name}`")
        }
    }

    fn a() -> Mem {
        Alloc::global()
    }

    fn lower(text: &'static str) -> Lowered<'static, 'static> {
        match lower_signature(text, Edition::E2021, &mut NoCfg, a()) {
            Ok(l) => l,
            Err(_) => panic!("`{text}` did not lower"),
        }
    }

    /// The traits' lifetime counts; every ADT and alias has none.
    struct Facts {
        traits: &'static [(ItemKey, u32)],
    }

    impl Arity for Facts {
        fn adt_lifetimes(&self, _: ItemKey) -> Count {
            Count::Known(0)
        }
        fn opaque_captures(&self, _: ItemKey) -> Count {
            Count::NotKnown
        }
        fn alias_lifetimes(&self, _: ItemKey, _: Name) -> Count {
            Count::Known(0)
        }
        fn trait_lifetimes(&self, t: ItemKey) -> Count {
            self.traits.iter().find(|e| e.0 == t).map_or(Count::NotKnown, |e| Count::Known(e.1))
        }
    }

    /// The test's sink: the syntax's own slots for references, paths' lifetime arguments (a
    /// path's lifetimes before its types, as an ADT's own slots come before its children's) and
    /// tuples: enough for the types these tests write.
    struct Syntax;

    /// A written lifetime as L1a's sink gives it: `'static` and `'_` by their meaning, any other
    /// by its name.
    fn named_slot(v: &View<'_>, n: lf::NameIdx) -> Slot {
        let at = name_site(v, n).unwrap();
        match v.name_text(n).unwrap() {
            "'static" => Slot::Static,
            "'_" => Slot::Elided(at),
            _ => Slot::Named { name: v.name(n).unwrap(), at },
        }
    }

    impl Written for Syntax {
        fn slots(&self, v: &View<'_>, t: lf::TypeId, _made: PTy, out: &mut Vec<Slot, Mem>) -> Result<(), RegionsRefusal> {
            let mut todo: Vec<lf::TypeId, Mem> = Vec::new_in(a());
            todo.push(t);
            while let Some(t) = todo.pop() {
                match v.ty(t).unwrap() {
                    lf::TypeRef::Ref { lifetime, inner, .. } => {
                        out.push(match lifetime {
                            Some(n) => named_slot(v, n),
                            None => Slot::Elided(type_site(v, t)?),
                        });
                        todo.push(inner);
                    }
                    lf::TypeRef::Path(p) => {
                        let seg = lf::Segment::decode(*v.segments(v.path(p).unwrap()).unwrap().last().unwrap()).unwrap();
                        let mut kids: Vec<lf::TypeId, Mem> = Vec::new_in(a());
                        for &n in v.rows(seg.args).unwrap() {
                            match lf::GenericArg::decode(n).unwrap() {
                                lf::GenericArg::Lifetime(l) => out.push(named_slot(v, l)),
                                lf::GenericArg::Type(k) => kids.push(k),
                                _ => {}
                            }
                        }
                        kids.iter().rev().for_each(|&k| todo.push(k));
                    }
                    lf::TypeRef::Tuple(l) => v.ids(l).unwrap().iter().rev().for_each(|&id| todo.push(lf::TypeId(id))),
                    _ => {}
                }
            }
            Ok(())
        }
    }

    fn cx<'c, 'n>(v: &'c View<'n>, facts: &'c Facts, scope: &'c Scope) -> Cx<'c, 'n> {
        Cx { v, facts, written: &Syntax, scope, made: &[], alloc: a() }
    }

    fn generic_rows<'v>(v: &View<'v>) -> std::vec::Vec<lf::GenericParam> {
        v.rows(v.list(head::GENERICS)).unwrap().iter().map(|&n| lf::GenericParam::decode(n).unwrap()).collect()
    }

    const SIZED: ItemKey = ItemKey(0x51);
    const TR: ItemKey = ItemKey(0x71);
    const FN: ItemKey = ItemKey(0xf1);
    const FN_ONCE: ItemKey = ItemKey(0xf0);
    const S: ItemKey = ItemKey(0x5);

    /// `struct W<'a, T: 'a>(&'a T)`: the field's column is [Param(0)], `T: 'a` is
    /// `TypeOutlives(T, Param(0))` with an empty column over `T`, and the implied `Sized` (a
    /// derived clause) gets no column, with or without `Sized`'s count known (rule 25).
    /// CONTROL: `struct V<T>(&T)` is E0106, never a guessed lifetime.
    #[test]
    fn struct_field_column_and_outlives() {
        let low = lower("struct W<'a, T: 'a>(&'a T);");
        let v = low.view();
        let scope = Scope::item(&v, None, 0, a()).unwrap();
        assert_eq!(scope.params(), 1);
        let facts = Facts { traits: &[(SIZED, 0)] };
        let mut tys = PortableTys::new_in(a());
        let t = tys.intern(PNode::Param(0)).unwrap();
        let mut preds = Preds::new_in(scope.clone());
        let lf::GenericParam::Type { bounds, .. } = generic_rows(&v)[1] else { panic!("T") };
        let lf::Bound::Lifetime(n) = lf::Bound::decode(v.rows(bounds).unwrap()[0]).unwrap() else { panic!("'a") };
        let region = lifetime(&v, &scope, Binders::NONE, n).unwrap();
        assert_eq!(region, Region::Param(0));
        preds.push(PClause::TypeOutlives { ty: t, region }, Src::TypeOutlives { on: On::Made, lifetime: n, binders: Binders::NONE });
        let sized_args = tys.intern_list(&[t]).unwrap();
        preds.push(PClause::Trait { trait_: SIZED, args: sized_args }, Src::Derived);
        let field = lf::Field::decode(v.rows(v.list(head::FIELDS)).unwrap()[0]).unwrap();
        let field_ty = tys.intern(PNode::Ref(Mutbl::Not, t)).unwrap();
        let c = cx(&v, &facts, &scope);
        let head = adt_regions(&c, &mut tys, &preds, &[FieldAt { variant: 0, field: 0, written: field.ty, ty: field_ty }]).unwrap();
        assert_eq!(head.params.count(), 1);
        assert_eq!(head.fields.len(), 1);
        assert_eq!((head.fields[0].0, head.fields[0].1), (0, 0));
        assert_eq!(head.fields[0].2.as_slice(), [Region::Param(0)]);
        assert_eq!(head.outlives.as_slice(), [Outlives::Type { clause: 0, region: Region::Param(0) }]);
        // `T: 'a` (clause 0) has its empty column over `T`; the implied `Sized` (clause 1,
        // derived) has none, whatever counts the facts hold (rule 25).
        assert_eq!(head.clauses.len(), 1);
        assert_eq!((head.clauses[0].nth, head.clauses[0].col.len()), (0, 0));
        assert!(head.clauses.iter().all(|c| c.nth != 1), "a derived clause was given a column");
        // The same with no count for `Sized` filed: the same columns (the derived clause needs
        // no count).
        let no_sized = Facts { traits: &[] };
        let c = cx(&v, &no_sized, &scope);
        let bare = adt_regions(&c, &mut tys, &preds, &[FieldAt { variant: 0, field: 0, written: field.ty, ty: field_ty }]).unwrap();
        assert_eq!(bare.clauses.len(), 1);
        assert_eq!(bare.clauses[0].nth, 0);

        // CONTROL: the same field with its lifetime elided is the language's E0106.
        let low = lower("struct V<T>(&T);");
        let v = low.view();
        let scope = Scope::item(&v, None, 0, a()).unwrap();
        let preds = Preds::new_in(scope.clone());
        let field = lf::Field::decode(v.rows(v.list(head::FIELDS)).unwrap()[0]).unwrap();
        let c = cx(&v, &facts, &scope);
        let got = adt_regions(&c, &mut tys, &preds, &[FieldAt { variant: 0, field: 0, written: field.ty, ty: field_ty }]);
        assert!(matches!(got, Err(RegionsRefusal::Missing { .. })), "CONTROL: an elided field lifetime was filled: {got:?}");
    }

    /// `impl<'a> Tr<'a> for &'a S`: one parameter; the self column [Param(0)]; the trait
    /// reference's over `ExTrait(Tr, [&S])` is [Param(0) (Tr's own), Param(0) (Self's)].
    /// CONTROL: `impl Tr<'_> for &S` elides two lifetimes, which are two fresh parameters in
    /// slot order (self first): [Param(0)] and [Param(1), Param(0)]; one shared fresh
    /// parameter would fail.
    #[test]
    fn impl_header_columns() {
        let facts = Facts { traits: &[(TR, 1)] };
        let mut tys = PortableTys::new_in(a());
        let s = tys.intern(PNode::Adt(S, List::EMPTY)).unwrap();
        let self_ty = tys.intern(PNode::Ref(Mutbl::Not, s)).unwrap();
        let args = tys.intern_list(&[self_ty]).unwrap();
        let header = |v: &View<'_>| ImplAt {
            self_written: lf::TypeId(v.head[head::SELF_TY]),
            self_ty,
            trait_ref: Some(TraitRef { path: lf::TypeId(v.head_opt(head::TRAIT).unwrap()), trait_: TR, args }),
        };

        let low = lower("impl<'a> Tr<'a> for &'a S {}");
        let v = low.view();
        let scope = Scope::item(&v, None, 0, a()).unwrap();
        let head = impl_header_regions(&cx(&v, &facts, &scope), &mut tys, header(&v)).unwrap();
        assert_eq!(head.params.count(), 1);
        let (self_col, trait_col) = head.impl_header.as_ref().unwrap();
        assert_eq!(self_col.as_slice(), [Region::Param(0)]);
        let trait_col = trait_col.as_ref().unwrap();
        assert_eq!(trait_col.as_slice(), [Region::Param(0), Region::Param(0)]);
        assert_eq!(trait_col.own(), [Region::Param(0)]);

        // CONTROL: elided header lifetimes are fresh and distinct, numbered in slot order.
        let low = lower("impl Tr<'_> for &S {}");
        let v = low.view();
        let scope = Scope::item(&v, None, 0, a()).unwrap();
        let head = impl_header_regions(&cx(&v, &facts, &scope), &mut tys, header(&v)).unwrap();
        assert_eq!(head.params.count(), 2);
        let (self_col, trait_col) = head.impl_header.as_ref().unwrap();
        assert_eq!(self_col.as_slice(), [Region::Param(0)]);
        assert_eq!(trait_col.as_ref().unwrap().as_slice(), [Region::Param(1), Region::Param(0)], "CONTROL: elided header lifetimes not fresh");
    }

    /// `where T: Tr<'a>` with `trait Tr<'a>`: the clause's own slots as `trait_clause` writes
    /// them (`Tr`'s written lifetime arguments, [Param(0)]) are the count the one counter gives
    /// `ExTrait(Tr, [T])` (`regions::region_arity`, one), so the column lays and L4's `own()`
    /// reads `'a`. CONTROL: a counter that gives a trait no own slots (the bug before
    /// `Arity::trait_lifetimes`) refuses the clause as E0107, so no trait tie could be read.
    #[test]
    fn a_trait_clause_counts_its_lifetimes_as_the_counter_does() {
        let low = lower("fn f<'a, T>(t: T) where T: Tr<'a> {}");
        let v = low.view();
        let scope = Scope::item(&v, None, 0, a()).unwrap();
        let mut tys = PortableTys::new_in(a());
        let t = tys.intern(PNode::Param(0)).unwrap();
        let args = tys.intern_list(&[t]).unwrap();
        let lf::WherePred::Type { binder, ty, bounds } = lf::WherePred::decode(v.rows(v.list(head::WHERE)).unwrap()[0]).unwrap() else { panic!("a where predicate") };
        let lf::Bound::Trait { ty: path, binder: inner, .. } = lf::Bound::decode(v.rows(bounds).unwrap()[0]).unwrap() else { panic!("a trait bound") };
        let binders = Binders::outer(binder).with(inner).unwrap();
        let mut preds = Preds::new_in(scope.clone());
        preds.push(PClause::Trait { trait_: TR, args }, Src::Trait { on: On::Written(ty), bound: path, binders });
        let facts = Facts { traits: &[(TR, 1)] };
        let node = PNode::ExTrait(TR, args);
        let node_ty = tys.intern(node).unwrap();
        assert_eq!(region_arity(node_ty, node, &facts), Ok(1), "the one counter: `Tr`'s own slots");
        let head = clause_regions(&cx(&v, &facts, &scope), &mut tys, &preds, scope.params(), None).unwrap();
        assert_eq!(head.clauses.len(), 1);
        assert_eq!(head.clauses[0].col.as_slice(), [Region::Param(0)], "ORACLE: `T: Tr<'a>`'s column");
        assert_eq!(head.clauses[0].col.own(), [Region::Param(0)], "ORACLE: the clause's own slots are `Tr`'s arguments");

        // CONTROL: the counter that gave `ExTrait` none of its own.
        let none = Facts { traits: &[(TR, 0)] };
        let got = clause_regions(&cx(&v, &none, &scope), &mut tys, &preds, scope.params(), None);
        assert!(matches!(got, Err(RegionsRefusal::LifetimeArgs { want: 0, got: 1, .. })), "CONTROL: a trait with no own slots took `'a`: {got:?}");
    }

    /// `fn g<T: 'static>(t: T)` has `TypeOutlives(T, Static)`. CONTROL: `fn g<'a, T: 'a>` has
    /// `Param(0)` there (the name is resolved, not taken as `'static`), and `fn g<T: 'b>` is
    /// E0261.
    #[test]
    fn static_bound_is_type_outlives_static() {
        let facts = Facts { traits: &[] };
        let mut tys = PortableTys::new_in(a());
        let t = tys.intern(PNode::Param(0)).unwrap();
        let outlives_of = |text: &'static str, tys: &mut PortableTys<'static>| -> Result<(PClause, HeadRegions<Mem>), RegionsRefusal> {
            let low = lower(text);
            let v = low.view();
            let scope = Scope::item(&v, None, 0, a())?;
            let ty_param = generic_rows(&v).into_iter().find(|g| matches!(g, lf::GenericParam::Type { .. })).unwrap();
            let lf::GenericParam::Type { bounds, .. } = ty_param else { unreachable!() };
            let lf::Bound::Lifetime(n) = lf::Bound::decode(v.rows(bounds).unwrap()[0]).unwrap() else { panic!("a lifetime bound") };
            let region = lifetime(&v, &scope, Binders::NONE, n)?;
            let mut preds = Preds::new_in(scope.clone());
            let clause = PClause::TypeOutlives { ty: t, region };
            preds.push(clause, Src::TypeOutlives { on: On::Made, lifetime: n, binders: Binders::NONE });
            let head = clause_regions(&cx(&v, &facts, &scope), tys, &preds, scope.params(), None)?;
            Ok((clause, head))
        };
        let (clause, head) = outlives_of("fn g<T: 'static>(t: T) {}", &mut tys).unwrap();
        assert_eq!(clause, PClause::TypeOutlives { ty: t, region: Region::Static });
        assert_eq!(head.outlives.as_slice(), [Outlives::Type { clause: 0, region: Region::Static }]);
        assert_eq!(head.clauses.len(), 1);
        assert_eq!(head.clauses[0].col.ty(), t);

        // CONTROL: a named lifetime is that parameter, and an undeclared one is refused.
        let (clause, _) = outlives_of("fn g<'a, T: 'a>(t: T) {}", &mut tys).unwrap();
        assert_eq!(clause, PClause::TypeOutlives { ty: t, region: Region::Param(0) }, "CONTROL: `'a` read as `'static`");
        let got = outlives_of("fn g<T: 'b>(t: T) {}", &mut tys);
        assert!(matches!(got, Err(RegionsRefusal::Undeclared { .. })), "CONTROL: an undeclared lifetime was taken: {got:?}");
    }

    /// The trait and projection clauses of `where F: Fn(INPUTS) -> OUT` as preds.rs makes them.
    fn fn_clauses(text: &'static str, inputs: usize, tys: &mut PortableTys<'static>) -> Result<HeadRegions<Mem>, RegionsRefusal> {
        let low = lower(text);
        let v = low.view();
        let scope = Scope::item(&v, None, 0, a())?;
        let facts = Facts { traits: &[(FN, 0), (FN_ONCE, 0)] };
        let lf::WherePred::Type { binder, ty, bounds } = lf::WherePred::decode(v.rows(v.list(head::WHERE)).unwrap()[0]).unwrap() else { panic!("a where predicate") };
        let lf::Bound::Trait { ty: path, binder: inner, .. } = lf::Bound::decode(v.rows(bounds).unwrap()[0]).unwrap() else { panic!("a trait bound") };
        let f = tys.intern(PNode::Param(0)).unwrap();
        let u8_ = tys.intern(PNode::Uint(UintTy::U8)).unwrap();
        let r = tys.intern(PNode::Ref(Mutbl::Not, u8_)).unwrap();
        let ins: std::vec::Vec<PTy> = (0..inputs).map(|_| r).collect();
        let l = tys.intern_list(&ins).unwrap();
        let tuple = tys.intern(PNode::Tuple(l)).unwrap();
        let args = tys.intern_list(&[f, tuple]).unwrap();
        let seg = last_segment(&v, path)?;
        let binders = Binders::outer(binder).with(inner)?;
        let mut preds = Preds::new_in(scope.clone());
        preds.push(PClause::Trait { trait_: FN, args }, Src::Trait { on: On::Written(ty), bound: path, binders });
        let terms = binding_terms(&v, &seg, a())?;
        assert_eq!(terms.len(), 1);
        preds.push(PClause::Projection { trait_: FN_ONCE, args, name: Name::of("Output"), term: r }, Src::Binding { of: 0, term: terms[0].0, gat: terms[0].1 });
        clause_regions(&cx(&v, &facts, &scope), tys, &preds, scope.params(), None)
    }

    /// `where for<'x> F: Fn(&'x u8) -> &'x u8` keeps its binder: both clauses bind one
    /// variable, `Fn`'s column is [Late(0, 0)] (the input), the `Output` projection's
    /// [Late(0, 0), Late(0, 0)] (the input, then the output). CONTROL: with the binder elided
    /// (`Fn(&u8) -> &u8`) the input is a fresh variable and the output takes it (the same
    /// columns); with two inputs (`Fn(&u8, &u8) -> &u8`) the output is E0106.
    #[test]
    fn higher_ranked_bound_keeps_its_binder() {
        let mut tys = PortableTys::new_in(a());
        let x = Region::Late { binder: 0, var: 0 };
        let head = fn_clauses("fn h<F>(f: F) where for<'x> F: Fn(&'x u8) -> &'x u8 {}", 1, &mut tys).unwrap();
        assert_eq!(head.clauses.len(), 2);
        assert_eq!((head.clauses[0].nth, head.clauses[0].binders), (0, 1));
        assert_eq!(head.clauses[0].col.as_slice(), [x]);
        assert_eq!((head.clauses[1].nth, head.clauses[1].binders), (1, 1));
        assert_eq!(head.clauses[1].col.as_slice(), [x, x]);

        let elided = fn_clauses("fn h<F>(f: F) where F: Fn(&u8) -> &u8 {}", 1, &mut tys).unwrap();
        assert_eq!(elided.clauses[0].binders, 1);
        assert_eq!(elided.clauses[1].col.as_slice(), [x, x]);

        // CONTROL: two input lifetimes leave the output's elided lifetime with no rule.
        let got = fn_clauses("fn h<F>(f: F) where F: Fn(&u8, &u8) -> &u8 {}", 2, &mut tys);
        assert!(matches!(got, Err(RegionsRefusal::Missing { .. })), "CONTROL: an ambiguous elided output was filled: {got:?}");
    }

    /// Captures: 2024 takes every lifetime in scope, 2021 the named ones, `use<..>` exactly
    /// its list. CONTROL: 2021 with no named lifetime captures none, not all.
    #[test]
    fn opaque_captures_by_edition_and_use() {
        let caps = |text: &'static str, rule: Capture| -> std::vec::Vec<Region> {
            let low = lower(text);
            let v = low.view();
            let scope = Scope::item(&v, None, 0, a()).unwrap();
            let ret = lf::TypeId(v.head_opt(head::TY).unwrap());
            let mut out: Vec<Region, Mem> = Vec::new_in(a());
            captures(&v, &scope, a(), Some(ret), rule, scope.params(), None, &mut out).unwrap();
            out.iter().copied().collect()
        };
        assert_eq!(caps("fn f<'a, 'b>(x: &'a u8, y: &'b u8) -> impl Sized {}", Capture::All), [Region::Param(0), Region::Param(1)]);
        assert_eq!(caps("fn f<'a, 'b>(x: &'a u8, y: &'b u8) -> impl Tr<'b> {}", Capture::Named), [Region::Param(1)]);
        assert_eq!(caps("fn f<'a, 'b>(x: &'a u8, y: &'b u8) -> impl Sized + use<'b, 'a> {}", Capture::All), [Region::Param(0), Region::Param(1)]);
        assert_eq!(caps("fn f<'a, 'b>(x: &'a u8, y: &'b u8) -> impl Sized + use<'b> {}", Capture::All), [Region::Param(1)]);
        // CONTROL: the 2021 rule is not the 2024 one.
        assert!(caps("fn f<'a, 'b>(x: &'a u8, y: &'b u8) -> impl Sized {}", Capture::Named).is_empty(), "CONTROL: 2021 captured unnamed lifetimes");
        assert_eq!(Capture::rule(Edition::E2021, false, false), Capture::Named);
        assert_eq!(Capture::rule(Edition::E2021, false, true), Capture::All);
        assert_eq!(Capture::rule(Edition::E2024, false, false), Capture::All);
    }

    /// Removing an implied `Sized` keeps every later clause's link to its bound: the
    /// `Binding { of }` past it moves down by one. CONTROL: without the fix-up the binding would
    /// name the clause after its bound.
    #[test]
    fn removing_a_clause_keeps_links() {
        let mut tys = PortableTys::new_in(a());
        let t = tys.intern(PNode::Param(0)).unwrap();
        let l = tys.intern_list(&[t]).unwrap();
        let mut preds = Preds::new_in(Scope::new_in(a()));
        preds.push(PClause::Trait { trait_: SIZED, args: l }, Src::Derived);
        preds.push(PClause::Trait { trait_: TR, args: l }, Src::Trait { on: On::Made, bound: lf::TypeId(0), binders: Binders::NONE });
        preds.push(PClause::Projection { trait_: TR, args: l, name: Name::of("X"), term: t }, Src::Binding { of: 1, term: None, gat: List::EMPTY });
        preds.remove_clause(0);
        assert_eq!(preds.clauses.len(), 2);
        assert_eq!(preds.sources.len(), 2);
        assert_eq!(preds.sources[1], Src::Binding { of: 0, term: None, gat: List::EMPTY }, "CONTROL: a binding's link was not moved");
    }
}
