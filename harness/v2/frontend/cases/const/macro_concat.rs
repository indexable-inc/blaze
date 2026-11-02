//@ expect: accept
//@ crate: bin
//@ edition: 2021
// constants written by built-in macros: `concat!`, `stringify!` and `line!`
const S: &str = concat!("a", 1, 'b', true, 2.5);
const L: usize = concat!("ab", "cd").len();
const T: &str = stringify!(1 + 2);
const N: u32 = line!();

fn main() {
    println!("{} {} {} {}", S, L, T, N);
}
