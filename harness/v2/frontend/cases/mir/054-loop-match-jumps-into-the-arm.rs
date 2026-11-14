//@ expect: accept
#![feature(loop_match)]
#![expect(incomplete_features)]

use std::sync::atomic::{AtomicU32, Ordering};

struct D(&'static str);

impl Drop for D {
    fn drop(&mut self) {
        println!("drop {}", self.0);
    }
}

#[derive(Clone, Copy)]
enum State {
    Start,
    Middle,
    End,
}

const MIDDLE: State = State::Middle;
const THREE: u8 = 3;

/// Each transition is a jump into its arm; the arm's locals drop before it, last first.
fn walk() -> u32 {
    let mut state = State::Start;
    let mut steps = 0;
    #[loop_match]
    'run: loop {
        state = 'next: {
            match state {
                State::Start => {
                    let _a = D("a");
                    let _b = D("b");
                    steps += 1;
                    #[const_continue]
                    break 'next MIDDLE;
                }
                State::Middle => {
                    steps += 10;
                    #[const_continue]
                    break 'next State::End;
                }
                State::End => break 'run,
            }
        }
    }
    steps
}

/// A jump to an arm with a binding binds the new value.
fn bind() {
    let mut state: u8 = 0;
    #[loop_match]
    'run: loop {
        state = 'next: {
            match state {
                0 => {
                    println!("start");
                    #[const_continue]
                    break 'next 2;
                }
                x @ 1..=3 => {
                    println!("in range {x}");
                    if x == THREE {
                        #[const_continue]
                        break 'next 7;
                    }
                    #[const_continue]
                    break 'next THREE;
                }
                x => {
                    println!("past {x}");
                    break 'run;
                }
            }
        }
    }
}

enum Dropping {
    A,
    B,
    C,
}

/// How many `Dropping` states have been dropped.
static DROPS: AtomicU32 = AtomicU32::new(0);

impl Drop for Dropping {
    fn drop(&mut self) {
        DROPS.fetch_add(1, Ordering::Relaxed);
    }
}

/// A state with `Drop` is accepted, and each transition drops the state it overwrites, as the
/// plain loop does: A on the jump to B, B on the update to C. The assert holds the case to that.
/// nightly-2026-10-27 drops neither (it writes the state without a drop), so it fails this
/// case; blaze differs from it on purpose (docs/v2/plan/design/loop-match.md).
fn dropping() {
    let mut state = Dropping::A;
    #[loop_match]
    'run: loop {
        state = 'next: {
            match state {
                Dropping::A => {
                    #[const_continue]
                    break 'next Dropping::B;
                }
                Dropping::B => Dropping::C,
                Dropping::C => break 'run,
            }
        }
    }
    assert_eq!(DROPS.load(Ordering::Relaxed), 2, "A and B dropped at their transitions");
}

fn main() {
    println!("{}", walk());
    bind();
    dropping();
}
