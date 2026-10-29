// Written by hand after push1-27-s2-cranelift.rs (#582), edition 2021: the closure `caught` is
// given is called from another closure, and the edit gives `caught` another one in its place
// from the body itself. A closure is named by its place among the closures of its body, so the
// patch calls the base's `caught::<{closure#0}>`, which nothing the patch compiled reached
// before: the new code of the closure behind it has to come with it, or the old one runs.
// Again: edits.py check '{"files": ["closure-called-again.rs"]}'
mod c0 {
    use super::*;
    pub fn run() {
        let first = || { caught(|| feed(1)); };
        first();
    }
}
//// then
mod c0 {
    use super::*;
    pub fn run() {
        caught(|| feed(3));
        let first = || feed(2);
        first();
    }
}
