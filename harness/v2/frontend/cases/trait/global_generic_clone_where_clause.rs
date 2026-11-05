//@ expect: accept
//@ edition: 2021
// Goals on type parameters under where clauses (`T: Clone` from the param env), and the same
// functions instantiated at several types.
use std::fmt::Debug;

fn pair<T: Clone>(t: T) -> (T, T) {
    (t.clone(), t)
}

fn many<T>(t: &T) -> Vec<T>
where
    T: Clone + Debug,
{
    vec![t.clone(); 3]
}

fn nested<T: Clone, U: Clone>(t: &T, u: &U) -> (Vec<T>, Option<U>) {
    (vec![t.clone()].clone(), Some(u.clone()).clone())
}

fn main() {
    let (a, b) = pair(String::from("p"));
    let m = many(&7u8);
    let (v, o) = nested(&'c', &vec![1u16]);
    println!("{a} {b} {m:?} {v:?} {o:?}");
}
