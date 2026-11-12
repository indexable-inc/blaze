const K: u32 = 5;

fn a() -> u32 {
    K + 1
}

fn b() -> u32 {
    K * 2
}

fn unrelated(x: u32) -> u32 {
    x * 2
}

fn main() {
    println!("{} {} {}", a(), b(), unrelated(2));
}
