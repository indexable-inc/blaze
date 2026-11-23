//! Class P: a written attribute macro over the root `fn main` (`#[bstd::main]` by hand) that
//! writes no `main` must name that call, never leave a silent "no `main`" (main_call.rs; rules
//! 19, 54). The Reference, "Procedural Macros", "Attribute macros": "The item to which the
//! attribute is applied is replaced by the zero or more items in the returned TokenStream", so
//! the written `fn main` is gone and only the call can say why.
//!
//! CONTROL (rule 8): a crate with no `fn main` at all joins no call (`MainCall::of` is `None`,
//! front2 keeps `NoMain`); with `takes_root_main` answering false, the first two tests fail with
//! "the call over `fn main` is named".

use std::format;

use blaze_own::Alloc;
use blaze_surface::{Namespace, PathKind};
use blaze_unit_graph::Implicit;

use super::entry::{Texts, input};
use crate::graph::Sysroot;
use crate::main_call::{MainCall, MainCallWhy};
use crate::names::Written;
use crate::resolve::{CrateInput, Names, resolve_crates};
use crate::task::Serial;

/// `std` whose attribute `main` takes `fn main()` and writes `granted_entry`, no `main`.
const STD: &str = "#![no_core]
#[macro_export]
macro_rules! main {
    attr() ($vis:vis fn main() $body:block) => {
        pub fn granted_entry() {}
    };
}
";

/// A written `#[std::main]` over `fn main`: it expands, and writes no `main`.
const WRITTEN: &str = "#![no_core]
#[std::main]
fn main() {}
";

/// A written attribute whose path names nothing.
const UNRESOLVED: &str = "#![no_core]
#[nothere::main]
fn main() {}
";

/// No `fn main` at all.
const NO_MAIN: &str = "#![no_core]
fn other() {}
";

static TEXTS: Texts = Texts(&[
    &[("/written/main.rs", WRITTEN)],
    &[("/std/lib.rs", STD)],
    &[("/unresolved/main.rs", UNRESOLVED)],
    &[("/nomain/main.rs", NO_MAIN)],
]);

const DEPS: &[(&str, usize)] = &[("std", 1)];

fn names() -> Names<'static> {
    let entry = |mut c: CrateInput<'static>| {
        c.implicit = Implicit { entry: true, ..c.implicit };
        c
    };
    let crates: [CrateInput<'static>; 4] = [
        entry(input("written", "/written/main.rs", DEPS)),
        input("std", "/std/lib.rs", &[]),
        entry(input("unresolved", "/unresolved/main.rs", DEPS)),
        entry(input("nomain", "/nomain/main.rs", DEPS)),
    ];
    resolve_crates(&crates, Sysroot::default(), &TEXTS, &Serial, super::SyncFrame::leaked()).unwrap_or_else(|e| panic!("resolve_crates: {e}"))
}

fn main_misses(names: &Names<'static>, krate: usize) {
    let alloc: Alloc<'static> = blaze_own::testing::heap().alloc();
    let found = names.path(names.root(krate), Namespace::Values, Written { kind: PathKind::Plain, segments: &["main"] }, alloc);
    assert!(found.is_err(), "crate {krate}: `main` must be missing from the root");
}

/// `#[std::main] fn main() {}`: the call ran and wrote `granted_entry`, no `main`. The miss
/// names `#[std::main]` and says it wrote no `main`. Passing prints
/// "test tests::main_call::a_written_attribute_that_writes_no_main_is_named ... ok".
#[test]
fn a_written_attribute_that_writes_no_main_is_named() {
    let names = names();
    main_misses(&names, 0);
    let call = MainCall::of(&names, 0).expect("the call over `fn main` is named");
    assert_eq!((call.path, call.implicit, call.why), ("std::main", false, MainCallWhy::WroteNoMain));
    let shown = format!("{call}");
    assert!(shown.contains("`#[std::main]`") && shown.contains("declares no `main`"), "{shown}");
}

/// `#[nothere::main] fn main() {}`: the attribute's path names nothing, so the call never ran
/// and `fn main` is no item. Passing prints
/// "test tests::main_call::an_unresolved_attribute_over_main_is_named ... ok".
#[test]
fn an_unresolved_attribute_over_main_is_named() {
    let names = names();
    main_misses(&names, 2);
    let call = MainCall::of(&names, 2).expect("the call over `fn main` is named");
    assert_eq!(call.path, "nothere::main");
    assert!(matches!(call.why, MainCallWhy::Unresolved { segment: 0, .. }), "{call:?}");
    let shown = format!("{call}");
    assert!(shown.contains("`#[nothere::main]`") && shown.contains("`nothere`") && shown.contains("does not resolve"), "{shown}");
}

/// CONTROL: no `fn main` at all: no call is joined (front2 says `NoMain`). Passing prints
/// "test tests::main_call::a_crate_without_fn_main_joins_no_call ... ok".
#[test]
fn a_crate_without_fn_main_joins_no_call() {
    let names = names();
    main_misses(&names, 3);
    assert_eq!(MainCall::of(&names, 3), None);
}
