//! The tests of the diagnostics type checking gives of bodies, by what they are of: calls,
//! members (fields, struct literals, associated items, names), patterns and types, and the
//! printing of types those diagnostics are made of, and the errors of building patterns.

mod calls;
mod const_patterns;
mod default_fields;
mod members;
mod patterns;
mod print;
mod speed;
mod types;
