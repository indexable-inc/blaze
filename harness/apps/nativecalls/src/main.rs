//! A program whose C calls back into it: what cargo links for the stand-ins of a build with
//! every crate blaze's has none of the functions the C calls.

unsafe extern "C" {
    fn nativecalls_iterate(seed: u64, rounds: u64) -> u64;
}

#[unsafe(no_mangle)]
extern "C" fn nativecalls_step(x: u64) -> u64 {
    x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407).rotate_right(17)
}

fn main() {
    let rounds = std::env::args().nth(1).and_then(|it| it.parse().ok()).unwrap_or(1000);
    println!("{}", unsafe { nativecalls_iterate(42, rounds) });
}
