//@ expect: reject E0308
fn main() {
    fn inner() -> u8 {
        "nested" //~ ERROR E0308
    }
}
