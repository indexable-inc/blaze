#[inline(never)]
fn the_deepest_one(n: u32) -> u32 {
    if n > 2 {
        panic!("too deep at {n}");
    }
    n
}

#[inline(never)]
fn the_one_between(n: u32) -> u32 {
    the_deepest_one(n + 1) + 1
}

fn main() {
    println!("{}", the_one_between(std::env::args().count() as u32 + 1));
}
