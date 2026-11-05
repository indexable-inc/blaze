//@ expect: accept
//@ edition: 2024
struct D(&'static str);

impl Drop for D {
    fn drop(&mut self) {
        println!("drop {}", self.0);
    }
}

fn get(d: &D, give: bool) -> Option<usize> {
    give.then(|| d.0.len())
}

fn main() {
    for give in [true, false] {
        if let Some(n) = get(&D("scrutinee"), give) {
            println!("then {n}");
        } else {
            println!("else");
        }
        println!("after the if");
    }
}
