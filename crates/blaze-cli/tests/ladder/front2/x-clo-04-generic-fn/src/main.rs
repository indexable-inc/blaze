//! x-clo-04: closures passed to a generic `F: Fn(u32) -> u32`, each its own instance.
//!
//! The Reference, "Closure types": "A closure expression produces a closure value with a
//! unique, anonymous type"; a generic function is instantiated per closure type, and the call
//! `f(x)` runs that closure's body with its own captures.
//! 1. apply(|x| x * 2, 21) = 42.
//! 2. with a capture k = 7: apply(|x| x + k, 1) = 8.
//! 3. twice(f, x) = f(f(x)): twice(|x| x * 3, 2) = 18; twice(|x| x + k, 0) = 14.
//! 4. a closure passed by reference (`&F` is also `Fn`): apply(&double, 5) = 10.
//! 5. a named function item passed the same way: apply(inc, 9) = 10.
//! A wrong builder that shares one instance between closures exits 2 (42 for x + k).

fn apply<F: Fn(u32) -> u32>(f: F, x: u32) -> u32 {
    f(x)
}

fn twice<F: Fn(u32) -> u32>(f: F, x: u32) -> u32 {
    f(f(x))
}

fn inc(x: u32) -> u32 {
    x + 1
}

fn main() -> std::entry::Status {
    let k = 7u32;
    let double = |x: u32| x * 2;
    let status = if apply(|x| x * 2, 21) != 42 {
        1
    } else if apply(|x| x + k, 1) != 8 {
        2
    } else if twice(|x| x * 3, 2) != 18 || twice(|x| x + k, 0) != 14 {
        3
    } else if apply(&double, 5) != 10 {
        4
    } else if apply(inc, 9) != 10 {
        5
    } else {
        0
    };
    std::entry::Status(status)
}
