//! A crate root under a false `#![cfg]` is an empty crate (turso, c1814:
//! testing/stress/tests/shuttle_mvcc.rs opens with `#![cfg(shuttle)]` and then
//! `use shuttle::scheduler::..;`; the test unit was walked and lowered whole, and `shuttle`,
//! a `cfg(shuttle)` dependency, was "no `shuttle` in this scope" twice in each of two files).
//!
//! The Reference, "Conditional compilation", cfg.attr.crate-level-attrs: "When a crate-level
//! cfg has a false predicate, the crate itself still exists. Any crate attributes preceding the
//! cfg are kept, and any crate attributes following the cfg are removed as well as removing all
//! of the following crate contents."

use std::vec::Vec;

use blaze_surface::ScopeError;

use super::entry::{Texts, input, resolved};

/// The reduced shuttle_mvcc.rs: an import of the cfg-gated dependency, a body naming a macro of
/// it, and an inline module whose import fails the same way.
const OFF: &str = "#![no_core]
#![cfg(nothing)]
use shuttle::scheduler::Task;
pub fn f() { missing::m!(); }
mod inner { use gone_inner::x; }
";

/// The twin: the same crate under a true `#![cfg]`.
const ON: &str = "#![no_core]
#![cfg(not(nothing))]
use shuttle::scheduler::Task;
pub fn f() { missing::m!(); }
mod inner { use gone_inner::x; }
";

/// The control: the same crate with no `#![cfg]` line.
const PLAIN: &str = "#![no_core]
use shuttle::scheduler::Task;
pub fn f() { missing::m!(); }
mod inner { use gone_inner::x; }
";

static TEXTS: Texts = Texts(&[&[("/off/lib.rs", OFF)], &[("/on/lib.rs", ON)], &[("/plain/lib.rs", PLAIN)]]);

/// Each crate's unresolved paths' first segments, sorted, and how many modules it has.
fn said(at: usize) -> (Vec<&'static str>, usize) {
    let crates = [input("off", "/off/lib.rs", &[]), input("on", "/on/lib.rs", &[]), input("plain", "/plain/lib.rs", &[])];
    let names = resolved(&crates, &TEXTS);
    let c = names.krate(at);
    let mut firsts: Vec<&'static str> = Vec::new();
    for r in c.done.values() {
        for error in r.scope.errors.iter() {
            match error {
                ScopeError::Import { path, .. } | ScopeError::Macro { path, .. } => firsts.push(path.segments.first().copied().unwrap_or("")),
                _ => {}
            }
        }
    }
    firsts.sort_unstable();
    (firsts, c.done.len())
}

/// Under the false `#![cfg]` the crate is its root alone, with no error: no import, body or
/// module of it is read. TWIN: under a true one, `shuttle`, `missing` and `gone_inner` are each
/// unresolved and `inner` is a module. CONTROL: with no `#![cfg]`, the same as the twin, so the
/// line, and nothing else of the text, is what empties the crate. Before the fix the false one
/// said what the twin says. Prints "CRATE CFG OFF: an empty crate, no error".
#[test]
fn a_false_crate_level_cfg_leaves_an_empty_crate_with_no_error() {
    let (off, off_modules) = said(0);
    assert!(off.is_empty(), "a crate under a false `#![cfg]` named {off:?}");
    assert!(off_modules <= 1, "the crate is its root at most, nothing under it: {off_modules} resolved");
    let (on, on_modules) = said(1);
    assert_eq!(on, ["gone_inner", "missing", "shuttle"], "TWIN: a true `#![cfg]` keeps the crate");
    assert!(on_modules > off_modules, "TWIN: `inner` (and the body's block) resolved: {on_modules}");
    let (plain, plain_modules) = said(2);
    assert_eq!((plain, plain_modules), (on, on_modules), "CONTROL: no `#![cfg]` is the twin");
    std::println!("CRATE CFG OFF: an empty crate, no error");
}
