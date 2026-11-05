//@ expect: accept
//@ edition: 2021
// `dyn Fn() + Send` is Send by its own bound, so its Box is: the walk hands `dyn` to the solver.
fn is_send<T: Send>(_: &T) -> &'static str {
    "send"
}

fn main() {
    let n = 41u8;
    let f: Box<dyn Fn() -> u8 + Send> = Box::new(move || n + 1);
    println!("{} {}", is_send(&f), f());
}
