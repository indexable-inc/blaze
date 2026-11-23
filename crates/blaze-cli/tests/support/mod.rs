//! What blaze-cli's process tests share (tests/run_output.rs, tests/command_rungs.rs): the
//! scratch home a started `blaze` is given. One home for it, so no test makes a second.

use std::path::{Path, PathBuf};

fn dev_of(path: &Path) -> Option<blaze_sys::posix::types::dev_t> {
    let bytes = path.to_str()?.as_bytes();
    let mut buf = [0u8; 4096];
    if bytes.is_empty() || bytes.len() >= buf.len() {
        return None;
    }
    buf[..bytes.len()].copy_from_slice(bytes);
    buf[bytes.len()] = 0;
    let cstr = core::ffi::CStr::from_bytes_with_nul(&buf[..=bytes.len()]).ok()?;
    let st = blaze_sys::posix::proc::stat_at(blaze_sys::posix::io::CWD, blaze_sys::Path::new(cstr), 0).ok()?;
    Some(st.st_dev)
}

/// A fresh folder for one test, outside the owner's home, holding `home/` (the `HOME` the
/// process is given) and whatever the test copies beside it. Nothing a test starts reads or
/// writes the real `~/.blaze` (2026-11-16: this test wrote a 24 MB pack into the owner's
/// `~/.blaze/v2/shared` at every run, and failed on what other runs had left there). The
/// toolchain's shelf, which only the owner's home has, is made visible by hard links of its
/// files (no bytes copied when on the same volume; falling back to copying on cross-device
/// `EXDEV` because Linux `openat2(RESOLVE_NO_SYMLINKS)` refuses symlinks; the shelf is only ever
/// read): `<home>/.blaze/v2/toolchain`.
pub fn fresh_home(test: &str, with_shelf: bool) -> PathBuf {
    let (tmp, owner_home) = (std::env::temp_dir(), with_shelf.then(|| PathBuf::from(std::env::var_os("HOME").expect("the owner's HOME, to find the toolchain's shelf"))));
    let real = owner_home.as_ref().map(|h| h.join(".blaze/v2/toolchain"));
    let parent = match (&owner_home, &real) {
        (Some(home_root), Some(shelf)) if dev_of(shelf).is_some() && dev_of(shelf) != dev_of(&tmp) => {
            home_root.join(".blaze/v2/test-tmp")
        }
        _ => tmp,
    };
    let base = parent.join(format!("blaze-cli-{test}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let home = base.join("home");
    std::fs::create_dir_all(&home).expect("the temporary home");
    if let Some(real) = real {
        let shelf = home.join(".blaze/v2/toolchain");
        // A work list of folders, not recursion: each folder made, its files linked.
        let mut folders = vec![(real.clone(), shelf)];
        while let Some((from, to)) = folders.pop() {
            std::fs::create_dir_all(&to).unwrap_or_else(|error| panic!("making {}: {error}", to.display()));
            for entry in std::fs::read_dir(&from).unwrap_or_else(|error| panic!("listing the shelf's {}: {error}", from.display())) {
                let entry = entry.expect("a shelf entry");
                let kind = entry.file_type().expect("a shelf entry's kind");
                let (path, at) = (entry.path(), to.join(entry.file_name()));
                if kind.is_dir() {
                    folders.push((path, at));
                } else if kind.is_symlink() {
                    let target = std::fs::read_link(&path).expect("a shelf link's target");
                    std::os::unix::fs::symlink(&target, &at).unwrap_or_else(|error| panic!("linking {}: {error}", at.display()));
                } else if let Err(error) = std::fs::hard_link(&path, &at) {
                    if error.kind() == std::io::ErrorKind::CrossesDevices || error.raw_os_error() == Some(18) {
                        std::fs::copy(&path, &at).unwrap_or_else(|copy_err| panic!("copying the shelf's {} to {}: {copy_err}", path.display(), at.display()));
                    } else {
                        panic!("hard-linking the shelf's {} at {}: {error}", path.display(), at.display());
                    }
                }
            }
        }
    }
    base
}
