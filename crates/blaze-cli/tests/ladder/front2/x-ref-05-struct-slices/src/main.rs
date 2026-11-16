//! x-ref-05: a slice of structs: reading fields by index, and writing through `&mut [T]`.
//!
//! Elements of `[T]` are laid out one after another at a stride of `size_of::<T>()` (the
//! Reference, "Type layout": "Arrays are laid out so that the zero-based nth element of the
//! array is offset from the start of the array by n * size_of::<T>() bytes"; a slice's
//! elements likewise).
//! ps = [P(1, 2), P(3, 4), P(5, 6)] with P { x: u32, y: u8 }:
//! 1. sum of x = 1 + 3 + 5 = 9; sum of y = 2 + 4 + 6 = 12.
//! 2. `scale(&mut ps, 10)` multiplies every x: ps[2].x = 50, ps[0].x = 10; y untouched: ps[1].y 4.
//! 3. a sub-slice &ps[1..]: its first x is 30.
//! A wrong builder that strides by the unpadded field sum (5 bytes, not 8) exits 1.

struct P {
    x: u32,
    y: u8,
}

fn sums(ps: &[P]) -> (u32, u32) {
    let (mut sx, mut sy, mut i) = (0, 0, 0);
    while i < ps.len() {
        sx += ps[i].x;
        sy += ps[i].y as u32;
        i += 1;
    }
    (sx, sy)
}

fn scale(ps: &mut [P], k: u32) {
    let mut i = 0;
    while i < ps.len() {
        ps[i].x *= k;
        i += 1;
    }
}

fn main() -> std::entry::Status {
    let mut ps = [P { x: 1, y: 2 }, P { x: 3, y: 4 }, P { x: 5, y: 6 }];
    let (sx, sy) = sums(&ps);
    scale(&mut ps, 10);
    let status = if sx != 9 || sy != 12 {
        1
    } else if ps[2].x != 50 || ps[0].x != 10 || ps[1].y != 4 {
        2
    } else if (&ps[1..])[0].x != 30 {
        3
    } else {
        0
    };
    std::entry::Status(status)
}
