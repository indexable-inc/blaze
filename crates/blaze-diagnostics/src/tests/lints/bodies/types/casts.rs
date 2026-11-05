//! rustc's UI tests of the late lint passes of casts and borrows of statics and places in
//! bodies. A test with no core is [`crate::tests::ui`]'s, one with minicore or a std [`ui`]'s;
//! both run it as compiletest does.

mod function_casts;
mod provenance;
mod raw_borrows;
mod static_mut_refs;

use super::{Ui, ui};
