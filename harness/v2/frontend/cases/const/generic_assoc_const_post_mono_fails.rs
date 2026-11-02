//@ expect: reject E0080
//@ crate: bin
//@ edition: 2021
// a generic associated constant `size_of::<T>() - 1`: fine for u32, fails only when used with the zero-sized ()
struct W<T>(T);

impl<T> W<T> {
    const LESS: usize = std::mem::size_of::<T>() - 1;
}

fn main() {
    println!("{}", W::<u32>::LESS);
    println!("{}", W::<()>::LESS);
}
