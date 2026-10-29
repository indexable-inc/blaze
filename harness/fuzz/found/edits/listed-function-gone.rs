// Found by edits.py: seed E1-6, edition 2021, backend cranelift. Reduced by hand.
// The edit (regen.run): a function of a block taken away.
// Again: edits.py check '{"files": ["listed-function-gone.rs"]}'
// panicked: Can't find ast id ErasedFileAstId { kind: "Fn", index: 0, hash: 7A5F } in AstIdMap
// (at crates/span/src/ast_id.rs:852:21). The daemon caught it and went on, and in the program
// it was found in died some edits later: cannot delete write-locked id `Id(1e89f)`; value
// leaked across threads (at salsa-0.28.5/src/tracked_struct.rs:803:17).
mod c0 {
    use super::*;
    pub fn run() {
        { fn gone(x: u64) -> u64 { x + 1 } feed(gone(3)); }
    }
}
//// then
mod c0 {
    use super::*;
    pub fn run() {
        feed(1);
    }
}
//// then
mod c0 {
    use super::*;
    pub fn run() {
        { fn back(x: u64) -> u64 { x + 5 } feed(back(3)); }
    }
}
//// then
mod c0 {
    use super::*;
    pub fn run() {
        feed(4);
    }
}
