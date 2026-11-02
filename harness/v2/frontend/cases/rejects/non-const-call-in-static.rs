//@ expect: reject E0015
//@ edition: 2021
//@ issue: #1440
// The kind of #1440's first case in a `static`: rustc rejects a call of a function that is not
// `const` in a static's initializer (E0015).
#![crate_type = "lib"]

fn seven() -> u32 {
    7
}

pub static SEVEN: u32 = seven();
