//@ expect: accept
struct D(&'static str);

impl Drop for D {
    fn drop(&mut self) {
        println!("drop {}", self.0);
    }
}

fn get(d: &D, give: bool) -> Option<usize> {
    give.then(|| d.0.len())
}

fn run(give: bool) -> usize {
    let _before = D("before");
    let Some(n) = get(&D("temporary"), give) else {
        println!("else");
        return 0;
    };
    let _after = D("after");
    n
}

fn main() {
    println!("{}", run(true));
    println!("{}", run(false));
}
