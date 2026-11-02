// `needs_drop` is in a type's fact: `nd::<&D>` and `nd::<&E>` may share, `nd::<D>` and
// `nd::<E>` must not.
struct D(u32);
impl Drop for D {
    fn drop(&mut self) {
        println!("drop D {}", self.0);
    }
}
struct E(#[allow(dead_code)] u32);

fn nd<T>() -> bool {
    std::mem::needs_drop::<T>()
}

fn main() {
    println!("{}", nd::<D>());
    println!("{}", nd::<E>());
    println!("{}", nd::<&D>());
    println!("{}", nd::<&E>());
    println!("{}", nd::<(E, D)>());
    println!("{}", nd::<std::mem::ManuallyDrop<D>>());
    let _ = E(0);
}
