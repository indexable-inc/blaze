//@ expect: accept
#![feature(loop_match)]
#![expect(incomplete_features)]

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

impl Drop for Dropping {
    fn drop(&mut self) {
        // No `as` cast: rustc refuses one of an enum with `Drop`.
        let name = match *self {
            Dropping::A => "A",
            Dropping::B => "B",
            Dropping::C => "C",
        };
        println!("drop {name}");
    }
}

/// A state with `Drop` is accepted. Neither the jump nor the loop's own update drops the old
/// state; only the last state drops, when `dropping` returns.
fn dropping() {
    let mut state = Dropping::A;
    #[loop_match]
    'run: loop {
        state = 'next: {
            match state {
                Dropping::A => {
                    println!("a");
                    #[const_continue]
                    break 'next Dropping::B;
                }
                Dropping::B => {
                    println!("b");
                    Dropping::C
                }
                Dropping::C => break 'run,
            }
        }
    }
}

fn main() {
    println!("{}", walk());
    bind();
    dropping();
}
