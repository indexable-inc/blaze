//! An item an attribute macro takes contributes no text of its own: only the expansion's items
//! have texts (brief 18, piece P18-A).
//!
//! The Reference, "Procedural Macros", "Attribute macros" (macro.proc.attribute.behavior):
//! "The second TokenStream is the rest of the item, including other attributes on the item."
//! ... "The item to which the attribute is applied is replaced by the zero or more items in the
//! returned TokenStream." So the macro's input is no item of the crate: its text is not in the
//! texts table (nor in the items record, nor head-checked by the front end); the `main` the
//! macro writes back is, at its expansion.
//!
//! Expected: `test tests::items_attr_text::an_item_an_attribute_macro_takes_has_no_text_of_its_own ... ok`.
//! Without the fix (the text pushed before the attribute-macro branch returns) the input's
//! `main` is a second text, at the file.

use std::vec::Vec;

use super::entry::{Texts, input, resolved};
use crate::resolve::At;

/// A `macro_rules!` attribute macro (bstd's `main` shape: an `attr()` arm) that keeps its fn
/// and adds one beside it, as `#[tokio::main]` and tempo's `#[instruction]` write a fn of the
/// same name back.
const DEP: &str = "#![no_core]
#[macro_export]
macro_rules! wrap {
    attr() ($(#[$m:meta])* $v:vis fn $name:ident() {}) => {
        $(#[$m])* $v fn $name() {}
        pub fn wrapped() {}
    };
}
";

const USER: &str = "#![no_core]
#[dep::wrap]
pub fn main() {}
#[inline]
pub fn kept() {}
";

static TEXTS: Texts = Texts(&[&[("/user/lib.rs", USER)], &[("/dep/lib.rs", DEP)]]);

#[test]
fn an_item_an_attribute_macro_takes_has_no_text_of_its_own() {
    let crates = [input("user", "/user/lib.rs", &[("dep", 1)]), input("dep", "/dep/lib.rs", &[])];
    let names = resolved(&crates, &TEXTS);
    let user = names.krate(0);
    let mains: Vec<&At> = user.tables.texts.values().filter(|(t, _)| t.name == Some("main")).map(|(_, at)| at).collect();
    assert_eq!(mains.len(), 1, "one `main` text, the expansion's; the macro's input has none: {mains:?}; notes {:?}", names.notes(0));
    assert!(matches!(mains[0], At::Expansion(_)), "the one `main` text is at the expansion: {mains:?}");
    // The clean neighbours: the expansion's other item and an item under a built-in attribute
    // keep their texts.
    let named = |n: &str| user.tables.texts.values().filter(|(t, _)| t.name == Some(n)).count();
    assert_eq!(named("wrapped"), 1, "`wrapped`, written by `#[dep::wrap]`, has its text");
    assert_eq!(named("kept"), 1, "`kept`, under the built-in `#[inline]`, has its text");
}
