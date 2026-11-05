//@ expect: reject E0015
//@ edition: 2021
//@ issue: #1440
// The kind of #1440's first case through a trait: rustc rejects a call of a trait's associated
// function, which is not `const`, in a `const` (E0015).
#![crate_type = "lib"]

pub trait Width {
    fn width() -> usize;
}

impl Width for u8 {
    fn width() -> usize {
        1
    }
}

pub const W: usize = <u8 as Width>::width();
