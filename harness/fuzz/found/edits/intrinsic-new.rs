// Found by edits.py: seeds E3-0 and E3-5, edition 2021, both backends. Reduced by hand.
// The edit (regen.fn): the first use of `powi` in the program, so rustc's first build has no
// copy of it for the daemon to call.
// Again: edits.py check '{"files": ["intrinsic-new.rs"]}'
// refused: `run` is not compiled by blaze and rustc's copy is from before the edit, build
// with cargo: unsupported: callee `powi`: callee `powi`: unsupported: intrinsic `powif32`
mod c0 {
    use super::*;
    pub fn run() {
        let a = black_box(1.5f32);
        feed(a.to_bits() as u64);
    }
}
//// then
mod c0 {
    use super::*;
    pub fn run() {
        let a = black_box(1.5f32);
        feed(a.powi(3).to_bits() as u64);
        feed(a.powi(black_box(-2)).to_bits() as u64);
    }
}
//// then
mod c0 {
    use super::*;
    pub fn run() {
        let a = black_box(-2.5f64);
        feed(a.powi(7).to_bits());
        feed(a.powi(black_box(i32::MIN)).to_bits());
        feed(a.powi(black_box(0)).to_bits());
    }
}
