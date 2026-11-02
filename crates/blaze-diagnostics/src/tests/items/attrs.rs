//! rustc's UI tests of the errors of builtin attributes: what attribute parsing gives
//! (`parse`, and one module per parser group: `codegen`, `stability`, `internal`, `misc`,
//! `doc`) and what `check_mod_attrs` gives (`check`).

mod check;
mod codegen;
mod doc;
mod internal;
mod misc;
mod parse;
mod stability;
