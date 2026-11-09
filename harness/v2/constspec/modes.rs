//! The program `harness/v2/constspec.py` times: hot loops that branch on an argument each call
//! passes as a constant, which `hir-ty-mir`'s `Specialize` gives a copy of the callee knowing
//! it (docs/v2/plan/design/constspec.md). One callee of each kind of parameter a copy may
//! know (an enum, a `bool`, an integer compared with 0), each called with several values so no
//! constant reaches the callee by interprocedural propagation alone, and one called once an
//! element from the caller's loop, where unswitching the callee's loop cannot help.
//!
//! `modes ROUNDS` prints each loop's checksum on stdout (`NAME HEX`) and its time on stderr
//! (`time NAME NANOSECONDS`). The checksums are what constspec.py computes of the same loops.

use std::hint::black_box;
use std::time::Instant;

#[derive(Clone, Copy)]
pub enum Mode {
    Sum,
    Mix,
    Checked,
}

const LEN: usize = 4096;
const GOLDEN: u64 = 0x9E37_79B9_7F4A_7C15;

/// `LEN` words of xorshift64 from a fixed seed.
fn data() -> Vec<u64> {
    let mut state = 0x2545_F491_4F6C_DD1Du64;
    let mut out = Vec::with_capacity(LEN);
    for _ in 0..LEN {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        out.push(state);
    }
    out
}

/// The mode-switched hot loop: every element goes through the `match` on `mode`.
#[inline(never)]
fn run(mode: Mode, xs: &[u64], rounds: u32) -> u64 {
    let mut acc = 0u64;
    for r in 0..rounds {
        for &x in xs {
            acc = match mode {
                Mode::Sum => acc.wrapping_add(x),
                Mode::Mix => (acc.rotate_left(5) ^ x).wrapping_mul(GOLDEN),
                Mode::Checked => match acc.checked_add((x >> 1) ^ r as u64) {
                    Some(sum) => sum,
                    None => acc >> 1,
                },
            };
        }
    }
    acc
}

/// One element's step, called from the caller's loop: the `match` is the callee's whole work.
#[inline(never)]
fn step(mode: Mode, acc: u64, x: u64) -> u64 {
    match mode {
        Mode::Sum => acc.wrapping_add(x),
        Mode::Mix => (acc.rotate_left(5) ^ x).wrapping_mul(GOLDEN),
        Mode::Checked => match acc.checked_add(x >> 1) {
            Some(sum) => sum,
            None => acc >> 1,
        },
    }
}

#[inline(never)]
fn filter(odd: bool, xs: &[u64], rounds: u32) -> u64 {
    let mut acc = 0u64;
    for r in 0..rounds {
        for &x in xs {
            if odd {
                if x & 1 == 1 {
                    acc = acc.wrapping_add(x >> 3);
                }
            } else {
                acc ^= x.rotate_right(r);
            }
        }
    }
    acc
}

#[inline(never)]
fn scale(k: u64, xs: &[u64], rounds: u32) -> u64 {
    let mut acc = 0u64;
    for _ in 0..rounds {
        for &x in xs {
            acc = if k == 0 { acc ^ x } else { acc.wrapping_add(x.wrapping_mul(k)) };
        }
    }
    acc
}

fn report(name: &str, value: u64, began: Instant) {
    let ns = began.elapsed().as_nanos();
    println!("{name} {value:016x}");
    eprintln!("time {name} {ns}");
}

fn main() {
    let rounds: u32 = match std::env::args().nth(1) {
        Some(it) => it.parse().expect("ROUNDS: a number of rounds"),
        None => 20_000,
    };
    let owned = data();
    let xs: &[u64] = black_box(&owned);
    let rounds = black_box(rounds);

    let began = Instant::now();
    report("run_sum", run(Mode::Sum, xs, rounds), began);
    let began = Instant::now();
    report("run_mix", run(Mode::Mix, xs, rounds), began);
    let began = Instant::now();
    report("run_checked", run(Mode::Checked, xs, rounds), began);

    let began = Instant::now();
    let mut acc = 0u64;
    for _ in 0..rounds {
        for &x in xs {
            acc = step(Mode::Mix, acc, x);
        }
    }
    report("step_mix", acc, began);
    let began = Instant::now();
    let mut acc = 0u64;
    for _ in 0..rounds {
        for &x in xs {
            acc = step(Mode::Checked, acc, x);
        }
    }
    report("step_checked", acc, began);

    let began = Instant::now();
    report("filter_odd", filter(true, xs, rounds), began);
    let began = Instant::now();
    report("filter_all", filter(false, xs, rounds), began);
    let began = Instant::now();
    report("scale_0", scale(0, xs, rounds), began);
    let began = Instant::now();
    report("scale_3", scale(3, xs, rounds), began);
}
