//! The one writer of a command's stdout and stderr: [`outln!`](crate::outln),
//! [`out!`](crate::out), [`errln!`](crate::errln) and [`err!`](crate::err), in place of std's
//! `println!`, `print!`, `eprintln!` and `eprint!`.
//!
//! std's macros panic when the stream is closed (`blaze --version | head -1`: "failed printing
//! to stdout: Broken pipe", with a backtrace): every one of them in a command's path is that
//! bug waiting for a reader that stops early. This writer ends the process quietly when its
//! reader is gone, with the status a shell gives a program ended by SIGPIPE
//! ([`CLOSED`], 128 + 13), after what the program asked to be done first ([`before_end`]: the
//! terminal put back). Any other failure of the stream is said as std says it, by a panic
//! naming the stream and the error.
//!
//! `tests/prints.rs` holds each crate to the number of std's macros it has left
//! (`tests/prints.txt`), which only goes down.

use std::{
    fmt,
    io::{self, Write as _},
    sync::OnceLock,
};

/// The exit status of a command whose reader closed the pipe: what a shell reports of a
/// program ended by SIGPIPE.
pub const CLOSED: i32 = 141;

/// Which stream.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Stream {
    Out,
    Err,
}

static BEFORE_END: OnceLock<fn()> = OnceLock::new();

/// `restore` is called before the process is ended for a closed stream: what puts the terminal
/// back. The first call's stands.
pub fn before_end(restore: fn()) {
    let _ = BEFORE_END.set(restore);
}

/// What writing `what` to a stream came to, as this writer acts on it.
#[derive(Debug, PartialEq, Eq)]
pub enum Wrote {
    Done,
    /// The reader is gone: the process ends with [`CLOSED`].
    Closed,
    /// Any other failure, with the error's words.
    Failed(String),
}

/// `what` written to `to`: the decision of [`say`], apart from the process's own streams so a
/// test hands it a stream that fails.
pub fn write(to: &mut dyn io::Write, what: fmt::Arguments<'_>) -> Wrote {
    match to.write_fmt(what) {
        Ok(()) => Wrote::Done,
        Err(error) if error.kind() == io::ErrorKind::BrokenPipe => Wrote::Closed,
        Err(error) => Wrote::Failed(error.to_string()), // alloc: once, as the command ends
    }
}

/// `what` written to the process's `stream`. Does not return when its reader is gone.
pub fn say(stream: Stream, what: fmt::Arguments<'_>) {
    let wrote = match stream {
        Stream::Out => write(&mut io::stdout().lock(), what),
        Stream::Err => write(&mut io::stderr().lock(), what),
    };
    match wrote {
        Wrote::Done => {}
        Wrote::Closed => {
            if let Some(restore) = BEFORE_END.get() {
                restore();
            }
            // Nothing more is said: there is no one to read it.
            std::process::exit(CLOSED);
        }
        Wrote::Failed(why) => panic!("failed printing to std{}: {why}", match stream {
            Stream::Out => "out",
            Stream::Err => "err",
        }),
    }
}

/// `println!` through [`say`].
#[macro_export]
macro_rules! outln {
    () => { $crate::out::say($crate::out::Stream::Out, ::core::format_args!("\n")) };
    ($($arg:tt)*) => {
        $crate::out::say($crate::out::Stream::Out, ::core::format_args!("{}\n", ::core::format_args!($($arg)*)))
    };
}

/// `print!` through [`say`].
#[macro_export]
macro_rules! out {
    ($($arg:tt)*) => { $crate::out::say($crate::out::Stream::Out, ::core::format_args!($($arg)*)) };
}

/// `eprintln!` through [`say`].
#[macro_export]
macro_rules! errln {
    () => { $crate::out::say($crate::out::Stream::Err, ::core::format_args!("\n")) };
    ($($arg:tt)*) => {
        $crate::out::say($crate::out::Stream::Err, ::core::format_args!("{}\n", ::core::format_args!($($arg)*)))
    };
}

/// `eprint!` through [`say`].
#[macro_export]
macro_rules! err {
    ($($arg:tt)*) => { $crate::out::say($crate::out::Stream::Err, ::core::format_args!($($arg)*)) };
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fails(io::ErrorKind);

    impl io::Write for Fails {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            Err(self.0.into())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// A reader that is gone ends the command quietly; any other failure is said.
    #[test]
    fn a_closed_stream_is_told_from_one_that_failed() {
        let mut kept = Vec::new();
        assert_eq!(write(&mut kept, format_args!("{}\n", format_args!("a {}", 1))), Wrote::Done);
        assert_eq!(kept, b"a 1\n");
        assert_eq!(write(&mut Fails(io::ErrorKind::BrokenPipe), format_args!("a")), Wrote::Closed);
        assert!(matches!(
            write(&mut Fails(io::ErrorKind::StorageFull), format_args!("a")),
            Wrote::Failed(_)
        ));
    }
}
