//@ expect: accept
//@ edition: 2021
// Auto-trait goals with `'static` (global, keyed) and with a function's own lifetimes (free
// local regions, which take the canonical path).
fn is_send<T: Send>() -> u32 {
    1
}
fn is_sync<T: Sync>() -> u32 {
    1
}

fn statics() -> u32 {
    is_send::<&'static str>() + is_sync::<&'static [u8]>() + is_send::<&'static &'static str>()
}

fn local<'a>(s: &'a String) -> u32 {
    is_send::<&'a String>() + is_sync::<&'a String>() + is_send::<(&'a str, &'a [u8])>() + s.len() as u32
}

fn bounded<'a, T: Sync + 'a>(t: &'a T) -> (&'a T, u32) {
    (t, is_send::<&'a T>() + is_send::<Option<&'a T>>())
}

fn main() {
    let s = String::from("abc");
    let (r, n) = bounded(&5u64);
    println!("{} {} {} {}", statics(), local(&s), r, n);
}
