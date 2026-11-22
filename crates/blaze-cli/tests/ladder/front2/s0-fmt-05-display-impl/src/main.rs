//! s0-fmt-05: user `Display` impls, `Formatter::pad`, `fmt::Write` into a `String`, positional,
//! named and inline arguments, and width and precision taken from arguments (`w$`, `1$`).
//!
//! The `std::fmt` docs: `write!(f, ..)` inside an impl ignores the caller's width (only
//! `Formatter::pad` and the numeric impls apply it); `{:>w$.p$}` reads width and precision from
//! the named arguments; `{:>1$}` reads the width from positional argument 1.

use std::fmt;
use std::fmt::Write as _;

struct Rgb(u8, u8, u8);

impl fmt::Display for Rgb {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "#{:02x}{:02x}{:02x}", self.0, self.1, self.2)
    }
}

struct Name(&'static str);

impl fmt::Display for Name {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.pad(self.0)
    }
}

struct Money(i64);

impl fmt::Display for Money {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let sign = if self.0 < 0 { "-" } else { "" };
        let cents = self.0.abs();
        write!(f, "{}${}.{:02}", sign, cents / 100, cents % 100)
    }
}

fn main() {
    println!("{}", Rgb(255, 128, 0));
    println!("[{:>6}] [{:<6}] [{:^6}]", Name("ab"), Name("ab"), Name("ab"));
    println!("[{:>6}]", Rgb(1, 2, 3));
    println!("{} {}", Money(1234), Money(-5));
    let mut s = String::new();
    for i in 0..3 {
        write!(s, "{i},").unwrap();
    }
    writeln!(s, "end").unwrap();
    print!("{s}");
    let w = 7;
    let p = 2;
    println!("[{:>w$.p$}]", 3.14159);
    println!("[{:>1$}]", "x", 3);
    let joined = format!("{}-{:?}", Name("n"), "q");
    println!("{joined}");
    println!("{}", format_args!("{} {}", 1, 2));
    let v: Vec<String> = [1, 2, 3].iter().map(|n| n.to_string()).collect();
    println!("{}", v.join("+"));
}
