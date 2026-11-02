//@ expect: reject E0369
struct Meters(u32);

fn unused(a: Meters, b: Meters) -> Meters {
    a + b //~ ERROR E0369
}

fn main() {
    let _ = Meters(1).0;
}
