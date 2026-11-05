//! 4 MiB of words, deflated, for the program to inflate.

fn main() {
    let words = ["const", "eval", "inflate", "blaze", "heap", "budget", "the", "a", "of", "step"];
    let mut data = Vec::with_capacity(4 << 20);
    let mut x = 1u32;
    while data.len() < 4 << 20 {
        x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        data.extend_from_slice(words[(x >> 28) as usize % words.len()].as_bytes());
        data.push(if x & 0xf == 0 { b'\n' } else { b' ' });
    }
    data.truncate(4 << 20);
    let deflated = miniz_oxide::deflate::compress_to_vec(&data, 6);
    let out = std::path::PathBuf::from(std::env::var_os("OUT_DIR").unwrap());
    std::fs::write(out.join("data.deflate"), deflated).unwrap();
}
