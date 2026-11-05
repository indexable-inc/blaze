//@ expect: accept
//@ edition: 2021
// A struct and an enum of scalars are Send, Sync and Unpin by the structural rule alone.
struct Scalars {
    a: u8,
    b: i64,
    c: f32,
    d: bool,
    e: char,
    f: usize,
}

enum Kind {
    A,
    B(u16),
    C { x: i128, y: f64 },
}

fn is_send<T: Send>() -> &'static str {
    "send"
}
fn is_sync<T: Sync>() -> &'static str {
    "sync"
}
fn is_unpin<T: Unpin>() -> &'static str {
    "unpin"
}

fn main() {
    let s = Scalars { a: 1, b: 2, c: 3.0, d: true, e: 'e', f: 6 };
    let k = Kind::C { x: 7, y: 8.0 };
    let n = match k {
        Kind::A => 0,
        Kind::B(v) => v as i128,
        Kind::C { x, y } => x + y as i128,
    };
    println!("{} {} {}", is_send::<Scalars>(), is_sync::<Scalars>(), is_unpin::<Scalars>());
    println!("{} {} {}", is_send::<Kind>(), is_sync::<Kind>(), is_unpin::<Kind>());
    println!("{} {} {} {} {} {} {}", s.a, s.b, s.c, s.d, s.e, s.f, n);
}
