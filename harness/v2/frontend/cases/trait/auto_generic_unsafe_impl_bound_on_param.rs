//@ expect: accept
//@ edition: 2021
// `unsafe impl<T: Send> Send for P<T>` over a raw pointer field: the impl's where clause is an
// auto trait on its parameter, asked with an argument that holds.
struct P<T> {
    ptr: *const T,
}

unsafe impl<T: Send> Send for P<T> {}

fn is_send<T: Send>() -> u32 {
    1
}

fn main() {
    let p: P<String> = P { ptr: std::ptr::null() };
    let n = is_send::<P<u8>>() + is_send::<P<String>>() + is_send::<P<P<u32>>>();
    println!("{} {}", n, p.ptr.is_null());
}
