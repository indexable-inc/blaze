//! Inflates 4 MiB with miniz_oxide at run time and, with the feature `const`, in a constant
//! too, which the program checks against what it computes.

const DEFLATED: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/data.deflate"));

fn inflate() -> (usize, u32) {
    let Ok(out) = miniz_oxide::inflate::decompress_to_vec(DEFLATED) else { return (0, 0) };
    let mut hash = 0x811c_9dc5u32;
    for &b in &out {
        hash = (hash ^ u32::from(b)).wrapping_mul(0x0100_0193);
    }
    (out.len(), hash)
}

#[cfg(feature = "const")]
const AT_COMPILE_TIME: (usize, u32) = inflate();

fn main() {
    let at_run_time = inflate();
    #[cfg(feature = "const")]
    assert_eq!(AT_COMPILE_TIME, at_run_time, "the constant is not what the program computes");
    println!("inflated {} bytes, FNV-1a {}", at_run_time.0, at_run_time.1);
}
