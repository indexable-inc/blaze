//! The importer's refusal names its caller (census 13, Brief 12): a portable term naming
//! `Param(i)` instantiated with fewer than `i + 1` arguments is `Malformed::Param`, and its `at`
//! is the line that asked for the import (through `#[track_caller]` on `fold::import`, the
//! `Cx` helpers and eval's and coherence's `import_pred`), so a census line says which rule
//! gave the short list instead of only that one was short.

use blaze_own::Alloc;

use super::{Inst, Scratch, import};
use crate::error::{Malformed, SolveError};
use crate::facts::Portable;
use crate::term::{Ty, TyKind};
use crate::testing::tys::TestTys;
use crate::tys::Tys;

/// `Param(0)` imported with an empty argument list is refused as `Malformed::Param { index: 0,
/// args: 0, .. }` whose `at` is this test's call of `fold::import` (its file and line), with no
/// goal (the driver fills that, `eval::Search::drive`); the same term with one argument is that
/// argument. A refusal that names fold.rs's own line (no `#[track_caller]` on `import` or
/// `import_walk`) fails the `at` asserts; an import that guesses a term for the missing
/// argument fails the match.
///
/// Expected line: "test fold::tests::param_refusal_names_its_caller ... ok".
#[test]
fn param_refusal_names_its_caller() {
    let mut tys = TestTys::new();
    let mut s = Scratch::new_in(Alloc::global());
    let nodes = [TyKind::Param(0)];
    let p = Portable { nodes: &nodes, lists: &[] };

    // Clean: one argument, so `Param(0)` is it.
    let bool_ = match tys.mk(TyKind::Bool) {
        Ok(t) => t,
        Err(e) => panic!("the test arena refused `bool`: {e:?}"),
    };
    match import(&mut tys, &mut s, p, Ty(0), Inst { args: &[bool_], bound: &[] }) {
        Ok(t) => assert_eq!(t, bool_, "`Param(0)` with `[bool]` is not `bool`"),
        Err(e) => panic!("`Param(0)` with one argument was refused: {e:?}"),
    }

    // Refused: no argument for `Param(0)`; the refusal names this line.
    let line = line!() + 1;
    let got = import(&mut tys, &mut s, p, Ty(0), Inst { args: &[], bound: &[] });
    match got {
        Err(SolveError::Malformed(Malformed::Param { index: 0, args: 0, at, goal: None })) => {
            assert_eq!(at.file(), file!(), "the refusal names {at}, not this test's call");
            assert_eq!(at.line(), line, "the refusal names {at}, not this test's call");
        }
        other => panic!("`Param(0)` with no arguments gave {other:?}, not `Malformed::Param {{ index: 0, args: 0, .. }}`"),
    }
}
