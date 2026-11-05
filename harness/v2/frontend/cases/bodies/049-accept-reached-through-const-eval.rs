//@ expect: accept
const fn cube(x: usize) -> usize {
    x * x * x
}

const LEN: usize = cube(2);

struct Buffer([u8; cube(1) + 1]);

fn main() {
    let a = [0u8; LEN];
    let b = Buffer([1; 2]);
    println!("{} {}", a.len(), b.0.len());
}
