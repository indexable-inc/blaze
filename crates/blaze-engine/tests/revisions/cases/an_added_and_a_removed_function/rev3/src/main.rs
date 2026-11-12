fn f(x: u32) -> u32 {
    x + 1
}

fn g(x: u32) -> u32 {
    x * 2
}

fn main() {
    println!("{} {}", f(1), g(2));
}
