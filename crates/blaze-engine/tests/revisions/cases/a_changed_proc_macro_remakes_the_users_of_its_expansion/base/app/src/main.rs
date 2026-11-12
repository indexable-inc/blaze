fn uses() -> u32 {
    mac::answer!()
}

fn other(x: u32) -> u32 {
    x * 2
}

fn main() {
    println!("{} {}", uses(), other(2));
}
