// An associated constant is read by its value: `v::<&P>` and `v::<&S>` (both 1) may share,
// `v::<&Q>` (2) must not.
trait K {
    const V: u32;
}

struct P;
struct Q;
struct S;

impl K for &'static P {
    const V: u32 = 1;
}
impl K for &'static Q {
    const V: u32 = 2;
}
impl K for &'static S {
    const V: u32 = 1;
}
impl K for P {
    const V: u32 = 3;
}

fn v<T: K>() -> u32 {
    T::V
}

fn twice<T: K>() -> [u32; 2] {
    [T::V; 2]
}

fn main() {
    println!("{} {} {} {}", v::<&'static P>(), v::<&'static Q>(), v::<&'static S>(), v::<P>());
    println!("{:?} {:?}", twice::<&'static P>(), twice::<&'static Q>());
    let _ = (P, Q, S);
}
