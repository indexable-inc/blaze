//@ expect: accept
//@ crate: bin
//@ edition: 2021
// statics computed from constants, an array static sized by a constant, and a constant reading a static
const A: u32 = 6;
static S: u32 = A * 7;
static ARR: [u8; A as usize] = [1; A as usize];
static NAME: &str = "blaze";
const READ: u32 = S + 1;

fn main() {
    println!("{} {} {} {}", S, ARR.len(), NAME, READ);
}
