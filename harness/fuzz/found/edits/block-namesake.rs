// Found by edits.py: seed E1-7, edition 2021, backend cranelift. Reduced by hand.
// The edit (regen.run): a second block with a function of the name the first block's has.
// Again: edits.py check '{"files": ["block-namesake.rs"]}'
// refused: `pick` is not compiled by blaze and rustc's copy is from before the edit, build
// with cargo: backend: Duplicate definition of identifier: _RNvNvNtCs6VGO1tlCAh9_2fz2c03run4pick
mod c0 {
    use super::*;
    pub fn run() {
        { fn pick(x: u64) -> u64 { x + 1 } feed(pick(3)); }
    }
}
//// then
mod c0 {
    use super::*;
    pub fn run() {
        { fn pick(x: u64) -> u64 { x + 2 } feed(pick(3)); }
        { fn pick(x: u64) -> u64 { x * 7 } feed(pick(3)); }
    }
}
//// then
mod c0 {
    use super::*;
    pub fn run() {
        { fn pick(x: u64) -> u64 { x * 7 } feed(pick(4)); }
        { fn pick(x: u64) -> u64 { x + 2 } feed(pick(3)); }
        { struct Local; impl Local { fn pick(&self) -> u64 { 5 } } feed(Local.pick()); }
        { struct Local; impl Local { fn pick(&self) -> u64 { 6 } } feed(Local.pick()); }
    }
}
