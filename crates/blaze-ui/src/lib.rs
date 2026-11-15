//! The agent API's responses as text ([`api`]), for a client of it to print, and why a build
//! made again what it did ([`why`]). What a build shows while it runs and when it ends is
//! blaze-screen's (docs/terminal.md).

#![forbid(unsafe_code)]
#![feature(allocator_api)]

/// `write!` into a `String`, which cannot fail.
macro_rules! put {
    ($out:expr, $($arg:tt)*) => {{
        let _ = ::std::fmt::Write::write_fmt(&mut *$out, format_args!($($arg)*));
    }};
}

pub mod api;
pub mod why;

use std::fmt;

const GREEN: &str = "\x1b[1m\x1b[92m";
const CYAN: &str = "\x1b[1m\x1b[96m";
const YELLOW: &str = "\x1b[1m\x1b[93m";
const RED: &str = "\x1b[1m\x1b[91m";
const DIM: &str = "\x1b[2m";
const RESET: &str = "\x1b[0m";

/// `T` written in `color` when `colored`, then the reset. It takes no width: pad what it
/// paints.
struct Painted<T>(&'static str, T, bool);

impl<T: fmt::Display> fmt::Display for Painted<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.2 {
            true => write!(f, "{}{}{RESET}", self.0, self.1),
            false => write!(f, "{}", self.1),
        }
    }
}
