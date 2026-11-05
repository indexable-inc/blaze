//@ expect: accept
struct D(&'static str);

impl Drop for D {
    fn drop(&mut self) {
        println!("drop {}", self.0);
    }
}

struct Wrap<'a> {
    inner: &'a D,
}

fn main() {
    let r = &D("extended by reference");
    let t = (&D("in a tuple"), 1);
    let w = Wrap { inner: &D("in a struct") };
    let s: &[D] = &[D("in an array")];
    let not_extended = D("call temporary").0.len();
    println!("{} {} {} {} {not_extended}", r.0, t.0 .0, w.inner.0, s[0].0);
    let block = {
        let _inner = D("inner");
        &D("block tail, extended")
    };
    println!("{}", block.0);
    let chosen = if not_extended > 3 { &D("then branch") } else { &D("else branch") };
    println!("{}", chosen.0);
}
