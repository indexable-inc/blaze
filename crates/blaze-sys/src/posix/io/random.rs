//! The kernel's randomness: `getrandom` on Linux, `getentropy` on both systems.

use crate::Errno;

/// replaces libc::getrandom (and its `GRND_NONBLOCK` as `RandomFlags::NONBLOCK`). Linux:
/// [`crate::linux::getrandom`].
/// replaces libc::GRND_NONBLOCK (blaze_sys::posix::io::RandomFlags::NONBLOCK; Linux only)
#[cfg(target_os = "linux")]
pub use crate::linux::{RandomFlags, getrandom};

/// The most bytes one [`getentropy`] fills (`GETENTROPY_MAX`, 256 on both systems).
pub const GETENTROPY_MAX: usize = 256;

/// `getentropy`: `into` filled whole from the kernel's generator, waiting for its seed at
/// boot; [`Errno::Inval`] for more than [`GETENTROPY_MAX`] bytes, and a short fill (which the
/// kernel does not make below that) is [`Errno::Io`], never a second call. Linux: one
/// `getrandom`; macOS: libSystem's `getentropy`. replaces libc::getentropy.
#[blaze::noalloc]
#[blaze::nopanic]
pub fn getentropy(into: &mut [u8]) -> Result<(), Errno> {
    if into.len() > GETENTROPY_MAX {
        return Err(Errno::Inval);
    }
    #[cfg(target_os = "linux")]
    {
        let wanted = into.len();
        let got = crate::linux::getrandom(into, RandomFlags::NONE)?;
        if got != wanted {
            return Err(Errno::Io);
        }
        Ok(())
    }
    #[cfg(target_os = "macos")]
    {
        // SAFETY: `into` is live memory of `into.len()` bytes (256 at most) the call fills.
        super::said(unsafe { crate::macos::ffi::getentropy(into.as_mut_ptr().cast(), into.len()) }).map(|_| ())
    }
}
