//! s2-gen-03: closures with the real `core`'s `Fn`, `FnMut`, `FnOnce`: captures by reference, by
//! unique borrow and by move; returned as `impl Fn` and as `Box<dyn Fn>`; composed; coerced to a
//! `fn` pointer (a non-capturing closure: the Reference, "Closure types", "Other traits");
//! a closure holding its own state; a fn item passed where a closure is expected.

fn apply<F: Fn(i32) -> i32>(f: F, x: i32) -> i32 {
    f(x)
}

fn apply_twice(mut f: impl FnMut()) {
    f();
    f();
}

fn consume(f: impl FnOnce() -> String) -> String {
    f()
}

fn make_adder(n: i32) -> impl Fn(i32) -> i32 {
    move |x| x + n
}

fn make_boxed(kind: &str) -> Box<dyn Fn(i32) -> i32> {
    match kind {
        "double" => Box::new(|x| x * 2),
        _ => Box::new(|x| x - 1),
    }
}

fn compose<A, B, C>(f: impl Fn(A) -> B, g: impl Fn(B) -> C) -> impl Fn(A) -> C {
    move |x| g(f(x))
}

fn main() {
    let k = 10;
    println!("{}", apply(|x| x * k, 4));
    let mut count = 0;
    apply_twice(|| count += 5);
    println!("{count}");
    let s = String::from("moved");
    println!("{}", consume(move || s + "!"));
    let add3 = make_adder(3);
    println!("{} {}", add3(1), add3(10));
    let fs = [make_boxed("double"), make_boxed("dec")];
    println!("{:?}", fs.iter().map(|f| f(7)).collect::<Vec<_>>());
    let h = compose(|x: i32| x + 1, |y: i32| y.to_string() + "?");
    println!("{}", h(41));
    let mut v = vec![3, 1, 2];
    let mut calls = 0;
    v.sort_by(|a, b| {
        calls += 1;
        a.cmp(b)
    });
    println!("{:?} {}", v, calls > 0);
    let fp: fn(i32) -> i32 = |x| x * x;
    println!("{}", [1, 2, 3].map(fp).iter().sum::<i32>());
    let mut counter = {
        let mut n = 0;
        move || {
            n += 1;
            n
        }
    };
    counter();
    counter();
    println!("{}", counter());
    let names = vec!["b", "a"];
    let has = |n: &str| names.contains(&n);
    println!("{} {}", has("a"), has("z"));
    println!("{}", apply(i32::abs, -9));
}
