//@ expect: accept
struct D(&'static str);

impl Drop for D {
    fn drop(&mut self) {
        println!("drop {}", self.0);
    }
}

struct Pair {
    left: D,
    right: D,
    tag: u8,
}

fn main() {
    let p = Pair { left: D("left"), right: D("right"), tag: 1 };
    let moved = p.left;
    println!("moved {} tag {}", moved.0, p.tag);
    let t = (D("t0"), D("t1"), D("t2"));
    let (_, middle, _) = t;
    println!("middle {}", middle.0);
    let q = Pair { left: D("q left"), right: D("q right"), tag: 2 };
    let Pair { right, .. } = q;
    println!("right {}", right.0);
}
