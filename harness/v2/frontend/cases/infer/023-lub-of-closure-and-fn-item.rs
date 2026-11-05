//@ expect: accept
fn double(x: u32) -> u32 {
    x * 2
}

fn main() {
    let flag = true;
    let f = if flag { double } else { |x: u32| x + 1 };
    assert!(f(4) == 8);
    let g = match 1 {
        0 => |x: u32| x,
        _ => |x: u32| x + 100,
    };
    assert!(g(1) == 101);
}
