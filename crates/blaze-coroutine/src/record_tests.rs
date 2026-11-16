//! The adapter on three bodies, each given as the scope events the builder's walk makes for it
//! (record.rs, "Who calls it"), compared with the printed form and the printed plan.
//!
//! The expectations are the rules, not a compiler's output. The Reference, "Await expressions",
//! steps 3 to 5: the future tmp is pinned and polled, and on `Poll::Pending` "execution returns
//! to step 3": the resume block is the head that pins and polls. "Destructors": "When control
//! flow leaves a drop scope all variables associated to that scope are dropped in reverse order
//! of declaration": a cancel drops the future tmp (the innermost binding of the await's
//! approximate desugaring), then the walk's snapshot, last declared first.

use std::alloc::Global;
use std::string::String;

use crate::ir::LocalId;
use crate::plan::{PlanError, Resume, plan};
use crate::record::{LocalFacts, Lowered, Origin, RecordError, Recorder};
use crate::{PointId, Tag};

const PLAIN: LocalFacts = LocalFacts { needs_drop: false };
const DROPS: LocalFacts = LocalFacts { needs_drop: true };

fn body_text(l: &Lowered<Global>) -> String {
    let mut s = String::new();
    let _ = l.body.print(&mut s);
    s
}

fn plan_text(l: &Lowered<Global>) -> Result<String, PlanError> {
    let p = plan(l, Global)?;
    let mut s = String::new();
    let _ = p.print(&mut s);
    Ok(s)
}

/// `async { let a = 1; { let c = 2; } let b = ready(2).await; a + b }`: `a` is in scope at the
/// await and is saved; `c`'s scope ended before it and `b` is declared after it, so neither is;
/// the future tmp is saved (it is polled in place). Prints `test
/// record_tests::one_await_in_a_line ... ok`.
#[test]
fn one_await_in_a_line() {
    let mut r = Recorder::new_in(Global);
    r.scope_open();
    let _a = r.declare(Origin::Binding(0), PLAIN);
    r.scope_open();
    let _c = r.declare(Origin::Binding(2), PLAIN);
    r.scope_close();
    let w = r.await_point(7, PLAIN, &[]);
    let _b = r.declare(Origin::Binding(1), PLAIN);
    r.scope_close();
    let l = r.finish().expect("a well-formed recording");
    assert_eq!(w.point, PointId(0));
    assert_eq!(
        body_text(&l),
        "locals 8 upvars 0 points 1 drop:\n\
         bb0:\n\
         \x20 _4 = const 0\n\
         \x20 _0 = &_4\n\
         \x20 _5 = const 0\n\
         \x20 _0 = &_5\n\
         \x20 dead _5\n\
         \x20 _6 = call f0()\n\
         \x20 -> goto bb1\n\
         bb1:\n\
         \x20 _3 = &_6\n\
         \x20 _2 = poll _6\n\
         \x20 -> switch _2 [bb2, bb4]\n\
         bb2:\n\
         \x20 -> suspend s0 resume bb1 drop bb3\n\
         bb3:\n\
         \x20 -> cancelled\n\
         bb4:\n\
         \x20 dead _6\n\
         \x20 _7 = const 0\n\
         \x20 _0 = &_7\n\
         \x20 dead _7\n\
         \x20 dead _4\n\
         \x20 -> return const 0\n"
    );
    assert_eq!(
        plan_text(&l).expect("a plan"),
        "field 0: binding 0 (_4)\n\
         field 1: future 7 (_6)\n\
         field 2: tag\n\
         point s0: await 7 tag 3 future field 1 cancel:\n\
         unresumed:\n"
    );
    let p = plan(&l, Global).expect("a plan");
    assert_eq!(p.resume(Tag::UNRESUMED), Resume::Start);
    assert_eq!(p.resume(Tag::point(PointId(0))), Resume::Point(PointId(0)));
    assert_eq!(p.resume(Tag::RETURNED), Resume::Completed);
    assert_eq!(p.resume(Tag(4)), Resume::NoSuchTag);
}

/// `async move { let total = 0; loop { let t = make(); step(&t).await; } }` with one captured
/// `String`: an await inside a loop's body with a local that needs drop live across it. The
/// snapshot at the await is the walk's drop scopes: the capture, then `t`. A cancel drops the
/// future tmp, `t`, the capture: each but the future behind its flag. A coroutine never polled
/// drops the capture alone. Prints `test record_tests::an_await_in_a_loop_with_a_local_across
/// ... ok`.
#[test]
fn an_await_in_a_loop_with_a_local_across() {
    let mut r = Recorder::new_in(Global);
    let cap = r.upvar(Origin::Upvar(0), DROPS);
    r.scope_open();
    r.scope_open();
    let _total = r.declare(Origin::Binding(1), PLAIN);
    r.scope_open();
    let t = r.declare(Origin::Binding(2), DROPS);
    r.await_point(9, DROPS, &[cap, t]);
    r.dropped(t);
    r.scope_close();
    r.scope_close();
    r.dropped(cap);
    r.scope_close();
    let l = r.finish().expect("a well-formed recording");
    assert_eq!(
        body_text(&l),
        "locals 8 upvars 1 points 1 drop: _0 _6 _7\n\
         bb0:\n\
         \x20 _5 = const 0\n\
         \x20 _1 = &_5\n\
         \x20 _6 = const 0\n\
         \x20 _1 = &_6\n\
         \x20 _7 = call f0()\n\
         \x20 -> goto bb1\n\
         bb1:\n\
         \x20 _4 = &_7\n\
         \x20 _3 = poll _7\n\
         \x20 -> switch _3 [bb2, bb4]\n\
         bb2:\n\
         \x20 -> suspend s0 resume bb1 drop bb3\n\
         bb3:\n\
         \x20 drop _7\n\
         \x20 drop _6\n\
         \x20 drop _0\n\
         \x20 -> cancelled\n\
         bb4:\n\
         \x20 drop _7\n\
         \x20 dead _7\n\
         \x20 drop _6\n\
         \x20 dead _6\n\
         \x20 dead _5\n\
         \x20 drop _0\n\
         \x20 -> return const 0\n"
    );
    assert_eq!(
        plan_text(&l).expect("a plan"),
        "field 0: upvar 0 (_0)\n\
         field 1: binding 1 (_5)\n\
         field 2: binding 2 (_6)\n\
         field 3: future 9 (_7)\n\
         field 4: flag of _0\n\
         field 5: flag of _6\n\
         field 6: tag\n\
         point s0: await 9 tag 3 future field 3 cancel: field 3, field 2 if field 5, field 0 if field 4\n\
         unresumed: field 0\n"
    );
}

/// `async { let s = make(); let r = &s; wait().await; read(r) }`: a borrow held across an
/// await. Both `s` and the reference to it are saved, so the reference points into the state
/// (why the state is pinned). A cancel drops `s` behind its flag. Prints `test
/// record_tests::a_borrow_held_across_an_await ... ok`.
#[test]
fn a_borrow_held_across_an_await() {
    let mut r = Recorder::new_in(Global);
    r.scope_open();
    let s = r.declare(Origin::Binding(0), DROPS);
    let rf = r.declare(Origin::Binding(1), PLAIN);
    r.borrowed(s, rf);
    r.await_point(4, PLAIN, &[s]);
    r.used(rf);
    r.dropped(s);
    r.scope_close();
    let l = r.finish().expect("a well-formed recording");
    assert_eq!(
        body_text(&l),
        "locals 7 upvars 0 points 1 drop: _4\n\
         bb0:\n\
         \x20 _4 = const 0\n\
         \x20 _0 = &_4\n\
         \x20 _5 = const 0\n\
         \x20 _0 = &_5\n\
         \x20 _5 = &_4\n\
         \x20 _6 = call f0()\n\
         \x20 -> goto bb1\n\
         bb1:\n\
         \x20 _3 = &_6\n\
         \x20 _2 = poll _6\n\
         \x20 -> switch _2 [bb2, bb4]\n\
         bb2:\n\
         \x20 -> suspend s0 resume bb1 drop bb3\n\
         bb3:\n\
         \x20 drop _4\n\
         \x20 -> cancelled\n\
         bb4:\n\
         \x20 dead _6\n\
         \x20 _1 = copy _5\n\
         \x20 drop _4\n\
         \x20 dead _5\n\
         \x20 dead _4\n\
         \x20 -> return const 0\n"
    );
    assert_eq!(
        plan_text(&l).expect("a plan"),
        "field 0: binding 0 (_4)\n\
         field 1: binding 1 (_5)\n\
         field 2: future 4 (_6)\n\
         field 3: flag of _4\n\
         field 4: tag\n\
         point s0: await 4 tag 3 future field 2 cancel: field 0 if field 3\n\
         unresumed:\n"
    );
}

/// The control of the seam's check: a snapshot naming a local whose scope has ended (a walk
/// that holds a binding in a drop scope the recorder was never told of, or told had closed) is
/// refused by name, since the state would not save what the cancel path drops. Prints `test
/// record_tests::a_snapshot_out_of_scope_is_refused ... ok`.
#[test]
fn a_snapshot_out_of_scope_is_refused() {
    let mut r = Recorder::new_in(Global);
    r.scope_open();
    r.scope_open();
    let gone = r.declare(Origin::Binding(0), DROPS);
    r.dropped(gone);
    r.scope_close();
    r.await_point(3, PLAIN, &[gone]);
    r.scope_close();
    assert_eq!(r.finish().err(), Some(RecordError::SnapshotNotInScope(LocalId(4))));
}
