//! The building blocks of compile identity (`hir-ty-identity`'s `instance_identity`, which
//! decides which generic instances compile to one body: idea 2 of
//! `docs/v2/plan/design/compile-time.md`, `docs/v2/plan/design/compile-time.md`), and the
//! code generator's spelling of types and bodies by content:
//!
//! - `fact`: what code that moves, copies, projects or passes a value of a type reads (its
//!   layout and shape, never its name), the one query per type and environment, [`Cx::fact`];
//! - [`identity`]: the full identity of a type, constant or argument list by content, which
//!   tells types apart where code does (`TypeId`, `type_name`, vtables, `dyn` calls);
//! - [`code`]: a body's code as the code generator compiles it;
//! - [`drop`]: what a drop of a value reads;
//! - [`deeply_normalize`]: a type with every alias in it normalized, as it is spelled;
//! - `intrinsics`: what rustc's definition of each intrinsic reads ([`classify`]);
//! - [`Unshared`]: why an instance has no compile identity.
//!
//! Nothing here reads a dense id into a hash: items by their [`blaze_items`] keys, types by
//! content.

#![forbid(unsafe_code)]
#![feature(allocator_api)]

pub mod code;
pub mod drop;
mod fact;
pub mod identity;
mod intrinsics;
mod layout;
mod normalize;

#[cfg(test)]
mod tests;

use std::fmt;

pub use fact::{Cx, is_plain_data};
pub use intrinsics::{Class, classify};
pub use normalize::deeply_normalize;

/// The domain of a type's fact.
pub const FACT_DOMAIN: &str = "blaze v2 share fact 2";

/// Why an instance has no compile identity (`hir_ty_identity::instance_identity`): the code
/// generator compiles it for its arguments alone where it is so by what it is (a cycle, a shim,
/// a per-instance intrinsic, arguments too deep to look into) and stops at every other reason.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Unshared {
    /// The instance is in a cycle of instances or types, which the query engine gives this to all of.
    Recursive,
    /// A shim the code generator builds without MIR: kept per instance.
    Shim(&'static str),
    /// An intrinsic that is not in [`intrinsics`]' table: its name, the body that calls it, and
    /// how the instance reached that body (rule 19). Each body is named as an error names one
    /// (`` `path` ``, "a closure in `path`"). [`Through`] is decided where the error is made and
    /// as it passes up a call, from the subjects themselves, never by comparing names (two
    /// bodies can have one path: methods of `impl Foo<u8>` and `impl Foo<u16>`), so a caller
    /// several calls away names its own callee and the body at the end, never the chain between.
    UnknownIntrinsic { name: String, body: String, through: Through },
    /// An intrinsic whose code is made for the one instance ([`Class::PerInstance`]).
    PerInstance(String),
    /// No options to key the body with: `hir_ty_identity::Compiled` is empty for its crate.
    NoOptions,
    /// The body has no MIR, or a part of it the code generator refuses too.
    NoMir(String),
    /// A type has no layout.
    NoLayout(String),
    /// A type or constant has no content: a parameter, an inference variable, an error, an
    /// alias that does not normalize, a constant made of other allocations.
    NoContent(String),
    /// A constant could not be evaluated.
    ConstEval(String),
    /// An item has no hash in [`blaze_items`].
    Unhashable(String),
    /// The body has a place whose type the key does not read.
    Shape(&'static str),
    /// An instance's arguments nest deeper than compile identity looks
    /// (`hir_ty_identity`'s `SHARED_DEPTH`): it is compiled for its arguments, as a cycle is,
    /// and a caller names it by them. No identity asks a callee deeper than itself, so only an
    /// instance the code generator asks for itself reaches it (a type written that deep, or the
    /// far end of a chain whose arguments grow with each call). rustc's recursion limit is the
    /// build's walk's to check (`blaze_codegen`'s `driver/recursion.rs`, with each function's
    /// body and crate's limit from `blaze_codegen::backend::Frontend::counted`), never this.
    TooDeep {
        /// How deep the arguments nest: each type or constant one level below what holds it.
        depth: u32,
        limit: u32,
    },
}

/// How an instance with [`Unshared::UnknownIntrinsic`] reached the body that calls the
/// intrinsic.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Through {
    /// The body is the instance's own.
    Own,
    /// The body is written in the instance's own: a closure or an `async` block in it, whose
    /// code is the instance's code too.
    Nested,
    /// The instance calls the body itself.
    Calls,
    /// The instance calls this callee, which reaches the body through others.
    Reaches(String),
}

impl Unshared {
    /// Whether an instance with no compile identity for this reason is compiled for its
    /// arguments alone by what it is: one of a cycle (the query engine's answer for all of it,
    /// whichever was asked first), a shim built without MIR, code that reads the instance itself
    /// (`PerInstance`), or arguments nested past what compile identity looks into. The code
    /// generator builds such an instance per instance, and a caller names it by its arguments;
    /// both read this one answer. Every other reason is something the identity could not read
    /// (a layout, a constant, an intrinsic missing from the table): an error of the instance
    /// and of every caller.
    pub fn is_per_instance(&self) -> bool {
        match self {
            Unshared::Recursive
            | Unshared::Shim(_)
            | Unshared::PerInstance(_)
            | Unshared::TooDeep { .. } => true,
            Unshared::UnknownIntrinsic { .. }
            | Unshared::NoOptions
            | Unshared::NoMir(_)
            | Unshared::NoLayout(_)
            | Unshared::NoContent(_)
            | Unshared::ConstEval(_)
            | Unshared::Unhashable(_)
            | Unshared::Shape(_) => false,
        }
    }
}

impl fmt::Display for Unshared {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Unshared::Recursive => f.write_str("it is in a cycle of instances or types"),
            Unshared::Shim(what) => write!(f, "it is a {what}, which is kept per instance"),
            Unshared::UnknownIntrinsic { name, body, through: Through::Own | Through::Nested } => {
                write!(f, "{body} calls the unclassified intrinsic `{name}`")
            }
            Unshared::UnknownIntrinsic { name, body, through: Through::Calls } => {
                write!(f, "it calls {body}, which calls the unclassified intrinsic `{name}`")
            }
            Unshared::UnknownIntrinsic { name, body, through: Through::Reaches(callee) } => {
                write!(
                    f,
                    "it calls {callee}, which reaches {body}, which calls the unclassified intrinsic `{name}`"
                )
            }
            Unshared::PerInstance(name) => {
                write!(f, "it calls `{name}`, whose code is per instance")
            }
            Unshared::NoOptions => f.write_str("no MIR options were given for its crate"),
            Unshared::NoMir(what) => write!(f, "its MIR: {what}"),
            Unshared::NoLayout(what) => write!(f, "a layout: {what}"),
            Unshared::NoContent(what) => write!(f, "no content: {what}"),
            Unshared::ConstEval(what) => write!(f, "a constant: {what}"),
            Unshared::Unhashable(what) => f.write_str(what),
            Unshared::Shape(what) => {
                write!(f, "its body has {what}, whose type the key does not read")
            }
            Unshared::TooDeep { depth, limit } => write!(
                f,
                "its arguments nest {depth} deep, past the {limit} compile identity looks into: \
                 it is compiled for its arguments"
            ),
        }
    }
}
