//@ expect: accept
//@ crate: bin
//@ edition: 2021
// repeat counts computed from constants: `[0; LEN + 1]` in a constant and in a fn body, including a count of zero
const LEN: usize = 4;
const ZEROS: [u8; LEN + 1] = [0; LEN + 1];

fn main() {
    let a = [0u64; LEN + 1];
    let b = [1i8; LEN * LEN - 1];
    let c = [true; LEN / 3];
    let d: [u8; 0] = [9; LEN - LEN];
    println!("{} {} {} {} {}", ZEROS.len(), a.len(), b.len(), c.len(), d.len());
}
