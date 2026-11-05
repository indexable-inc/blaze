//@ expect: accept
fn main() {
    let f = |x| x + 1;
    let y: u16 = f(2);
    assert!(y == 3);
    let g = |a, b| a * b;
    assert!(g(3u64, 4) == 12);
}
