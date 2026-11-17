//! Layout: the one home for the size, alignment, field offsets, tag and niches of every ground
//! type (AGENTS.md rule 30). Every other part of blaze that needs a type's bytes (the walk,
//! constant evaluation, statics, intrinsics such as `size_of`) asks [`Layouts::of`].
//!
//! The target is AArch64 (and every 64-bit little-endian target blaze builds for): pointers are
//! 8 bytes, `i128`/`u128` are aligned to 16 (the AAPCS64 rule rustc adopted in 1.77), `char` is
//! a `u32`, `bool` a byte.
//!
//! # The rules, each named
//!
//! - **Primitives** (Reference, "Type layout", "Primitive data layout"): an integer or float of
//!   `n` bytes has size `n` and alignment `n`; `bool` is one byte whose valid values are 0 and 1;
//!   `char` is four bytes whose valid values are `0..=0x10FFFF` (the surrogates are inside the
//!   range: the niche used is the values above it); `!` is zero-sized and uninhabited.
//! - **Pointers**: a pointer to a sized type is one word; to a slice or `str` two words, the
//!   address at offset 0 and the length at offset 8 (Reference, "Pointers and references
//!   layout": "pointers to unsized types are ... twice the size of `usize`"). A reference is
//!   never null (its niche is 0); a raw pointer may hold any value. A fn pointer is one word and
//!   never null. A trait object pointer would be (address, vtable): the portable type form has
//!   no `dyn` node yet, so none is laid out.
//! - **Size is a multiple of alignment** (Reference: "The size of a value is always a multiple of
//!   its alignment"), and no layout is past `isize::MAX` bytes ([`Refusal::TooLarge`]).
//! - **Arrays**: `[T; N]` is `N` elements `size_of::<T>()` apart (the stride is the size).
//!   **Slices** `[T]` and `str` are unsized: their sized prefix is 0 bytes and their tail
//!   [`Tail::Slice`].
//! - **`repr(Rust)` structs and tuples** (blaze's own rule; Rust leaves the order unspecified):
//!   fields are placed in *decreasing alignment*, ties in declaration order, each at the next
//!   offset that is a multiple of its alignment; an unsized last field stays last. For
//!   power-of-two alignments this leaves no padding between fields. Offsets are always asked by
//!   declaration index ([`Layouts::field_offset`]).
//! - **`repr(C)`** (Reference, "The C Representation"): declaration order, each field at the
//!   next multiple of its alignment; alignment the most of the fields'; size rounded up.
//! - **`repr(packed(n))`**: each field's alignment is capped at `n`, and so is the struct's; the
//!   fields keep declaration order (blaze does not reorder a packed struct).
//! - **`repr(align(n))`**: the alignment is raised to at least `n`; the size rounds up to it.
//! - **`repr(transparent)`**: the layout of the one field that is not zero-sized (its scalar and
//!   niche too); every field is at offset 0.
//! - **Unions**: every field at offset 0; size the largest field's rounded to the alignment, the
//!   most of the fields'; no niche (any bit pattern may be valid).
//! - **Zero-sized types**: a struct or tuple of no non-zero-sized fields is zero-sized, its
//!   alignment the most of its fields' (1 when none).
//! - **Niches**: a scalar whose valid values are a proper range has a niche (its invalid
//!   values). An aggregate's niche is the one of its fields' with the most invalid values (ties:
//!   the lowest offset); an array's is its first element's. `#[rustc_layout_scalar_valid_range_*]`
//!   ([`LayoutFacts::valid_range`]) narrows the valid range of a struct around one scalar (or the
//!   first half of a wide pointer): this is how `core` declares `NonNull` and the `NonZero`
//!   integers. `UnsafeCell` hides its field's niche ([`ReprAttrs::hide_niche`]): a value behind a
//!   shared reference may be changed, so no outer type may read meaning into its bytes.
//! - **Enums, `repr(Rust)`** (blaze's rule, which keeps every layout Rust guarantees):
//!   - A variant is *absent* when it is uninhabited and all its fields are zero-sized: it is
//!     never built, so it needs no bytes and no tag value of its own.
//!   - No variant present: zero-sized and uninhabited. One present: the layout of that
//!     variant as a struct, no tag.
//!   - Otherwise the smaller of two candidates (the niche one when equal):
//!     *tagged*: the tag at offset 0, the smallest integer holding every discriminant (signed if
//!     one is negative), then each variant's fields after it in *increasing* alignment (so small
//!     fields fill the bytes next to the tag);
//!     *niche*: when every present variant but one (the largest, ties the first) is zero-sized,
//!     that variant is laid out as a struct and the others are encoded in invalid values of its
//!     niche: variants `first..=last` (every variant but the untagged one lies in that range)
//!     take the values `start + (v - first)`, wrapping at the niche's width, where `start` is
//!     the value after the niche's valid range.
//!   This gives the null-pointer optimisation Rust guarantees (std's `option` module doc,
//!   "Representation"): `Option<T>` has `T`'s size and `None` is all zeros at the pointer for
//!   `&T`, `&mut T`, `Box<T>`, `NonNull<T>`, `fn` pointers and `NonZero*`; tested by name.
//! - **Enums, `repr(C)` and `repr(<int>)`** (RFC 2195, the Reference's "Primitive
//!   representation of enums with fields"): a field-less enum is its tag (`repr(C)`: C's `int`,
//!   32 bits; refused when a discriminant does not fit); with fields, `repr(<int>)` is a union of
//!   `repr(C)` structs each starting with the tag, `repr(C)` (and `repr(C, <int>)`) a
//!   `repr(C)` struct of the tag then a union of `repr(C)` structs of the variants' fields. No
//!   niche is used to encode such an enum, but its tag's invalid values are a niche for an
//!   outer type.
//! - **Discriminants**: explicit (`= n`, [`LayoutFacts::discriminant`]) or one more than the
//!   previous variant's, the first 0.
//!
//! - **`repr(simd)`** (layout/simd.rs): a vector is its one array field `[E; N]` aligned to its
//!   size, 8 or 16 bytes (AAPCS64's short vectors); any other vector is refused by name.
//!
//! Refused by name, never guessed ([`Refusal`]): closures (their captures are
//! not in the portable form), types with no ground form (a parameter, an alias, an opaque
//! type, a hole), a type that holds itself by value, a `repr(C)` discriminant past C's `int`,
//! a valid-range attribute on a struct that is not one scalar.
//!
//! The walk over types uses a work stack: no recursion on the input.

use core::alloc::Allocator;
use core::fmt;

use blaze_own::Alloc;
use blaze_typeck::{Adt, AdtKind, ExprId, FloatTy, IntTy, ItemKey, Name, PNode, PTy, PortableTys, UintTy};

use crate::{BuildError, ground};

pub mod simd;
mod simd_rule;
#[cfg(test)]
pub(crate) mod tests;

/// The target's pointer size in bytes.
pub const PTR_BYTES: u64 = 8;
/// log2 of the pointer alignment.
const PTR_ALIGN: u8 = 3;
/// The largest size a layout may have: `isize::MAX` on a 64-bit target.
pub const MAX_SIZE: u64 = i64::MAX as u64;

/// The `repr` attributes of an ADT, as the world reads them.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct ReprAttrs {
    pub c: bool,
    /// `repr(u8)`, `repr(i32)`, ...: an enum's tag type.
    pub int: Option<IntRepr>,
    pub transparent: bool,
    /// `repr(packed(n))`: log2 of `n` (`repr(packed)` is 0).
    pub packed: Option<u8>,
    /// `repr(align(n))`: log2 of `n`.
    pub align: Option<u8>,
    /// `repr(simd)`: a vector, its one array field at the vector alignment (layout/simd.rs,
    /// layout/simd_rule.rs).
    pub simd: bool,
    /// The type hides its fields' niches: `UnsafeCell` (the `unsafe_cell` lang item) and
    /// `UnsafePinned`.
    pub hide_niche: bool,
}

impl ReprAttrs {
    /// No attribute: `repr(Rust)`.
    pub const RUST: ReprAttrs =
        ReprAttrs { c: false, int: None, transparent: false, packed: None, align: None, simd: false, hide_niche: false };
}

/// An integer `repr`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum IntRepr {
    Signed(IntTy),
    Unsigned(UintTy),
}

/// `#[rustc_layout_scalar_valid_range_start(s)]` and `..._end(e)`, as written.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct ValidRange {
    pub start: Option<u128>,
    pub end: Option<u128>,
}

/// What layout reads from outside the type: the ADTs' definitions and attributes. A function of
/// the records: equal questions, equal answers.
pub trait LayoutFacts {
    fn adt(&self, key: ItemKey) -> Option<Adt<'_>>;
    fn repr(&self, key: ItemKey) -> ReprAttrs;
    /// The explicit discriminant `= n` of variant `variant` of enum `key`, evaluated; `None`
    /// when it is not written (it is then one more than the previous variant's).
    /// `Err(Missing)` when that value's record is not made yet: the layout then refuses with
    /// [`Refusal::Missing`] and caches nothing.
    fn discriminant(&self, key: ItemKey, variant: u32) -> Result<Option<i128>, Missing>;
    fn valid_range(&self, key: ItemKey) -> ValidRange;

    /// The projection `<args[0] as trait_<args[1..]>>::name` (as `PNode::Alias` holds it) at
    /// concrete arguments, normalized by the front end's solver: `args` are types of `ground`
    /// with no parameter and no projection inside; the answer is interned in `ground` and holds
    /// no projection (the build still checks, and normalizes one it meets again: normalize.rs).
    /// The default, and an answer the solver cannot give: [`crate::BuildError::Projection`]
    /// naming it. The Reference, "Associated Items", "Associated types". Here, not on `Adts`,
    /// because layout grounds a field of projection type too and reads only these facts; the
    /// walk reaches it through `Adts::facts`.
    fn normalize(&self, trait_: ItemKey, name: Name, args: &[PTy], ground: &mut PortableTys<'_>) -> Result<PTy, crate::BuildError> {
        let _ = (args, ground);
        Err(crate::BuildError::Projection { trait_, name })
    }

    /// The facts of closure `ordinal` of `owner`'s body (typeck.md, "A closure's identity";
    /// codegen.md, "Closures and the Fn traits"): its kind, signature and environment, in the
    /// owner's generic parameters, made from the owner's check record by [`closure_facts`].
    /// `None`: not answered, and the build refuses naming the closure.
    fn closure(&self, owner: ItemKey, ordinal: u32) -> Option<ClosureFacts<'_>> {
        let _ = (owner, ordinal);
        None
    }

    /// The facts of coroutine `ordinal` of `owner`'s body (coroutine.rs, the module doc: the
    /// `async` blocks of the body in expression order): its upvars' tuple and its output, in
    /// the owner's generic parameters, made from the owner's body and check record by
    /// [`crate::coroutine_facts`]. `None`: not answered, and the layout refuses naming it.
    fn coroutine(&self, owner: ItemKey, ordinal: u32) -> Option<crate::CoroutineFacts<'_>> {
        let _ = (owner, ordinal);
        None
    }

    /// The state of coroutine `ordinal` of `owner` at the owner's ground arguments `args`
    /// (types of `ground`): [`crate::StateShape::of`] the layout `blaze_lir::coroutine::split`
    /// returned for that instance's built body ([`crate::CoroutineOf`] names it). `Ok(None)`:
    /// not answered, refused by name. `Err(Missing)`: that body is not built and split yet;
    /// the layout refuses with [`Refusal::Missing`] and caches nothing, and is asked again
    /// once the record exists.
    fn coroutine_state(&self, owner: ItemKey, ordinal: u32, args: &[PTy], ground: blaze_typeck::PTys<'_>) -> Result<Option<crate::StateShape>, Missing> {
        let _ = (owner, ordinal, args, ground);
        Ok(None)
    }

    /// The tag's valid range in the state of that instance ([`crate::niche_of`] the layout the
    /// pass returned, kept beside [`LayoutFacts::coroutine_state`]'s shape): its other values
    /// are the state's niche, which an enclosing enum (`Option<impl Future>`) uses (ASYNC.md
    /// D-A7). `Ok(None)`: not answered; the state then offers no niche. `Err(Missing)`: as
    /// `coroutine_state`. A world answers both from one record or neither, so one type has one
    /// layout in a build.
    fn coroutine_niche(&self, owner: ItemKey, ordinal: u32, args: &[PTy], ground: blaze_typeck::PTys<'_>) -> Result<Option<crate::StateNiche>, Missing> {
        let _ = (owner, ordinal, args, ground);
        Ok(None)
    }

    /// The hidden type of `impl Trait` type `opaque` at `args` (the opaque's generic arguments,
    /// types of `ground`): interned in `ground`, ground, with no projection and no opaque
    /// inside, as [`LayoutFacts::normalize`] answers. The Reference, "Impl trait", "Abstract
    /// return types": the type is "an anonymous type" the fn's body fixes (the check record's
    /// `Checked::hidden`). An `async fn`'s is its coroutine, made with
    /// [`crate::coroutine_identity`] at the ordinal of the fn body's root block
    /// ([`crate::coroutines_in`]: not 0 when the body holds other `async` blocks) with these
    /// arguments (the Reference, "Async functions"). The default, and an opaque the world
    /// cannot answer: refused naming the form.
    fn opaque_hidden(&self, opaque: ItemKey, args: &[PTy], ground: &mut PortableTys<'_>) -> Result<PTy, crate::BuildError> {
        let _ = (opaque, args, ground);
        Err(crate::BuildError::UnsupportedSig { what: "an `impl Trait` type in an instance (the world's `opaque_hidden` did not answer)" })
    }
}

/// A closure's facts, of its owner's generic parameters (`Param(i)`) in `types`.
#[derive(Clone, Copy)]
pub struct ClosureFacts<'a> {
    pub types: blaze_typeck::PTys<'a>,
    /// The checker's kind (`Checked::closures`): the trait method that is its body.
    pub kind: crate::FnTrait,
    /// Its signature, a `FnPtr`: the inputs, then the output.
    pub sig: PTy,
    /// Its ENVIRONMENT: a tuple with one field per capture in the checker's order
    /// (`Checked::captures`): `&T` for `ByRef`, `&mut T` for `ByMutRef`, `T` for `ByValue`.
    pub env: PTy,
}

/// The facts of closure `ordinal` of `body`, whose record is `checked`, written into `out` (the
/// seam's own table; `generics` the owner's parameter count, parents first): the kind and
/// signature the checker recorded (`Checked::closures[ordinal]`), and the environment from its
/// captures ([`environment`]). Every type is copied AS WRITTEN ([`crate::copy_written`]): an
/// `impl Trait` capture, a projection in the signature and an `async` block the closure returns
/// stay what the record says, settled when an instance grounds them with the world's facts. The
/// answer of [`LayoutFacts::closure`]; it holds no expression id of the body.
pub fn closure_facts(
    body: &blaze_typeck::BodyView<'_>,
    checked: &blaze_typeck::Checked<'_>,
    ordinal: u32,
    generics: u32,
    out: &mut PortableTys<'_>,
    scratch: Alloc<'_>,
) -> Result<(crate::FnTrait, PTy, PTy), BuildError> {
    let full = |_| BuildError::Malformed { what: "a closure's facts table is full" };
    let row = ordinal as usize;
    let rows = &checked.closures;
    if row >= rows.len() {
        return Err(BuildError::Malformed { what: "a closure ordinal past its owner's `Checked::closures`" });
    }
    let (e, kind, sig) = (rows.column::<0>()[row], rows.column::<1>()[row], rows.column::<2>()[row]);
    let mut params: Vec<PTy, Alloc<'_>> = Vec::with_capacity_in(generics as usize, scratch);
    for i in 0..generics {
        params.push(out.intern(PNode::Param(i)).map_err(full)?);
    }
    let sig = crate::copy_written(checked.types.view(), sig, &params, body, out, scratch)?;
    let env = environment(body, checked, e, &params, out, scratch)?;
    let kind = match kind {
        blaze_typeck::ClosureKind::Fn => crate::FnTrait::Fn,
        blaze_typeck::ClosureKind::FnMut => crate::FnTrait::FnMut,
        blaze_typeck::ClosureKind::FnOnce => crate::FnTrait::FnOnce,
    };
    Ok((kind, sig, env))
}

/// The captures of closure or `async` block `e` of the body `checked` records, in the checker's
/// order (the rows of `Checked::captures` whose first column is `e`): the binding and how. THE
/// ONE LIST an environment is made of. Field `j` of the environment IS capture `j` here: the
/// facts' environment type ([`environment`], for [`closure_facts`] and `coroutine_facts`) and the
/// walk's every write and read of a capture (closure.rs `captures_of`, coroutine.rs) take it from
/// this function, so the two cannot disagree on a count or an order.
pub fn captures<'c>(checked: &'c blaze_typeck::Checked<'_>, e: ExprId) -> impl Iterator<Item = (blaze_typeck::PatId, blaze_typeck::CaptureKind)> + 'c {
    let caps = &checked.captures;
    (0..caps.len()).filter(move |&c| caps.column::<0>()[c] == e).map(move |c| (caps.column::<1>()[c], caps.column::<2>()[c]))
}

/// The ENVIRONMENT of closure or `async` block `e`: a tuple of one field per [`captures`] row,
/// in its order (the Reference, "Closure types": "approximately equivalent to a struct which
/// contains the captured values"), `&T` for `ByRef`, `&mut T` for `ByMutRef`, `T` for
/// `ByValue`, each binding's type copied as written into `out` at `params`.
pub(crate) fn environment(
    body: &blaze_typeck::BodyView<'_>,
    checked: &blaze_typeck::Checked<'_>,
    e: ExprId,
    params: &[PTy],
    out: &mut PortableTys<'_>,
    scratch: Alloc<'_>,
) -> Result<PTy, BuildError> {
    let full = |_| BuildError::Malformed { what: "an environment's facts table is full" };
    let from = checked.types.view();
    let mut fields: Vec<PTy, Alloc<'_>> = Vec::new_in(scratch);
    for (p, by) in captures(checked, e) {
        let lt = *checked.local_ty.get(p.ix()).ok_or(BuildError::Malformed { what: "a captured binding without a type" })?;
        let t = crate::copy_written(from, lt, params, body, out, scratch)?;
        fields.push(match by {
            blaze_typeck::CaptureKind::ByRef => out.intern(PNode::Ref(blaze_typeck::Mutbl::Not, t)).map_err(full)?,
            blaze_typeck::CaptureKind::ByMutRef => out.intern(PNode::Ref(blaze_typeck::Mutbl::Mut, t)).map_err(full)?,
            blaze_typeck::CaptureKind::ByValue => t,
        });
    }
    let l = out.intern_list(&fields).map_err(full)?;
    out.intern(PNode::Tuple(l)).map_err(full)
}

/// What a layout reads and where it grounds field types.
pub struct LayoutCx<'g, 't> {
    pub ground: &'g mut PortableTys<'t>,
    pub facts: &'g dyn LayoutFacts,
    pub scratch: Alloc<'g>,
}

/// A primitive's kind and width.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Prim {
    Int { bytes: u8, signed: bool },
    F16,
    F32,
    F64,
    F128,
    Ptr,
}

impl Prim {
    pub const fn size(self) -> u64 {
        match self {
            Prim::Int { bytes, .. } => bytes as u64,
            Prim::F16 => 2,
            Prim::F32 => 4,
            Prim::F64 => 8,
            Prim::F128 => 16,
            Prim::Ptr => PTR_BYTES,
        }
    }

    /// log2 of the alignment: the size on this target, `i128` and `f128` included.
    pub const fn align(self) -> u8 {
        self.size().trailing_zeros() as u8
    }

    pub const fn bits(self) -> u32 {
        (self.size() * 8) as u32
    }
}

/// The all-ones value of `bits` bits.
pub const fn mask(bits: u32) -> u128 {
    if bits >= 128 { u128::MAX } else { (1u128 << bits) - 1 }
}

/// An inclusive range of a scalar's values that may wrap around (`start > end`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct WrappingRange {
    pub start: u128,
    pub end: u128,
}

impl WrappingRange {
    pub const fn full(bits: u32) -> WrappingRange {
        WrappingRange { start: 0, end: mask(bits) }
    }

    pub const fn contains(self, v: u128, bits: u32) -> bool {
        let m = mask(bits);
        (v.wrapping_sub(self.start) & m) <= (self.end.wrapping_sub(self.start) & m)
    }

    /// How many values lie outside the range.
    pub const fn invalid(self, bits: u32) -> u128 {
        let m = mask(bits);
        let span = self.end.wrapping_sub(self.start) & m;
        // The valid count is span + 1; the invalid count 2^bits - span - 1 = m - span.
        m - span
    }
}

/// A primitive and its valid values.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Scalar {
    pub prim: Prim,
    pub valid: WrappingRange,
}

impl Scalar {
    pub const fn full(prim: Prim) -> Scalar {
        Scalar { prim, valid: WrappingRange::full(prim.bits()) }
    }
}

/// Invalid values of a scalar at `offset`, which an enum may use to encode its variants.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Niche {
    pub offset: u64,
    pub scalar: Scalar,
}

impl Niche {
    fn of(offset: u64, scalar: Scalar) -> Option<Niche> {
        (scalar.valid.invalid(scalar.prim.bits()) > 0).then_some(Niche { offset, scalar })
    }

    pub const fn available(&self) -> u128 {
        self.scalar.valid.invalid(self.scalar.prim.bits())
    }

    /// Takes `count` invalid values just past the valid range: the first value taken, and the
    /// scalar with them counted valid.
    fn reserve(&self, count: u128) -> Option<(u128, Scalar)> {
        if count == 0 || count > self.available() {
            return None;
        }
        let bits = self.scalar.prim.bits();
        let m = mask(bits);
        let start = self.scalar.valid.end.wrapping_add(1) & m;
        let end = self.scalar.valid.end.wrapping_add(count) & m;
        Some((start, Scalar { prim: self.scalar.prim, valid: WrappingRange { start: self.scalar.valid.start, end } }))
    }
}

/// A layout's row in [`Layouts`].
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct LayoutId(pub u32);

/// Where a layout's fields are.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Fields {
    /// A primitive: no fields.
    Primitive,
    /// `count` elements of `elem`, `stride` apart (a slice: `count` is 0 and the tail says it).
    Array { elem: LayoutId, stride: u64, count: u64 },
    /// Fields at the offsets `Layouts::field_offset` gives, by declaration index (a wide
    /// pointer: the address at 0, the metadata at 8).
    Struct { first: u32, len: u32 },
    /// `count` fields, all at offset 0.
    Union { count: u32 },
}

/// How an enum's variant is told.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TagEncoding {
    /// The tag holds the variant's discriminant, truncated to its width.
    Direct,
    /// Variants `first..=last` other than `untagged` are the values `start + (v - first)`
    /// (wrapping) of the niche scalar; every other value means `untagged`.
    Niche { untagged: u32, first: u32, last: u32, start: u128 },
}

/// A layout's variants.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Variants {
    /// Not an enum, or an enum with one present variant: no tag.
    Single { index: u32 },
    /// An enum with a tag: its variants' layouts are `Layouts::variant`.
    Multiple { tag: Scalar, tag_offset: u64, encoding: TagEncoding, first: u32, count: u32 },
}

/// An unsized layout's tail.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Tail {
    Sized,
    /// A slice of elements `stride` bytes apart (`str`: 1), after the sized prefix.
    Slice { stride: u64 },
    /// A trait object (`dyn Tr`, codegen.md "Trait objects"): its size and alignment are not
    /// the type's but the value's, words 1 and 2 of the vtable its pointer carries.
    Dyn,
}

/// The static part of a layout ending in a trait object ([`Layouts::dyn_prefix`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct DynPrefix {
    /// Where the sized prefix ends: the trait object's offset before it is rounded up to the
    /// vtable's alignment.
    pub tail_at: u64,
    /// log2 of the sized prefix's alignment (`repr(align)` included).
    pub align: u8,
}

/// A type's layout.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Layout {
    /// The type (of the ground table); a variant's row holds its enum's type.
    pub ty: PTy,
    /// The size; of an unsized layout, the offset where its tail starts, not rounded up to the
    /// alignment (`Layouts::size_of_val` rounds once, after adding the tail).
    pub size: u64,
    /// log2 of the alignment.
    pub align: u8,
    pub tail: Tail,
    /// No value of it exists.
    pub uninhabited: bool,
    /// The value is one scalar at offset 0 of the whole size.
    pub scalar: Option<Scalar>,
    /// The value is two scalars at offsets 0 and 8 (a wide pointer, or a newtype of one).
    pub pair: Option<(Scalar, Scalar)>,
    pub fields: Fields,
    pub variants: Variants,
    /// The niche with the most invalid values.
    pub niche: Option<Niche>,
    /// An enum's discriminants, by variant: `Layouts::discriminant`.
    discrs: (u32, u32),
}

impl Layout {
    pub fn is_zst(&self) -> bool {
        self.size == 0 && self.tail == Tail::Sized
    }

    pub fn align_bytes(&self) -> u64 {
        1u64 << self.align
    }
}

/// Why a type has no layout.
#[derive(Debug)]
pub enum Refusal {
    /// A type blaze does not lay out yet, named.
    Unsupported(&'static str),
    /// A type the ground table or a world should not hold.
    Malformed(&'static str),
    /// The type holds itself by value: its size is infinite.
    Recursive,
    /// Past `isize::MAX` bytes.
    TooLarge,
    /// The world does not know this ADT.
    NoAdt(ItemKey),
    /// Grounding a field's type was refused.
    Ground(BuildError),
    /// Grounding the type of field `field` (its row in `Adt::fields`, named `name`) of ADT
    /// `adt` was refused: `error` says why (a projection no impl answers, an opaque type).
    FieldGround { adt: ItemKey, field: u32, name: Name, error: BuildError },
    /// A record the layout reads is not made yet (an explicit discriminant's value): nothing
    /// was cached, so asking again once it exists lays the type out afresh.
    Missing(Missing),
}

/// A fact [`LayoutFacts`] cannot answer yet: the record of `item` (an enum whose explicit
/// discriminant is not evaluated) is not made.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Missing {
    pub item: ItemKey,
}

/// A type with no layout: the type of the ground table, and why.
#[derive(Debug)]
pub struct LayoutError {
    pub ty: PTy,
    pub why: Refusal,
}

impl fmt::Display for LayoutError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let t = self.ty.0;
        match &self.why {
            Refusal::Unsupported(what) => write!(f, "type {t}: {what} is not laid out yet"),
            Refusal::Malformed(what) => write!(f, "type {t}: a malformed type or definition: {what}"),
            Refusal::Recursive => write!(f, "type {t} holds itself by value (an infinite size)"),
            Refusal::TooLarge => write!(f, "type {t} is larger than `isize::MAX` bytes"),
            Refusal::NoAdt(key) => write!(f, "type {t}: the world does not know ADT {:#x}", key.0),
            Refusal::Ground(error) => write!(f, "type {t}: grounding a field's type: {error}"),
            Refusal::FieldGround { adt, field, name, error } => {
                write!(f, "type {t}: field {field} (name {:#x}) of ADT {:#x}: grounding its type: {error}", name.0, adt.0)
            }
            Refusal::Missing(m) => write!(f, "type {t}: the record of item {:#x} (an explicit discriminant) is not made yet", m.item.0),
        }
    }
}

impl core::error::Error for LayoutError {}

/// What to store to make a variant: `value` as `scalar` at `offset`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct TagWrite {
    pub offset: u64,
    pub scalar: Scalar,
    pub value: u128,
}

const NONE: u32 = u32::MAX;
const BUSY: u32 = u32::MAX - 1;

/// The order a struct-like layout places its fields in.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Order {
    Declaration,
    Decreasing,
    Increasing,
}

/// A struct-like layout before it is a row.
struct Uni {
    first: u32,
    len: u32,
    size: u64,
    align: u8,
    tail: Tail,
    uninhabited: bool,
    niche: Option<Niche>,
    scalar: Option<Scalar>,
    pair: Option<(Scalar, Scalar)>,
}

/// The layouts of one program's ground types, made on demand.
pub struct Layouts<A: Allocator + Clone> {
    rows: Vec<Layout, A>,
    /// By ground type: its row, `NONE`, or `BUSY` while its parts are laid out.
    by_ty: Vec<u32, A>,
    offsets: Vec<u64, A>,
    variant_rows: Vec<LayoutId, A>,
    discrs: Vec<i128, A>,
    alloc: A,
}

fn align_up(at: u64, align: u8) -> Result<u64, Refusal> {
    let a = 1u64 << align;
    at.checked_add(a - 1).map(|v| v & !(a - 1)).filter(|&v| v <= MAX_SIZE).ok_or(Refusal::TooLarge)
}

fn int_prim(i: IntTy) -> Prim {
    let bytes = match i {
        IntTy::I8 => 1,
        IntTy::I16 => 2,
        IntTy::I32 => 4,
        IntTy::I64 | IntTy::Isize => 8,
        IntTy::I128 => 16,
    };
    Prim::Int { bytes, signed: true }
}

fn uint_prim(u: UintTy) -> Prim {
    let bytes = match u {
        UintTy::U8 => 1,
        UintTy::U16 => 2,
        UintTy::U32 => 4,
        UintTy::U64 | UintTy::Usize => 8,
        UintTy::U128 => 16,
    };
    Prim::Int { bytes, signed: false }
}

/// The smallest integer holding `min..=max`: signed when `min` is negative.
fn fit_int(min: i128, max: i128) -> Prim {
    let signed = min < 0;
    for bytes in [1u8, 2, 4, 8] {
        let bits = u32::from(bytes) * 8;
        let fits = if signed {
            min >= -(1i128 << (bits - 1)) && max < (1i128 << (bits - 1))
        } else {
            max < (1i128 << bits)
        };
        if fits {
            return Prim::Int { bytes, signed };
        }
    }
    Prim::Int { bytes: 16, signed }
}

/// Whether `v` fits integer `prim`.
fn fits(prim: Prim, v: i128) -> bool {
    let Prim::Int { bytes, signed } = prim else { return false };
    let bits = u32::from(bytes) * 8;
    if bits == 128 {
        return signed || v >= 0;
    }
    if signed { v >= -(1i128 << (bits - 1)) && v < (1i128 << (bits - 1)) } else { v >= 0 && v < (1i128 << bits) }
}

/// The better of two niches: more invalid values, then the lower offset.
fn better(a: Option<Niche>, b: Option<Niche>) -> Option<Niche> {
    match (a, b) {
        (Some(x), Some(y)) => {
            if y.available() > x.available() || (y.available() == x.available() && y.offset < x.offset) {
                Some(y)
            } else {
                Some(x)
            }
        }
        (x, None) => x,
        (None, y) => y,
    }
}

impl<A: Allocator + Clone> Layouts<A> {
    pub fn new_in(alloc: A) -> Layouts<A> {
        Layouts {
            rows: Vec::new_in(alloc.clone()),
            by_ty: Vec::new_in(alloc.clone()),
            offsets: Vec::new_in(alloc.clone()),
            variant_rows: Vec::new_in(alloc.clone()),
            discrs: Vec::new_in(alloc.clone()),
            alloc,
        }
    }

    /// The allocator the table's columns are in.
    pub fn alloc(&self) -> A {
        self.alloc.clone()
    }

    pub fn get(&self, id: LayoutId) -> &Layout {
        &self.rows[id.0 as usize]
    }

    /// The offset of field `field` (by declaration index) of a struct, tuple, union, wide
    /// pointer or variant row.
    pub fn field_offset(&self, id: LayoutId, field: u32) -> Option<u64> {
        match self.rows.get(id.0 as usize)?.fields {
            Fields::Struct { first, len } if field < len => self.offsets.get((first + field) as usize).copied(),
            Fields::Union { count } if field < count => Some(0),
            Fields::Array { stride, count, .. } if u64::from(field) < count => Some(stride * u64::from(field)),
            _ => None,
        }
    }

    /// Variant `v` of an enum with a tag: its fields at their offsets in the enum. An enum of
    /// one present variant answers for that variant with its own row.
    pub fn variant(&self, id: LayoutId, v: u32) -> Option<LayoutId> {
        match self.rows.get(id.0 as usize)?.variants {
            Variants::Multiple { first, count, .. } if v < count => self.variant_rows.get((first + v) as usize).copied(),
            Variants::Single { index } if index == v => Some(id),
            _ => None,
        }
    }

    /// The discriminant of variant `v` of an enum (`discriminant_value`, an `as` cast).
    pub fn discriminant(&self, id: LayoutId, v: u32) -> Option<i128> {
        let (first, len) = self.rows.get(id.0 as usize)?.discrs;
        (v < len).then(|| self.discrs[(first + v) as usize])
    }

    /// What to store to make variant `v`; `None` when nothing is stored (one variant, or the
    /// niche encoding's untagged variant, whose own fields hold valid values there).
    pub fn tag_write(&self, id: LayoutId, v: u32) -> Option<TagWrite> {
        let l = self.rows.get(id.0 as usize)?;
        let Variants::Multiple { tag, tag_offset, encoding, .. } = l.variants else { return None };
        let m = mask(tag.prim.bits());
        let value = match encoding {
            TagEncoding::Direct => (self.discriminant(id, v)? as u128) & m,
            TagEncoding::Niche { untagged, first, last, start } => {
                if v == untagged || v < first || v > last {
                    return None;
                }
                start.wrapping_add(u128::from(v - first)) & m
            }
        };
        Some(TagWrite { offset: tag_offset, scalar: tag, value })
    }

    /// Where the tag is read: its offset and scalar.
    pub fn read_tag(&self, id: LayoutId) -> Option<(u64, Scalar)> {
        match self.rows.get(id.0 as usize)?.variants {
            Variants::Multiple { tag, tag_offset, .. } => Some((tag_offset, tag)),
            Variants::Single { .. } => None,
        }
    }

    /// The variant whose tag reads `bits`; `None` for a direct tag no variant has (UB to hold).
    pub fn variant_of_tag(&self, id: LayoutId, bits: u128) -> Option<u32> {
        let l = self.rows.get(id.0 as usize)?;
        match l.variants {
            Variants::Single { index } => Some(index),
            Variants::Multiple { tag, encoding, count, .. } => {
                let m = mask(tag.prim.bits());
                match encoding {
                    TagEncoding::Direct => {
                        (0..count).find(|&v| self.discriminant(id, v).is_some_and(|d| (d as u128) & m == bits & m))
                    }
                    TagEncoding::Niche { untagged, first, last, start } => {
                        let rel = bits.wrapping_sub(start) & m;
                        if rel <= u128::from(last - first) { Some(first + rel as u32) } else { Some(untagged) }
                    }
                }
            }
        }
    }

    /// The size of a value of layout `id` whose metadata (an unsized tail's length) is `len`.
    /// `None` for a trait object: its size is its vtable's word 1, read at run time. A slice or
    /// `str` tail: rustc's `size_and_align_of_dst`, `round_up(tail_at + len * stride, align)`,
    /// where `tail_at` is the row's unrounded `size` (`univariant`) and `align` already holds
    /// the element's alignment.
    pub fn size_of_val(&self, id: LayoutId, len: u64) -> Option<u64> {
        let l = self.rows.get(id.0 as usize)?;
        match l.tail {
            Tail::Sized => Some(l.size),
            Tail::Slice { stride } => {
                let tail_at = l.size;
                let end = stride.checked_mul(len)?.checked_add(tail_at)?;
                align_up(end, l.align).ok()
            }
            Tail::Dyn => None,
        }
    }

    /// What of a trait object or a struct ending in one (`ArcInner<dyn Tr>`) is static: the
    /// tail's offset before it is rounded up to the vtable's alignment, and the alignment of
    /// the sized prefix. `None` for a row that does not end in a trait object. A bare `dyn Tr`
    /// is the empty prefix (offset 0, alignment 1). Its run-time size and alignment are
    /// `dyn_tail::size_and_align`.
    pub fn dyn_prefix(&self, id: LayoutId) -> Option<DynPrefix> {
        let l = self.rows.get(id.0 as usize)?;
        if l.tail != Tail::Dyn {
            return None;
        }
        let tail_at = match l.fields {
            Fields::Struct { len: 0, .. } => 0,
            Fields::Struct { first, len } => *self.offsets.get((first + len - 1) as usize)?,
            Fields::Primitive | Fields::Union { .. } | Fields::Array { .. } => return None,
        };
        Some(DynPrefix { tail_at, align: l.align })
    }

    fn slot(&mut self, t: PTy) -> &mut u32 {
        let at = t.0 as usize;
        if self.by_ty.len() <= at {
            self.by_ty.resize(at + 1, NONE);
        }
        &mut self.by_ty[at]
    }

    fn done(&self, t: PTy) -> Option<LayoutId> {
        match self.by_ty.get(t.0 as usize).copied() {
            Some(NONE) | Some(BUSY) | None => None,
            Some(id) => Some(LayoutId(id)),
        }
    }

    fn push(&mut self, l: Layout) -> Result<LayoutId, Refusal> {
        let id = u32::try_from(self.rows.len()).ok().filter(|&n| n < BUSY).ok_or(Refusal::TooLarge)?;
        self.rows.push(l);
        Ok(LayoutId(id))
    }

    /// The layout of ground type `ty`, the types it holds by value first, by a work stack.
    pub fn of(&mut self, ty: PTy, cx: &mut LayoutCx<'_, '_>) -> Result<LayoutId, LayoutError> {
        if let Some(id) = self.done(ty) {
            return Ok(id);
        }
        let mut todo: Vec<PTy, A> = Vec::new_in(self.alloc.clone());
        todo.push(ty);
        let walked = self.of_stack(&mut todo, cx);
        if walked.is_err() {
            // A refusal caches nothing: the types still on the stack are marked busy, and a
            // later `of` must lay them out afresh (a missing record may be made by then).
            for &t in todo.iter() {
                if self.by_ty.get(t.0 as usize) == Some(&BUSY) {
                    *self.slot(t) = NONE;
                }
            }
        }
        walked?;
        self.done(ty).ok_or(LayoutError { ty, why: Refusal::Malformed("a layout that was not made") })
    }

    /// `of`'s work stack: lays out the top of `todo` until it is empty, or stops at a refusal
    /// with the types being laid out still on it.
    fn of_stack(&mut self, todo: &mut Vec<PTy, A>, cx: &mut LayoutCx<'_, '_>) -> Result<(), LayoutError> {
        let mut parts: Vec<PTy, A> = Vec::new_in(self.alloc.clone());
        while let Some(&t) = todo.last() {
            if self.done(t).is_some() {
                todo.pop();
                continue;
            }
            *self.slot(t) = BUSY;
            let err = |why| LayoutError { ty: t, why };
            parts.clear();
            self.parts(t, cx, &mut parts).map_err(err)?;
            let mut missing = None;
            for &p in parts.iter() {
                if self.done(p).is_none() {
                    if self.by_ty.get(p.0 as usize) == Some(&BUSY) {
                        return Err(err(Refusal::Recursive));
                    }
                    missing = Some(p);
                    break;
                }
            }
            if let Some(p) = missing {
                todo.push(p);
                continue;
            }
            let id = self.compute(t, cx, &parts).map_err(err)?;
            *self.slot(t) = id.0;
            todo.pop();
        }
        Ok(())
    }

    /// The types `ty` holds by value, appended to `out`, each laid out: a struct's, tuple's or
    /// union's fields by declaration index, an array's or slice's element once, an enum's
    /// variants' fields one variant after another; nothing for a primitive.
    pub fn field_types(&mut self, ty: PTy, cx: &mut LayoutCx<'_, '_>, out: &mut Vec<PTy, A>) -> Result<(), LayoutError> {
        let from = out.len();
        self.parts(ty, cx, out).map_err(|why| LayoutError { ty, why })?;
        for i in from..out.len() {
            self.of(out[i], cx)?;
        }
        Ok(())
    }

    /// The types `t` holds by value, whose layouts its own needs: in declaration order, an
    /// enum's variants one after another.
    fn parts(&mut self, t: PTy, cx: &mut LayoutCx<'_, '_>, out: &mut Vec<PTy, A>) -> Result<(), Refusal> {
        let view = cx.ground.view();
        let node = view.node(t).map_err(|_| Refusal::Malformed("a type past the ground table"))?;
        match node {
            PNode::Tuple(l) => out.extend_from_slice(view.list(l).map_err(|_| Refusal::Malformed("a list past the ground table"))?),
            PNode::Array(e, _) | PNode::Slice(e) | PNode::Pat(e, _) => out.push(e),
            // A closure (codegen.md, "Closures and the Fn traits"): a struct of one field, its
            // environment, a fact of its identity grounded at the owner's arguments.
            PNode::Closure { owner, ordinal, args: l } => {
                let facts = cx.facts;
                let c = facts.closure(owner, ordinal).ok_or(Refusal::Unsupported("a closure whose facts the world did not answer"))?;
                let mut args: Vec<PTy, A> = Vec::new_in(self.alloc.clone());
                args.extend_from_slice(view.list(l).map_err(|_| Refusal::Malformed("a list past the ground table"))?);
                out.push(ground(c.types, c.env, &args, cx.ground, cx.scratch, facts).map_err(Refusal::Ground)?);
            }
            // A coroutine's state (coroutine.rs): its upvars' tuple first, a fact of its
            // identity grounded at the owner's arguments; the rest is the pass's (`compute`).
            PNode::Coroutine { .. } => {
                let facts = cx.facts;
                let of = crate::coroutine_of(view, t).ok_or(Refusal::Malformed("a coroutine node `coroutine_of` does not read"))?;
                let c = facts.coroutine(of.owner, of.ordinal).ok_or(Refusal::Unsupported("a coroutine whose facts the world did not answer"))?;
                let mut args: Vec<PTy, A> = Vec::new_in(self.alloc.clone());
                args.extend_from_slice(view.list(of.owner_args).map_err(|_| Refusal::Malformed("a list past the ground table"))?);
                out.push(ground(c.types, c.env, &args, cx.ground, cx.scratch, facts).map_err(Refusal::Ground)?);
            }
            PNode::Adt(key, l) => {
                let facts = cx.facts;
                let adt = facts.adt(key).ok_or(Refusal::NoAdt(key))?;
                let mut args: Vec<PTy, A> = Vec::new_in(self.alloc.clone());
                args.extend_from_slice(view.list(l).map_err(|_| Refusal::Malformed("a list past the ground table"))?);
                if adt.generics as usize != args.len() {
                    return Err(Refusal::Malformed("an ADT type whose argument count is not its generic count"));
                }
                for (i, f) in adt.fields.iter().enumerate() {
                    let named = |error| Refusal::FieldGround { adt: key, field: i as u32, name: f.name, error };
                    out.push(ground(adt.types, f.ty, &args, cx.ground, cx.scratch, cx.facts).map_err(named)?);
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn compute(&mut self, t: PTy, cx: &mut LayoutCx<'_, '_>, parts: &[PTy]) -> Result<LayoutId, Refusal> {
        let node = cx.ground.view().node(t).map_err(|_| Refusal::Malformed("a type past the ground table"))?;
        let prim = |p: Prim, valid: Option<WrappingRange>| {
            let scalar = Scalar { prim: p, valid: valid.unwrap_or(WrappingRange::full(p.bits())) };
            Layout {
                ty: t,
                size: p.size(),
                align: p.align(),
                tail: Tail::Sized,
                uninhabited: false,
                scalar: Some(scalar),
                pair: None,
                fields: Fields::Primitive,
                variants: Variants::Single { index: 0 },
                niche: Niche::of(0, scalar),
                discrs: (0, 0),
            }
        };
        let zst = |uninhabited| Layout {
            ty: t,
            size: 0,
            align: 0,
            tail: Tail::Sized,
            uninhabited,
            scalar: None,
            pair: None,
            fields: Fields::Struct { first: 0, len: 0 },
            variants: Variants::Single { index: 0 },
            niche: None,
            discrs: (0, 0),
        };
        let nonnull = Some(WrappingRange { start: 1, end: mask(64) });
        let layout = match node {
            PNode::Bool => prim(Prim::Int { bytes: 1, signed: false }, Some(WrappingRange { start: 0, end: 1 })),
            PNode::Char => prim(Prim::Int { bytes: 4, signed: false }, Some(WrappingRange { start: 0, end: 0x10FFFF })),
            PNode::Int(i) => prim(int_prim(i), None),
            PNode::Uint(u) => prim(uint_prim(u), None),
            PNode::Float(f) => prim(
                match f {
                    FloatTy::F16 => Prim::F16,
                    FloatTy::F32 => Prim::F32,
                    FloatTy::F64 => Prim::F64,
                    FloatTy::F128 => Prim::F128,
                },
                None,
            ),
            PNode::Never => zst(true),
            PNode::FnDef(..) => zst(false),
            PNode::Future(..) => {
                return Err(Refusal::Unsupported(
                    "an async block's future named by its expression in a ground type (only its identity, `PNode::Coroutine`, crosses a body: coroutine.rs)",
                ));
            }
            PNode::FnPtr(..) => prim(Prim::Ptr, nonnull),
            // A pattern type (typeck.md, "Pattern types"): its base's layout, its scalar (a
            // pointer's address, for a wide one) valid only in the node's one canonical range;
            // so an enclosing enum finds the niche (`Option<NonZero<u32>>` is 4 bytes).
            PNode::Pat(_, l) => {
                let base = *self.get(self.done(parts[0]).ok_or(Refusal::Malformed("a pattern type's base not laid out"))?);
                let view = cx.ground.view();
                let values = view.list(l).map_err(|_| Refusal::Malformed("a list past the ground table"))?;
                let bits = |v: PTy| match view.node(v) {
                    Ok(PNode::ConstValue { bits, .. }) => Ok(bits),
                    _ => Err(Refusal::Malformed("a pattern type's range end that is not a value")),
                };
                let valid = match values {
                    [start, end] => WrappingRange { start: bits(*start)?, end: bits(*end)? },
                    _ => return Err(Refusal::Unsupported("a pattern type of other than one range")),
                };
                // The range replaces the base's, so the base's must be full (an integer's, a raw
                // pointer's address): a `bool`, `char` or reference base would be widened. Its
                // ends are kept to the scalar's width (a signed bound may arrive sign-extended).
                let narrowed = |s: Scalar| -> Result<Scalar, Refusal> {
                    let bits = s.prim.bits();
                    if s.valid != WrappingRange::full(bits) {
                        return Err(Refusal::Unsupported("a pattern type over a scalar that has its own valid range"));
                    }
                    let m = mask(bits);
                    Ok(Scalar { prim: s.prim, valid: WrappingRange { start: valid.start & m, end: valid.end & m } })
                };
                match (base.scalar, base.pair) {
                    (Some(s), _) => {
                        let scalar = narrowed(s)?;
                        Layout { ty: t, scalar: Some(scalar), niche: Niche::of(0, scalar), ..base }
                    }
                    (None, Some((addr, len))) => {
                        let addr = narrowed(addr)?;
                        Layout { ty: t, pair: Some((addr, len)), niche: Niche::of(0, addr), ..base }
                    }
                    (None, None) => return Err(Refusal::Unsupported("a pattern type over a base that is not a scalar")),
                }
            }
            PNode::Ref(_, p) | PNode::Ptr(_, p) => {
                let is_ref = matches!(node, PNode::Ref(..));
                let addr = Scalar { prim: Prim::Ptr, valid: if is_ref { nonnull.unwrap_or(WrappingRange::full(64)) } else { WrappingRange::full(64) } };
                if let Some(tail) = self.pointee_tail(p, cx)? {
                    // The metadata: a length, or a trait object's vtable address (codegen.md,
                    // "Trait objects"); the niche stays in the data word, as a thin pointer's.
                    let len = match tail {
                        Tail::Dyn => Scalar::full(Prim::Ptr),
                        Tail::Slice { .. } | Tail::Sized => Scalar::full(Prim::Int { bytes: 8, signed: false }),
                    };
                    let first = self.offsets.len() as u32;
                    self.offsets.extend_from_slice(&[0, PTR_BYTES]);
                    Layout {
                        ty: t,
                        size: 2 * PTR_BYTES,
                        align: PTR_ALIGN,
                        tail: Tail::Sized,
                        uninhabited: false,
                        scalar: None,
                        pair: Some((addr, len)),
                        fields: Fields::Struct { first, len: 2 },
                        variants: Variants::Single { index: 0 },
                        niche: Niche::of(0, addr),
                        discrs: (0, 0),
                    }
                } else {
                    prim(Prim::Ptr, Some(addr.valid))
                }
            }
            PNode::Str | PNode::Slice(_) => {
                let (stride, align, elem) = match node {
                    PNode::Slice(_) => {
                        let e = self.done(parts[0]).ok_or(Refusal::Malformed("an element not laid out"))?;
                        let el = self.get(e);
                        if el.tail != Tail::Sized {
                            return Err(Refusal::Malformed("a slice of an unsized element"));
                        }
                        (el.size, el.align, e)
                    }
                    _ => (1, 0, LayoutId(NONE)),
                };
                Layout {
                    ty: t,
                    size: 0,
                    align,
                    tail: Tail::Slice { stride },
                    uninhabited: false,
                    scalar: None,
                    pair: None,
                    fields: Fields::Array { elem, stride, count: 0 },
                    variants: Variants::Single { index: 0 },
                    niche: None,
                    discrs: (0, 0),
                }
            }
            PNode::Array(_, n) => {
                let n = cx.ground.view().value_u64(n).ok_or(Refusal::Malformed("an array length that is not a value"))?;
                let e = self.done(parts[0]).ok_or(Refusal::Malformed("an element not laid out"))?;
                let el = *self.get(e);
                if el.tail != Tail::Sized {
                    return Err(Refusal::Malformed("an array of an unsized element"));
                }
                let size = el.size.checked_mul(n).filter(|&s| s <= MAX_SIZE).ok_or(Refusal::TooLarge)?;
                Layout {
                    ty: t,
                    size,
                    align: el.align,
                    tail: Tail::Sized,
                    uninhabited: n > 0 && el.uninhabited,
                    scalar: None,
                    pair: None,
                    fields: Fields::Array { elem: e, stride: el.size, count: n },
                    variants: Variants::Single { index: 0 },
                    niche: if n > 0 { el.niche } else { None },
                    discrs: (0, 0),
                }
            }
            PNode::Tuple(_) | PNode::Closure { .. } => {
                let u = self.univariant(parts, Order::Decreasing, ReprAttrs::RUST, 0, 0)?;
                self.struct_row(t, u)
            }
            PNode::Adt(key, _) => return self.adt(t, key, cx, parts),
            // A coroutine's state (coroutine.rs): a struct of one field, its upvars' tuple at
            // offset 0, in the size and alignment the pass gave the instance's built body. No
            // scalar form: the bytes past the upvars are the pass's. Its niche is the tag's
            // invalid values (coroutine_niche.rs), when the world answers the tag's range.
            PNode::Coroutine { .. } => {
                let view = cx.ground.view();
                let of = crate::coroutine_of(view, t).ok_or(Refusal::Malformed("a coroutine node `coroutine_of` does not read"))?;
                let args = view.list(of.owner_args).map_err(|_| Refusal::Malformed("a list past the ground table"))?;
                let state = match cx.facts.coroutine_state(of.owner, of.ordinal, args, view) {
                    Err(missing) => return Err(Refusal::Missing(missing)),
                    Ok(None) => return Err(Refusal::Unsupported("a coroutine whose state the world did not answer")),
                    Ok(Some(state)) => state,
                };
                let u = self.univariant(parts, Order::Decreasing, ReprAttrs::RUST, 0, 0)?;
                if u64::from(state.size) < u.size || state.align < u.align || u64::from(state.tag_offset) < u.size {
                    return Err(Refusal::Malformed("a coroutine's state smaller than its upvars"));
                }
                let mut row = self.struct_row(t, u);
                row.size = u64::from(state.size);
                row.align = state.align;
                row.scalar = None;
                row.pair = None;
                row.niche = match cx.facts.coroutine_niche(of.owner, of.ordinal, args, view) {
                    Err(missing) => return Err(Refusal::Missing(missing)),
                    Ok(None) => None,
                    Ok(Some(n)) => match crate::coroutine_niche::state_row_niche(n, state) {
                        Ok(niche) => niche,
                        Err(refused) => return Err(Refusal::Malformed(refused.what())),
                    },
                };
                row
            }
            // codegen.md "Trait objects": unsized, its size and alignment the vtable's words 1
            // and 2; no field is visible through it.
            PNode::Dynamic(_) => Layout {
                ty: t,
                size: 0,
                align: 0,
                tail: Tail::Dyn,
                uninhabited: false,
                scalar: None,
                pair: None,
                fields: Fields::Struct { first: self.offsets.len() as u32, len: 0 },
                variants: Variants::Single { index: 0 },
                niche: None,
                discrs: (0, 0),
            },
            PNode::ExTrait(..) | PNode::ExProj { .. } | PNode::ExAuto(_) => {
                return Err(Refusal::Malformed("a trait object's predicate row as a type"));
            }
            PNode::Param(_) | PNode::Alias { .. } | PNode::Opaque(..) | PNode::Infer | PNode::Error => {
                return Err(Refusal::Malformed("a type with no ground form"));
            }
            PNode::ConstValue { .. } | PNode::ConstParam(_) => {
                return Err(Refusal::Malformed("a const generic argument where a type is laid out"));
            }
        };
        self.push(layout)
    }

    fn struct_row(&self, t: PTy, u: Uni) -> Layout {
        Layout {
            ty: t,
            size: u.size,
            align: u.align,
            tail: u.tail,
            uninhabited: u.uninhabited,
            scalar: u.scalar,
            pair: u.pair,
            fields: Fields::Struct { first: u.first, len: u.len },
            variants: Variants::Single { index: 0 },
            niche: u.niche,
            discrs: (0, 0),
        }
    }

    /// Whether a pointer to `p` is wide, and with which metadata: its pointee is a slice or
    /// `str` (a length) or a trait object (a vtable), or ends in one; `None` when it is thin.
    /// Only the kind is read: a slice's stride here is a placeholder, its row has the real one.
    fn pointee_tail(&mut self, mut p: PTy, cx: &mut LayoutCx<'_, '_>) -> Result<Option<Tail>, Refusal> {
        // Each step goes to a strictly smaller type: at most the table's length of steps.
        for _ in 0..=cx.ground.len() {
            let view = cx.ground.view();
            match view.node(p).map_err(|_| Refusal::Malformed("a type past the ground table"))? {
                PNode::Str => return Ok(Some(Tail::Slice { stride: 1 })),
                PNode::Slice(_) => return Ok(Some(Tail::Slice { stride: 0 })),
                PNode::Dynamic(_) => return Ok(Some(Tail::Dyn)),
                PNode::Tuple(l) => match view.list(l).map_err(|_| Refusal::Malformed("a list past the ground table"))?.last() {
                    Some(&last) => p = last,
                    None => return Ok(None),
                },
                PNode::Adt(key, l) => {
                    let facts = cx.facts;
                    let adt = facts.adt(key).ok_or(Refusal::NoAdt(key))?;
                    if adt.kind != AdtKind::Struct {
                        return Ok(None);
                    }
                    let Some(last) = adt.fields.last() else { return Ok(None) };
                    let mut args: Vec<PTy, A> = Vec::new_in(self.alloc.clone());
                    args.extend_from_slice(view.list(l).map_err(|_| Refusal::Malformed("a list past the ground table"))?);
                    let field = adt.fields.len() as u32 - 1;
                    let named = |error| Refusal::FieldGround { adt: key, field, name: last.name, error };
                    p = ground(adt.types, last.ty, &args, cx.ground, cx.scratch, cx.facts).map_err(named)?;
                }
                PNode::Param(_) | PNode::Alias { .. } | PNode::Opaque(..) | PNode::Infer | PNode::Error => {
                    return Err(Refusal::Malformed("a pointee with no ground form"));
                }
                _ => return Ok(None),
            }
        }
        Err(Refusal::Malformed("a pointee tail that does not end"))
    }

    /// Fields `parts` (by declaration) placed in `order` from offset `start`, the alignment at
    /// least `min_align`, with `attrs`' packing and alignment. A sized result's size is rounded
    /// up to the alignment; an unsized one's is its tail's offset, unrounded.
    fn univariant(&mut self, parts: &[PTy], order: Order, attrs: ReprAttrs, start: u64, min_align: u8) -> Result<Uni, Refusal> {
        let mut ids: Vec<LayoutId, A> = Vec::with_capacity_in(parts.len(), self.alloc.clone());
        for &p in parts {
            ids.push(self.done(p).ok_or(Refusal::Malformed("a field not laid out"))?);
        }
        for (i, id) in ids.iter().enumerate() {
            if self.get(*id).tail != Tail::Sized && i + 1 != ids.len() {
                return Err(Refusal::Malformed("an unsized field that is not the last"));
            }
        }
        let field_align = |l: &Layout| match attrs.packed {
            Some(pack) => l.align.min(pack),
            None => l.align,
        };
        // The placement order: the unsized tail, if any, stays last.
        let mut place: Vec<u32, A> = Vec::with_capacity_in(ids.len(), self.alloc.clone());
        place.extend(0..ids.len() as u32);
        let sortable = match ids.last() {
            Some(last) if self.get(*last).tail != Tail::Sized => ids.len() - 1,
            _ => ids.len(),
        };
        let order = if attrs.c || attrs.packed.is_some() { Order::Declaration } else { order };
        match order {
            Order::Declaration => {}
            Order::Decreasing => place[..sortable].sort_by_key(|&i| core::cmp::Reverse(field_align(self.get(ids[i as usize])))),
            Order::Increasing => place[..sortable].sort_by_key(|&i| field_align(self.get(ids[i as usize]))),
        }
        let first = self.offsets.len() as u32;
        self.offsets.resize(self.offsets.len() + ids.len(), 0);
        let mut at = start;
        let mut align = min_align;
        let mut niche = None;
        let mut uninhabited = false;
        let mut tail = Tail::Sized;
        for &i in place.iter() {
            let l = *self.get(ids[i as usize]);
            if l.tail == Tail::Dyn {
                // `struct S { a: u8, t: dyn Tr }` (`ArcInner<dyn Tr>`): the trait object's offset
                // is the prefix's end rounded up to the vtable's alignment, a run-time value
                // (`dyn_tail`). Its row here is that end unrounded: `dyn`'s own alignment is 1
                // and its size 0, so the plain placement below puts it there.
                if !matches!(l.fields, Fields::Struct { len: 0, .. }) {
                    // `struct Outer { a: u8, inner: S<dyn Tr> }`: `inner`'s own offset depends
                    // on the vtable's alignment too, so a single rounding of a static offset
                    // is not its place.
                    return Err(Refusal::Unsupported(
                        "a struct whose last field is itself a struct ending in a trait object (`dyn`): the nested tail's offset needs two run-time roundings",
                    ));
                }
                if attrs.packed.is_some() {
                    // The Reference, `[layout.repr.alignment.packed-fields]`: the vtable's
                    // alignment would be capped by the packing, which `dyn_tail` does not read.
                    return Err(Refusal::Unsupported("a `repr(packed)` struct ending in a trait object (`dyn`)"));
                }
            }
            let a = field_align(&l);
            let off = align_up(at, a)?;
            self.offsets[(first + i) as usize] = off;
            at = off.checked_add(l.size).filter(|&s| s <= MAX_SIZE).ok_or(Refusal::TooLarge)?;
            align = align.max(a);
            uninhabited |= l.uninhabited;
            if l.tail != Tail::Sized {
                tail = l.tail;
            }
            niche = better(niche, l.niche.map(|n| Niche { offset: n.offset + off, scalar: n.scalar }));
        }
        if let Some(pack) = attrs.packed {
            align = align.min(pack);
        }
        if let Some(raise) = attrs.align {
            align = align.max(raise);
        }
        // An unsized layout's `size` is where its tail starts (`Layout::size`), never rounded:
        // rustc's `size_and_align_of_dst` rounds `tail_at + len * stride` once, so rounding the
        // prefix here first would answer 24, not 16, for `{ u64, u8, [u8] }` of length 3. The
        // tail's own row has size 0, so `at` is its offset, or, for a struct ending in an unsized
        // struct, that struct's offset plus its own tail's start.
        let size = if tail == Tail::Sized { align_up(at, align)? } else { at };
        // A newtype (one field of the whole size at offset 0, the rest zero-sized) carries its
        // field's scalar or pair.
        let mut scalar = None;
        let mut pair = None;
        let big: Vec<u32, A> = {
            let mut v = Vec::new_in(self.alloc.clone());
            v.extend((0..ids.len() as u32).filter(|&i| !self.get(ids[i as usize]).is_zst()));
            v
        };
        if let [only] = big[..] {
            let l = self.get(ids[only as usize]);
            if self.offsets[(first + only) as usize] == 0 && l.size == size && l.tail == Tail::Sized {
                scalar = l.scalar;
                pair = l.pair;
            }
        }
        Ok(Uni { first, len: ids.len() as u32, size, align, tail, uninhabited, niche, scalar, pair })
    }

    fn adt(&mut self, t: PTy, key: ItemKey, cx: &mut LayoutCx<'_, '_>, parts: &[PTy]) -> Result<LayoutId, Refusal> {
        let facts = cx.facts;
        let adt = facts.adt(key).ok_or(Refusal::NoAdt(key))?;
        let attrs = facts.repr(key);
        // A vector (layout/simd.rs): its array at the target's short-vector alignment.
        if attrs.simd {
            return self.simd(t, &adt, attrs, cx, parts);
        }
        match adt.kind {
            AdtKind::Struct => {
                let [variant] = adt.variants else { return Err(Refusal::Malformed("a struct without exactly one variant")) };
                let fields = parts.get(variant.fields.range()).ok_or(Refusal::Malformed("a struct's fields past their column"))?;
                let mut u = if attrs.transparent { self.transparent(fields)? } else { self.univariant(fields, Order::Decreasing, attrs, 0, 0)? };
                let range = facts.valid_range(key);
                if range.start.is_some() || range.end.is_some() {
                    let narrow = |s: Scalar| {
                        let m = mask(s.prim.bits());
                        Scalar { prim: s.prim, valid: WrappingRange { start: range.start.unwrap_or(s.valid.start) & m, end: range.end.unwrap_or(s.valid.end) & m } }
                    };
                    if let Some(s) = u.scalar {
                        let s = narrow(s);
                        u.scalar = Some(s);
                        u.niche = Niche::of(0, s);
                    } else if let Some((a, b)) = u.pair {
                        let a = narrow(a);
                        u.pair = Some((a, b));
                        u.niche = Niche::of(0, a);
                    } else {
                        return Err(Refusal::Unsupported("a valid-range attribute on a struct that is not one scalar or one wide pointer"));
                    }
                }
                if attrs.hide_niche {
                    u.niche = None;
                    u.scalar = u.scalar.map(|s| Scalar::full(s.prim));
                    u.pair = u.pair.map(|(a, b)| (Scalar::full(a.prim), Scalar::full(b.prim)));
                }
                let row = self.struct_row(t, u);
                self.push(row)
            }
            AdtKind::Union => {
                let mut size = 0u64;
                let mut align = 0u8;
                let mut ids: Vec<LayoutId, A> = Vec::new_in(self.alloc.clone());
                for &p in parts {
                    ids.push(self.done(p).ok_or(Refusal::Malformed("a field not laid out"))?);
                }
                for id in ids.iter() {
                    let l = self.get(*id);
                    if l.tail != Tail::Sized {
                        return Err(Refusal::Malformed("an unsized union field"));
                    }
                    size = size.max(l.size);
                    align = align.max(match attrs.packed {
                        Some(p) => l.align.min(p),
                        None => l.align,
                    });
                }
                if let Some(p) = attrs.packed {
                    align = align.min(p);
                }
                if let Some(raise) = attrs.align {
                    align = align.max(raise);
                }
                let size = align_up(size, align)?;
                self.push(Layout {
                    ty: t,
                    size,
                    align,
                    tail: Tail::Sized,
                    uninhabited: false,
                    scalar: None,
                    pair: None,
                    fields: Fields::Union { count: ids.len() as u32 },
                    variants: Variants::Single { index: 0 },
                    niche: None,
                    discrs: (0, 0),
                })
            }
            AdtKind::Enum => self.enum_(t, key, &adt, attrs, facts, parts),
        }
    }

    fn transparent(&mut self, fields: &[PTy]) -> Result<Uni, Refusal> {
        let mut inner = None;
        let mut align = 0u8;
        for &p in fields {
            let id = self.done(p).ok_or(Refusal::Malformed("a field not laid out"))?;
            let l = *self.get(id);
            align = align.max(l.align);
            if !l.is_zst() {
                if inner.is_some() {
                    return Err(Refusal::Malformed("a `repr(transparent)` struct with two non-zero-sized fields"));
                }
                inner = Some(l);
            }
        }
        let first = self.offsets.len() as u32;
        self.offsets.resize(self.offsets.len() + fields.len(), 0);
        Ok(match inner {
            Some(l) => Uni {
                first,
                len: fields.len() as u32,
                size: l.size,
                align: l.align,
                tail: l.tail,
                uninhabited: l.uninhabited,
                niche: l.niche,
                scalar: l.scalar,
                pair: l.pair,
            },
            None => Uni { first, len: fields.len() as u32, size: 0, align, tail: Tail::Sized, uninhabited: false, niche: None, scalar: None, pair: None },
        })
    }

    fn enum_(&mut self, t: PTy, key: ItemKey, adt: &Adt<'_>, attrs: ReprAttrs, facts: &dyn LayoutFacts, parts: &[PTy]) -> Result<LayoutId, Refusal> {
        let n = adt.variants.len();
        let count = u32::try_from(n).map_err(|_| Refusal::TooLarge)?;
        // Discriminants: explicit, or one more than the previous.
        let discr_first = self.discrs.len() as u32;
        let mut prev: Option<i128> = None;
        for v in 0..count {
            let written = facts.discriminant(key, v).map_err(|m| {
                self.discrs.truncate(discr_first as usize);
                Refusal::Missing(m)
            })?;
            let d = match written {
                Some(d) => d,
                None => match prev {
                    None => 0,
                    Some(p) => p.checked_add(1).ok_or(Refusal::Malformed("a discriminant past i128"))?,
                },
            };
            self.discrs.push(d);
            prev = Some(d);
        }
        let discrs = (discr_first, count);
        let ds = &self.discrs[discr_first as usize..];
        let (min, max) = (ds.iter().copied().min().unwrap_or(0), ds.iter().copied().max().unwrap_or(0));
        let fields_of = |v: usize| parts.get(adt.variants[v].fields.range()).ok_or(Refusal::Malformed("a variant's fields past their column"));

        if n == 0 {
            let mut l = self.zst_row(t, true);
            l.discrs = discrs;
            return self.push(l);
        }

        // repr(C) and repr(<int>): explicit tagged layouts (RFC 2195).
        if attrs.c || attrs.int.is_some() {
            let prim = match attrs.int {
                Some(IntRepr::Signed(i)) => int_prim(i),
                Some(IntRepr::Unsigned(u)) => uint_prim(u),
                None => Prim::Int { bytes: 4, signed: true },
            };
            if !fits(prim, min) || !fits(prim, max) {
                return Err(if attrs.int.is_none() {
                    Refusal::Unsupported("a `repr(C)` enum whose discriminant does not fit C's `int`")
                } else {
                    Refusal::Malformed("a discriminant past its `repr` integer")
                });
            }
            let m = mask(prim.bits());
            let tag = Scalar { prim, valid: WrappingRange { start: (min as u128) & m, end: (max as u128) & m } };
            let tag_size = prim.size();
            // `repr(C)` (with or without an int): the variants' structs start where their union
            // starts, after the tag at the union's alignment; `repr(<int>)` alone: each variant's
            // struct starts with the tag.
            let start = if attrs.c {
                let mut union_align = 0u8;
                for v in 0..n {
                    let u = self.univariant(fields_of(v)?, Order::Declaration, ReprAttrs { c: true, ..ReprAttrs::RUST }, 0, 0)?;
                    union_align = union_align.max(u.align);
                    self.offsets.truncate(u.first as usize);
                }
                align_up(tag_size, union_align)?
            } else {
                tag_size
            };
            let c_attrs = ReprAttrs { c: true, align: attrs.align, ..ReprAttrs::RUST };
            return self.tagged(t, n, tag, 0, start, c_attrs, Order::Declaration, &fields_of, discrs);
        }

        // repr(Rust).
        let mut present: Vec<bool, A> = Vec::with_capacity_in(n, self.alloc.clone());
        let mut zero: Vec<bool, A> = Vec::with_capacity_in(n, self.alloc.clone());
        for v in 0..n {
            let mut all_zst = true;
            let mut uninhabited = false;
            for &p in fields_of(v)? {
                let l = self.get(self.done(p).ok_or(Refusal::Malformed("a field not laid out"))?);
                all_zst &= l.is_zst();
                uninhabited |= l.uninhabited;
            }
            present.push(!(uninhabited && all_zst));
            zero.push(all_zst);
        }
        let present_count = present.iter().filter(|&&p| p).count();
        if present_count == 0 {
            let mut l = self.zst_row(t, true);
            l.discrs = discrs;
            return self.push(l);
        }
        if present_count == 1 {
            let v = present.iter().position(|&p| p).unwrap_or(0);
            let u = self.univariant(fields_of(v)?, Order::Decreasing, attrs, 0, 0)?;
            let mut l = self.struct_row(t, u);
            l.variants = Variants::Single { index: v as u32 };
            l.discrs = discrs;
            return self.push(l);
        }

        // The tagged candidate.
        let tag_prim = fit_int(min, max);
        let m = mask(tag_prim.bits());
        let tag = Scalar { prim: tag_prim, valid: WrappingRange { start: (min as u128) & m, end: (max as u128) & m } };
        let mark = (self.offsets.len(), self.variant_rows.len(), self.rows.len());
        let tagged = self.tagged(t, n, tag, 0, tag_prim.size(), attrs, Order::Increasing, &fields_of, discrs)?;
        let tagged_size = self.get(tagged).size;

        // The niche candidate: every present variant but the largest is zero-sized.
        let mut untagged = None;
        let mut best_size = 0u64;
        let mut others_zero = true;
        let mut sizes: Vec<u64, A> = Vec::with_capacity_in(n, self.alloc.clone());
        for v in 0..n {
            let u = self.univariant(fields_of(v)?, Order::Decreasing, attrs, 0, 0)?;
            sizes.push(u.size);
            self.offsets.truncate(u.first as usize);
        }
        for v in 0..n {
            if present[v] && (untagged.is_none() || sizes[v] > best_size) {
                untagged = Some(v);
                best_size = sizes[v];
            }
        }
        let Some(untagged) = untagged else { return Ok(tagged) };
        for v in 0..n {
            if v != untagged && present[v] && !zero[v] {
                others_zero = false;
            }
        }
        if !others_zero {
            return Ok(tagged);
        }
        let first_v = (0..n).find(|&v| v != untagged && present[v]);
        let last_v = (0..n).rev().find(|&v| v != untagged && present[v]);
        let (Some(first_v), Some(last_v)) = (first_v, last_v) else { return Ok(tagged) };
        let u = self.univariant(fields_of(untagged)?, Order::Decreasing, attrs, 0, 0)?;
        let Some(niche) = u.niche else { return Ok(tagged) };
        let Some((start, reserved)) = niche.reserve((last_v - first_v + 1) as u128) else { return Ok(tagged) };
        // The others' (zero-sized) fields are at offset 0; their alignment counts.
        let mut align = u.align;
        for v in 0..n {
            if v != untagged {
                for &p in fields_of(v)? {
                    align = align.max(self.get(self.done(p).ok_or(Refusal::Malformed("a field not laid out"))?).align);
                }
            }
        }
        let size = align_up(u.size, align)?;
        if size > tagged_size {
            return Ok(tagged);
        }
        // The niche layout wins: drop the tagged candidate's rows (the last ones made).
        let untagged_offsets: Vec<u64, A> = {
            let mut v = Vec::new_in(self.alloc.clone());
            v.extend_from_slice(&self.offsets[u.first as usize..(u.first + u.len) as usize]);
            v
        };
        self.offsets.truncate(mark.0);
        self.variant_rows.truncate(mark.1);
        self.rows.truncate(mark.2);
        let variants_first = self.variant_rows.len() as u32;
        let mut variant_ids: Vec<LayoutId, A> = Vec::with_capacity_in(n, self.alloc.clone());
        let enum_niche = Niche::of(niche.offset, reserved);
        for v in 0..n {
            let fs = fields_of(v)?;
            let first = self.offsets.len() as u32;
            if v == untagged {
                self.offsets.extend_from_slice(&untagged_offsets);
            } else {
                self.offsets.resize(self.offsets.len() + fs.len(), 0);
            }
            let mut uninhabited = false;
            for &p in fs {
                uninhabited |= self.get(self.done(p).ok_or(Refusal::Malformed("a field not laid out"))?).uninhabited;
            }
            let id = self.push(Layout {
                ty: t,
                size,
                align,
                tail: Tail::Sized,
                uninhabited,
                scalar: None,
                pair: None,
                fields: Fields::Struct { first, len: fs.len() as u32 },
                variants: Variants::Single { index: v as u32 },
                niche: if v == untagged { Niche::of(niche.offset, reserved) } else { None },
                discrs: (0, 0),
            })?;
            variant_ids.push(id);
        }
        self.variant_rows.extend_from_slice(&variant_ids);
        let scalar = if size == reserved.prim.size() && niche.offset == 0 { Some(reserved) } else { None };
        let pair = match u.pair {
            Some((a, b)) if niche.offset == 0 && a.prim == reserved.prim => Some((reserved, b)),
            _ => None,
        };
        self.push(Layout {
            ty: t,
            size,
            align,
            tail: Tail::Sized,
            uninhabited: false,
            scalar,
            pair,
            fields: Fields::Struct { first: 0, len: 0 },
            variants: Variants::Multiple {
                tag: reserved,
                tag_offset: niche.offset,
                encoding: TagEncoding::Niche { untagged: untagged as u32, first: first_v as u32, last: last_v as u32, start },
                first: variants_first,
                count: n as u32,
            },
            niche: enum_niche,
            discrs,
        })
    }

    fn zst_row(&self, t: PTy, uninhabited: bool) -> Layout {
        Layout {
            ty: t,
            size: 0,
            align: 0,
            tail: Tail::Sized,
            uninhabited,
            scalar: None,
            pair: None,
            fields: Fields::Struct { first: 0, len: 0 },
            variants: Variants::Single { index: 0 },
            niche: None,
            discrs: (0, 0),
        }
    }

    /// A tagged layout: the tag at `tag_offset`, each variant's fields placed from `start` in
    /// `order`.
    #[allow(clippy::too_many_arguments)]
    fn tagged<'p>(
        &mut self,
        t: PTy,
        n: usize,
        tag: Scalar,
        tag_offset: u64,
        start: u64,
        attrs: ReprAttrs,
        order: Order,
        fields_of: &dyn Fn(usize) -> Result<&'p [PTy], Refusal>,
        discrs: (u32, u32),
    ) -> Result<LayoutId, Refusal> {
        let tag_align = tag.prim.align();
        let mut unis: Vec<Uni, A> = Vec::with_capacity_in(n, self.alloc.clone());
        let mut size = 0u64;
        let mut align = tag_align;
        let mut uninhabited = true;
        for v in 0..n {
            let u = self.univariant(fields_of(v)?, order, ReprAttrs { align: None, ..attrs }, start, tag_align)?;
            size = size.max(u.size);
            align = align.max(u.align);
            uninhabited &= u.uninhabited;
            unis.push(u);
        }
        if let Some(raise) = attrs.align {
            align = align.max(raise);
        }
        let size = align_up(size, align)?;
        let first = self.variant_rows.len() as u32;
        let mut ids: Vec<LayoutId, A> = Vec::with_capacity_in(n, self.alloc.clone());
        for (v, u) in unis.iter().enumerate() {
            let id = self.push(Layout {
                ty: t,
                size,
                align,
                tail: Tail::Sized,
                uninhabited: u.uninhabited,
                scalar: None,
                pair: None,
                fields: Fields::Struct { first: u.first, len: u.len },
                variants: Variants::Single { index: v as u32 },
                niche: None,
                discrs: (0, 0),
            })?;
            ids.push(id);
        }
        self.variant_rows.extend_from_slice(&ids);
        let scalar = (size == tag.prim.size()).then_some(tag);
        self.push(Layout {
            ty: t,
            size,
            align,
            tail: Tail::Sized,
            uninhabited,
            scalar,
            pair: None,
            fields: Fields::Struct { first: 0, len: 0 },
            variants: Variants::Multiple { tag, tag_offset, encoding: TagEncoding::Direct, first, count: n as u32 },
            niche: Niche::of(tag_offset, tag),
            discrs,
        })
    }
}
