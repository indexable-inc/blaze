// A function turned into a pointer is the pointer to its own instance: `ptr::<&P>` and
// `ptr::<&Q>` must not share. Only what Rust guarantees is printed: a pointer from one
// instance compares equal to itself and calls its function. Whether pointers to two
// instances whose code is the same compare equal is not guaranteed (rustc may merge them, as
// `unnamed_addr` allows), so it is not printed.
struct P(u32);
struct Q(u32);

fn id<T>(x: T) -> T {
    x
}

fn ptr<T>() -> fn(T) -> T {
    id::<T>
}

fn apply<T>(f: fn(T) -> T, x: T) -> T {
    f(x)
}

fn main() {
    let a = ptr::<&P>();
    println!("{}", a == a);
    println!("{}", a(&P(5)).0);
    println!("{}", ptr::<&Q>()(&Q(6)).0);
    println!("{}", apply(id::<u32>, 7));
    let closure: fn(u8) -> u8 = |x| x + 1;
    println!("{}", apply(closure, 8));
}
