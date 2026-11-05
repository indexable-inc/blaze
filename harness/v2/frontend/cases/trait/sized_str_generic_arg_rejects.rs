//@ expect: reject E0277
//@ edition: 2021
// A type parameter is `Sized` by default: `str` is not.
fn size_of_param<T>() -> usize {
    std::mem::size_of::<T>()
}

fn main() {
    let _n = size_of_param::<str>();
}
