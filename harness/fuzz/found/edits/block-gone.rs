// Written by hand after listed-function-gone.rs, edition 2021: the block a function is in goes while a
// function of its name stays.
// Again: edits.py check '{"files": ["block-gone.rs"]}'
mod c0 {
    use super::*;
    pub fn run() {
        { fn pick(x: u64) -> u64 { x + 1 } feed(pick(3)); }
        { fn other(x: u64) -> u64 { x + 9 } feed(other(3)); }
    }
}
//// then
mod c0 {
    use super::*;
    pub fn run() {
        fn pick(x: u64) -> u64 { x + 2 }
        feed(pick(3));
        { fn other(x: u64) -> u64 { x + 9 } feed(other(4)); }
    }
}
//// then
mod c0 {
    use super::*;
    pub fn run() {
        fn pick(x: u64) -> u64 { x + 3 }
        feed(pick(3));
        feed({ fn other(x: u64) -> u64 { x + 8 } other(4) });
    }
}
//// then
mod c0 {
    use super::*;
    pub fn run() {
        fn pick(x: u64) -> u64 { x + 3 }
        feed(pick(5));
        feed({ fn other(x: u64) -> u64 { x + 7 } other(4) });
    }
}
