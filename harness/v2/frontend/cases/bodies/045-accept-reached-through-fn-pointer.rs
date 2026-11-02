//@ expect: accept
fn double(x: u32) -> u32 {
    x * 2
}

fn square(x: u32) -> u32 {
    x * x
}

fn pick(square_it: bool) -> fn(u32) -> u32 {
    if square_it { square } else { double }
}

fn main() {
    let f = pick(std::env::args().count() > 5);
    println!("{}", f(7));
}
