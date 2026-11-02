//@ expect: reject E0381
fn main() {
    let x: i32;
    let y = x + 1; //~ ERROR E0381
    println!("{y}");
}
