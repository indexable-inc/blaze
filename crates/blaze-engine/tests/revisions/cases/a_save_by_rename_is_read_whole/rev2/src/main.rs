fn f(x: u32) -> u32 {
    x + 10
}

fn g(x: u32) -> u32 {
    x * 2
}

fn main() {
    println!("{} {}", f(1), g(2));
}
