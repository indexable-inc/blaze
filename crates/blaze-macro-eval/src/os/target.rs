//! What differs between the targets a proc macro runs on: the codes of `errno`, the flags of
//! `open` and the `*at` calls, the numbers of the system calls std makes through `syscall`,
//! and the layouts of `struct stat`, `struct statx` and `struct dirent`, each as glibc (x86-64
//! and AArch64) and macOS (AArch64 and x86-64, with 64-bit inodes) have them. The tests check
//! the host's against `libc`.

/// A target the shims model: a 64-bit Linux with glibc, or a 64-bit macOS.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Target {
    pub(crate) system: System,
    pub(crate) arch: Arch,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum System {
    Linux,
    Apple,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Arch {
    X86_64,
    Aarch64,
}

/// The `errno`s a shim gives.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Errno {
    Noent,
    Io,
    Badf,
    Acces,
    Fault,
    Notdir,
    Isdir,
    Inval,
    Spipe,
    Rofs,
    Range,
    Nametoolong,
    Loop,
}

/// What a file is, as a `stat` and a `dirent` say it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Kind {
    File,
    Dir,
    Link,
    /// A FIFO, a socket or a device: listed with `DT_UNKNOWN`, never looked at or opened.
    Other,
}

/// What a `stat` of a file says: its kind and size, and an inode of its real path. The rest
/// (times, owner, device, permissions) is the same for every file, so that a look at one says
/// no more than its kind and bytes, which is what the trace keys it by.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Stat {
    pub(crate) kind: Kind,
    pub(crate) size: u64,
    pub(crate) ino: u64,
}

/// The device every file is on.
pub(crate) const DEV: u64 = 1;

/// The largest `struct stat` of the targets.
pub(crate) const STAT_MAX: usize = 144;

/// `struct statx`, on Linux.
pub(crate) const STATX_SIZE: usize = 256;

/// The largest `struct dirent` of the targets.
pub(crate) const DIRENT_MAX: usize = 1048;

/// `STATX_BASIC_STATS`: what a `statx` fills.
const STATX_BASIC_STATS: u32 = 0x7ff;

/// Where a field is in a struct: its offset and its width in bytes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Field(pub(crate) usize, pub(crate) usize);

/// Where `struct stat`'s fields are on a target.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct StatLayout {
    pub(crate) size: usize,
    pub(crate) dev: Field,
    pub(crate) ino: Field,
    pub(crate) mode: Field,
    pub(crate) nlink: Field,
    pub(crate) uid: Field,
    pub(crate) gid: Field,
    pub(crate) rdev: Field,
    pub(crate) st_size: Field,
    pub(crate) blksize: Field,
    pub(crate) blocks: Field,
}

/// Where `struct dirent` (Linux's `dirent64`, which is its `dirent` too) has its fields.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct DirentLayout {
    pub(crate) size: usize,
    pub(crate) ino: Field,
    /// `d_off` on Linux, `d_seekoff` on macOS.
    pub(crate) off: Field,
    pub(crate) reclen: Field,
    /// macOS's `d_namlen`.
    pub(crate) namlen: Option<Field>,
    pub(crate) kind: Field,
    pub(crate) name: usize,
}

const LINUX_X86_64_STAT: StatLayout = StatLayout {
    size: 144,
    dev: Field(0, 8),
    ino: Field(8, 8),
    nlink: Field(16, 8),
    mode: Field(24, 4),
    uid: Field(28, 4),
    gid: Field(32, 4),
    rdev: Field(40, 8),
    st_size: Field(48, 8),
    blksize: Field(56, 8),
    blocks: Field(64, 8),
};

const LINUX_AARCH64_STAT: StatLayout = StatLayout {
    size: 128,
    dev: Field(0, 8),
    ino: Field(8, 8),
    mode: Field(16, 4),
    nlink: Field(20, 4),
    uid: Field(24, 4),
    gid: Field(28, 4),
    rdev: Field(32, 8),
    st_size: Field(48, 8),
    blksize: Field(56, 4),
    blocks: Field(64, 8),
};

const APPLE_STAT: StatLayout = StatLayout {
    size: 144,
    dev: Field(0, 4),
    mode: Field(4, 2),
    nlink: Field(6, 2),
    ino: Field(8, 8),
    uid: Field(16, 4),
    gid: Field(20, 4),
    rdev: Field(24, 4),
    st_size: Field(96, 8),
    blocks: Field(104, 8),
    blksize: Field(112, 4),
};

const LINUX_DIRENT: DirentLayout = DirentLayout {
    size: 280,
    ino: Field(0, 8),
    off: Field(8, 8),
    reclen: Field(16, 2),
    namlen: None,
    kind: Field(18, 1),
    name: 19,
};

const APPLE_DIRENT: DirentLayout = DirentLayout {
    size: 1048,
    ino: Field(0, 8),
    off: Field(8, 8),
    reclen: Field(16, 2),
    namlen: Some(Field(18, 2)),
    kind: Field(20, 1),
    name: 21,
};

/// `struct statx`'s fields, on both Linux targets.
pub(crate) mod statx {
    use super::Field;

    pub(crate) const MASK: Field = Field(0, 4);
    pub(crate) const BLKSIZE: Field = Field(4, 4);
    pub(crate) const NLINK: Field = Field(16, 4);
    pub(crate) const UID: Field = Field(20, 4);
    pub(crate) const GID: Field = Field(24, 4);
    pub(crate) const MODE: Field = Field(28, 2);
    pub(crate) const INO: Field = Field(32, 8);
    pub(crate) const SIZE: Field = Field(40, 8);
    pub(crate) const BLOCKS: Field = Field(48, 8);
    pub(crate) const DEV_MAJOR: Field = Field(136, 4);
    pub(crate) const DEV_MINOR: Field = Field(140, 4);
}

const S_IFDIR: u64 = 0o040000;
const S_IFREG: u64 = 0o100000;
const S_IFLNK: u64 = 0o120000;

const DT_UNKNOWN: u8 = 0;
const DT_DIR: u8 = 4;
const DT_REG: u8 = 8;
const DT_LNK: u8 = 10;

/// The flags of `open` that write, create or truncate, and those the shims look at.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct OpenFlags {
    /// `O_WRONLY | O_RDWR | O_CREAT | O_TRUNC | O_APPEND`, and Linux's `O_TMPFILE`.
    pub(crate) writes: i64,
    pub(crate) directory: i64,
    pub(crate) nofollow: i64,
}

/// The flags of the `*at` calls, and the directory `AT_FDCWD` names.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct AtFlags {
    pub(crate) fdcwd: i64,
    pub(crate) symlink_nofollow: i64,
    /// Linux's `AT_EMPTY_PATH`; none on macOS.
    pub(crate) empty_path: i64,
}

/// What a `syscall` of a number is, as the shims answer it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Sys {
    Statx,
    Getrandom,
    /// One the evaluator's thread shims answer (`futex`, `gettid`).
    Evaluator,
    /// Any other, by its name when it has a known one.
    Other(Option<&'static str>),
}

impl Target {
    /// The target of a target tuple, when it is one the shims model.
    pub(crate) fn of_tuple(tuple: &str) -> Option<Target> {
        let arch = match tuple.split('-').next()? {
            "x86_64" => Arch::X86_64,
            "aarch64" | "arm64" => Arch::Aarch64,
            _ => return None,
        };
        let system = if tuple.contains("-linux-gnu") {
            System::Linux
        } else if tuple.contains("-apple-darwin") || tuple.contains("-apple-macos") {
            System::Apple
        } else {
            return None;
        };
        Some(Target { system, arch })
    }

    /// The target this was built for, when it is one the shims model.
    #[cfg(test)]
    pub(crate) fn host() -> Option<Target> {
        let arch = match std::env::consts::ARCH {
            "x86_64" => Arch::X86_64,
            "aarch64" => Arch::Aarch64,
            _ => return None,
        };
        let system = match std::env::consts::OS {
            "linux" if cfg!(target_env = "gnu") => System::Linux,
            "macos" => System::Apple,
            _ => return None,
        };
        Some(Target { system, arch })
    }

    pub(crate) fn errno(self, errno: Errno) -> i32 {
        let apple = self.system == System::Apple;
        match errno {
            Errno::Noent => 2,
            Errno::Io => 5,
            Errno::Badf => 9,
            Errno::Acces => 13,
            Errno::Fault => 14,
            Errno::Notdir => 20,
            Errno::Isdir => 21,
            Errno::Inval => 22,
            Errno::Spipe => 29,
            Errno::Rofs => 30,
            Errno::Range => 34,
            Errno::Nametoolong if apple => 63,
            Errno::Nametoolong => 36,
            Errno::Loop if apple => 62,
            Errno::Loop => 40,
        }
    }

    pub(crate) fn open_flags(self) -> OpenFlags {
        match (self.system, self.arch) {
            (System::Linux, arch) => OpenFlags {
                // O_WRONLY | O_RDWR | O_CREAT | O_TRUNC | O_APPEND | __O_TMPFILE
                writes: 0o1 | 0o2 | 0o100 | 0o1000 | 0o2000 | 0o20000000,
                directory: if arch == Arch::X86_64 { 0o200000 } else { 0o40000 },
                nofollow: if arch == Arch::X86_64 { 0o400000 } else { 0o100000 },
            },
            (System::Apple, _) => OpenFlags {
                // O_WRONLY | O_RDWR | O_APPEND | O_CREAT | O_TRUNC
                writes: 0x1 | 0x2 | 0x8 | 0x200 | 0x400,
                directory: 0x100000,
                nofollow: 0x100,
            },
        }
    }

    pub(crate) fn at_flags(self) -> AtFlags {
        match self.system {
            System::Linux => AtFlags { fdcwd: -100, symlink_nofollow: 0x100, empty_path: 0x1000 },
            System::Apple => AtFlags { fdcwd: -2, symlink_nofollow: 0x20, empty_path: 0 },
        }
    }

    /// `PATH_MAX`: what a `realpath` into the caller's buffer may write, its NUL included.
    pub(crate) fn path_max(self) -> usize {
        match self.system {
            System::Linux => 4096,
            System::Apple => 1024,
        }
    }

    pub(crate) fn stat_layout(self) -> StatLayout {
        match (self.system, self.arch) {
            (System::Linux, Arch::X86_64) => LINUX_X86_64_STAT,
            (System::Linux, Arch::Aarch64) => LINUX_AARCH64_STAT,
            (System::Apple, _) => APPLE_STAT,
        }
    }

    pub(crate) fn dirent_layout(self) -> DirentLayout {
        match self.system {
            System::Linux => LINUX_DIRENT,
            System::Apple => APPLE_DIRENT,
        }
    }

    /// `stat` as this target's `struct stat`, in `out`'s first [`StatLayout::size`] bytes.
    pub(crate) fn put_stat<'o>(self, stat: &Stat, out: &'o mut [u8; STAT_MAX]) -> &'o [u8] {
        let layout = self.stat_layout();
        *out = [0; STAT_MAX];
        put(out, layout.dev, DEV);
        put(out, layout.ino, stat.ino);
        put(out, layout.mode, mode(stat.kind));
        put(out, layout.nlink, nlink(stat.kind));
        put(out, layout.rdev, 0);
        put(out, layout.st_size, stat.size);
        put(out, layout.blksize, 4096);
        put(out, layout.blocks, stat.size.div_ceil(512));
        &out[..layout.size]
    }

    /// `stat` as Linux's `struct statx`.
    pub(crate) fn put_statx<'o>(self, stat: &Stat, out: &'o mut [u8; STATX_SIZE]) -> &'o [u8] {
        *out = [0; STATX_SIZE];
        put(out, statx::MASK, u64::from(STATX_BASIC_STATS));
        put(out, statx::BLKSIZE, 4096);
        put(out, statx::NLINK, nlink(stat.kind));
        put(out, statx::MODE, mode(stat.kind));
        put(out, statx::INO, stat.ino);
        put(out, statx::SIZE, stat.size);
        put(out, statx::BLOCKS, stat.size.div_ceil(512));
        // `makedev(0, 1)` is `DEV`, as a `stat` has it.
        put(out, statx::DEV_MAJOR, 0);
        put(out, statx::DEV_MINOR, DEV);
        out
    }

    /// The `struct dirent` of the entry `name` (`index`th of its directory) in `out`: its
    /// fields and name with its NUL, which is all of it a reader reads.
    pub(crate) fn put_dirent<'o>(
        self,
        (name, kind, ino, index): (&[u8], Kind, u64, u64),
        out: &'o mut [u8; DIRENT_MAX],
    ) -> &'o [u8] {
        let layout = self.dirent_layout();
        let end = layout.name + name.len() + 1;
        out[..end].fill(0);
        put(out, layout.ino, ino);
        put(out, layout.off, index + 1);
        let reclen = match layout.namlen {
            Some(_) => layout.size,
            None => end.next_multiple_of(8),
        };
        put(out, layout.reclen, reclen as u64);
        if let Some(namlen) = layout.namlen {
            put(out, namlen, name.len() as u64);
        }
        put(out, layout.kind, u64::from(dirent_kind(kind)));
        out[layout.name..layout.name + name.len()].copy_from_slice(name);
        &out[..end]
    }

    /// What `syscall` with the number `number` is. macOS's std makes none.
    pub(crate) fn sys(self, number: i64) -> Sys {
        let Some(numbers) = self.sys_numbers() else { return Sys::Other(None) };
        match number {
            _ if number == numbers.statx => Sys::Statx,
            _ if number == numbers.getrandom => Sys::Getrandom,
            _ if number == numbers.futex || number == numbers.gettid => Sys::Evaluator,
            _ => Sys::Other(numbers.names.iter().find(|it| it.0 == number).map(|it| it.1)),
        }
    }

    fn sys_numbers(self) -> Option<SysNumbers> {
        match (self.system, self.arch) {
            (System::Apple, _) => None,
            (System::Linux, Arch::X86_64) => Some(SysNumbers {
                statx: 332,
                getrandom: 318,
                futex: 202,
                gettid: 186,
                names: LINUX_X86_64_SYSCALLS,
            }),
            (System::Linux, Arch::Aarch64) => Some(SysNumbers {
                statx: 291,
                getrandom: 278,
                futex: 98,
                gettid: 178,
                names: LINUX_AARCH64_SYSCALLS,
            }),
        }
    }

    #[cfg(test)]
    pub(crate) fn sys_number_of_statx(self) -> i64 {
        self.sys_numbers().map_or(-1, |it| it.statx)
    }

    #[cfg(test)]
    pub(crate) fn sys_number_of_futex(self) -> i64 {
        self.sys_numbers().map_or(-1, |it| it.futex)
    }
}

/// The numbers of the system calls std makes through `syscall` on a Linux target, and of
/// those the evaluator's threads make, with the names of others for the error that stops one.
struct SysNumbers {
    statx: i64,
    getrandom: i64,
    futex: i64,
    gettid: i64,
    names: &'static [(i64, &'static str)],
}

fn mode(kind: Kind) -> u64 {
    match kind {
        Kind::File => S_IFREG | 0o644,
        Kind::Dir => S_IFDIR | 0o755,
        Kind::Link => S_IFLNK | 0o777,
        Kind::Other => 0,
    }
}

fn nlink(kind: Kind) -> u64 {
    match kind {
        Kind::Dir => 2,
        Kind::File | Kind::Link | Kind::Other => 1,
    }
}

fn dirent_kind(kind: Kind) -> u8 {
    match kind {
        Kind::File => DT_REG,
        Kind::Dir => DT_DIR,
        Kind::Link => DT_LNK,
        Kind::Other => DT_UNKNOWN,
    }
}

/// `value` at `field` of `out`, little-endian, truncated to the field's width.
fn put(out: &mut [u8], Field(at, width): Field, value: u64) {
    out[at..at + width].copy_from_slice(&value.to_le_bytes()[..width]);
}

/// Names of the system calls a proc macro is likeliest to make, for the error that stops it.
const LINUX_X86_64_SYSCALLS: &[(i64, &str)] = &[
    (0, "read"),
    (1, "write"),
    (2, "open"),
    (3, "close"),
    (4, "stat"),
    (5, "fstat"),
    (6, "lstat"),
    (8, "lseek"),
    (9, "mmap"),
    (10, "mprotect"),
    (11, "munmap"),
    (16, "ioctl"),
    (17, "pread64"),
    (19, "readv"),
    (20, "writev"),
    (21, "access"),
    (39, "getpid"),
    (41, "socket"),
    (42, "connect"),
    (56, "clone"),
    (57, "fork"),
    (59, "execve"),
    (60, "exit"),
    (62, "kill"),
    (72, "fcntl"),
    (79, "getcwd"),
    (82, "rename"),
    (83, "mkdir"),
    (84, "rmdir"),
    (87, "unlink"),
    (89, "readlink"),
    (217, "getdents64"),
    (228, "clock_gettime"),
    (231, "exit_group"),
    (257, "openat"),
    (262, "newfstatat"),
    (435, "clone3"),
];

const LINUX_AARCH64_SYSCALLS: &[(i64, &str)] = &[
    (17, "getcwd"),
    (25, "fcntl"),
    (29, "ioctl"),
    (34, "mkdirat"),
    (35, "unlinkat"),
    (38, "renameat"),
    (48, "faccessat"),
    (56, "openat"),
    (57, "close"),
    (61, "getdents64"),
    (62, "lseek"),
    (63, "read"),
    (64, "write"),
    (65, "readv"),
    (66, "writev"),
    (67, "pread64"),
    (78, "readlinkat"),
    (79, "newfstatat"),
    (80, "fstat"),
    (93, "exit"),
    (94, "exit_group"),
    (113, "clock_gettime"),
    (129, "kill"),
    (172, "getpid"),
    (198, "socket"),
    (203, "connect"),
    (215, "munmap"),
    (220, "clone"),
    (221, "execve"),
    (222, "mmap"),
    (226, "mprotect"),
    (435, "clone3"),
];
