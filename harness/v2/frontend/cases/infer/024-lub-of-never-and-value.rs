//@ expect: accept
fn first_even(xs: &[u32]) -> u32 {
    let mut i = 0;
    loop {
        let x = if i < xs.len() { xs[i] } else { return 0 };
        if x % 2 == 0 {
            break x;
        }
        i += 1;
    }
}

fn main() {
    assert!(first_even(&[1, 3, 4, 6]) == 4);
    assert!(first_even(&[1]) == 0);
}
