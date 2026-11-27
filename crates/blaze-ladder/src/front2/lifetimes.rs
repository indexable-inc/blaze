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
use super::heads::{Store, with_lowered};
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

    pub fn clear(&mut self) {
        let Lives { fns, adts, containers, counts, alloc: _ } = self;
        fns.clear();
        adts.clear();
        containers.clear();
        counts.clear();
        if counts.capacity() > 256 {
            counts.shrink_to(64);
        }
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

/// What lifetime bound a containing ADT declares on its `j`-th type parameter (`T: 'a`),
/// substituted with the ADT's lifetime arguments at the use site.
#[derive(Clone, Copy)]
enum AdtParamBound {
    /// The containing ADT declares no lifetime bound on this type parameter.
    None,
    /// The containing ADT declares a unique lifetime bound (or `'static` when any bound is `'static`).
    Unique(Region),
    /// Multiple distinct non-`'static` lifetime bounds (or unreadable bounds) -> refuse `ObjectDefault`.
    Ambiguous,
}

/// What encloses a type: a trait object's default lifetime reads it.
#[derive(Clone, Copy)]
enum Ctx {
    Ref(Region),
    /// A type argument of an ADT, carrying the containing parameter's substituted lifetime bound.
    Adt(AdtParamBound),
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
    /// The module the item's paths are resolved in, its token marks (for macro-expanded
    /// `$crate` / def-site paths), and its type and const parameters' names (a parameter shadows
    /// an item of its name).
    module: LibKey,
    marks: Option<blaze_scope::crate_roots::TextMarks<'n>>,
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
    #[expect(clippy::too_many_arguments, reason = "reader state for one item head: names, keys, locals, types, counts, module, marks, alloc")]
    fn new(
        names: S,
        keys: &'c Keys,
        locals: &'c Locals<'n>,
        tys: PTys<'c>,
        counts: &'c mut Map<LibKey, u32>,
        module: LibKey,
        marks: Option<blaze_scope::crate_roots::TextMarks<'n>>,
        alloc: Mem,
    ) -> Reader<'c, 'n, S> {
        Reader {
            names,
            keys,
            locals,
            tys,
            counts,
            module,
            marks,
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

    fn resolve_path(&self, v: &View<'_>, p: tc::PathIdx, abs: bool, texts: &[&str], at: Range) -> Result<crate::resolve::Target, LifeRefusal> {
        super::item_site::resolve_written(&Paths { names: self.names }, self.marks, self.module, v, p, abs, texts, Ns::Type, at)
            .map_err(|_| LifeRefusal::Unresolved { at })
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
        let n = with_lowered(self.names, lib, text, |v| declared_lifetimes(v))
            .map_err(|_| LifeRefusal::Text)??;
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
        self.quiet_path_bound(v, p, &[], todo, 0)
    }

    /// [`Self::quiet_path`] with `bound` holding lifetime name hashes bound by an enclosing
    /// `for<..>` binder: written lifetimes in `bound` do not count as free lifetimes, and
    /// parenthesized `Fn(A) -> B` sugar is read with its inputs and optional output pushed.
    fn quiet_path_bound(&mut self, v: &View<'_>, p: tc::PathIdx, bound: &[u64], todo: &mut List<lf::TypeId>, alias_depth: u8) -> Result<bool, LifeRefusal> {
        let Ok((abs, texts, segs, at)) = super::types::path_texts_of(v, p, true) else { return Ok(false) };
        let mut last_bound_lts = 0u32;
        let mut written_pos_args = 0usize;
        for (i, s) in segs.iter().enumerate() {
            if s.kind == lf::SegKind::Anchor {
                return Ok(false);
            }
            if !s.has_args {
                continue;
            }
            let is_last = i + 1 == segs.len();
            if is_last && s.parenthesized {
                written_pos_args = 1;
            }
            for &row in v.rows(s.args)? {
                match lf::GenericArg::decode(row)? {
                    lf::GenericArg::Type(t) => {
                        if is_last && !s.parenthesized {
                            written_pos_args += 1;
                        }
                        todo.push(t);
                    }
                    lf::GenericArg::Const(_) => {
                        if is_last && !s.parenthesized {
                            written_pos_args += 1;
                        }
                    }
                    lf::GenericArg::AssocType { args, ty, .. } if v.rows(args)?.is_empty() => todo.push(ty),
                    lf::GenericArg::Lifetime(nm) if is_last && bound.contains(&v.name(nm)?) => {
                        last_bound_lts = last_bound_lts.saturating_add(1);
                    }
                    _ => return Ok(false),
                }
            }
            if s.parenthesized
                && let Some(ret) = s.ty
            {
                todo.push(ret);
            }
        }
        let Some(first) = texts.first().copied() else { return Ok(false) };
        if !abs && first == "Self" {
            return Ok(true);
        }
        if !abs && texts.len() <= 2 && self.params.iter().any(|n| *n == Name::of(first)) {
            return Ok(true);
        }
        let Ok(found) = self.resolve_path(v, p, abs, &texts, at) else { return Ok(false) };
        if usize::try_from(found.consumed).ok() != Some(texts.len()) {
            return Ok(false);
        }
        match found.kind {
            Kind::Builtin(_) => Ok(true),
            Kind::Struct | Kind::Enum | Kind::Union | Kind::Trait => {
                let lts_ok = match self.lifetimes_of(found.key) {
                    Ok(0) => last_bound_lts == 0,
                    Ok(n) => last_bound_lts == n,
                    Err(_) => false,
                };
                if !lts_ok {
                    return Ok(false);
                }
                self.quiet_omitted_defaults(found.key, written_pos_args, alias_depth)
            }
            Kind::TraitAlias => self.quiet_trait_alias(found.key, last_bound_lts, written_pos_args, alias_depth),
            _ => Ok(false),
        }
    }

    /// Whether every omitted defaulted type parameter (`pos >= written_pos_args`) of `item_key`
    /// writes no free or hidden lifetime (with `item_key`'s own lifetime parameters treated as
    /// already verified quiet/bound by the caller).
    fn quiet_omitted_defaults(&mut self, item_key: LibKey, written_pos_args: usize, depth: u8) -> Result<bool, LifeRefusal> {
        const MOST_DEFAULT_DEPTH: u8 = 8;
        if depth >= MOST_DEFAULT_DEPTH {
            return Ok(false);
        }
        let Ok((text, item_module)) = item_text(self.names, self.locals, item_key) else {
            return Ok(false);
        };
        let item_marks = if let Some(local) = self.locals.head(checker_of(item_key)) {
            local.marks
        } else {
            super::item_site::item_marks(self.names, item_key, text).ok().flatten()
        };
        let alloc = self.alloc;
        let saved_module = self.module;
        let saved_marks = self.marks;
        let mut saved_params: List<Name> = Vec::with_capacity_in(self.params.len(), alloc);
        saved_params.extend_from_slice(&self.params);
        self.module = item_module;
        self.marks = item_marks;
        self.params.clear();
        let names = self.names;
        let res = with_lowered(names, item_key, text, |av| -> Result<bool, LifeRefusal> {
            let mut lt_names: List<u64> = Vec::new_in(alloc);
            let mut todo: List<lf::TypeId> = Vec::new_in(alloc);
            let mut pos = 0usize;
            for &row in av.rows(av.list(head::GENERICS))? {
                match lf::GenericParam::decode(row)? {
                    lf::GenericParam::Lifetime { name, .. } => lt_names.push(av.name(name)?),
                    lf::GenericParam::Type { name, default, .. } => {
                        if pos >= written_pos_args
                            && let Some(def_ty) = default
                        {
                            todo.push(def_ty);
                        }
                        self.params.push(Name(av.name(name)?));
                        pos += 1;
                    }
                    lf::GenericParam::Const { name, .. } => {
                        self.params.push(Name(av.name(name)?));
                        pos += 1;
                    }
                }
            }
            if todo.is_empty() {
                return Ok(true);
            }
            self.quiet_all_bound(av, &lt_names, todo, depth + 1)
        });
        self.module = saved_module;
        self.marks = saved_marks;
        self.params.clear();
        self.params.extend_from_slice(&saved_params);
        match res {
            Ok(inner) => inner,
            Err(_) => Ok(false),
        }
    }

    /// Whether trait alias `alias_key` (with `last_bound_lts` enclosing `for<..>` lifetimes and
    /// `written_pos_args` positional arguments written on its use) introduces no free or hidden
    /// lifetimes in its expanded bounds or omitted type defaults.
    fn quiet_trait_alias(&mut self, alias_key: LibKey, last_bound_lts: u32, written_pos_args: usize, depth: u8) -> Result<bool, LifeRefusal> {
        const MOST_ALIAS_DEPTH: u8 = 8;
        if depth >= MOST_ALIAS_DEPTH {
            return Ok(false);
        }
        let Ok((text, alias_module)) = item_text(self.names, self.locals, alias_key) else {
            return Ok(false);
        };
        let alias_marks = if let Some(local) = self.locals.head(checker_of(alias_key)) {
            local.marks
        } else {
            super::item_site::item_marks(self.names, alias_key, text).ok().flatten()
        };
        let saved_module = self.module;
        let saved_marks = self.marks;
        let saved_params_len = self.params.len();
        let alloc = self.alloc;
        let mut saved_params: List<Name> = Vec::with_capacity_in(saved_params_len, alloc);
        saved_params.extend_from_slice(&self.params);
        self.module = alias_module;
        self.marks = alias_marks;
        self.params.clear();
        let res = with_lowered(self.names, alias_key, text, |av| -> Result<bool, LifeRefusal> {
            let mut lt_names: List<u64> = Vec::new_in(alloc);
            let mut todo: List<lf::TypeId> = Vec::new_in(alloc);
            let mut pos = 0usize;
            for &row in av.rows(av.list(head::GENERICS))? {
                match lf::GenericParam::decode(row)? {
                    lf::GenericParam::Lifetime { name, .. } => lt_names.push(av.name(name)?),
                    lf::GenericParam::Type { name, default, .. } => {
                        if pos >= written_pos_args
                            && let Some(def_ty) = default
                        {
                            todo.push(def_ty);
                        }
                        self.params.push(Name(av.name(name)?));
                        pos += 1;
                    }
                    lf::GenericParam::Const { name, .. } => {
                        self.params.push(Name(av.name(name)?));
                        pos += 1;
                    }
                }
            }
            if u32::try_from(lt_names.len()).ok() != Some(last_bound_lts) {
                return Ok(false);
            }
            if !self.quiet_bounds_bound(av, av.list(head::BOUNDS), false, &lt_names, &mut todo, depth + 1)? {
                return Ok(false);
            }
            self.quiet_all_bound(av, &lt_names, todo, depth + 1)
        });
        self.module = saved_module;
        self.marks = saved_marks;
        self.params.clear();
        self.params.extend_from_slice(&saved_params);
        match res {
            Ok(inner) => inner,
            Err(_) => Ok(false),
        }
    }

    /// Whether the bounds `bounds` (a `dyn`'s or an `impl`'s) write no lifetime and hide none,
    /// a lifetime bound itself allowed or not.
    fn quiet_bounds(&mut self, v: &View<'_>, bounds: tc::List, lifetime_ok: bool, todo: &mut List<lf::TypeId>) -> Result<bool, LifeRefusal> {
        self.quiet_bounds_bound(v, bounds, lifetime_ok, &[], todo, 0)
    }

    /// [`Self::quiet_bounds`] carrying `bound` lifetime hashes from enclosing `for<..>` binders.
    fn quiet_bounds_bound(
        &mut self,
        v: &View<'_>,
        bounds: tc::List,
        lifetime_ok: bool,
        bound: &[u64],
        todo: &mut List<lf::TypeId>,
        alias_depth: u8,
    ) -> Result<bool, LifeRefusal> {
        for &row in v.rows(bounds)? {
            match lf::Bound::decode(row)? {
                lf::Bound::Trait { ty, binder, .. } => {
                    let binder_rows = v.rows(binder)?;
                    let lf::TypeRef::Path(p) = v.ty(ty)? else { return Ok(false) };
                    if binder_rows.is_empty() {
                        if !self.quiet_path_bound(v, p, bound, todo, alias_depth)? {
                            return Ok(false);
                        }
                    } else {
                        let mut nested: List<u64> = Vec::with_capacity_in(bound.len() + binder_rows.len(), self.alloc);
                        nested.extend_from_slice(bound);
                        for &brow in binder_rows {
                            if let lf::GenericParam::Lifetime { name, .. } = lf::GenericParam::decode(brow)? {
                                nested.push(v.name(name)?);
                            }
                        }
                        let mut sub_todo: List<lf::TypeId> = Vec::new_in(self.alloc);
                        if !self.quiet_path_bound(v, p, &nested, &mut sub_todo, alias_depth)? {
                            return Ok(false);
                        }
                        if !self.quiet_all_bound(v, &nested, sub_todo, alias_depth)? {
                            return Ok(false);
                        }
                    }
                }
                lf::Bound::Lifetime(_) if lifetime_ok => {}
                _ => return Ok(false),
            }
        }
        Ok(true)
    }

    /// Whether every type in `todo`, and all they are made of, writes no lifetime and hides none.
    fn quiet_all(&mut self, v: &View<'_>, todo: List<lf::TypeId>) -> Result<bool, LifeRefusal> {
        self.quiet_all_bound(v, &[], todo, 0)
    }

    /// [`Self::quiet_all`] carrying `bound` lifetime hashes from enclosing `for<..>` binders so
    /// bound lifetimes nested inside type arguments (e.g. `for<'a> Tr<&'a u8>`) are recognized as
    /// bound rather than free input lifetimes.
    fn quiet_all_bound(&mut self, v: &View<'_>, bound: &[u64], todo: List<lf::TypeId>, alias_depth: u8) -> Result<bool, LifeRefusal> {
        let mut bound_stack: List<u64> = Vec::with_capacity_in(bound.len(), self.alloc);
        bound_stack.extend_from_slice(bound);
        let mut work: List<QuietWork> = Vec::with_capacity_in(todo.len(), self.alloc);
        for &t in todo.iter() {
            work.push(QuietWork::Ty(t));
        }
        let mut sub_todo: List<lf::TypeId> = Vec::new_in(self.alloc);
        let mut cur_depth = alias_depth;
        let mut steps = 0u32;
        while let Some(item) = work.pop() {
            let t = match item {
                QuietWork::PopBound { prev_len, prev_depth } => {
                    bound_stack.truncate(prev_len);
                    cur_depth = prev_depth;
                    continue;
                }
                QuietWork::Ty(t) => t,
            };
            steps += 1;
            if steps > MOST {
                return Err(LifeRefusal::Bound);
            }
            match v.ty(t)? {
                lf::TypeRef::Never | lf::TypeRef::Infer => {}
                lf::TypeRef::Ref { lifetime: Some(nm), inner, .. } if bound_stack.contains(&v.name(nm)?) => {
                    work.push(QuietWork::Ty(inner));
                }
                lf::TypeRef::Ptr { inner, .. } | lf::TypeRef::Slice(inner) => work.push(QuietWork::Ty(inner)),
                lf::TypeRef::Array { elem, .. } => work.push(QuietWork::Ty(elem)),
                lf::TypeRef::Pat { base, .. } => work.push(QuietWork::Ty(base)),
                lf::TypeRef::Tuple(l) | lf::TypeRef::Union(l) => {
                    for &i in v.ids(l)? {
                        work.push(QuietWork::Ty(lf::TypeId(i)));
                    }
                }
                lf::TypeRef::Path(p) => {
                    sub_todo.clear();
                    if !self.quiet_path_bound(v, p, &bound_stack, &mut sub_todo, cur_depth)? {
                        return Ok(false);
                    }
                    for &st in sub_todo.iter() {
                        work.push(QuietWork::Ty(st));
                    }
                }
                lf::TypeRef::Dyn(b) | lf::TypeRef::Impl(b) => {
                    sub_todo.clear();
                    if !self.quiet_bounds_bound(v, b, false, &bound_stack, &mut sub_todo, cur_depth)? {
                        return Ok(false);
                    }
                    for &st in sub_todo.iter() {
                        work.push(QuietWork::Ty(st));
                    }
                }
                lf::TypeRef::For { binder, inner } if cur_depth < 8 => {
                    let prev_len = bound_stack.len();
                    let prev_depth = cur_depth;
                    for &brow in v.rows(binder)? {
                        if let lf::GenericParam::Lifetime { name, .. } = lf::GenericParam::decode(brow)? {
                            bound_stack.push(v.name(name)?);
                        }
                    }
                    cur_depth = cur_depth.saturating_add(1);
                    work.push(QuietWork::PopBound { prev_len, prev_depth });
                    work.push(QuietWork::Ty(inner));
                }
                // A reference with an unbound/elided lifetime, a fn pointer, or a macro call: a
                // lifetime is or may be written.
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
        let found = self.resolve_path(v, p, abs, &texts, at)?;
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
                Some(Some(t)) => {
                    let bound = if n == 0 {
                        AdtParamBound::None
                    } else {
                        self.adt_param_bound(found.key, j, &per)?
                    };
                    work.push(Pair { syn: Syn::Ty(t), ty: e, base: off, ctx: Ctx::Adt(bound) });
                }
                Some(None) if n == 0 => {}
                Some(None) => return Err(unaligned),
                None if n == 0 => {}
                None => return Err(LifeRefusal::DefaultWithPositions { at }),
            }
            off += n;
        }
        Ok(())
    }

    /// Inspects `adt_key`'s declared bounds on its `param_idx`-th type parameter (both inline
    /// `T: 'a` bounds and `where T: 'a` clauses), substituting `per` for the ADT's declared
    /// lifetimes, to compute the containing-parameter object-lifetime default (the Reference,
    /// "Default trait object lifetimes").
    fn adt_param_bound(&self, adt_key: LibKey, param_idx: usize, per: &[Region]) -> Result<AdtParamBound, LifeRefusal> {
        let Ok((text, _)) = item_text(self.names, self.locals, adt_key) else {
            return Ok(AdtParamBound::Ambiguous);
        };
        let alloc = self.alloc;
        let names = self.names;
        let lowered = with_lowered(names, adt_key, text, |av| -> Result<AdtParamBound, LifeRefusal> {
            let mut lt_names: List<u64> = Vec::new_in(alloc);
            let mut target_name: Option<u64> = None;
            let mut bound_lists: List<tc::List> = Vec::new_in(alloc);
            let mut pos = 0usize;
            for &row in av.rows(av.list(head::GENERICS))? {
                match lf::GenericParam::decode(row)? {
                    lf::GenericParam::Lifetime { name, .. } => lt_names.push(av.name(name)?),
                    lf::GenericParam::Type { name, bounds, .. } => {
                        if pos == param_idx {
                            target_name = Some(av.name(name)?);
                            bound_lists.push(bounds);
                        }
                        pos += 1;
                    }
                    lf::GenericParam::Const { .. } => {
                        pos += 1;
                    }
                }
            }
            let Some(param_hash) = target_name else {
                return Ok(AdtParamBound::None);
            };
            if lt_names.len() != per.len() {
                return Ok(AdtParamBound::Ambiguous);
            }
            for &row in av.rows(av.list(head::WHERE))? {
                if let lf::WherePred::Type { ty, bounds, .. } = lf::WherePred::decode(row)?
                    && let lf::TypeRef::Path(sp) = av.ty(ty)?
                {
                    let segs = av.segments(av.path(sp)?)?;
                    if let [one] = segs {
                        let seg = lf::Segment::decode(*one)?;
                        if seg.kind == lf::SegKind::Ident
                            && !seg.has_args
                            && let Some(nm) = seg.name
                            && av.name(nm)? == param_hash
                        {
                            bound_lists.push(bounds);
                        }
                    }
                }
            }
            let mut has_unknown = false;
            let mut found_bounds: List<Region> = Vec::new_in(alloc);
            for bl in bound_lists {
                for &row in av.rows(bl)? {
                    if let lf::Bound::Lifetime(nm) = lf::Bound::decode(row)? {
                        let t = av.name_text(nm)?;
                        let bare = t.strip_prefix('\'').unwrap_or(t);
                        if bare == "static" {
                            if !found_bounds.iter().any(|&x| x == Region::Static) {
                                found_bounds.push(Region::Static);
                            }
                        } else if bare == "_" {
                            has_unknown = true;
                        } else {
                            let h = av.name(nm)?;
                            match lt_names.iter().position(|&x| x == h).and_then(|i| per.get(i).copied()) {
                                Some(r) => {
                                    if !found_bounds.iter().any(|&x| x == r) {
                                        found_bounds.push(r);
                                    }
                                }
                                None => has_unknown = true,
                            }
                        }
                    }
                }
            }
            if has_unknown {
                Ok(AdtParamBound::Ambiguous)
            } else if found_bounds.is_empty() {
                Ok(AdtParamBound::None)
            } else if found_bounds.len() == 1
                && let Some(&one) = found_bounds.first()
            {
                Ok(AdtParamBound::Unique(one))
            } else {
                Ok(AdtParamBound::Ambiguous)
            }
        });
        match lowered {
            Ok(res) => res,
            Err(_) => Ok(AdtParamBound::Ambiguous),
        }
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
                    let idx = usize::try_from(p.base).map_err(|_| unaligned)?;
                    let slot = regions.get_mut(idx).ok_or(unaligned)?;
                    *slot = r;
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
                            Ctx::Ref(r) | Ctx::Adt(AdtParamBound::Unique(r)) => r,
                            Ctx::Adt(AdtParamBound::None) if plain => Region::Static,
                            _ => return Err(LifeRefusal::ObjectDefault { at }),
                        },
                    };
                    if mode == Elide::Input && !plain {
                        self.outside = true;
                    }
                    let idx = usize::try_from(p.base).map_err(|_| unaligned)?;
                    let slot = regions.get_mut(idx).ok_or(unaligned)?;
                    *slot = r;
                }
                (lf::TypeRef::Path(path), PNode::Adt(k, l)) => self.adt_path(v, path, at, k, l, p, mode, regions, &mut work)?,
                (lf::TypeRef::Path(_) | lf::TypeRef::For { .. }, PNode::Dynamic(_)) => {
                    let mut bound: List<u64> = Vec::new_in(self.alloc);
                    let mut cur = written;
                    let mut unwrap_steps = 0u8;
                    while let lf::TypeRef::For { binder, inner } = cur {
                        unwrap_steps = unwrap_steps.saturating_add(1);
                        if unwrap_steps > 16 {
                            return Err(unaligned);
                        }
                        for &row in v.rows(binder)? {
                            if let lf::GenericParam::Lifetime { name, .. } = lf::GenericParam::decode(row)? {
                                bound.push(v.name(name)?);
                            }
                        }
                        cur = v.ty(inner)?;
                    }
                    let lf::TypeRef::Path(path) = cur else { return Err(unaligned) };
                    let (abs, texts, segs, _) = super::types::path_texts_of(v, path, true).map_err(|_| unaligned)?;
                    let Some(first) = texts.first().copied() else { return Err(unaligned) };
                    if segs.iter().any(|s| s.kind == lf::SegKind::Anchor) || (!abs && first == "Self") {
                        return Err(unaligned);
                    }
                    if !abs && texts.len() == 1 && self.params.iter().any(|n| *n == Name::of(first)) {
                        return Err(unaligned);
                    }
                    let found = self.resolve_path(v, path, abs, &texts, at)?;
                    match found.kind {
                        Kind::Trait | Kind::TraitAlias if usize::try_from(found.consumed).ok() == Some(texts.len()) => {}
                        Kind::TypeAlias => return Err(LifeRefusal::Alias { at }),
                        _ => return Err(unaligned),
                    }
                    let mut todo: List<lf::TypeId> = Vec::new_in(self.alloc);
                    let plain = self.quiet_path_bound(v, path, &bound, &mut todo, 0)? && self.quiet_all_bound(v, &bound, todo, 0)?;
                    let root_lts = self.materialize_bare_root_lts(v, path, &bound, found.key, mode, at)?;
                    let r = match p.ctx {
                        Ctx::Ref(r) | Ctx::Adt(AdtParamBound::Unique(r)) => r,
                        Ctx::Adt(AdtParamBound::None) | Ctx::Other => match self.trait_object_default(found.key, &root_lts)? {
                            TraitObjDefault::Resolved(r) => r,
                            TraitObjDefault::Unknown => return Err(LifeRefusal::ObjectDefault { at }),
                        },
                        Ctx::Adt(AdtParamBound::Ambiguous) => return Err(LifeRefusal::ObjectDefault { at }),
                    };
                    if mode == Elide::Input && !plain && !self.record_bare_object_inputs(v, path, &bound, &root_lts, at)? {
                        self.outside = true;
                    }
                    let idx = usize::try_from(p.base).map_err(|_| unaligned)?;
                    let slot = regions.get_mut(idx).ok_or(unaligned)?;
                    *slot = r;
                }
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

/// Result of inspecting a trait / trait alias and its supertraits for the Reference's
/// "Default trait object lifetimes" trait-bound rule (used when no containing reference or
/// lifetime-parameterized ADT supplies a default).
enum TraitObjDefault {
    Resolved(Region),
    Unknown,
}

impl<'c, 'n, S: NamesSource<'n>> Reader<'c, 'n, S> {
    /// Records `r` in `self.used` if not already present.
    fn record_used(&mut self, r: Region) {
        if !self.used.iter().any(|&u| u == r) {
            self.used.push(r);
        }
    }

    /// Materializes the root lifetime arguments of bare trait object path `path` targeting
    /// `found_key` once so both [`Self::trait_object_default`] and
    /// [`Self::record_bare_object_inputs`] share the exact same regions (including elided `'_`
    /// and omitted trait/alias lifetime arguments such as `trait Tr<'a>: 'a {}; fn f(_: *const Tr)`).
    fn materialize_bare_root_lts(
        &mut self,
        v: &View<'_>,
        path: tc::PathIdx,
        bound: &[u64],
        found_key: LibKey,
        mode: Elide,
        at: Range,
    ) -> Result<List<Option<Region>>, LifeRefusal> {
        let alloc = self.alloc;
        let (_, _, segs, pat) = super::types::path_texts_of(v, path, true).map_err(|_| LifeRefusal::Form)?;
        let mut root_args: List<Option<Region>> = Vec::new_in(alloc);
        let parenthesized = segs.last().is_some_and(|s| s.parenthesized);
        if let Some(last) = segs.last()
            && last.has_args
            && !last.parenthesized
        {
            for &row in v.rows(last.args)? {
                if let lf::GenericArg::Lifetime(nm) = lf::GenericArg::decode(row)? {
                    if bound.contains(&v.name(nm)?) {
                        root_args.push(None);
                    } else {
                        let r = self.site_region(v, Some(nm), mode, pat)?;
                        root_args.push(Some(r));
                    }
                }
            }
        }
        if root_args.is_empty()
            && !parenthesized
            && let Ok(declared) = self.lifetimes_of(found_key)
        {
            for _ in 0..declared {
                let r = self.elided(mode, at)?;
                if mode == Elide::Input {
                    self.record_used(r);
                }
                root_args.push(Some(r));
            }
        }
        Ok(root_args)
    }

    /// Computes the trait-bound object-lifetime default for bare trait object `found_key` with
    /// already-materialized lifetime arguments `root_args` (the Reference, "Default trait object
    /// lifetimes"):
    /// - if `'static` appears on any lifetime bound of the trait or its supertraits -> `'static`;
    /// - else if a single distinct lifetime bound appears -> that lifetime;
    /// - else if no lifetime bounds appear anywhere on the trait or its supertraits -> `'static`;
    /// - else (multiple distinct bounds, higher-ranked/elided bound, or unreadable/overflowed
    ///   supertrait graph) -> `TraitObjDefault::Unknown` (caller refuses `ObjectDefault`).
    fn trait_object_default(&mut self, found_key: LibKey, root_args: &[Option<Region>]) -> Result<TraitObjDefault, LifeRefusal> {
        const MOST_DEFAULT_WALK: usize = 64;
        let alloc = self.alloc;
        let mut arg_pool: List<Option<Region>> = Vec::new_in(alloc);
        let root_len = u32::try_from(root_args.len()).map_err(|_| LifeRefusal::Bound)?;
        arg_pool.extend_from_slice(root_args);
        let mut work: List<(LibKey, u32, u32)> = Vec::new_in(alloc);
        work.push((found_key, 0, root_len));

        let mut has_static = false;
        let mut has_unknown_bound = false;
        let mut found_bounds: List<Region> = Vec::new_in(alloc);
        let mut next = 0usize;
        while let Some(&(k, start, len)) = work.get(next) {
            next += 1;
            let Ok((text, item_module)) = item_text(self.names, self.locals, k) else {
                return Ok(TraitObjDefault::Unknown);
            };
            let item_marks = if let Some(local) = self.locals.head(checker_of(k)) {
                local.marks
            } else {
                super::item_site::item_marks(self.names, k, text).ok().flatten()
            };
            let Ok(st) = usize::try_from(start) else { return Ok(TraitObjDefault::Unknown) };
            let Ok(en) = usize::try_from(start.saturating_add(len)) else { return Ok(TraitObjDefault::Unknown) };
            let mut cur_args: List<Option<Region>> = Vec::new_in(alloc);
            if let Some(sl) = arg_pool.get(st..en) {
                cur_args.extend_from_slice(sl);
            } else {
                return Ok(TraitObjDefault::Unknown);
            }
            let names = self.names;
            let mut step_unknown = false;
            let lowered = with_lowered(names, k, text, |tv| -> Result<(), LifeRefusal> {
                let mut lt_names: List<u64> = Vec::new_in(alloc);
                for &row in tv.rows(tv.list(head::GENERICS))? {
                    if let lf::GenericParam::Lifetime { name, .. } = lf::GenericParam::decode(row)? {
                        lt_names.push(tv.name(name)?);
                    }
                }
                let mut param_map: List<Option<Region>> = Vec::with_capacity_in(lt_names.len(), alloc);
                if cur_args.is_empty() {
                    for _ in 0..lt_names.len() {
                        param_map.push(None);
                    }
                } else if cur_args.len() == lt_names.len() {
                    param_map.extend_from_slice(&cur_args);
                } else {
                    step_unknown = true;
                    return Ok(());
                }
                let resolve_lt = |nm: lf::NameIdx, extra_bound: &[u64]| -> Result<Option<Region>, LifeRefusal> {
                    let t = tv.name_text(nm)?;
                    let bare = t.strip_prefix('\'').unwrap_or(t);
                    if bare == "static" {
                        return Ok(Some(Region::Static));
                    }
                    if bare == "_" {
                        return Ok(None);
                    }
                    let h = tv.name(nm)?;
                    if extra_bound.contains(&h) {
                        return Ok(None);
                    }
                    Ok(lt_names.iter().position(|&x| x == h).and_then(|pos| param_map.get(pos).copied().flatten()))
                };
                let mut bound_lists: List<tc::List> = Vec::new_in(alloc);
                bound_lists.push(tv.list(head::BOUNDS));
                for &row in tv.rows(tv.list(head::WHERE))? {
                    if let lf::WherePred::Type { ty, bounds, .. } = lf::WherePred::decode(row)?
                        && let lf::TypeRef::Path(sp) = tv.ty(ty)?
                        && is_self_path(tv, sp)?
                    {
                        bound_lists.push(bounds);
                    }
                }
                for bl in bound_lists {
                    for &row in tv.rows(bl)? {
                        match lf::Bound::decode(row)? {
                            lf::Bound::Lifetime(nm) => match resolve_lt(nm, &[])? {
                                Some(Region::Static) => has_static = true,
                                Some(r) => {
                                    if !found_bounds.iter().any(|&x| x == r) {
                                        found_bounds.push(r);
                                    }
                                }
                                None => has_unknown_bound = true,
                            },
                            lf::Bound::Trait { ty, binder, mods } => {
                                if mods.maybe || mods.negative {
                                    continue;
                                }
                                let lf::TypeRef::Path(bp) = tv.ty(ty)? else {
                                    step_unknown = true;
                                    return Ok(());
                                };
                                let mut sub_bound: List<u64> = Vec::new_in(alloc);
                                for &brow in tv.rows(binder)? {
                                    if let lf::GenericParam::Lifetime { name, .. } = lf::GenericParam::decode(brow)? {
                                        sub_bound.push(tv.name(name)?);
                                    }
                                }
                                let Ok((babs, btexts, bsegs, br)) = super::types::path_texts_of(tv, bp, true) else {
                                    step_unknown = true;
                                    return Ok(());
                                };
                                let mut sub_args: List<Option<Region>> = Vec::new_in(alloc);
                                if let Some(blast) = bsegs.last()
                                    && blast.has_args
                                    && !blast.parenthesized
                                {
                                    for &arow in tv.rows(blast.args)? {
                                        if let lf::GenericArg::Lifetime(nm) = lf::GenericArg::decode(arow)? {
                                            sub_args.push(resolve_lt(nm, &sub_bound)?);
                                        }
                                    }
                                }
                                let resolver = Paths { names };
                                let Ok(sub) = super::item_site::resolve_written(&resolver, item_marks, item_module, tv, bp, babs, &btexts, Ns::Type, br) else {
                                    step_unknown = true;
                                    return Ok(());
                                };
                                if matches!(sub.kind, Kind::Trait | Kind::TraitAlias) && usize::try_from(sub.consumed).ok() == Some(btexts.len()) {
                                    let already = work.iter().any(|&(wk, wst, wlen)| {
                                        wk == sub.key
                                            && usize::try_from(wst)
                                                .ok()
                                                .zip(usize::try_from(wst.saturating_add(wlen)).ok())
                                                .and_then(|(a, b)| arg_pool.get(a..b))
                                                == Some(sub_args.as_slice())
                                    });
                                    if !already {
                                        if work.len() >= MOST_DEFAULT_WALK {
                                            step_unknown = true;
                                            return Ok(());
                                        }
                                        let Ok(sub_start) = u32::try_from(arg_pool.len()) else {
                                            step_unknown = true;
                                            return Ok(());
                                        };
                                        let Ok(sub_len) = u32::try_from(sub_args.len()) else {
                                            step_unknown = true;
                                            return Ok(());
                                        };
                                        arg_pool.extend_from_slice(&sub_args);
                                        work.push((sub.key, sub_start, sub_len));
                                    }
                                } else {
                                    step_unknown = true;
                                    return Ok(());
                                }
                            }
                            lf::Bound::Use(_) => {
                                step_unknown = true;
                                return Ok(());
                            }
                        }
                    }
                }
                Ok(())
            });
            match lowered {
                Ok(Ok(())) => {}
                Ok(Err(e)) => return Err(e),
                Err(_) => return Ok(TraitObjDefault::Unknown),
            }
            if step_unknown {
                return Ok(TraitObjDefault::Unknown);
            }
        }
        if has_static {
            Ok(TraitObjDefault::Resolved(Region::Static))
        } else if has_unknown_bound {
            Ok(TraitObjDefault::Unknown)
        } else if found_bounds.is_empty() {
            Ok(TraitObjDefault::Resolved(Region::Static))
        } else if found_bounds.len() == 1
            && let Some(&one) = found_bounds.first()
        {
            Ok(TraitObjDefault::Resolved(one))
        } else {
            Ok(TraitObjDefault::Unknown)
        }
    }

    /// Walks a non-quiet bare trait object path in input position, recording every free/elided
    /// input lifetime into `self.used` so output elision still succeeds when all input lifetimes
    /// agree (e.g. `fn f<'a>(x: &'a Tr<'a>) -> &u8`). Returns `Ok(false)` if any form cannot be
    /// walked completely (caller sets `self.outside = true`).
    fn record_bare_object_inputs(
        &mut self,
        v: &View<'_>,
        path: tc::PathIdx,
        bound: &[u64],
        root_lts: &[Option<Region>],
        at: Range,
    ) -> Result<bool, LifeRefusal> {
        let mut todo: List<(lf::TypeId, bool)> = Vec::new_in(self.alloc);
        if !self.record_path_inputs(v, path, bound, None, Some(root_lts), false, at, &mut todo, 0)? {
            return Ok(false);
        }
        self.record_all_inputs(v, bound, None, at, todo, 0)
    }

    /// Records the lifetimes written or elided on path `p`, pushing its type arguments to `todo`.
    #[expect(clippy::too_many_arguments, reason = "one path input walk: view, path, bound set, optional alias/root lifetime maps, parenthesized flag, site, worklist, depth")]
    fn record_path_inputs(
        &mut self,
        v: &View<'_>,
        p: tc::PathIdx,
        bound: &[u64],
        alias_map: Option<(&[u64], &[Option<Region>])>,
        pre_root_lts: Option<&[Option<Region>]>,
        elide_bound: bool,
        at: Range,
        todo: &mut List<(lf::TypeId, bool)>,
        depth: u8,
    ) -> Result<bool, LifeRefusal> {
        let Ok((abs, texts, segs, pat)) = super::types::path_texts_of(v, p, true) else {
            return Ok(false);
        };
        let mut last_lts: List<Option<Region>> = Vec::new_in(self.alloc);
        let mut written_pos_args = 0usize;
        for (i, s) in segs.iter().enumerate() {
            if s.kind == lf::SegKind::Anchor {
                return Ok(false);
            }
            if !s.has_args {
                continue;
            }
            let is_last = i + 1 == segs.len();
            if s.parenthesized {
                if is_last {
                    written_pos_args = 1;
                }
                for &row in v.rows(s.args)? {
                    match lf::GenericArg::decode(row)? {
                        lf::GenericArg::Type(t) => todo.push((t, true)),
                        _ => return Ok(false),
                    }
                }
                if let Some(ret) = s.ty {
                    todo.push((ret, true));
                }
                continue;
            }
            for &row in v.rows(s.args)? {
                match lf::GenericArg::decode(row)? {
                    lf::GenericArg::Type(t) => {
                        if is_last {
                            written_pos_args += 1;
                        }
                        todo.push((t, elide_bound));
                    }
                    lf::GenericArg::Const(_) => {
                        if is_last {
                            written_pos_args += 1;
                        }
                    }
                    lf::GenericArg::AssocType { args, ty, .. } if v.rows(args)?.is_empty() => todo.push((ty, elide_bound)),
                    lf::GenericArg::Lifetime(nm) => {
                        if is_last && pre_root_lts.is_some() {
                            continue;
                        }
                        let Some(opt_r) = self.record_written_lt(v, nm, bound, alias_map, elide_bound, pat)? else {
                            return Ok(false);
                        };
                        if is_last {
                            last_lts.push(opt_r);
                        }
                    }
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
        let Ok(found) = self.resolve_path(v, p, abs, &texts, pat) else { return Ok(false) };
        if usize::try_from(found.consumed).ok() != Some(texts.len()) {
            return Ok(false);
        }
        let eff_lts: &[Option<Region>] = pre_root_lts.unwrap_or(&last_lts);
        match found.kind {
            Kind::Builtin(_) => Ok(true),
            Kind::Struct | Kind::Enum | Kind::Union | Kind::Trait => {
                let Ok(declared) = self.lifetimes_of(found.key) else { return Ok(false) };
                let mut lt_vals: List<Option<Region>> = Vec::with_capacity_in(usize::try_from(declared).unwrap_or(0), self.alloc);
                if eff_lts.is_empty() {
                    for _ in 0..declared {
                        if elide_bound {
                            lt_vals.push(None);
                        } else {
                            let r = self.elided(Elide::Input, at)?;
                            self.record_used(r);
                            lt_vals.push(Some(r));
                        }
                    }
                } else if u32::try_from(eff_lts.len()).ok() == Some(declared) {
                    lt_vals.extend_from_slice(eff_lts);
                } else {
                    return Ok(false);
                }
                self.record_omitted_defaults_inputs(found.key, written_pos_args, &lt_vals, elide_bound, at, depth)
            }
            Kind::TraitAlias => self.record_trait_alias_inputs(found.key, eff_lts, written_pos_args, elide_bound, at, depth),
            _ => Ok(false),
        }
    }

    /// Walks omitted defaulted type arguments (`pos >= written_pos_args`) of `item_key` in input
    /// position, recording any free/elided lifetimes (such as `'static` in `trait Defaulted<T = &'static u8> {}`)
    /// into `self.used`.
    fn record_omitted_defaults_inputs(
        &mut self,
        item_key: LibKey,
        written_pos_args: usize,
        lt_vals: &[Option<Region>],
        elide_bound: bool,
        at: Range,
        depth: u8,
    ) -> Result<bool, LifeRefusal> {
        const MOST_DEFAULT_DEPTH: u8 = 8;
        if depth >= MOST_DEFAULT_DEPTH {
            return Ok(false);
        }
        let Ok((text, item_module)) = item_text(self.names, self.locals, item_key) else {
            return Ok(false);
        };
        let item_marks = if let Some(local) = self.locals.head(checker_of(item_key)) {
            local.marks
        } else {
            super::item_site::item_marks(self.names, item_key, text).ok().flatten()
        };
        let alloc = self.alloc;
        let saved_module = self.module;
        let saved_marks = self.marks;
        let mut saved_params: List<Name> = Vec::with_capacity_in(self.params.len(), alloc);
        saved_params.extend_from_slice(&self.params);
        self.module = item_module;
        self.marks = item_marks;
        self.params.clear();
        let names = self.names;
        let res = with_lowered(names, item_key, text, |av| -> Result<bool, LifeRefusal> {
            let mut lt_names: List<u64> = Vec::new_in(alloc);
            let mut todo: List<(lf::TypeId, bool)> = Vec::new_in(alloc);
            let mut pos = 0usize;
            for &row in av.rows(av.list(head::GENERICS))? {
                match lf::GenericParam::decode(row)? {
                    lf::GenericParam::Lifetime { name, .. } => lt_names.push(av.name(name)?),
                    lf::GenericParam::Type { name, default, .. } => {
                        if pos >= written_pos_args
                            && let Some(def_ty) = default
                        {
                            todo.push((def_ty, elide_bound));
                        }
                        self.params.push(Name(av.name(name)?));
                        pos += 1;
                    }
                    lf::GenericParam::Const { name, .. } => {
                        self.params.push(Name(av.name(name)?));
                        pos += 1;
                    }
                }
            }
            if todo.is_empty() {
                return Ok(true);
            }
            if lt_names.len() != lt_vals.len() {
                return Ok(false);
            }
            self.record_all_inputs(av, &[], Some((lt_names.as_slice(), lt_vals)), at, todo, depth + 1)
        });
        self.module = saved_module;
        self.marks = saved_marks;
        self.params.clear();
        self.params.extend_from_slice(&saved_params);
        match res {
            Ok(inner) => inner,
            Err(_) => Ok(false),
        }
    }

    /// Resolves and records one written lifetime name `nm` in input position. Returns:
    /// - `Ok(Some(None))` when `nm` is bound by `bound` or an elided lifetime inside `Fn(..)`;
    /// - `Ok(Some(Some(r)))` when `nm` resolves to region `r` (recorded in `self.used`);
    /// - `Ok(None)` when `nm` cannot be mapped (caller marks `self.outside = true`).
    fn record_written_lt(
        &mut self,
        v: &View<'_>,
        nm: lf::NameIdx,
        bound: &[u64],
        alias_map: Option<(&[u64], &[Option<Region>])>,
        elide_bound: bool,
        at: Range,
    ) -> Result<Option<Option<Region>>, LifeRefusal> {
        let text = v.name_text(nm)?;
        let bare = text.strip_prefix('\'').unwrap_or(text);
        if bare == "static" {
            self.record_used(Region::Static);
            return Ok(Some(Some(Region::Static)));
        }
        if bare == "_" {
            if elide_bound {
                return Ok(Some(None));
            }
            let r = self.elided(Elide::Input, at)?;
            self.record_used(r);
            return Ok(Some(Some(r)));
        }
        let h = v.name(nm)?;
        if bound.contains(&h) {
            return Ok(Some(None));
        }
        if let Some((lt_names, lt_vals)) = alias_map {
            let Some(pos) = lt_names.iter().position(|&x| x == h) else {
                return Ok(None);
            };
            let Some(&opt_r) = lt_vals.get(pos) else {
                return Ok(None);
            };
            if let Some(r) = opt_r {
                self.record_used(r);
            }
            return Ok(Some(opt_r));
        }
        let r = self.site_region(v, Some(nm), Elide::Input, at)?;
        Ok(Some(Some(r)))
    }

    /// Expands trait alias `alias_key` in input position, substituting its written lifetimes
    /// `written_lts` into its bounds and recording all resulting free/elided lifetimes.
    fn record_trait_alias_inputs(
        &mut self,
        alias_key: LibKey,
        written_lts: &[Option<Region>],
        written_pos_args: usize,
        elide_bound: bool,
        at: Range,
        depth: u8,
    ) -> Result<bool, LifeRefusal> {
        const MOST_ALIAS_DEPTH: u8 = 8;
        if depth >= MOST_ALIAS_DEPTH {
            return Ok(false);
        }
        let Ok((text, alias_module)) = item_text(self.names, self.locals, alias_key) else {
            return Ok(false);
        };
        let alias_marks = if let Some(local) = self.locals.head(checker_of(alias_key)) {
            local.marks
        } else {
            super::item_site::item_marks(self.names, alias_key, text).ok().flatten()
        };
        let alloc = self.alloc;
        let saved_module = self.module;
        let saved_marks = self.marks;
        let mut saved_params: List<Name> = Vec::with_capacity_in(self.params.len(), alloc);
        saved_params.extend_from_slice(&self.params);
        self.module = alias_module;
        self.marks = alias_marks;
        self.params.clear();
        let names = self.names;
        let res = with_lowered(names, alias_key, text, |av| -> Result<bool, LifeRefusal> {
            let mut lt_names: List<u64> = Vec::new_in(alloc);
            let mut todo: List<(lf::TypeId, bool)> = Vec::new_in(alloc);
            let mut pos = 0usize;
            for &row in av.rows(av.list(head::GENERICS))? {
                match lf::GenericParam::decode(row)? {
                    lf::GenericParam::Lifetime { name, .. } => lt_names.push(av.name(name)?),
                    lf::GenericParam::Type { name, default, .. } => {
                        if pos >= written_pos_args
                            && let Some(def_ty) = default
                        {
                            todo.push((def_ty, elide_bound));
                        }
                        self.params.push(Name(av.name(name)?));
                        pos += 1;
                    }
                    lf::GenericParam::Const { name, .. } => {
                        self.params.push(Name(av.name(name)?));
                        pos += 1;
                    }
                }
            }
            let mut lt_vals: List<Option<Region>> = Vec::with_capacity_in(lt_names.len(), alloc);
            if written_lts.is_empty() {
                for _ in 0..lt_names.len() {
                    if elide_bound {
                        lt_vals.push(None);
                    } else {
                        let r = self.elided(Elide::Input, at)?;
                        self.record_used(r);
                        lt_vals.push(Some(r));
                    }
                }
            } else if written_lts.len() == lt_names.len() {
                lt_vals.extend_from_slice(written_lts);
            } else {
                return Ok(false);
            }
            let amap = Some((lt_names.as_slice(), lt_vals.as_slice()));
            if !self.record_bounds_inputs(av, av.list(head::BOUNDS), &[], amap, elide_bound, at, &mut todo, depth + 1)? {
                return Ok(false);
            }
            self.record_all_inputs(av, &[], amap, at, todo, depth + 1)
        });
        self.module = saved_module;
        self.marks = saved_marks;
        self.params.clear();
        self.params.extend_from_slice(&saved_params);
        match res {
            Ok(inner) => inner,
            Err(_) => Ok(false),
        }
    }

    /// Walks trait/lifetime bounds `bounds` in input position, recording lifetimes into `self.used`.
    #[expect(clippy::too_many_arguments, reason = "bounds input walk: view, bounds, bound set, alias map, parenthesized flag, site, worklist, depth")]
    fn record_bounds_inputs(
        &mut self,
        v: &View<'_>,
        bounds: tc::List,
        bound: &[u64],
        alias_map: Option<(&[u64], &[Option<Region>])>,
        elide_bound: bool,
        at: Range,
        todo: &mut List<(lf::TypeId, bool)>,
        depth: u8,
    ) -> Result<bool, LifeRefusal> {
        for &row in v.rows(bounds)? {
            match lf::Bound::decode(row)? {
                lf::Bound::Trait { ty, binder, .. } => {
                    let binder_rows = v.rows(binder)?;
                    let lf::TypeRef::Path(p) = v.ty(ty)? else { return Ok(false) };
                    if binder_rows.is_empty() {
                        if !self.record_path_inputs(v, p, bound, alias_map, None, elide_bound, at, todo, depth)? {
                            return Ok(false);
                        }
                    } else {
                        let mut nested: List<u64> = Vec::with_capacity_in(bound.len() + binder_rows.len(), self.alloc);
                        nested.extend_from_slice(bound);
                        for &brow in binder_rows {
                            if let lf::GenericParam::Lifetime { name, .. } = lf::GenericParam::decode(brow)? {
                                nested.push(v.name(name)?);
                            }
                        }
                        let mut sub_todo: List<(lf::TypeId, bool)> = Vec::new_in(self.alloc);
                        if !self.record_path_inputs(v, p, &nested, alias_map, None, elide_bound, at, &mut sub_todo, depth)? {
                            return Ok(false);
                        }
                        if !self.record_all_inputs(v, &nested, alias_map, at, sub_todo, depth)? {
                            return Ok(false);
                        }
                    }
                }
                lf::Bound::Lifetime(nm) => {
                    if self.record_written_lt(v, nm, bound, alias_map, elide_bound, at)?.is_none() {
                        return Ok(false);
                    }
                }
                lf::Bound::Use(_) => return Ok(false),
            }
        }
        Ok(true)
    }

    /// Drains `todo` in input position, recording all free/elided lifetimes into `self.used`.
    fn record_all_inputs(
        &mut self,
        v: &View<'_>,
        bound: &[u64],
        alias_map: Option<(&[u64], &[Option<Region>])>,
        at: Range,
        todo: List<(lf::TypeId, bool)>,
        depth: u8,
    ) -> Result<bool, LifeRefusal> {
        let mut bound_stack: List<u64> = Vec::with_capacity_in(bound.len(), self.alloc);
        bound_stack.extend_from_slice(bound);
        let mut work: List<RecordInputWork> = Vec::with_capacity_in(todo.len(), self.alloc);
        for &(t, elide_bound) in todo.iter() {
            work.push(RecordInputWork::Ty(t, elide_bound));
        }
        let mut sub_todo: List<(lf::TypeId, bool)> = Vec::new_in(self.alloc);
        let mut cur_depth = depth;
        let mut steps = 0u32;
        while let Some(item) = work.pop() {
            let (t, elide_bound) = match item {
                RecordInputWork::PopBound { prev_len, prev_depth } => {
                    bound_stack.truncate(prev_len);
                    cur_depth = prev_depth;
                    continue;
                }
                RecordInputWork::Ty(t, elide_bound) => (t, elide_bound),
            };
            steps += 1;
            if steps > MOST {
                return Err(LifeRefusal::Bound);
            }
            match v.ty(t)? {
                lf::TypeRef::Never | lf::TypeRef::Infer => {}
                lf::TypeRef::Ref { lifetime, inner, .. } => {
                    match lifetime {
                        Some(nm) => {
                            if self.record_written_lt(v, nm, &bound_stack, alias_map, elide_bound, at)?.is_none() {
                                return Ok(false);
                            }
                        }
                        None => {
                            if !elide_bound {
                                let r = self.elided(Elide::Input, at)?;
                                self.record_used(r);
                            }
                        }
                    }
                    work.push(RecordInputWork::Ty(inner, elide_bound));
                }
                lf::TypeRef::Ptr { inner, .. } | lf::TypeRef::Slice(inner) => work.push(RecordInputWork::Ty(inner, elide_bound)),
                lf::TypeRef::Array { elem, .. } => work.push(RecordInputWork::Ty(elem, elide_bound)),
                lf::TypeRef::Pat { base, .. } => work.push(RecordInputWork::Ty(base, elide_bound)),
                lf::TypeRef::Tuple(l) | lf::TypeRef::Union(l) => {
                    for &i in v.ids(l)? {
                        work.push(RecordInputWork::Ty(lf::TypeId(i), elide_bound));
                    }
                }
                lf::TypeRef::Path(p) => {
                    sub_todo.clear();
                    if !self.record_path_inputs(v, p, &bound_stack, alias_map, None, elide_bound, at, &mut sub_todo, cur_depth)? {
                        return Ok(false);
                    }
                    for &(st, eb) in sub_todo.iter() {
                        work.push(RecordInputWork::Ty(st, eb));
                    }
                }
                lf::TypeRef::Dyn(b) | lf::TypeRef::Impl(b) => {
                    sub_todo.clear();
                    if !self.record_bounds_inputs(v, b, &bound_stack, alias_map, elide_bound, at, &mut sub_todo, cur_depth)? {
                        return Ok(false);
                    }
                    for &(st, eb) in sub_todo.iter() {
                        work.push(RecordInputWork::Ty(st, eb));
                    }
                }
                lf::TypeRef::For { binder, inner } if cur_depth < 8 => {
                    let prev_len = bound_stack.len();
                    let prev_depth = cur_depth;
                    for &brow in v.rows(binder)? {
                        if let lf::GenericParam::Lifetime { name, .. } = lf::GenericParam::decode(brow)? {
                            bound_stack.push(v.name(name)?);
                        }
                    }
                    cur_depth = cur_depth.saturating_add(1);
                    work.push(RecordInputWork::PopBound { prev_len, prev_depth });
                    work.push(RecordInputWork::Ty(inner, elide_bound));
                }
                _ => return Ok(false),
            }
        }
        Ok(true)
    }
}

/// Worklist entry for `Eliding::quiet_all_bound`.
enum QuietWork {
    Ty(lf::TypeId),
    PopBound { prev_len: usize, prev_depth: u8 },
}

/// Worklist entry for `Eliding::record_all_inputs`.
enum RecordInputWork {
    Ty(lf::TypeId, bool),
    PopBound { prev_len: usize, prev_depth: u8 },
}

/// No position is left unfilled.
fn filled(regions: &List<Region>) -> Result<(), LifeRefusal> {
    if regions.iter().any(|r| *r == HOLE) {
        return Err(LifeRefusal::Unaligned { at: Range::new(0, 0) });
    }
    Ok(())
}

/// The `Self` type a member fn belongs to, used to detect a receiver reference chain around `Self`.
#[derive(Clone, Copy)]
enum ReceiverSelf {
    Impl(PTy),
    Trait,
}

impl ReceiverSelf {
    fn matches<'n, S: NamesSource<'n>>(self, r: &Reader<'_, 'n, S>, ty: PTy) -> bool {
        match self {
            ReceiverSelf::Impl(s) => ty == s,
            ReceiverSelf::Trait => matches!(r.node(ty), Ok(PNode::Param(0))),
        }
    }
}

/// Whether a typed `self` parameter (`self: &Arc<Self>`, `self: Pin<&mut Self>`, `self: &Self`,
/// ...) wraps `self_ty` in a pointer/reference chain with **exactly one** `&` / `&mut` and only
/// zero-position wrapper ADTs before it (the Reference, "Lifetime elision in functions"; RFC 2394).
fn receiver_has_single_ref<'n, S: NamesSource<'n>>(r: &mut Reader<'_, 'n, S>, mut ty: PTy, self_ty: ReceiverSelf) -> Result<bool, LifeRefusal> {
    const MAX_DEPTH: usize = 16;
    let mut refs = 0u32;
    for _ in 0..MAX_DEPTH {
        if self_ty.matches(r, ty) {
            return Ok(refs == 1);
        }
        match r.node(ty)? {
            PNode::Ref(_, inner) => {
                refs += 1;
                if refs > 1 {
                    return Ok(false);
                }
                ty = inner;
            }
            PNode::Adt(adt, args) => {
                if refs == 0 && r.adt_positions(adt)? != 0 {
                    return Ok(false);
                }
                let list = r.list(args)?;
                let Some(&first) = list.first() else { return Ok(false) };
                ty = first;
            }
            _ => return Ok(false),
        }
    }
    Ok(false)
}

/// A fn's lifetime part: `v` its own text's view, `inputs` and `output` its head's erased types,
/// `container` its impl's or trait's part. `r` is new, with the fn's module set.
fn read_fn<'n, S: NamesSource<'n>>(
    r: &mut Reader<'_, 'n, S>,
    v: &View<'_>,
    inputs: &[PTy],
    output: PTy,
    container: Option<&ContainerLife>,
    receiver_self: Option<ReceiverSelf>,
) -> Result<FnLife, LifeRefusal> {
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
    // Skip the trailing `...` parameter of a C-variadic fn: `inputs` holds only the fixed ones.
    let fixed_len = match rows.last().and_then(|&row| lf::Param::decode(row).ok()) {
        Some(p) if p.variadic => rows.len() - 1,
        _ => rows.len(),
    };
    if fixed_len != inputs.len() {
        return Err(LifeRefusal::ParamCount { text: fixed_len as u32, head: inputs.len() as u32 });
    }
    // The receiver's reference, when it is `&self`, `&mut self`, `self: &Self` or `self: &mut Self`.
    let mut receiver: Option<Region> = None;
    for (j, &row) in rows.iter().take(fixed_len).enumerate() {
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
                let by_ref = match (j, prm.self_kind, receiver_self) {
                    (0, lf::SelfKind::Typed, Some(rs)) => receiver_has_single_ref(r, ty, rs)?,
                    _ => false,
                };
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

    /// The lifetime part of the impl or trait `lib`, whose head is made, computed directly from
    /// an already-lowered `View` so callers like `Store::impl_` and `Store::trait_members` do not
    /// re-lex and re-lower the item text a second time.
    pub(super) fn container_life_from_view<S: NamesSource<'n>>(&mut self, names: S, keys: &Keys, lib: LibKey, module: LibKey, v: &View<'_>) {
        let ck = checker_of(lib);
        if self.lives.containers.contains_key(&ck) {
            return;
        }
        let self_ty = match self.impls.get(&ck) {
            Some(i) => Some(i.self_ty),
            None if self.maps.traits.contains_key(&lib) => None,
            None => return,
        };
        let Store { tys, lives, locals, .. } = self;
        let Lives { counts, alloc, .. } = lives;
        let marks = if let Some(local) = locals.head(ck) {
            local.marks
        } else {
            super::item_site::item_marks(names, lib, v.item()).ok().flatten()
        };
        let mut r = Reader::new(names, keys, locals, tys.view(), counts, module, marks, *alloc);
        let made = read_container(&mut r, v, self_ty);
        self.lives.containers.insert(ck, made);
    }

    /// The lifetime part of the struct, enum or union `lib`, computed directly from an
    /// already-lowered `View` so `Store::adt` does not re-lex and re-lower the item text.
    pub(super) fn adt_life_from_view<S: NamesSource<'n>>(&mut self, names: S, keys: &Keys, lib: LibKey, module: LibKey, v: &View<'_>) {
        if self.lives.adts.contains_key(&lib) {
            return;
        }
        let Store { tys, adts, lives, locals, .. } = self;
        let Lives { counts, alloc, .. } = lives;
        let Some(row) = adts.get(&checker_of(lib)) else { return };
        let mut field_tys: List<PTy> = Vec::with_capacity_in(row.fields.len(), *alloc);
        for f in row.fields.iter() {
            field_tys.push(f.ty);
        }
        let marks = if let Some(local) = locals.head(checker_of(lib)) {
            local.marks
        } else {
            super::item_site::item_marks(names, lib, v.item()).ok().flatten()
        };
        let mut r = Reader::new(names, keys, locals, tys.view(), counts, module, marks, *alloc);
        let made = read_adt(&mut r, v, &field_tys, row.kind == tc::AdtKind::Enum);
        self.lives.adts.insert(lib, made);
    }

    /// The lifetime parts actually encoded in head records (form 22: ADTs, traits, and impls;
    /// fn records carry `HeadRegions` instead of form-10 `FnLife`, so computing `FnLife` during
    /// export/digesting would lower every fn a second time and discard the result).
    pub fn make_record_lives<S: NamesSource<'n>>(&mut self, names: S, keys: &Keys, heads: &[HeadAt]) {
        for at in heads.iter().copied() {
            match at {
                HeadAt::Adt(lib) => {
                    let _ = self.adt_life(names, keys, lib);
                }
                HeadAt::Trait(lib) => {
                    let _ = self.container_life(names, keys, lib);
                }
                HeadAt::Impl(k) => match keys.library_in(names, k) {
                    Some(lib) => {
                        let _ = self.container_life(names, keys, lib);
                    }
                    None => {
                        self.lives.containers.insert(k, Err(LifeRefusal::NoHead));
                    }
                },
                HeadAt::Fn(_) | HeadAt::Ctor(_) | HeadAt::Alias(_) | HeadAt::TraitAlias(_) | HeadAt::Opaque(_) => {}
            }
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
        let Store { tys, fns, impls, inferred_outputs, lives, locals, .. } = self;
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
        let receiver_self = if has_container {
            match impls.get(&checker_of(lib)) {
                Some(i) => Some(ReceiverSelf::Impl(i.self_ty)),
                None => Some(ReceiverSelf::Trait),
            }
        } else {
            None
        };
        let marks = if let Some(local) = locals.head(key) {
            local.marks
        } else if has_container {
            match keys.origin(key) {
                Some(super::keys::Origin::Member { name, .. }) => {
                    super::item_site::head_marks(names, lib, name, blaze_surface::ItemKind::Function, h.text).ok().flatten()
                }
                _ => super::item_site::item_marks(names, lib, h.text).ok().flatten(),
            }
        } else {
            super::item_site::item_marks(names, lib, h.text).ok().flatten()
        };
        let context = match receiver_self {
            Some(ReceiverSelf::Trait) => blaze_lower::ItemContext::Trait,
            _ => blaze_lower::ItemContext::TopLevel,
        };
        super::heads::with_lowered_in_context(names, lib, h.text, context, |v| {
            let mut r = Reader::new(names, keys, locals, tys.view(), counts, h.module, marks, *alloc);
            // The container's type parameters shadow items too: every name the head was read with.
            for &(name, _) in h.params.iter() {
                r.params.push(name);
            }
            read_fn(&mut r, v, &h.inputs[..], h_output, container, receiver_self)
        })
        .map_err(|_| LifeRefusal::Text)?
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
        let marks = if let Some(local) = locals.head(checker_of(lib)) {
            local.marks
        } else {
            super::item_site::item_marks(names, lib, text).ok().flatten()
        };
        super::heads::with_lowered(names, lib, text, |v| {
            let mut r = Reader::new(names, keys, locals, tys.view(), counts, module, marks, *alloc);
            read_container(&mut r, v, self_ty)
        })
        .map_err(|_| LifeRefusal::Text)?
    }

    fn make_adt_life<S: NamesSource<'n>>(&mut self, names: S, keys: &Keys, lib: LibKey) -> Result<AdtLife, LifeRefusal> {
        let Store { tys, adts, lives, locals, .. } = self;
        let Lives { counts, alloc, .. } = lives;
        let row = adts.get(&checker_of(lib)).ok_or(LifeRefusal::NoHead)?;
        let mut field_tys: List<PTy> = Vec::with_capacity_in(row.fields.len(), *alloc);
        for f in row.fields.iter() {
            field_tys.push(f.ty);
        }
        let (text, module) = item_text(names, locals, lib)?;
        let marks = if let Some(local) = locals.head(checker_of(lib)) {
            local.marks
        } else {
            super::item_site::item_marks(names, lib, text).ok().flatten()
        };
        super::heads::with_lowered(names, lib, text, |v| {
            let mut r = Reader::new(names, keys, locals, tys.view(), counts, module, marks, *alloc);
            read_adt(&mut r, v, &field_tys, row.kind == tc::AdtKind::Enum)
        })
        .map_err(|_| LifeRefusal::Text)?
    }
}
