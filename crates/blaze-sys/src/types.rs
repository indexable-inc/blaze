//! The types every target's layer shares: a descriptor, a path view and a time.

use core::{ffi::CStr, time::Duration};

use crate::Errno;

/// A descriptor's number, borrowed: it closes nothing.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
#[repr(transparent)]
pub struct Fd(i32);

impl Fd {
    /// Standard error: the one descriptor the panic handler writes to (sys.md, "Runtime").
    pub const STDERR: Fd = Fd(2);

    /// The descriptor numbered `raw`.
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub const fn new(raw: i32) -> Fd {
        Fd(raw)
    }

    /// Its number.
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub const fn raw(self) -> i32 {
        self.0
    }
}

/// A path as the kernel takes it: bytes ending in a NUL, lent. Never allocated here; a caller
/// with bytes and no NUL makes one in a buffer of its own ([`Path::in_buffer`]).
#[derive(Clone, Copy, Debug)]
pub struct Path<'a>(&'a CStr);

impl<'a> Path<'a> {
    /// The empty path: with `AT_EMPTY_PATH`, the descriptor itself.
    pub const EMPTY: Path<'static> = Path(c"");

    /// `path`, as it is.
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub const fn new(path: &'a CStr) -> Path<'a> {
        Path(path)
    }

    /// `bytes` and a NUL written into `buffer`: [`Errno::NameTooLong`] where they do not fit,
    /// [`Errno::Inval`] where `bytes` holds a NUL.
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub fn in_buffer(bytes: &[u8], buffer: &'a mut [u8]) -> Result<Path<'a>, Errno> {
        let Some(room) = buffer.get_mut(..bytes.len().saturating_add(1)) else {
            return Err(Errno::NameTooLong);
        };
        let Some((end, name)) = room.split_last_mut() else {
            return Err(Errno::NameTooLong);
        };
        for (to, from) in name.iter_mut().zip(bytes) {
            *to = *from;
        }
        *end = 0;
        match CStr::from_bytes_with_nul(room) {
            Ok(path) => Ok(Path(path)),
            Err(_) => Err(Errno::Inval),
        }
    }

    /// The bytes, without the NUL.
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub const fn bytes(self) -> &'a [u8] {
        self.0.to_bytes()
    }

    /// The pointer the kernel reads to the NUL.
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub(crate) const fn as_ptr(self) -> *const u8 {
        self.0.as_ptr().cast()
    }
}

/// `struct __kernel_timespec`: seconds and nanoseconds, 64 bits each on every target blaze
/// runs on. A time on some clock, or a length of time; the call says which.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
#[repr(C)]
pub struct Timespec {
    /// Whole seconds.
    pub sec: i64,
    /// Nanoseconds, below one second.
    pub nsec: i64,
}

impl Timespec {
    /// Zero.
    pub const ZERO: Timespec = Timespec { sec: 0, nsec: 0 };

    /// `time` as a timespec; its seconds stop at `i64::MAX` (292 billion years).
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub const fn from_duration(time: Duration) -> Timespec {
        let secs = time.as_secs();
        let sec = if secs > i64::MAX as u64 { i64::MAX } else { secs as i64 };
        Timespec { sec, nsec: time.subsec_nanos() as i64 }
    }

    /// The timespec as a duration; `None` for a negative one or nanoseconds out of range.
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub const fn to_duration(self) -> Option<Duration> {
        if self.sec < 0 || self.nsec < 0 || self.nsec >= 1_000_000_000 {
            return None;
        }
        Some(Duration::new(self.sec as u64, self.nsec as u32))
    }
}
