// As bun's: the C runtime calls a `main` of C's, and the crate has none of Rust's.
#![no_main]

use std::hint::black_box;

macro_rules! times4 {
    ($($body:tt)*) => { $($body)* $($body)* $($body)* $($body)* };
}

// More instructions than LLVM is given in a build that edits follow: in a whole program's
// it is LLVM's all the same. Braced, as rustc parses `times4!(times4!(...))` as an expression
// and rejects it, and so does blaze (#154).
#[inline(never)]
fn mix(seed: u64) -> u64 {
    let mut acc = seed;
    times4! { times4! { times4! { times4! { times4! { times4! { times4! {
        acc = acc.rotate_left(5) ^ black_box(acc >> 3);
    } } } } } } }
    acc
}

#[unsafe(no_mangle)]
extern "C" fn main(argc: i32, _argv: *const *const u8) -> i32 {
    let words: Vec<String> = ["a", "main", "of", "c's"].iter().map(|it| it.to_uppercase()).collect();
    println!("{} {:x}", words.join(" "), mix(argc as u64));
    eprintln!("{argc} arguments");
    3
}
