//! The lifetime part of a head: what the erased types of [`super::heads`] leave out and the
//! borrow check needs (docs/v2/plan/design/flow.md, "What the signature must say (H1)").
//!
//! Three records, each made when first asked for, from the item's own lowered syntax (the
//! generics, parameters and types the head builder walks) lined up with the head's erased types,
//! and kept beside the heads in [`Lives`] (`Store::lives`):
//!
//! - [`FnLife`], per fn: one [`Region`] per ORIGIN POSITION of each input in order, then of the
//!   output; where each input's regions end; and every declared `'a: 'b`, the fn's and its
//!   container's, where clauses included. Implied bounds are not listed (the lowering derives
//!   them from the types), and neither is `T: 'a` (it relates no two regions).
//! - [`AdtLife`], per struct, enum or union: how many lifetime parameters it declares, their
//!   variances, its declared bounds, and each field's regions in the ADT's own parameters.
//! - [`ContainerLife`], per impl or trait: its lifetime parameters, its declared bounds, and for
//!   an impl the regions of its self type, so `Self` in a method is its impl's lifetimes and
//!   never a region apart.
//!
//! POSITIONS. A type's positions are counted in pre-order, a node's own before its children's:
//! one per reference, one per ADT that declares any lifetime parameter ([`GRAIN`]: the lowering
//! counts ONE per such ADT today; `Grain::PerParam` is the end state and one constant away),
//! one per trait object (its bound), one per opaque or associated type ([`Region::Hidden`]).
//! A fn pointer has none HERE (the head record's lifetime part). The borrow lowering's count
//! (crates/blaze-flow-lower/src/sig.rs, lib.rs `npos`) gives a fn pointer its inputs' and
//! output's positions since lane B15, and reads which its binder binds (`Region::Bound` there);
//! this record does not say a fn pointer's lifetimes, so the two counts differ exactly at fn
//! pointers holding a reference, an ADT with lifetimes, an opaque or a trait object.
//!
//! REGIONS. `Named(i)` is a lifetime parameter: the container's first (an impl's declared ones,
//! then one per lifetime elided in its header), then the fn's own, in declaration order.
//! `Anon(i)` is an elided input lifetime. Elided OUTPUT lifetimes are already resolved, by the
//! Reference's "Lifetime elision": "Each elided lifetime in the parameters becomes a distinct
//! lifetime parameter"; "If there is exactly one lifetime used in the parameters (elided or
//! not), that lifetime is assigned to all elided output lifetimes"; and, in method signatures,
//! "If the receiver has type `&Self` or `&mut Self`, then the lifetime of that reference to
//! `Self` is assigned to all elided output lifetime parameters". Where none of them decides, to
//! elide an output lifetime is an error, and the reader refuses ([`LifeRefusal::ElisionUndecided`]).
//! "Used in the parameters" is what the parameters WRITE: `Self`'s lifetimes are the impl's and
//! are not counted. A parameter that may write a lifetime where no position is (a fn pointer, a
//! bound's arguments, a type alias) makes the count unknown, and an elided output is then
//! refused, never guessed.
//!
//! TRAIT OBJECTS. The Reference, "Default trait object lifetimes": "If the trait object is used
//! as `&'a dyn Trait`, the default is `'a`"; as an argument of a type with no lifetime
//! parameter, of traits with none, it is `'static`. Every other default is refused by name
//! ([`LifeRefusal::ObjectDefault`]).
//!
//! VARIANCE. The Reference, "Subtyping and Variance", gives the table (the variance in `'a`,
//! then in `T`): `&'a T` covariant, covariant; `&'a mut T` covariant, invariant; `*const T`
//! covariant; `*mut T` invariant; `[T]` and `[T; n]` covariant; `fn() -> T` covariant;
//! `fn(T) -> ()` contravariant; `std::cell::UnsafeCell<T>` invariant;
//! `std::marker::PhantomData<T>` covariant; `dyn Trait<T> + 'a` covariant, invariant. A
//! struct's, enum's or union's own variance comes from its fields, and a parameter used in
//! positions of different variances is invariant. This first version computes it only for an
//! ADT whose every field is made of references, raw pointers, tuples, slices, arrays, fn
//! pointers (lane B15: inputs contravariant, output covariant, a lifetime of the pointer's own
//! `for<..>` binder no parameter's), built-in types and its own type parameters: there the
//! table alone decides. For every other ADT each
//! lifetime parameter is answered INVARIANT with `computed: false`. Invariant is the sound
//! over-approximation for the check: it demands that two lifetimes be equal where the true
//! variance would let one outlive the other, so it can only add findings, never accept a
//! program the true variance refuses.
//!
//! What is not lined up exactly is refused by name ([`LifeRefusal`]), never guessed: a type
//! alias over a type with positions, a defaulted argument with positions, `Self::X` or a
//! qualified path that names an ADT, two different lifetimes of one ADT under
//! `Grain::OnePerAdt`. A const's or static's row has no parameters and its elided lifetimes
//! are `'static` by another rule; asked here, an elided one is refused.
//!
//! Every walk is a work list with a bound; nothing recurses.

use blaze_hash::HashMap;
use blaze_id::ItemKey as LibKey;
use blaze_lower::machine::sig;
use blaze_lower::{TextAt, View, form as lf, head};
use blaze_own::{Alloc, Vec};
use blaze_surface::Range;
use blaze_typeck as tc;
use blaze_typeck::{ItemKey, Name, PNode, PTy, PTys};

use super::head_record::HeadAt;
use super::heads::{Store, lower};
use super::keys::{Keys, checker_of};
use super::locals::Locals;
use super::source::NamesSource;
use super::types::path_texts;
use crate::resolve::{Kind, Ns, Paths};

/// The allocator the records are kept in: the store's.
type Mem = Alloc<'static>;
type List<T> = Vec<T, Mem>;
type Map<K, V> = HashMap<K, V, Mem>;

/// What stands at one origin position.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum Region {
    /// `'static`.
    Static,
    /// A lifetime parameter: the container's first, then the item's own.
    Named(u32),
    /// An elided input lifetime: each its own parameter.
    Anon(u32),
    /// A position whose lifetimes the head does not say (an opaque or associated type).
    Hidden(u32),
}

/// How many positions an ADT with lifetime parameters has.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Grain {
    /// One, whatever it declares: what the lowering counts today. Two different lifetimes
    /// written on one ADT are refused ([`LifeRefusal::TwoLifetimesOneOrigin`]).
    OnePerAdt,
    /// One per lifetime parameter: the end state.
    PerParam,
}

/// The grain every record here is made in. One home: the lowering's count and this move together.
pub const GRAIN: Grain = Grain::OnePerAdt;

/// A fn's lifetime part (flow.md's `SigLife`, owned).
#[derive(Debug)]
pub struct FnLife {
    /// The regions of every input's positions, in order, then the output's.
    pub regions: List<Region>,
    /// Per input, where its regions end in `regions`; the output's are the rest.
    pub ends: List<u32>,
    /// `(a, b)`: `a: b` is declared, by the container (first) or the fn.
    pub bounds: List<(Region, Region)>,
    /// How many of the `Named` regions are the container's (`Named(0..container)`).
    pub container: u32,
    /// How many `Named` regions there are in all.
    pub named: u32,
}

/// A variance (the Reference, "Subtyping and Variance").
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Variance {
    Co,
    Contra,
    In,
}

/// One lifetime parameter's variance, and whether it was computed from the fields
/// (`computed: false`: answered `In` as the sound over-approximation; see the module).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct LifeVariance {
    pub variance: Variance,
    pub computed: bool,
}

/// An ADT's lifetime part.
#[derive(Debug)]
pub struct AdtLife {
    /// How many lifetime parameters it declares: `Named(0..lifetimes)`.
    pub lifetimes: u32,
    /// Per lifetime parameter.
    pub variances: List<LifeVariance>,
    /// `(a, b)`: `a: b` is declared.
    pub bounds: List<(Region, Region)>,
    /// The regions of every field's positions, the fields in the head's order (`AdtRow::fields`:
    /// every variant's, in order).
    pub fields: List<Region>,
    /// Per field, where its regions end in `fields`.
    pub field_ends: List<u32>,
}

/// An impl's or a trait's lifetime part.
#[derive(Debug)]
pub struct ContainerLife {
    /// Its declared lifetime parameters: `Named(0..lifetimes)`.
    pub lifetimes: u32,
    /// The lifetimes elided in an impl's self type (`impl Tr for &u8`, `impl S<'_>`): each a
    /// parameter of the impl with no name, `Named(lifetimes..lifetimes + elided)`.
    pub elided: u32,
    /// An impl's self type's regions, one per position; empty for a trait (`Self` is a type
    /// parameter there and has no position).
    pub self_regions: List<Region>,
    /// `(a, b)`: `a: b` is declared.
    pub bounds: List<(Region, Region)>,
    /// Each declared lifetime's name (its hash) and index: how a member fn's text names them.
    pub names: List<(u64, u32)>,
}

/// One head's lifetime part as the store holds it ([`Store::lives_of`]): made, or refused with
/// its typed reason (a refusal is a fact of the head too: a head record holds it).
#[derive(Clone, Copy, Debug)]
pub enum LifeOf<'a> {
    Fn(Result<&'a FnLife, LifeRefusal>),
    Adt(Result<&'a AdtLife, LifeRefusal>),
    Container(Result<&'a ContainerLife, LifeRefusal>),
}

/// Why a lifetime part was not made: the rule, and where in the item's text.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LifeRefusal {
    /// The store holds no head for the item (ask `Store::ensure` first).
    NoHead,
    /// The item's text is not in the names library, or did not lower.
    Text,
    /// A row or a range of the lowered text is not what its column holds.
    Form,
    /// The checker's type table has no such node or list.
    TypeTable,
    /// An ADT the key map does not know.
    UnknownAdt,
    /// The text's parameters are not as many as the head's inputs.
    ParamCount { text: u32, head: u32 },
    /// The text's fields are not as many as the head's.
    FieldCount { text: u32, head: u32 },
    /// The written type and the erased type are not the same form.
    Unaligned { at: Range },
    /// A path the names library does not resolve.
    Unresolved { at: Range },
    /// A type alias over a type with positions.
    Alias { at: Range },
    /// A defaulted generic argument with positions.
    DefaultWithPositions { at: Range },
    /// Lifetime arguments written, but not as many as the ADT declares.
    LifetimeArgs { at: Range },
    /// Under `Grain::OnePerAdt`: two different lifetimes on one ADT.
    TwoLifetimesOneOrigin { at: Range },
    /// A lifetime name no generics list in scope declares.
    UnknownLifetime { at: Range },
    /// An elided output lifetime that no rule of the Reference's "Lifetime elision" decides, or
    /// whose deciding count is unknown (a lifetime may be written where no position is).
    ElisionUndecided { at: Range },
    /// An elided lifetime in a field's type.
    ElidedInField { at: Range },
    /// A trait object whose default lifetime needs its containing type's or its trait's bounds.
    ObjectDefault { at: Range },
    /// `Self` where no impl's self regions are known, or they are not as many as its positions.
    SelfRegions { at: Range },
    /// A closure or coroutine type in a head.
    TypeNode,
    /// A walk passed its bound.
    Bound,
}

impl LifeRefusal {
    /// The rule that refused, in words.
    pub fn rule(&self) -> &'static str {
        match self {
            LifeRefusal::NoHead => "the store holds no head for the item",
            LifeRefusal::Text => "the item's text is not in the names library or did not lower",
            LifeRefusal::Form => "a row of the lowered text is not what its column holds",
            LifeRefusal::TypeTable => "the type table has no such node",
            LifeRefusal::UnknownAdt => "an ADT the key map does not know",
            LifeRefusal::ParamCount { .. } => "the text's parameters are not as many as the head's inputs",
            LifeRefusal::FieldCount { .. } => "the text's fields are not as many as the head's",
            LifeRefusal::Unaligned { .. } => "the written type and the erased type are not the same form",
            LifeRefusal::Unresolved { .. } => "a path the names library does not resolve",
            LifeRefusal::Alias { .. } => "a type alias over a type with lifetime positions",
            LifeRefusal::DefaultWithPositions { .. } => "a defaulted generic argument with lifetime positions",
            LifeRefusal::LifetimeArgs { .. } => "lifetime arguments written, but not as many as the type declares",
            LifeRefusal::TwoLifetimesOneOrigin { .. } => "two different lifetimes on one ADT (one position per ADT)",
            LifeRefusal::UnknownLifetime { .. } => "a lifetime no generics list in scope declares",
            LifeRefusal::ElisionUndecided { .. } => "an elided output lifetime no elision rule decides",
            LifeRefusal::ElidedInField { .. } => "an elided lifetime in a field's type",
            LifeRefusal::ObjectDefault { .. } => "a trait object's default lifetime that needs declared bounds",
            LifeRefusal::SelfRegions { .. } => "`Self` with no known self-type regions",
            LifeRefusal::TypeNode => "a closure or coroutine type in a head",
            LifeRefusal::Bound => "a walk passed its bound",
        }
    }
}

impl core::fmt::Display for LifeRefusal {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "lifetimes: {} ({self:?})", self.rule())
    }
}

impl From<lf::BadRow> for LifeRefusal {
    fn from(_: lf::BadRow) -> LifeRefusal {
        LifeRefusal::Form
    }
}

impl From<TextAt> for LifeRefusal {
    fn from(_: TextAt) -> LifeRefusal {
        LifeRefusal::Form
    }
}

/// The lifetime records made so far, by key; a refusal is kept too (asked again, it is the same).
pub struct Lives {
    // `Rows`, filed through `&self`: a head imported during a body's check files its part here.
    // In the store's allocator, `alloc` (front2 rows.rs says whose).
    fns: super::rows::Rows<ItemKey, Result<FnLife, LifeRefusal>, Mem>,
    adts: super::rows::Rows<LibKey, Result<AdtLife, LifeRefusal>, Mem>,
    /// By the container's checker key (`checker_of`), as its impl's head is filed.
    containers: super::rows::Rows<ItemKey, Result<ContainerLife, LifeRefusal>, Mem>,
    /// How many lifetime parameters an ADT's or a trait's generics declare.
    counts: Map<LibKey, u32>,
    alloc: Mem,
}

impl Lives {
    pub fn new_in(alloc: Mem) -> Lives {
        Lives { fns: super::rows::Rows::new_in(alloc), adts: super::rows::Rows::new_in(alloc), containers: super::rows::Rows::new_in(alloc), counts: HashMap::new_in(alloc), alloc }
    }
}

/// The bound on any one walk (types, rows): a head past it is refused.
const MOST: u32 = 1 << 16;

/// A position not yet filled; none is left in a record.
const HOLE: Region = Region::Hidden(u32::MAX);

/// What an elided lifetime is where it is read.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Elide {
    /// A fn's parameter: a fresh `Anon`.
    Input,
    /// A fn's output: the region the elision rules decided, if they did.
    Output(Option<Region>),
    /// An impl's header: a fresh parameter of the impl.
    Header,
    /// A field: an error.
    Field,
}

/// What encloses a type: a trait object's default lifetime reads it.
#[derive(Clone, Copy)]
enum Ctx {
    Ref(Region),
    /// A type argument of an ADT, which has lifetime parameters or not.
    Adt(bool),
    Other,
}

/// The written side of a pair.
#[derive(Clone, Copy)]
enum Syn {
    Ty(lf::TypeId),
    /// `Self`, unwritten: a `self` or `&self` receiver's.
    SelfTy,
}

/// One pair still to line up: an erased type, its written type, and the index in `regions` of
/// its first position.
#[derive(Clone, Copy)]
struct Pair {
    syn: Syn,
    ty: PTy,
    base: u32,
    ctx: Ctx,
}

/// The text and module of `lib`: a body's local item's from its row, else the names library's.
fn item_text<'n, S: NamesSource<'n>>(names: S, locals: &Locals<'n>, lib: LibKey) -> Result<(&'n str, LibKey), LifeRefusal> {
    match locals.head_of(lib) {
        Some(Ok((_, text, module))) => Ok((text, module)),
        Some(Err(_)) => Err(LifeRefusal::Text),
        None => {
            let text = names.signature_text_of(lib).ok_or(LifeRefusal::Text)?.text;
            Ok((text, names.home_of(lib).ok_or(LifeRefusal::Text)?))
        }
    }
}

/// Where type `t` is written, in its item's text.
fn site(v: &View<'_>, t: lf::TypeId) -> Result<Range, LifeRefusal> {
    Ok(v.site(v.type_range(t)?)?)
}

/// Whether path `p` is `Self` alone.
fn is_self_path(v: &View<'_>, p: tc::PathIdx) -> Result<bool, LifeRefusal> {
    let segs = v.segments(v.path(p)?)?;
    match segs {
        [one] => Ok(lf::Segment::decode(*one)?.kind == lf::SegKind::SelfTy),
        _ => Ok(false),
    }
}

/// How many lifetime parameters `v`'s generics declare.
fn declared_lifetimes(v: &View<'_>) -> Result<u32, LifeRefusal> {
    let mut n = 0u32;
    for &row in v.rows(v.list(head::GENERICS))? {
        if matches!(lf::GenericParam::decode(row)?, lf::GenericParam::Lifetime { .. }) {
            n += 1;
        }
    }
    Ok(n)
}

/// One item's reader: the scope its lifetimes are named in and the counters of what it makes.
struct Reader<'c, 'n, S> {
    names: S,
    keys: &'c Keys,
    locals: &'c Locals<'n>,
    tys: PTys<'c>,
    counts: &'c mut Map<LibKey, u32>,
    /// The module the item's paths are resolved in, and its type and const parameters' names
    /// (a parameter shadows an item of its name).
    module: LibKey,
    params: List<Name>,
    /// Each lifetime in scope: its name's hash and its `Named` index.
    scope: List<(u64, u32)>,
    /// The next `Named` index.
    named: u32,
    /// What `Self` is, one region per position; `None` where there is no `Self` type.
    self_regions: Option<List<Region>>,
    anon: u32,
    hidden: u32,
    /// The distinct lifetimes the parameters write, elided or not.
    used: List<Region>,
    /// A parameter may write a lifetime where no position is: `used` is not all of them.
    outside: bool,
    alloc: Mem,
}

impl<'c, 'n, S: NamesSource<'n>> Reader<'c, 'n, S> {
    fn new(names: S, keys: &'c Keys, locals: &'c Locals<'n>, tys: PTys<'c>, counts: &'c mut Map<LibKey, u32>, module: LibKey, alloc: Mem) -> Reader<'c, 'n, S> {
        Reader {
            names,
            keys,
            locals,
            tys,
            counts,
            module,
            params: Vec::new_in(alloc),
            scope: Vec::new_in(alloc),
            named: 0,
            self_regions: None,
            anon: 0,
            hidden: 0,
            used: Vec::new_in(alloc),
            outside: false,
            alloc,
        }
    }

    fn node(&self, t: PTy) -> Result<PNode, LifeRefusal> {
        self.tys.node(t).map_err(|_| LifeRefusal::TypeTable)
    }

    fn list(&self, l: tc::List) -> Result<&'c [PTy], LifeRefusal> {
        self.tys.list(l).map_err(|_| LifeRefusal::TypeTable)
    }

    /// How many lifetime parameters the ADT or trait `lib` declares, from its text's generics.
    fn lifetimes_of(&mut self, lib: LibKey) -> Result<u32, LifeRefusal> {
        if let Some(&n) = self.counts.get(&lib) {
            return Ok(n);
        }
        let (text, _) = item_text(self.names, self.locals, lib)?;
        let low = lower(self.names, lib, text).map_err(|_| LifeRefusal::Text)?;
        let n = declared_lifetimes(&low.view())?;
        self.counts.insert(lib, n);
        Ok(n)
    }

    /// How many positions the ADT `k` itself has.
    fn adt_positions(&mut self, k: ItemKey) -> Result<u32, LifeRefusal> {
        let lib = self.keys.library_in(self.names, k).ok_or(LifeRefusal::UnknownAdt)?;
        let n = self.lifetimes_of(lib)?;
        Ok(match GRAIN {
            Grain::OnePerAdt => u32::from(n > 0),
            Grain::PerParam => n,
        })
    }

    /// The number of origin positions of `t` (the module's "POSITIONS").
    fn count(&mut self, t: PTy) -> Result<u32, LifeRefusal> {
        let mut stack: List<PTy> = Vec::new_in(self.alloc);
        stack.push(t);
        let (mut n, mut steps) = (0u32, 0u32);
        while let Some(t) = stack.pop() {
            steps += 1;
            if steps > MOST {
                return Err(LifeRefusal::Bound);
            }
            match self.node(t)? {
                PNode::Ref(_, i) => {
                    n += 1;
                    stack.push(i);
                }
                PNode::Ptr(_, i) | PNode::Slice(i) | PNode::Pat(i, _) | PNode::Array(i, _) => stack.push(i),
                PNode::Tuple(l) | PNode::Record(_, l) | PNode::Union(l) => stack.extend_from_slice(self.list(l)?),
                PNode::Adt(k, l) => {
                    n += self.adt_positions(k)?;
                    stack.extend_from_slice(self.list(l)?);
                }
                PNode::Opaque(_, _) | PNode::Alias { .. } | PNode::Dynamic(_) => n += 1,
                PNode::Closure { .. } | PNode::Future(..) | PNode::Coroutine { .. } => return Err(LifeRefusal::TypeNode),
                _ => {}
            }
        }
        Ok(n)
    }

    /// Adds `v`'s declared lifetime parameters to the scope, in order; their count.
    fn declare(&mut self, v: &View<'_>) -> Result<u32, LifeRefusal> {
        let mut n = 0u32;
        for &row in v.rows(v.list(head::GENERICS))? {
            if let lf::GenericParam::Lifetime { name, .. } = lf::GenericParam::decode(row)? {
                self.scope.push((v.name(name)?, self.named));
                self.named += 1;
                n += 1;
            }
        }
        Ok(n)
    }

    /// Adds `v`'s type and const parameters' names to `params`.
    fn declare_params(&mut self, v: &View<'_>) -> Result<(), LifeRefusal> {
        for &row in v.rows(v.list(head::GENERICS))? {
            match lf::GenericParam::decode(row)? {
                lf::GenericParam::Lifetime { .. } => {}
                lf::GenericParam::Type { name, .. } | lf::GenericParam::Const { name, .. } => self.params.push(Name(v.name(name)?)),
            }
        }
        Ok(())
    }

    /// The region a written lifetime names; `None` for `'_`.
    fn written(&self, v: &View<'_>, n: lf::NameIdx) -> Result<Option<Region>, LifeRefusal> {
        let text = v.name_text(n)?;
        let bare = text.strip_prefix('\'').unwrap_or(text);
        if bare == "_" {
            return Ok(None);
        }
        if bare == "static" {
            return Ok(Some(Region::Static));
        }
        let hash = v.name(n)?;
        match self.scope.iter().find(|(h, _)| *h == hash) {
            Some(&(_, i)) => Ok(Some(Region::Named(i))),
            None => Err(LifeRefusal::UnknownLifetime { at: v.site(v.name_range(n)?)? }),
        }
    }

    fn elided(&mut self, mode: Elide, at: Range) -> Result<Region, LifeRefusal> {
        match mode {
            Elide::Input => {
                self.anon += 1;
                Ok(Region::Anon(self.anon - 1))
            }
            Elide::Output(Some(r)) => Ok(r),
            Elide::Output(None) => Err(LifeRefusal::ElisionUndecided { at }),
            Elide::Header => {
                self.named += 1;
                Ok(Region::Named(self.named - 1))
            }
            Elide::Field => Err(LifeRefusal::ElidedInField { at }),
        }
    }

    /// The region at a place a lifetime is or could be written: the written one, else the
    /// elided one; in a parameter it is one of the lifetimes the parameters use.
    fn site_region(&mut self, v: &View<'_>, lifetime: Option<lf::NameIdx>, mode: Elide, at: Range) -> Result<Region, LifeRefusal> {
        let named = match lifetime {
            Some(n) => self.written(v, n)?,
            None => None,
        };
        let r = match named {
            Some(r) => r,
            None => self.elided(mode, at)?,
        };
        if mode == Elide::Input && !self.used.iter().any(|u| *u == r) {
            self.used.push(r);
        }
        Ok(r)
    }

    /// The declared `'a: 'b` of `v`'s generics and where clauses, in order.
    fn declared_bounds(&self, v: &View<'_>, out: &mut List<(Region, Region)>) -> Result<(), LifeRefusal> {
        let mut preds: List<(lf::NameIdx, tc::List)> = Vec::new_in(self.alloc);
        for &row in v.rows(v.list(head::GENERICS))? {
            if let lf::GenericParam::Lifetime { name, bounds } = lf::GenericParam::decode(row)? {
                preds.push((name, bounds));
            }
        }
        for &row in v.rows(v.list(head::WHERE))? {
            if let lf::WherePred::Lifetime { name, bounds } = lf::WherePred::decode(row)? {
                preds.push((name, bounds));
            }
        }
        for &(name, bounds) in preds.iter() {
            let at = v.site(v.name_range(name)?)?;
            let a = self.written(v, name)?.ok_or(LifeRefusal::UnknownLifetime { at })?;
            for &row in v.rows(bounds)? {
                if let lf::Bound::Lifetime(b) = lf::Bound::decode(row)? {
                    let b = self.written(v, b)?.ok_or(LifeRefusal::UnknownLifetime { at })?;
                    out.push((a, b));
                }
            }
        }
        Ok(())
    }

    /// Whether path `p` writes no lifetime and hides none: its type arguments go to `todo`.
    /// A form not known to be quiet answers `false`.
    fn quiet_path(&mut self, v: &View<'_>, p: tc::PathIdx, todo: &mut List<lf::TypeId>) -> Result<bool, LifeRefusal> {
        let Ok((abs, texts, segs, _)) = path_texts(v, p) else { return Ok(false) };
        for s in segs.iter() {
            if s.kind == lf::SegKind::Anchor {
                return Ok(false);
            }
            if !s.has_args {
                continue;
            }
            for &row in v.rows(s.args)? {
                match lf::GenericArg::decode(row)? {
                    lf::GenericArg::Type(t) => todo.push(t),
                    lf::GenericArg::Const(_) => {}
                    lf::GenericArg::AssocType { args, ty, .. } if v.rows(args)?.is_empty() => todo.push(ty),
                    _ => return Ok(false),
                }
            }
        }
        let Some(first) = texts.first().copied() else { return Ok(false) };
        if !abs && first == "Self" {
            return Ok(true);
        }
        if !abs && texts.len() <= 2 && self.params.iter().any(|n| *n == Name::of(first)) {
            return Ok(true);
        }
        let Ok(found) = (Paths { names: self.names }).path(self.module, abs, &texts, Ns::Type) else { return Ok(false) };
        if found.consumed as usize != texts.len() {
            return Ok(false);
        }
        Ok(match found.kind {
            Kind::Builtin(_) => true,
            Kind::Struct | Kind::Enum | Kind::Union | Kind::Trait => matches!(self.lifetimes_of(found.key), Ok(0)),
            _ => false,
        })
    }

    /// Whether the bounds `bounds` (a `dyn`'s or an `impl`'s) write no lifetime and hide none,
    /// a lifetime bound itself allowed or not.
    fn quiet_bounds(&mut self, v: &View<'_>, bounds: tc::List, lifetime_ok: bool, todo: &mut List<lf::TypeId>) -> Result<bool, LifeRefusal> {
        for &row in v.rows(bounds)? {
            match lf::Bound::decode(row)? {
                lf::Bound::Trait { ty, binder, .. } => {
                    if !v.rows(binder)?.is_empty() {
                        return Ok(false);
                    }
                    let lf::TypeRef::Path(p) = v.ty(ty)? else { return Ok(false) };
                    if !self.quiet_path(v, p, todo)? {
                        return Ok(false);
                    }
                }
                lf::Bound::Lifetime(_) if lifetime_ok => {}
                _ => return Ok(false),
            }
        }
        Ok(true)
    }

    /// Whether every type in `todo`, and all they are made of, writes no lifetime and hides none.
    fn quiet_all(&mut self, v: &View<'_>, mut todo: List<lf::TypeId>) -> Result<bool, LifeRefusal> {
        let mut steps = 0u32;
        while let Some(t) = todo.pop() {
            steps += 1;
            if steps > MOST {
                return Err(LifeRefusal::Bound);
            }
            match v.ty(t)? {
                lf::TypeRef::Never | lf::TypeRef::Infer => {}
                lf::TypeRef::Ptr { inner, .. } | lf::TypeRef::Slice(inner) => todo.push(inner),
                lf::TypeRef::Array { elem, .. } => todo.push(elem),
                lf::TypeRef::Pat { base, .. } => todo.push(base),
                lf::TypeRef::Tuple(l) => {
                    for &i in v.ids(l)? {
                        todo.push(lf::TypeId(i));
                    }
                }
                lf::TypeRef::Path(p) => {
                    if !self.quiet_path(v, p, &mut todo)? {
                        return Ok(false);
                    }
                }
                lf::TypeRef::Dyn(b) | lf::TypeRef::Impl(b) => {
                    if !self.quiet_bounds(v, b, false, &mut todo)? {
                        return Ok(false);
                    }
                }
                // A reference, a fn pointer, a `for<..>` binder, a macro call: a lifetime is or
                // may be written.
                _ => return Ok(false),
            }
        }
        Ok(true)
    }

    fn quiet(&mut self, v: &View<'_>, syn: Syn) -> Result<bool, LifeRefusal> {
        match syn {
            Syn::SelfTy => Ok(true),
            Syn::Ty(t) => {
                let mut todo: List<lf::TypeId> = Vec::new_in(self.alloc);
                todo.push(t);
                self.quiet_all(v, todo)
            }
        }
    }

    /// In a parameter: a written type left unread may use a lifetime the count must know.
    fn left_unread(&mut self, v: &View<'_>, syn: Syn, mode: Elide) -> Result<(), LifeRefusal> {
        if mode == Elide::Input && !self.outside && !self.quiet(v, syn)? {
            self.outside = true;
        }
        Ok(())
    }

    /// `Self`'s `n` positions from `base`: the impl's self type's regions.
    fn copy_self(&self, base: u32, n: u32, at: Range, regions: &mut List<Region>) -> Result<(), LifeRefusal> {
        let own = self.self_regions.as_ref().ok_or(LifeRefusal::SelfRegions { at })?;
        if own.len() as u32 != n {
            return Err(LifeRefusal::SelfRegions { at });
        }
        for (k, &r) in own.iter().enumerate() {
            regions[base as usize + k] = r;
        }
        Ok(())
    }

    /// A path written where the erased type is the ADT `k` with arguments `l`: the path must
    /// name that ADT itself; fills its own positions and pushes its type arguments.
    #[expect(clippy::too_many_arguments, reason = "one pair's parts and the walk's two lists")]
    fn adt_path(&mut self, v: &View<'_>, p: tc::PathIdx, at: Range, k: ItemKey, l: tc::List, pair: Pair, mode: Elide, regions: &mut List<Region>, work: &mut List<Pair>) -> Result<(), LifeRefusal> {
        let unaligned = LifeRefusal::Unaligned { at };
        let (abs, texts, segs, _) = path_texts(v, p).map_err(|_| unaligned)?;
        let Some(first) = texts.first().copied() else { return Err(unaligned) };
        // `Self::X` or `<T as Tr>::X` that the head builder answered with an ADT: the text does
        // not say which of its lifetimes are whose.
        if segs.iter().any(|s| s.kind == lf::SegKind::Anchor) || (!abs && first == "Self") {
            return Err(unaligned);
        }
        if !abs && texts.len() == 1 && self.params.iter().any(|n| *n == Name::of(first)) {
            return Err(unaligned);
        }
        let found = (Paths { names: self.names }).path(self.module, abs, &texts, Ns::Type).map_err(|_| LifeRefusal::Unresolved { at })?;
        match found.kind {
            Kind::Struct | Kind::Enum | Kind::Union if found.consumed as usize == texts.len() && checker_of(found.key) == k => {}
            Kind::TypeAlias => return Err(LifeRefusal::Alias { at }),
            _ => return Err(unaligned),
        }
        let Some((last, before)) = segs.split_last() else { return Err(unaligned) };
        if before.iter().any(|s| s.has_args) {
            return Err(unaligned);
        }
        let declared = self.lifetimes_of(found.key)?;
        let mut lifetimes: List<Option<Region>> = Vec::new_in(self.alloc);
        // The type and const arguments in order: a const argument has no written type to read.
        let mut args: List<Option<lf::TypeId>> = Vec::new_in(self.alloc);
        if last.has_args {
            for &row in v.rows(last.args)? {
                match lf::GenericArg::decode(row)? {
                    lf::GenericArg::Lifetime(n) => lifetimes.push(self.written(v, n)?),
                    lf::GenericArg::Type(t) => args.push(Some(t)),
                    lf::GenericArg::Const(_) => args.push(None),
                    _ => return Err(unaligned),
                }
            }
        }
        if !lifetimes.is_empty() && lifetimes.len() as u32 != declared {
            return Err(LifeRefusal::LifetimeArgs { at });
        }
        let mut per: List<Region> = Vec::new_in(self.alloc);
        for i in 0..declared {
            let r = match lifetimes.get(i as usize).copied().flatten() {
                Some(r) => r,
                None => self.elided(mode, at)?,
            };
            if mode == Elide::Input && !self.used.iter().any(|u| *u == r) {
                self.used.push(r);
            }
            per.push(r);
        }
        let mut off = pair.base;
        match GRAIN {
            Grain::PerParam => {
                for &r in per.iter() {
                    regions[off as usize] = r;
                    off += 1;
                }
            }
            Grain::OnePerAdt => {
                if let Some(&one) = per.first() {
                    if per.iter().any(|r| *r != one) {
                        return Err(LifeRefusal::TwoLifetimesOneOrigin { at });
                    }
                    regions[off as usize] = one;
                    off += 1;
                }
            }
        }
        let elems = self.list(l)?;
        if args.len() > elems.len() {
            return Err(unaligned);
        }
        for (j, &e) in elems.iter().enumerate() {
            let n = self.count(e)?;
            match args.get(j).copied() {
                Some(Some(t)) => work.push(Pair { syn: Syn::Ty(t), ty: e, base: off, ctx: Ctx::Adt(declared > 0) }),
                Some(None) if n == 0 => {}
                Some(None) => return Err(unaligned),
                None if n == 0 => {}
                None => return Err(LifeRefusal::DefaultWithPositions { at }),
            }
            off += n;
        }
        Ok(())
    }

    /// Lines up one input, output, field or self type: fills `regions[first.base..]`.
    fn line_up(&mut self, v: &View<'_>, first: Pair, mode: Elide, regions: &mut List<Region>) -> Result<(), LifeRefusal> {
        let mut work: List<Pair> = Vec::new_in(self.alloc);
        work.push(first);
        let mut steps = 0u32;
        while let Some(p) = work.pop() {
            steps += 1;
            if steps > MOST {
                return Err(LifeRefusal::Bound);
            }
            let n = self.count(p.ty)?;
            if n == 0 {
                self.left_unread(v, p.syn, mode)?;
                continue;
            }
            let t = match p.syn {
                Syn::Ty(t) => t,
                Syn::SelfTy => {
                    self.copy_self(p.base, n, Range::new(0, 0), regions)?;
                    continue;
                }
            };
            let at = site(v, t)?;
            let written = v.ty(t)?;
            if let lf::TypeRef::Path(path) = written
                && is_self_path(v, path)?
            {
                self.copy_self(p.base, n, at, regions)?;
                continue;
            }
            let node = self.node(p.ty)?;
            if matches!(node, PNode::Opaque(_, _) | PNode::Alias { .. }) {
                self.hidden += 1;
                regions[p.base as usize] = Region::Hidden(self.hidden - 1);
                self.left_unread(v, p.syn, mode)?;
                continue;
            }
            let unaligned = LifeRefusal::Unaligned { at };
            match (written, node) {
                (lf::TypeRef::Ref { lifetime, inner, .. }, PNode::Ref(_, pi)) => {
                    let r = self.site_region(v, lifetime, mode, at)?;
                    regions[p.base as usize] = r;
                    work.push(Pair { syn: Syn::Ty(inner), ty: pi, base: p.base + 1, ctx: Ctx::Ref(r) });
                }
                (lf::TypeRef::Ptr { inner, .. }, PNode::Ptr(_, pi)) | (lf::TypeRef::Slice(inner), PNode::Slice(pi)) => {
                    work.push(Pair { syn: Syn::Ty(inner), ty: pi, base: p.base, ctx: Ctx::Other });
                }
                (lf::TypeRef::Array { elem, .. }, PNode::Array(pi, _)) => {
                    work.push(Pair { syn: Syn::Ty(elem), ty: pi, base: p.base, ctx: Ctx::Other });
                }
                (lf::TypeRef::Pat { base, .. }, PNode::Pat(pi, _)) => {
                    work.push(Pair { syn: Syn::Ty(base), ty: pi, base: p.base, ctx: Ctx::Other });
                }
                (lf::TypeRef::Tuple(l), PNode::Tuple(pl)) | (lf::TypeRef::Union(l), PNode::Union(pl)) => {
                    let ids = v.ids(l)?;
                    let elems = self.list(pl)?;
                    if ids.len() != elems.len() {
                        return Err(unaligned);
                    }
                    let mut off = p.base;
                    for (j, &e) in elems.iter().enumerate() {
                        work.push(Pair { syn: Syn::Ty(lf::TypeId(ids[j])), ty: e, base: off, ctx: Ctx::Other });
                        off += self.count(e)?;
                    }
                }
                (lf::TypeRef::Dyn(bounds), PNode::Dynamic(_)) => {
                    let mut bound: Option<lf::NameIdx> = None;
                    for &row in v.rows(bounds)? {
                        if let lf::Bound::Lifetime(n) = lf::Bound::decode(row)? {
                            bound = Some(n);
                        }
                    }
                    // Whether its traits write or hide a lifetime: the default reads it, and so
                    // does the count of the lifetimes the parameters use.
                    let mut todo: List<lf::TypeId> = Vec::new_in(self.alloc);
                    let plain = self.quiet_bounds(v, bounds, true, &mut todo)? && self.quiet_all(v, todo)?;
                    let r = match bound {
                        Some(n) => self.site_region(v, Some(n), mode, at)?,
                        None => match p.ctx {
                            Ctx::Ref(r) => r,
                            Ctx::Adt(false) if plain => Region::Static,
                            _ => return Err(LifeRefusal::ObjectDefault { at }),
                        },
                    };
                    if mode == Elide::Input && !plain {
                        self.outside = true;
                    }
                    regions[p.base as usize] = r;
                }
                (lf::TypeRef::Path(path), PNode::Adt(k, l)) => self.adt_path(v, path, at, k, l, p, mode, regions, &mut work)?,
                _ => return Err(unaligned),
            }
        }
        Ok(())
    }

    /// Reserves `ty`'s positions at the end of `regions`; where they start.
    fn reserve(&mut self, ty: PTy, regions: &mut List<Region>) -> Result<(u32, u32), LifeRefusal> {
        let base = regions.len() as u32;
        let n = self.count(ty)?;
        regions.resize((base + n) as usize, HOLE);
        Ok((base, n))
    }
}

/// No position is left unfilled.
fn filled(regions: &List<Region>) -> Result<(), LifeRefusal> {
    if regions.iter().any(|r| *r == HOLE) {
        return Err(LifeRefusal::Unaligned { at: Range::new(0, 0) });
    }
    Ok(())
}

/// A fn's lifetime part: `v` its own text's view, `inputs` and `output` its head's erased types,
/// `container` its impl's or trait's part. `r` is new, with the fn's module set.
fn read_fn<'n, S: NamesSource<'n>>(r: &mut Reader<'_, 'n, S>,v: &View<'_>, inputs: &[PTy], output: PTy, container: Option<&ContainerLife>) -> Result<FnLife, LifeRefusal> {
    let alloc = r.alloc;
    let mut bounds: List<(Region, Region)> = Vec::new_in(alloc);
    let mut container_named = 0u32;
    if let Some(c) = container {
        r.scope.extend_from_slice(&c.names);
        container_named = c.lifetimes + c.elided;
        r.named = container_named;
        let mut own: List<Region> = Vec::new_in(alloc);
        own.extend_from_slice(&c.self_regions);
        r.self_regions = Some(own);
        bounds.extend_from_slice(&c.bounds);
    }
    r.declare(v)?;
    r.declare_params(v)?;
    r.declared_bounds(v, &mut bounds)?;
    let mut regions: List<Region> = Vec::new_in(alloc);
    let mut ends: List<u32> = Vec::new_in(alloc);
    let list = v.list(head::PARAMS);
    let rows = v.rows(list)?;
    if rows.len() != inputs.len() {
        return Err(LifeRefusal::ParamCount { text: rows.len() as u32, head: inputs.len() as u32 });
    }
    // The receiver's reference, when it is `&self`, `&mut self`, `self: &Self` or `self: &mut Self`.
    let mut receiver: Option<Region> = None;
    for (j, &row) in rows.iter().enumerate() {
        let prm = lf::Param::decode(row)?;
        let ty = inputs[j];
        let at = v.site(v.row_range(list.start + j as u32)?)?;
        let (base, _) = r.reserve(ty, &mut regions)?;
        match prm.self_kind {
            lf::SelfKind::Ref(_) => {
                let PNode::Ref(_, inner) = r.node(ty)? else { return Err(LifeRefusal::Unaligned { at }) };
                let region = r.site_region(v, prm.lifetime, Elide::Input, at)?;
                regions[base as usize] = region;
                if j == 0 {
                    receiver = Some(region);
                }
                r.line_up(v, Pair { syn: Syn::SelfTy, ty: inner, base: base + 1, ctx: Ctx::Ref(region) }, Elide::Input, &mut regions)?;
            }
            lf::SelfKind::Value => r.line_up(v, Pair { syn: Syn::SelfTy, ty, base, ctx: Ctx::Other }, Elide::Input, &mut regions)?,
            lf::SelfKind::No | lf::SelfKind::Typed => {
                let t = prm.ty.ok_or(LifeRefusal::Unaligned { at })?;
                let mut by_ref = false;
                if j == 0
                    && prm.self_kind == lf::SelfKind::Typed
                    && let lf::TypeRef::Ref { inner, .. } = v.ty(t)?
                    && let lf::TypeRef::Path(p) = v.ty(inner)?
                {
                    by_ref = is_self_path(v, p)?;
                }
                r.line_up(v, Pair { syn: Syn::Ty(t), ty, base, ctx: Ctx::Other }, Elide::Input, &mut regions)?;
                if by_ref {
                    receiver = regions.get(base as usize).copied();
                }
            }
        }
        ends.push(regions.len() as u32);
    }
    // The Reference, "Lifetime elision": the receiver's reference first, else the one lifetime
    // the parameters use.
    let decided = match receiver {
        Some(region) => Some(region),
        None if r.outside => None,
        None => match (r.used.first(), r.used.len()) {
            (Some(&one), 1) => Some(one),
            _ => None,
        },
    };
    let (base, n) = r.reserve(output, &mut regions)?;
    if v.head[head::FLAGS] & sig::ASYNC != 0 {
        // The head's output is the fn's opaque future: one position, which says no lifetime.
        if n != 1 {
            return Err(LifeRefusal::Unaligned { at: Range::new(0, 0) });
        }
        r.hidden += 1;
        regions[base as usize] = Region::Hidden(r.hidden - 1);
    } else {
        match v.head_opt(head::TY) {
            Some(t) => r.line_up(v, Pair { syn: Syn::Ty(lf::TypeId(t)), ty: output, base, ctx: Ctx::Other }, Elide::Output(decided), &mut regions)?,
            None if n == 0 => {}
            None => return Err(LifeRefusal::Unaligned { at: Range::new(0, 0) }),
        }
    }
    filled(&regions)?;
    Ok(FnLife { regions, ends, bounds, container: container_named, named: r.named })
}

/// An impl's (`self_ty` its head's self type) or a trait's (`None`) lifetime part, from its
/// text's view. `r` is new, with the container's module set.
fn read_container<'n, S: NamesSource<'n>>(r: &mut Reader<'_, 'n, S>,v: &View<'_>, self_ty: Option<PTy>) -> Result<ContainerLife, LifeRefusal> {
    let alloc = r.alloc;
    let lifetimes = r.declare(v)?;
    r.declare_params(v)?;
    let mut bounds: List<(Region, Region)> = Vec::new_in(alloc);
    r.declared_bounds(v, &mut bounds)?;
    let mut self_regions: List<Region> = Vec::new_in(alloc);
    if let Some(ty) = self_ty {
        let (base, _) = r.reserve(ty, &mut self_regions)?;
        let t = lf::TypeId(v.head[head::SELF_TY]);
        r.line_up(v, Pair { syn: Syn::Ty(t), ty, base, ctx: Ctx::Other }, Elide::Header, &mut self_regions)?;
        filled(&self_regions)?;
    }
    // The declared names only: an elided lifetime of the header has none.
    let mut names: List<(u64, u32)> = Vec::new_in(alloc);
    names.extend_from_slice(&r.scope);
    Ok(ContainerLife { lifetimes, elided: r.named - lifetimes, self_regions, bounds, names })
}

/// The variances of an ADT's `n` lifetime parameters from its fields' written types (the
/// module's "VARIANCE": the Reference's table decides where every field is plain).
fn variances<'n, S: NamesSource<'n>>(r: &mut Reader<'_, 'n, S>,v: &View<'_>, fields: &[lf::TypeId], n: u32) -> Result<List<LifeVariance>, LifeRefusal> {
    let alloc = r.alloc;
    // Per parameter: used in a covariant position, in a contravariant one, in an invariant one.
    let mut seen: List<(bool, bool, bool)> = Vec::new_in(alloc);
    seen.resize(n as usize, (false, false, false));
    let mut plain = true;
    // The names a `for<..>` binder of a field's fn pointer binds (lane B15): a lifetime bound
    // there is the pointer's, no parameter of the ADT (the Reference, "Function pointer types",
    // "Higher-ranked trait bounds"). Shadowing a parameter's name in a binder is E0496, so one
    // list for the whole walk says the same as a scoped one for every program rustc accepts.
    let mut bound: List<u64> = Vec::new_in(alloc);
    // Each type with the variance it stands at (the Reference's table, composed: `&mut T` and
    // `*mut T` invariant in `T`, `fn(A) -> R` contravariant in `A`).
    let mut todo: List<(lf::TypeId, Variance)> = Vec::new_in(alloc);
    for &f in fields {
        todo.push((f, Variance::Co));
    }
    let under = |at: Variance, inner: Variance| match (at, inner) {
        (Variance::In, _) | (_, Variance::In) => Variance::In,
        (Variance::Co, x) => x,
        (Variance::Contra, Variance::Co) => Variance::Contra,
        (Variance::Contra, Variance::Contra) => Variance::Co,
    };
    let of_mut = |m: tc::Mutbl| if m == tc::Mutbl::Mut { Variance::In } else { Variance::Co };
    let mut steps = 0u32;
    while let Some((t, at)) = todo.pop() {
        steps += 1;
        if steps > MOST {
            return Err(LifeRefusal::Bound);
        }
        match v.ty(t)? {
            lf::TypeRef::Never => {}
            lf::TypeRef::Ref { mutbl, lifetime, inner } => {
                if let Some(name) = lifetime
                    && !bound.contains(&v.name(name)?)
                    && let Some(Region::Named(i)) = r.written(v, name)?
                    && let Some(s) = seen.get_mut(i as usize)
                {
                    match at {
                        Variance::Co => s.0 = true,
                        Variance::Contra => s.1 = true,
                        Variance::In => s.2 = true,
                    }
                }
                todo.push((inner, under(at, of_mut(mutbl))));
            }
            lf::TypeRef::Ptr { mutbl, inner } => todo.push((inner, under(at, of_mut(mutbl)))),
            lf::TypeRef::Slice(inner) => todo.push((inner, at)),
            lf::TypeRef::Array { elem, .. } => todo.push((elem, at)),
            lf::TypeRef::Tuple(l) => {
                for &i in v.ids(l)? {
                    todo.push((lf::TypeId(i), at));
                }
            }
            // `fn(A) -> R` (lane B15): its inputs contravariant, its output covariant. A
            // lifetime its binder binds is no parameter's (`bound`); an elided one neither.
            lf::TypeRef::FnPtr { params, ret, .. } => {
                for &row in v.rows(params)? {
                    let p = lf::Param::decode(row)?;
                    if p.variadic {
                        continue;
                    }
                    match p.ty {
                        Some(ty) => todo.push((ty, under(at, Variance::Contra))),
                        None => plain = false,
                    }
                }
                if let Some(ret) = ret {
                    todo.push((ret, at));
                }
            }
            lf::TypeRef::For { binder, inner } => {
                for &row in v.rows(binder)? {
                    match lf::GenericParam::decode(row)? {
                        lf::GenericParam::Lifetime { name, .. } => bound.push(v.name(name)?),
                        _ => plain = false,
                    }
                }
                todo.push((inner, at));
            }
            lf::TypeRef::Path(p) => {
                // One name with no arguments that is a type parameter or a built-in type.
                let quiet = match path_texts(v, p) {
                    Ok((false, texts, segs, _)) if texts.len() == 1 && !segs[0].has_args && segs[0].kind == lf::SegKind::Ident => {
                        r.params.iter().any(|n| *n == Name::of(texts[0]))
                            || matches!((Paths { names: r.names }).path(r.module, false, &texts, Ns::Type), Ok(found) if matches!(found.kind, Kind::Builtin(_)))
                    }
                    _ => false,
                };
                plain &= quiet;
            }
            _ => plain = false,
        }
    }
    let mut out: List<LifeVariance> = Vec::new_in(alloc);
    for &(co, contra, inv) in seen.iter() {
        out.push(match (plain, co, contra, inv) {
            (true, true, false, false) => LifeVariance { variance: Variance::Co, computed: true },
            (true, false, true, false) => LifeVariance { variance: Variance::Contra, computed: true },
            // Used at two variances, or at an invariant one: invariant.
            (true, true, true, _) | (true, _, _, true) => LifeVariance { variance: Variance::In, computed: true },
            // Not plain, or a parameter no field uses (an error of the Reference's "Generic
            // parameters"; nothing to compute from): the sound over-approximation.
            _ => LifeVariance { variance: Variance::In, computed: false },
        });
    }
    Ok(out)
}

/// An ADT's lifetime part: `v` its text's view, `field_tys` its head's fields' erased types in
/// the head's order, `is_enum` whether its rows are variants. `r` is new, with its module set.
fn read_adt<'n, S: NamesSource<'n>>(r: &mut Reader<'_, 'n, S>,v: &View<'_>, field_tys: &[PTy], is_enum: bool) -> Result<AdtLife, LifeRefusal> {
    let alloc = r.alloc;
    let lifetimes = r.declare(v)?;
    r.declare_params(v)?;
    let mut bounds: List<(Region, Region)> = Vec::new_in(alloc);
    r.declared_bounds(v, &mut bounds)?;
    // `Self` in a field is the ADT over its own parameters.
    let mut own: List<Region> = Vec::new_in(alloc);
    let identity = match GRAIN {
        Grain::PerParam => lifetimes,
        Grain::OnePerAdt => lifetimes.min(1),
    };
    for i in 0..identity {
        own.push(Region::Named(i));
    }
    // Under one position per ADT, two lifetime parameters have no one region: `Self` is refused.
    r.self_regions = if GRAIN == Grain::OnePerAdt && lifetimes > 1 { None } else { Some(own) };
    let mut written: List<lf::TypeId> = Vec::new_in(alloc);
    let rows = v.rows(v.list(head::FIELDS))?;
    if is_enum {
        for &row in rows {
            for &f in v.rows(lf::Variant::decode(row)?.fields)? {
                written.push(lf::Field::decode(f)?.ty);
            }
        }
    } else {
        for &f in rows {
            written.push(lf::Field::decode(f)?.ty);
        }
    }
    if written.len() != field_tys.len() {
        return Err(LifeRefusal::FieldCount { text: written.len() as u32, head: field_tys.len() as u32 });
    }
    let mut fields: List<Region> = Vec::new_in(alloc);
    let mut field_ends: List<u32> = Vec::new_in(alloc);
    for (j, &ty) in field_tys.iter().enumerate() {
        let (base, _) = r.reserve(ty, &mut fields)?;
        r.line_up(v, Pair { syn: Syn::Ty(written[j]), ty, base, ctx: Ctx::Other }, Elide::Field, &mut fields)?;
        field_ends.push(fields.len() as u32);
    }
    filled(&fields)?;
    let variances = variances(r, v, &written, lifetimes)?;
    Ok(AdtLife { lifetimes, variances, bounds, fields, field_ends })
}

impl<'n> Store<'n> {
    /// The lifetime part of the fn `key`, whose head is made (`Store::fns`); made on first
    /// asking and kept, a refusal too.
    pub fn fn_life<S: NamesSource<'n>>(&mut self, names: S, keys: &Keys, key: ItemKey) -> Result<&FnLife, LifeRefusal> {
        if !self.lives.fns.contains_key(&key) {
            let made = self.make_fn_life(names, keys, key);
            self.lives.fns.insert(key, made);
        }
        match self.lives.fns.get(&key) {
            Some(Ok(life)) => Ok(life),
            Some(Err(why)) => Err(*why),
            None => Err(LifeRefusal::NoHead),
        }
    }

    /// The lifetime part of the impl or trait `lib`, whose head is made.
    pub fn container_life<S: NamesSource<'n>>(&mut self, names: S, keys: &Keys, lib: LibKey) -> Result<&ContainerLife, LifeRefusal> {
        let ck = checker_of(lib);
        if !self.lives.containers.contains_key(&ck) {
            let made = self.make_container_life(names, keys, lib);
            self.lives.containers.insert(ck, made);
        }
        match self.lives.containers.get(&ck) {
            Some(Ok(life)) => Ok(life),
            Some(Err(why)) => Err(*why),
            None => Err(LifeRefusal::NoHead),
        }
    }

    /// The lifetime part of the struct, enum or union `lib`, whose row is made (`Want::Adt`).
    pub fn adt_life<S: NamesSource<'n>>(&mut self, names: S, keys: &Keys, lib: LibKey) -> Result<&AdtLife, LifeRefusal> {
        if !self.lives.adts.contains_key(&lib) {
            let made = self.make_adt_life(names, keys, lib);
            self.lives.adts.insert(lib, made);
        }
        match self.lives.adts.get(&lib) {
            Some(Ok(life)) => Ok(life),
            Some(Err(why)) => Err(*why),
            None => Err(LifeRefusal::NoHead),
        }
    }

    /// Every lifetime part the heads at `heads` have, made now (a refusal is made too): a fn's,
    /// an ADT's, a trait's and an impl's, so a head record of them holds its part
    /// ([`Store::lives_of`]). An impl whose library key the key map cannot tell is refused
    /// (`NoHead`), by name in its record.
    pub fn make_lives<S: NamesSource<'n>>(&mut self, names: S, keys: &Keys, heads: &[HeadAt]) {
        for at in heads.iter().copied() {
            match at {
                HeadAt::Fn(k) => {
                    let _ = self.fn_life(names, keys, k);
                }
                HeadAt::Adt(lib) => {
                    let _ = self.adt_life(names, keys, lib);
                }
                HeadAt::Trait(lib) => {
                    let _ = self.container_life(names, keys, lib);
                }
                HeadAt::Impl(k) => {
                    match keys.library_in(names, k) {
                        Some(lib) => {
                            let _ = self.container_life(names, keys, lib);
                        }
                        None => {
                            self.lives.containers.insert(k, Err(LifeRefusal::NoHead));
                        }
                    }
                }
                HeadAt::Ctor(_) | HeadAt::Alias(_) | HeadAt::TraitAlias(_) | HeadAt::Opaque(_) => {}
            }
        }
    }

    /// The lifetime part made for the head at `at`, read without making it: `None` when it was
    /// never asked ([`Store::make_lives`]) or the head has none (a constructor's is its ADT's;
    /// an alias's, a trait alias's and an opaque's are refused where a fn names them).
    pub fn lives_of(&self, at: HeadAt) -> Option<LifeOf<'_>> {
        match at {
            HeadAt::Fn(k) => self.lives.fns.get(&k).map(|r| LifeOf::Fn(r.as_ref().map_err(|w| *w))),
            HeadAt::Adt(lib) => self.lives.adts.get(&lib).map(|r| LifeOf::Adt(r.as_ref().map_err(|w| *w))),
            HeadAt::Trait(lib) => self.lives.containers.get(&checker_of(lib)).map(|r| LifeOf::Container(r.as_ref().map_err(|w| *w))),
            HeadAt::Impl(k) => self.lives.containers.get(&k).map(|r| LifeOf::Container(r.as_ref().map_err(|w| *w))),
            HeadAt::Ctor(_) | HeadAt::Alias(_) | HeadAt::TraitAlias(_) | HeadAt::Opaque(_) => None,
        }
    }

    /// Files fn `key`'s lifetime part read from a head record (heads.rs `Store::file_heads`):
    /// kept as if made here. One already made here is the same item's from the same text.
    pub fn file_fn_life(&self, key: ItemKey, life: Result<FnLife, LifeRefusal>) {
        self.lives.fns.insert_absent(key, life);
    }

    /// [`Store::file_fn_life`] for the ADT `lib`.
    pub fn file_adt_life(&self, lib: LibKey, life: Result<AdtLife, LifeRefusal>) {
        self.lives.adts.insert_absent(lib, life);
    }

    /// [`Store::file_fn_life`] for the trait or impl whose checker key is `key`.
    pub fn file_container_life(&self, key: ItemKey, life: Result<ContainerLife, LifeRefusal>) {
        self.lives.containers.insert_absent(key, life);
    }

    fn make_fn_life<S: NamesSource<'n>>(&mut self, names: S, keys: &Keys, key: ItemKey) -> Result<FnLife, LifeRefusal> {
        // A member's `lib` is its container's key (`Store::impl_`, `Store::trait_members`).
        let (lib, has_container) = {
            let h = self.fns.get(&key).ok_or(LifeRefusal::NoHead)?;
            (h.lib, h.container.is_some())
        };
        if has_container {
            self.container_life(names, keys, lib)?;
        }
        let Store { tys, fns, inferred_outputs, lives, locals, .. } = self;
        let Lives { counts, containers, alloc, .. } = lives;
        let h = fns.get(&key).ok_or(LifeRefusal::NoHead)?;
        let h_output = inferred_outputs.get(&key).copied().unwrap_or(h.output);
        let container = if has_container {
            match containers.get(&checker_of(lib)) {
                Some(Ok(c)) => Some(c),
                Some(Err(why)) => return Err(*why),
                None => return Err(LifeRefusal::NoHead),
            }
        } else {
            None
        };
        let low = lower(names, lib, h.text).map_err(|_| LifeRefusal::Text)?;
        let mut r = Reader::new(names, keys, locals, tys.view(), counts, h.module, *alloc);
        // The container's type parameters shadow items too: every name the head was read with.
        for &(name, _) in h.params.iter() {
            r.params.push(name);
        }
        read_fn(&mut r, &low.view(), &h.inputs[..], h_output, container)
    }

    fn make_container_life<S: NamesSource<'n>>(&mut self, names: S, keys: &Keys, lib: LibKey) -> Result<ContainerLife, LifeRefusal> {
        let self_ty = match self.impls.get(&checker_of(lib)) {
            Some(i) => Some(i.self_ty),
            None if self.maps.traits.contains_key(&lib) => None,
            None => return Err(LifeRefusal::NoHead),
        };
        let Store { tys, lives, locals, .. } = self;
        let Lives { counts, alloc, .. } = lives;
        let (text, module) = item_text(names, locals, lib)?;
        let low = lower(names, lib, text).map_err(|_| LifeRefusal::Text)?;
        let mut r = Reader::new(names, keys, locals, tys.view(), counts, module, *alloc);
        read_container(&mut r, &low.view(), self_ty)
    }

    fn make_adt_life<S: NamesSource<'n>>(&mut self, names: S, keys: &Keys, lib: LibKey) -> Result<AdtLife, LifeRefusal> {
        let Store { tys, adts, lives, locals, .. } = self;
        let Lives { counts, alloc, .. } = lives;
        let row = adts.get(&checker_of(lib)).ok_or(LifeRefusal::NoHead)?;
        let mut field_tys: List<PTy> = Vec::new_in(*alloc);
        for f in row.fields.iter() {
            field_tys.push(f.ty);
        }
        let (text, module) = item_text(names, locals, lib)?;
        let low = lower(names, lib, text).map_err(|_| LifeRefusal::Text)?;
        let mut r = Reader::new(names, keys, locals, tys.view(), counts, module, *alloc);
        read_adt(&mut r, &low.view(), &field_tys, row.kind == tc::AdtKind::Enum)
    }
}
