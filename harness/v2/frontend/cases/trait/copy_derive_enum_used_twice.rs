//@ expect: accept
//@ edition: 2021
// `#[derive(Clone, Copy)]` on a non-generic enum with data.
#[derive(Clone, Copy)]
enum Shape {
    Dot,
    Square(u32),
    Rect { w: u32, h: u32 },
}

fn area(s: Shape) -> u32 {
    match s {
        Shape::Dot => 0,
        Shape::Square(n) => n * n,
        Shape::Rect { w, h } => w * h,
    }
}

fn main() {
    let r = Shape::Rect { w: 2, h: 5 };
    let s = Shape::Square(3);
    let all = [r, s, Shape::Dot, r];
    let total: u32 = all.iter().map(|x| area(*x)).sum();
    println!("{} {} {}", area(r), area(s.clone()), total);
}
