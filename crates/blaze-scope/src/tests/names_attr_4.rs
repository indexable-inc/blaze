//! names-attr-4: a derive's helper attributes on an item, its variants and its fields reach the
//! cells on the real path; a built-in derive's helpers are kept in the crate's record.
//!
//! The Reference, "Procedural Macros", "Derive macro helper attributes": "Derive macros can add
//! additional attributes into the scope of the item they are on. Said attributes are called
//! derive macro helper attributes. These attributes are inert". The pinned library declares a
//! built-in derive's the same way: core/src/default.rs `#[rustc_builtin_macro(Default,
//! attributes(default))] pub macro Default($item:item)`. A one-segment attribute no scope binds
//! is "cannot find attribute" (rustc's E0433 family; here `Unresolved::NotFound`).

use std::string::String;
use std::vec::Vec;

use blaze_id::ItemKey;
use blaze_unit_graph::{Implicit, Prelude, StdIs};

use super::entry::{Texts, input, resolved};
use crate::crate_scope::{Ready, read_record, resolve_by_records};
use crate::graph::Sysroot;
use crate::resolve::{CrateInput, CrateNames, aligned};
use crate::task::{MacroBody, Serial};
use crate::{CallShape, ScopeError, Unresolved};

const WAIT: std::time::Duration = std::time::Duration::from_secs(60);

/// The attribute calls of `names`' root named `path`, by call.
fn attr_calls(names: &CrateNames<'_>, path: &str) -> Vec<ItemKey> {
    let decls = &names.decls.get(&names.root).expect("the root is lowered").1;
    decls.calls.iter().filter(|c| c.kind == CallShape::Attr && c.path.segments.join("::") == path).map(|c| c.call).collect()
}

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

/// A `core` of two built-in derives: `Default` with its helper, `Clone` with none.
const DEP: &str = "#![no_core]
#[rustc_builtin_macro(Default, attributes(default))]
pub macro Default($item:item) {}
#[rustc_builtin_macro]
pub macro Clone($item:item) {}
";

const USER: &str = "#![no_core]
use dep::{Clone, Default};
#[derive(Default)]
pub enum Right {
    #[default]
    A,
    B,
}
#[derive(Default)]
pub enum Wrong {
    #[defualt]
    A,
}
#[derive(Clone)]
pub struct Plain {
    #[default]
    pub f: u8,
}
";

static TEXTS: Texts = Texts(&[&[("/user/lib.rs", USER)], &[("/dep/lib.rs", DEP)]]);

/// Rule: `#[default]` on a variant of an enum deriving the built-in `Default` is that derive's
/// helper, inert; `#[defualt]` there names nothing, and `#[default]` on a field of a struct
/// deriving only `Clone` names nothing either: a helper is in scope only on an item a derive
/// declaring it is applied to. Through the record path the command takes
/// (`crate_scope::resolve_by_records`: the user crate reads `dep`'s `CrateScope` record), whose
/// macro rows now keep a built-in derive's `attributes(..)` (`MacroBody::BuiltinDerive`).
///
/// Expected: "test blaze-scope: exit 0; 1 passed, 0 failed". Controls: without the lowering's
/// `Lower::helper_checks` no `default` call is lowered ("each helper use reaches the cells") and
/// the misspelling is not reported; with the record keeping no helpers (`helpers: ""` written for
/// `BuiltinDerive`) the `#[default]` of `Right` fails NotFound too (two `default` errors).
#[test]
fn a_built_in_derive_s_helper_is_inert_on_its_item_alone_through_the_records() {
    let frame = blaze_mem::Frame::new(blaze_mem::Tag::Nameres).expect("frame");
    let erased = blaze_own::Erased::new(&frame);
    let bump = erased.bump();
    let alloc = bump.alloc();
    let crates = [input("user", "/user/lib.rs", &[("dep", 1)]), input("dep", "/dep/lib.rs", &[])];
    let (all, _) = resolve_by_records(&crates, Sysroot::default(), &TEXTS, &Serial, &Ready::serial(WAIT), bump)
        .unwrap_or_else(|e| panic!("by records: {e:?}"));

    // The record keeps the library's helper list.
    let dep = read_record(aligned(&all[1].record, bump), alloc).unwrap_or_else(|e| panic!("dep's record reads: {e:?}"));
    let bodies: Vec<(&str, MacroBody<'_>)> = dep.macros.values().map(|m| (m.name, m.body)).collect();
    assert!(
        bodies.iter().any(|(name, body)| *name == "Default" && matches!(body, MacroBody::BuiltinDerive { helpers: "default" })),
        "`Default`'s helpers are in dep's record: {bodies:?}"
    );
    assert!(
        bodies.iter().any(|(name, body)| *name == "Clone" && matches!(body, MacroBody::BuiltinDerive { helpers: "" })),
        "`Clone` is a built-in derive with no helpers: {bodies:?}"
    );

    let user = all[0].names;
    assert_eq!(attr_calls(user, "default").len(), 2, "each helper use reaches the cells, once per item");
    assert_eq!(attr_calls(user, "defualt").len(), 1, "the misspelled helper reaches the cells");
    assert_eq!(
        macro_errors(user),
        [(String::from("default"), Unresolved::NotFound), (String::from("defualt"), Unresolved::NotFound)],
        "only `Plain`'s `#[default]` (Clone has no helper) and `#[defualt]` fail; notes {:?}",
        user.notes
    );
}

/// The proc-macro crate: a derive whose helper is `serde`.
const MACROS: &str = "#![no_core]
#[proc_macro_derive(Ser, attributes(serde))]
pub fn ser(item: TokenStream) -> TokenStream { item }
";

const PROC_USER: &str = "#![no_core]
use macros::Ser;
#[derive(Ser)]
#[serde(crate = \"x\")]
pub struct Point {
    #[serde(rename = \"x\")]
    pub x: u8,
    #[serdr]
    pub y: u8,
    #[cfg(any())]
    #[gone]
    pub z: u8,
}
#[derive(Ser)]
pub enum Shape {
    #[serde(rename = \"c\")]
    Circle { #[serde(default)] r: u8 },
}
";

static PROC_TEXTS: Texts = Texts(&[&[("/user/lib.rs", PROC_USER)], &[("/macros/lib.rs", MACROS)]]);

/// Rule: a proc derive's declared helper (`attributes(serde)`, serde_derive's shape) is inert on
/// the item, on a field, on a variant and on a variant's field; an undeclared one on a field
/// (`#[serdr]`) is refused; an attribute on a field a false `cfg` removes is never read (rustc
/// strips the field before the derive sees the item). No runner is given: the derive resolves and
/// its helpers are read from its definition, which is all the helper scope needs.
///
/// Expected: "test blaze-scope: exit 0; 1 passed, 0 failed". Controls: without names-attr-3's
/// helper scope (`Cells::derive_helper`) every `serde` fails NotFound; without the lowering's
/// `Lower::helper_checks` no `serde` or `serdr` call is lowered.
#[test]
fn a_proc_derive_s_helper_is_inert_on_its_item_fields_and_variants() {
    let implicit = Implicit { std: StdIs::Rust, prelude: Prelude::Rust(edition::Edition::Edition2021), proc_macro: true, entry: false };
    let crates = [input("user", "/user/lib.rs", &[("macros", 1)]), CrateInput { implicit, ..input("macros", "/macros/lib.rs", &[]) }];
    let names = resolved(&crates, &PROC_TEXTS);
    let user = names.krate(0);
    assert_eq!(attr_calls(user, "serde").len(), 2, "one `serde` check per derived item");
    assert_eq!(attr_calls(user, "serdr").len(), 1, "the undeclared name reaches the cells");
    assert!(attr_calls(user, "gone").is_empty(), "a field a false `cfg` removes is not read");
    assert_eq!(
        macro_errors(user),
        [(String::from("serdr"), Unresolved::NotFound)],
        "only the undeclared helper fails; notes {:?}",
        names.notes(0)
    );
}
