//! `needless_borrows_for_generic_args` of a closure passed by reference where the parameter's
//! bound is higher-ranked: `impl Fn(&Key) -> bool` is `for<'a> F: Fn<(&'a Key,)>`, which the
//! closure meets itself, binder and all. blaze's `driver.rs` passes `&mergeable` to `dedupe`
//! this way; taking the bound out of its binder made `{closure}: Fn<(&'^0 Key,)>`, and the
//! solver's `Binder::dummy` of it panicked (`has escaping bound vars`), refusing the crate.

use crate::tests::{CLIPPY_OFF, lint};

const MESSAGE: &str = "the borrowed expression implements the required traits";

#[test]
fn closure_by_reference_meets_a_higher_ranked_fn_bound() {
    let fixture = r#"
//- minicore: sized, copy, clone, fn
//- /src/lib.rs crate:main
pub struct Key(u32);
pub struct Nodes<T>(T);
impl<T> Nodes<T> {
    pub fn has(&self, _: &Key) -> bool {
        true
    }
}
fn dedupe(_: impl Fn(&Key) -> bool) {}
fn dedupe_named<F: Fn(&Key) -> bool>(_: F) {}
pub fn finish<T>(nodes: &Nodes<T>) {
    let mergeable = |key: &Key| nodes.has(key);
    dedupe(&mergeable);
    dedupe_named(&mergeable);
}
"#;
    let out = lint(fixture, &format!("{CLIPPY_OFF} -W clippy::needless_borrows_for_generic_args"));
    // Both calls: the closure is `Copy` (it captures `&Nodes<T>`) and meets the bound itself.
    assert_eq!(out.matches(MESSAGE).count(), 2, "{out}");
    assert!(out.contains("`mergeable`"), "{out}");
}
