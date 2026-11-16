//! The record of one checked body, in columns (typeck.md, "Results: one record in columns"):
//! every type in the record's own hash-consed table, items by key, positions item-relative.
//! Per-node facts are one `Vec` per node kind; sparse facts (adjustments, callees, fields,
//! captures) are `Soa` tables sorted by expression, so a lookup scans only the expression column.

use blaze_own::{Soa, Vec};
use core::alloc::Allocator;

use crate::diag::Diag;
use crate::ids::{ExprId, ItemKey, List, Mutbl, PatId};
use crate::portable::{PTy, PortableTys};
use crate::world::{Needs, ReadList};

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
pub type Adjusts<A> = Soa<(ExprId, AdjustKind, PTy), A>;
/// The resolved callee of each call and method call: the expression, the item, and its generic
/// arguments (a list of the record's `types`).
pub type Callees<A> = Soa<(ExprId, ItemKey, List), A>;
/// Each field access: the expression, the variant and the field's index.
pub type Fields<A> = Soa<(ExprId, u32, u32), A>;
/// Each capture: the closure, the binding captured, how.
pub type Captures<A> = Soa<(ExprId, PatId, CaptureKind), A>;

/// The record of one body.
pub struct Checked<A: Allocator + Clone> {
    /// Every type the record names.
    pub types: PortableTys<A>,
    /// One per expression: its type before adjustments.
    pub expr_ty: Vec<PTy, A>,
    /// One per pattern.
    pub pat_ty: Vec<PTy, A>,
    /// One per pattern: the binding's type for a binding pattern, the error type otherwise.
    pub local_ty: Vec<PTy, A>,
    pub adjusts: Adjusts<A>,
    pub callees: Callees<A>,
    pub fields: Fields<A>,
    pub captures: Captures<A>,
    /// The body's diagnostics, in the order found.
    pub diags: Vec<Diag, A>,
    /// What the check read, and what to fetch next time.
    pub reads: ReadList<A>,
    pub needs: Needs<A>,
}

/// Two three-column tables are equal column by column.
fn soa3_eq<X: PartialEq, Y: PartialEq, Z: PartialEq, A: Allocator, B: Allocator>(
    a: &Soa<(X, Y, Z), A>,
    b: &Soa<(X, Y, Z), B>,
) -> bool {
    a.column::<0>() == b.column::<0>() && a.column::<1>() == b.column::<1>() && a.column::<2>() == b.column::<2>()
}

/// Columns compared one by one, for records whose fields are `Vec`s (a `derive` would demand
/// `A: PartialEq`).
macro_rules! columns_eq {
    ($ty:ident { $($f:ident),* $(,)? }) => {
        impl<A: Allocator + Clone> PartialEq for $ty<A> {
            fn eq(&self, other: &$ty<A>) -> bool {
                true $(&& self.$f == other.$f)*
            }
        }

        impl<A: Allocator + Clone> Eq for $ty<A> {}

        impl<A: Allocator + Clone> core::fmt::Debug for $ty<A> {
            fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
                f.debug_struct(stringify!($ty))
                    $(.field(stringify!($f), &self.$f))*
                    .finish()
            }
        }
    };
}

pub(crate) use columns_eq;

impl<A: Allocator + Clone> Checked<A> {
    /// Equal results ignoring the read list and needs: what the read-list property compares.
    pub fn same_result(&self, other: &Checked<A>) -> bool {
        self.types == other.types
            && self.expr_ty == other.expr_ty
            && self.pat_ty == other.pat_ty
            && self.local_ty == other.local_ty
            && soa3_eq(&self.adjusts, &other.adjusts)
            && soa3_eq(&self.callees, &other.callees)
            && soa3_eq(&self.fields, &other.fields)
            && soa3_eq(&self.captures, &other.captures)
            && self.diags == other.diags
    }
}

impl<A: Allocator + Clone> PartialEq for Checked<A> {
    fn eq(&self, other: &Checked<A>) -> bool {
        self.same_result(other) && self.reads == other.reads && self.needs == other.needs
    }
}

impl<A: Allocator + Clone> Eq for Checked<A> {}

impl<A: Allocator + Clone> core::fmt::Debug for Checked<A> {
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
            .field("diags", &self.diags)
            .field("reads", &self.reads)
            .field("needs", &self.needs)
            .finish()
    }
}
