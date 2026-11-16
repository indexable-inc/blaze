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
}

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
/// The same for `Pat::Path`, `Pat::TupleStruct` and `Pat::Struct`, by pattern: the variant a
/// `match` switches on.
pub type PatDefs<'a> = Soa<(PatId, Def), Alloc<'a>>;
/// Each field access: the expression, the variant and the field's index.
pub type Fields<'a> = Soa<(ExprId, u32, u32), Alloc<'a>>;
/// Each capture: the closure, the binding captured, how.
pub type Captures<'a> = Soa<(ExprId, PatId, CaptureKind), Alloc<'a>>;
/// One row per closure expression of the body, row `i` the closure of ordinal `i`
/// (`PNode::Closure`: closure expressions in `ExprId` order): its expression, its kind (the
/// strongest `Fn*` trait it implements, from its captures) and its signature (a `FnPtr`).
pub type Closures<'a> = Soa<(ExprId, ClosureKind, PTy), Alloc<'a>>;
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
    pub pat_defs: PatDefs<'a>,
    pub fields: Fields<'a>,
    pub captures: Captures<'a>,
    pub closures: Closures<'a>,
    /// The body's diagnostics, in the order found.
    pub diags: Vec<Diag, Alloc<'a>>,
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
            && soa2_eq(&self.pat_defs, &other.pat_defs)
            && soa3_eq(&self.fields, &other.fields)
            && soa3_eq(&self.captures, &other.captures)
            && soa3_eq(&self.closures, &other.closures)
            && self.diags == other.diags
            && self.suggests == other.suggests
            && self.hidden == other.hidden
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
            .field("fields", &self.fields)
            .field("captures", &self.captures)
            .field("closures", &self.closures)
            .field("diags", &self.diags)
            .field("suggests", &self.suggests)
            .field("hidden", &self.hidden)
            .field("reads", &self.reads)
            .field("needs", &self.needs)
            .finish()
    }
}
