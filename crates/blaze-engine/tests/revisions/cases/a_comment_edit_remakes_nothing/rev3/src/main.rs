fn f(x: u32) -> u32 {
    x + 1 // one more, said otherwise
}

// A line that moves every line below it.
fn g(x: u32) -> u32 {
    x * 2
}

fn main() {
    println!("{} {}", f(1), g(2));
}
