//@ expect: accept
struct D(&'static str);

impl Drop for D {
    fn drop(&mut self) {
        println!("drop {}", self.0);
    }
}

struct Holder {
    first: D,
    second: D,
}

fn main() {
    let mut x = D("old");
    println!("before");
    x = D("new");
    println!("after {}", x.0);
    let mut h = Holder { first: D("f1"), second: D("s1") };
    h.second = D("s2");
    println!("field assigned");
    let mut slots = [D("slot0"), D("slot1")];
    slots[1] = D("slot1 again");
    let mut late;
    late = D("late");
    println!("{} {} {} {}", h.first.0, h.second.0, slots[1].0, late.0);
    late = D("later");
    println!("{}", late.0);
}
