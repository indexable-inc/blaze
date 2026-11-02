//@ expect: reject E0308
struct Meters(u32);
struct Feet(u32);

fn main() {
    let m = Meters(3);
    let _f: Feet = m; //~ ERROR E0308
}
