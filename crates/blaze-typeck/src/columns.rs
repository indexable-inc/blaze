//! The record of one checked body, in columns (typeck.md, "Results: one record in columns"):
//! every type in the record's own hash-consed table, items by key, positions item-relative.
//! Sparse facts (adjustments, callees, fields, captures) are parallel columns sorted by
//! expression.

use blaze_own::Vec;
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

/// One adjustment and the type it produces.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Adjust {
    pub kind: AdjustKind,
    pub target: PTy,
}

/// How a closure captures a local.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum CaptureKind {
    ByRef,
    ByMutRef,
    ByValue,
}

/// The record of one body.
pub struct Checked<A: Allocator + Clone> {
    /// Every type the record names.
    pub types: PortableTys<A>,
    /// One per expression: its type before adjustments.
    pub expr_ty: Vec<PTy, A>,
    /// One per pattern.
    pub pat_ty: Vec<PTy, A>,
    /// One per pattern: the binding's type for a binding pattern, the pattern's type otherwise.
    pub local_ty: Vec<PTy, A>,
    /// Adjustments: the expression, and each step in order.
    pub adjust_expr: Vec<ExprId, A>,
    pub adjust: Vec<Adjust, A>,
    /// The resolved callee of each call and method call: the expression, the item, and its
    /// generic arguments (a list of `types`).
    pub callee_expr: Vec<ExprId, A>,
    pub callee_item: Vec<ItemKey, A>,
    pub callee_args: Vec<List, A>,
    /// Each field access: the expression, the variant and the field's index.
    pub field_expr: Vec<ExprId, A>,
    pub field_variant: Vec<u32, A>,
    pub field_index: Vec<u32, A>,
    /// Each capture: the closure, the binding captured, how.
    pub capture_closure: Vec<ExprId, A>,
    pub capture_local: Vec<PatId, A>,
    pub capture_kind: Vec<CaptureKind, A>,
    /// The body's diagnostics, in the order found.
    pub diags: Vec<Diag, A>,
    /// What the check read, and what to fetch next time.
    pub reads: ReadList<A>,
    pub needs: Needs<A>,
}

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

columns_eq!(Checked {
    types,
    expr_ty,
    pat_ty,
    local_ty,
    adjust_expr,
    adjust,
    callee_expr,
    callee_item,
    callee_args,
    field_expr,
    field_variant,
    field_index,
    capture_closure,
    capture_local,
    capture_kind,
    diags,
    reads,
    needs,
});

impl<A: Allocator + Clone> Checked<A> {
    /// Equal results ignoring the read list and needs: what the read-list property compares.
    pub fn same_result(&self, other: &Checked<A>) -> bool {
        self.types == other.types
            && self.expr_ty == other.expr_ty
            && self.pat_ty == other.pat_ty
            && self.local_ty == other.local_ty
            && self.adjust_expr == other.adjust_expr
            && self.adjust == other.adjust
            && self.callee_expr == other.callee_expr
            && self.callee_item == other.callee_item
            && self.callee_args == other.callee_args
            && self.field_expr == other.field_expr
            && self.field_variant == other.field_variant
            && self.field_index == other.field_index
            && self.capture_closure == other.capture_closure
            && self.capture_local == other.capture_local
            && self.capture_kind == other.capture_kind
            && self.diags == other.diags
    }
}
