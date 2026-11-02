//@ expect: accept
fn pick(n: u32) -> u32 {
    let x;
    if n > 2 {
        x = 1;
    } else {
        x = 2;
    }
    let y: u32;
    match n {
        0 => y = 10,
        1 | 2 => y = 20,
        _ => y = 30,
    }
    let z;
    loop {
        z = n * 2;
        break;
    }
    x + y + z
}

fn main() {
    for n in 0..5 {
        println!("{}", pick(n));
    }
}
