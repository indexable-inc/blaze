//@ expect: reject E0381
fn main() {
    let p: (i32, i32);
    p.0 = 1; //~ ERROR E0381
    println!("done");
}
