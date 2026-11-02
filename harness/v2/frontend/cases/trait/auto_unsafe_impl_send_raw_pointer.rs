//@ expect: accept
//@ edition: 2021
// `unsafe impl Send` on a struct holding a raw pointer: the impl answers, not the field.
struct Handle {
    ptr: *mut u8,
    len: usize,
}

unsafe impl Send for Handle {}

fn is_send<T: Send>() -> &'static str {
    "send"
}

fn main() {
    let h = Handle { ptr: std::ptr::null_mut(), len: 3 };
    println!("{} {} {}", is_send::<Handle>(), h.ptr.is_null(), h.len);
}
