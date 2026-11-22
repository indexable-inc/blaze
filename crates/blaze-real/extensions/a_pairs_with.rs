//! pairs_with: `#[blaze::pairs_with]` names a call's closing pair; an early return skips it. A working layer flags it (`blaze::skipped_pair`).
//! expect: refuse
#![deny(blaze::skipped_pair)]

fn end(depth: i32) -> i32 {
    depth - 1
}

#[blaze::pairs_with(end)]
fn begin(depth: i32) -> i32 {
    depth + 1
}

fn walk(stop: bool) -> i32 {
    let d = begin(0);
    if stop {
        return d;
    }
    end(d)
}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    walk(true) + 41
}
