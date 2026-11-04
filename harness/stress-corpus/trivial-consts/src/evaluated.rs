//! The few constants that need real const evaluation: operators over other constants, const fn
//! calls with loops, `size_of`, array repeat lengths and const blocks.
#![allow(dead_code)]

use crate::consts::{c_int, c_ulong};

pub const O_RDONLY: c_int = 0;
pub const O_WRONLY: c_int = 1;
pub const O_RDWR: c_int = 2;
pub const O_ACCMODE: c_int = O_RDONLY | O_WRONLY | O_RDWR;

#[repr(C)]
pub struct Winsize {
    pub ws_row: u16,
    pub ws_col: u16,
    pub ws_xpixel: u16,
    pub ws_ypixel: u16,
}

pub const fn ioc(dir: u32, ty: u32, nr: u32, size: u32) -> u32 {
    (dir << 30) | (size << 16) | (ty << 8) | nr
}

pub const TIOCGWINSZ: c_ulong =
    ioc(2, b'T' as u32, 0x13, core::mem::size_of::<Winsize>() as u32) as c_ulong;

pub const FD_SETSIZE: usize = 1024;
pub const FD_WORDS: usize = FD_SETSIZE / (8 * core::mem::size_of::<c_ulong>());
pub const EMPTY_SET: [c_ulong; FD_WORDS] = [0; FD_WORDS];

pub const fn fib(n: u32) -> u64 {
    let (mut a, mut b, mut i) = (0u64, 1u64, 0);
    while i < n {
        let t = a + b;
        a = b;
        b = t;
        i += 1;
    }
    a
}

pub const FIB_80: u64 = fib(80);

pub const MASKS: [u32; 32] = {
    let mut m = [0u32; 32];
    let mut i = 0;
    while i < 32 {
        m[i] = 1 << i;
        i += 1;
    }
    m
};

pub const SIG_NAMES: [&str; 4] = ["HUP", "INT", "QUIT", "ILL"];

pub const SIGRTMIN: c_int = 34;
pub const SIGRTMAX: c_int = SIGRTMIN + (MASKS.len() as c_int) - 2;
