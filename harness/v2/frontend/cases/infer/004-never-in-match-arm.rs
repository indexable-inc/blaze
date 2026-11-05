//@ expect: accept
fn half(n: u32) -> u32 {
    match n % 2 {
        0 => n / 2,
        _ => loop {
            break n;
        },
    }
}

fn checked(n: u32) -> u32 {
    match n {
        0 => panic!("zero"),
        n => n,
    }
}

fn main() {
    assert!(half(4) == 2);
    assert!(half(3) == 3);
    assert!(checked(5) == 5);
}
