//@ expect: reject E0381
fn main() {
    let ok = std::env::args().count() > 5;
    let x: i32;
    if ok {
        x = 1;
    }
    let y = x; //~ ERROR E0381
    println!("{y}");
}
