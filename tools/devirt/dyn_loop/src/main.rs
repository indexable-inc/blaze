//! Calls through trait objects in hot loops, one kind each, for `tools/devirt/bench.py`: a
//! `dyn` with one type in the program (`Step`), one with three (`Shape`), and a boxed closure
//! (`Box<dyn Fn>`, the one closure of its signature). Prints one checksum a loop, which the
//! bench compares between builds: devirtualized code computes what the indirect calls did.

use std::hint::black_box;

trait Step {
    fn step(&self, x: u64) -> u64;
}

struct Mix(u64);

impl Step for Mix {
    fn step(&self, x: u64) -> u64 {
        x.rotate_left(5) ^ self.0
    }
}

trait Shape {
    fn area(&self) -> u64;
}

struct Square(u64);
struct Rect(u64, u64);
struct Tri(u64, u64);

impl Shape for Square {
    fn area(&self) -> u64 {
        self.0 * self.0
    }
}

impl Shape for Rect {
    fn area(&self) -> u64 {
        self.0 * self.1
    }
}

impl Shape for Tri {
    fn area(&self) -> u64 {
        self.0 * self.1 / 2
    }
}

#[inline(never)]
fn one(step: &dyn Step, n: u64) -> u64 {
    let mut x = 1;
    for _ in 0..n {
        x = step.step(x);
    }
    x
}

#[inline(never)]
fn three(shapes: &[Box<dyn Shape>], n: u64) -> u64 {
    let mut sum = 0u64;
    for at in 0..n {
        sum = sum.wrapping_add(shapes[(at % 3) as usize].area());
    }
    sum
}

#[inline(never)]
fn boxed(f: &Box<dyn Fn(u64) -> u64>, n: u64) -> u64 {
    let mut x = 0;
    for at in 0..n {
        x = f(x ^ at);
    }
    x
}

fn main() {
    let n: u64 = std::env::args()
        .nth(1)
        .and_then(|it| it.parse().ok())
        .expect("one argument: the iterations of each loop");
    let step: Box<dyn Step> = Box::new(Mix(black_box(0x9e37_79b9)));
    let shapes: Vec<Box<dyn Shape>> = vec![
        Box::new(Square(black_box(3))),
        Box::new(Rect(black_box(4), 5)),
        Box::new(Tri(black_box(6), 7)),
    ];
    let k = black_box(0x2545_f491_4f6c_dd1du64);
    let f: Box<dyn Fn(u64) -> u64> = Box::new(move |x| x.wrapping_mul(k).rotate_left(3));
    println!("one {}", one(&*step, black_box(n)));
    println!("three {}", three(&shapes, black_box(n)));
    println!("boxed {}", boxed(&f, black_box(n)));
}
