//@ expect: accept
enum Maybe<T> {
    Nothing,
    Just(T),
}

fn unwrap_or<T>(m: Maybe<T>, default: T) -> T {
    match m {
        Maybe::Nothing => default,
        Maybe::Just(t) => t,
    }
}

fn main() {
    let a: Maybe<u32> = Maybe::Nothing;
    let b = Maybe::Just(4);
    let c: Maybe<u64> = b;
    assert!(unwrap_or(a, 1) == 1);
    assert!(unwrap_or(c, 1) == 4);
}
