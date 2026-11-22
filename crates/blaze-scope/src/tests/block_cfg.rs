//! A body's block module keeps only what the configuration keeps (tempo, 2026-11-16: unresolved
//! macro paths 0 -> 911 after statement calls made blocks modules; hyper's `trace!` expands to
//! `#[cfg(feature = "tracing")] { tracing::trace!(..); }`, quick-xml's `#[test] fn pi() { use
//! pretty_assertions::assert_eq; .. }`).
//!
//! The Reference, "Conditional compilation": a `cfg` attribute whose predicate is false removes
//! the thing it is on, statements and block expressions included ("Expression attributes":
//! outer attributes before a statement or a block expression are allowed, `cfg` among them).
//! "Testing attributes", `attributes.testing.test.enabled`: a `#[test]` function is compiled
//! only in test mode; this configuration is not one (`cfg(test)` is false here,
//! `task::lower::KNOWN_FALSE`).

use std::vec::Vec;

use blaze_surface::ScopeError;

use super::entry::{Texts, input, resolved};

const TEXT: &str = "#![no_core]
macro_rules! traced { () => { #[cfg(nothing)] { absent::trace!(); } } }
pub fn f() { #[cfg(nothing)] { missing::m!(); } }
pub fn g() { traced!(); }
pub fn h() { #[cfg(nothing)] gone_too::m!(); }
pub fn k() { #[cfg(nothing)] let s = { run(|| { gone_let::m!(); }) }; }
pub fn l() { #[cfg(nothing)] run(|| { gone_arg::m!(); }); { kept::m!(); } }
#[test]
fn t() { use gone::x; struct S; }
pub fn n() { macro mm($t:ty) { impl Tr for $t {} } mm!(u8); }
pub trait Tr {}
#[cfg_attr(nothing, allow(dead_code))]
pub fn control() { present::p!(); }
";

static TEXTS: Texts = Texts(&[&[("/user/lib.rs", TEXT)]]);

/// Only the controls' `present::p!` and `kept::m!` are unresolved: a block or statement under a
/// false `cfg`, in a body or in an expansion, a block inside a `let` or an expression statement
/// under one (tokio's `#[cfg(..)] let resource_span = { .. in_scope(|| { tracing::trace!(..);
/// }) .. };`), and a `#[test]` function's body give no path at all. Before the fix: `missing`,
/// `absent`, `gone`, `gone_let` and `gone_arg` are unresolved too. A `macro` (`decl_macro`)
/// written in a body names its calls there (std's `macro spec_str` in `OsString::push`);
/// before `NestedKind::Macro`, `mm` is unresolved.
/// Prints: "BLOCK CFG: only the control is unresolved".
#[test]
fn a_false_cfg_block_and_a_test_body_give_no_unresolved_path() {
    let crates = [input("user", "/user/lib.rs", &[])];
    let names = resolved(&crates, &TEXTS);
    let c = names.krate(0);
    let mut firsts: Vec<&str> = Vec::new();
    for r in c.done.values() {
        for error in r.scope.errors.iter() {
            match error {
                ScopeError::Import { path, .. } | ScopeError::Macro { path, .. } => firsts.push(path.segments.first().copied().unwrap_or("")),
                _ => {}
            }
        }
    }
    firsts.sort_unstable();
    // `kept`: a block statement after a false-`cfg` statement is not under its attributes.
    assert_eq!(firsts, ["kept", "present"], "unresolved paths' first segments");
    std::println!("BLOCK CFG: only the control is unresolved");
}
