//@ expect: reject E0277
//@ edition: 2021
// `dyn Fn()` without `+ Send` is not Send, whatever the closure behind it.
fn is_send<T: Send>(_: &T) {}

fn main() {
    let f: Box<dyn Fn() -> u8> = Box::new(|| 1);
    is_send(&f);
}
