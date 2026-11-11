//! The file effects' calls on Linux, on `blaze-sys`: raw system calls, no `libc` and no
//! `std::fs` (docs/v2/plan/design/sys.md, "Migration": the first rows off the ban list).
//!
//! Each function here is one effect's work behind the boundary it had (`fs::truncate`,
//! `fs::write_at`, `fs::read`, `fs::append`, `fs::sync`), so the `threads` driver and what the
//! io_uring driver does where it is submitted call the same names as before. A path is still a
//! whole path from the driver's root (`CWD`); the opens beneath a handle come with
//! `beneath.rs`.
//!
//! What a kernel may do short of what was asked is continued here, with a bound: a short read
//! or write goes on from where it stopped (each pass moves at least one byte, so the pass
//! count is bounded by the length), and an interrupted call is made again up to
//! [`INTERRUPTS_MOST`] times and then is the error.
//!
//! The calls are behind [`Calls`] so a test's kernel can write short and interrupt; [`Kernel`]
//! is the real one. The two planted bugs ([`Plant`]) are the controls of the tests below.

use std::{io, path::Path};

use blaze_sys::{
    Errno, Fd,
    linux::{self as sys, OpenFlags, OwnedFd},
};

/// How many times in a row one call may be interrupted by a signal before the interruption
/// is the effect's error. A signal handler runs in microseconds; 64 in a row is a storm.
pub(crate) const INTERRUPTS_MOST: u32 = 64;

/// A bug planted for a test's control.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Plant {
    None,
    /// A write the kernel cut short is reported done.
    #[cfg(test)]
    ShortWriteNotContinued,
    /// `ENOENT` is reported as `EACCES`: "not there" reads as "not allowed".
    #[cfg(test)]
    WrongErrno,
}

/// The typed errno as the `io::Error` the drivers carry: the same number, so
/// `io::Error::kind` and `raw_os_error` say what they said when libc set `errno`.
pub(crate) fn os_error(errno: Errno, plant: Plant) -> io::Error {
    let errno = match (plant, errno) {
        #[cfg(test)]
        (Plant::WrongErrno, Errno::NoEnt) => Errno::Acces,
        _ => errno,
    };
    io::Error::from_raw_os_error(i32::from(errno.raw()))
}

/// The reads and writes of a file effect.
pub(crate) trait Calls {
    fn pread(&self, fd: Fd, into: &mut [u8], at: u64) -> Result<usize, Errno>;
    fn pwrite(&self, fd: Fd, bytes: &[u8], at: u64) -> Result<usize, Errno>;
    fn write(&self, fd: Fd, bytes: &[u8]) -> Result<usize, Errno>;
}

/// The real kernel's calls.
pub(crate) struct Kernel;

impl Calls for Kernel {
    fn pread(&self, fd: Fd, into: &mut [u8], at: u64) -> Result<usize, Errno> {
        sys::pread(fd, into, at)
    }

    fn pwrite(&self, fd: Fd, bytes: &[u8], at: u64) -> Result<usize, Errno> {
        sys::pwrite(fd, bytes, at)
    }

    fn write(&self, fd: Fd, bytes: &[u8]) -> Result<usize, Errno> {
        sys::write(fd, bytes)
    }
}

/// `path` opened with `flags` (and close-on-exec), a file it creates given `0o666` less the
/// umask, as `std::fs::OpenOptions` gave it.
fn open(path: &Path, flags: OpenFlags, plant: Plant) -> io::Result<OwnedFd> {
    let flags = flags.with(OpenFlags::CLOSE_ON_EXEC);
    super::with_c_path(path, |path| {
        let path = blaze_sys::Path::new(path);
        // Bounded: one more open per interruption, at most `INTERRUPTS_MOST`.
        let mut interrupted = 0;
        loop {
            match sys::openat(sys::CWD, path, flags, 0o666) {
                Err(Errno::Intr) if interrupted < INTERRUPTS_MOST => interrupted += 1,
                opened => return opened.map_err(|errno| os_error(errno, plant)),
            }
        }
    })
}

/// One call until it is not interrupted, at most [`INTERRUPTS_MOST`] times in a row.
fn uninterrupted(mut call: impl FnMut() -> Result<usize, Errno>) -> Result<usize, Errno> {
    let mut interrupted = 0;
    loop {
        match call() {
            Err(Errno::Intr) if interrupted < INTERRUPTS_MOST => interrupted += 1,
            done => return done,
        }
    }
}

/// Up to `into.len()` bytes of `fd` from `at`: how many were read, fewer only at the file's
/// end. A short read is continued.
pub(crate) fn read_full(
    calls: &impl Calls,
    fd: Fd,
    into: &mut [u8],
    at: u64,
) -> Result<usize, Errno> {
    let mut read = 0;
    // Bounded: every pass reads at least one byte or ends the loop.
    while let Some(left) = into.get_mut(read..).filter(|left| !left.is_empty()) {
        match uninterrupted(|| calls.pread(fd, left, at + read as u64))? {
            0 => break,
            n => read += n,
        }
    }
    Ok(read)
}

/// All of `bytes` written to `fd`: at `at`, or for `None` at the descriptor's position (its
/// end, for an append). A short write is continued; a write of nothing is `WriteZero`.
pub(crate) fn write_all(
    calls: &impl Calls,
    fd: Fd,
    bytes: &[u8],
    at: Option<u64>,
    plant: Plant,
) -> io::Result<()> {
    let mut written = 0;
    // Bounded: every pass writes at least one byte or ends the loop.
    while let Some(left) = bytes.get(written..).filter(|left| !left.is_empty()) {
        let wrote = uninterrupted(|| match at {
            Some(at) => calls.pwrite(fd, left, at + written as u64),
            None => calls.write(fd, left),
        })
        .map_err(|errno| os_error(errno, plant))?;
        if wrote == 0 {
            return Err(io::Error::from(io::ErrorKind::WriteZero));
        }
        written += wrote;
        #[cfg(test)]
        if plant == Plant::ShortWriteNotContinued {
            return Ok(());
        }
    }
    Ok(())
}

/// [`crate::Effect::Read`]: up to `len` bytes of the file at `path` from `at`.
pub(crate) fn read(path: &Path, at: u64, len: usize) -> io::Result<Vec<u8>> {
    let file = open(path, OpenFlags::READ, Plant::None)?;
    // alloc: the read's payload.
    let mut bytes = vec![0; len];
    let read = read_full(&Kernel, file.fd(), &mut bytes, at)
        .map_err(|errno| os_error(errno, Plant::None))?;
    bytes.truncate(read);
    Ok(bytes)
}

/// [`crate::Effect::Append`]: `bytes` at the end of the file at `path`, made with its
/// directories where it is missing.
pub(crate) fn append(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let flags = OpenFlags::WRITE | OpenFlags::CREATE | OpenFlags::APPEND;
    let file = super::making_parent(path, || open(path, flags, Plant::None))?;
    write_all(&Kernel, file.fd(), bytes, None, Plant::None)
}

/// [`crate::Effect::Sync`]: the file at `path`, made where it is missing, synced to the
/// device.
pub(crate) fn sync(path: &Path) -> io::Result<()> {
    let flags = OpenFlags::WRITE | OpenFlags::CREATE | OpenFlags::APPEND;
    let file = open(path, flags, Plant::None)?;
    // durable: the effect is the sync its caller asked for.
    sys::fsync(file.fd()).map_err(|errno| os_error(errno, Plant::None))
}

/// [`crate::Effect::Truncate`]: as [`super::truncate`], whose error message wraps this one.
pub(crate) fn truncate(path: &Path, len: u64) -> io::Result<()> {
    let flags = OpenFlags::WRITE | OpenFlags::CREATE;
    let file = super::making_parent(path, || open(path, flags, Plant::None))?;
    sys::ftruncate(file.fd(), len).map_err(|errno| os_error(errno, Plant::None))
}

/// [`crate::Effect::WriteAt`]: `bytes` at `at` of the file at `path`, which exists.
pub(crate) fn write_at(path: &Path, at: u64, bytes: &[u8]) -> io::Result<()> {
    let file = open(path, OpenFlags::WRITE, Plant::None)?;
    write_all(&Kernel, file.fd(), bytes, Some(at), Plant::None)
}

#[cfg(test)]
mod tests {
    //! The moved calls on the real kernel against `std::fs` as the oracle, under a kernel
    //! that writes short and interrupts, and the two planted bugs each caught.

    use std::cell::Cell;

    use super::*;

    fn dir(what: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("blaze-core-fs-linux-{what}-{}", std::process::id()));
        _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// The real kernel, made to move at most `most` bytes a call and to interrupt every
    /// `every`th call.
    struct Stingy {
        most: usize,
        every: u32,
        calls: Cell<u32>,
    }

    impl Stingy {
        fn interrupts(&self) -> bool {
            self.calls.set(self.calls.get() + 1);
            self.every != 0 && self.calls.get() % self.every == 0
        }
    }

    impl Calls for Stingy {
        fn pread(&self, fd: Fd, into: &mut [u8], at: u64) -> Result<usize, Errno> {
            if self.interrupts() {
                return Err(Errno::Intr);
            }
            let most = into.len().min(self.most);
            Kernel.pread(fd, &mut into[..most], at)
        }

        fn pwrite(&self, fd: Fd, bytes: &[u8], at: u64) -> Result<usize, Errno> {
            if self.interrupts() {
                return Err(Errno::Intr);
            }
            Kernel.pwrite(fd, &bytes[..bytes.len().min(self.most)], at)
        }

        fn write(&self, fd: Fd, bytes: &[u8]) -> Result<usize, Errno> {
            if self.interrupts() {
                return Err(Errno::Intr);
            }
            Kernel.write(fd, &bytes[..bytes.len().min(self.most)])
        }
    }

    /// The invariant: after a write effect that reported done, the file holds every byte
    /// asked for, where it was asked for. The oracle is `std::fs::read`.
    fn wrote_all(path: &Path, expected: &[u8]) -> Result<(), String> {
        let found = std::fs::read(path).map_err(|error| format!("{}: {error}", path.display()))?;
        match found == expected {
            true => Ok(()),
            false => Err(format!(
                "{}: holds {} bytes ({:?}..), and {} were written",
                path.display(),
                found.len(),
                &found[..found.len().min(8)],
                expected.len()
            )),
        }
    }

    /// Writes `bytes` at 0 of a new file in `what`'s directory through a kernel that writes
    /// three bytes a call and interrupts every other call, with `plant`.
    fn short_write(what: &str, bytes: &[u8], plant: Plant) -> Result<(), String> {
        let path = dir(what).join("file");
        std::fs::write(&path, b"").unwrap();
        let file = open(&path, OpenFlags::WRITE, Plant::None).unwrap();
        let stingy = Stingy { most: 3, every: 2, calls: Cell::new(0) };
        write_all(&stingy, file.fd(), bytes, Some(0), plant).map_err(|error| error.to_string())?;
        wrote_all(&path, bytes)
    }

    #[test]
    fn a_short_write_is_continued_to_its_end() {
        let bytes: Vec<u8> = (0..=255).cycle().take(1000).collect();
        short_write("short-write", &bytes, Plant::None).unwrap();
    }

    #[test]
    fn the_oracle_catches_a_short_write_not_continued() {
        let bytes: Vec<u8> = (0..=255).cycle().take(1000).collect();
        let caught = short_write("short-write-planted", &bytes, Plant::ShortWriteNotContinued);
        let why = caught.expect_err("a write cut short at 3 bytes of 1000 was reported whole");
        assert!(why.contains("holds 3 bytes"), "{why}");
    }

    #[test]
    fn a_short_read_is_continued_to_the_files_end() {
        let path = dir("short-read").join("file");
        let bytes: Vec<u8> = (0..=255).cycle().take(700).collect();
        std::fs::write(&path, &bytes).unwrap();
        let file = open(&path, OpenFlags::READ, Plant::None).unwrap();
        let stingy = Stingy { most: 7, every: 3, calls: Cell::new(0) };
        let mut into = vec![0; 1000];
        assert_eq!(read_full(&stingy, file.fd(), &mut into, 100), Ok(600));
        assert_eq!(&into[..600], &bytes[100..]);
    }

    #[test]
    fn a_call_always_interrupted_ends_with_the_interruption() {
        let path = dir("storm").join("file");
        std::fs::write(&path, b"x").unwrap();
        let file = open(&path, OpenFlags::WRITE, Plant::None).unwrap();
        let storm = Stingy { most: 1, every: 1, calls: Cell::new(0) };
        let ended = write_all(&storm, file.fd(), b"abc", Some(0), Plant::None);
        assert_eq!(ended.unwrap_err().kind(), io::ErrorKind::Interrupted);
        assert_eq!(storm.calls.get(), INTERRUPTS_MOST + 1, "the bound, and one call past it");
    }

    /// The invariant: a file that is not there is `NotFound`, which is what makes a parent
    /// directory (`making_parent`) and what an empty `ReadAll` reads. The oracle is
    /// `std::fs`'s error for the same path.
    fn missing_is_not_found(what: &str, plant: Plant) -> Result<(), String> {
        let path = dir(what).join("no/such/file");
        let ours = open(&path, OpenFlags::READ, plant).expect_err("a missing file opened");
        let stds = std::fs::File::open(&path).expect_err("a missing file opened");
        match (ours.kind(), ours.raw_os_error()) == (stds.kind(), stds.raw_os_error()) {
            true => Ok(()),
            false => Err(format!("a missing file is {ours:?} here and {stds:?} by std")),
        }
    }

    #[test]
    fn an_errno_is_the_error_std_gives() {
        missing_is_not_found("errno", Plant::None).unwrap();
        // And the effects built on it: an append makes its directories, a write at an offset
        // of a missing file does not.
        let root = dir("errno-effects");
        append(&root.join("a/b/c"), b"made").unwrap();
        assert_eq!(std::fs::read(root.join("a/b/c")).unwrap(), b"made");
        let missing = write_at(&root.join("missing"), 0, b"x").unwrap_err();
        assert_eq!(missing.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn the_oracle_catches_an_errno_mapped_to_the_wrong_error() {
        let caught = missing_is_not_found("errno-planted", Plant::WrongErrno);
        let why = caught.expect_err("ENOENT reported as EACCES passed for NotFound");
        assert!(why.contains("PermissionDenied"), "{why}");
    }

    /// Each moved effect leaves what `std::fs` reads, and reads what `std::fs` wrote.
    #[test]
    fn the_moved_effects_agree_with_std() {
        let root = dir("agree");
        let path = root.join("deep/er/file");
        truncate(&path, 4).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), [0; 4]);
        write_at(&path, 2, b"xyz").unwrap();
        append(&path, b"!").unwrap();
        sync(&path).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"\0\0xyz!");
        assert_eq!(read(&path, 2, 3).unwrap(), b"xyz");
        assert_eq!(read(&path, 2, 100).unwrap(), b"xyz!");
        assert_eq!(read(&path, 100, 4).unwrap(), b"");
        truncate(&path, 1).unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 1);
        sync(&root.join("made-by-sync")).unwrap();
        assert_eq!(std::fs::metadata(root.join("made-by-sync")).unwrap().len(), 0);
        assert_eq!(read(&root.join("none"), 0, 1).unwrap_err().kind(), io::ErrorKind::NotFound);
    }
}
