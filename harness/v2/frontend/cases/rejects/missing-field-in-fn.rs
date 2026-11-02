//@ expect: reject E0063
//@ edition: 2021
//@ issue: #1522
// The kind of #1522's first case outside a test: rustc rejects a struct literal that leaves out
// a field (E0063) in an ordinary function.
#![crate_type = "lib"]

pub struct Config {
    pub jobs: u32,
    pub told: bool,
}

pub fn config() -> Config {
    Config { jobs: 1 }
}
