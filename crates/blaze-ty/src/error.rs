//! Typed errors: each names the nodes it is about (rule 19). Printing them in words is the
//! caller's (`crate::print`), which knows the item names.

use core::fmt;

use crate::ids::{Node, Universe};

/// Why two terms do not relate (rustc_type_ir/src/error.rs, `TypeError`, cut to what this
/// crate can tell apart).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TypeError {
    /// Different kinds, items or numbers: `a` is not `b`.
    Mismatch { a: Node, b: Node },
    /// Lists of different lengths (tuples, signatures, arguments).
    Arity { a: Node, b: Node },
    /// Binding `var` to `term` would make it contain itself (the occurs check).
    Cyclic { var: Node, term: Node },
    /// `var` cannot name `placeholder`, which is of a later universe.
    Universe { var: Node, placeholder: Node, universe: Universe },
    /// A bound region related with another region.
    BoundRegion { a: Node, b: Node },
}

/// Why a fold refused (substitution, shifting, instantiation, canonicalization, import).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FoldError {
    /// A parameter whose index the arguments do not have.
    ParamOutOfRange { param: Node, args: u32 },
    /// A parameter whose argument is of another class (a type for a region, ..).
    ParamClass { param: Node },
    /// Shifting out a bound variable bound inside the shift.
    ShiftUnderflow { node: Node },
    /// A parameter, or a free region from the caller, in a response to canonicalize.
    InResponse { node: Node },
    /// A canonical variable whose value the instantiation does not have.
    CanonicalOutOfRange { node: Node, values: u32 },
}

/// Why a canonical response could not be applied.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ResponseError {
    /// The response's value is not a list of variable values.
    NotAList { node: Node },
    /// The response has `response` values for `orig` original values.
    Arity { response: u32, orig: usize },
    Fold(FoldError),
    Relate(TypeError),
}

impl fmt::Display for ResponseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ResponseError::NotAList { node } => write!(f, "response {:#x} is not a list of values", node.bits()),
            ResponseError::Arity { response, orig } => {
                write!(f, "a response of {response} values for {orig} original values")
            }
            ResponseError::Fold(e) => write!(f, "instantiating the response: {e}"),
            ResponseError::Relate(e) => write!(f, "applying the response: {e}"),
        }
    }
}

impl core::error::Error for ResponseError {}

impl fmt::Display for TypeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TypeError::Mismatch { a, b } => write!(f, "node {:#x} is not node {:#x}", a.bits(), b.bits()),
            TypeError::Arity { a, b } => {
                write!(f, "lists {:#x} and {:#x} have different lengths", a.bits(), b.bits())
            }
            TypeError::Cyclic { var, term } => {
                write!(f, "variable {:#x} would contain itself in {:#x} (occurs check)", var.bits(), term.bits())
            }
            TypeError::Universe { var, placeholder, universe } => write!(
                f,
                "variable {:#x} of universe {} cannot name placeholder {:#x}",
                var.bits(),
                universe.0,
                placeholder.bits()
            ),
            TypeError::BoundRegion { a, b } => {
                write!(f, "bound region {:#x} related with region {:#x}", a.bits(), b.bits())
            }
        }
    }
}

impl fmt::Display for FoldError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FoldError::ParamOutOfRange { param, args } => {
                write!(f, "parameter {:#x} is past the {args} arguments given", param.bits())
            }
            FoldError::ParamClass { param } => {
                write!(f, "parameter {:#x} is given an argument of another class", param.bits())
            }
            FoldError::ShiftUnderflow { node } => {
                write!(f, "bound variable {:#x} cannot be shifted out of its binder", node.bits())
            }
            FoldError::InResponse { node } => {
                write!(f, "{:#x} cannot be in a canonical response", node.bits())
            }
            FoldError::CanonicalOutOfRange { node, values } => {
                write!(f, "canonical variable {:#x} is past the {values} values given", node.bits())
            }
        }
    }
}

impl core::error::Error for TypeError {}
impl core::error::Error for FoldError {}
