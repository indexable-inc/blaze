//! A fixed "other program" for the gap between calls (Zooko, September 30,
//! 2026): a program that hashes between other tasks, or shares a machine
//! with other programs, runs other code between its calls, and that code
//! fills the caches in place of the hash's own instructions.
//!
//! [`run`] executes [`FUNCTIONS`] distinct functions once each, about
//! 1 KiB of machine code apiece (each round loads 64-bit constants unique
//! to its function, so no two bodies are alike and none fold away): about
//! 1 MiB of instructions, several times any core's L1 instruction cache
//! (Apple M4 P-core 192 KiB), on every platform, with no platform code.
//! It touches no memory beyond the function table and its stack.

/// Distinct generated functions [`run`] calls.
pub const FUNCTIONS: usize = 1024;

macro_rules! round {
    ($x:ident, $n:expr, $r:expr) => {
        $x = ($x ^ ($n.wrapping_mul(0x9e37_79b9_7f4a_7c15).wrapping_add(($r as u64).wrapping_mul(0xbf58_476d_1ce4_e5b9))))
            .rotate_left(($r * 7 + 3) % 64)
            .wrapping_mul(($n ^ ($r << 32)) | 1);
    };
}

#[inline(never)]
fn block<const N: u64>(mut x: u64) -> u64 {
    round!(x, N, 1);
    round!(x, N, 2);
    round!(x, N, 3);
    round!(x, N, 4);
    round!(x, N, 5);
    round!(x, N, 6);
    round!(x, N, 7);
    round!(x, N, 8);
    round!(x, N, 9);
    round!(x, N, 10);
    round!(x, N, 11);
    round!(x, N, 12);
    round!(x, N, 13);
    round!(x, N, 14);
    round!(x, N, 15);
    round!(x, N, 16);
    round!(x, N, 17);
    round!(x, N, 18);
    round!(x, N, 19);
    round!(x, N, 20);
    round!(x, N, 21);
    round!(x, N, 22);
    round!(x, N, 23);
    round!(x, N, 24);
    round!(x, N, 25);
    round!(x, N, 26);
    round!(x, N, 27);
    round!(x, N, 28);
    round!(x, N, 29);
    round!(x, N, 30);
    round!(x, N, 31);
    round!(x, N, 32);
    x
}

macro_rules! table4 {
    ($b:expr) => {
        [block::<{ $b }>, block::<{ $b + 1 }>, block::<{ $b + 2 }>, block::<{ $b + 3 }>]
    };
}
macro_rules! table16 {
    ($b:expr) => {
        concat4::<4, 16>(table4!($b), table4!($b + 4), table4!($b + 8), table4!($b + 12))
    };
}
macro_rules! table64 {
    ($b:expr) => {
        concat4::<16, 64>(table16!($b), table16!($b + 16), table16!($b + 32), table16!($b + 48))
    };
}
macro_rules! table256 {
    ($b:expr) => {
        concat4::<64, 256>(table64!($b), table64!($b + 64), table64!($b + 128), table64!($b + 192))
    };
}

type Block = fn(u64) -> u64;

const fn concat4<const M: usize, const R: usize>(a: [Block; M], b: [Block; M], c: [Block; M], d: [Block; M]) -> [Block; R] {
    assert!(R == 4 * M, "four parts make the whole");
    let mut out: [Block; R] = [block::<0>; R];
    let mut i = 0;
    while i < M {
        out[i] = a[i];
        out[M + i] = b[i];
        out[2 * M + i] = c[i];
        out[3 * M + i] = d[i];
        i += 1;
    }
    out
}

static TABLE: [Block; FUNCTIONS] = concat4::<256, 1024>(table256!(0), table256!(256), table256!(512), table256!(768));

/// Run every function once, in order, each on the result of the one
/// before: about 1 MiB of distinct code, executed once.
pub fn run() {
    let mut x = std::hint::black_box(1u64);
    for f in std::hint::black_box(&TABLE) {
        x = f(x);
    }
    std::hint::black_box(x);
}

#[cfg(test)]
mod tests {
    #[test]
    fn functions_are_distinct_and_large() {
        let mut addresses: Vec<usize> = super::TABLE.iter().map(|&f| f as usize).collect();
        addresses.sort_unstable();
        addresses.dedup();
        assert_eq!(addresses.len(), super::FUNCTIONS, "no two functions merged");
        // Code size: the spread of the functions' addresses (the linker
        // lays them out in one run), at least 512 bytes each.
        let span = addresses.last().unwrap() - addresses.first().unwrap();
        assert!(span >= super::FUNCTIONS * 512, "{span} bytes of code for {} functions", super::FUNCTIONS);
        super::run();
    }
}
