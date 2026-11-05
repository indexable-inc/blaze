//@ expect: accept
//@ edition: 2021
// A manual `impl Clone` is the one called: its result is printed.
struct Counter {
    n: u32,
}

impl Clone for Counter {
    fn clone(&self) -> Self {
        Counter { n: self.n + 1 }
    }
}

fn twice<T: Clone>(t: &T) -> T {
    t.clone().clone()
}

fn main() {
    let c = Counter { n: 0 };
    let d = c.clone();
    let e = twice(&d);
    let v = vec![Counter { n: 10 }];
    let w = v.clone();
    println!("{} {} {} {}", c.n, d.n, e.n, w[0].n);
}
