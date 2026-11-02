//@ expect: accept
fn adder(n: u32) -> impl Fn(u32) -> u32 {
    move |x| x + n
}

fn boxed(n: u32) -> Box<dyn FnMut() -> u32> {
    let mut count = 0;
    Box::new(move || {
        count += n;
        count
    })
}

fn main() {
    let add = adder(3);
    let mut next = boxed(2);
    next();
    println!("{} {}", add(4), next());
}
