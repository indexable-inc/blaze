#[cfg(from_build)]
fn said() -> &'static str {
    "declared"
}

#[cfg(not(from_build))]
fn said() -> &'static str {
    "not applied"
}

#[cfg(from_build_value = "one")]
fn never() {}

fn main() {
    #[cfg(from_build_value = "one")]
    never();
    println!("{}", said());
}
