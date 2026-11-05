//@ expect: accept
struct D(String);

impl Drop for D {
    fn drop(&mut self) {
        println!("drop {}", self.0);
    }
}

fn consume(d: D) -> usize {
    d.0.len()
}

fn pick(value: Option<D>) -> usize {
    match value {
        Some(d) if d.0.starts_with('l') => consume(d),
        Some(d) if d.0.is_empty() => 0,
        Some(d) => {
            println!("other {}", d.0);
            1
        }
        None => 99,
    }
}

fn main() {
    println!("{}", pick(Some(D("long".into()))));
    println!("{}", pick(Some(D(String::new()))));
    println!("{}", pick(Some(D("short".into()))));
    println!("{}", pick(None));
}
