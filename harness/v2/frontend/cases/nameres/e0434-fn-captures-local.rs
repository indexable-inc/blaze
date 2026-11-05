//@ expect: reject E0434
//@ edition: 2021
fn main() {
    let a = 1;
    fn inner() -> i32 {
        a
    }
    let _ = inner();
}
