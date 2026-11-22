//! attr-bang-kind-1/-2: an attribute macro written after a derive on the item itself takes the item.
//!
//! tempo's names census l3f/l3g (2026-11-16): 54 new "a proc macro's (not expanded yet):
//! attribute" rows, all of this shape. alloy-provider 2.5.0
//! src/provider/watch_canonical_blocks_from.rs:
//!
//! ```text
//! use pin_project::pin_project;
//! #[derive(Debug)]
//! #[pin_project]
//! pub struct WatchCanonicalBlocksFromStream<N: Network> { .. #[pin] stream: .. }
//! ```
//!
//! and reth-eth-wire-types' `#[derive(..)] #[add_arbitrary_tests(rlp)] pub struct ..`. rustc
//! takes an item's first non-built-in attribute (`InvocationCollector::take_first_attr`): the
//! derive, then `#[pin_project]`, which, being no helper of a derive before it (the Reference,
//! "Derive macro helper attributes"), resolves in the macro namespace to the attribute macro and
//! expands with the item as its input. `#[pin]` on the field is then pin_project's input, never
//! a name of its own. On a field or a variant an attribute macro is refused ("expected non-macro
//! attribute, found attribute macro").

use std::string::String;
use std::vec::Vec;

use blaze_unit_graph::{Implicit, Prelude, StdIs};

use super::entry::{Texts, input, resolved};
use crate::resolve::{CrateInput, CrateNames};
use blaze_surface::CallRole;

use crate::{CallShape, ScopeError, Unresolved};

/// A proc-macro crate of pin-project's and serde's shapes: an attribute macro, and a derive
/// declaring the helper `serde`.
const MACROS: &str = "#![no_core]
#[proc_macro_attribute]
pub fn pin_project(args: TokenStream, item: TokenStream) -> TokenStream { item }
#[proc_macro_derive(Ser, attributes(serde))]
pub fn ser(item: TokenStream) -> TokenStream { item }
#[proc_macro_derive(Other)]
pub fn other(item: TokenStream) -> TokenStream { item }
";

const USER: &str = "#![no_core]
use macros::{pin_project, Ser};
#[derive(Ser)]
#[pin_project]
pub struct Taken {
    #[pin]
    pub f: u8,
}
#[derive(Ser)]
#[serde(crate = \"x\")]
pub struct Kept {
    #[pin_project]
    pub g: u8,
}
";

static TEXTS: Texts = Texts(&[&[("/user/lib.rs", USER)], &[("/macros/lib.rs", MACROS)]]);

/// The root's macro errors, as (path, why), sorted.
fn macro_errors(names: &CrateNames<'_>) -> Vec<(String, Unresolved)> {
    let errors = &names.done.get(&names.root).expect("the root is done").scope.errors;
    let mut out: Vec<(String, Unresolved)> = errors
        .iter()
        .filter_map(|e| match e {
            ScopeError::Macro { path, why, .. } => Some((path.segments.join("::"), *why)),
            _ => None,
        })
        .collect();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

/// Rule: `#[pin_project]` after `#[derive(Ser)]` on `Taken` is no helper of `Ser`, so it is the
/// attribute macro that takes `Taken` (a call whose input is the item, the derive cut from it as
/// rustc cuts it); `#[pin]` on `Taken`'s field is that macro's input, inert, never an error;
/// `#[serde]` on `Kept` is `Ser`'s helper, inert, and `Kept` is lowered; `#[pin_project]` on
/// `Kept`'s field is an attribute macro in a field's place, refused.
///
/// Expected after attr-bang-kind-2: "1 passed, 0 failed". Before it (names-attr-4's stated
/// limit: `Cells::called`'s `after_derive` arm) it fails at "only the field's attribute macro is
/// refused" with `[("pin", NotFound), ("pin_project", WrongMacroKind), ("pin_project",
/// WrongMacroKind)]`. Controls: with `Cells::item_fate` answering `Open` always, `pin` fails
/// NotFound again; with `Cells::called`'s `is_taker` removed, the item's `pin_project` is
/// refused WrongMacroKind again (two `pin_project` errors).
#[test]
fn an_attribute_macro_after_a_derive_takes_its_item() {
    let alloc = blaze_own::testing::heap().alloc();
    let implicit = Implicit { std: StdIs::Rust, prelude: Prelude::Rust(edition::Edition::Edition2021), proc_macro: true, entry: false };
    let crates = [input("user", "/user/lib.rs", &[("macros", 1)]), CrateInput { implicit, ..input("macros", "/macros/lib.rs", &[]) }];
    let names = resolved(&crates, &TEXTS);
    let user = names.krate(0);
    let decls = &user.decls.get(&user.root).expect("the root is lowered").1;
    let takers: Vec<_> = decls
        .calls
        .iter()
        .filter(|c| c.kind == CallShape::Attr && c.role == CallRole::AfterDerive && c.path.segments.join("::") == "pin_project")
        .map(|c| c.call)
        .collect();
    assert_eq!(takers.len(), 1, "`#[pin_project]` on `Taken` is one taker call");
    let input_text = user.tables.calls.get(&takers[0]).unwrap_or_else(|| panic!("call {:?} has its source", takers[0])).input;
    assert!(input_text.contains("pub struct Taken"), "the taker's input is the item: {input_text:?}");
    assert_eq!(
        decls.calls.iter().filter(|c| c.role == CallRole::Keep).count(),
        2,
        "each derived item with a one-segment attribute on itself has its keep call"
    );

    // End to end: the macro's input as the driver cuts it (resolve.rs `expand_decls`' attribute
    // walk, blaze-expand's `ItemAttrs::attr_call`) holds no `#[derive(..)]` and not the attribute.
    let stream = blaze_expand::lex(input_text, crate::Edition::E2021, alloc).unwrap_or_else(|e| panic!("the input lexes: {e:?}"));
    let mut walk = blaze_expand::ItemAttrs::new(stream);
    let attr = match walk.next(&mut |_, _| Ok::<bool, core::convert::Infallible>(false)) {
        Ok(blaze_expand::NextAttr::Call { path, attr }) if path.as_str() == "pin_project" => attr,
        other => panic!("the first attribute call is `pin_project`: {other:?}"),
    };
    let cut = walk.attr_call(attr).unwrap_or_else(|e| panic!("the call cuts: {e:?}"));
    assert_eq!(cut.derives_taken, 1, "`Ser` is cut from the input and runs before the macro");
    let words: Vec<&str> = (0..cut.item.len()).map(|t| cut.item.text_at(t)).collect();
    assert!(!words.contains(&"derive"), "the macro's input has no `#[derive(..)]`: {words:?}");
    assert!(!words.contains(&"pin_project"), "the macro's input has not its own attribute: {words:?}");
    assert!(words.contains(&"Taken") && words.contains(&"pin"), "the item, its field's `#[pin]` kept: {words:?}");

    assert_eq!(
        macro_errors(user),
        [(String::from("pin_project"), Unresolved::WrongMacroKind)],
        "only the field's attribute macro is refused; notes {:?}",
        names.notes(0)
    );
}

const LATE_USER: &str = "#![no_core]
use macros::{pin_project, Other, Ser};
#[derive(Ser)]
#[pin_project]
#[derive(Other)]
pub struct Late {
    pub f: u8,
}
#[derive(Ser)]
#[serde(crate = \"x\")]
#[derive(Other)]
pub struct Fine {
    pub g: u8,
}
";

static LATE_TEXTS: Texts = Texts(&[&[("/user/lib.rs", LATE_USER)], &[("/macros/lib.rs", MACROS)]]);

/// Rule (the Reference, "Attributes": an item's attributes are taken in written order): a
/// `#[derive]` written after an attribute macro stays on the item the macro takes, unexpanded,
/// and runs once, in the macro's output. The lowering sends the derives written BEFORE the
/// macro, then the macro (`AfterDerive`), never the later derive beside it (lane AD1, task
/// lower.rs `AttrOrder::Written`); so the macro is no longer refused `DeriveAfterAttrMacro`
/// (that refusal stood while every derive of an item was sent at once: tests/attr_order.rs
/// `the_old_order_refuses_the_tempo_shape` is its control). A derive's helper between two
/// derives (`Fine`) is inert and refused nothing.
#[test]
fn a_derive_after_an_attribute_macro_is_refused() {
    let implicit = Implicit { std: StdIs::Rust, prelude: Prelude::Rust(edition::Edition::Edition2021), proc_macro: true, entry: false };
    let crates = [input("user", "/user/lib.rs", &[("macros", 1)]), CrateInput { implicit, ..input("macros", "/macros/lib.rs", &[]) }];
    let names = resolved(&crates, &LATE_TEXTS);
    let user = names.krate(0);
    let decls = &user.decls.get(&user.root).expect("the root is lowered").1;
    let roles: Vec<(String, CallRole)> = decls
        .calls
        .iter()
        .filter(|c| c.kind == CallShape::Attr && c.role != CallRole::Call)
        .map(|c| (c.path.segments.join("::"), c.role))
        .collect();
    assert_eq!(
        roles,
        [
            (String::from("pin_project"), CallRole::AfterDerive),
            (String::from("derive"), CallRole::Keep),
            (String::from("serde"), CallRole::AfterDerive),
            (String::from("derive"), CallRole::Keep),
        ],
        "each name is sent after the derives written before it, each item has its keep call"
    );
    let errors = macro_errors(user);
    assert!(
        errors.iter().all(|(_, why)| *why != Unresolved::DeriveAfterAttrMacro),
        "no macro is refused for a derive written after it: {errors:?}; notes {:?}",
        names.notes(0)
    );
}
