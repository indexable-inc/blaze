//! WASI preview 1 as blaze serves it to a wasm guest: the one implementation of each call's
//! arguments and memory layout, for both pointer widths, over a [`System`] that says what the
//! call does.
//!
//! - **Widths.** A 32-bit guest (`wasm32-wasip1`: a proc macro, a build script) imports the
//!   calls from `wasi_snapshot_preview1` ([`W32`]). A 64-bit guest (memory64: the bootstrap
//!   seed, later macros) imports the same calls from `wasi_snapshot_preview1_64` ([`W64`]),
//!   where every `pointer` and `size` of the witx signatures is an `i64` and every `size` field
//!   of a struct in memory is 8 bytes at 8-byte alignment: an `iovec` is 16 bytes (`buf` at 0,
//!   `buf_len` at 8), a `prestat` is 16 (its tag at 0, `pr_name_len` at 8), and each `argv` or
//!   `environ` pointer is 8. Everything that is not a pointer or a size keeps its witx type and
//!   layout on both: descriptors, `errno` and flags are `i32`; timestamps, rights, offsets and
//!   cookies `i64`; `fdstat` (24 bytes), `filestat` (64) and `dirent` (24 and its name) are the
//!   same bytes. The module name tells the two apart, so a 32-bit guest can never be linked
//!   against the 64-bit calls by accident, or the other way.
//! - **Policy.** What a call does (which files it may read, whether a write reaches a file or
//!   is dropped, what the clock says) is the [`System`]'s: the macro sandbox's in
//!   `blaze-macro-eval` (`wasm/wasi.rs`), the seed runner's in `blaze-seed-run`. This crate
//!   decodes the guest's arguments, checks every address against the guest's memory, calls the
//!   system, and writes its answer back. A guest address outside its memory stops the guest
//!   with a [`Stop`] naming it, never a host fault.
//! - **Store.** The embedder's store data implements [`Embed`]: it hands the guest's memory and
//!   its system to each call, and keeps the [`Stop`] a call ended with.
//! - **Two ABIs.** [`link`] defines [`FUNCTIONS`], the WASI p1 calls every guest may import:
//!   the reads, stdio, clocks and exits a sandboxed macro or build script is served, nothing
//!   that writes the file system. The seed's own calls ([`seed`], module `blaze_seed`: files
//!   written, memory decommitted) are blaze's ABI, not WASI's, linked only by an embedder whose
//!   store implements [`seed::EmbedSeed`]: the seed runner. The macro host does not, so it
//!   cannot link them, and its import check refuses their module.
//! - **Descriptors.** Every embedder preopens one directory, `/` ([`ROOT`]), at [`ROOT_FD`];
//!   0, 1 and 2 are stdio.

mod calls;
pub mod seed;

use std::fmt;

pub use calls::{bytes, link, put};
use wasmtime::{Caller, WasmTy};

/// Why a guest was stopped that is not its own trap: what it did, said so that the embedder's
/// error names the guest before it ("the proc macro `x` <what>").
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Stop(pub String);

impl fmt::Display for Stop {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// The width of a guest's pointers and sizes, and the module it imports WASI from.
pub trait Width: Send + Sync + 'static {
    /// A pointer or size argument as the guest passes it: `i32` or `i64`.
    type Ptr: WasmTy + Copy + Into<u64> + Send + Sync + 'static;
    /// The bytes of a pointer or a `size` in the guest's memory.
    const BYTES: u64;
    /// The import module the guest's calls come from.
    const MODULE: &'static str;
}

/// `wasm32-wasip1`: 32-bit pointers and sizes, from `wasi_snapshot_preview1`.
pub enum W32 {}

/// memory64: 64-bit pointers and sizes, from `wasi_snapshot_preview1_64`.
pub enum W64 {}

impl Width for W32 {
    type Ptr = u32;
    const BYTES: u64 = 4;
    const MODULE: &'static str = "wasi_snapshot_preview1";
}

impl Width for W64 {
    type Ptr = u64;
    const BYTES: u64 = 8;
    const MODULE: &'static str = "wasi_snapshot_preview1_64";
}

/// Every function [`link`] defines, the same names under both modules: what an embedder checks
/// a guest's imports against.
pub const FUNCTIONS: &[&str] = &[
    "args_get",
    "args_sizes_get",
    "clock_res_get",
    "clock_time_get",
    "environ_get",
    "environ_sizes_get",
    "fd_close",
    "fd_fdstat_get",
    "fd_fdstat_set_flags",
    "fd_filestat_get",
    "fd_prestat_dir_name",
    "fd_prestat_get",
    "fd_read",
    "fd_readdir",
    "fd_seek",
    "fd_tell",
    "fd_write",
    "path_filestat_get",
    "path_open",
    "path_readlink",
    "poll_oneoff",
    "proc_exit",
    "random_get",
    "sched_yield",
];

/// The one preopened directory's descriptor.
pub const ROOT_FD: u32 = 3;

/// The one preopened directory's name: the root of the paths a guest names.
pub const ROOT: &[u8] = b"/";

/// A WASI `errno`: what a call that failed returns to the guest.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Errno(pub u16);

impl Errno {
    pub const ACCES: Errno = Errno(2);
    pub const BADF: Errno = Errno(8);
    pub const EXIST: Errno = Errno(20);
    pub const FAULT: Errno = Errno(21);
    pub const FBIG: Errno = Errno(22);
    pub const INTR: Errno = Errno(27);
    pub const INVAL: Errno = Errno(28);
    pub const IO: Errno = Errno(29);
    pub const ISDIR: Errno = Errno(31);
    pub const LOOP: Errno = Errno(32);
    pub const MFILE: Errno = Errno(33);
    pub const NAMETOOLONG: Errno = Errno(37);
    pub const NOENT: Errno = Errno(44);
    pub const NOMEM: Errno = Errno(48);
    pub const NOSPC: Errno = Errno(51);
    pub const NOTDIR: Errno = Errno(54);
    pub const NOTEMPTY: Errno = Errno(55);
    pub const NOTSUP: Errno = Errno(58);
    pub const OVERFLOW: Errno = Errno(61);
    pub const PERM: Errno = Errno(63);
    pub const PIPE: Errno = Errno(64);
    pub const RANGE: Errno = Errno(68);
    pub const ROFS: Errno = Errno(69);
    pub const SPIPE: Errno = Errno(70);
    pub const XDEV: Errno = Errno(75);
    pub const NOTCAPABLE: Errno = Errno(76);

    /// Its POSIX name (`ENOENT`), for those above; wasi-libc's `errno` is WASI's number.
    pub fn name(self) -> Option<&'static str> {
        Some(match self {
            Errno::ACCES => "EACCES",
            Errno::BADF => "EBADF",
            Errno::EXIST => "EEXIST",
            Errno::FAULT => "EFAULT",
            Errno::FBIG => "EFBIG",
            Errno::INTR => "EINTR",
            Errno::INVAL => "EINVAL",
            Errno::IO => "EIO",
            Errno::ISDIR => "EISDIR",
            Errno::LOOP => "ELOOP",
            Errno::MFILE => "EMFILE",
            Errno::NAMETOOLONG => "ENAMETOOLONG",
            Errno::NOENT => "ENOENT",
            Errno::NOMEM => "ENOMEM",
            Errno::NOSPC => "ENOSPC",
            Errno::NOTDIR => "ENOTDIR",
            Errno::NOTEMPTY => "ENOTEMPTY",
            Errno::NOTSUP => "ENOTSUP",
            Errno::OVERFLOW => "EOVERFLOW",
            Errno::PERM => "EPERM",
            Errno::PIPE => "EPIPE",
            Errno::RANGE => "ERANGE",
            Errno::ROFS => "EROFS",
            Errno::SPIPE => "ESPIPE",
            Errno::XDEV => "EXDEV",
            Errno::NOTCAPABLE => "ENOTCAPABLE",
            _ => return None,
        })
    }
}

/// WASI's `filetype`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum Filetype {
    Unknown = 0,
    BlockDevice = 1,
    CharacterDevice = 2,
    Directory = 3,
    RegularFile = 4,
    SymbolicLink = 7,
}

/// WASI's `filestat`: what a file is.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Filestat {
    pub dev: u64,
    pub ino: u64,
    pub filetype: Filetype,
    pub nlink: u64,
    pub size: u64,
    /// Access, modification and status-change times, in nanoseconds since the epoch.
    pub atim: u64,
    pub mtim: u64,
    pub ctim: u64,
}

/// One entry of a directory: what `fd_readdir` writes as a `dirent` and its name.
pub struct Dirent<'a> {
    pub ino: u64,
    pub filetype: Filetype,
    pub name: &'a [u8],
}

/// How `path_open` asks to open a path: its flags as the guest passed them.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Open {
    pub dirflags: u32,
    pub oflags: u32,
    pub rights_base: u64,
    pub rights_inheriting: u64,
    pub fdflags: u32,
}

impl Open {
    /// `lookupflags`' `symlink_follow`: the last component is followed if it is a link.
    pub fn follow(&self) -> bool {
        self.dirflags & 1 != 0
    }

    /// `oflags`' `creat`.
    pub fn create(&self) -> bool {
        self.oflags & 1 != 0
    }

    /// `oflags`' `directory`: the path must name a directory.
    pub fn directory(&self) -> bool {
        self.oflags & 2 != 0
    }

    /// `oflags`' `excl`: with `creat`, the file must not exist.
    pub fn exclusive(&self) -> bool {
        self.oflags & 4 != 0
    }

    /// `oflags`' `trunc`.
    pub fn truncate(&self) -> bool {
        self.oflags & 8 != 0
    }

    /// The right `fd_read`, which wasi-libc asks for a file opened `O_RDONLY` or `O_RDWR`.
    pub fn read(&self) -> bool {
        self.rights_base & (1 << 1) != 0
    }

    /// The right `fd_write`, which wasi-libc asks for a file opened `O_WRONLY` or `O_RDWR`.
    pub fn write(&self) -> bool {
        self.rights_base & (1 << 6) != 0
    }

    /// `fdflags`' `append`.
    pub fn append(&self) -> bool {
        self.fdflags & 1 != 0
    }
}

/// What a call answers: its value, the `errno` the guest is given, or why the guest stops.
pub type Answer<T> = Result<Result<T, Errno>, Stop>;

/// What each WASI call does, with its arguments decoded and its buffers checked: the policy of
/// one embedder. Descriptors are the guest's, as it passed them.
pub trait System {
    /// The arguments, each NUL-terminated, back to back, and how many there are.
    fn args(&mut self) -> Result<(&[u8], usize), Stop>;
    /// The environment as `NAME=value` strings, each NUL-terminated, back to back, and how many.
    fn environ(&mut self) -> Result<(&[u8], usize), Stop>;
    fn clock_res(&mut self, id: u32) -> Answer<u64>;
    /// The time on clock `id`, in nanoseconds; `fuel_left` is the fuel the store has left when
    /// it counts fuel.
    fn clock_time(&mut self, id: u32, fuel_left: Option<u64>) -> Answer<u64>;
    /// Fills `into`, a buffer of the guest's memory, with random bytes.
    fn random(&mut self, into: &mut [u8]) -> Result<(), Stop>;
    /// Writes `bytes` to `fd`: how many were written.
    fn write(&mut self, fd: u32, bytes: &[u8]) -> Answer<usize>;
    /// Reads from `fd` into `into`, a buffer of the guest's memory: how many were read.
    fn read(&mut self, fd: u32, into: &mut [u8]) -> Answer<usize>;
    /// What `fd` is, for `fd_fdstat_get`.
    fn fd_filetype(&mut self, fd: u32) -> Answer<Filetype>;
    fn set_fd_flags(&mut self, fd: u32, flags: u32) -> Answer<()>;
    fn close(&mut self, fd: u32) -> Answer<()>;
    /// Moves `fd`'s position by `offset` from `whence` (0 the start, 1 where it is, 2 the end):
    /// the new position.
    fn seek(&mut self, fd: u32, offset: i64, whence: u32) -> Answer<u64>;
    fn fd_stat(&mut self, fd: u32) -> Answer<Filestat>;
    /// Opens `path`, the bytes the guest named, from the directory `at`: the new descriptor.
    fn open(&mut self, at: u32, path: &[u8], how: Open) -> Answer<u32>;
    fn path_stat(&mut self, at: u32, path: &[u8], follow: bool) -> Answer<Filestat>;
    /// The target of the link `path` from `at` names.
    fn readlink(&mut self, at: u32, path: &[u8]) -> Answer<Vec<u8>>;
    /// Entry `cookie` of the directory `fd` opened, or `None` past its last.
    fn dir_entry(&mut self, fd: u32, cookie: u64) -> Answer<Option<Dirent<'_>>>;
    /// `poll_oneoff`, which `thread::sleep` makes: no system blaze runs a guest under serves a
    /// wait, so each says why it stops the guest.
    fn poll_oneoff(&mut self) -> Stop;
    /// `proc_exit(code)`: why the guest ends here, as the embedder tells an exit apart.
    fn exit(&mut self, code: u32) -> Stop;
}

/// The store data of an embedder that links the calls: each call's way to the guest's memory
/// and the [`System`] that answers it.
pub trait Embed: Sized + 'static {
    /// Runs `f` on the guest's memory and the system; a [`Stop`] it returns is kept by the
    /// embedder and ends the guest with a Wasmtime error.
    fn with<R>(
        caller: &mut Caller<'_, Self>,
        f: impl FnOnce(&mut [u8], &mut dyn System) -> Result<R, Stop>, // dyn: each embedder's system, behind one set of calls
    ) -> wasmtime::Result<R>;
}

#[cfg(test)]
mod tests;
