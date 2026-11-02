// Drop glue is per type: `hold::<D>` and `hold::<&D>` must not share, and every value drops
// where and in the order rustc drops it.
struct D(&'static str);
impl Drop for D {
    fn drop(&mut self) {
        println!("drop {}", self.0);
    }
}

struct Pair<A, B>(#[allow(dead_code)] A, #[allow(dead_code)] B);

fn hold<T>(x: T) {
    println!("hold");
    let _kept = x;
}

fn swap<T>(a: T, b: T) -> (T, T) {
    (b, a)
}

fn main() {
    hold(D("a"));
    hold(&D("b"));
    hold(Pair(D("c"), D("d")));
    hold((D("e"), 1u8));
    hold(Some(D("f")));
    hold(None::<D>);
    let (x, y) = swap(D("g"), D("h"));
    drop(y);
    drop(x);
    println!("end");
}
