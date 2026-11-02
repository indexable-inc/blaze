//@ expect: accept
struct D(&'static str);

impl Drop for D {
    fn drop(&mut self) {
        println!("drop {}", self.0);
    }
}

fn consume(d: D) {
    println!("consume {}", d.0);
}

fn maybe(give: bool) {
    let a = D("a");
    let b = D("b");
    if give {
        consume(a);
    } else {
        println!("kept a");
    }
    let c = D("c");
    match give {
        true => drop(c),
        false => println!("kept c"),
    }
    println!("end of maybe with {}", b.0);
}

fn main() {
    maybe(std::hint::black_box(true));
    maybe(std::hint::black_box(false));
}
