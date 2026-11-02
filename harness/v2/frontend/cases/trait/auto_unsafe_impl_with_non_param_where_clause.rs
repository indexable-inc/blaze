//@ expect: accept
//@ edition: 2021
// An auto-trait impl whose where clause is not an auto trait on a parameter (`Vec<T>: Clone`):
// the shape walk hands this to the solver.
struct Q<T> {
    ptr: *const T,
}

unsafe impl<T> Send for Q<T> where Vec<T>: Clone {}

fn is_send<T: Send>() -> u32 {
    1
}

fn main() {
    let q: Q<u8> = Q { ptr: std::ptr::null() };
    println!("{} {}", is_send::<Q<u8>>() + is_send::<Q<String>>(), q.ptr.is_null());
}
