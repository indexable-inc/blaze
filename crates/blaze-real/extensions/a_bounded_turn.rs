//! bounded_turn: `#[blaze::bounded_turn]` on a turn that loops with no bound. A working layer refuses it.
//! expect: refuse

#[blaze::bounded_turn]
fn turn(mut n: i32) -> i32 {
    while n != 1 {
        n = if n % 2 == 0 { n / 2 } else { 3 * n + 1 };
    }
    n
}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    turn(27)
}
