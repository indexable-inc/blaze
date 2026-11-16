//! Diagnostics as data (typeck.md, "Diagnostics are values"): a user's type error is a value in
//! the record with an item-relative range (rule 71), never a panic and never a stop. Codes are
//! rustc's, so a person's search finds the same page.

use crate::ids::{ExprId, Name, PatId, Range};
use crate::portable::PTy;
use crate::ty::Ty;

/// What went wrong, by rustc's code.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum DiagCode {
    /// E0308: mismatched types.
    Mismatch,
    /// E0308, `if` and `else` (or match arms, or breaks) have incompatible types.
    IncompatibleArms,
    /// E0317: `if` without `else` whose type is not `()`.
    IfWithoutElse,
    /// E0282: type annotations needed.
    AnnotationsNeeded,
    /// E0277: a bound is not satisfied.
    Unsatisfied,
    /// E0425: cannot find value.
    UnresolvedPath,
    /// E0423 / E0533: expected value, found a type, trait or struct name.
    NotAValue,
    /// E0574: expected struct, found something else.
    NotAStruct,
    /// E0599: no method named so.
    NoMethod,
    /// E0599, the name is an associated function, not a method.
    NotAMethod,
    /// E0034: multiple applicable items in scope.
    AmbiguousMethod,
    /// E0609: no field on type.
    NoField,
    /// E0610: a primitive type has no fields.
    PrimitiveField,
    /// E0560: a struct has no field so named.
    NoSuchFieldInit,
    /// E0063: missing fields in a struct expression.
    MissingFields,
    /// E0061: wrong number of arguments.
    ArgCount,
    /// E0107: wrong number of generic arguments.
    GenericArgCount,
    /// E0618: expected function, found something else.
    NotCallable,
    /// E0614: type cannot be dereferenced.
    CantDeref,
    /// E0608: cannot index into a value of this type.
    CantIndex,
    /// E0369: binary operation cannot be applied.
    BinOp,
    /// E0600: unary operation cannot be applied.
    UnOp,
    /// E0606: casting is invalid.
    InvalidCast,
    /// E0605: non-primitive cast.
    NonPrimitiveCast,
    /// E0604: only `u8` can be cast as `char`.
    CharCast,
    /// E0055: the autoderef recursion limit.
    RecursionLimit,
    /// E0023: wrong number of fields in a tuple-struct pattern.
    PatternArity,
    /// E0026: a struct pattern names a field the struct lacks.
    NoSuchFieldPat,
    /// E0027: a struct pattern without `..` misses fields.
    MissingFieldPats,
    /// Edition 2024: a binding modifier (`mut`, `ref`) or `&` pattern where the default binding
    /// mode is not `move`.
    BindingModifier2024,
    /// E0070: invalid left-hand side of an assignment.
    InvalidLhs,
}

impl DiagCode {
    /// rustc's error code, for printing.
    pub const fn code(self) -> &'static str {
        match self {
            DiagCode::Mismatch | DiagCode::IncompatibleArms => "E0308",
            DiagCode::IfWithoutElse => "E0317",
            DiagCode::AnnotationsNeeded => "E0282",
            DiagCode::Unsatisfied => "E0277",
            DiagCode::UnresolvedPath => "E0425",
            DiagCode::NotAValue => "E0423",
            DiagCode::NotAStruct => "E0574",
            DiagCode::NoMethod | DiagCode::NotAMethod => "E0599",
            DiagCode::AmbiguousMethod => "E0034",
            DiagCode::NoField => "E0609",
            DiagCode::PrimitiveField => "E0610",
            DiagCode::NoSuchFieldInit => "E0560",
            DiagCode::MissingFields => "E0063",
            DiagCode::ArgCount => "E0061",
            DiagCode::GenericArgCount => "E0107",
            DiagCode::NotCallable => "E0618",
            DiagCode::CantDeref => "E0614",
            DiagCode::CantIndex => "E0608",
            DiagCode::BinOp => "E0369",
            DiagCode::UnOp => "E0600",
            DiagCode::InvalidCast => "E0606",
            DiagCode::NonPrimitiveCast => "E0605",
            DiagCode::CharCast => "E0604",
            DiagCode::RecursionLimit => "E0055",
            DiagCode::PatternArity => "E0023",
            DiagCode::NoSuchFieldPat => "E0026",
            DiagCode::MissingFieldPats => "E0027",
            DiagCode::BindingModifier2024 => "E0000",
            DiagCode::InvalidLhs => "E0070",
        }
    }
}

/// Where a diagnostic points, before ranges are looked up.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum At {
    Expr(ExprId),
    Pat(PatId),
    Body,
}

/// A diagnostic while the walk runs: types as handles, exported at writeback.
#[derive(Clone, Copy, Debug)]
pub(crate) struct PendingDiag {
    pub code: DiagCode,
    pub at: At,
    pub expected: Option<Ty>,
    pub found: Option<Ty>,
    pub name: Option<Name>,
}

/// A diagnostic of the record.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Diag {
    pub code: DiagCode,
    /// Item-relative byte range.
    pub at: Range,
    /// The expected type, in the record's type table.
    pub expected: Option<PTy>,
    /// The type found.
    pub found: Option<PTy>,
    /// The name the error is about (a method, a field, a path's last segment).
    pub name: Option<Name>,
}
