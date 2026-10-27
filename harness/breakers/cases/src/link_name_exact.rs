//! A foreign function whose `#[link_name]` starts with `\u{1}`, which names the symbol exactly,
//! the platform's prefix included. bindgen writes it where a symbol differs from its Rust name:
//! `aws-lc-sys` declares `#[link_name = "\u{1}_aws_lc_0_45_0_SHA384"]` on Apple targets.
//! ra-jit gave the object writer the name as written: on Mach-O it put its `_` before the mark
//! and the link failed with `"__strlen"` undefined (the mark is not printed); on ELF the mark
//! stayed in.

use std::ffi::{CStr, c_char};

unsafe extern "C" {
    #[cfg_attr(target_vendor = "apple", link_name = "\u{1}_strlen")]
    #[cfg_attr(not(target_vendor = "apple"), link_name = "\u{1}strlen")]
    fn length_of(text: *const c_char) -> usize;
}

pub fn run() -> Vec<usize> {
    let texts: [&CStr; 3] = [c"", c"one", c"a longer one"];
    texts.iter().map(|text| unsafe { length_of(text.as_ptr()) }).collect()
}
