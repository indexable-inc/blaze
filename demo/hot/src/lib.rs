#[derive(Clone, Copy)]
pub struct Point {
    pub x: i64,
    pub y: i64,
    pub z: i64,
}

#[derive(Clone, Copy)]
pub struct Pair(pub u32, pub u64);

/// Stays compiled by rustc; called from the RA-compiled functions.
pub fn mix(a: u64, b: u64) -> u64 {
    a.wrapping_mul(31).wrapping_add(b)
}

/// Stays compiled by rustc; takes a struct by reference and returns a pair.
pub fn swap(p: &Point, k: u32) -> Pair {
    Pair(k + 1, (p.x + p.y) as u64)
}

pub fn score(p: &Point, pair: Pair, n: u32) -> u64 {
    let mut acc = pair.1;
    let mut i = 0;
    while i < n {
        if i % 2 == 0 {
            acc = mix(acc, p.x as u64);
        } else {
            acc = mix(acc, pair.0 as u64);
        }
        i += 1;
    }
    let s = swap(p, n);
    acc + s.1 + s.0 as u64 + 1000
}

pub fn make_point(a: i64) -> Point {
    Point { x: a, y: a * 2, z: a - 8 }
}

pub fn shift(p: Point, by: i64) -> Point {
    Point { x: p.x + by, y: p.y + by, z: p.z + by }
}
