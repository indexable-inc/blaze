//! The Linux test sandbox (test.md, piece 9): what one test's launcher may touch, made into a
//! Landlock ruleset, a seccomp filter and rlimits (blaze-sys `linux::confine`) in the parent,
//! and entered by the child between its clone and its execve (blaze-core spawn.rs,
//! `confine::enter`): a refusal at any step names it and the program never runs (rule 9).
//!
//! It mirrors the macOS seatbelt profile (blaze-run child.rs `seatbelt`, test_drive.rs
//! `with_native`):
//!
//! - read: everything, as seatbelt's `(allow file-read*)`: one rule on `/` beneath;
//! - write: the test's temp folder and `/dev/null` only;
//! - run: the launcher, the system set (the loader and the system libraries, the hermetic
//!   children's set: bstd hermetic/landlock.rs `SYSTEM`) and the suite's native image, each
//!   its own rule;
//! - fork and exec allowed (`processes: true`), as seatbelt's `process-fork`/`process-exec`;
//! - no network, ptrace, mount or namespace, io_uring or keys (blaze-sys
//!   `linux::confine::build`'s denied set): stricter than the seatbelt profile's localhost
//!   allowance, exactly as the hermetic children's (build scripts) is;
//! - CPU and address-space limits held by the kernel (rlimits), core dumps off.
//!
//! A path the host lacks is refused by name (`openat`), as in the hermetic prepare.

use blaze_sys::{
    Path,
    linux::{
        OwnedFd,
        AccessFs, AccessNet, CWD, OpenFlags, PathBeneath, RulesetAttr, SockFilter,
        confine::{self, ChildConfine, Filter, MOST_FILTER, Resource, Rlimit},
        landlock_abi, landlock_add_rule_path, landlock_create_ruleset, openat,
    },
};
use core::ffi::CStr;
use std::time::Duration;

use crate::test_drive::OneError;

/// The loader and the system libraries: read whole, and run (the hermetic children's set,
/// bstd hermetic/landlock.rs `SYSTEM`; each path is one every supported host has).
#[cfg(target_arch = "x86_64")]
const SYSTEM: [&str; 4] = ["/usr/lib", "/lib", "/lib64", "/etc/ld.so.cache"];
#[cfg(not(target_arch = "x86_64"))]
const SYSTEM: [&str; 3] = ["/usr/lib", "/lib", "/etc/ld.so.cache"];

/// The oldest Landlock ABI that confines a test here (the hermetic floor).
const LEAST_ABI: u32 = 3;

/// `LANDLOCK_SCOPE_ABSTRACT_UNIX_SOCKET | LANDLOCK_SCOPE_SIGNAL` (ABI 6).
const SCOPES: u64 = 0b11;

/// A test child's address-space limit: the hermetic children's (blaze-proc-macro
/// native_bridge.rs).
const MEMORY: u64 = 16 << 30;

/// The rights every ruleset handles: everything of ABI 1, refer (2) and truncate (3).
fn handled() -> AccessFs {
    AccessFs::ALL_ABI1.with(AccessFs::REFER).with(AccessFs::TRUNCATE)
}

/// The rights a rule on a non-folder may carry (the kernel refuses folder rights on a file).
fn file_rights() -> AccessFs {
    AccessFs::EXECUTE.with(AccessFs::WRITE_FILE).with(AccessFs::READ_FILE).with(AccessFs::TRUNCATE)
}

/// The rights of a write rule: everything handled but running a file and making devices (the
/// hermetic `Access::Write`).
fn write_rights() -> AccessFs {
    AccessFs::from_bits(handled().bits() & !(AccessFs::EXECUTE.with(AccessFs::MAKE_CHAR).with(AccessFs::MAKE_BLOCK).bits()))
}

/// A test child's confinement, made in the parent (the seatbelt profile's mirror).
#[derive(Debug)]
pub(crate) struct Prepared {
    ruleset: OwnedFd,
    filter: [SockFilter; MOST_FILTER],
    filter_len: usize,
    limits: [(Resource, Rlimit); confine::MOST_LIMITS],
}

impl Prepared {
    /// What the child enters between its clone and its execve (blaze-sys `linux::confine::enter`).
    pub(crate) fn confine(&self) -> ChildConfine<'_> {
        ChildConfine { ruleset: self.ruleset.fd(), filter: &self.filter[..self.filter_len], limits: &self.limits }
    }
}

/// One rule of the test plan: `path` (one file, or the whole folder beneath it with
/// `subtree`) with `granted` rights.
struct Rule<'p> {
    path: &'p str,
    granted: AccessFs,
    subtree: bool,
}

/// Adds `rule` to `ruleset`: the path opened `O_PATH` (as a folder, else as the file it is —
/// the kernel takes only file rights on a file), its rights masked to what its kind carries.
fn add(ruleset: &OwnedFd, rule: &Rule<'_>, name: &mut [u8; 4096]) -> Result<(), OneError> {
    let refused = |why: &str| OneError::Profile { why: Box::from(why) };
    let bytes = rule.path.as_bytes();
    if bytes.len() + 1 > name.len() {
        return Err(refused("a test sandbox path is over 4095 bytes"));
    }
    name[..bytes.len()].copy_from_slice(bytes);
    name[bytes.len()] = 0;
    let path = CStr::from_bytes_with_nul(&name[..=bytes.len()])
        .map_err(|_| refused("a test sandbox path holds a NUL byte"))?;
    let flags = OpenFlags::PATH.with(OpenFlags::CLOSE_ON_EXEC);
    let (fd, folder) = match openat(CWD, Path::new(path), flags.with(OpenFlags::DIRECTORY), 0) {
        Ok(fd) => (fd, true),
        Err(blaze_sys::Errno::NotDir) => (
            openat(CWD, Path::new(path), flags, 0)
                .map_err(|_| refused("a test sandbox path cannot be opened (O_PATH)"))?,
            false,
        ),
        Err(_) => return Err(refused("a test sandbox path cannot be opened (O_PATH)")),
    };
    let granted = if folder && rule.subtree { rule.granted } else { AccessFs::from_bits(rule.granted.bits() & file_rights().bits()) };
    let beneath = PathBeneath { allowed_access: granted.bits(), parent_fd: fd.fd().raw() };
    landlock_add_rule_path(ruleset.fd(), &beneath)
        .map_err(|_| refused("landlock_add_rule refused a test sandbox rule"))
}

/// The confinement of one test's launcher: its temp folder `temp` writable, the records
/// `store` and the suite's `native` image (the store's file, by its hex name) mapped
/// executable, the `launcher` run, everything read, under `cpu` seconds of CPU.
pub(crate) fn prepare(
    temp: &str,
    store: &str,
    launcher: &str,
    native: Option<&str>,
    cpu: Duration,
) -> Result<Prepared, OneError> {
    let refused = |why: &str| OneError::Profile { why: Box::from(why) };
    if !temp.starts_with('/') || !store.starts_with('/') || !launcher.starts_with('/') {
        return Err(refused("a test sandbox path is not absolute"));
    }
    let abi = landlock_abi().map_err(|_| refused("landlock_create_ruleset (version) failed: this kernel makes no Landlock ruleset"))?;
    if abi < LEAST_ABI {
        return Err(refused("this kernel's Landlock is older than ABI 3: a test sandbox cannot be made"));
    }
    let attr = RulesetAttr {
        handled_access_fs: handled().bits(),
        // Handled and never granted: no TCP of any family (the seccomp filter refuses
        // `socket` whole; this is the kernel's second lock on the same door).
        handled_access_net: if abi >= 4 { AccessNet::BIND_TCP.with(AccessNet::CONNECT_TCP).bits() } else { 0 },
        scoped: if abi >= 6 { SCOPES } else { 0 },
    };
    let ruleset = landlock_create_ruleset(&attr).map_err(|_| refused("landlock_create_ruleset failed"))?;
    let mut name = [0u8; 4096];
    // Read everything (seatbelt's `file-read*`); the write, run and system rules follow.
    add(&ruleset, &Rule { path: "/", granted: AccessFs::READ_FILE.with(AccessFs::READ_DIR), subtree: true }, &mut name)?;
    add(&ruleset, &Rule { path: temp, granted: write_rights(), subtree: true }, &mut name)?;
    add(&ruleset, &Rule { path: "/dev/null", granted: AccessFs::WRITE_FILE.with(AccessFs::READ_FILE).with(AccessFs::TRUNCATE), subtree: false }, &mut name)?;
    for system in SYSTEM {
        add(&ruleset, &Rule { path: system, granted: AccessFs::READ_FILE.with(AccessFs::READ_DIR).with(AccessFs::EXECUTE), subtree: true }, &mut name)?;
    }
    add(&ruleset, &Rule { path: launcher, granted: AccessFs::EXECUTE.with(AccessFs::READ_FILE), subtree: false }, &mut name)?;
    if let Some(hex) = native {
        if !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(refused("the native image's name is no hex string"));
        }
        let mut image = [0u8; 4096];
        let store_len = store.len();
        if store_len + 1 + hex.len() + 1 > image.len() {
            return Err(refused("the native image's path is over 4095 bytes"));
        }
        image[..store_len].copy_from_slice(store.as_bytes());
        image[store_len] = b'/';
        image[store_len + 1..store_len + 1 + hex.len()].copy_from_slice(hex.as_bytes());
        let end = store_len + 1 + hex.len();
        let image = core::str::from_utf8(&image[..end]).map_err(|_| refused("the native image's path is no UTF-8"))?;
        add(&ruleset, &Rule { path: image, granted: AccessFs::EXECUTE.with(AccessFs::READ_FILE), subtree: false }, &mut name)?;
    }
    let mut filter = [SockFilter { code: 0, jt: 0, jf: 0, k: 0 }; MOST_FILTER];
    let filter_len = confine::build(Filter { processes: true }, &mut filter).len();
    let seconds = cpu.as_secs() + u64::from(cpu.subsec_nanos() > 0);
    let limits = [
        (Resource::Cpu, Rlimit { soft: seconds.max(1), hard: seconds.max(1) + 1 }),
        (Resource::AddressSpace, Rlimit { soft: MEMORY, hard: MEMORY }),
        (Resource::Core, Rlimit { soft: 0, hard: 0 }),
    ];
    Ok(Prepared { ruleset, filter, filter_len, limits })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The confinement of a test is made for real on this host: a ruleset the kernel takes,
    /// the filter built, the limits set; a relative path is refused by name before any call.
    #[test]
    fn a_tests_confinement_is_made_and_a_relative_path_is_refused() {
        let prepared = prepare("/tmp", "/tmp", "/bin/true", None, Duration::from_secs(5)).ok().expect("a confinement");
        let confine = prepared.confine();
        assert!(confine.filter.len() > 4, "the filter: {} instructions", confine.filter.len());
        assert_eq!(confine.limits.len(), 3);
        let error = prepare("tmp", "/tmp", "/bin/true", None, Duration::from_secs(5)).unwrap_err();
        assert!(error.to_string().contains("not absolute"), "{error}");
    }

    /// The launcher and the native image are rules of the one ruleset: a native name that is
    /// no hex is refused by name.
    #[test]
    fn a_native_image_that_is_no_hex_is_refused() {
        let error = prepare("/tmp", "/tmp", "/bin/true", Some("not hex!"), Duration::from_secs(5)).unwrap_err();
        assert!(error.to_string().contains("no hex"), "{error}");
    }
}
