//! Allocation budgets of the body lints' hot paths, on a crate none of whose bodies lints: the
//! dispatch tables are built without allocating, a body's preorder is built in the buffers of
//! the body before, and the late passes allocate the same at 4 bodies as at 64.

use std::fmt::Write as _;

use blaze_alloc_count::{assert_at_most, measure};

use hir_def::DefWithBodyId;
use hir_expand::fixture as fx;

use super::{BodyCx, Passes, Scratch, after_abort, before_abort, body_owners};
use crate::{
    cx::{Cx, Files},
    lints::registry::LintId,
};

/// Each run of [`super::body_lints_clean`], by the database it ran in, with whether it was
/// of the late passes: what the tests count to say which bodies a check linted, a memo's
/// verification counting for none.
pub(super) static LINTED: blaze_core::lock::Lock<Vec<(base_db::Nonce, DefWithBodyId, bool)>> =
    blaze_core::lock::Lock::new("test.diagnostics.lints.linted", Vec::new());

/// The bodies whose lints ran in `db` since the last take, once for each of the passes
/// before the abort and the late passes, in no order.
fn take_linted(db: &base_db::Db) -> Vec<DefWithBodyId> {
    let nonce = db.nonce_and_revision().0;
    let mut linted = LINTED.lock();
    let mut own = Vec::new();
    linted.retain(|&(of, def, _)| {
        if of == nonce {
            own.push(def);
        }
        of != nonce
    });
    own
}

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
    blaze_query::db::attach(db, || {
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

/// A crate of `functions` functions no lint is found in, each calling `edited`, in two files:
/// `edited` first in `main.rs` with `near` of them after it, the rest in `rest.rs`.
fn two_files(near: usize, functions: usize) -> String {
    let mut text = String::from("//- minicore: sized, copy\n//- /main.rs crate:main\nmod rest;\n");
    text.push_str("pub fn edited() -> u32 { 1 }\n");
    for at in 0..near {
        let _ = writeln!(text, "pub fn near{at}(x: u32) -> u32 {{ let _ = edited(); x }}");
    }
    text.push_str("//- /rest.rs\n");
    for at in 0..functions - near {
        let _ = writeln!(text, "pub fn far{at}(x: u32) -> u32 {{ let _ = super::edited(); x }}");
    }
    text
}

/// `main.rs` of [`two_files`] with another body for `edited`, two lines longer, so every
/// function after it in the file moves.
fn edited_main(near: usize) -> String {
    let mut text =
        String::from("mod rest;\npub fn edited() -> u32 {\n    let two = 2;\n    two\n}\n");
    for at in 0..near {
        let _ = writeln!(text, "pub fn near{at}(x: u32) -> u32 {{ let _ = edited(); x }}");
    }
    text
}

/// How many runs of a body's lints a check of [`two_files`]`(near, functions)` made after
/// `edited`'s body changed, the check before it having linted every body. `kept` is whether
/// the memos the first check made are the ones the second asks; without it the second asks
/// under another key (the passes before the abort as a build that makes no code runs them,
/// and the late passes of another set of lints), which is the planted bug: a check that
/// lints every body again.
fn linted_after_an_edit(near: usize, functions: usize, kept: bool) -> usize {
    let fixture = fx::db(&two_files(near, functions));
    let main = fixture.file_at("/main.rs").file_id(fixture.db());
    let mut db = fixture.into_db();
    let run = |db: &base_db::Db, codegen: bool, args: &[&str]| {
        let args: Vec<String> = args.iter().map(|it| (*it).to_owned()).collect();
        let cmd = crate::lints::CommandLine::parse(&args).expect("the test's lint flags parse");
        blaze_query::db::attach(db, || {
            let krate = hir::check::workspace_crates(db)[0];
            let frame = crate::tests::frame();
            let cx = Cx::new(db, krate, Files::unnamed(), &frame).with_lints(&cmd);
            let early = before_abort(&cx, codegen);
            let late = after_abort(&cx);
            assert!(early.is_empty() && late.is_empty(), "the fixture is clean: {early:?} {late:?}");
        });
    };
    run(&db, true, &[]);
    let first = take_linted(&db).len();
    assert!(first >= 2 * functions, "the first check lints every body: {first} of {functions}");
    db.set_file_text(main, &edited_main(near));
    match kept {
        true => run(&db, true, &[]),
        false => run(&db, false, &["-A", "unused-comparisons"]),
    }
    take_linted(&db).len()
}

/// The invariant: after a body-only edit of one function, the bodies linted again are at
/// most those of the edited file (the edited body, and a body after it whose lints read
/// where it is written, which the edit moved), whatever the size of the rest of the crate:
/// never a body of a file the edit did not touch.
#[test]
fn an_edit_of_one_body_lints_no_body_of_another_file_again() {
    let near = 4;
    let small = linted_after_an_edit(near, 20, true);
    let large = linted_after_an_edit(near, 200, true);
    assert!(small >= 1, "the edited body is linted again");
    assert!(
        small <= 2 * (near + 1),
        "{small} runs for one edited body and {near} bodies after it, each by two sets of passes"
    );
    assert_eq!(large, small, "the lints of an edit run the same among 20 bodies and 200");
}

/// The control of [`an_edit_of_one_body_lints_no_body_of_another_file_again`]: a check that
/// asks no memo the check before it made (another key) lints every body, and is told from
/// one that lints the edited file's.
#[test]
fn the_oracle_catches_a_check_that_lints_every_body() {
    let near = 4;
    let small = linted_after_an_edit(near, 20, false);
    let large = linted_after_an_edit(near, 200, false);
    assert!(small >= 2 * 20 && large >= 2 * 200, "the planted check lints all: {small}, {large}");
    assert_ne!(large, small, "the oracle tells a check that grows with the crate");
}

/// A body a lint is found in is not said clean, and the check gives its lint as before: the
/// memo decides only which bodies the check lints, never what it says.
#[test]
fn a_body_with_a_lint_is_linted_by_the_check_and_its_lint_given() {
    let checked = crate::tests::checked(
        "//- minicore: sized, copy\n//- /main.rs crate:main\n\
         pub fn clean(x: u32) -> u32 { x }\n\
         pub fn unused() { let x = 1; }\n",
    );
    assert!(
        checked.messages().iter().any(|it| it.contains("unused variable: `x`")),
        "the check gives the body's lint: {:?}",
        checked.messages()
    );
}
