//@ expect: accept
//@ edition: 2021
// An auto-trait goal on a parameter, proved by the where clause that names it, directly and
// through ADTs over the parameter.
struct Wrap<T>(T);

fn is_send<T: Send>() -> u32 {
    1
}

fn check<S>(s: S) -> u32
where
    S: Send,
{
    let n = is_send::<S>() + is_send::<Wrap<S>>() + is_send::<Vec<(S, u8)>>() + is_send::<Option<Box<S>>>();
    drop(s);
    n
}

fn main() {
    println!("{} {}", check(5u8), check(String::from("s")));
}
