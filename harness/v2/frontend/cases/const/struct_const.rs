//@ expect: accept
//@ crate: bin
//@ edition: 2021
// constants of a struct type: a literal, field reads into an integer constant, struct update syntax, an array of structs
#[derive(Debug, Clone, Copy)]
struct P {
    x: i32,
    y: i32,
}

const O: P = P { x: -1, y: 2 };
const SUM: i32 = O.x + O.y;
const ARR: [P; 2] = [O, P { x: 3, ..O }];
const LAST_X: i32 = ARR[1].x;

fn main() {
    println!("{:?} {} {:?} {}", O, SUM, ARR, LAST_X);
}
