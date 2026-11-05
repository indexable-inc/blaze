//@ expect: accept
struct D(&'static str);

impl Drop for D {
    fn drop(&mut self) {
        println!("drop {}", self.0);
    }
}

fn parse(text: &str) -> Result<u32, String> {
    text.parse::<u32>().map_err(|e| e.to_string())
}

fn sum(a: &str, b: &str) -> Result<u32, String> {
    let _a = D("a");
    let x = parse(a)?;
    let _b = D("b");
    let y = parse(b)?;
    let _c = D("c");
    Ok(x + y)
}

fn first(items: &[Option<u8>]) -> Option<u8> {
    let _guard = D("guard");
    let pair = (D("left"), (*items.first()?)?);
    Some(pair.1)
}

fn main() {
    println!("{:?}", sum("1", "2"));
    println!("{:?}", sum("1", "x"));
    println!("{:?}", sum("x", "2"));
    println!("{:?}", first(&[Some(4)]));
    println!("{:?}", first(&[None]));
    println!("{:?}", first(&[]));
}
