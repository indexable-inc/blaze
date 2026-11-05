//@ expect: accept
//@ edition: 2021
// Box, Vec and String reach a raw pointer inside, but their `unsafe impl`s (through `Unique`
// and the impls on Vec and String's fields) make them Send and Sync when the element is.
fn is_send<T: Send>() -> u32 {
    1
}
fn is_sync<T: Sync>() -> u32 {
    1
}

struct Owned {
    name: String,
    bytes: Vec<u8>,
    child: Option<Box<Owned>>,
    boxed: Box<[u16]>,
}

fn main() {
    let o = Owned {
        name: String::from("o"),
        bytes: vec![1, 2],
        child: None,
        boxed: vec![3u16, 4, 5].into_boxed_slice(),
    };
    let n = is_send::<Box<u8>>()
        + is_sync::<Box<str>>()
        + is_send::<Vec<String>>()
        + is_sync::<Vec<Vec<u8>>>()
        + is_send::<String>()
        + is_send::<Owned>()
        + is_sync::<Owned>();
    println!("{} {} {} {} {}", n, o.name, o.bytes.len(), o.child.is_none(), o.boxed.len());
}
