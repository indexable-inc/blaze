//! What the WASI preview 1 functions std for `wasm32-wasip1` calls do for a macro: each one the
//! machine a macro on the evaluator sees ([`Os`]), traced the same. Their arguments and memory
//! layout are `blaze_wasi`'s, the one implementation for every pointer width; this file is the
//! macro sandbox's [`System`] behind them.
//!
//! - The environment is the calling crate's, sorted by name. The module's `getenv` is the
//!   glue's, which asks the bridge's `env_var` one name at a time, so `env::var` keys an
//!   expansion by that variable alone. `environ_get`, which only `env::vars` reaches, gives it
//!   whole and keys it by all of them (`ALL_ENV`).
//! - The realtime clock is real and untraceable, as on the evaluator. The monotonic and CPU
//!   clocks count the fuel the instance spent, a nanosecond a unit: deterministic, so a macro
//!   that times itself is still kept.
//! - Randomness is real and untraceable.
//! - Writes to stdout and stderr are taken and dropped, as the evaluator drops them (its
//!   `MirOutput`, which nothing reads). This differs from rustc, where a macro's `println!`
//!   and `eprintln!` reach rustc's own stdout and stderr, the terminal: a gap, not rustc's
//!   behavior. Showing them needs the bytes carried out with the expansion and shown again
//!   when a kept expansion is reused.
//! - `/` is the one preopened directory, so every path the macro names reaches the host (wasi-libc
//!   hands it over relative to `/`) instead of failing inside libc untraced. Each is served by
//!   the evaluator's own files ([`Os`]'s `os/files.rs`): walked on the real file system, read
//!   below the calling crate's `CARGO_MANIFEST_DIR` and `OUT_DIR` only, noted so the expansion
//!   is keyed by what it read, and a span each. A path outside them stops the expansion, naming
//!   it; a write stops it too. A file the macro opens is descriptor [`ROOT_FD`] + 1 and up.
//!   wasi-libc makes a relative path absolute against its own working directory before the
//!   host sees it, so the glue enters the directory cargo runs rustc in (`Os::current_dir`,
//!   through the bridge's `current_dir`) before its macro runs: a relative path then starts
//!   where it starts under rustc. `chdir`'s look at that directory is answered as a directory
//!   even when it is outside the roots (a workspace's root); what is in it is not.
//! - A wait (`poll_oneoff`, which `thread::sleep` makes) or an exit stops the expansion.
//!
//! [`Os`]: crate::os::Os

use blaze_wasi::{Answer, Dirent, Errno, Filestat, Filetype, Open, ROOT_FD, System};

use super::host::{Cx, FUEL};
use crate::{
    guest::{Guest, Stop},
    os::{
        At, FIRST_FD,
        target::{self, Kind, Stat},
    },
};

/// The WASI `errno` of what a call of [`Os`]'s files failed with.
///
/// [`Os`]: crate::os::Os
fn wasi_errno(errno: target::Errno) -> Errno {
    match errno {
        target::Errno::Noent => Errno::NOENT,
        target::Errno::Io => Errno::IO,
        target::Errno::Badf => Errno::BADF,
        target::Errno::Acces => Errno::ACCES,
        target::Errno::Fault => Errno::FAULT,
        target::Errno::Notdir => Errno::NOTDIR,
        target::Errno::Isdir => Errno::ISDIR,
        target::Errno::Inval => Errno::INVAL,
        target::Errno::Spipe => Errno::SPIPE,
        target::Errno::Rofs => Errno::ROFS,
        target::Errno::Range => Errno::RANGE,
        target::Errno::Nametoolong => Errno::NAMETOOLONG,
        target::Errno::Loop => Errno::LOOP,
    }
}

/// [`Os`]'s descriptor of the WASI descriptor `fd`, past [`ROOT_FD`]: its first, [`FIRST_FD`],
/// is the one after `/`.
///
/// [`Os`]: crate::os::Os
fn os_fd(fd: u32) -> i64 {
    i64::from(fd) - i64::from(ROOT_FD) - 1 + FIRST_FD
}

/// The WASI descriptor of [`Os`]'s `fd`.
///
/// [`Os`]: crate::os::Os
fn wasi_fd(fd: i64) -> Result<u32, Stop> {
    // alloc: error path
    u32::try_from(fd - FIRST_FD + i64::from(ROOT_FD) + 1)
        .map_err(|_| Stop(format!("opened descriptor {fd}, past WASI's")))
}

/// WASI's `filetype`.
fn filetype(kind: Kind) -> Filetype {
    match kind {
        Kind::Dir => Filetype::Directory,
        Kind::File => Filetype::RegularFile,
        Kind::Link => Filetype::SymbolicLink,
        Kind::Other => Filetype::Unknown,
    }
}

/// WASI's `filestat` of `stat`: no times, owner or permissions, as [`Stat`] has none.
fn filestat(stat: &Stat) -> Filestat {
    Filestat {
        dev: target::DEV,
        ino: stat.ino,
        filetype: filetype(stat.kind),
        nlink: 1,
        size: stat.size,
        atim: 0,
        mtim: 0,
        ctim: 0,
    }
}

/// The path the macro named: one that is not UTF-8 stops the expansion, as the trace names
/// paths by UTF-8.
fn path_at(bytes: &[u8]) -> Result<&str, Stop> {
    std::str::from_utf8(bytes).map_err(|_| {
        // alloc: error path
        Stop(format!(
            "names the path {:?}, which is not UTF-8, and blaze traces what a proc macro reads by UTF-8 paths",
            String::from_utf8_lossy(bytes)
        ))
    })
}

/// Where a path the macro named from `fd` starts: `/` or a directory it opened, or `EBADF`.
fn at_of(fd: u32) -> Result<At, Errno> {
    match fd {
        ROOT_FD => Ok(At::Root),
        0..ROOT_FD => Err(Errno::BADF),
        _ => Ok(At::Fd(os_fd(fd))),
    }
}

/// The `len` bytes at `ptr` of a module's memory, as the bridge's shims pass them.
pub(super) fn bytes(mem: &[u8], ptr: u32, len: u32) -> Result<&[u8], Stop> {
    blaze_wasi::bytes(mem, u64::from(ptr), u64::from(len))
}

/// Writes `bytes` at `ptr` of a module's memory, as the bridge's shims pass it.
pub(super) fn put(mem: &mut [u8], ptr: u32, bytes: &[u8]) -> Result<(), Stop> {
    blaze_wasi::put(mem, u64::from(ptr), bytes)
}

/// A module's memory as the evaluator's shims reach a guest: addresses are its offsets.
pub(super) struct Linear<'m>(pub(super) &'m mut [u8]);

impl Guest for Linear<'_> {
    fn read(&self, addr: u64, len: usize) -> Result<&[u8], Stop> {
        blaze_wasi::bytes(self.0, addr, len as u64)
    }

    fn write(&mut self, addr: u64, bytes: &[u8]) -> Result<(), Stop> {
        blaze_wasi::put(self.0, addr, bytes)
    }

    /// WASI returns its `errno`; nothing sets one.
    fn set_errno(&mut self, code: i32) -> Result<(), Stop> {
        Err(Stop(format!("reached a shim that sets errno {code}, which WASI returns instead"))) // alloc: error path
    }

    fn alloc(&mut self, bytes: &[u8], _align: usize) -> Result<u64, Stop> {
        // alloc: error path
        Err(Stop(format!(
            "reached a shim that allocates {} bytes in its memory, which the wasm host does not",
            bytes.len()
        )))
    }
}

impl System for Cx<'_> {
    /// A macro is given no arguments.
    fn args(&mut self) -> Result<(&[u8], usize), Stop> {
        Ok((&[], 0))
    }

    /// `NAME=value\0` of each of the calling crate's variables, sorted by name, the whole
    /// environment noted read.
    fn environ(&mut self) -> Result<(&[u8], usize), Stop> {
        let vars = self.os.walk_environ();
        self.scratch.clear();
        for (name, value) in &vars {
            self.scratch.extend_from_slice(name.as_bytes());
            self.scratch.push(b'=');
            self.scratch.extend_from_slice(value.as_bytes());
            self.scratch.push(0);
        }
        Ok((&self.scratch, vars.len()))
    }

    fn clock_res(&mut self, id: u32) -> Answer<u64> {
        Ok(if id > 3 { Err(Errno::INVAL) } else { Ok(1) })
    }

    /// The realtime clock is real; the others count the fuel the instance spent.
    fn clock_time(&mut self, id: u32, fuel_left: Option<u64>) -> Answer<u64> {
        Ok(Ok(match id {
            // REALTIME
            0 => self.os.realtime_ns(),
            // MONOTONIC, PROCESS_CPUTIME_ID, THREAD_CPUTIME_ID
            1..=3 => match fuel_left {
                Some(left) => FUEL.saturating_sub(left),
                None => {
                    return Err(Stop("read a clock on an engine that counts no fuel".to_owned()));
                } // alloc: error path
            },
            _ => return Ok(Err(Errno::INVAL)),
        }))
    }

    fn random(&mut self, into: &mut [u8]) -> Result<(), Stop> {
        let mut ret = [0; 8];
        let len = into.len() as u64;
        self.os.fill_random(&mut Linear(into), &mut ret, (0, len), Some(0))?;
        Ok(())
    }

    /// Stdout's and stderr's bytes, counted and dropped (rustc shows them; see the module's
    /// doc).
    fn write(&mut self, fd: u32, bytes: &[u8]) -> Answer<usize> {
        Ok(match fd {
            1 | 2 => Ok(bytes.len()),
            _ => Err(Errno::BADF),
        })
    }

    /// Stdin is empty; a file the macro opened is read from what [`Os`] read of it at its open.
    ///
    /// [`Os`]: crate::os::Os
    fn read(&mut self, fd: u32, into: &mut [u8]) -> Answer<usize> {
        Ok(match fd {
            0 => Ok(0),
            1 | 2 => Err(Errno::BADF),
            ROOT_FD => Err(Errno::ISDIR),
            _ => match self.os.read_fd(os_fd(fd), into.len() as u64, None) {
                Ok(bytes) => {
                    into[..bytes.len()].copy_from_slice(bytes);
                    Ok(bytes.len())
                }
                Err(errno) => Err(wasi_errno(errno)),
            },
        })
    }

    /// Stdio a character device, [`ROOT_FD`] and an opened directory a directory, an opened
    /// file a regular file.
    fn fd_filetype(&mut self, fd: u32) -> Answer<Filetype> {
        Ok(match fd {
            0..=2 => Ok(Filetype::CharacterDevice),
            ROOT_FD => Ok(Filetype::Directory),
            _ => self.os.stat_fd(os_fd(fd)).map(|stat| filetype(stat.kind)).map_err(wasi_errno),
        })
    }

    fn set_fd_flags(&mut self, fd: u32, _flags: u32) -> Answer<()> {
        Ok(Err(match fd {
            0..=ROOT_FD => Errno::NOTSUP,
            _ if self.os.stat_fd(os_fd(fd)).is_ok() => Errno::NOTSUP,
            _ => Errno::BADF,
        }))
    }

    fn close(&mut self, fd: u32) -> Answer<()> {
        match fd {
            0..=2 => Ok(Ok(())),
            // alloc: error path
            ROOT_FD => Err(Stop("closes `/`, its one preopened directory".to_owned())),
            _ => Ok(self.os.close_fd(os_fd(fd)).map_err(wasi_errno)),
        }
    }

    /// Stdio does not seek; a file the macro opened does.
    fn seek(&mut self, fd: u32, offset: i64, whence: u32) -> Answer<u64> {
        Ok(match fd {
            0..=2 => Err(Errno::SPIPE),
            ROOT_FD => Err(Errno::BADF),
            _ => self.os.seek_fd(os_fd(fd), offset, i64::from(whence)).map_err(wasi_errno),
        })
    }

    /// What a file or directory the macro opened is. A look at stdio or `/` itself stops the
    /// expansion: neither is below what it may read.
    fn fd_stat(&mut self, fd: u32) -> Answer<Filestat> {
        match fd {
            // alloc: error path
            0..=2 => Err(Stop(format!(
                "looks at its standard stream {fd} (`fd_filestat_get`), which the wasm host does not serve"
            ))),
            ROOT_FD => Err(self.os.refuse_root()),
            _ => Ok(self.os.stat_fd(os_fd(fd)).map(|stat| filestat(&stat)).map_err(wasi_errno)),
        }
    }

    /// The file or directory `path` from `at` names, opened to read. One opened to create,
    /// truncate, append or write (the right wasi-libc asks for `O_WRONLY` and `O_RDWR`) stops
    /// the expansion.
    fn open(&mut self, at: u32, path: &[u8], how: Open) -> Answer<u32> {
        let path = path_at(path)?;
        if how.create() || how.exclusive() || how.truncate() || how.write() || how.append() {
            // alloc: error path
            return Err(Stop(format!("opens `{path}` to write, which a proc macro may not")));
        }
        let at = match at_of(at) {
            Ok(at) => at,
            Err(errno) => return Ok(Err(errno)),
        };
        match self.os.open_path((at, path), how.follow(), how.directory())? {
            Ok(opened) => Ok(Ok(wasi_fd(opened)?)),
            Err(errno) => Ok(Err(wasi_errno(errno))),
        }
    }

    /// A look at what `path` from `at` names.
    fn path_stat(&mut self, at: u32, path: &[u8], follow: bool) -> Answer<Filestat> {
        let path = path_at(path)?;
        let at = match at_of(at) {
            Ok(at) => at,
            Err(errno) => return Ok(Err(errno)),
        };
        Ok(self.os.look((at, path), follow)?.map(|stat| filestat(&stat)).map_err(wasi_errno))
    }

    /// The target of the link `path` from `at` names.
    fn readlink(&mut self, at: u32, path: &[u8]) -> Answer<Vec<u8>> {
        let path = path_at(path)?;
        let at = match at_of(at) {
            Ok(at) => at,
            Err(errno) => return Ok(Err(errno)),
        };
        Ok(self
            .os
            .link_target((at, path))?
            .map(|target| target.into_os_string().into_encoded_bytes())
            .map_err(wasi_errno))
    }

    /// The entries of a directory the macro opened. Listing `/` stops the expansion.
    fn dir_entry(&mut self, fd: u32, cookie: u64) -> Answer<Option<Dirent<'_>>> {
        let fd = match fd {
            0..=2 => return Ok(Err(Errno::NOTDIR)),
            ROOT_FD => return Err(self.os.refuse_root()),
            _ => os_fd(fd),
        };
        Ok(match self.os.dir_entry(fd, cookie) {
            Ok(entry) => Ok(entry.map(|entry| Dirent {
                ino: entry.ino,
                filetype: filetype(entry.kind),
                name: entry.name,
            })),
            Err(errno) => Err(wasi_errno(errno)),
        })
    }

    fn poll_oneoff(&mut self) -> Stop {
        Stop(
            "waits (`poll_oneoff`, as `thread::sleep` does), which a macro's expansion may not"
                .to_owned(),
        ) // alloc: error path
    }

    fn exit(&mut self, code: u32) -> Stop {
        // alloc: error path
        Stop(format!(
            "exits the process with code {code} (`std::process::exit`), which a macro's expansion may not"
        ))
    }
}
