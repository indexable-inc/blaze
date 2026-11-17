//! names-attr-3: a tool the crate root registers keeps its item (a), `#[test]` is the prelude's
//! macro and a false `cfg` out of test mode (b), a derive's helpers are in scope after it on its
//! item and a helper no derive declares is refused (c).

use std::string::String;
use std::vec::Vec;

use blaze_id::ItemKey;
use blaze_own::AVec;

use super::entry::{Texts, input, resolved};
use super::{Solved, V, World, solve};
use crate::task::{Attrs, Cfg};
use crate::{CallShape, DefKind, Edition, Namespace, ScopeError, Unresolved};

fn solved(w: &World) -> Solved {
    match solve(w, None) {
        Ok(s) => s,
        Err(e) => panic!("the driver failed: {e}"),
    }
}

fn macro_error(s: &Solved, m: ItemKey, call: ItemKey) -> Option<Unresolved> {
    s.errors(m).iter().find_map(|e| match e {
        ScopeError::Macro { call: c, why, .. } if *c == call => Some(*why),
        _ => None,
    })
}

const ROOT_TOOL: &str = "#![no_core]
#![register_tool(xtool)]
#[xtool::anything]
pub fn kept() {}
";

static ROOT_TOOL_TEXTS: Texts = Texts(&[&[("/user/lib.rs", ROOT_TOOL)]]);

/// (a) Rule: a tool the crate root registers makes an attribute whose path starts with it inert
/// on every item of the crate (the Reference, "Attributes", `attributes.tool`: "the tool
/// attributes .. and tools registered with `register_tool`", a crate-level attribute): the item
/// is kept, declared with its text, and no call is made of the attribute. Through the real path
/// (`resolve::resolve_crates`).
///
/// Expected: "test blaze-scope: exit 0; 1 passed, 0 failed". Control: without names-attr-3's
/// crate tool set in the lowering (`LowerCx::tools`, `Lower::attrs`'s taker choice) the
/// lowering makes `xtool::anything` the item's attribute macro, the cells skip it as a tool's,
/// and `kept` is never declared: the test fails at "`kept` is declared".
#[test]
fn a_tool_the_root_registers_keeps_its_item() {
    let crates = [input("user", "/user/lib.rs", &[])];
    let names = resolved(&crates, &ROOT_TOOL_TEXTS);
    let user = names.krate(0);
    let root = names.root(0);
    let decls = &user.decls.get(&root).expect("the root is lowered").1;
    let texts: Vec<Option<&str>> = user.tables.texts.values().map(|(t, _)| t.name).collect();
    assert!(texts.contains(&Some("kept")), "`kept` is declared: {texts:?}; notes {:?}", names.notes(0));
    let calls: Vec<String> = decls.calls.iter().map(|c| c.path.segments.join("::")).collect();
    assert!(calls.is_empty(), "a registered tool's attribute is no call: {calls:?}");
}

/// (b) Rule: `test` is the prelude's attribute macro, not a built-in attribute (the pinned
/// library, core/src/macros/mod.rs: `#[rustc_builtin_macro] pub macro test($item:item)`,
/// re-exported by core/src/prelude/v1.rs). Out of test mode the item is gone as under a false
/// `cfg` (the Reference, `attributes.testing.test.enabled`: "Test functions are only compiled
/// when in test mode"); in it the item stays and `test` is no built-in attribute check. In the
/// cells `test` is not in the built-in attribute scope: a macro the scope binds under the name
/// (here a crate-root attribute macro, standing for the prelude's) is what `#[test]` names, with
/// no E0659.
///
/// Expected: "test blaze-scope: exit 0; 1 passed, 0 failed". Control: with `test` in the crate
/// root's `builtin_attrs` (the World's list before names-attr-3, or the lowering's old
/// `BUILTIN_ATTRS` written to the root record) the call fails `Ambiguous(BuiltinAttr)`.
#[test]
fn test_is_a_false_cfg_out_of_test_mode_and_the_prelude_macro_in_it() {
    let alloc = blaze_own::testing::heap().alloc();
    let off = Cfg::from_atoms(&[], alloc);
    let on = Cfg::from_atoms(&[("test", None)], alloc);
    for (cfg, kept) in [(&off, false), (&on, true)] {
        let mut attrs = Attrs::new_in(alloc);
        let mut stack = AVec::new_in(alloc);
        cfg.attr(&mut attrs, "test", &mut stack);
        assert_eq!(attrs.cfg_ok, kept, "`#[test]` keeps its item only in test mode");
        assert_eq!(attrs.attr_macro, None, "`test` is read by the lowering, never the item's taker");
        assert!(attrs.checks.is_empty(), "`test` is no built-in attribute check: {:?}", attrs.checks);
    }

    let mut w = World::new();
    let root = w.root(1, Edition::E2021);
    w.item(root, "test", Namespace::Macros, DefKind::Macro(crate::MacroShapes::ATTR), V::Pub);
    let call = w.call_on(root, "test", CallShape::Attr, Some(1));
    let s = solved(&w);
    assert_eq!(macro_error(&s, root, call), None, "`#[test]` names the macro, no ambiguity: {:?}", s.errors(root));
}

/// (c) Rule: a derive's helper attribute is in scope on the item the derive is applied to, from
/// the derive's place on (the Reference, "Procedural Macros", "Derive macro helper
/// attributes": "Derive macros can add additional attributes into the scope of the item they
/// are on"; core/src/default.rs: `#[rustc_builtin_macro(Default, attributes(default))]`). So
/// `#[derive(Default)] enum E { #[default] A }` is clean, and `#[defualt]` there is no helper,
/// no attribute and no macro: "cannot find attribute" (`NotFound`). The variant's attribute is
/// filed on its enum's item, the place a derive's helpers are scoped by.
///
/// Expected: "test blaze-scope: exit 0; 1 passed, 0 failed". Control: without names-attr-3's
/// helper scope (`Cells::derive_helper` in `try_item`) `#[default]` fails `NotFound` too.
#[test]
fn a_derive_helper_is_in_scope_and_a_misspelled_one_is_not() {
    let mut w = World::new();
    let root = w.root(1, Edition::E2021);
    w.derive_macro(root, "Default", &["default"]);
    w.call_on(root, "Default", CallShape::Derive, Some(1));
    let helper = w.call_on(root, "default", CallShape::Attr, Some(1));
    let typo = w.call_on(root, "defualt", CallShape::Attr, Some(1));
    let elsewhere = w.call_on(root, "default", CallShape::Attr, Some(2));
    let s = solved(&w);
    assert_eq!(macro_error(&s, root, helper), None, "`#[default]` is Default's helper: {:?}", s.errors(root));
    assert_eq!(macro_error(&s, root, typo), Some(Unresolved::NotFound), "a helper no derive declares is refused");
    assert_eq!(macro_error(&s, root, elsewhere), Some(Unresolved::NotFound), "a helper is in scope only on its derive's item");
}
