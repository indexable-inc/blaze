//! `loop_match_candidate`: one state machine found per shape, and one quiet case per thing the
//! lint leaves alone. Each quiet fixture also holds `control`, a machine over `next` that is
//! found, so a quiet case is quiet because of what it tests and not because nothing ran.

use crate::tests::lint;

const MINICORE: &str = "//- minicore: sized, copy, clone, option, add, ord, eq, builtin_impls, drop\n";

const MAIN: &str = "//- /src/lib.rs crate:main\n";

/// What the lint says, it alone on, of `main` (`src/lib.rs`).
fn only(main: &str) -> String {
    let fixture = format!("{MINICORE}{MAIN}{}", main.trim_start_matches('\n'));
    lint(&fixture, "-A clippy::all --warn=blaze::loop_match_candidate")
}

const FOUND: &str = "is a state machine over";

/// A machine over `next`, the first shape, that every quiet fixture holds beside its case.
const CONTROL: &str = r#"
pub fn control(mut fuel: u32) -> u32 {
    let mut next = 0u8;
    loop {
        next = match next {
            0 => 1,
            1 => { fuel += 1; if fuel > 9 { 2 } else { 0 } }
            _ => break,
        };
    }
    fuel
}
"#;

/// `out` holds one finding, the control's.
fn only_the_control(out: &str) {
    assert_eq!(out.matches(FOUND).count(), 1, "{out}");
    assert!(out.contains("over `next`"), "{out}");
}

/// `state = match state { .. };` over a fieldless enum is found, each `if` branch counted.
#[test]
fn finds_a_match_assigned_to_its_state() {
    let found = only(
        r#"
pub enum Stage { Start, Middle, End }
pub fn run(mut count: u32) -> u32 {
    let mut state = Stage::Start;
    loop {
        state = match state {
            Stage::Start => Stage::Middle,
            Stage::Middle => { count += 1; if count > 3 { Stage::End } else { Stage::Middle } }
            Stage::End => break,
        };
    }
    count
}
"#,
    );
    assert_eq!(found.matches(FOUND).count(), 1, "{found}");
    assert!(found.contains("over `state`: 3 of its 3 transitions"), "{found}");
    assert!(found.contains("#[const_continue]"), "{found}");
    assert!(found.contains("#![feature(loop_match)]"), "{found}");
    assert!(found.contains("incomplete"), "{found}");
}

/// `match state { A => { ..; state = B; } .. }` over a `u8` and module `const`s is found.
#[test]
fn finds_arms_that_assign_their_state() {
    let found = only(
        r#"
const READ: u8 = 0;
const WRITE: u8 = 1;
const DONE: u8 = 2;
pub fn pump(mut budget: u32) -> u32 {
    let mut state = READ;
    let mut moved = 0;
    loop {
        match state {
            READ => { budget += 1; state = WRITE; }
            WRITE => { moved += 1; state = if budget > 9 { DONE } else { READ }; }
            _ => return moved,
        }
    }
}
"#,
    );
    assert_eq!(found.matches(FOUND).count(), 1, "{found}");
    assert!(found.contains("over `state`: 3 of its 3 transitions"), "{found}");
}

/// A state in a field is no local, and rustc's `loop_match` takes only a local.
#[test]
fn leaves_a_field_state_alone() {
    let found = only(&format!(
        "{CONTROL}{}",
        r#"
pub struct Parser { stage: u8 }
impl Parser {
    pub fn run(&mut self) {
        loop {
            self.stage = match self.stage {
                0 => 1,
                1 => 2,
                _ => break,
            };
        }
    }
}
"#
    ));
    only_the_control(&found);
}

/// An enum with a variant that carries data is no type `loop_match` takes.
#[test]
fn leaves_a_state_with_data_alone() {
    let found = only(&format!(
        "{CONTROL}{}",
        r#"
pub enum Job { Idle, Run(u32) }
pub fn run() {
    let mut state = Job::Idle;
    loop {
        state = match state {
            Job::Idle => Job::Run(1),
            Job::Run(0) => break,
            Job::Run(n) => Job::Run(n + 1),
        };
    }
}
"#
    ));
    only_the_control(&found);
}

/// An arm with a guard is an error under `loop_match`.
#[test]
fn leaves_a_guarded_arm_alone() {
    let found = only(&format!(
        "{CONTROL}{}",
        r#"
pub fn run(go: bool) {
    let mut state = 0u8;
    loop {
        state = match state {
            0 if go => 1,
            0 => 2,
            _ => break,
        };
    }
}
"#
    ));
    only_the_control(&found);
}

/// A second statement in the loop body is not the one `state = ..` rustc takes.
#[test]
fn leaves_a_loop_with_more_statements_alone() {
    let found = only(&format!(
        "{CONTROL}{}",
        r#"
pub fn run(mut ticks: u32) -> u32 {
    let mut state = 0u8;
    loop {
        ticks += 1;
        state = match state {
            0 => 1,
            1 => 0,
            _ => break,
        };
    }
    ticks
}
"#
    ));
    only_the_control(&found);
}

/// With no transition to a constant, `#[const_continue]` has nothing to make a jump.
#[test]
fn leaves_a_machine_with_no_constant_transition_alone() {
    let found = only(&format!(
        "{CONTROL}{}",
        r#"
fn step(it: u8) -> u8 { it + 2 }
pub fn run() {
    let mut state = 200u8;
    loop {
        state = match state {
            0 => break,
            1 => step(state),
            other => other + 1,
        };
    }
}
"#
    ));
    only_the_control(&found);
}

/// A loop that has `#[loop_match]` already is left alone. The same body without the attributes
/// is found (`finds_the_labeled_block_form_without_the_attribute`), so it is the attribute that
/// keeps the lint quiet.
#[test]
fn leaves_a_loop_match_loop_alone() {
    let found = only(&format!(
        "#![feature(loop_match)]\n{CONTROL}{}",
        r#"
pub fn run() {
    let mut state = 0u8;
    #[loop_match]
    'machine: loop {
        state = 'next: {
            match state {
                0 => {
                    #[const_continue]
                    break 'next 1;
                }
                1 => {
                    #[const_continue]
                    break 'next 2;
                }
                _ => break 'machine,
            }
        };
    }
}
"#
    ));
    only_the_control(&found);
}

/// The labeled-block form with `#[loop_match]` and `#[const_continue]` left off is found, its
/// `break 'next VALUE`s counted as the transitions: what a person has after writing the form
/// and forgetting the attribute.
#[test]
fn finds_the_labeled_block_form_without_the_attribute() {
    let found = only(&format!(
        "{CONTROL}{}",
        r#"
pub fn run() {
    let mut state = 0u8;
    'machine: loop {
        state = 'next: {
            match state {
                0 => {
                    break 'next 1;
                }
                1 => {
                    break 'next 2;
                }
                _ => break 'machine,
            }
        };
    }
}
"#
    ));
    assert_eq!(found.matches(FOUND).count(), 2, "{found}");
    assert!(found.contains("over `next`"), "{found}");
    assert!(found.contains("over `state`: 2 of its 2 transitions"), "{found}");
}

/// A state with a destructor is left alone: rustc does not drop the old state inside a
/// `#[loop_match]` and blaze does, so the rewrite would change what the loop prints.
#[test]
fn leaves_a_state_that_needs_drop_alone() {
    let found = only(&format!(
        "{CONTROL}{}",
        r#"
pub enum Light { Red, Green }
impl Drop for Light {
    fn drop(&mut self) {}
}
pub fn run(mut n: u32) {
    let mut state = Light::Red;
    loop {
        state = match state {
            Light::Red => Light::Green,
            Light::Green => { n += 1; if n > 3 { break } else { Light::Red } }
        };
    }
}
"#
    ));
    only_the_control(&found);
}
