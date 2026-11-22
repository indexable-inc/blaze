//! Whether a standard stream of the process is a terminal, and its size in cells: what
//! [`crate::out::Out::is_terminal`] and [`crate::out::Out::size`] answer in a native world, read
//! once per stream at the first ask (the native backend's `tty`; the world keeps it).
//!
//! - macOS: `isatty` and `ioctl(TIOCGWINSZ)` through blaze-sys, which finds both at their first
//!   call (`blaze_sys::macos::cold`), so a program that never asks binds neither.
//! - Linux: `ioctl(TCGETS)` and `ioctl(TIOCGWINSZ)` as raw system calls (blaze-sys's `linux`).
//! - Other targets: no reader is built; a stream there reads as not a terminal, which the
//!   reader names in its doc rather than guessing a size.
//!
//! A terminal whose size the kernel will not say (`TIOCGWINSZ` refused) is a terminal of unknown
//! size, [`Tty::Terminal`]`(None)`; a terminal that says 0 by 0 (a serial line nobody sized) is
//! the same.

use crate::{
    out::Stream,
    term::{Size, Tty},
};

/// The facts of `stream` (descriptor 1 or 2) as the kernel says them now.
pub(crate) fn read(stream: Stream) -> Tty {
    let fd = match stream {
        Stream::Out => 1,
        Stream::Err => 2,
    };
    read_fd(fd)
}

/// The facts of the descriptor numbered `fd`.
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(crate) fn read_fd(fd: i32) -> Tty {
    #[cfg(target_os = "linux")]
    use blaze_sys::linux::{is_terminal, window_size};
    #[cfg(target_os = "macos")]
    use blaze_sys::macos::{is_terminal, window_size};
    let fd = blaze_sys::Fd::new(fd);
    if !is_terminal(fd) {
        return Tty::NotTerminal;
    }
    match window_size(fd) {
        Ok(size) if size.cols > 0 && size.rows > 0 => Tty::Terminal(Some(Size { cols: size.cols, rows: size.rows })),
        Ok(_) | Err(_) => Tty::Terminal(None),
    }
}

/// No reader is built on this target: every stream reads as not a terminal.
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub(crate) fn read_fd(_fd: i32) -> Tty {
    Tty::NotTerminal
}

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
mod tests {
    use super::*;

    /// A pipe's end reads as not a terminal (the real check, on this machine's kernel).
    #[test]
    fn a_pipe_reads_as_not_a_terminal() {
        let (reader, writer) = std::io::pipe().expect("a pipe");
        use std::os::fd::AsRawFd as _;
        assert_eq!(read_fd(reader.as_raw_fd()), Tty::NotTerminal);
        assert_eq!(read_fd(writer.as_raw_fd()), Tty::NotTerminal);
    }
}
