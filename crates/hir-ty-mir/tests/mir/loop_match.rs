//! `#[loop_match]` and `#[const_continue]`: a state machine prints the same with and without
//! the attributes, a transition's jump goes past the loop head's arm tests, an arm's locals drop
//! before the jump as a plain `break` drops them, a jump to an arm with a binding binds the new
//! value, a state with `Drop` is dropped as the plain loop drops it, and each shape rustc refuses is
//! refused with its message, unless an earlier error of rustc's stops it first. Each program runs on Linux
//! x86-64 under [`Plain`] with minicore.

use hir_expand::fixture;
use hir_ty::{
    db::HirDatabase,
    mir::{BasicBlockId, MirBody, Rvalue, StatementKind, TerminatorKind},
    next_solver::TyKind,
};
use hir_ty_mir::mir::{Plain, loop_match::loop_match_check, run_function};
use rustc_type_ir::inherent::IntoKind;

use crate::function;

/// The attributes, written where `/*lm*/` and `/*cc*/` stand in a program; [`plain`] removes
/// them.
fn attributed(program: &str) -> String {
    program.replace("/*lm*/", "#[loop_match]").replace("/*cc*/", "#[const_continue]")
}

fn plain(program: &str) -> String {
    program.replace("/*lm*/", "").replace("/*cc*/", "")
}

macro_rules! program {
    ($body:literal) => {
        concat!(
            "//- minicore: sized, drop, copy\n",
            "//- /main.rs cfg:target_os=linux,target_arch=x86_64\n",
            r#"
#![feature(loop_match)]

extern "C" {
    fn write(fd: i32, buf: *const u8, count: usize) -> isize;
}

fn put(byte: u8) {
    unsafe { write(1, &byte as *const u8, 1) };
}

struct Noisy(u8);

impl Drop for Noisy {
    fn drop(&mut self) {
        put(self.0);
    }
}
"#,
            $body
        )
    };
}

#[track_caller]
fn prints(fixture: &str, expected: &str) {
    let fx = fixture::db(fixture);
    let _ = fx.only_file();
    let db = fx.into_db();
    let (result, stdout) = blaze_query::db::attach(&db, || {
        let main = function(&db, "main");
        let (result, output, Plain) = run_function(&db, main, Plain);
        (result.map_err(|e| format!("{e:?}")), output.stdout().into_owned())
    });
    assert!(result.is_ok(), "{result:?}, printed {stdout:?}");
    assert_eq!(stdout, expected);
}

/// Prints `expected` with the attributes and without them.
#[track_caller]
fn prints_both_ways(program: &str, expected: &str) {
    prints(&plain(program), expected);
    prints(&attributed(program), expected);
}

const WALK: &str = program!(
    r#"
enum State {
    Start,
    Middle,
    End,
}

const MIDDLE: State = State::Middle;

fn walk() {
    let mut state = State::Start;
    /*lm*/
    'run: loop {
        state = 'next: {
            match state {
                State::Start => {
                    put(b's');
                    /*cc*/
                    break 'next MIDDLE;
                }
                State::Middle => {
                    put(b'm');
                    /*cc*/
                    break 'next State::End;
                }
                State::End => {
                    put(b'e');
                    break 'run;
                }
            }
        }
    }
}

fn main() {
    walk();
}
"#
);

#[test]
fn a_state_machine_prints_the_same_with_and_without_the_attributes() {
    prints_both_ways(WALK, "sme");
}

/// A state with a `Drop` impl, which rustc's `is_supported_loop_match_type` accepts: it asks
/// only that every variant has no fields.
const DROPPING: &str = program!(
    r#"
enum State {
    A,
    B,
    C,
}

impl Drop for State {
    fn drop(&mut self) {
        put(match *self {
            State::A => b'a',
            State::B => b'b',
            State::C => b'c',
        });
    }
}

fn walk() {
    let mut state = State::A;
    /*lm*/
    'run: loop {
        state = 'next: {
            match state {
                State::A => {
                    put(b'1');
                    /*cc*/
                    break 'next State::B;
                }
                State::B => {
                    put(b'2');
                    State::C
                }
                State::C => {
                    put(b'3');
                    break 'run;
                }
            }
        }
    }
    put(b'.');
}

fn main() {
    walk();
}
"#
);

/// [`DROPPING`] with a `Noisy` local in the arm that jumps.
const DROPPING_WITH_A_LOCAL: &str = program!(
    r#"
enum State {
    A,
    B,
}

impl Drop for State {
    fn drop(&mut self) {
        put(match *self {
            State::A => b'a',
            State::B => b'b',
        });
    }
}

fn held() {
    let mut state = State::A;
    /*lm*/
    'run: loop {
        state = 'next: {
            match state {
                State::A => {
                    let _n = Noisy(b'n');
                    put(b'1');
                    /*cc*/
                    break 'next State::B;
                }
                State::B => {
                    put(b'2');
                    break 'run;
                }
            }
        }
    }
    put(b'.');
}

fn main() {
    held();
}
"#
);

/// Each `state = ..` drops the state it overwrites, a jump's too, so the attributes change no
/// output. nightly-2026-10-27 prints `123.c` for [`DROPPING`] with them, never dropping the old
/// state; blaze keeps the plain loop's (docs/v2/plan/design/loop-match.md).
#[test]
fn a_state_with_a_destructor_is_dropped_at_each_transition_as_the_plain_loop_drops_it() {
    prints_both_ways(DROPPING, "1a2b3.c");
}

/// The jumping arm's local drops before the old state, as a `break 'next` leaves the arm
/// before the loop's `state = ..` runs.
#[test]
fn an_arms_local_drops_before_the_old_state() {
    prints_both_ways(DROPPING_WITH_A_LOCAL, "1na2.b");
}

/// From `from`, the first `SwitchInt`, through the blocks that only go on to one other.
fn first_switch(body: &MirBody<'_>, from: BasicBlockId) -> (BasicBlockId, Vec<BasicBlockId>) {
    let mut at = from;
    let mut path = vec![at];
    loop {
        let kind = &body.basic_blocks[at].terminator.as_ref().expect("a terminated block").kind;
        at = match kind {
            TerminatorKind::SwitchInt { .. } => return (at, path),
            TerminatorKind::Goto { target } => *target,
            TerminatorKind::Drop { target, .. } => *target,
            TerminatorKind::FalseEdge { real_target, .. } => *real_target,
            TerminatorKind::FalseUnwind { real_target, .. } => *real_target,
            other => panic!("{at:?} ends in {other:?} before any switch"),
        };
        assert!(!path.contains(&at), "a cycle with no switch through {at:?}");
        path.push(at);
    }
}

/// The blocks that write a new value of `walk`'s state, the one enum of the program, other
/// than by copying one: the transitions, and what starts the loop. With the switch each goes
/// on to first, and the loop head's.
fn transitions(fixture: &str) -> (BasicBlockId, Vec<(BasicBlockId, BasicBlockId)>) {
    let fx = fixture::db(fixture);
    let _ = fx.only_file();
    let db = fx.into_db();
    blaze_query::db::attach(&db, || {
        let body = db.mir_body(function(&db, "walk").into()).expect("the body of `walk`");
        let (head, start) = first_switch(&body, body.start_block);
        let writes_state = |kind: &StatementKind| match kind {
            StatementKind::Assign(place, rvalue) => {
                let place = place.as_ref();
                place.projection.as_slice().is_empty()
                    && matches!(body.locals[place.local].ty.as_ref().kind(), TyKind::Adt(..))
                    && !matches!(rvalue, Rvalue::Use(operand) if matches!(
                        operand.kind,
                        hir_ty::mir::OperandKind::Copy(_) | hir_ty::mir::OperandKind::Move(_)
                    ))
            }
            _ => false,
        };
        let found = (body.basic_blocks.iter())
            .filter(|(id, _)| !start.contains(id))
            .filter(|(_, block)| block.statements.iter().any(|it| writes_state(&it.kind)))
            .map(|(id, _)| (id, first_switch(&body, id).0))
            .collect();
        (head, found)
    })
}

/// The attributed `walk`'s two transitions each go to a switch that is not the head's: their
/// arm's own test. Without the attributes the same writes all go back to the head, so the
/// assertion tells the two apart.
#[test]
fn a_transition_jumps_past_the_loop_head() {
    let (head, jumps) = transitions(&attributed(WALK));
    assert_eq!(jumps.len(), 2, "the two `#[const_continue]`s: {jumps:?}");
    for (block, switch) in jumps {
        assert_ne!(switch, head, "{block:?} goes back to the loop head {head:?}");
    }
    let (head, breaks) = transitions(&plain(WALK));
    assert!(!breaks.is_empty(), "the plain loop's two breaks write a value");
    for (block, switch) in breaks {
        assert_eq!(switch, head, "{block:?}, a plain break, reaches the loop head");
    }
}

#[test]
fn an_arm_drops_its_locals_before_the_jump_in_plain_break_order() {
    prints_both_ways(
        program!(
            r#"
fn main() {
    let mut state: u8 = 0;
    /*lm*/
    'run: loop {
        state = 'next: {
            match state {
                0 => {
                    let _a = Noisy(b'a');
                    let _b = Noisy(b'b');
                    put(b'0');
                    /*cc*/
                    break 'next 1;
                }
                _ => {
                    put(b'1');
                    break 'run;
                }
            }
        }
    }
}
"#
        ),
        "0ba1",
    );
}

#[test]
fn a_jump_to_an_arm_with_a_binding_binds_the_new_value() {
    prints_both_ways(
        program!(
            r#"
const THREE: u8 = 3;

fn main() {
    let mut state: u8 = 0;
    /*lm*/
    'run: loop {
        state = 'next: {
            match state {
                0 => {
                    put(b's');
                    /*cc*/
                    break 'next 2;
                }
                x @ 1..=3 => {
                    put(b'0' + x);
                    if x == THREE {
                        /*cc*/
                        break 'next 7;
                    }
                    /*cc*/
                    break 'next THREE;
                }
                x => {
                    put(b'0' + x);
                    break 'run;
                }
            }
        }
    }
}
"#
        ),
        "s237",
    );
}

/// The messages of what `loop_match_check` refuses in the function `f` of `program`.
#[track_caller]
fn refusals(program: &str) -> Vec<&'static str> {
    let fixture = format!("//- minicore: sized, copy\n#![feature(loop_match)]\n{program}");
    let fx = fixture::db(&fixture);
    let _ = fx.only_file();
    let db = fx.into_db();
    blaze_query::db::attach(&db, || {
        let def = hir_def::DefWithBodyId::FunctionId(function(&db, "f"));
        loop_match_check(&db, def).errors().iter().map(|it| it.kind.message()).collect()
    })
}

#[track_caller]
fn refused(program: &str, message: &str) {
    assert_eq!(refusals(program), [message]);
}

#[test]
fn a_loop_whose_body_is_no_assignment_is_refused() {
    refused(
        "fn f() { #[loop_match] loop { break } }",
        "expected a single assignment expression",
    );
}

#[test]
fn statements_before_the_assignment_are_refused() {
    refused(
        r#"
fn f() {
    let mut state: u8 = 0;
    #[loop_match]
    'run: loop {
        let other = 1u8;
        state = 'next: { match state { _ => break 'run } }
    }
}
"#,
        "statements are not allowed in this position within a `#[loop_match]`",
    );
}

#[test]
fn an_assignment_of_no_labeled_block_is_refused() {
    refused(
        r#"
fn f() {
    let mut state: u8 = 0;
    #[loop_match]
    'run: loop {
        state = match state { _ => break 'run };
    }
}
"#,
        "this expression must be a single `match` wrapped in a labeled block",
    );
}

#[test]
fn a_match_on_no_local_is_refused() {
    refused(
        r#"
fn zero() -> u8 { 0 }
fn f() {
    let mut state: u8 = 0;
    #[loop_match]
    'run: loop {
        state = 'next: { match zero() { _ => break 'run } }
    }
}
"#,
        "invalid match on `#[loop_match]` state",
    );
}

#[test]
fn an_update_of_another_local_is_refused() {
    refused(
        r#"
fn f() {
    let mut state: u8 = 0;
    let mut other: u8 = 0;
    #[loop_match]
    'run: loop {
        other = 'next: { match state { _ => break 'run } }
    }
}
"#,
        "invalid update of the `#[loop_match]` state",
    );
}

#[test]
fn a_state_of_a_type_with_fields_is_refused() {
    refused(
        r#"
fn f() {
    let mut state: (u8, u8) = (0, 0);
    #[loop_match]
    'run: loop {
        state = 'next: { match state { _ => break 'run } }
    }
}
"#,
        "this `#[loop_match]` state value has a type that is not supported",
    );
}

#[test]
fn a_guarded_arm_is_refused() {
    refused(
        r#"
fn f() {
    let mut state: u8 = 0;
    #[loop_match]
    'run: loop {
        state = 'next: {
            match state {
                x if x > 1 => break 'run,
                _ => break 'run,
            }
        }
    }
}
"#,
        "match arms that are part of a `#[loop_match]` cannot have guards",
    );
}

/// rustc makes this check in THIR building, which it skips for a body whose type check failed.
/// `break 'next;` leaves a block of type `u8` with `()`, so the nightly reports E0308 alone and
/// no `#[loop_match]` error; inference reports the mismatch here.
#[test]
fn a_const_continue_with_no_value_to_a_typed_block_gets_only_the_type_error() {
    assert_eq!(
        refusals(
            r#"
fn f() {
    let mut state: u8 = 0;
    #[loop_match]
    'run: loop {
        state = 'next: {
            match state {
                0 => {
                    #[const_continue]
                    break 'next;
                }
                _ => break 'run,
            }
        }
    }
}
"#
        ),
        Vec::<&str>::new(),
    );
}

/// With a `()` state the break with no value type-checks, so THIR building runs and refuses
/// it. Its refusal is fatal, so MIR building never refuses the `()` state's type.
#[test]
fn a_const_continue_with_no_value_is_refused() {
    refused(
        r#"
fn f() {
    let mut state = ();
    #[loop_match]
    loop {
        state = 'next: {
            match state {
                () => {
                    #[const_continue]
                    break 'next;
                }
            }
        }
    }
}
"#,
        "a `#[const_continue]` must break to a label with a value",
    );
}

/// The label check is type checking's, made before inference, so a type error elsewhere in
/// the body does not hide it. The break's innermost breakable is a loop, so it gets no E0695.
#[test]
fn a_const_continue_with_no_label_is_refused_in_a_body_with_a_type_error() {
    refused(
        r#"
fn f() {
    let wrong: u8 = true;
    let mut state: u8 = 0;
    #[loop_match]
    'run: loop {
        state = 'next: {
            match state {
                0 => {
                    loop {
                        #[const_continue]
                        break;
                    }
                    break 'run;
                }
                _ => break 'run,
            }
        }
    }
}
"#,
        "`#[const_continue]` must break to a labeled block that participates in a `#[loop_match]`",
    );
}

/// The break's innermost breakable is the block `'next`, so type checking gives E0695
/// ("unlabeled `break` inside of a labeled block", inference's here) and skips its label
/// check. E0695 does not taint the body, so THIR building refuses the break too:
/// nightly-2026-10-27 prints both errors.
#[test]
fn a_const_continue_with_no_label_in_a_labeled_block_gets_e0695_and_a_missing_label() {
    refused(
        r#"
fn f() {
    let mut state: u8 = 0;
    #[loop_match]
    'run: loop {
        state = 'next: {
            match state {
                0 => {
                    #[const_continue]
                    break;
                }
                _ => break 'run,
            }
        }
    }
}
"#,
        "a `#[const_continue]` must break to a label with a value",
    );
}

#[test]
fn a_const_continue_to_another_label_is_refused() {
    refused(
        r#"
fn f() {
    let mut state: u8 = 0;
    'outer: loop {
        #[loop_match]
        'run: loop {
            state = 'next: {
                match state {
                    0 => {
                        #[const_continue]
                        break 'outer;
                    }
                    _ => break 'run,
                }
            }
        }
    }
}
"#,
        "`#[const_continue]` must break to a labeled block that participates in a `#[loop_match]`",
    );
}

const NOT_EARLY: &str = "could not determine the target branch for this `#[const_continue]`";

#[test]
fn a_jump_to_a_const_parameter_is_refused() {
    refused(
        r#"
fn f<const N: u8>() {
    let mut state: u8 = 0;
    #[loop_match]
    'run: loop {
        state = 'next: {
            match state {
                0 => {
                    #[const_continue]
                    break 'next N;
                }
                _ => break 'run,
            }
        }
    }
}
"#,
        NOT_EARLY,
    );
}

#[test]
fn a_jump_to_a_const_block_is_refused() {
    refused(
        r#"
fn f() {
    let mut state: u8 = 0;
    #[loop_match]
    'run: loop {
        state = 'next: {
            match state {
                0 => {
                    #[const_continue]
                    break 'next const { 1 };
                }
                _ => break 'run,
            }
        }
    }
}
"#,
        NOT_EARLY,
    );
}

#[test]
fn a_jump_to_a_call_is_refused() {
    refused(
        r#"
fn one() -> u8 { 1 }
fn f() {
    let mut state: u8 = 0;
    #[loop_match]
    'run: loop {
        state = 'next: {
            match state {
                0 => {
                    #[const_continue]
                    break 'next one();
                }
                _ => break 'run,
            }
        }
    }
}
"#,
        NOT_EARLY,
    );
}

#[test]
fn a_jump_to_a_generic_constant_is_refused() {
    refused(
        r#"
trait Next { const NEXT: u8; }
fn f<T: Next>() {
    let mut state: u8 = 0;
    #[loop_match]
    'run: loop {
        state = 'next: {
            match state {
                0 => {
                    #[const_continue]
                    break 'next T::NEXT;
                }
                _ => break 'run,
            }
        }
    }
}
"#,
        NOT_EARLY,
    );
}

/// A value no arm accepts is possible only in a `match` that is not exhaustive, which match
/// checking refuses too (E0004); the check's own refusal is still given, as rustc's would be
/// were it to build the body.
#[test]
fn a_jump_no_arm_accepts_is_refused() {
    refused(
        r#"
fn f() {
    let mut state: u8 = 0;
    #[loop_match]
    'run: loop {
        state = 'next: {
            match state {
                0 => {
                    #[const_continue]
                    break 'next 5;
                }
                1 => break 'run,
            }
        }
    }
}
"#,
        "the target of this `#[const_continue]` is not statically known",
    );
}
