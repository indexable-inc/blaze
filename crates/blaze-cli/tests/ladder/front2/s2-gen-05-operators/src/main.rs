//! s2-gen-05: operator traits (`Add`, `Mul<i32>`, `Neg`, `AddAssign`, `Index`), `From`/`Into`,
//! `TryFrom`/`TryInto`, derived `Default`, `PartialEq`, `Copy`, and `Ord` helpers.
//!
//! `u8::try_from(300i32)`'s error displays as this nightly's `TryFromIntError` says
//! (library/core/src/num/error.rs, `impl Display for TryFromIntError`: `PosOverflow =>
//! "number too large to fit in target type"`). `Ord::clamp`, `max`, `min`: core/src/cmp.rs.

use std::ops::{Add, AddAssign, Index, Mul, Neg};

#[derive(Debug, Clone, Copy, PartialEq, Default)]
struct V2 {
    x: i32,
    y: i32,
}

impl Add for V2 {
    type Output = V2;
    fn add(self, o: V2) -> V2 {
        V2 { x: self.x + o.x, y: self.y + o.y }
    }
}

impl Mul<i32> for V2 {
    type Output = V2;
    fn mul(self, k: i32) -> V2 {
        V2 { x: self.x * k, y: self.y * k }
    }
}

impl Neg for V2 {
    type Output = V2;
    fn neg(self) -> V2 {
        V2 { x: -self.x, y: -self.y }
    }
}

impl AddAssign for V2 {
    fn add_assign(&mut self, o: V2) {
        self.x += o.x;
        self.y += o.y;
    }
}

struct Poly(Vec<i32>);

impl Index<usize> for Poly {
    type Output = i32;
    fn index(&self, i: usize) -> &i32 {
        self.0.get(i).unwrap_or(&0)
    }
}

struct Celsius(f64);
struct Fahrenheit(f64);

impl From<Celsius> for Fahrenheit {
    fn from(c: Celsius) -> Self {
        Fahrenheit(c.0 * 9.0 / 5.0 + 32.0)
    }
}

#[derive(Debug)]
struct Even(u32);

impl TryFrom<u32> for Even {
    type Error = String;
    fn try_from(n: u32) -> Result<Self, String> {
        if n % 2 == 0 { Ok(Even(n)) } else { Err(format!("{n} is odd")) }
    }
}

fn main() {
    let a = V2 { x: 1, y: 2 };
    let b = V2 { x: 10, y: 20 };
    println!("{:?}", a + b);
    println!("{:?}", -(a * 3));
    let mut c = V2::default();
    c += a;
    c += a;
    println!("{:?} {}", c, c == a * 2);
    let p = Poly(vec![5, 6]);
    println!("{} {} {}", p[0], p[1], p[9]);
    let f: Fahrenheit = Celsius(100.0).into();
    println!("{}", f.0);
    println!("{:?} {:?}", Even::try_from(4), Even::try_from(5));
    let r: Result<Even, _> = 8u32.try_into();
    println!("{:?}", r);
    println!("{:?}", u8::try_from(300i32).map_err(|e| e.to_string()));
    println!("{}", i64::from(7u8) + i64::from(true));
    let s: String = 'c'.into();
    println!("{s}");
    println!("{:?}", "x".cmp("y"));
    println!("{:?}", 3.max(9).min(5).clamp(0, 4));
}
