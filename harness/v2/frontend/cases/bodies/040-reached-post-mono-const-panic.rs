//@ expect: reject E0080
struct Check<T>(T);

impl<T> Check<T> {
    const FAIL: () = panic!("instantiated"); //~ ERROR E0080
}

fn instantiated<T>() {
    let () = Check::<T>::FAIL;
}

fn main() {
    instantiated::<u8>();
}
