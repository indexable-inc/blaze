fn other(x: u32) -> u32 {
    x * 2
}

fn main() {
    println!("{} {}", items::count(), other(2));
}
