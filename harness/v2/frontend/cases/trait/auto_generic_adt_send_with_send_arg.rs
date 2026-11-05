//@ expect: accept
//@ edition: 2021
// A generic ADT whose answer depends on its argument, asked with arguments that hold.
struct Wrap<T> {
    inner: T,
    count: usize,
}

enum Either<A, B> {
    Left(A),
    Right(B),
}

fn is_send<T: Send>() -> u32 {
    1
}
fn is_sync<T: Sync>() -> u32 {
    1
}

fn main() {
    let w = Wrap { inner: String::from("w"), count: 1 };
    let e: Either<u8, Vec<u8>> = Either::Right(vec![1, 2]);
    let n = is_send::<Wrap<u8>>()
        + is_send::<Wrap<String>>()
        + is_sync::<Wrap<Wrap<&'static str>>>()
        + is_send::<Either<u8, Vec<u8>>>()
        + is_sync::<Either<Box<u8>, (u8, char)>>();
    let len = match e {
        Either::Left(_) => 0,
        Either::Right(v) => v.len(),
    };
    println!("{} {} {} {}", n, w.inner, w.count, len);
}
