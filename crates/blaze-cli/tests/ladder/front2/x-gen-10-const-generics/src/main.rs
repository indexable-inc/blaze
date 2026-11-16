//! x-gen-10: const generics: `[T; N]` with N read in the body, per instance.
//!
//! The Reference, "Generic parameters", "Const generics": "Const generic parameters allow
//! items to be generic over constant values"; "The const identifier introduces a name in the
//! value namespace for the constant parameter".
//! 1. total([1, 2, 3]) with N = 3: 6; and it reports N: 3.
//! 2. total([10; 5]) with N = 5: 50; N 5.
//! 3. Buf::<4>::new().cap() = 4; Buf::<16>: 16; size_of::<Buf<16>>() = 16 (an array of 16 u8).
//! 4. N in a type and a value at once: `first_n::<2>(&[7, 8, 9])` copies the first 2 into a
//!    [u32; 2]: [7, 8]: 7 * 10 + 8 = 78.
//! A wrong builder that shares one instance for every N exits 2 (N read as 3).

use core::mem::size_of;

fn total<const N: usize>(a: [u32; N]) -> (u32, usize) {
    let (mut s, mut i) = (0, 0);
    while i < N {
        s += a[i];
        i += 1;
    }
    (s, N)
}

struct Buf<const N: usize> {
    data: [u8; N],
}

impl<const N: usize> Buf<N> {
    fn new() -> Self {
        Buf { data: [0; N] }
    }
    fn cap(&self) -> usize {
        N + self.data[0] as usize
    }
}

fn first_n<const N: usize>(s: &[u32]) -> [u32; N] {
    let mut out = [0; N];
    let mut i = 0;
    while i < N {
        out[i] = s[i];
        i += 1;
    }
    out
}

fn main() -> std::entry::Status {
    let two = first_n::<2>(&[7, 8, 9]);
    let status = if total([1, 2, 3]) != (6, 3) {
        1
    } else if total([10; 5]) != (50, 5) {
        2
    } else if Buf::<4>::new().cap() != 4 || Buf::<16>::new().cap() != 16 || size_of::<Buf<16>>() != 16 {
        3
    } else if two[0] * 10 + two[1] != 78 {
        4
    } else {
        0
    };
    std::entry::Status(status)
}
