//@ expect: accept
struct D(String);

impl Drop for D {
    fn drop(&mut self) {
        println!("drop {}", self.0);
    }
}

fn main() {
    for i in 0..4 {
        let _a = D(format!("a{i}"));
        if i % 2 == 0 {
            let _skip = D(format!("skip{i}"));
            continue;
        }
        let _b = D(format!("b{i}"));
        println!("body {i}");
    }
    let mut i = 0;
    while i < 3 {
        i += 1;
        let _w = D(format!("w{i}"));
        if i == 2 {
            continue;
        }
        println!("while {i}");
    }
}
