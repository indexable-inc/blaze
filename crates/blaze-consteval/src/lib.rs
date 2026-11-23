//! blaze's constant evaluator (docs/v2/plan/design/consteval.md): the values the language
//! requires at compile time (`const` items, `static` initialisers, array lengths and const
//! generic arguments, enum discriminants, inline `const` blocks) and the `const fn` calls in
//! them.
//!
//! It interprets the checker's typed body form ([`Typed`]: a [`BodyView`] and its
//! [`Checked`] record) with an explicit work stack, no recursion on the input, a step bound, a
//! memory bound and a call-depth bound ([`Limits`]). Memory has provenance ([`mem`]): an
//! address is an allocation and an offset. A value leaves as a record of bytes and relocations
//! ([`ConstRecord`]), keyed from text before the work ([`RecordKey`]).
//!
//! Every entry point is a pure function of its inputs that returns its result or the record
//! it lacks ([`Outcome::Need`]; docs/v2/plan/design/bottom.md, section 2): the driver gets it
//! and enters again from the top. [`Settle`] is the sans-IO loop a driver runs for that, which
//! names a cycle between constants.
//!
//! The seams:
//! - the checker: [`eval_len`] (an array length or a const generic argument) and
//!   [`eval_discriminant`];
//! - the code builder and the statics' data: [`value_of`] (a const, a static, an inline const).

#![no_std]
#![feature(allocator_api)]
#![forbid(unsafe_code)]

extern crate alloc;
#[cfg(any(test, feature = "testing"))]
extern crate std;

/// The integer and float rules on bits, shared with blaze-body-eval (rule 30).
pub mod arith;
mod drop_ask;
mod error;
pub mod layout;
mod lit;
mod machine;
pub mod mem;
mod record;
mod soft;

/// A world, bodies and a store written by hand, for the tests and `examples/cost.rs`.
#[cfg(any(test, feature = "testing"))]
pub mod fixture;
#[cfg(test)]
mod tests;

use blaze_ids::ContentId;
use blaze_own::Alloc;
use blaze_typeck::{
    AdjustKind, BodyView, Checked, Def, ExprId, Fetch, ItemKey, List, PTy, PTys, PatId, PortableTys, Range, UnionPat,
};

pub use error::{At, ConstError, Op, PanicPart, Refusal};
pub use layout::{IntRepr, Layout, LayoutCx, LayoutFacts, Layouts, Missing, ReprAttrs, TagWrite, ValidRange};
pub use lit::LitError;
pub use machine::{Answer, Codes, Need, NeedKind, Outcome, Settle, eval_discriminant, eval_len, value_of};
pub use record::{ConstFn, ConstRecord, DataAlloc, DataRecord, DataReloc, DataTarget, RecordKey};

/// The written text of every literal of a body, read where the literal was written: the item's
/// own text or a macro expansion's (blaze-lower's `View::text_at` of the literal's range). A
/// body's ranges are item-relative sites (rule 71), and a literal made by an expansion has the
/// macro call as its site, so a literal's value is never read at its range: only here.
#[derive(Clone, Copy, Debug, Default)]
pub struct Lits<'b> {
    /// The literals' texts, each at a range of this buffer.
    pub text: &'b str,
    /// Each literal expression, sorted, and its text's range in `text`.
    pub exprs: (&'b [ExprId], &'b [Range]),
    /// Each literal pattern (`Pat::Lit`, `Pat::NegLit`: the minus included), sorted.
    pub pats: (&'b [PatId], &'b [Range]),
}

impl<'b> Lits<'b> {
    /// The written text of literal expression `e`.
    pub fn expr(&self, e: ExprId) -> Option<&'b str> {
        let i = self.exprs.0.binary_search(&e).ok()?;
        let r = self.exprs.1[i];
        self.text.get(r.start as usize..r.end as usize)
    }

    /// The written text of literal pattern `p`, a leading minus included.
    pub fn pat(&self, p: PatId) -> Option<&'b str> {
        let i = self.pats.0.binary_search(&p).ok()?;
        let r = self.pats.1[i];
        self.text.get(r.start as usize..r.end as usize)
    }
}

/// One checked body as the evaluator reads it: the body form, its item's text, its literals'
/// texts ([`Lits`]), the text's hash (the body's part of every key that reads it), and the
/// columns of its check. Borrowed, never copied.
#[derive(Clone, Copy, Debug)]
pub struct Typed<'b> {
    pub body: BodyView<'b>,
    /// The item's text: `body`'s ranges are offsets into it.
    pub text: &'b str,
    /// Every literal's written text.
    pub lits: Lits<'b>,
    /// The content hash of the body's text.
    pub hash: ContentId,
    pub types: PTys<'b>,
    /// One per expression: its type before adjustments.
    pub expr_ty: &'b [PTy],
    /// One per pattern.
    pub pat_ty: &'b [PTy],
    /// One per pattern: a binding's type.
    pub local_ty: &'b [PTy],
    /// Adjustments, sorted by expression, each expression's in order: expression, step, the
    /// type it gives.
    pub adjusts: (&'b [ExprId], &'b [AdjustKind], &'b [PTy]),
    /// The resolved callee of each call: expression, fn item, its generic arguments.
    pub callees: (&'b [ExprId], &'b [ItemKey], &'b [List]),
    /// Each named field access: expression, variant, field index.
    pub fields: (&'b [ExprId], &'b [u32], &'b [u32]),
    /// What each value path and struct expression resolved to, sorted.
    pub expr_defs: (&'b [ExprId], &'b [Def]),
    /// What each path, tuple-struct and struct pattern resolved to, sorted.
    pub pat_defs: (&'b [PatId], &'b [Def]),
    /// The generic arguments each constant value path is read at, sorted by expression (a
    /// list of `types`; blaze-typeck `Checked::const_args`): `[Self, trait args..]` for
    /// `Self::NAME`, `T::NAME` or `<T as Tr>::NAME`, the impl's for an inherent impl's.
    pub const_args: (&'b [ExprId], &'b [List]),
    /// Each field a base-less struct expression `S { a: 1, .. }` takes from its default
    /// (RFC 3681; blaze-typeck `Checked::default_fields`), sorted by expression then field:
    /// the expression, the field's index in its variant, the default's key (a constant over
    /// the ADT's generics, read at the expression's type's arguments).
    pub default_fields: (&'b [ExprId], &'b [u32], &'b [ItemKey]),
    /// Each field a struct or record update `{ a: 1, ..base }` takes from its base, sorted by
    /// expression then field: the expression, the field's index, its type.
    pub fru_fields: (&'b [ExprId], &'b [u32], &'b [PTy]),
    /// Structural union pattern bindings (`UnionPat::Variant` / `UnionPat::Residual`), sorted by
    /// pattern id.
    pub union_pats: (&'b [PatId], &'b [UnionPat]),
}

impl<'b> Typed<'b> {
    /// The body `body`, checked as `checked`, of the item whose text is `text`.
    pub fn new(body: BodyView<'b>, checked: &'b Checked<'_>, text: &'b str, lits: Lits<'b>, hash: ContentId) -> Typed<'b> {
        Typed {
            body,
            text,
            lits,
            hash,
            types: checked.types.view(),
            expr_ty: &checked.expr_ty,
            pat_ty: &checked.pat_ty,
            local_ty: &checked.local_ty,
            adjusts: (checked.adjusts.column::<0>(), checked.adjusts.column::<1>(), checked.adjusts.column::<2>()),
            callees: (checked.callees.column::<0>(), checked.callees.column::<1>(), checked.callees.column::<2>()),
            fields: (checked.fields.column::<0>(), checked.fields.column::<1>(), checked.fields.column::<2>()),
            expr_defs: (checked.expr_defs.column::<0>(), checked.expr_defs.column::<1>()),
            pat_defs: (checked.pat_defs.column::<0>(), checked.pat_defs.column::<1>()),
            const_args: (checked.const_args.column::<0>(), checked.const_args.column::<1>()),
            default_fields: (checked.default_fields.column::<0>(), checked.default_fields.column::<1>(), checked.default_fields.column::<2>()),
            fru_fields: (checked.fru_fields.column::<0>(), checked.fru_fields.column::<1>(), checked.fru_fields.column::<2>()),
            union_pats: (checked.union_pats.column::<0>(), checked.union_pats.column::<1>()),
        }
    }
}

/// What calling a fn item does in a constant.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FnKind {
    /// A `const fn` with a body: interpreted.
    Const,
    /// Not `const`: refused (`NotConstFn`).
    NotConst,
    /// A `#[rustc_intrinsic]` fn whose name blaze's one intrinsic table does not hold: refused
    /// as such (`Refusal::UnknownIntrinsic`), a reason the unit's kept code keeps.
    UnknownIntrinsic,
    /// A tuple struct's or variant's constructor.
    Ctor { adt: ItemKey, variant: u32 },
    /// An intrinsic the evaluator computes itself.
    Intrinsic(Intrinsic),
    /// A panic entry of core (`panic`, `panic_str`, `panic_display`, `panic_fmt` and the
    /// `panic_const_*` of the overflow checks): the constant fails with the arguments as the
    /// message's parts.
    Panic,
    /// The world does not hold `Fact`, which the call's kind needs: refused as
    /// `Refusal::FnFact`, a pending refusal (code holding it is compiled again once the world
    /// answers). Never `NotConst`: "not const" is said only of a head read without `const`.
    Missing(Fact),
}

/// A fact about a called fn that the world does not hold (yet): [`FnKind::Missing`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fact {
    /// The callee's written text: the world holds none for it, so its head was not read.
    Text,
    /// The crate the callee is in, or that crate's edition or cfg.
    Crate,
    /// The callee's head is written and did not lower.
    Head,
    /// A `#[rustc_intrinsic]` callee's name.
    IntrinsicName,
    /// An impl's fn's signature, made on use: not made yet.
    Signature,
    /// A trait's fn: its trait's members, not made yet.
    TraitMembers,
    /// The callee's signature was refused by the front end (a gap of its own there).
    SignatureRefused,
    /// The callee's container: neither an impl nor a trait the world keys.
    Container,
    /// The callee is a member that is not a fn (a constant, a type) called as one.
    NotAFn,
    /// The callee has no origin the world knows: no item, no member, no row.
    Origin,
}

impl core::fmt::Display for Fact {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(match self {
            Fact::Text => "its head was not read: no text is held for it",
            Fact::Crate => "its head was not read: its crate, or the crate's edition or cfg, is not known",
            Fact::Head => "its head is written and did not lower",
            Fact::IntrinsicName => "it is a `#[rustc_intrinsic]` and no name is held for it",
            Fact::Signature => "it is an impl's fn whose signature is not made",
            Fact::TraitMembers => "it is a trait's fn whose trait's members are not made",
            Fact::SignatureRefused => "its signature was refused",
            Fact::Container => "it is a member of neither an impl nor a trait",
            Fact::NotAFn => "it is a member that is not a fn",
            Fact::Origin => "it has no library item, no member origin and no signature row",
        })
    }
}

/// The intrinsics, by blaze's one table (`blaze_build::intrinsics`, rule 30): a world answers
/// `FnKind::Intrinsic(Which::named(name)?)`. The evaluator computes the layout queries,
/// `transmute` of plain data (a pointer's bytes stay a pointer), the integer intrinsics
/// (`unchecked_*` overflow refused as `Overflow`), `offset` within its allocation,
/// `discriminant_value`, the hints, `ptr_metadata`, `aggregate_raw_ptr`,
/// `ptr_guaranteed_cmp` (2, "unknown", unless certain) and `const_eval_select` (its const
/// fn); any other row is refused by its name.
pub use blaze_build::intrinsics::Which as Intrinsic;

/// The result of looking up a method on a function lang trait.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FnTraitMethod {
    Found(ItemKey),
    Absent,
    /// The trait exists, but its members have not been loaded yet.
    Pending { trait_: ItemKey },
}

/// Everything the evaluator reads from outside the body. Implemented by the driver over the
/// item layer's and the checker's records; by [`fixture::World`] in the tests.
pub trait ConstWorld: LayoutFacts {
    /// The checked body of `item`: a const, a static, a `const fn`, an inline const, or an
    /// explicit discriminant's expression. `Missing`: not checked yet (the evaluation returns
    /// `Need`); `None`: the item has no body.
    fn body(&self, item: ItemKey) -> Fetch<Option<Typed<'_>>>;
    /// Called on the first execution of a body in this evaluation, including
    /// when a previous evaluation compiled its code.
    fn entered_body(&self, _item: ItemKey) {}
    /// What a call of fn item `item` does.
    fn fn_kind(&self, item: ItemKey) -> FnKind;
    /// The item that runs for an associated fn or const at ground arguments `args` (impl
    /// selection: `blaze_typeck::resolve_instance`); an inherent item resolves to itself.
    fn resolve(&self, item: ItemKey, args: List, ground: &mut PortableTys<'_>, scratch: Alloc<'_>) -> Option<(ItemKey, List)> {
        let _ = (ground, scratch);
        Some((item, args))
    }
    /// The code identity of an item (its text's id): part of every record key.
    fn code_id(&self, item: ItemKey) -> ContentId;
    /// Whether `item` is a `static` (or `static mut`): its record is then the static's own
    /// memory, and the allocations written through it are marked so (`DataAlloc::mutable`).
    fn is_static(&self, item: ItemKey) -> bool;
    /// The record of value `key`, if it has been made.
    fn record(&self, key: RecordKey) -> Fetch<&ConstRecord<Alloc<'_>>>;
    /// The body of variant `variant`'s explicit discriminant (`A = expr`), if it has one.
    fn explicit_discriminant(&self, adt: ItemKey, variant: u32) -> Option<ItemKey>;

    /// Whether ground ADT type `ty` (of `ground`) has a `Drop` impl: its `drop` fn, as
    /// `blaze_build::Adts::drop_impl` answers the builder (one solver call). Read by the
    /// `needs_drop` intrinsic (drop_ask.rs). `None`, the default: this world does not answer,
    /// and `needs_drop` of a type reaching an ADT is refused naming the ADT
    /// ([`Refusal::Missing`]); never a guessed "no impl".
    fn drop_impl(&self, ty: PTy, ground: &mut PortableTys<'_>) -> Option<Result<Option<ItemKey>, blaze_build::BuildError>> {
        let _ = (ty, ground);
        None
    }

    /// core's `ManuallyDrop` ADT (`#[lang = "manually_drop"]`), whose value is never dropped:
    /// `Some(None)` when the library declares none. `None`, the default: not answered, refused
    /// as [`ConstWorld::drop_impl`]'s default is.
    fn manually_drop(&self) -> Option<Option<ItemKey>> {
        None
    }

    /// The trait method `which` names (`Fn::call`, `FnMut::call_mut`, `FnOnce::call_once`), as
    /// `blaze_build::Adts::fn_trait_method` answers the builder (rule 30: the same item). A
    /// closure coerced to a fn pointer in a constant points to `FnOnce::call_once` at
    /// `[closure, (args..)]`, the instance blaze-build's `closure_fn_addr` builds.
    /// An absent method refuses the coercion. A pending lookup names the trait to load.
    fn fn_trait_method(&self, _which: blaze_build::FnTrait) -> FnTraitMethod {
        FnTraitMethod::Absent
    }

    /// The names `type_name` prints a type with (blaze-build type_name.rs): the paths of
    /// items. `None`, the default: `type_name` is refused by name, never printed without them.
    fn type_names(&self) -> Option<&dyn blaze_build::type_name::TypeNames> {
        None
    }
}

/// The bounds of one evaluation (rule 18): each passed is a typed refusal naming the constant.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    /// Work-stack steps: about one per expression evaluated.
    pub steps: u64,
    /// Live bytes of compile-time memory.
    pub memory: u64,
    /// Nested `const fn` calls.
    pub depth: u32,
}

impl Limits {
    /// The defaults: 20 million steps (about a second), 64 MiB, 512 calls deep.
    pub const DEFAULT: Limits = Limits { steps: 20_000_000, memory: 64 << 20, depth: 512 };
}
