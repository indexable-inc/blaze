//! Landlock (linux/landlock.h): an unprivileged process confines itself, and every child it
//! starts after, to the files and ports its rules name. The confinement of a native build
//! script or a tool a sandboxed step starts: bstd's hermetic run (D177) makes these calls, and
//! blaze-core's linked start enters the result in its child (`linux::confine`).
//!
//! The order: [`landlock_abi`] says which rights the kernel knows (refuse to confine when it
//! is older than the rights a grant needs: no weaker confinement in its place); a ruleset
//! naming every right it handles ([`landlock_create_ruleset`]); a rule per path or port
//! ([`landlock_add_rule_path`], [`landlock_add_rule_port`]); then, in the process to confine,
//! `prctl(NoNewPrivs)` and [`landlock_restrict_self`].

use super::{
    fs::{OwnedFd, arg, owned},
    nr, raw,
};
use crate::{Errno, Fd};

flag_set! {
    /// The file-system rights (`LANDLOCK_ACCESS_FS_*`); the ABI each came in is in its doc.
    AccessFs: u64 {
        /// ABI 1: run a file.
        EXECUTE = 1 << 0;
        /// ABI 1: open a file to write.
        WRITE_FILE = 1 << 1;
        /// ABI 1: open a file to read.
        READ_FILE = 1 << 2;
        /// ABI 1: list a directory.
        READ_DIR = 1 << 3;
        /// ABI 1.
        REMOVE_DIR = 1 << 4;
        /// ABI 1.
        REMOVE_FILE = 1 << 5;
        /// ABI 1.
        MAKE_CHAR = 1 << 6;
        /// ABI 1.
        MAKE_DIR = 1 << 7;
        /// ABI 1.
        MAKE_REG = 1 << 8;
        /// ABI 1.
        MAKE_SOCK = 1 << 9;
        /// ABI 1.
        MAKE_FIFO = 1 << 10;
        /// ABI 1.
        MAKE_BLOCK = 1 << 11;
        /// ABI 1.
        MAKE_SYM = 1 << 12;
        /// ABI 2: link or rename a file into another directory.
        REFER = 1 << 13;
        /// ABI 3: truncate a file.
        TRUNCATE = 1 << 14;
        /// ABI 5: `ioctl` on a device.
        IOCTL_DEV = 1 << 15;
    }
}

impl AccessFs {
    /// Every right of ABI 1.
    pub const ALL_ABI1: AccessFs = AccessFs::from_bits((1 << 13) - 1);
}

flag_set! {
    /// The network rights (`LANDLOCK_ACCESS_NET_*`), ABI 4.
    AccessNet: u64 {
        /// Bind a TCP socket to a port.
        BIND_TCP = 1 << 0;
        /// Connect a TCP socket to a port.
        CONNECT_TCP = 1 << 1;
    }
}

/// `struct landlock_ruleset_attr`, through its ABI-6 field: the rights a ruleset handles (each
/// one denied unless a rule grants it). A kernel older than a field takes this size while the
/// fields it lacks are 0.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(C)]
pub struct RulesetAttr {
    pub handled_access_fs: u64,
    /// ABI 4.
    pub handled_access_net: u64,
    /// ABI 6: `LANDLOCK_SCOPE_*`.
    pub scoped: u64,
}

/// `struct landlock_path_beneath_attr` (packed: 12 bytes): `allowed` under the directory or
/// file open as `parent` (an `O_PATH` descriptor does).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(C, packed)]
pub struct PathBeneath {
    pub allowed_access: u64,
    pub parent_fd: i32,
}

/// `struct landlock_net_port_attr`: `allowed` on one TCP port.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(C)]
pub struct NetPort {
    pub allowed_access: u64,
    pub port: u64,
}

/// `LANDLOCK_CREATE_RULESET_VERSION`.
const CREATE_RULESET_VERSION: usize = 1 << 0;
/// `LANDLOCK_RULE_PATH_BENEATH` and `LANDLOCK_RULE_NET_PORT`.
const RULE_PATH_BENEATH: usize = 1;
const RULE_NET_PORT: usize = 2;

/// The Landlock ABI this kernel has (1 or more). [`Errno::OpNotSupp`] where Landlock is built
/// but not enabled at boot, [`Errno::NoSys`] where it is not built: the sandbox stops, naming
/// it (rule 9: where no sandbox can be made, blaze stops).
/// replaces libc::syscall(libc::SYS_landlock_create_ruleset) (the probe: null attr, size 0, LANDLOCK_CREATE_RULESET_VERSION; as blaze_sys::linux::landlock_abi(); returns Result<u32 ABI, Errno>)
/// replaces libc::SYS_landlock_create_ruleset
#[blaze::noalloc]
#[blaze::nopanic]
pub fn landlock_abi() -> Result<u32, Errno> {
    // SAFETY: a null attribute of size 0 with the version flag reads no memory.
    let abi = raw::check(unsafe { raw::syscall3(nr::LANDLOCK_CREATE_RULESET, 0, 0, CREATE_RULESET_VERSION) })?;
    Ok(abi as u32)
}

/// `landlock_create_ruleset`: a ruleset handling `attr`'s rights. [`Errno::Inval`] for a right
/// the kernel's ABI lacks, which the caller has refused before by [`landlock_abi`].
/// replaces libc::syscall(libc::SYS_landlock_create_ruleset) (a ruleset: as blaze_sys::linux::landlock_create_ruleset(&RulesetAttr); the size is passed for you; returns Result<OwnedFd, Errno>)
/// replaces libc::SYS_landlock_create_ruleset
#[blaze::noalloc]
#[blaze::nopanic]
pub fn landlock_create_ruleset(attr: &RulesetAttr) -> Result<OwnedFd, Errno> {
    // SAFETY: `attr` is a live `struct landlock_ruleset_attr` of the size passed, which the
    // kernel reads and does not keep.
    owned(unsafe {
        raw::syscall3(nr::LANDLOCK_CREATE_RULESET, core::ptr::from_ref(attr) as usize, size_of::<RulesetAttr>(), 0)
    })
}

/// `landlock_add_rule(LANDLOCK_RULE_PATH_BENEATH)`: `rule.allowed_access` granted beneath
/// `rule.parent_fd`. A file (not a directory) takes only a file's rights, others are
/// [`Errno::Inval`].
#[blaze::noalloc]
#[blaze::nopanic]
pub fn landlock_add_rule_path(ruleset: Fd, rule: &PathBeneath) -> Result<(), Errno> {
    // SAFETY: `rule` is a live packed `struct landlock_path_beneath_attr` the kernel reads and
    // does not keep.
    raw::check(unsafe {
        raw::syscall4(nr::LANDLOCK_ADD_RULE, arg(ruleset), RULE_PATH_BENEATH, core::ptr::from_ref(rule) as usize, 0)
    })
    .map(|_| ())
}

/// `landlock_add_rule(LANDLOCK_RULE_NET_PORT)`: `rule.allowed_access` granted on one port
/// (ABI 4).
#[blaze::noalloc]
#[blaze::nopanic]
pub fn landlock_add_rule_port(ruleset: Fd, rule: &NetPort) -> Result<(), Errno> {
    // SAFETY: `rule` is a live `struct landlock_net_port_attr` the kernel reads and does not
    // keep.
    raw::check(unsafe {
        raw::syscall4(nr::LANDLOCK_ADD_RULE, arg(ruleset), RULE_NET_PORT, core::ptr::from_ref(rule) as usize, 0)
    })
    .map(|_| ())
}

/// `landlock_restrict_self`: this thread (and what it starts after) confined to the ruleset,
/// for good. Needs `prctl(NoNewPrivs)` first ([`Errno::Perm`] without it). Called in the child
/// between `clone3` and `execveat`, where only system calls run.
#[blaze::noalloc]
#[blaze::nopanic]
pub fn landlock_restrict_self(ruleset: Fd) -> Result<(), Errno> {
    // SAFETY: the call takes numbers and no memory.
    raw::check(unsafe { raw::syscall2(nr::LANDLOCK_RESTRICT_SELF, arg(ruleset), 0) }).map(|_| ())
}
