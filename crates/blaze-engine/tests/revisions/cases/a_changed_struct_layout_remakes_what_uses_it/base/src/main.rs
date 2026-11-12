struct Point {
    x: u8,
    y: u32,
}

fn make(y: u32) -> Point {
    Point { x: 1, y }
}

fn read(point: &Point) -> u32 {
    point.y + point.x as u32
}

fn unrelated(x: u32) -> u32 {
    x * 2
}

fn main() {
    let point = make(7);
    println!("{} {} {}", read(&point), std::mem::size_of::<Point>(), unrelated(2));
}
