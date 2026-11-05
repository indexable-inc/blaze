//@ expect: accept
//@ edition: 2021
// Paths find functions and consts, inherent and of traits, with no autoref.
struct S;

impl S {
    const N: u32 = 3;
    fn make() -> S {
        S
    }
    fn get(&self) -> u32 {
        Self::N
    }
}

trait K {
    const M: u32;
}
impl K for S {
    const M: u32 = 4;
}

fn main() {
    let s = S::make();
    println!("{} {} {}", S::get(&s), <S>::N, S::M);
}
