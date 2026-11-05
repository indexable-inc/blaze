//@ expect: accept
#[cfg(any())]
fn configured_out() -> u32 {
    let s = String::new();
    let t = s;
    s.len() + "not a number"
}

fn main() {
    #[cfg(any())]
    let _: u8 = "also configured out";
}
