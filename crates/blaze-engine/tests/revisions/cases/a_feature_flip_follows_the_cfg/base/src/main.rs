#[cfg(feature = "loud")]
fn word() -> &'static str {
    "LOUD"
}

#[cfg(not(feature = "loud"))]
fn word() -> &'static str {
    "quiet"
}

fn unrelated(x: u32) -> u32 {
    x * 2
}

fn main() {
    println!("{} {}", word(), unrelated(2));
}
