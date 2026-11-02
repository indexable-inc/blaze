//@ expect: accept
struct D(String);

impl Drop for D {
    fn drop(&mut self) {
        println!("drop {}", self.0);
    }
}

enum Shape {
    Dot,
    Line(u32),
    Pair(Option<u32>, Option<u32>),
}

fn walk(shapes: &[Shape]) -> u32 {
    let mut total = 0;
    'shapes: for (index, shape) in shapes.iter().enumerate() {
        let _shape = D(format!("shape{index}"));
        match shape {
            Shape::Dot => continue,
            Shape::Line(n) => {
                let _line = D(format!("line{n}"));
                loop {
                    match total {
                        t if t > 20 => break 'shapes,
                        t if t % 2 == 0 => {
                            total += n;
                            break;
                        }
                        _ => total += 1,
                    }
                }
            }
            Shape::Pair(a, b) => match (a, b) {
                (Some(x), Some(y)) if x == y => {
                    let _same = D(format!("same{x}"));
                    total += x * 2;
                }
                (Some(x), _) | (None, Some(x)) => total += x,
                (None, None) => break,
            },
        }
    }
    total
}

fn main() {
    let shapes = [
        Shape::Dot,
        Shape::Line(3),
        Shape::Pair(Some(2), Some(2)),
        Shape::Pair(None, Some(5)),
        Shape::Line(7),
        Shape::Pair(Some(1), None),
        Shape::Line(30),
        Shape::Pair(None, None),
        Shape::Line(1),
    ];
    println!("{}", walk(&shapes));
    println!("{}", walk(&shapes[..2]));
}
