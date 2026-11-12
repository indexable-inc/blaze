fn f(x: u32) -> u32 {
    x + 1
}

fn caller(x: u32) -> u32 {
    f(x) * 3
}

fn unrelated(x: u32) -> u32 {
    x * 2
}

fn main() {
    println!("{} {}", caller(1), unrelated(2));
}
