//! `<IX_PROF>/http.endpoint`: where a running program's `/debug/pprof` is, its address and its
//! token in one file written whole (one rename), so a reader never pairs a new token with an
//! old address. And the checks both sides make on that directory and file.
//!
//! The threat is another local user. Whoever owns a directory, or can write to it, can rename
//! and replace any file in it (sticky bit or not), so in a directory someone else made first
//! (`mkdir -m 0777 /tmp/prof`) they could put their own port in the address and collect the
//! token from the next client. So the directory must be this user's own, not a symbolic link,
//! with owner-only mode and no ACL; the file regular, this user's, mode 0600, with no ACL and at
//! most [`MAX_FILE`] bytes; and the address a loopback one. An ACL is refused because it grants
//! access the mode bits never show: on macOS, whoever owns a parent directory can put
//! inheritable entries on it, and a 0700 directory made under it, and the 0600 files made in
//! that, inherit them. And the directory must be on a local volume that keeps ownership and
//! that this user or root mounted: on a macOS volume that ignores ownership, every local user
//! counts as the owner of every file; whoever mounted a volume can read all of it (on macOS a
//! disk image's owner re-attaches it with ownership ignored); on a network file system (NFS,
//! SMB, AFS, Coda) the server grants access by rules neither the mode nor a local ACL shows; and
//! a Linux FUSE daemon makes up the owner, mode and ACL the checks read, so FUSE is trusted only
//! when this user or root mounted it, as `/proc/self/mountinfo` shows (and this user's own only
//! as root or in the initial user namespace, since `user_id` is shown in the mounter's), and
//! never when root mounted a network store through it (sshfs, rclone, s3fs, ...). ix-prof never
//! changes the mode or ACL of a directory it did not make: it refuses it, naming the rule. On
//! macOS it does set the directory's owner to this user, which is who macOS already shows: that
//! turns a directory stored as uid 99 (made while ownership was ignored, shown as owned by
//! whoever asks) into one really this user's before any file goes in. Every file operation goes
//! through the directory's descriptor,
//! opened without following a link at its last component (trailing `/` and `.` stripped first)
//! and checked once, so a path swapped after the check is never followed. Links in the parent
//! components are followed; what holds the token safe is the opened directory's owner, mode and
//! ACL.

use std::{
    net::SocketAddr,
    path::{Path, PathBuf},
};

/// The endpoint's file in the program's `IX_PROF` directory.
pub const FILE: &str = "http.endpoint";
/// The most a token or endpoint file may hold. A loopback address and a token fit in 55.
pub const MAX_FILE: usize = 64;

/// A running program's `/debug/pprof`: its address, always loopback, and its token.
#[derive(Clone, PartialEq, Eq)]
pub struct Endpoint {
    pub addr: SocketAddr,
    pub token: String,
}

/// Leaves the token out, so an `Endpoint` in an error or a log never carries it.
impl std::fmt::Debug for Endpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Endpoint").field("addr", &self.addr).field("token", &"<redacted>").finish()
    }
}

impl Endpoint {
    /// The file's bytes: the address on one line, the token on the next.
    pub fn encode(&self) -> String {
        format!("{}\n{}\n", self.addr, self.token)
    }

    /// Reads [`Endpoint::encode`]'s bytes from `path`, refusing an address that is not loopback
    /// (the token would go to another host) and a token that is not one header-safe word.
    pub fn parse(bytes: &[u8], path: &Path) -> Result<Endpoint, String> {
        let shown = path.display();
        let text =
            std::str::from_utf8(bytes).map_err(|_| format!("{shown} is not UTF-8: refused"))?;
        let mut lines = text.lines();
        let (Some(addr), Some(token), None) = (lines.next(), lines.next(), lines.next()) else {
            return Err(format!("{shown} is not an address line and a token line: refused"));
        };
        let addr = loopback(addr.trim()).map_err(|why| format!("{shown}: {why}"))?;
        Ok(Endpoint { addr, token: header_word(token, path)? })
    }
}

/// `host_port` as the address a token may go to: an IP address and port (no host name, which
/// a resolver could point elsewhere) on this machine's loopback. Both sides of `blaze pprof`
/// use it: the address in [`FILE`], and the source of `--token-file`.
pub fn loopback(host_port: &str) -> Result<SocketAddr, String> {
    let addr: SocketAddr =
        host_port.parse().map_err(|_| format!("{host_port:?} is not an IP address and port: refused (the token goes only to a loopback address, given as one)"))?;
    if !addr.ip().is_loopback() {
        return Err(format!(
            "{addr} is not a loopback address: refused (the token goes to this machine's loopback only, never to another host)"
        ));
    }
    Ok(addr)
}

/// `token`, trimmed, if it can go in an `Authorization` header as it is: visible ASCII, no
/// space or line break (which would add headers of its own), not empty.
fn header_word(token: &str, path: &Path) -> Result<String, String> {
    let token = token.trim();
    if token.is_empty() {
        return Err(format!("{} holds no token: refused", path.display()));
    }
    if !token.bytes().all(|b| b.is_ascii_graphic()) {
        return Err(format!(
            "the token in {} has a character other than visible ASCII: refused (it goes in a header as it is)",
            path.display()
        ));
    }
    Ok(token.to_owned())
}

/// The endpoint of the program whose `IX_PROF` is `dir`: [`FILE`] in it, read through
/// [`PrivateDir`]'s checks. `Ok(None)` when there is no such file (the program is not serving,
/// or has stopped).
pub fn read(dir: &Path) -> Result<Option<Endpoint>, String> {
    let path = dir.join(FILE);
    match PrivateDir::open(dir)?.read_small(FILE)? {
        Some(bytes) => Endpoint::parse(&bytes, &path).map(Some),
        None => Ok(None),
    }
}

/// The token in `file` (`blaze pprof --token-file`), read with the same checks as [`FILE`]:
/// its directory this user's own, owner-only mode and no ACL, the file regular, this user's,
/// 0600, no ACL and at most [`MAX_FILE`] bytes.
pub fn read_token_file(file: &Path) -> Result<String, String> {
    let (Some(name), parent) = (file.file_name(), file.parent()) else {
        return Err(format!("{} names no file", file.display()));
    };
    let parent = parent.filter(|it| !it.as_os_str().is_empty()).unwrap_or(Path::new("."));
    let name = name.to_str().ok_or_else(|| format!("{} is not UTF-8", file.display()))?;
    let bytes = PrivateDir::open(parent)?
        .read_small(name)?
        .ok_or_else(|| format!("{}: no such file", file.display()))?;
    let text = std::str::from_utf8(&bytes)
        .map_err(|_| format!("{} is not UTF-8: refused", file.display()))?;
    header_word(text, file)
}

/// A directory opened without following a link at its last component and checked to be this
/// user's own, with owner-only mode and no ACL; files are read, written and removed through its
/// descriptor.
pub struct PrivateDir {
    #[cfg(unix)]
    fd: std::os::fd::OwnedFd,
    path: PathBuf,
    #[cfg(not(unix))]
    never: std::convert::Infallible,
}

#[cfg(unix)]
mod unix {
    use std::{
        ffi::CString,
        io::{Read as _, Write as _},
        os::fd::{AsRawFd as _, FromRawFd as _, OwnedFd, RawFd},
        path::{Path, PathBuf},
    };

    use super::{MAX_FILE, PrivateDir};

    fn c(s: &[u8], what: &Path) -> Result<CString, String> {
        CString::new(s).map_err(|_| format!("{} has a NUL byte", what.display()))
    }

    fn fstat(fd: RawFd) -> std::io::Result<libc::stat> {
        // SAFETY: `stat` is plain old data; all zeroes is a valid value.
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        // SAFETY: `st` is a valid `stat` to write; `fd` is the caller's open descriptor.
        if unsafe { libc::fstat(fd, &mut st) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(st)
    }

    fn me() -> u32 {
        // SAFETY: geteuid cannot fail.
        unsafe { libc::geteuid() }
    }

    fn kind(mode: u32) -> &'static str {
        match mode & libc::S_IFMT as u32 {
            m if m == libc::S_IFDIR as u32 => "a directory",
            m if m == libc::S_IFREG as u32 => "a regular file",
            m if m == libc::S_IFLNK as u32 => "a symbolic link",
            m if m == libc::S_IFIFO as u32 => "a FIFO",
            m if m == libc::S_IFSOCK as u32 => "a socket",
            m if m == libc::S_IFCHR as u32 || m == libc::S_IFBLK as u32 => "a device",
            _ => "not a regular file",
        }
    }

    /// The ACL the object open at `fd` has, described, if any. macOS checks an extended ACL
    /// before the mode bits and `fstat` never shows one, so any entry, inherited or set (an
    /// inheritable entry on a directory included), is reported.
    #[cfg(target_os = "macos")]
    fn acl(fd: RawFd, _dir: bool) -> std::io::Result<Option<&'static str>> {
        use std::ffi::{c_int, c_void};
        // <sys/acl.h>
        const ACL_TYPE_EXTENDED: c_int = 0x100;
        const ACL_FIRST_ENTRY: c_int = 0;
        unsafe extern "C" {
            fn acl_get_fd_np(fd: c_int, ty: c_int) -> *mut c_void;
            fn acl_get_entry(acl: *mut c_void, entry_id: c_int, entry: *mut *mut c_void) -> c_int;
            fn acl_free(obj: *mut c_void) -> c_int;
        }
        // SAFETY: `fd` is the caller's open descriptor; the result, if any, is freed below.
        let acl = unsafe { acl_get_fd_np(fd, ACL_TYPE_EXTENDED) };
        if acl.is_null() {
            let why = std::io::Error::last_os_error();
            return match why.raw_os_error() {
                // No ACL; or a file system that cannot hold one, so none grants anything.
                Some(libc::ENOENT | libc::ENOTSUP) => Ok(None),
                _ => Err(why),
            };
        }
        let mut entry = std::ptr::null_mut();
        // SAFETY: `acl` is the live ACL just returned; `entry` is a valid place to write.
        let first = unsafe { acl_get_entry(acl, ACL_FIRST_ENTRY, &mut entry) };
        // SAFETY: `acl` came from `acl_get_fd_np` and is freed once.
        unsafe { acl_free(acl) };
        Ok((first == 0).then_some("an ACL"))
    }

    /// The POSIX ACL the object open at `fd` has, described, if any: an access ACL, or on a
    /// directory (`dir`) a default one, which every file made in it inherits. The kernel keeps
    /// `system.posix_acl_access` only for an ACL with entries beyond the mode bits. (The ACL
    /// mask is what the group mode bits show, so the mode check already voids an access ACL's
    /// named entries; this is defense in depth, and refusing a default ACL at the directory
    /// refuses at start what every later write in it would refuse.)
    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn acl(fd: RawFd, dir: bool) -> std::io::Result<Option<&'static str>> {
        fn present(fd: RawFd, name: &std::ffi::CStr) -> std::io::Result<bool> {
            // SAFETY: `fd` is the caller's open descriptor, the name NUL-terminated; a size of 0
            // only asks whether the attribute exists and how long it is.
            if unsafe { libc::fgetxattr(fd, name.as_ptr(), std::ptr::null_mut(), 0) } >= 0 {
                return Ok(true);
            }
            let why = std::io::Error::last_os_error();
            match why.raw_os_error() {
                // No ACL; or a file system that cannot hold one, so none grants anything.
                Some(libc::ENODATA | libc::EOPNOTSUPP) => Ok(false),
                _ => Err(why),
            }
        }
        if present(fd, c"system.posix_acl_access")? {
            return Ok(Some("an ACL"));
        }
        if dir && present(fd, c"system.posix_acl_default")? {
            return Ok(Some("a default ACL, which every file made in it inherits"));
        }
        Ok(None)
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "android")))]
    fn acl(_fd: RawFd, _dir: bool) -> std::io::Result<Option<&'static str>> {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            format!("ix-prof cannot read an ACL on {}", std::env::consts::OS),
        ))
    }

    /// What [`refuse_volume`] makes of a macOS volume's `fstatfs` flags and mounting user, kept
    /// apart from the call so a test can give it a volume another user mounted.
    #[cfg(target_os = "macos")]
    #[derive(Debug, PartialEq, Eq)]
    pub(super) enum Volume {
        Trusted,
        IgnoresOwnership,
        Network,
        /// Mounted by this uid, neither this user nor root.
        MountedBy(u32),
    }

    /// The rule for a macOS volume: it keeps ownership, is local, and was mounted by this user
    /// (`me`) or root. Whoever mounted a volume can read every file on it whatever its mode: a
    /// disk image's owner can re-attach it with ownership ignored, or read the image itself.
    /// This is what refuses another user's image holding an on-disk uid-99 directory, which xnu
    /// shows as owned by whoever asks (unless root asks) on every volume, so the owner check
    /// passes. A uid-99 directory on a volume this user mounted can't be detected without root,
    /// but it can be fixed without it: [`PrivateDir::open`] sets its owner to this user.
    #[cfg(target_os = "macos")]
    pub(super) fn volume_verdict(flags: u32, owner: u32, me: u32) -> Volume {
        if flags & libc::MNT_IGNORE_OWNERSHIP as u32 != 0 {
            Volume::IgnoresOwnership
        } else if flags & libc::MNT_LOCAL as u32 == 0 {
            Volume::Network
        } else if owner != me && owner != 0 {
            Volume::MountedBy(owner)
        } else {
            Volume::Trusted
        }
    }

    /// Refuses the directory open at `fd` if its volume does not keep ownership the checks can
    /// trust. On macOS ([`volume_verdict`]): a volume mounted with ownership ignored (Finder's
    /// "Ignore ownership on this volume", `hdiutil attach -owners off`, `diskutil
    /// disableOwnership`), where `fstat` shows every caller as the owner and xnu gives every
    /// caller the owner's bits; a volume that is not local (NFS, SMB, AFP, WebDAV), whose
    /// server decides access; and a volume another user mounted (`f_owner`, of the descriptor's
    /// own volume), who can read it all.
    #[cfg(target_os = "macos")]
    fn refuse_volume(fd: RawFd, shown: &std::path::Display<'_>) -> Result<(), String> {
        // SAFETY: `statfs` is plain old data; all zeroes is a valid value.
        let mut sf: libc::statfs = unsafe { std::mem::zeroed() };
        // SAFETY: `fd` is the caller's open descriptor and `sf` a valid `statfs` to write.
        if unsafe { libc::fstatfs(fd, &mut sf) } != 0 {
            return Err(format!(
                "checking the volume of {shown}: {}: refused",
                std::io::Error::last_os_error()
            ));
        }
        let text = |chars: &[libc::c_char]| {
            let bytes: Vec<u8> =
                chars.iter().take_while(|it| **it != 0).map(|it| *it as u8).collect(); // alloc: an error's text
            String::from_utf8_lossy(&bytes).into_owned()
        };
        let on = || text(&sf.f_mntonname);
        match volume_verdict(sf.f_flags, sf.f_owner, me()) {
            Volume::Trusted => Ok(()),
            Volume::IgnoresOwnership => Err(format!(
                "{shown} is on a volume that ignores ownership ({}): refused (on it every local user counts as the owner of every file, so owner-only mode protects nothing and another user could read the token or plant their own port; `diskutil enableOwnership {}`, then make a new directory, since one made while ownership was ignored stays owned by every user (uid 99, which only `sudo ls -ln` shows); or use a directory on another volume)",
                on(),
                on()
            )),
            Volume::Network => Err(format!(
                "{shown} is on a network volume ({} at {}): refused (its server decides who can read the token, by rules neither the mode nor a local ACL shows; use a directory on a local disk)",
                text(&sf.f_fstypename),
                on()
            )),
            Volume::MountedBy(owner) => Err(format!(
                "{shown} is on a volume mounted by uid {owner} ({} at {}), neither this user (uid {}) nor root: refused (whoever mounted a volume can read every file on it whatever its mode, e.g. by re-attaching its disk image with ownership ignored or reading the image; use a directory on a volume you or the system mounted)",
                text(&sf.f_fstypename),
                on(),
                me()
            )),
        }
    }

    /// Refuses the directory open at `fd` if its file system lets someone other than this user
    /// decide access, by rules neither the mode nor a local ACL shows:
    /// - a network file system: NFS and SMB/CIFS servers can grant access by their own ACLs
    ///   (NFSv4's, CIFS's; NFSv3's POSIX ACLs do show in `system.posix_acl_access`), and AFS
    ///   and Coda grant by per-directory ACLs, ignoring the group and other bits;
    /// - a FUSE mount that [`super::mountinfo::fuse_verdict`] refuses: one another user mounted
    ///   (their daemon makes up the owner, mode and ACL, and receives every byte written), or
    ///   one whose daemon decides access for other users (`allow_other` without
    ///   `default_permissions`), one whose `user_id` matches this user only as seen from
    ///   another user namespace, or a network store root mounted. The mount is found by the descriptor's mount ID
    ///   (`statx(STATX_MNT_ID)`) in `/proc/self/mountinfo`, and its device must match the
    ///   descriptor's, so a recycled mount ID is never taken for it (the descriptor holds its
    ///   own file system, and so its device number, alive).
    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn refuse_volume(fd: RawFd, shown: &std::path::Display<'_>) -> Result<(), String> {
        // <linux/magic.h>
        const NETWORK: [(u64, &str); 7] = [
            (0x6969, "NFS"),
            (0x517b, "SMB"),
            (0xff53_4d42, "CIFS"),
            (0xfe53_4d42, "SMB2"),
            (0x6b41_4653, "AFS (kAFS)"),
            (0x5346_414f, "AFS (OpenAFS)"),
            (0x7375_7245, "Coda"),
        ];
        const FUSE: u64 = 0x6573_5546;
        // SAFETY: `statfs` is plain old data; all zeroes is a valid value.
        let mut sf: libc::statfs = unsafe { std::mem::zeroed() };
        // SAFETY: `fd` is the caller's open descriptor and `sf` a valid `statfs` to write.
        if unsafe { libc::fstatfs(fd, &mut sf) } != 0 {
            return Err(format!(
                "checking the file system of {shown}: {}: refused",
                std::io::Error::last_os_error()
            ));
        }
        // `f_type`'s width and sign differ by target; the magic is its low 32 bits.
        #[allow(
            clippy::unnecessary_cast,
            reason = "f_type is c_long on some targets, c_uint on others"
        )]
        let magic = sf.f_type as u64 & 0xffff_ffff;
        if let Some((_, name)) = NETWORK.iter().find(|(m, _)| *m == magic) {
            return Err(format!(
                "{shown} is on {name}, a network file system: refused (its server can grant access by ACLs that neither the mode nor a local ACL shows; use a directory on a local disk)"
            ));
        }
        if magic != FUSE {
            return Ok(());
        }
        // SAFETY: `statx` is plain old data; all zeroes is a valid value.
        let mut stx: libc::statx = unsafe { std::mem::zeroed() };
        // SAFETY: `fd` is the caller's open descriptor, the path empty and NUL-terminated (with
        // AT_EMPTY_PATH, `fd` itself), and `stx` a valid `statx` to write.
        if unsafe {
            libc::statx(fd, c"".as_ptr(), libc::AT_EMPTY_PATH, libc::STATX_MNT_ID, &mut stx)
        } != 0
        {
            return Err(format!(
                "finding the FUSE mount of {shown} (statx STATX_MNT_ID): {}: refused (ix-prof trusts a FUSE mount only once it has checked who mounted it)",
                std::io::Error::last_os_error()
            ));
        }
        if stx.stx_mask & libc::STATX_MNT_ID == 0 {
            return Err(format!(
                "{shown} is on FUSE, and this kernel does not report a descriptor's mount (statx STATX_MNT_ID, Linux 5.8): refused (ix-prof trusts a FUSE mount only once it has checked who mounted it; use a directory on a local disk)"
            ));
        }
        let table = std::fs::read_to_string("/proc/self/mountinfo") // alloc: once per FUSE directory opened
            .map_err(|why| {
                format!(
                    "reading /proc/self/mountinfo for the FUSE mount of {shown}: {why}: refused"
                )
            })?;
        let mount = super::mountinfo::find(&table, stx.stx_mnt_id)
            .map_err(|why| format!("finding the FUSE mount of {shown}: {why}: refused"))?
            .ok_or_else(|| {
                format!(
                    "{shown} is on FUSE mount {}, which /proc/self/mountinfo does not list (a mount in another namespace, reached through /proc/<pid>/root?): refused (ix-prof trusts a FUSE mount only once it has checked who mounted it)",
                    stx.stx_mnt_id
                )
            })?;
        if mount.dev != (stx.stx_dev_major, stx.stx_dev_minor) {
            return Err(format!(
                "{shown} is on device {}:{}, but mount {} in /proc/self/mountinfo ({}) is on {}:{}: refused (the mount changed while it was checked)",
                stx.stx_dev_major,
                stx.stx_dev_minor,
                mount.id,
                mount.point,
                mount.dev.0,
                mount.dev.1
            ));
        }
        // `user_id` is shown in the mounter's user namespace, so it is comparable with this
        // process's uid only in the initial one (or as root, see `fuse_verdict`).
        let initial = me() == 0
            || {
                let map = std::fs::read_to_string("/proc/self/uid_map") // alloc: once per FUSE directory opened
                .map_err(|why| format!("reading /proc/self/uid_map for the FUSE mount of {shown}: {why}: refused (ix-prof compares a FUSE mount's user_id with this user only in the initial user namespace)"))?;
                super::mountinfo::initial_userns(&map)
            };
        super::mountinfo::fuse_verdict(mount.fstype, mount.super_options, me(), initial).map_err(|why| {
            format!(
                "{shown} is on the FUSE mount {} ({}, options {}): refused ({why}; use a directory on a local disk, or a FUSE mount of your own without allow_other)",
                mount.point, mount.fstype, mount.super_options
            )
        })
    }

    /// Elsewhere [`acl`] refuses every object, so there is nothing more to check here.
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "android")))]
    fn refuse_volume(_fd: RawFd, _shown: &std::path::Display<'_>) -> Result<(), String> {
        Ok(())
    }

    /// Refuses the object open at `fd` (shown as `shown`) if it has an ACL, naming it and the
    /// rule. ix-prof never removes an ACL: the user does, or picks another directory.
    fn refuse_acl(
        fd: RawFd,
        dir: bool,
        shown: &std::path::Display<'_>,
        mode: u32,
    ) -> Result<(), String> {
        match acl(fd, dir) {
            Ok(None) => Ok(()),
            Ok(Some(what)) => Err(format!(
                "{shown} has {what} (access control list): refused (a profile and the endpoint's token must have owner-only mode and no ACL, and an ACL can let other users in where its mode {:04o} shows none; ix-prof never removes one: see it with `ls -le` on macOS or `getfacl` on Linux, remove it with `chmod -N` or `setfacl -b`, or use a directory whose parent has no inheritable ACL)",
                mode & 0o7777
            )),
            Err(why) => Err(format!(
                "checking {shown} for an ACL (access control list): {why}: refused (a profile and the endpoint's token must have no ACL)"
            )),
        }
    }

    impl PrivateDir {
        /// Opens `path` and checks it: a directory (not a symbolic link) on a local volume that
        /// keeps ownership and that this user or root mounted ([`refuse_volume`]), owned by
        /// this user, with no group or other mode bits and no ACL. A refusal names the path,
        /// what it is and the rule.
        pub fn open(path: &Path) -> Result<PrivateDir, String> {
            use std::os::unix::ffi::OsStrExt as _;
            let shown = path.display();
            let rule = "the directory holding a profile and the endpoint's token must be this user's own: whoever owns or can write a directory can replace the files in it";
            // `link/` and `link/.` resolve the link at `link` even under O_NOFOLLOW, which
            // applies to the last component only: with trailing `/` and `.` stripped, `link` is
            // that last component.
            let last: PathBuf = path.components().collect(); // alloc: once per directory opened
            let link = std::fs::symlink_metadata(&last).map_err(|why| format!("{shown}: {why}"))?;
            if link.file_type().is_symlink() {
                return Err(format!(
                    "{shown} is a symbolic link: refused ({rule}, and a link can point into another user's)"
                ));
            }
            let cpath = c(last.as_os_str().as_bytes(), path)?;
            // SAFETY: a NUL-terminated path; the descriptor, if any, is owned below.
            let fd = unsafe {
                libc::open(
                    cpath.as_ptr(),
                    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                )
            };
            if fd < 0 {
                let why = std::io::Error::last_os_error();
                if why.raw_os_error() == Some(libc::ELOOP) {
                    return Err(format!(
                        "{shown} is a symbolic link: refused ({rule}, and a link can point into another user's)"
                    ));
                }
                return Err(format!("opening the directory {shown}: {why}"));
            }
            // SAFETY: `fd` was just opened and nothing else owns it.
            let fd = unsafe { OwnedFd::from_raw_fd(fd) };
            let st = fstat(fd.as_raw_fd()).map_err(|why| format!("{shown}: {why}"))?;
            let mode = st.st_mode as u32;
            if mode & libc::S_IFMT as u32 != libc::S_IFDIR as u32 {
                return Err(format!("{shown} is {}, not a directory: refused", kind(mode)));
            }
            // First: on such a volume the owner and mode below mean nothing.
            refuse_volume(fd.as_raw_fd(), &shown)?;
            if st.st_uid != me() {
                return Err(format!(
                    "{shown} belongs to uid {}, not this user (uid {}): refused ({rule}; use a directory of your own, e.g. under your home or target/)",
                    st.st_uid,
                    me()
                ));
            }
            // macOS shows a directory stored as uid 99 (made while its volume ignored ownership)
            // as owned by whoever asks, so the check above passes for every user. Setting the
            // owner to this user, who is the owner macOS shows, needs no privilege (xnu compares
            // against the mapped owner) and stores the real uid, so from here on the directory
            // is this user's alone. For a directory already this user's it changes nothing.
            #[cfg(target_os = "macos")]
            {
                // SAFETY: `fd` is an open descriptor; a gid of -1 leaves the group as it is.
                if unsafe { libc::fchown(fd.as_raw_fd(), me(), libc::gid_t::MAX) } != 0 {
                    return Err(format!(
                        "{shown}: setting its owner to this user (uid {}): {}: refused (macOS shows a directory stored as uid 99, which every user can write, as owned by whoever asks, so ix-prof makes it this user's for real before writing in it; use a directory of your own on a volume that keeps ownership)",
                        me(),
                        std::io::Error::last_os_error()
                    ));
                }
            }
            if mode & 0o077 != 0 {
                return Err(format!(
                    "{shown} has mode {:04o}, open to group or other: refused ({rule}; `chmod 700 {shown}`, or name a new directory, which ix-prof makes 0700)",
                    mode & 0o7777
                ));
            }
            refuse_acl(fd.as_raw_fd(), true, &shown, mode)?;
            Ok(PrivateDir { fd, path: path.to_owned() })
        }

        fn at(&self, name: &str) -> std::path::PathBuf {
            self.path.join(name)
        }

        /// `name` in this directory, if there: a regular file (never followed through a link,
        /// and never a FIFO or device, so the read cannot block or run on) owned by this user,
        /// mode 0600, no ACL, at most [`MAX_FILE`] bytes.
        pub fn read_small(&self, name: &str) -> Result<Option<Vec<u8>>, String> {
            let path = self.at(name);
            let shown = path.display();
            let cname = c(name.as_bytes(), &path)?;
            // SAFETY: `self.fd` is an open directory and `cname` NUL-terminated.
            let fd = unsafe {
                libc::openat(
                    self.fd.as_raw_fd(),
                    cname.as_ptr(),
                    libc::O_RDONLY
                        | libc::O_NOFOLLOW
                        | libc::O_NONBLOCK
                        | libc::O_NOCTTY
                        | libc::O_CLOEXEC,
                )
            };
            if fd < 0 {
                let why = std::io::Error::last_os_error();
                return match why.raw_os_error() {
                    Some(libc::ENOENT) => Ok(None),
                    Some(libc::ELOOP) => Err(format!(
                        "{shown} is a symbolic link: refused (it is read only as this user's own regular file)"
                    )),
                    _ => Err(format!("opening {shown}: {why}")),
                };
            }
            // SAFETY: `fd` was just opened and nothing else owns it.
            let file = std::fs::File::from(unsafe { OwnedFd::from_raw_fd(fd) });
            let st = fstat(file.as_raw_fd()).map_err(|why| format!("{shown}: {why}"))?;
            let mode = st.st_mode as u32;
            if mode & libc::S_IFMT as u32 != libc::S_IFREG as u32 {
                return Err(format!(
                    "{shown} is {}: refused (it is read only as a regular file)",
                    kind(mode)
                ));
            }
            if st.st_uid != me() {
                return Err(format!(
                    "{shown} belongs to uid {}, not this user (uid {}): refused",
                    st.st_uid,
                    me()
                ));
            }
            if mode & 0o777 != 0o600 {
                return Err(format!(
                    "{shown} has mode {:04o}, not 0600: refused (a token must be readable by this user only)",
                    mode & 0o7777
                ));
            }
            refuse_acl(file.as_raw_fd(), false, &shown, mode)?;
            if st.st_size as u64 > MAX_FILE as u64 {
                return Err(format!("{shown} is {} bytes, over {MAX_FILE}: refused", st.st_size));
            }
            let mut bytes = Vec::with_capacity(MAX_FILE);
            (&file)
                .take(MAX_FILE as u64 + 1)
                .read_to_end(&mut bytes)
                .map_err(|why| format!("reading {shown}: {why}"))?;
            if bytes.len() > MAX_FILE {
                return Err(format!("{shown} grew past {MAX_FILE} bytes while read: refused"));
            }
            Ok(Some(bytes))
        }

        /// Writes `name` whole: `part` made new (never an existing file or a link), mode 0600,
        /// written, then renamed over `name`, so a reader sees the old file or the new one.
        pub fn write_whole(&self, name: &str, part: &str, bytes: &[u8]) -> Result<(), String> {
            let path = self.at(name);
            let (cname, cpart) = (c(name.as_bytes(), &path)?, c(part.as_bytes(), &path)?);
            let dir = self.fd.as_raw_fd();
            // SAFETY: an open directory and NUL-terminated names.
            let fd = unsafe {
                libc::openat(
                    dir,
                    cpart.as_ptr(),
                    libc::O_WRONLY
                        | libc::O_CREAT
                        | libc::O_EXCL
                        | libc::O_NOFOLLOW
                        | libc::O_CLOEXEC,
                    0o600 as libc::c_uint,
                )
            };
            if fd < 0 {
                return Err(format!(
                    "making {}: {}",
                    self.at(part).display(),
                    std::io::Error::last_os_error()
                ));
            }
            // SAFETY: `fd` was just opened and nothing else owns it.
            let mut file = std::fs::File::from(unsafe { OwnedFd::from_raw_fd(fd) });
            let written = (|| -> std::io::Result<()> {
                // Exactly 0600 under any umask: a reader refuses any other mode.
                // SAFETY: an open descriptor.
                if unsafe { libc::fchmod(file.as_raw_fd(), 0o600) } != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                // The directory was checked to have no ACL, so the part file inherits none; the
                // file is checked too before any byte goes in, so a token is never written
                // where another user could read it.
                if acl(file.as_raw_fd(), false)?.is_some() {
                    return Err(std::io::Error::other(
                        "it was made with an ACL (access control list): refused (the token must have no ACL)",
                    ));
                }
                file.write_all(bytes)?;
                // SAFETY: an open directory and NUL-terminated names.
                if unsafe { libc::renameat(dir, cpart.as_ptr(), dir, cname.as_ptr()) } != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            })();
            written.map_err(|why| {
                // SAFETY: an open directory and a NUL-terminated name; a part file we made.
                unsafe { libc::unlinkat(dir, cpart.as_ptr(), 0) };
                format!("writing {}: {why}", path.display())
            })
        }

        /// Removes `name` from this directory.
        pub fn remove(&self, name: &str) -> Result<(), String> {
            let path = self.at(name);
            let cname = c(name.as_bytes(), &path)?;
            // SAFETY: an open directory and a NUL-terminated name.
            if unsafe { libc::unlinkat(self.fd.as_raw_fd(), cname.as_ptr(), 0) } != 0 {
                return Err(format!(
                    "removing {}: {}",
                    path.display(),
                    std::io::Error::last_os_error()
                ));
            }
            Ok(())
        }
    }
}

#[cfg(not(unix))]
impl PrivateDir {
    pub fn open(path: &Path) -> Result<PrivateDir, String> {
        Err(format!(
            "{}: ix-prof checks who owns a directory by Unix owner and mode, which {} lacks: refused",
            path.display(),
            std::env::consts::OS
        ))
    }

    pub fn read_small(&self, _name: &str) -> Result<Option<Vec<u8>>, String> {
        match self.never {}
    }

    pub fn write_whole(&self, _name: &str, _part: &str, _bytes: &[u8]) -> Result<(), String> {
        match self.never {}
    }

    pub fn remove(&self, _name: &str) -> Result<(), String> {
        match self.never {}
    }
}

/// `/proc/self/mountinfo`, read as far as the Linux FUSE rule needs ([`fuse_verdict`]). Plain
/// text, so the parser and the rule are tested on every OS.
#[cfg(any(target_os = "linux", target_os = "android", test))]
pub(crate) mod mountinfo {
    /// One line of the table: `101 40 0:61 / /tmp/prof rw,relatime shared:60 - fuse.sshfs
    /// host:/ rw,user_id=1001,group_id=1001,allow_other` (proc(5)).
    #[derive(Debug, PartialEq, Eq)]
    pub(crate) struct Mount<'a> {
        pub id: u64,
        /// The device, `major:minor`.
        pub dev: (u32, u32),
        /// The mount point, its `\ooo` escapes undone.
        pub point: String,
        /// `fuse.<subtype>`, `fuseblk`, `virtiofs`, `ext4`, ...
        pub fstype: &'a str,
        /// The file system's own options, after the `-`: for FUSE, `user_id`,
        /// `default_permissions` and `allow_other`.
        pub super_options: &'a str,
    }

    /// The mount with ID `id` in `table`, or `Ok(None)` if no line has it. A line whose ID or
    /// fields do not parse is an error naming the line, never skipped.
    pub(crate) fn find(table: &str, id: u64) -> Result<Option<Mount<'_>>, String> {
        for (n, line) in table.lines().enumerate() {
            let bad = || format!("/proc/self/mountinfo line {} does not parse: {line:?}", n + 1);
            let first =
                line.split(' ').next().and_then(|it| it.parse::<u64>().ok()).ok_or_else(bad)?;
            if first == id {
                return parse(line, id).map(Some).ok_or_else(bad);
            }
        }
        Ok(None)
    }

    fn parse(line: &str, id: u64) -> Option<Mount<'_>> {
        // Fields hold no space (the kernel writes it `\040`), so ` - ` is the separator.
        let (head, tail) = line.split_once(" - ")?;
        let mut head = head.split(' ');
        let (_id, _parent, dev, _root, point, _options) =
            (head.next()?, head.next()?, head.next()?, head.next()?, head.next()?, head.next()?);
        let (major, minor) = dev.split_once(':')?;
        let mut tail = tail.split(' ');
        let (fstype, _source, super_options) = (tail.next()?, tail.next()?, tail.next()?);
        if tail.next().is_some() {
            return None;
        }
        Some(Mount {
            id,
            dev: (major.parse().ok()?, minor.parse().ok()?),
            point: unescape(point)?,
            fstype,
            super_options,
        })
    }

    /// `\ooo` octal escapes (space, tab, newline, backslash) undone.
    fn unescape(field: &str) -> Option<String> {
        let bytes = field.as_bytes();
        let mut out = Vec::with_capacity(bytes.len()); // alloc: an error's text, once per FUSE directory opened
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] == b'\\' {
                let digits = std::str::from_utf8(bytes.get(i + 1..i + 4)?).ok()?;
                out.push(u8::from_str_radix(digits, 8).ok()?);
                i += 4;
            } else {
                out.push(bytes[i]);
                i += 1;
            }
        }
        Some(String::from_utf8_lossy(&out).into_owned())
    }

    /// FUSE types whose daemon stores the bytes on a server (SSH, S3, GCS, WebDAV, ...), which
    /// then decides who can read them, as an NFS server does. One root mounted is refused; one
    /// this user mounted is their own choice of where the token goes.
    pub(crate) const NETWORK_FUSE: [&str; 7] = [
        "fuse.sshfs",
        "fuse.s3fs",
        "fuse.rclone",
        "fuse.gcsfuse",
        "fuse.goofys",
        "fuse.juicefs",
        "fuse.davfs",
    ];

    /// Whether `/proc/self/uid_map` (`uid_map`) is the initial user namespace's: exactly one
    /// line mapping every uid to itself, `0 0 4294967295` (the kernel pads the fields with
    /// spaces). Any other map, a container's included, is not.
    pub(crate) fn initial_userns(uid_map: &str) -> bool {
        let mut lines = uid_map.lines();
        let (Some(line), None) = (lines.next(), lines.next()) else {
            return false;
        };
        let mut f = line.split_whitespace();
        (f.next(), f.next(), f.next(), f.next()) == (Some("0"), Some("0"), Some("4294967295"), None)
    }

    /// Whether a FUSE mount (its mountinfo `fstype` and `super_options`) may hold this user's
    /// (`me`) token, and if not, why. A FUSE daemon makes up the owner, mode and ACL that the
    /// directory checks read, and receives every byte written, so it must be this user's own
    /// or root's:
    /// - mounted by this user, and either without `allow_other` (no one else can reach it, by
    ///   default not even root; root can when the fuse module's `allow_sys_admin_access` is
    ///   set) or with `default_permissions` (the kernel checks the mode it reports);
    /// - mounted by root with `default_permissions` (rootless containers' fuse-overlayfs),
    ///   unless its type is a network store's ([`NETWORK_FUSE`]);
    /// - virtiofs, which only root can mount and whose daemon is the host's.
    ///
    /// The kernel shows `user_id` in the user namespace of whoever mounted it, not this
    /// process's. A mount listed in this process's mountinfo need not have been made in its
    /// namespaces: a new mount namespace copies its parent's mounts, and a container's bind of
    /// `/tmp` carries a host user's FUSE mount in, its `user_id` still the host's. So `user_id ==
    /// me` counts as this user only when `me` is 0 or this process is in the initial user
    /// namespace (the `initial_userns` argument, read by [`initial_userns`]). `user_id=0` is safe in any
    /// namespace: it is the root of the mounter's namespace, which is this one or an ancestor
    /// that already controls this process.
    pub(crate) fn fuse_verdict(
        fstype: &str,
        super_options: &str,
        me: u32,
        initial_userns: bool,
    ) -> Result<(), String> {
        if fstype == "virtiofs" {
            return Ok(());
        }
        let family = fstype.split_once('.').map_or(fstype, |(it, _)| it);
        if family != "fuse" && family != "fuseblk" {
            return Err(format!("its type {fstype} is not one ix-prof knows a FUSE rule for"));
        }
        let (mut user, mut allow_other, mut default_permissions) = (None, false, false);
        for option in super_options.split(',') {
            match option.split_once('=') {
                Some(("user_id", value)) => {
                    user = Some(
                        value
                            .parse::<u32>()
                            .map_err(|_| format!("its user_id {value:?} is not a uid"))?,
                    );
                }
                _ if option == "allow_other" => allow_other = true,
                _ if option == "default_permissions" => default_permissions = true,
                _ => {}
            }
        }
        let Some(user) = user else {
            return Err("its options name no user_id, the user who mounted it".to_owned()); // alloc: an error's text
        };
        match user {
            _ if user == me && me != 0 && !initial_userns => Err(format!(
                "its user_id {user} matches this user's uid, but the kernel shows user_id in the user namespace of whoever mounted it, and this process runs in another (/proc/self/uid_map is not the identity map `0 0 4294967295`), so it may be another user's mount"
            )),
            _ if user == me && (!allow_other || default_permissions) => Ok(()),
            0 if NETWORK_FUSE.contains(&fstype) => Err(format!(
                "root mounted it as {fstype}, a network store, whose server decides who can read the token by rules neither the mode nor a local ACL shows, like NFS"
            )),
            0 if default_permissions => Ok(()),
            _ if user == me => Err(
                "it is mounted with allow_other and without default_permissions, so other users reach it through its daemon, which, not the mode, decides what they read".to_owned(), // alloc: an error's text
            ),
            0 => Err(
                "root mounted it without default_permissions, so its daemon, not the mode, decides who reads it".to_owned(), // alloc: an error's text
            ),
            _ => Err(format!(
                "uid {user} mounted it, neither this user (uid {me}) nor root, and a FUSE daemon makes up the owner, mode and ACL ix-prof checks and receives every byte written"
            )),
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::os::unix::fs::PermissionsExt as _;

    use super::{Endpoint, FILE, PrivateDir};

    struct Tmp(std::path::PathBuf);

    impl Tmp {
        fn new(what: &str, mode: u32) -> Tmp {
            let dir = std::env::temp_dir().join(format!(
                "ix-prof-endpoint-{what}-{}-{}",
                std::process::id(),
                crate::nonce()
            ));
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(mode)).unwrap();
            Tmp(dir)
        }
    }

    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = std::fs::set_permissions(&self.0, std::fs::Permissions::from_mode(0o700));
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn write(path: &std::path::Path, bytes: &[u8], mode: u32) {
        std::fs::write(path, bytes).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
    }

    const GOOD: &[u8] = b"127.0.0.1:6060\n0123456789abcdef0123456789abcdef\n";

    /// The client's half of S1: `blaze pprof --dir` and `--token-file` read only this user's
    /// own 0600 regular file, in a directory that is this user's own, closed to group and
    /// other, and not a link; and the address must be loopback.
    #[test]
    fn a_client_reads_the_endpoint_only_from_a_private_directory_and_file() {
        let dir = Tmp::new("client", 0o700);
        write(&dir.0.join(FILE), GOOD, 0o600);
        let ok = super::read(&dir.0).unwrap().expect("the file");
        assert_eq!(ok.addr.to_string(), "127.0.0.1:6060");
        assert_eq!(ok.token, "0123456789abcdef0123456789abcdef");

        let refused = |what: &str, got: Result<Option<Endpoint>, String>, says: &str| {
            let why = got.expect_err(what);
            assert!(why.contains(says), "{what}: {why}");
        };
        // The directory open to group or other.
        for mode in [0o755, 0o770, 0o707, 0o777] {
            std::fs::set_permissions(&dir.0, std::fs::Permissions::from_mode(mode)).unwrap();
            refused(&format!("dir mode {mode:o}"), super::read(&dir.0), "open to group or other");
        }
        std::fs::set_permissions(&dir.0, std::fs::Permissions::from_mode(0o700)).unwrap();
        // The directory a link to a private one.
        let link = dir.0.with_extension("link");
        std::os::unix::fs::symlink(&dir.0, &link).unwrap();
        let got = super::read(&link);
        let _ = std::fs::remove_file(&link);
        refused("a linked directory", got, "is a symbolic link");
        // The file readable by others, too long, a link, a FIFO, a directory.
        write(&dir.0.join(FILE), GOOD, 0o644);
        refused("file mode 0644", super::read(&dir.0), "not 0600");
        write(&dir.0.join(FILE), &[b'a'; 65], 0o600);
        refused("65 bytes", super::read(&dir.0), "over 64");
        std::fs::remove_file(dir.0.join(FILE)).unwrap();
        let other = dir.0.join("other");
        write(&other, GOOD, 0o600);
        std::os::unix::fs::symlink(&other, dir.0.join(FILE)).unwrap();
        refused("a linked file", super::read(&dir.0), "is a symbolic link");
        std::fs::remove_file(dir.0.join(FILE)).unwrap();
        let fifo = std::ffi::CString::new(dir.0.join(FILE).as_os_str().as_encoded_bytes()).unwrap();
        // SAFETY: a NUL-terminated path.
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
        refused("a FIFO", super::read(&dir.0), "a FIFO");
        std::fs::remove_file(dir.0.join(FILE)).unwrap();
        // A remote or unparsable address; a token that would add a header.
        write(&dir.0.join(FILE), b"10.1.2.3:6060\nabc\n", 0o600);
        refused("a remote address", super::read(&dir.0), "not a loopback address");
        write(&dir.0.join(FILE), b"evil.example:80\nabc\n", 0o600);
        refused("a host name", super::read(&dir.0), "not an IP address and port");
        write(&dir.0.join(FILE), b"127.0.0.1:1\nab\rX: y\n", 0o600);
        refused("a token with a CR", super::read(&dir.0), "visible ASCII");
        // No file: nothing, not an error.
        std::fs::remove_file(dir.0.join(FILE)).unwrap();
        assert_eq!(super::read(&dir.0), Ok(None));

        // `--token-file`: the same rules.
        let token = dir.0.join("token");
        write(&token, b"abc\n", 0o600);
        assert_eq!(super::read_token_file(&token).as_deref(), Ok("abc"));
        write(&token, b"abc\n", 0o640);
        let why = super::read_token_file(&token).unwrap_err();
        assert!(why.contains("not 0600"), "{why}");
        write(&token, b"abc\n", 0o600);
        std::fs::set_permissions(&dir.0, std::fs::Permissions::from_mode(0o711)).unwrap();
        let why = super::read_token_file(&token).unwrap_err();
        assert!(
            why.contains("open to group or other") && why.contains(&dir.0.display().to_string()),
            "{why}"
        );
    }

    /// What ix-prof writes it can read back, mode 0600, and a writer never leaves its part file.
    #[test]
    fn an_endpoint_written_whole_reads_back() {
        let dir = Tmp::new("write", 0o700);
        let e = Endpoint { addr: "[::1]:7".parse().unwrap(), token: "t0k".to_owned() };
        let d = PrivateDir::open(&dir.0).unwrap();
        d.write_whole(FILE, "x.part", e.encode().as_bytes()).unwrap();
        assert_eq!(super::read(&dir.0), Ok(Some(e.clone())));
        let names: Vec<_> =
            std::fs::read_dir(&dir.0).unwrap().map(|it| it.unwrap().file_name()).collect();
        assert_eq!(names, [FILE]);
        // M1: the token never shows in `{:?}`, which an error or a log may print.
        let shown = format!("{e:?}");
        assert!(!shown.contains("t0k") && shown.contains("[::1]:7"), "{shown}");
    }

    #[cfg(target_os = "macos")]
    fn chmod_acl(op: &str, entry: &str, path: &std::path::Path) {
        // /bin/chmod: a GNU chmod earlier on PATH has no `+a`.
        let ok = std::process::Command::new("/bin/chmod")
            .arg(op)
            .args((!entry.is_empty()).then_some(entry))
            .arg(path)
            .status()
            .unwrap();
        assert!(ok.success(), "/bin/chmod {op} {entry:?} {}", path.display());
    }

    /// S1a (macOS): whoever owns a parent directory can put inheritable ACL entries on it, and
    /// a 0700 directory made under it, and the 0600 `http.endpoint` written in that, inherit
    /// them, so its owner could read the token though the modes show nothing. Both sides refuse
    /// a directory or file with any ACL entry, naming it, and never remove the ACL. With the ACL
    /// removed by hand, the same directory and file are accepted, so the ACL is the cause.
    #[cfg(target_os = "macos")]
    #[test]
    fn a_directory_or_file_with_an_acl_is_refused() {
        let inherit =
            "everyone allow list,search,read,readattr,readsecurity,file_inherit,directory_inherit";
        let parent = Tmp::new("acl", 0o700);
        chmod_acl("+a", inherit, &parent.0);
        let run = parent.0.join("run");
        let run_s = run.display().to_string();

        // ix-prof makes `run` 0700, and it inherits the parent's entries: refused at start.
        let made = crate::private_dir(&run).map(drop);
        assert!(
            made.as_ref().is_err_and(|why| why.contains(&run_s) && why.contains("has an ACL")),
            "the server: {made:?}"
        );
        let mode = std::fs::metadata(&run).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700, "the modes show nothing");
        // The client refuses the same directory.
        let read = super::read(&run);
        assert!(
            read.as_ref().is_err_and(|why| why.contains(&run_s) && why.contains("has an ACL")),
            "the client: {read:?}"
        );
        // ix-prof did not strip it.
        assert!(PrivateDir::open(&run).is_err(), "the ACL is still there");

        // A 0600 file with an ACL, in a directory without one.
        chmod_acl("-N", "", &run);
        write(&run.join(FILE), GOOD, 0o600);
        chmod_acl("+a", "everyone allow read", &run.join(FILE));
        let file_s = run.join(FILE).display().to_string();
        let read = super::read(&run);
        assert!(
            read.as_ref().is_err_and(|why| why.contains(&file_s) && why.contains("has an ACL")),
            "the file: {read:?}"
        );
        let token = super::read_token_file(&run.join(FILE));
        assert!(
            token.as_ref().is_err_and(|why| why.contains(&file_s) && why.contains("has an ACL")),
            "--token-file: {token:?}"
        );

        // The ACL removed, both are accepted: it was the ACL that was refused.
        chmod_acl("-N", "", &run.join(FILE));
        assert!(matches!(super::read(&run), Ok(Some(_))), "{:?}", super::read(&run));
    }

    /// S1b: `link/` and `link/.` name a link as much as `link` does (`lstat` and O_NOFOLLOW
    /// would follow it there), and are refused the same way.
    #[test]
    fn a_linked_directory_is_refused_with_a_trailing_slash_too() {
        let dir = Tmp::new("slash", 0o700);
        write(&dir.0.join(FILE), GOOD, 0o600);
        let link = dir.0.with_extension("slashlink");
        std::os::unix::fs::symlink(&dir.0, &link).unwrap();
        let got: Vec<_> = ["", "/", "//", "/.", "/./"]
            .into_iter()
            .map(|tail| {
                let mut p = link.clone().into_os_string();
                p.push(tail);
                (tail, super::read(std::path::Path::new(&p)))
            })
            .collect();
        let _ = std::fs::remove_file(&link);
        for (tail, got) in got {
            assert!(
                got.as_ref().is_err_and(|why| why.contains("is a symbolic link")),
                "link{tail}: {got:?}"
            );
        }
        // The directory itself, with a trailing slash, is still fine.
        let mut p = dir.0.clone().into_os_string();
        p.push("/");
        assert!(matches!(super::read(std::path::Path::new(&p)), Ok(Some(_))));
    }

    /// A disk image attached for one test, detached and deleted on drop (a failed assertion too).
    #[cfg(target_os = "macos")]
    struct Image {
        base: std::path::PathBuf,
        mnt: std::path::PathBuf,
    }

    #[cfg(target_os = "macos")]
    impl Image {
        fn attach(what: &str, attach: &[&str]) -> Image {
            let base = std::env::temp_dir().join(format!(
                "ix-prof-endpoint-{what}-{}-{}",
                std::process::id(),
                crate::nonce()
            ));
            std::fs::create_dir_all(&base).unwrap();
            let (dmg, mnt) = (base.join("v.dmg"), base.join("mnt"));
            let made = std::process::Command::new("/usr/bin/hdiutil")
                .args(["create", "-quiet", "-size", "20m", "-fs", "APFS", "-volname", "ixprof"])
                .arg(&dmg)
                .status()
                .unwrap();
            assert!(made.success(), "hdiutil create {}", dmg.display());
            let image = Image { base, mnt };
            let attached = std::process::Command::new("/usr/bin/hdiutil")
                .args(["attach", "-quiet", "-nobrowse"])
                .args(attach)
                .arg("-mountpoint")
                .arg(&image.mnt)
                .arg(&dmg)
                .status()
                .unwrap();
            assert!(attached.success(), "hdiutil attach {attach:?} {}", dmg.display());
            image
        }

        /// Detaches the image and attaches it again with `attach`, at the same mount point.
        fn reattach(&self, attach: &[&str]) {
            let detached = std::process::Command::new("/usr/bin/hdiutil")
                .args(["detach", "-quiet"])
                .arg(&self.mnt)
                .status()
                .unwrap();
            assert!(detached.success(), "hdiutil detach {}", self.mnt.display());
            let attached = std::process::Command::new("/usr/bin/hdiutil")
                .args(["attach", "-quiet", "-nobrowse"])
                .args(attach)
                .arg("-mountpoint")
                .arg(&self.mnt)
                .arg(self.base.join("v.dmg"))
                .status()
                .unwrap();
            assert!(
                attached.success(),
                "hdiutil attach {attach:?} {}",
                self.base.join("v.dmg").display()
            );
        }
    }

    #[cfg(target_os = "macos")]
    impl Drop for Image {
        fn drop(&mut self) {
            let _ = std::process::Command::new("/usr/bin/hdiutil")
                .args(["detach", "-quiet", "-force"])
                .arg(&self.mnt)
                .status();
            let _ = std::fs::remove_dir_all(&self.base);
        }
    }

    /// Review round 3, S1 (macOS): on a volume mounted with ownership ignored, `fstat` shows
    /// every caller as the owner of every file and xnu gives every caller the owner's bits, so
    /// the owner, mode and ACL checks all pass while any local user could read the token or
    /// plant their own port. Both sides refuse a directory there, naming it and the rule. The
    /// same image attached with ownership kept is accepted, so the volume flag is the cause.
    #[cfg(target_os = "macos")]
    #[test]
    fn a_directory_on_a_volume_that_ignores_ownership_is_refused() {
        let image = Image::attach("noowners", &["-owners", "off"]);
        let run = image.mnt.join("prof");
        let run_s = run.display().to_string();
        let made = crate::private_dir(&run).map(drop);
        assert!(
            made.as_ref()
                .is_err_and(|why| why.contains(&run_s) && why.contains("ignores ownership")),
            "the server: {made:?}"
        );
        // S6: the fix it names includes making a new directory, since one made now keeps uid 99.
        assert!(
            made.as_ref().is_err_and(
                |why| why.contains("enableOwnership") && why.contains("make a new directory")
            ),
            "the hint: {made:?}"
        );
        let st = std::fs::metadata(&run).unwrap();
        // SAFETY: geteuid cannot fail.
        let me = unsafe { libc::geteuid() };
        assert_eq!(
            (std::os::unix::fs::MetadataExt::uid(&st), st.permissions().mode() & 0o777),
            (me, 0o700),
            "what fstat shows passes the owner and mode checks"
        );
        write(&run.join(FILE), GOOD, 0o600);
        let read = super::read(&run);
        assert!(
            read.as_ref()
                .is_err_and(|why| why.contains(&run_s) && why.contains("ignores ownership")),
            "the client: {read:?}"
        );
        let token = super::read_token_file(&run.join(FILE));
        assert!(
            token.as_ref().is_err_and(|why| why.contains("ignores ownership")),
            "--token-file: {token:?}"
        );
        drop(image);

        let kept = Image::attach("owners", &["-owners", "on"]);
        let run = kept.mnt.join("prof");
        let d = crate::private_dir(&run).unwrap_or_else(|why| panic!("ownership kept: {why}"));
        let e = Endpoint { addr: "127.0.0.1:6060".parse().unwrap(), token: "t0k".to_owned() };
        d.write_whole(FILE, "x.part", e.encode().as_bytes()).unwrap();
        assert_eq!(super::read(&run), Ok(Some(e)));
    }

    /// Review round 3, S6b (macOS): a directory made while its volume ignored ownership is
    /// stored as uid 99, which xnu shows as owned by whoever asks, so once ownership is kept
    /// again it passes the owner check for every user and any of them can replace its files.
    /// `PrivateDir::open` sets its owner to this user, which needs no root; this test runs
    /// without root, so that `fchown` succeeding on the uid-99 directory is what lets it open.
    /// (Only root sees the stored uid change from 99; the reviewer checked that by hand.)
    #[cfg(target_os = "macos")]
    #[test]
    fn a_uid_99_directory_is_made_this_users_own() {
        let image = Image::attach("uid99", &["-owners", "off"]);
        let run = image.mnt.join("prof");
        std::fs::create_dir(&run).unwrap();
        std::fs::set_permissions(&run, std::fs::Permissions::from_mode(0o700)).unwrap();
        image.reattach(&["-owners", "on"]);
        let d = PrivateDir::open(&run).unwrap_or_else(|why| {
            panic!("a uid-99 directory on a volume that keeps ownership: {why}")
        });
        let e = Endpoint { addr: "127.0.0.1:6060".parse().unwrap(), token: "t0k".to_owned() };
        d.write_whole(FILE, "x.part", e.encode().as_bytes()).unwrap();
        assert_eq!(super::read(&run), Ok(Some(e)));
    }

    /// Review round 4, S4 (macOS): xnu shows a file whose on-disk uid is 99 as owned by whoever
    /// asks (unless root asks), on every volume, so another user's disk image with a uid-99 0700
    /// directory passes the owner, mode, ACL and ownership-flag checks, and its owner reads the
    /// token by re-attaching the image with ownership ignored. A volume mounted by anyone but
    /// this user or root is refused. Making such a mount needs a second account, so the rule is
    /// tested as `volume_verdict`; the images above, which this user mounts, show that the real
    /// `f_owner` passes it, and the volume this test's own temporary directory is on (mounted by
    /// this user or root) does too.
    #[cfg(target_os = "macos")]
    #[test]
    fn a_volume_another_user_mounted_is_refused() {
        use super::unix::{Volume, volume_verdict};
        let local = libc::MNT_LOCAL as u32;
        assert_eq!(volume_verdict(local, 501, 501), Volume::Trusted, "mounted by this user");
        assert_eq!(volume_verdict(local, 0, 501), Volume::Trusted, "mounted by root");
        assert_eq!(
            volume_verdict(local, 502, 501),
            Volume::MountedBy(502),
            "mounted by another user"
        );
        assert_eq!(volume_verdict(local, 0, 0), Volume::Trusted, "root's own");
        assert_eq!(
            volume_verdict(local | libc::MNT_IGNORE_OWNERSHIP as u32, 501, 501),
            Volume::IgnoresOwnership
        );
        assert_eq!(volume_verdict(0, 501, 501), Volume::Network, "not local");
        let dir = Tmp::new("owner", 0o700);
        assert!(PrivateDir::open(&dir.0).is_ok(), "a volume this user or root mounted");
    }

    /// Review round 4, S5 (Linux): a FUSE mount's daemon makes up the owner, mode and ACL the
    /// checks read and receives the token's bytes, so a mount another user made with
    /// `allow_other` passes every other check. ix-prof finds the descriptor's mount in
    /// `/proc/self/mountinfo` and accepts FUSE only when this user mounted it (without
    /// `allow_other`, or with `default_permissions`) or root did with `default_permissions`.
    /// The table is in the kernel's format (fs/proc_namespace.c, fs/fuse/inode.c's
    /// `fuse_show_options`), with its lines as an Ubuntu host shows them.
    #[test]
    fn a_fuse_mount_another_user_made_is_refused() {
        use super::mountinfo::{find, fuse_verdict};
        let table = "\
22 1 259:2 / / rw,relatime shared:1 - ext4 /dev/nvme0n1p2 rw,errors=remount-ro
25 22 0:22 / /proc rw,nosuid,nodev,noexec,relatime shared:12 - proc proc rw
40 22 0:35 / /tmp rw,nosuid,nodev shared:17 - tmpfs tmpfs rw,size=8388608k,inode64
101 40 0:61 / /tmp/prof rw,nosuid,nodev,relatime shared:60 - fuse.evil evil rw,user_id=1001,group_id=1001,default_permissions,allow_other
102 22 0:62 / /home/me/remote\\040dir rw,nosuid,nodev,relatime shared:61 - fuse.sshfs me@host:/srv rw,user_id=1000,group_id=1000
103 22 0:63 / /home/me/shared rw,nosuid,nodev,relatime shared:62 - fuse.sshfs me@host:/x rw,user_id=1000,group_id=1000,allow_other
104 22 0:64 / /home/me/checked rw,nosuid,nodev,relatime shared:63 - fuse.sshfs me@host:/y rw,user_id=1000,group_id=1000,default_permissions,allow_other
105 22 0:65 / /var/lib/containers/storage/overlay/merged rw,nodev,relatime - fuse.fuse-overlayfs fuse-overlayfs rw,user_id=0,group_id=0,default_permissions,allow_other
106 22 0:66 / /mnt/rootfuse rw,nosuid,nodev,relatime shared:64 - fuse rootfs rw,user_id=0,group_id=0,allow_other
107 22 0:67 / /mnt/host rw,relatime shared:65 - virtiofs share rw
108 22 0:68 / /mnt/odd rw,relatime shared:66 - fuse.odd odd rw
109 22 0:69 / /mnt/blk rw,relatime shared:67 - fuseblk /dev/sdb1 rw,user_id=0,group_id=0,default_permissions,allow_other,blksize=4096
";
        let me = 1000;
        let verdict = |id: u64| {
            let m = find(table, id).unwrap().unwrap_or_else(|| panic!("mount {id}"));
            assert_eq!(m.id, id);
            fuse_verdict(m.fstype, m.super_options, me, true)
        };
        // The attack: another user's FUSE with allow_other, faking this user's 0700 directory.
        let evil = find(table, 101).unwrap().unwrap();
        assert_eq!(
            (evil.dev, evil.point.as_str(), evil.fstype),
            ((0, 61), "/tmp/prof", "fuse.evil")
        );
        assert!(
            verdict(101).is_err_and(|why| why.contains("uid 1001 mounted it")),
            "{:?}",
            verdict(101)
        );
        // This user's own, private or checked by the kernel: accepted. The point is unescaped.
        assert_eq!(find(table, 102).unwrap().unwrap().point, "/home/me/remote dir");
        assert_eq!(verdict(102), Ok(()));
        assert_eq!(verdict(104), Ok(()));
        // This user's own with allow_other but no default_permissions: others read through it.
        assert!(verdict(103).is_err_and(|why| why.contains("allow_other")), "{:?}", verdict(103));
        // Root's, with default_permissions (fuse-overlayfs, a fuseblk disk): accepted; without: refused.
        assert_eq!(verdict(105), Ok(()));
        assert_eq!(verdict(109), Ok(()));
        assert!(
            verdict(106).is_err_and(|why| why.contains("root mounted it")),
            "{:?}",
            verdict(106)
        );
        // virtiofs: only root mounts it.
        assert_eq!(verdict(107), Ok(()));
        // No user_id shown, or not FUSE at all: refused.
        assert!(verdict(108).is_err_and(|why| why.contains("no user_id")), "{:?}", verdict(108));
        let root = find(table, 22).unwrap().unwrap();
        assert!(fuse_verdict(root.fstype, root.super_options, me, true).is_err());
        // The same 101 seen by the user who made it: their own mount, checked by the kernel.
        assert_eq!(fuse_verdict(evil.fstype, evil.super_options, 1001, true), Ok(()));
        // A mount not listed (another namespace's) and a line that does not parse.
        assert_eq!(find(table, 999), Ok(None));
        let broken = "7 1 0:5 / /x rw shared:1 fuse.x x rw,user_id=1000\n";
        assert!(find(broken, 7).is_err_and(|why| why.contains("line 1")), "no separator");
        assert!(find("x 1 0:5 / /x rw - fuse x rw\n", 7).is_err(), "an ID that is not a number");
    }

    /// Review round 3, S8 (Linux): the kernel shows a FUSE mount's `user_id` in the mounter's
    /// user namespace. A host user's `allow_other,default_permissions` mount bound into a
    /// container (`-v /tmp:/tmp`) shows `user_id=1000`, and inside a container whose uid 1000 is
    /// some other host uid, that matched "this user". `user_id == me` now counts only as root
    /// or in the initial user namespace (`/proc/self/uid_map` exactly `0 0 4294967295`); the
    /// same line in the initial namespace is still this user's own, so the namespace is the
    /// cause. `user_id=0` stays accepted in any namespace (rootless podman's fuse-overlayfs).
    #[test]
    fn a_fuse_mount_seen_from_another_user_namespace_is_not_this_users() {
        use super::mountinfo::{fuse_verdict, initial_userns};
        // The kernel's format: "%10u %10u %10u\n".
        assert!(initial_userns("         0          0 4294967295\n"));
        assert!(initial_userns("0 0 4294967295"));
        assert!(!initial_userns("         0     166536      65536\n"), "a rootless container");
        assert!(
            !initial_userns("         0       1000          1\n         1     100000      65536\n"),
            "two lines"
        );
        assert!(!initial_userns("         0          0 4294967294\n"), "not the whole range");
        assert!(!initial_userns("         0          0 4294967295 7\n"), "an extra field");
        assert!(!initial_userns(""), "no map");

        let (fstype, options) =
            ("fuse.sshfs", "rw,user_id=1000,group_id=1000,default_permissions,allow_other");
        assert_eq!(
            fuse_verdict(fstype, options, 1000, true),
            Ok(()),
            "the initial namespace: this user's own"
        );
        let why = fuse_verdict(fstype, options, 1000, false).unwrap_err();
        assert!(why.contains("user namespace") && why.contains("uid_map"), "{why}");
        // Private (no allow_other) is refused too: the rule is about who user_id names.
        assert!(fuse_verdict(fstype, "rw,user_id=1000,group_id=1000", 1000, false).is_err());
        // Root inside a container, and root's mounts from any namespace: accepted.
        let overlay = "rw,user_id=0,group_id=0,default_permissions,allow_other";
        assert_eq!(fuse_verdict("fuse.fuse-overlayfs", overlay, 0, false), Ok(()));
        assert_eq!(fuse_verdict("fuse.fuse-overlayfs", overlay, 1000, false), Ok(()));
    }

    /// Review round 3, S9 (Linux): a FUSE mount root made of a network store (sshfs, rclone,
    /// s3fs, ...) puts the token where the server decides who reads it, as NFS does, so it is
    /// refused even with `default_permissions`; the same options on a local FUSE type are
    /// accepted, so the type is the cause. This user's own sshfs mount stays their choice.
    #[test]
    fn a_network_fuse_mount_root_made_is_refused() {
        use super::mountinfo::{NETWORK_FUSE, fuse_verdict};
        let options = "rw,user_id=0,group_id=0,default_permissions,allow_other";
        for fstype in NETWORK_FUSE {
            let why = fuse_verdict(fstype, options, 1000, true).unwrap_err();
            assert!(why.contains(fstype) && why.contains("network store"), "{fstype}: {why}");
        }
        assert_eq!(fuse_verdict("fuse.fuse-overlayfs", options, 1000, true), Ok(()));
        assert_eq!(fuse_verdict("fuse.sshfs", "rw,user_id=1000,group_id=1000", 1000, true), Ok(()));
    }

    /// Linux: a POSIX ACL with a named entry on the directory or the file, hidden from the mode
    /// by a `---` mask, is refused; so is a default ACL on the directory (which every file made
    /// in it would inherit), at the directory, so a start refuses it rather than the write at
    /// exit. Without them the same directory and file are accepted. The ACLs are set as their
    /// xattrs, so the test needs no setfacl; a file system without POSIX ACLs fails it, loudly.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_posix_acl_or_default_acl_is_refused() {
        // linux/posix_acl_xattr.h: a version, then (tag u16, perm u16, id u32) entries sorted by
        // tag: USER_OBJ, USER, GROUP_OBJ, MASK, OTHER.
        fn acl(owner: u16, named: u16, other: u16) -> Vec<u8> {
            let mut b = 2u32.to_le_bytes().to_vec();
            for (tag, perm, id) in [
                (0x01u16, owner, u32::MAX),
                (0x02, named, 65534),
                (0x04, 0, u32::MAX),
                (0x10, 0, u32::MAX),
                (0x20, other, u32::MAX),
            ] {
                b.extend_from_slice(&tag.to_le_bytes());
                b.extend_from_slice(&perm.to_le_bytes());
                b.extend_from_slice(&id.to_le_bytes());
            }
            b
        }
        fn set(path: &std::path::Path, name: &std::ffi::CStr, value: Option<&[u8]>) {
            let c = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
            // SAFETY: NUL-terminated path and name; the value, if any, is a live slice.
            let rc = unsafe {
                match value {
                    Some(v) => {
                        libc::setxattr(c.as_ptr(), name.as_ptr(), v.as_ptr().cast(), v.len(), 0)
                    }
                    None => libc::removexattr(c.as_ptr(), name.as_ptr()),
                }
            };
            assert_eq!(
                rc,
                0,
                "{name:?} on {}: {} (does this file system hold POSIX ACLs?)",
                path.display(),
                std::io::Error::last_os_error()
            );
        }
        let (access, default) = (c"system.posix_acl_access", c"system.posix_acl_default");
        let dir = Tmp::new("posixacl", 0o700);
        let dir_s = dir.0.display().to_string();
        let refused =
            |what: &str, got: Result<Option<Endpoint>, String>, names: &str, says: &str| {
                assert!(
                    got.as_ref().is_err_and(|why| why.contains(names) && why.contains(says)),
                    "{what}: {got:?}"
                );
            };

        set(&dir.0, access, Some(&acl(7, 7, 0)));
        assert_eq!(
            std::fs::metadata(&dir.0).unwrap().permissions().mode() & 0o777,
            0o700,
            "the mask hides the entry"
        );
        refused("an access ACL on the directory", super::read(&dir.0), &dir_s, "has an ACL");
        set(&dir.0, access, None);

        set(&dir.0, default, Some(&acl(7, 7, 0)));
        refused("a default ACL on the directory", super::read(&dir.0), &dir_s, "has a default ACL");
        let made = crate::private_dir(&dir.0).map(drop);
        assert!(
            made.as_ref().is_err_and(|why| why.contains("has a default ACL")),
            "the server: {made:?}"
        );
        set(&dir.0, default, None);

        write(&dir.0.join(FILE), GOOD, 0o600);
        set(&dir.0.join(FILE), access, Some(&acl(6, 4, 0)));
        let file_s = dir.0.join(FILE).display().to_string();
        refused("an access ACL on the file", super::read(&dir.0), &file_s, "has an ACL");
        set(&dir.0.join(FILE), access, None);

        assert!(
            matches!(super::read(&dir.0), Ok(Some(_))),
            "without them: {:?}",
            super::read(&dir.0)
        );
    }
}
