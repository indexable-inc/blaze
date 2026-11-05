//! SHA-256 of 1 MiB with sha2 (its portable code) at run time and, with the feature `const`, in
//! a constant too, which the program checks against what it computes.

use sha2::{Digest, Sha256};

fn digest() -> [u8; 32] {
    let data: Vec<u8> = (0..1u32 << 20).map(|i| (i.wrapping_mul(31) % 251) as u8).collect();
    Sha256::digest(&data).into()
}

#[cfg(feature = "const")]
const AT_COMPILE_TIME: [u8; 32] = digest();

fn main() {
    let at_run_time = digest();
    #[cfg(feature = "const")]
    assert_eq!(AT_COMPILE_TIME, at_run_time, "the constant is not what the program computes");
    let hex: String = at_run_time.iter().map(|b| format!("{b:02x}")).collect();
    println!("sha256 of 1048576 bytes: {hex}");
}
