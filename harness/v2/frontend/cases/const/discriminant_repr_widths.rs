//@ expect: accept
//@ crate: bin
//@ edition: 2021
// discriminants at the edges of their repr: i8 MIN and MAX, a negative one followed by an implicit one, u64 MAX, i128 MIN
#[derive(Clone, Copy)]
#[repr(i8)]
enum Small {
    Min = -128,
    Neg = -1,
    Zero,
    Max = 127,
}

#[derive(Clone, Copy)]
#[repr(u64)]
enum Big {
    Max = 18446744073709551615,
    Low = 0,
}

#[derive(Clone, Copy)]
#[repr(i128)]
enum Huge {
    Min = -170141183460469231731687303715884105728,
    Next,
}

fn main() {
    println!("{} {} {} {} {}", Small::Min as i8, Small::Neg as i8, Small::Zero as i8, Small::Max as i8, Small::Min as u8);
    println!("{} {}", Big::Max as u64, Big::Low as u64);
    println!("{} {}", Huge::Min as i128, Huge::Next as i128);
}
