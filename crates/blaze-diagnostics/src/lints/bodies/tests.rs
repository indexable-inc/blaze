//! Allocation budgets of the body lints' hot paths, on a crate none of whose bodies lints: the
//! dispatch tables are built without allocating, a body's preorder is built in the buffers of
//! the body before, and the late passes allocate the same at 4 bodies as at 64.

use std::fmt::Write as _;

use blaze_alloc_count::{assert_at_most, measure};

use super::{BodyCx, Passes, Scratch, after_abort, body_owners};
use crate::{cx::Cx, lints::registry::LintId};

/// A crate of `n` functions that use what the late passes look at (calls, method calls,
/// comparisons, literals, `for`, `if`, `match`, `let`) and give no diagnostic.
fn fixture(n: usize) -> String {
    let mut text = String::from("//- /main.rs crate:main\n");
    for i in 0..n {
        let _ = writeln!(
            text,
            "pub fn f{i}(x: u32, v: &[u32]) -> u32 {{
    let mut total = x;
    for &it in v {{
        if it > 3 {{ total += it; }} else {{ total = total.wrapping_add(1); }}
    }}
    match total {{ 0 => 1, n => n * 2 }}
}}"
        );
    }
    text.push_str("fn main() {}\n");
    text
}

/// `check` given the context of the first workspace crate of `fixture(n)`, with the crate's
/// lint levels and its bodies' queries asked once, so that what is measured is the lints'.
fn with_cx(n: usize, check: impl FnOnce(&Cx<'_, '_>)) {
    let checked = crate::tests::checked(&fixture(n));
    let db = &checked.db;
    hir::attach_db(db, || {
        let krate = hir::check::workspace_crates(db)[0];
        let frame = crate::tests::frame();
        let cx = Cx::new(db, krate, checked.files(), &frame);
        cx.lint_may_fire(LintId::WARNINGS);
        assert!(after_abort(&cx).is_empty(), "the fixture gives no late lint");
        check(&cx);
    });
}

/// The tables of which passes run at which hook and tag are fixed arrays, and a pass's state
/// is made without allocating.
#[test]
fn dispatch_tables() {
    with_cx(1, |cx| {
        let passes = Passes::new(cx);
        assert!(passes.any, "a late pass runs at default levels");
        assert_at_most(0, "the late passes' dispatch tables", || Passes::new(cx));
    });
}

/// A body's preorder, asked of each body in turn, reuses the buffers the bodies before grew.
#[test]
fn preorder_reuses_scratch() {
    with_cx(8, |cx| {
        let owners = body_owners(cx);
        let walk = |scratch: &mut Scratch| {
            let mut nodes = 0;
            for &owner in &owners {
                let Some(mut b) = BodyCx::new(cx, owner) else { continue };
                b.lend(std::mem::take(scratch));
                nodes += b.exprs().len();
                *scratch = b.into_scratch();
            }
            nodes
        };
        let mut scratch = Scratch::default();
        let nodes = walk(&mut scratch);
        assert_ne!(nodes, 0, "the fixture has bodies");
        assert_at_most(0, "the preorders of bodies after the first", || walk(&mut scratch));
    });
}

/// The late passes over a crate without a diagnostic allocate per crate, not per body: 60
/// bodies more add none.
#[test]
fn late_passes_per_crate() {
    let used = |n: usize| {
        let mut used = None;
        with_cx(n, |cx| used = Some(measure(|| after_abort(cx)).1));
        used.expect("the fixture was checked")
    };
    let (small, large) = (used(4), used(64));
    assert!(
        large.allocations <= small.allocations,
        "the late passes allocate per body: {} allocations at 4 bodies, {} at 64",
        small.allocations,
        large.allocations
    );
}
