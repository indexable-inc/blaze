//@ expect: accept
fn main() {
    let f = 1.5;
    let g: f32 = f;
    let h = 2.0;
    let k = h * 2.0;
    assert!(g == 1.5 && k == 4.0);
}
