//@ expect: reject E0015
//@ edition: 2021
//@ issue: #1440
// rustc rejects a call of a function that is not `const` in a `const` (E0015). blaze built it
// and evaluated the call in its interpreter.
#![crate_type = "lib"]

fn digest() -> [u8; 4] {
    let bytes = vec![1u8, 2, 3, 4];
    [bytes[0], bytes[1], bytes[2], bytes[3]]
}

pub const D: [u8; 4] = digest();
