//@ expect: reject E0382
//@ edition: 2021
// `#[derive(Clone, Copy)]` on `W<T>` is `impl<T: Copy> Copy for W<T>`: `W<String>` is not Copy,
// so the second use is a use after move.
#[derive(Clone, Copy)]
struct W<T>(T);

fn take<T>(_: W<T>) {}

fn main() {
    let ok = W(1u8);
    take(ok);
    take(ok);
    let w = W(String::from("moved"));
    take(w);
    take(w);
}
