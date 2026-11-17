#[cfg(first)]
fn f() -> u32 {
    1
}

#[cfg(second)]
fn f() -> u32 {
    2
}

fn g() -> u32 {
    5
}

fn main() {
    println!("{} {}", f(), g());
}
