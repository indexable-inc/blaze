//! Port of CPython's `Lib/re/`.
//!
//! [`parser`] mirrors the public shape of CPython's `_parser.py` /
//! `_constants.py` closely enough that
//! `hypothesis.strategies._internal.regex` can be ported without papering
//! over Python-vs-Rust regex semantic differences; the regex string
//! generator walks its AST. [`matcher`] is a backtracking matcher over the
//! same AST with CPython's search order, for the places that must know
//! where CPython's `re` would end a match (checking that a generated
//! terminal lexes back to itself, simulating a lexer at token boundaries);
//! [`vm`] finds the same match ends for the patterns it can express by
//! simulating every path at once, in time linear in the text.

pub mod casefold;
pub mod categories;
pub mod constants;
pub mod constraint;
pub mod matcher;
pub mod parser;
pub mod vm;
pub mod width;
