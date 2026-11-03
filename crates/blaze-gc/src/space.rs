//! A volume's size and free space, measured: one `statvfs` (unix) or `GetDiskFreeSpaceExW`
//! (Windows) a call, cheap enough for the daemon to measure as a build starts and every second
//! while builds run ([`crate::pace`]).

use std::path::Path;

use crate::pace::Volume;

/// The volume `path` is on: its nearest ancestor that is there, so an area not made yet is
/// measured on the volume it will be made on.
pub fn volume(path: &Path) -> Result<Volume, String> {
    let at = path
        .ancestors()
        .find(|it| it.exists())
        .ok_or_else(|| format!("no part of {} is there", path.display()))?;
    measure(at)
}

#[cfg(unix)]
#[allow(unsafe_code)]
fn measure(path: &Path) -> Result<Volume, String> {
    use std::{ffi::CString, os::unix::ffi::OsStrExt as _};
    let name = CString::new(path.as_os_str().as_bytes())
        .map_err(|_| format!("{} has a NUL in it", path.display()))?;
    // SAFETY: `statvfs` writes a `struct statvfs`, plain data any bytes of which are a value,
    // and reads the NUL-terminated `name`, alive across the call.
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: as above.
    if unsafe { libc::statvfs(name.as_ptr(), &mut stat) } != 0 {
        return Err(format!(
            "the free space of {}: {}",
            path.display(),
            std::io::Error::last_os_error()
        ));
    }
    #[allow(
        clippy::unnecessary_cast,
        clippy::useless_conversion,
        reason = "the field widths differ by target"
    )]
    let (frsize, blocks, avail, fsid) =
        (stat.f_frsize as u64, stat.f_blocks as u64, stat.f_bavail as u64, stat.f_fsid as u64);
    Ok(Volume {
        id: volume_id(path, fsid)?,
        total_bytes: blocks.saturating_mul(frsize),
        free_bytes: avail.saturating_mul(frsize),
    })
}

/// What tells volumes apart, so two areas whose free space is one are paced as one and their
/// bytes to free not counted twice (review N11): first a pool the file system shares among its
/// mounts (an APFS container on macOS, a btrfs file system's device on Linux, whose subvolumes
/// each have their own `st_dev` and `f_fsid`); then `f_fsid` when it is not 0 (an overlay
/// reports its upper file system's, while its `st_dev` is its own); else the device
/// (`st_dev`), as on tmpfs and others whose `f_fsid` is 0.
#[cfg(unix)]
fn volume_id(path: &Path, fsid: u64) -> Result<u64, String> {
    use std::os::unix::fs::MetadataExt as _;
    if let Some(pool) = pool_of(path)? {
        return Ok(hashed("blaze gc volume pool", pool.as_bytes()));
    }
    if fsid != 0 {
        return Ok(hashed("blaze gc volume fsid", &fsid.to_le_bytes()));
    }
    std::fs::metadata(path)
        .map(|it| it.dev())
        .map_err(|error| format!("the device of {}: {error}", path.display()))
}

#[cfg(unix)]
fn hashed(domain: &'static str, bytes: &[u8]) -> u64 {
    let id = blaze_ids::Hasher::new(domain).part(bytes).finish();
    let mut low = [0u8; 8];
    low.copy_from_slice(&id.0[..8]);
    u64::from_le_bytes(low)
}

/// The APFS container `path`'s volume is in: its volumes share its free space.
#[cfg(target_os = "macos")]
fn pool_of(path: &Path) -> Result<Option<String>, String> {
    Ok(crate::netfs::apfs_container(path)?.map(|it| format!("apfs {it}")))
}

/// The btrfs file system `path` is on, by the device its mount names: its subvolumes share
/// its free space.
#[cfg(any(target_os = "linux", target_os = "android"))]
fn pool_of(path: &Path) -> Result<Option<String>, String> {
    Ok(crate::netfs::btrfs_source(path)?.map(|it| format!("btrfs {it}")))
}

#[cfg(all(unix, not(any(target_os = "macos", target_os = "linux", target_os = "android"))))]
fn pool_of(_path: &Path) -> Result<Option<String>, String> {
    Ok(None)
}

/// WASI (the wasm32-wasip1 seed) has no call that reports a volume's size or free space, so
/// measuring one is an error naming the path, never a size made up.
#[cfg(target_os = "wasi")]
fn measure(path: &Path) -> Result<Volume, String> {
    Err(format!(
        "{}: statvfs is refused on WASI (the wasm32-wasip1 seed): WASI reports no volume's size or free space",
        path.display()
    ))
}

/// Whether `error` (an [`std::io::Error`]'s text, as an error that crossed the daemon's API
/// carries it) says a disk or a quota is full: `ENOSPC` and `EDQUOT` on unix,
/// `ERROR_DISK_FULL` and `ERROR_HANDLE_DISK_FULL` on Windows.
pub fn is_no_space(error: &str) -> bool {
    let codes: &[u32] = if cfg!(windows) {
        &[112, 39]
    } else if cfg!(target_os = "linux") || cfg!(target_os = "android") {
        &[28, 122]
    } else {
        &[28, 69]
    };
    codes.iter().any(|code| error.contains(&format!("(os error {code})")))
}

#[cfg(windows)]
#[allow(unsafe_code)]
fn measure(path: &Path) -> Result<Volume, String> {
    use std::os::windows::ffi::OsStrExt as _;
    use windows_sys::Win32::Storage::FileSystem::{
        GetDiskFreeSpaceExW, GetVolumeInformationW, GetVolumePathNameW,
    };
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain([0]).collect();
    let (mut avail, mut total, mut free) = (0u64, 0u64, 0u64);
    // SAFETY: `wide` is NUL-terminated and alive across the call; the outputs are plain u64s.
    if unsafe { GetDiskFreeSpaceExW(wide.as_ptr(), &mut avail, &mut total, &mut free) } == 0 {
        return Err(format!(
            "the free space of {}: {}",
            path.display(),
            std::io::Error::last_os_error()
        ));
    }
    let mut root = [0u16; 261];
    // SAFETY: `root` is a buffer of the length given; `wide` as above.
    if unsafe { GetVolumePathNameW(wide.as_ptr(), root.as_mut_ptr(), root.len() as u32) } == 0 {
        return Err(format!(
            "the volume of {}: {}",
            path.display(),
            std::io::Error::last_os_error()
        ));
    }
    let mut serial = 0u32;
    // SAFETY: `root` is NUL-terminated by the call above; only the serial is asked for.
    let ok = unsafe {
        GetVolumeInformationW(
            root.as_ptr(),
            std::ptr::null_mut(),
            0,
            &mut serial,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            0,
        )
    };
    if ok == 0 {
        return Err(format!(
            "the volume of {}: {}",
            path.display(),
            std::io::Error::last_os_error()
        ));
    }
    Ok(Volume { id: u64::from(serial), total_bytes: total, free_bytes: avail })
}
