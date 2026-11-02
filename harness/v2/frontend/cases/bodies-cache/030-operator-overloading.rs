//@ expect: accept
//@ edition: 2021
//@ aux-crate: ops=ops.rs
use ops::V2;
fn main() {
    let v = V2 { x: 1, y: 2 } + V2 { x: 3, y: 4 };
    println!("{:?} {:?}", v * 2, -v);
    println!("{}", ops::Grid(vec![5, 6, 7])[1]);
}
