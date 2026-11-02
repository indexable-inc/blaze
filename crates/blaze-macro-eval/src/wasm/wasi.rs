//! The WASI preview 1 functions std for `wasm32-wasip1` calls, answered for a macro over its
//! linear memory: each one the machine a macro on the evaluator sees ([`Os`]), traced the same.
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
//!   A relative path is resolved against `/`, as wasi-libc without `chdir` does, not against the
//!   package's directory as under rustc.
//! - A wait (`poll_oneoff`, which `thread::sleep` makes) or an exit stops the expansion.
//!
//! [`Os`]: crate::os::Os

use std::ops::Range;

use super::host::Cx;
use crate::{
    guest::{Guest, Stop},
    os::{
        At, FIRST_FD,
        target::{Errno, Kind, Stat},
    },
};

/// WASI's `errno` values std reads.
mod errno {
    pub(super) const SUCCESS: i32 = 0;
    pub(super) const ACCES: i32 = 2;
    pub(super) const BADF: i32 = 8;
    pub(super) const FAULT: i32 = 21;
    pub(super) const INVAL: i32 = 28;
    pub(super) const IO: i32 = 29;
    pub(super) const ISDIR: i32 = 31;
    pub(super) const LOOP: i32 = 32;
    pub(super) const NAMETOOLONG: i32 = 37;
    pub(super) const NOENT: i32 = 44;
    pub(super) const NOTDIR: i32 = 54;
    pub(super) const NOTSUP: i32 = 58;
    pub(super) const RANGE: i32 = 68;
    pub(super) const ROFS: i32 = 69;
    pub(super) const SPIPE: i32 = 70;
}

/// The WASI `errno` of what a call of [`Os`]'s files failed with.
///
/// [`Os`]: crate::os::Os
fn wasi_errno(errno: Errno) -> i32 {
    match errno {
        Errno::Noent => errno::NOENT,
        Errno::Io => errno::IO,
        Errno::Badf => errno::BADF,
        Errno::Acces => errno::ACCES,
        Errno::Fault => errno::FAULT,
        Errno::Notdir => errno::NOTDIR,
        Errno::Isdir => errno::ISDIR,
        Errno::Inval => errno::INVAL,
        Errno::Spipe => errno::SPIPE,
        Errno::Rofs => errno::ROFS,
        Errno::Range => errno::RANGE,
        Errno::Nametoolong => errno::NAMETOOLONG,
        Errno::Loop => errno::LOOP,
    }
}

/// The preopened directory `/`.
const ROOT_FD: u32 = 3;

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
    u32::try_from(fd - FIRST_FD + i64::from(ROOT_FD) + 1).map_err(|_| Stop(format!("opened descriptor {fd}, past WASI's")))
}

/// `path_open`'s `oflags` that create or truncate.
const OFLAGS_CREAT: u32 = 1;
const OFLAGS_DIRECTORY: u32 = 2;
const OFLAGS_EXCL: u32 = 4;
const OFLAGS_TRUNC: u32 = 8;
/// The right to write, which wasi-libc asks for a file opened `O_WRONLY` or `O_RDWR`.
const RIGHTS_FD_WRITE: u64 = 1 << 6;
/// `fdflags`' append.
const FDFLAGS_APPEND: u32 = 1;
/// The `lookupflags` bit that follows the last link.
const LOOKUP_SYMLINK_FOLLOW: u32 = 1;

/// WASI's `filetype`.
fn filetype(kind: Kind) -> u8 {
    match kind {
        Kind::Dir => 3,
        Kind::File => 4,
        Kind::Link => 7,
        Kind::Other => 0,
    }
}

/// The path of `len` bytes at `ptr`, which the macro named: one that is not UTF-8 stops the
/// expansion, as the trace names paths by UTF-8.
fn path_at(mem: &[u8], ptr: u32, len: u32) -> Result<&str, Stop> {
    let bytes = bytes(mem, ptr, len)?;
    std::str::from_utf8(bytes).map_err(|_| {
        // alloc: error path
        Stop(format!(
            "names the path {:?}, which is not UTF-8, and blaze traces what a proc macro reads by UTF-8 paths",
            String::from_utf8_lossy(bytes)
        ))
    })
}

/// Where a path the macro named from `fd` starts: `/` or a directory it opened, or `EBADF`.
fn at_of(fd: u32) -> Result<At, i32> {
    match fd {
        ROOT_FD => Ok(At::Root),
        0..ROOT_FD => Err(errno::BADF),
        _ => Ok(At::Fd(os_fd(fd))),
    }
}

/// The bytes `len` long at `ptr` of `mem`, or why they are not in it.
fn range(mem: &[u8], ptr: u32, len: u32) -> Result<Range<usize>, Stop> {
    let (start, len) = (ptr as usize, len as usize);
    match start.checked_add(len) {
        Some(end) if end <= mem.len() => Ok(start..end),
        _ => Err(Stop(format!(
            // alloc: error path
            "passed the host {len} bytes at {ptr:#x}, outside its memory of {} bytes",
            mem.len()
        ))),
    }
}

pub(super) fn bytes(mem: &[u8], ptr: u32, len: u32) -> Result<&[u8], Stop> {
    Ok(&mem[range(mem, ptr, len)?])
}

pub(super) fn put(mem: &mut [u8], ptr: u32, bytes: &[u8]) -> Result<(), Stop> {
    let at = range(mem, ptr, bytes.len() as u32)?;
    mem[at].copy_from_slice(bytes);
    Ok(())
}

fn put_u32(mem: &mut [u8], ptr: u32, value: u32) -> Result<(), Stop> {
    put(mem, ptr, &value.to_le_bytes())
}

fn put_u64(mem: &mut [u8], ptr: u32, value: u64) -> Result<(), Stop> {
    put(mem, ptr, &value.to_le_bytes())
}

fn read_u32(mem: &[u8], ptr: u32) -> Result<u32, Stop> {
    let mut word = [0; 4];
    word.copy_from_slice(bytes(mem, ptr, 4)?);
    Ok(u32::from_le_bytes(word))
}

/// A module's memory as the evaluator's shims reach a guest: addresses are its offsets.
pub(super) struct Linear<'m>(pub(super) &'m mut [u8]);

impl Guest for Linear<'_> {
    fn read(&self, addr: u64, len: usize) -> Result<&[u8], Stop> {
        let (addr, len) = narrow(addr, len)?;
        bytes(self.0, addr, len)
    }

    fn write(&mut self, addr: u64, bytes: &[u8]) -> Result<(), Stop> {
        let (addr, _) = narrow(addr, bytes.len())?;
        put(self.0, addr, bytes)
    }

    /// WASI returns its `errno`; nothing sets one.
    fn set_errno(&mut self, code: i32) -> Result<(), Stop> {
        Err(Stop(format!("reached a shim that sets errno {code}, which WASI returns instead"))) // alloc: error path
    }

    fn alloc(&mut self, bytes: &[u8], _align: usize) -> Result<u64, Stop> {
        // alloc: error path
        Err(Stop(format!("reached a shim that allocates {} bytes in its memory, which the wasm host does not", bytes.len())))
    }
}

/// `addr` and `len` of a 32-bit memory.
fn narrow(addr: u64, len: usize) -> Result<(u32, u32), Stop> {
    match (u32::try_from(addr), u32::try_from(len)) {
        (Ok(addr), Ok(len)) => Ok((addr, len)),
        // alloc: error path
        _ => Err(Stop(format!("passed the host {len} bytes at {addr:#x}, past a 32-bit memory"))),
    }
}

pub(super) fn args_sizes_get(mem: &mut [u8], count: u32, size: u32) -> Result<i32, Stop> {
    put_u32(mem, count, 0)?;
    put_u32(mem, size, 0)?;
    Ok(errno::SUCCESS)
}

pub(super) fn args_get(_mem: &mut [u8], _argv: u32, _buf: u32) -> Result<i32, Stop> {
    Ok(errno::SUCCESS)
}

/// `NAME=value\0` of each variable, in order, into `cx`'s buffer.
fn environ<'c>(cx: &'c mut Cx<'_>) -> (&'c [u8], usize) {
    let vars = cx.os.walk_environ();
    cx.scratch.clear();
    for (name, value) in &vars {
        cx.scratch.extend_from_slice(name.as_bytes());
        cx.scratch.push(b'=');
        cx.scratch.extend_from_slice(value.as_bytes());
        cx.scratch.push(0);
    }
    (&cx.scratch, vars.len())
}

pub(super) fn environ_sizes_get(mem: &mut [u8], cx: &mut Cx<'_>, count: u32, size: u32) -> Result<i32, Stop> {
    let (strings, n) = environ(cx);
    put_u32(mem, count, n as u32)?;
    put_u32(mem, size, strings.len() as u32)?;
    Ok(errno::SUCCESS)
}

pub(super) fn environ_get(mem: &mut [u8], cx: &mut Cx<'_>, pointers: u32, buf: u32) -> Result<i32, Stop> {
    let (strings, _) = environ(cx);
    put(mem, buf, strings)?;
    let mut at = 0;
    let mut start = 0;
    for (end, _) in strings.iter().enumerate().filter(|(_, b)| **b == 0) {
        put_u32(mem, pointers.wrapping_add(4 * at), buf.wrapping_add(start as u32))?;
        at += 1;
        start = end + 1;
    }
    Ok(errno::SUCCESS)
}

pub(super) fn clock_res_get(mem: &mut [u8], id: u32, out: u32) -> Result<i32, Stop> {
    if id > 3 {
        return Ok(errno::INVAL);
    }
    put_u64(mem, out, 1)?;
    Ok(errno::SUCCESS)
}

/// `clock_time_get(id, precision, out)`, with `fuel` the fuel the instance spent so far.
pub(super) fn clock_time_get(mem: &mut [u8], cx: &mut Cx<'_>, id: u32, out: u32, fuel: u64) -> Result<i32, Stop> {
    let ns = match id {
        // REALTIME
        0 => cx.os.realtime_ns(),
        // MONOTONIC, PROCESS_CPUTIME_ID, THREAD_CPUTIME_ID
        1..=3 => fuel,
        _ => return Ok(errno::INVAL),
    };
    put_u64(mem, out, ns)?;
    Ok(errno::SUCCESS)
}

pub(super) fn random_get(mem: &mut [u8], cx: &mut Cx<'_>, buf: u32, len: u32) -> Result<i32, Stop> {
    let mut ret = [0; 8];
    cx.os.fill_random(&mut Linear(mem), &mut ret, (u64::from(buf), u64::from(len)), Some(0))?;
    Ok(errno::SUCCESS)
}

/// `fd_write(fd, iovs, iovs_len, written)`: stdout's and stderr's bytes, counted and dropped
/// (rustc shows them; see the module's doc).
pub(super) fn fd_write(mem: &mut [u8], fd: u32, iovs: u32, count: u32, written: u32) -> Result<i32, Stop> {
    if !matches!(fd, 1 | 2) {
        return Ok(errno::BADF);
    }
    let mut total = 0u32;
    for n in 0..count {
        let len = read_u32(mem, iovs.wrapping_add(n.wrapping_mul(8)).wrapping_add(4))?;
        total = total.saturating_add(len);
    }
    put_u32(mem, written, total)?;
    Ok(errno::SUCCESS)
}

/// `fd_read(fd, iovs, iovs_len, read)`: stdin is empty; a file the macro opened is read from
/// what [`Os`] read of it at its open.
///
/// [`Os`]: crate::os::Os
pub(super) fn fd_read(mem: &mut [u8], cx: &mut Cx<'_>, fd: u32, iovs: u32, count: u32, read: u32) -> Result<i32, Stop> {
    match fd {
        0 => {
            put_u32(mem, read, 0)?;
            return Ok(errno::SUCCESS);
        }
        1 | 2 => return Ok(errno::BADF),
        ROOT_FD => return Ok(errno::ISDIR),
        _ => {}
    }
    let fd = os_fd(fd);
    let mut total = 0u32;
    for n in 0..count {
        let iov = iovs.wrapping_add(n.wrapping_mul(8));
        let (base, len) = (read_u32(mem, iov)?, read_u32(mem, iov.wrapping_add(4))?);
        let bytes = match cx.os.read_fd(fd, u64::from(len), None) {
            Ok(bytes) => bytes,
            Err(errno) => return Ok(wasi_errno(errno)),
        };
        put(mem, base, bytes)?;
        total = total.saturating_add(bytes.len() as u32);
        if (bytes.len() as u32) < len {
            break;
        }
    }
    put_u32(mem, read, total)?;
    Ok(errno::SUCCESS)
}

/// `fd_prestat_get(fd, out)`: `/` at [`ROOT_FD`], a directory whose name is one byte.
pub(super) fn fd_prestat_get(mem: &mut [u8], fd: u32, out: u32) -> Result<i32, Stop> {
    if fd != ROOT_FD {
        return Ok(errno::BADF);
    }
    put(mem, out, &[0, 0, 0, 0])?;
    put_u32(mem, out + 4, 1)?;
    Ok(errno::SUCCESS)
}

pub(super) fn fd_prestat_dir_name(mem: &mut [u8], fd: u32, path: u32, len: u32) -> Result<i32, Stop> {
    if fd != ROOT_FD {
        return Ok(errno::BADF);
    }
    if len < 1 {
        return Ok(errno::INVAL);
    }
    put(mem, path, b"/")?;
    Ok(errno::SUCCESS)
}

/// `fd_fdstat_get(fd, out)`: stdio a character device, [`ROOT_FD`] and an opened directory a
/// directory, an opened file a regular file, each with every right.
pub(super) fn fd_fdstat_get(mem: &mut [u8], cx: &mut Cx<'_>, fd: u32, out: u32) -> Result<i32, Stop> {
    let filetype = match fd {
        0..=2 => 2,
        ROOT_FD => 3,
        _ => match cx.os.stat_fd(os_fd(fd)) {
            Ok(stat) => filetype(stat.kind),
            Err(errno) => return Ok(wasi_errno(errno)),
        },
    };
    let mut stat = [0; 24];
    stat[0] = filetype;
    stat[8..16].copy_from_slice(&u64::MAX.to_le_bytes());
    stat[16..24].copy_from_slice(&u64::MAX.to_le_bytes());
    put(mem, out, &stat)?;
    Ok(errno::SUCCESS)
}

pub(super) fn fd_fdstat_set_flags(cx: &mut Cx<'_>, fd: u32) -> Result<i32, Stop> {
    Ok(match fd {
        0..=ROOT_FD => errno::NOTSUP,
        _ if cx.os.stat_fd(os_fd(fd)).is_ok() => errno::NOTSUP,
        _ => errno::BADF,
    })
}

pub(super) fn fd_close(cx: &mut Cx<'_>, fd: u32) -> Result<i32, Stop> {
    match fd {
        0..=2 => Ok(errno::SUCCESS),
        // alloc: error path
        ROOT_FD => Err(Stop("closes `/`, its one preopened directory".to_owned())),
        _ => Ok(match cx.os.close_fd(os_fd(fd)) {
            Ok(()) => errno::SUCCESS,
            Err(errno) => wasi_errno(errno),
        }),
    }
}

/// `fd_seek(fd, offset, whence, out)`, and `fd_tell` as a seek by 0 from where it is: stdio
/// does not seek; a file the macro opened does.
pub(super) fn fd_seek(mem: &mut [u8], cx: &mut Cx<'_>, fd: u32, offset: i64, whence: u32, out: u32) -> Result<i32, Stop> {
    match fd {
        0..=2 => Ok(errno::SPIPE),
        ROOT_FD => Ok(errno::BADF),
        _ => match cx.os.seek_fd(os_fd(fd), offset, i64::from(whence)) {
            Ok(at) => {
                put_u64(mem, out, at)?;
                Ok(errno::SUCCESS)
            }
            Err(errno) => Ok(wasi_errno(errno)),
        },
    }
}

/// WASI's `filestat` of `stat`: no times, owner or permissions, as [`Stat`] has none.
fn put_filestat(mem: &mut [u8], out: u32, stat: &Stat) -> Result<(), Stop> {
    let mut bytes = [0; 64];
    bytes[0..8].copy_from_slice(&crate::os::target::DEV.to_le_bytes());
    bytes[8..16].copy_from_slice(&stat.ino.to_le_bytes());
    bytes[16] = filetype(stat.kind);
    bytes[24..32].copy_from_slice(&1u64.to_le_bytes());
    bytes[32..40].copy_from_slice(&stat.size.to_le_bytes());
    put(mem, out, &bytes)
}

/// `fd_filestat_get(fd, out)`: what a file or directory the macro opened is. A look at stdio
/// or `/` itself stops the expansion: neither is below what it may read.
pub(super) fn fd_filestat_get(mem: &mut [u8], cx: &mut Cx<'_>, fd: u32, out: u32) -> Result<i32, Stop> {
    match fd {
        // alloc: error path
        0..=2 => Err(Stop(format!("looks at its standard stream {fd} (`fd_filestat_get`), which the wasm host does not serve"))),
        ROOT_FD => Err(cx.os.refuse_root()),
        _ => match cx.os.stat_fd(os_fd(fd)) {
            Ok(stat) => {
                put_filestat(mem, out, &stat)?;
                Ok(errno::SUCCESS)
            }
            Err(errno) => Ok(wasi_errno(errno)),
        },
    }
}

/// `path_open(fd, dirflags, path, len, oflags, base, inheriting, fdflags, out)`: the file or
/// directory `path` from `fd` names, opened to read. One opened to create, truncate, append or
/// write (the right wasi-libc asks for `O_WRONLY` and `O_RDWR`) stops the expansion.
#[allow(clippy::too_many_arguments)]
pub(super) fn path_open(
    mem: &mut [u8],
    cx: &mut Cx<'_>,
    (fd, dirflags, path, len): (u32, u32, u32, u32),
    oflags: u32,
    rights: u64,
    fdflags: u32,
    out: u32,
) -> Result<i32, Stop> {
    let path = path_at(mem, path, len)?;
    if oflags & (OFLAGS_CREAT | OFLAGS_EXCL | OFLAGS_TRUNC) != 0 || rights & RIGHTS_FD_WRITE != 0 || fdflags & FDFLAGS_APPEND != 0 {
        // alloc: error path
        return Err(Stop(format!("opens `{path}` to write, which a proc macro may not")));
    }
    let at = match at_of(fd) {
        Ok(at) => at,
        Err(errno) => return Ok(errno),
    };
    let follow = dirflags & LOOKUP_SYMLINK_FOLLOW != 0;
    match cx.os.open_path((at, path), follow, oflags & OFLAGS_DIRECTORY != 0)? {
        Ok(opened) => {
            put_u32(mem, out, wasi_fd(opened)?)?;
            Ok(errno::SUCCESS)
        }
        Err(errno) => Ok(wasi_errno(errno)),
    }
}

/// `path_filestat_get(fd, flags, path, len, out)`: a look at what `path` from `fd` names.
pub(super) fn path_filestat_get(mem: &mut [u8], cx: &mut Cx<'_>, (fd, flags, path, len): (u32, u32, u32, u32), out: u32) -> Result<i32, Stop> {
    let path = path_at(mem, path, len)?;
    let at = match at_of(fd) {
        Ok(at) => at,
        Err(errno) => return Ok(errno),
    };
    match cx.os.look((at, path), flags & LOOKUP_SYMLINK_FOLLOW != 0)? {
        Ok(stat) => {
            put_filestat(mem, out, &stat)?;
            Ok(errno::SUCCESS)
        }
        Err(errno) => Ok(wasi_errno(errno)),
    }
}

/// `path_readlink(fd, path, len, buf, buf_len, out)`: the target of the link `path` names, cut
/// to `buf_len` bytes.
pub(super) fn path_readlink(mem: &mut [u8], cx: &mut Cx<'_>, (fd, path, len): (u32, u32, u32), (buf, buf_len, out): (u32, u32, u32)) -> Result<i32, Stop> {
    let path = path_at(mem, path, len)?;
    let at = match at_of(fd) {
        Ok(at) => at,
        Err(errno) => return Ok(errno),
    };
    let target = match cx.os.link_target((at, path))? {
        Ok(target) => target,
        Err(errno) => return Ok(wasi_errno(errno)),
    };
    let bytes = target.as_os_str().as_encoded_bytes();
    let n = bytes.len().min(buf_len as usize);
    put(mem, buf, &bytes[..n])?;
    put_u32(mem, out, n as u32)?;
    Ok(errno::SUCCESS)
}

/// `fd_readdir(fd, buf, buf_len, cookie, out)`: the entries of a directory the macro opened
/// from entry `cookie` on, each a 24-byte `dirent` and its name, as many as fit; the last one
/// cut short, and `out` then `buf_len`, when they don't. Listing `/` stops the expansion.
pub(super) fn fd_readdir(mem: &mut [u8], cx: &mut Cx<'_>, fd: u32, (buf, buf_len): (u32, u32), cookie: u64, out: u32) -> Result<i32, Stop> {
    let fd = match fd {
        0..=2 => return Ok(errno::NOTDIR),
        ROOT_FD => return Err(cx.os.refuse_root()),
        _ => os_fd(fd),
    };
    let mut used = 0u32;
    let mut n = cookie;
    while used < buf_len {
        let entry = match cx.os.dir_entry(fd, n) {
            Ok(Some(entry)) => entry,
            Ok(None) => break,
            Err(errno) => return Ok(wasi_errno(errno)),
        };
        let mut head = [0u8; 24];
        head[0..8].copy_from_slice(&(n + 1).to_le_bytes());
        head[8..16].copy_from_slice(&entry.ino.to_le_bytes());
        head[16..20].copy_from_slice(&(entry.name.len() as u32).to_le_bytes());
        head[20] = filetype(entry.kind);
        for part in [&head[..], entry.name] {
            let room = (buf_len - used) as usize;
            let part = &part[..part.len().min(room)];
            put(mem, buf.wrapping_add(used), part)?;
            used += part.len() as u32;
        }
        n += 1;
    }
    put_u32(mem, out, used)?;
    Ok(errno::SUCCESS)
}

pub(super) fn poll_oneoff() -> Result<i32, Stop> {
    Err(Stop("waits (`poll_oneoff`, as `thread::sleep` does), which a macro's expansion may not".to_owned())) // alloc: error path
}

pub(super) fn proc_exit(code: u32) -> Stop {
    // alloc: error path
    Stop(format!("exits the process with code {code} (`std::process::exit`), which a macro's expansion may not"))
}
