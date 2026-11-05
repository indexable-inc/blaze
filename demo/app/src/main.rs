use hot::{make_point, score, shift, Pair};

fn main() {
    let p = shift(make_point(5), 3);
    println!("point = ({}, {}, {})", p.x, p.y, p.z);
    println!("score = {}", score(&p, Pair(9, 100), 10));
}
