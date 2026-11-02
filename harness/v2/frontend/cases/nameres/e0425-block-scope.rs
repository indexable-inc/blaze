//@ expect: reject E0425
//@ edition: 2021
fn main() {
    {
        let inner = 1;
        let _ = inner;
    }
    let _ = inner;
}
