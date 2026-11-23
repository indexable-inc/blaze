//! The record of one checked body, in columns (typeck.md, "Results: one record in columns"):
//! every type in the record's own hash-consed table, items by key, positions item-relative.
//! Per-node facts are one `Vec` per node kind; sparse facts (adjustments, callees, fields,
//! captures) are `Soa` tables sorted by expression, so a lookup scans only the expression column.

use blaze_own::{Alloc, Soa, Vec};

use crate::diag::Diag;
use crate::ids::{ExprId, ItemKey, List, Mutbl, PatId};
use crate::portable::{PTy, PortableTys};
use crate::ty::ClosureKind;
use crate::world::{Def, Needs, ReadList};

/// One adjustment step, applied to an expression's value in order (rustc's `Adjust`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum AdjustKind {
    /// `!` to any type.
    NeverToAny,
    /// `*x`: builtin when `overloaded` is `None`, else `Deref::deref` (`Not`) or
    /// `DerefMut::deref_mut` (`Mut`).
    Deref { overloaded: Option<Mutbl> },
    /// `&x` or `&mut x` (`raw`: `&raw const x` / `&raw mut x`); `two_phase` for an autoref of a
    /// method receiver or an argument.
    Borrow { mutbl: Mutbl, two_phase: bool, raw: bool },
    /// A fn item to a fn pointer.
    ReifyFnPointer,
    /// A closure that captures nothing to a fn pointer (the Reference, "Type coercions": "Non
    /// capturing closures to `fn` pointers"); the target is the pointer type, safe or `unsafe`.
    ClosureFnPointer,
    /// A safe fn pointer to the `unsafe` one of its ABI (`fn()` to `unsafe fn()`).
    UnsafeFnPointer,
    /// `*mut T` to `*const T`.
    MutToConstPointer,
    /// An unsizing coercion (`&[T; N]` to `&[T]`).
    Unsize,
    /// Member injection into a structural union (`A -> A | B`).
    UnionInject { variant: u32 },
    /// Sub-union widening (`A | B -> A | B | C`).
    UnionWiden,
    /// Structural record width-subtyping projection (`{ a, b } -> { a }`).
    RecordProject,
}

/// How a pattern binds or narrows a structural union (`PNode::Union`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum UnionPat {
    /// Matches member `variant` of the scrutinee's union and extracts its payload.
    Variant(u32),
    /// Catch-all residual arm narrowed to a multi-member sub-union of the scrutinee.
    Residual,
}

/// Structural union pattern bindings, sorted by pattern id.
pub type UnionPats<'a> = Soa<(PatId, UnionPat), Alloc<'a>>;

/// How a closure captures a local.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum CaptureKind {
    ByRef,
    ByMutRef,
    ByValue,
}

/// Adjustments: the expression, the step, the type it produces. Each expression's steps are
/// consecutive and in order.
pub type Adjusts<'a> = Soa<(ExprId, AdjustKind, PTy), Alloc<'a>>;
/// The resolved callee of each call and method call: the expression, the item, and its generic
/// arguments (a list of the record's `types`).
pub type Callees<'a> = Soa<(ExprId, ItemKey, List), Alloc<'a>>;
/// What each resolved value path (`Expr::Path`) and struct expression's path names, by
/// expression: a variant is `Def::Ctor` with its index, a struct `Def::Adt` (variant 0), a
/// type-relative `T::f` the picked associated item. A path that failed has no row.
pub type ExprDefs<'a> = Soa<(ExprId, Def), Alloc<'a>>;
/// The generic arguments each constant value path is read at, by expression (a list of the
/// record's `types`): `<T as Tr>::C` holds `[Self, trait args..]`, an inherent impl's constant
/// the impl's arguments. A constant with no generics (a free `const` item) has no row; the
/// item itself is the expression's `Def::Const` in `expr_defs`.
pub type ConstArgs<'a> = Soa<(ExprId, List), Alloc<'a>>;
/// The same for each constant pattern (`Pat::Path` naming a `Def::Const`), by pattern: an
/// inherent impl's constant at the impl's arguments (`U256::ZERO`). Never over a generic
/// parameter (E0158). Read with [`Checked::pat_const_args_of`].
pub type PatConstArgs<'a> = Soa<(PatId, List), Alloc<'a>>;
/// The same for `Pat::Path`, `Pat::TupleStruct` and `Pat::Struct`, by pattern: the variant a
/// `match` switches on.
pub type PatDefs<'a> = Soa<(PatId, Def), Alloc<'a>>;
/// Each field access: the expression, the variant and the field's index.
pub type Fields<'a> = Soa<(ExprId, u32, u32), Alloc<'a>>;
/// Each capture: the closure, the binding captured, how.
pub type Captures<'a> = Soa<(ExprId, PatId, CaptureKind), Alloc<'a>>;
/// Each captured PLACE of a closure or async block whose captures the place analysis decided
/// (capture_places.rs, RFC 2229), in the environment's field order: the closure, the place's
/// root binding, its projections ([`CaptureProjs`] rows `start..start + len`), how it is held,
/// and the field's type as stored (`T`, `&T` or `&mut T`). Sorted by closure, stable, so a
/// closure's rows are its fields in order. A closure the analysis did not decide has no row
/// here and is read from [`Captures`] (whole bindings). Read with [`Checked::capture_places_of`].
pub type CapturePlaceRows<'a> = Soa<(ExprId, PatId, u32, u32, CaptureKind, PTy), Alloc<'a>>;
/// The projections of every captured place, each place's a run: the kind (0 a deref, 1 a field
/// of a variant, 2 an index, never kept by the analysis), the variant, the field, and the type
/// after the projection.
pub type CaptureProjs<'a> = Soa<(u8, u32, u32, PTy), Alloc<'a>>;
/// One row per closure expression of the body, row `i` the closure of ordinal `i`
/// (`PNode::Closure`: closure expressions in `ExprId` order): its expression, its kind (the
/// strongest `Fn*` trait it implements), signature, exact captured-place tuple when known, and
/// where its signature's expectation came from ([`SigFrom`]).
pub type Closures<'a> = Soa<(ExprId, ClosureKind, PTy, Option<PTy>, SigFrom), Alloc<'a>>;

/// Where a closure's signature was expected from (D148, docs/v2/plan/design/flow.md "Lifetimes
/// are produced once and carried beside the types", piece L5). The erased signature in
/// [`Closures`] holds no region; the flow lowering reads a closure's regions from WHERE the
/// expectation came from, never from a copy kept here: a clause's region column says which
/// signature positions its `for<..>` binds (`Late`: universal in the closure's body), and every
/// other position is a region of the closure's maker.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SigFrom {
    /// A pending `Fn*` obligation registered by clause `nth` of the item the path or method call
    /// `expr` names (`Cause::Clause`): the inputs are that clause's argument tuple; the output is
    /// clause `output`'s term (the `FnOnce::Output` projection of the same use) when one was
    /// pending, else the body's own.
    Clause { expr: ExprId, nth: u32, output: Option<u32> },
    /// Every parameter's type and the return type written on the closure, no expectation used.
    Written,
    /// No expectation (or one the body itself made, not a declared clause): every region is the
    /// maker's.
    Inferred,
    /// Expected as a trait object (`Box<dyn Fn(&T)>`): refused by name by the flow at first.
    Object,
    /// Expected as a fn pointer (`let f: fn(&u8) = |x| ..`): refused by name by the flow at first.
    FnPtr,
}

/// Which information still separates the checked interior from the stored state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CoroutineWitness {
    /// Binding rows alone could not establish the captured places and their modes.
    NeedsCapturePlaces,
    /// Captures are known. Flow must determine the values saved at suspension.
    NeedsSavedLocals,
    /// The checked tuple is the complete state even without a flow record.
    Complete,
}
/// rustc's `fru_field_types`: each field a struct update `S { a: x, ..base }` takes from its
/// base (every field not written), sorted by the struct expression, each expression's rows in
/// field order: the expression, the field's index in the struct, its type at the struct's
/// arguments. Read with [`Checked::fru_fields_of`].
pub type FruFields<'a> = Soa<(ExprId, u32, PTy), Alloc<'a>>;
/// Each field a base-less struct expression `S { a: 1, .. }` (RFC 3681) takes from its
/// default, sorted by expression then field: the expression, the field's index in the
/// variant, the default's key (an anonymous constant of the field's type over the ADT's
/// generics, evaluated at the expression's type arguments). Each such field also has its
/// [`FruFields`] row (its type). Read with [`Checked::default_fields_of`].
pub type DefaultFields<'a> = Soa<(ExprId, u32, ItemKey), Alloc<'a>>;
/// The record of one body.
pub struct Checked<'a> {
    /// Every type the record names.
    pub types: PortableTys<'a>,
    /// One per expression: its type before adjustments.
    pub expr_ty: Vec<PTy, Alloc<'a>>,
    /// One per pattern.
    pub pat_ty: Vec<PTy, Alloc<'a>>,
    /// One per pattern: the binding's type for a binding pattern, the error type otherwise.
    pub local_ty: Vec<PTy, Alloc<'a>>,
    pub adjusts: Adjusts<'a>,
    pub callees: Callees<'a>,
    pub expr_defs: ExprDefs<'a>,
    /// Sorted by expression; read with [`Checked::const_args_of`].
    pub const_args: ConstArgs<'a>,
    /// Sorted by pattern; read with [`Checked::pat_const_args_of`].
    pub pat_const_args: PatConstArgs<'a>,
    pub pat_defs: PatDefs<'a>,
    /// Structural union pattern match/narrowing records, sorted by pattern; read with
    /// [`Checked::union_pat_of`].
    pub union_pats: UnionPats<'a>,
    pub fields: Fields<'a>,
    pub captures: Captures<'a>,
    /// The captured places of each decided closure ([`CapturePlaceRows`]).
    pub capture_places: CapturePlaceRows<'a>,
    /// Their projections ([`CaptureProjs`]).
    pub capture_projs: CaptureProjs<'a>,
    pub closures: Closures<'a>,
    /// The body's diagnostics, in the order found.
    pub diags: Vec<Diag, Alloc<'a>>,
    /// The steps of every finding's why-chain ([`Diag::why`]), each chain's steps together,
    /// root first; read with [`Checked::why_of`]. Empty with no diagnostic.
    pub why: Vec<crate::diag::WhyStep, Alloc<'a>>,
    /// What the check read, and what to fetch next time.
    pub reads: ReadList<'a>,
    pub needs: Needs<'a>,
    /// For each E0425 at a one-segment value name: the names in scope that could replace it
    /// (rules/suggest.rs; `suggest::pick` chooses the one fix with the item's text). Empty, and
    /// no work done, for a body where every name resolved.
    pub suggests: crate::rules::suggest::Suggests<'a>,
    /// For each return-position `impl Trait` (and an `async fn`'s future) this body DEFINES: the
    /// opaque's key and its hidden type, over the owner's generics (rules/opaque_def.rs), in the
    /// order the signature names them. What the world's `opaque_hidden` answers from, so a
    /// caller's auto-trait goal on the opaque (`Send` "leaks" from the hidden type) is decided.
    pub hidden: Vec<(crate::ids::ItemKey, PTy), Alloc<'a>>,
    /// Each `&expr` promoted to a `'static` slot (rules/promote.rs), sorted: the builder makes
    /// it the address of a constant row, never a frame temporary.
    pub promoted: Vec<crate::ids::ExprId, Alloc<'a>>,
    /// Each auto-trait goal of an async block's future that failed only on the superset
    /// interior, left for the exact one (src/stalled.rs), in the order it failed. Empty except
    /// under `check_deferring`; a caller deciding them passes them to `decide_stalled`.
    pub stalled: Vec<crate::stalled::Stalled, Alloc<'a>>,
    /// The precision of each block's checked interior, sorted by expression.
    pub coroutine_witnesses: Vec<(ExprId, CoroutineWitness), Alloc<'a>>,
    /// Each `.await` and the future it holds while suspended, `<E as IntoFuture>::IntoFuture`
    /// (the Reference, "Await expressions", steps 1-2: `IntoFuture::into_future` of the
    /// operand, evaluated to the future `tmp` that is polled), as the check decided it
    /// (rules/async_fmt.rs `expr_await`), sorted by expression; read with
    /// [`Checked::awaited_of`]. The flow lowering's `.await` takes it as the type of its
    /// `tmp` (blaze-flow-lower coroutine.rs), so an async block's exact interior holds it. An
    /// `.await` whose future is not wholly known has no row.
    pub awaited: Vec<(ExprId, PTy), Alloc<'a>>,
    /// Each projection a callee's signature names at each call, and the impl the check chose
    /// for it (src/selected.rs), sorted by expression, each call's rows in the order its
    /// signature was lowered; read with [`Checked::selected_at`]. The borrow check reads a
    /// hidden input (`x: T::Out`) through the chosen impl's `type` text (blaze-flow-lower
    /// impl_call.rs).
    pub selected: Vec<crate::selected::Projected, Alloc<'a>>,
    /// The fields each struct update takes from its base (rules/adt.rs `record_fru_fields`):
    /// what the flow lowering projects out of a place base, one move or copy per field.
    pub fru_fields: FruFields<'a>,
    /// The fields each base-less struct expression takes from its defaults (rules/adt.rs
    /// `record_default_fields`).
    pub default_fields: DefaultFields<'a>,
    /// Each overloaded index expression (`a[b]` on a type that is not an array or slice indexed
    /// by `usize`) that is a mutable place, sorted (rules/mut_places.rs): its place is
    /// `*IndexMut::index_mut(&mut a, b)`; every other overloaded index's is
    /// `*Index::index(&a, b)` (the Reference, "Array and slice indexing expressions"). Read
    /// with [`Checked::index_is_mut`].
    pub index_mut: Vec<ExprId, Alloc<'a>>,
    /// The typed lints' findings (rules/args_inline.rs `args-inline`), sorted: no type error,
    /// so a body with findings and no `diags` still lowers and builds; each has no level
    /// (rule 81) and an item-relative range (rule 71).
    pub findings: Vec<crate::rules::args_inline::TypedFinding, Alloc<'a>>,
    /// What the unsafety check could not decide (rules/unsafety.rs `Undecided`), sorted by
    /// place: no type error and no pass; the command files each as "not checked" (rule 3).
    pub undecided: Vec<crate::rules::unsafety::Undecided, Alloc<'a>>,
    /// When the owner's return signature wrote one or more `_` placeholders (`-> _`,
    /// `-> Option<_>`, `-> Result<T, _>`), the concrete finalized return type in `types`.
    pub inferred_output: Option<PTy>,
}

/// Two two-column tables are equal column by column.
fn soa2_eq<X: PartialEq, Y: PartialEq>(a: &Soa<(X, Y), Alloc<'_>>, b: &Soa<(X, Y), Alloc<'_>>) -> bool {
    a.column::<0>() == b.column::<0>() && a.column::<1>() == b.column::<1>()
}

/// Two three-column tables are equal column by column.
fn soa3_eq<X: PartialEq, Y: PartialEq, Z: PartialEq>(
    a: &Soa<(X, Y, Z), Alloc<'_>>,
    b: &Soa<(X, Y, Z), Alloc<'_>>,
) -> bool {
    a.column::<0>() == b.column::<0>() && a.column::<1>() == b.column::<1>() && a.column::<2>() == b.column::<2>()
}

/// Columns compared one by one, for records whose fields are `Vec`s.
macro_rules! columns_eq {
    ($ty:ident { $($f:ident),* $(,)? }) => {
        impl<'a> PartialEq for $ty<'a> {
            fn eq(&self, other: &$ty<'a>) -> bool {
                true $(&& self.$f == other.$f)*
            }
        }

        impl Eq for $ty<'_> {}

        impl core::fmt::Debug for $ty<'_> {
            fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
                f.debug_struct(stringify!($ty))
                    $(.field(stringify!($f), &self.$f))*
                    .finish()
            }
        }
    };
}

pub(crate) use columns_eq;

impl<'a> Checked<'a> {
    /// The generic arguments the constant path at `e` is read at (a list of `types`), or `None`
    /// for a constant with no generics or an expression that names no constant.
    pub fn const_args_of(&self, e: ExprId) -> Option<List> {
        let at = self.const_args.column::<0>().binary_search(&e).ok()?;
        Some(self.const_args.column::<1>()[at])
    }

    /// The generic arguments the constant pattern `p` is read at (a list of `types`), or
    /// `None` for a constant with no generics or a pattern that names no constant.
    pub fn pat_const_args_of(&self, p: PatId) -> Option<List> {
        let at = self.pat_const_args.column::<0>().binary_search(&p).ok()?;
        Some(self.pat_const_args.column::<1>()[at])
    }

    /// The future the `.await` at `e` holds while suspended (`<E as IntoFuture>::IntoFuture`,
    /// a type of `types`), or `None` for an expression that is no `.await` or whose future
    /// is not wholly known.
    pub fn awaited_of(&self, e: ExprId) -> Option<PTy> {
        let at = self.awaited.binary_search_by_key(&e, |r| r.0).ok()?;
        Some(self.awaited[at].1)
    }

    /// The steps of the why-chain `w` (a finding's [`Diag::why`]), root first; `None` when
    /// they are not this record's rows (a finding of another record).
    pub fn why_of(&self, w: crate::diag::Why) -> Option<&[crate::diag::WhyStep]> {
        self.why.get(w.start as usize..w.start.checked_add(w.len)? as usize)
    }

    pub fn coroutine_witness(&self, block: ExprId) -> Option<CoroutineWitness> {
        let row = self.coroutine_witnesses.binary_search_by_key(&block, |r| r.0).ok()?;
        Some(self.coroutine_witnesses[row].1)
    }

    /// The projections the callee's signature named at the call `e`, in lowering order (row
    /// `i` has `at == i`); empty for an expression that is no call or whose callee names none.
    pub fn selected_at(&self, e: ExprId) -> &[crate::selected::Projected] {
        let rows = self.call_selections(e);
        &rows[..rows.partition_point(|r| r.name.is_some())]
    }

    /// The projections in signature order, followed by the callee's lifetime-bearing trait bounds.
    pub fn call_selections(&self, e: ExprId) -> &[crate::selected::Projected] {
        let lo = self.selected.partition_point(|r| r.expr < e);
        let hi = self.selected.partition_point(|r| r.expr <= e);
        &self.selected[lo..hi]
    }

    /// The rows of [`Checked::capture_places`] of the closure or async block `e`, its
    /// environment's fields in order: empty when the place analysis did not decide `e` (its
    /// captures are then the whole bindings of [`Checked::captures`]).
    pub fn capture_places_of(&self, e: ExprId) -> core::ops::Range<usize> {
        let col = self.capture_places.column::<0>();
        let lo = col.partition_point(|&x| x < e);
        let hi = col.partition_point(|&x| x <= e);
        lo..hi
    }

    /// The fields the struct update at `e` takes from its base, as `(field index, type)` in
    /// field order: two parallel slices, empty for an expression that is no struct update.
    pub fn fru_fields_of(&self, e: ExprId) -> (&[u32], &[PTy]) {
        let col = self.fru_fields.column::<0>();
        let lo = col.partition_point(|&x| x < e);
        let hi = col.partition_point(|&x| x <= e);
        (&self.fru_fields.column::<1>()[lo..hi], &self.fru_fields.column::<2>()[lo..hi])
    }

    /// The fields the base-less struct expression at `e` takes from its defaults, as
    /// `(field index, default's key)` in field order: empty for any other expression.
    pub fn default_fields_of(&self, e: ExprId) -> (&[u32], &[ItemKey]) {
        let col = self.default_fields.column::<0>();
        let lo = col.partition_point(|&x| x < e);
        let hi = col.partition_point(|&x| x <= e);
        (&self.default_fields.column::<1>()[lo..hi], &self.default_fields.column::<2>()[lo..hi])
    }

    /// Whether overloaded index expression `e` calls `IndexMut::index_mut` (a mutable place),
    /// not `Index::index`.
    pub fn index_is_mut(&self, e: ExprId) -> bool {
        self.index_mut.binary_search(&e).is_ok()
    }

    /// How pattern `p` matches or narrows a structural union, if applicable.
    pub fn union_pat_of(&self, p: PatId) -> Option<UnionPat> {
        let at = self.union_pats.column::<0>().binary_search(&p).ok()?;
        self.union_pats.column::<1>().get(at).copied()
    }

    /// Equal results ignoring the read list and needs: what the read-list property compares.
    pub fn same_result(&self, other: &Checked<'a>) -> bool {
        self.types == other.types
            && self.expr_ty == other.expr_ty
            && self.pat_ty == other.pat_ty
            && self.local_ty == other.local_ty
            && soa3_eq(&self.adjusts, &other.adjusts)
            && soa3_eq(&self.callees, &other.callees)
            && soa2_eq(&self.expr_defs, &other.expr_defs)
            && soa2_eq(&self.const_args, &other.const_args)
            && soa2_eq(&self.pat_const_args, &other.pat_const_args)
            && soa2_eq(&self.pat_defs, &other.pat_defs)
            && soa2_eq(&self.union_pats, &other.union_pats)
            && soa3_eq(&self.fields, &other.fields)
            && soa3_eq(&self.captures, &other.captures)
            && self.capture_places.column::<0>() == other.capture_places.column::<0>()
            && self.capture_places.column::<1>() == other.capture_places.column::<1>()
            && self.capture_places.column::<2>() == other.capture_places.column::<2>()
            && self.capture_places.column::<3>() == other.capture_places.column::<3>()
            && self.capture_places.column::<4>() == other.capture_places.column::<4>()
            && self.capture_places.column::<5>() == other.capture_places.column::<5>()
            && self.capture_projs.column::<0>() == other.capture_projs.column::<0>()
            && self.capture_projs.column::<1>() == other.capture_projs.column::<1>()
            && self.capture_projs.column::<2>() == other.capture_projs.column::<2>()
            && self.capture_projs.column::<3>() == other.capture_projs.column::<3>()
            && self.closures.column::<0>() == other.closures.column::<0>()
            && self.closures.column::<1>() == other.closures.column::<1>()
            && self.closures.column::<2>() == other.closures.column::<2>()
            && self.closures.column::<3>() == other.closures.column::<3>()
            && self.closures.column::<4>() == other.closures.column::<4>()
            && self.diags == other.diags
            && self.suggests == other.suggests
            && self.hidden == other.hidden
            && self.promoted == other.promoted
            && self.stalled == other.stalled
            && self.coroutine_witnesses == other.coroutine_witnesses
            && self.awaited == other.awaited
            && self.selected == other.selected
            && soa3_eq(&self.fru_fields, &other.fru_fields)
            && soa3_eq(&self.default_fields, &other.default_fields)
            && self.index_mut == other.index_mut
            && self.findings == other.findings
            && self.undecided == other.undecided
            && self.inferred_output == other.inferred_output
    }
}

impl<'a> PartialEq for Checked<'a> {
    fn eq(&self, other: &Checked<'a>) -> bool {
        self.same_result(other) && self.reads == other.reads && self.needs == other.needs
    }
}

impl<'a> Eq for Checked<'a> {}

impl<'a> core::fmt::Debug for Checked<'a> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Checked")
            .field("types", &self.types)
            .field("expr_ty", &self.expr_ty)
            .field("pat_ty", &self.pat_ty)
            .field("local_ty", &self.local_ty)
            .field("adjusts", &self.adjusts)
            .field("callees", &self.callees)
            .field("union_pats", &self.union_pats)
            .field("fields", &self.fields)
            .field("captures", &self.captures)
            .field("capture_places", &self.capture_places)
            .field("capture_projs", &self.capture_projs)
            .field("closures", &self.closures)
            .field("diags", &self.diags)
            .field("why", &self.why)
            .field("suggests", &self.suggests)
            .field("hidden", &self.hidden)
            .field("promoted", &self.promoted)
            .field("stalled", &self.stalled)
            .field("coroutine_witnesses", &self.coroutine_witnesses)
            .field("awaited", &self.awaited)
            .field("selected", &self.selected)
            .field("fru_fields", &self.fru_fields)
            .field("default_fields", &self.default_fields)
            .field("index_mut", &self.index_mut)
            .field("findings", &self.findings)
            .field("undecided", &self.undecided)
            .field("inferred_output", &self.inferred_output)
            .field("reads", &self.reads)
            .field("needs", &self.needs)
            .finish()
    }
}
