//@ expect: reject E0080
//@ crate: lib
//@ edition: 2021
// a constant whose overflowing `255 + 1` a `macro_rules!` writes: E0080 at the macro's expansion
#![crate_type = "lib"]
macro_rules! inc {
    ($e:expr) => {
        $e + 1
    };
}

pub const X: u8 = inc!(255);
