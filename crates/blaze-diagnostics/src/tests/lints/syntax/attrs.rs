//! The lints of attributes (`lints::syntax::attrs`) against rustc's UI tests: what attribute
//! parsing says (`parse`), `check_attr`'s lints (`check_attr`), `#[doc]`'s and doc comments'
//! (`doc`), `#[diagnostic::..]`'s (`diagnostic`) and the feature lints (`features`).

mod check_attr;
mod diagnostic;
mod doc;
mod features;
mod parse;
