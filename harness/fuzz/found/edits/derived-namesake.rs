// Found by edits.py: seed E2-15, edition 2021, backend cranelift. Reduced by hand.
// The edit (regen.fn): a second block with a type of the name the first block's has, each
// with a derived `impl`.
// Again: edits.py check '{"files": ["derived-namesake.rs"]}'
// refused: `default` is not compiled by blaze and rustc's copy is from before the edit, build
// with cargo: backend: Duplicate definition of identifier
mod c0 {
    use super::*;
    pub fn run() {
        { #[derive(Default)] struct Plain { a: u64 } feed(Plain::default().a + 1); }
    }
}
//// then
mod c0 {
    use super::*;
    pub fn run() {
        { #[derive(Default)] struct Plain { a: u64 } feed(Plain::default().a + 2); }
        { #[derive(Default)] struct Plain { a: u64, b: u64 } feed(Plain::default().b + 3); }
    }
}
//// then
mod c0 {
    use super::*;
    pub fn run() {
        { #[derive(Default)] struct Plain { a: u64, b: u64 } feed(Plain::default().b + 4); }
        { #[derive(Default)] struct Plain { a: u64 } feed(Plain::default().a + 2); }
        { struct Plain(u64); impl Default for Plain { fn default() -> Plain { Plain(7) } } feed(Plain::default().0); }
    }
}
