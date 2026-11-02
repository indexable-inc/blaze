//@ expect: reject E0308
fn main() {
    let _never_called = |x: u32| -> u32 {
        x == 1 //~ ERROR E0308
    };
}
