//! A tool attribute in an expansion's text is inert, as in a file's.
//!
//! tempo census 14, "diag E0425" (piece e0425-1): `tempo_contracts::contracts::CreateX::..::
//! deploy_builder` names `BYTECODE` and `tempo_evm::genesis::deploy_permit2` names
//! `tempo_contracts::Permit2::BYTECODE`, both "cannot find value". alloy-sol-macro-expander
//! 1.7.3 (expand/contract.rs:52) writes the contract's module with
//! `#[rustfmt::skip] #[allow(clippy::all)] pub static BYTECODE: .. = ..;`: the only items of a
//! `sol!` expansion that carry a tool attribute. An expansion's text holds a space between its
//! tokens, so the attribute's head ([`super::lower::head`]) is `rustfmt :: skip`, and
//! `Cfg::attr` split it at `::` without trimming: the tool `rustfmt ` (with its space) is no
//! tool, the attribute became the item's attribute macro (`Attrs::attr_macro`), the item
//! "declares nothing until the call expands", and the cells (cells/mod.rs, the tool check on the
//! call's parsed segments) never make that call: `BYTECODE` was never filed.
//!
//! The Reference, "Attributes", "Tool attributes": "The tool name must be ... a tool known to
//! the compiler ... `rustfmt`, `clippy`"; "When a tool is not in use, the tool's attributes are
//! accepted without a warning" — inert, never an attribute macro, whatever the spacing of the
//! tokens (a path's tokens are the same path with or without spaces between them).
//!
//! Expected: `tool_attribute_in_an_expansion_is_inert` passes. Control: without the trim at the
//! tool's segment in `Cfg::attr` (task/lower.rs), `attr_macro` is `Some("rustfmt :: skip")` and
//! the first assertion fails with "the spaced tool attribute became an attribute macro: Some(\"rustfmt :: skip\")".

use super::{Attrs, Cfg};
use blaze_own::AVec;

/// The attributes `contents` give under an empty configuration.
fn attrs_of<'t>(contents: &[&'t str], alloc: blaze_own::Alloc<'t>) -> Attrs<'t, 't> {
    let cfg = Cfg::from_atoms(&[], alloc);
    let mut attrs = Attrs::new_in(alloc);
    let mut stack = AVec::new_in(alloc);
    for &c in contents {
        cfg.attr(&mut attrs, c, &mut stack);
    }
    attrs
}

#[test]
fn tool_attribute_in_an_expansion_is_inert() {
    let alloc = blaze_own::testing::heap().alloc();
    // As alloy-sol-macro's tokens print in an expansion's text.
    let spaced = attrs_of(&["rustfmt :: skip", "allow (clippy :: all)"], alloc);
    assert_eq!(spaced.attr_macro, None, "the spaced tool attribute became an attribute macro: {:?}", spaced.attr_macro);
    // As a file writes it.
    let tight = attrs_of(&["rustfmt::skip"], alloc);
    assert_eq!(tight.attr_macro, None, "the tool attribute as a file writes it: {:?}", tight.attr_macro);
    // An attribute macro of a path no tool owns stays one, spaced or not (the fix trims, it
    // does not take every two-segment path as a tool's).
    let spaced_macro = attrs_of(&["serde_with :: serde_as"], alloc);
    assert_eq!(spaced_macro.attr_macro, Some("serde_with :: serde_as"), "a spaced attribute macro is still one");
    let tight_macro = attrs_of(&["tokio::main"], alloc);
    assert_eq!(tight_macro.attr_macro, Some("tokio::main"), "an attribute macro is still one");
}
