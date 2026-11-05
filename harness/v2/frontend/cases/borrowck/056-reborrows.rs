//@ expect: accept
fn bump(x: &mut i32) {
    *x += 1;
}

fn main() {
    let mut n = 0;
    let r = &mut n;
    bump(r);
    bump(r);
    {
        let r2 = &mut *r;
        *r2 += 10;
    }
    *r += 100;
    let shared = &*r;
    let again = &*r;
    println!("{} {}", shared, again);
    println!("{n}");
}
