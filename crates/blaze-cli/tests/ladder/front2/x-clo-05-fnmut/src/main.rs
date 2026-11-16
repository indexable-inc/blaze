//! x-clo-05: a closure passed as `F: FnMut()` and called many times keeps its state across calls.
//!
//! The Reference, "Closure types", "Call traits and coercions": "`FnMut`: The closure can be
//! called multiple times as mutable reference"; each call runs with `&mut` to the same
//! captured state.
//! 1. call_n(|| count += 1, 5): count = 5.
//! 2. a closure whose own `move` state is a running sum: s = 0, k = 1..=4 fed one per call by
//!    `feed`: s = 1 + 2 + 3 + 4 = 10, returned by the last call.
//! 3. `FnMut` passed by `&mut F` (also FnMut): call_n(&mut add2, 3) on total = 0: 6; then a
//!    direct call: 8.
//! A wrong builder that passes a copy of the state to each call exits 1 (count 0 or 1).

fn call_n<F: FnMut()>(mut f: F, n: u32) {
    let mut i = 0;
    while i < n {
        f();
        i += 1;
    }
}

fn feed<F: FnMut(u32) -> u32>(mut f: F, upto: u32) -> u32 {
    let (mut k, mut last) = (1, 0);
    while k <= upto {
        last = f(k);
        k += 1;
    }
    last
}

fn main() -> std::entry::Status {
    let mut count = 0u32;
    call_n(|| count += 1, 5);
    let mut s = 0u32;
    let sum = feed(
        move |k| {
            s += k;
            s
        },
        4,
    );
    let mut total = 0u32;
    let mut add2 = || total += 2;
    call_n(&mut add2, 3);
    add2();
    let status = if count != 5 {
        1
    } else if sum != 10 {
        2
    } else if total != 8 {
        3
    } else {
        0
    };
    std::entry::Status(status)
}
