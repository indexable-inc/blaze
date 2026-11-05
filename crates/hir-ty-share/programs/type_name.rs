// `type_name` names the type: `name::<&P>` and `name::<&Q>` must not share.
struct P;
struct Q;

fn name<T>() -> &'static str {
    std::any::type_name::<T>()
}

fn last(path: &str) -> &str {
    path.rsplit("::").next().unwrap()
}

fn main() {
    println!("{}", last(name::<&P>()));
    println!("{}", last(name::<&Q>()));
    println!("{}", name::<u32>());
    println!("{}", name::<(u8, i64)>());
    let _ = (P, Q);
}
