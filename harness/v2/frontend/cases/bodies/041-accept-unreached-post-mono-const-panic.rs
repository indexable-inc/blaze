//@ expect: accept
struct Check<T>(T);

impl<T> Check<T> {
    const FAIL: () = panic!("never instantiated");
}

#[allow(dead_code)]
fn never_instantiated<T>() {
    let () = Check::<T>::FAIL;
}

fn main() {}
