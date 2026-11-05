//! Whether a path is on a network file system, whose `flock` the kernel emulates with locks a
//! process loses on any close of the file (NFS) or does not keep across clients: a lease there
//! would not hold, so the collector refuses such an area (`crate::fs::check_local`).

use std::path::Path;

/// The kind of network file system `path` is on, or `None` for a local one.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[allow(unsafe_code)]
pub fn network(path: &Path) -> Result<Option<&'static str>, String> {
    use std::{ffi::CString, os::unix::ffi::OsStrExt as _};
    let name = CString::new(path.as_os_str().as_bytes())
        .map_err(|_| format!("{} has a NUL in it", path.display()))?;
    // SAFETY: `statfs` writes a `struct statfs` into `stat`, which is plain data any bytes of
    // which are a value, and reads the NUL-terminated `name`, alive across the call.
    let mut stat: libc::statfs = unsafe { std::mem::zeroed() };
    // SAFETY: as above.
    if unsafe { libc::statfs(name.as_ptr(), &mut stat) } != 0 {
        return Err(format!(
            "the file system of {}: {}",
            path.display(),
            std::io::Error::last_os_error()
        ));
    }
    Ok(kind(&stat))
}

#[cfg(target_os = "linux")]
fn kind(stat: &libc::statfs) -> Option<&'static str> {
    // The magic numbers of linux/magic.h, and of the network and user-space file systems
    // outside it. Each fits 32 bits: `f_type` is cut to them, never sign-extended (it is a
    // signed 32-bit word on 32-bit targets, where 0xFF534D42 reads negative).
    #[allow(clippy::unnecessary_cast, reason = "f_type is i64 on some targets and i32 on others")]
    match stat.f_type as u32 {
        0x6969 => Some("nfs"),
        0x517B => Some("smb"),
        0xFF53_4D42 => Some("cifs"),
        0xFE53_4D42 => Some("smb2"),
        0x5346_414F => Some("afs"),
        0x7375_7245 => Some("coda"),
        0x0102_1997 => Some("9p"),
        0x00C3_6400 => Some("ceph"),
        0x0BD0_0BD0 => Some("lustre"),
        0x4750_4653 => Some("gpfs"),
        // FUSE (sshfs, rclone, s3fs and the rest: whether `flock` holds is up to each).
        0x6573_5546 => Some("fuse"),
        0x6A65_6A63 => Some("virtiofs"),
        0x786F_4256 => Some("vboxsf"),
        0x0116_1970 => Some("gfs2"),
        0x7461_636F => Some("ocfs2"),
        0x1983_0326 => Some("beegfs"),
        _ => None,
    }
}

#[cfg(target_os = "macos")]
fn kind(stat: &libc::statfs) -> Option<&'static str> {
    let name: Vec<u8> =
        stat.f_fstypename.iter().take_while(|it| **it != 0).map(|it| *it as u8).collect();
    match name.as_slice() {
        b"nfs" => Some("nfs"),
        b"smbfs" => Some("smbfs"),
        b"afpfs" => Some("afpfs"),
        b"webdav" => Some("webdav"),
        b"cifs" => Some("cifs"),
        b"macfuse" | b"osxfuse" | b"fusefs" => Some("macfuse"),
        _ => None,
    }
}

/// Elsewhere (Windows, whose byte-range locks hold on SMB) no network file system is refused.
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub fn network(_path: &Path) -> Result<Option<&'static str>, String> {
    Ok(None)
}

/// Every mount point on the machine, as the kernel names them (absolute, symlinks resolved): a
/// walk that removes a directory refuses one with a mount at or under it, a bind mount of the
/// same device too, which no `st_dev` tells apart ([`crate::fs::mount_inside`], review S1).
#[cfg(any(target_os = "linux", target_os = "android"))]
pub fn mounts() -> Result<Vec<std::path::PathBuf>, String> {
    let table = "/proc/self/mountinfo";
    let text =
        std::fs::read_to_string(table).map_err(|error| format!("reading {table}: {error}"))?;
    // alloc: one a mount, once a check
    let mut found = Vec::new();
    for line in text.lines() {
        // `id parent major:minor root mount-point options ...`: the fifth field, its space,
        // tab, newline and backslash written as `\ooo`.
        let Some(point) = line.split(' ').nth(4) else {
            return Err(format!("{table} has a line of fewer than five fields: {line}"));
        };
        found.push(std::path::PathBuf::from(unescape(point)));
    }
    Ok(found)
}

/// The device a btrfs file system holding `path` is mounted from (mountinfo's source of the
/// mount `path` is under, the longest mount point that holds it); `None` off btrfs.
#[cfg(any(target_os = "linux", target_os = "android"))]
pub fn btrfs_source(path: &Path) -> Result<Option<String>, String> {
    let resolved = std::fs::canonicalize(path)
        .map_err(|error| format!("resolving {}: {error}", path.display()))?;
    let table = "/proc/self/mountinfo";
    let text =
        std::fs::read_to_string(table).map_err(|error| format!("reading {table}: {error}"))?;
    let mut best: Option<(usize, Option<String>)> = None;
    for line in text.lines() {
        // `id parent major:minor root mount-point options [optional...] - fstype source super`.
        let Some(point) = line.split(' ').nth(4) else {
            return Err(format!("{table} has a line of fewer than five fields: {line}"));
        };
        let point = std::path::PathBuf::from(unescape(point));
        if !resolved.starts_with(&point) {
            continue;
        }
        let depth = point.components().count();
        if best.as_ref().is_some_and(|(it, _)| *it > depth) {
            continue;
        }
        let mut after = line.split(" - ").nth(1).unwrap_or("").split(' ');
        let source = match (after.next(), after.next()) {
            (Some("btrfs"), Some(source)) => Some(unescape(source).to_string_lossy().into_owned()),
            _ => None,
        };
        // A later line of the same depth is mounted over the earlier one.
        best = Some((depth, source));
    }
    Ok(best.and_then(|(_, source)| source))
}

/// `\ooo` octal escapes undone, as mountinfo writes a mount point's space, tab, newline and
/// backslash.
#[cfg(any(target_os = "linux", target_os = "android"))]
fn unescape(field: &str) -> std::ffi::OsString {
    use std::os::unix::ffi::OsStringExt as _;
    let bytes = field.as_bytes();
    // alloc: one a mount point
    let mut out = Vec::with_capacity(bytes.len());
    let mut at = 0;
    while at < bytes.len() {
        let octal =
            bytes.get(at + 1..at + 4).filter(|it| it.iter().all(|b| (b'0'..=b'7').contains(b)));
        match (bytes[at], octal) {
            (b'\\', Some(digits)) => {
                let value = digits.iter().fold(0u32, |n, b| n * 8 + u32::from(b - b'0'));
                out.push(value as u8);
                at += 4;
            }
            (byte, _) => {
                out.push(byte);
                at += 1;
            }
        }
    }
    std::ffi::OsString::from_vec(out)
}

/// Every mount point on the machine (`getfsstat`'s `f_mntonname`).
#[cfg(target_os = "macos")]
#[allow(unsafe_code)]
pub fn mounts() -> Result<Vec<std::path::PathBuf>, String> {
    use std::os::unix::ffi::OsStrExt as _;
    // SAFETY: a null buffer asks only for the count.
    let count = unsafe { libc::getfsstat(std::ptr::null_mut(), 0, libc::MNT_NOWAIT) };
    if count < 0 {
        return Err(format!("listing the mounts (getfsstat): {}", std::io::Error::last_os_error()));
    }
    // Room for a few mounted since the count.
    let room = count as usize + 16;
    // SAFETY: `struct statfs` is plain data any bytes of which are a value.
    let mut stats: Vec<libc::statfs> = (0..room).map(|_| unsafe { std::mem::zeroed() }).collect(); // alloc: once a check
    let bytes = (room * std::mem::size_of::<libc::statfs>()) as libc::c_int;
    // SAFETY: `stats` holds `room` entries, `bytes` long, alive across the call.
    let got = unsafe { libc::getfsstat(stats.as_mut_ptr(), bytes, libc::MNT_NOWAIT) };
    if got < 0 {
        return Err(format!("listing the mounts (getfsstat): {}", std::io::Error::last_os_error()));
    }
    Ok(stats[..got as usize]
        .iter()
        .map(|it| {
            let name: Vec<u8> =
                it.f_mntonname.iter().take_while(|it| **it != 0).map(|it| *it as u8).collect(); // alloc: one a mount
            std::path::PathBuf::from(std::ffi::OsStr::from_bytes(&name))
        })
        .collect())
}

/// On Windows a mount point inside a directory (a junction, a volume mounted in a folder) is a
/// reparse point, which `symlink_metadata` reports as a link and no walk of blaze's descends
/// into: there is no mount a walk could cross, so none to list.
#[cfg(windows)]
pub fn mounts() -> Result<Vec<std::path::PathBuf>, String> {
    Ok(Vec::new())
}

/// Elsewhere the mounts are not listed, so nothing is removed: a walk could not tell a bind
/// mount inside a directory from the directory.
#[cfg(not(any(target_os = "linux", target_os = "android", target_os = "macos", windows)))]
pub fn mounts() -> Result<Vec<std::path::PathBuf>, String> {
    Err(format!(
        "listing the mounts is not done on {}: blaze removes no directory there, since it could not tell a mount inside one",
        std::env::consts::OS
    ))
}

/// The APFS container `path`'s volume is in (`disk3` of `/dev/disk3s5`): the volumes of one
/// container share its free space, so they are one volume to the pacer. `None` off APFS.
#[cfg(target_os = "macos")]
#[allow(unsafe_code)]
pub fn apfs_container(path: &Path) -> Result<Option<String>, String> {
    use std::{ffi::CString, os::unix::ffi::OsStrExt as _};
    let name = CString::new(path.as_os_str().as_bytes())
        .map_err(|_| format!("{} has a NUL in it", path.display()))?;
    // SAFETY: as in [`network`].
    let mut stat: libc::statfs = unsafe { std::mem::zeroed() };
    // SAFETY: as above.
    if unsafe { libc::statfs(name.as_ptr(), &mut stat) } != 0 {
        return Err(format!(
            "the file system of {}: {}",
            path.display(),
            std::io::Error::last_os_error()
        ));
    }
    let text = |field: &[libc::c_char]| -> String {
        let bytes: Vec<u8> = field.iter().take_while(|it| **it != 0).map(|it| *it as u8).collect(); // alloc: one a measure
        String::from_utf8_lossy(&bytes).into_owned()
    };
    if text(&stat.f_fstypename) != "apfs" {
        return Ok(None);
    }
    let from = text(&stat.f_mntfromname);
    // `/dev/disk3s5`, `/dev/disk3s1s1` (a sealed snapshot), and a snapshot mount's
    // `com.apple.os.update-<hash>@/dev/disk1s5` (review N12): the container is `disk3`, after the
    // last `/dev/`.
    let device = from.rfind("/dev/").map_or(from.as_str(), |at| &from[at + "/dev/".len()..]);
    let digits =
        device.strip_prefix("disk").map(|rest| rest.bytes().take_while(u8::is_ascii_digit).count());
    match digits {
        Some(n) if n > 0 => Ok(Some(device[..4 + n].to_owned())),
        _ => Err(format!(
            "{} is on APFS mounted from {from}, which names no container (diskN)",
            path.display()
        )),
    }
}
