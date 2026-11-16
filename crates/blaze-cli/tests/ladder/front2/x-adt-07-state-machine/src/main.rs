//! x-adt-07: an enum state machine stepped by value: each step reads the variant and its data.
//!
//! The Reference, "Match expressions" (the first matching arm is chosen) and "Enumerations"
//! (a value is one variant with that variant's fields). `step` is:
//!   Idle -> Running(1); Running(n) with n < 3 -> Running(n + 1); Running(3) -> Done { code: 30 };
//!   Done stays Done.
//! 1. from Idle, one step: Running(1): score 1.
//! 2. four steps: Idle -> R(1) -> R(2) -> R(3) -> Done { code: 30 }: score 100 + 30 = 130.
//! 3. a fifth step leaves Done { code: 30 }: 130.
//! 4. the states visited in order, folded as score digits: R(1), R(2), R(3) give 1, 2, 3 and
//!    Done gives 9: 1239.
//! A wrong builder that loses the payload across a move by value exits 2.

#[derive(Clone, Copy)]
enum State {
    Idle,
    Running(u32),
    Done { code: u8 },
}

fn step(s: State) -> State {
    match s {
        State::Idle => State::Running(1),
        State::Running(n) if n < 3 => State::Running(n + 1),
        State::Running(n) => State::Done { code: n as u8 * 10 },
        State::Done { code } => State::Done { code },
    }
}

fn score(s: State) -> u32 {
    match s {
        State::Idle => 0,
        State::Running(n) => n,
        State::Done { code } => 100 + code as u32,
    }
}

fn main() -> std::entry::Status {
    let mut s = State::Idle;
    let mut digits = 0;
    let mut i = 0;
    while i < 4 {
        s = step(s);
        digits = digits * 10 + if score(s) >= 100 { 9 } else { score(s) };
        i += 1;
    }
    let status = if score(step(State::Idle)) != 1 {
        1
    } else if score(s) != 130 {
        2
    } else if score(step(s)) != 130 {
        3
    } else if digits != 1239 {
        4
    } else {
        0
    };
    std::entry::Status(status)
}
