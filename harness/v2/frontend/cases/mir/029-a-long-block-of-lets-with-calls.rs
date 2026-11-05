//@ expect: accept
use std::panic;

struct D(&'static str);

impl Drop for D {
    fn drop(&mut self) {
        println!("drop {}", self.0);
    }
}

fn make(name: &'static str) -> D {
    if std::hint::black_box(name) == "panic" {
        panic!("made {name}");
    }
    D(name)
}

fn long() {
    let a = make("a");
    let b = make("b");
    let c = make("c");
    let d = make("d");
    let e = make("e");
    let f = make("f");
    let g = make("g");
    let h = make("h");
    let i = make("i");
    let j = make("j");
    let k = make("k");
    let l = make("l");
    let m = make("m");
    let n = make("n");
    let o = make("o");
    let p = make("p");
    let q = make("q");
    let r = make("r");
    let s = make("s");
    let t = make("t");
    let u = make("u");
    let v = make("v");
    let w = make("w");
    let x = make("x");
    let y = make("y");
    let z = make("z");
    let names = [a.0, b.0, c.0, d.0, e.0, f.0, g.0, h.0, i.0, j.0, k.0, l.0, m.0];
    let more = [n.0, o.0, p.0, q.0, r.0, s.0, t.0, u.0, v.0, w.0, x.0, y.0, z.0];
    println!("{} {}", names.concat(), more.concat());
    let _last = make("panic");
}

fn main() {
    let caught = panic::catch_unwind(long);
    println!("caught: {}", caught.is_err());
}
