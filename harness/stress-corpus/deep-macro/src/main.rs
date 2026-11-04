//! Deep `macro_rules!` recursion and a large token stream. The invocations are generated into
//! generated.rs by ../../gen.py; the macros are here, above `mod generated`, so they are in its
//! textual scope.
#![recursion_limit = "4096"]

/// Emits one function per identifier, each calling the one before: recursion as deep as the list.
macro_rules! chain {
    ($prev:ident;) => {};
    ($prev:ident; $name:ident $($rest:ident)*) => {
        fn $name() -> u64 {
            $prev() + 1
        }
        chain!($name; $($rest)*);
    };
}

/// Reverses a token list one token per step, re-matching the rest each time: quadratic in tokens.
macro_rules! reverse {
    ([] $($acc:tt)*) => {
        [$($acc),*]
    };
    ([$head:tt $($tail:tt)*] $($acc:tt)*) => {
        reverse!([$($tail)*] $head $($acc)*)
    };
}

/// One flat repetition over a very large invocation.
macro_rules! pairs {
    ($($a:literal => $b:literal),* $(,)?) => {
        pub static PAIRS: &[(u64, u64)] = &[$(($a, $b)),*];
    };
}

/// A macro that defines a macro, nested: each level's expansion defines the next level's macro.
macro_rules! nest {
    ($name:ident, $value:expr) => {
        macro_rules! $name {
            () => {
                $value
            };
        }
    };
}

nest!(level_one, 1u32);
nest!(level_two, level_one!() + 1);
nest!(level_three, level_two!() * 10);

mod generated;

fn main() {
    use generated::*;

    assert_eq!(last(), CHAIN_LEN);
    assert_eq!(REV.len() as u32, REVERSE_LEN);
    assert_eq!(REV[0], REVERSE_LEN);
    assert_eq!(REV[REV.len() - 1], 1);
    assert_eq!(PAIRS.len(), PAIR_COUNT);
    assert_eq!(PAIRS.iter().map(|&(a, b)| a * b).sum::<u64>(), PAIR_SUM);
    assert_eq!(level_three!(), 20);
    println!("deep-macro ok: chain {}, {} pairs", last(), PAIRS.len());
}
