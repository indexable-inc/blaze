//@ expect: accept
//@ edition: 2021
#[derive(Debug)]
struct Vec;

#[derive(Debug)]
enum Option {
    Some,
}

fn main() {
    let v = Vec;
    let o = Option::Some;
    println!("{:?} {:?}", v, o);
}
