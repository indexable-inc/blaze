//! The directory of each sandboxed run's own (`.blaze-spawn/<pid>-<serial>` under the driver's
//! root), which the confine keeps its record in: made empty as the run starts, removed with what
//! is in it once its sandbox has been asked what it read.
//!
//! Both are made in buffers the driver keeps: the run's path in one of the paths given back by
//! the runs before (their room kept), its wide verbatim form after a prefix made once, and the
//! walk of what is in it in the same wide buffer, so a warm run's directory allocates nothing.

use std::{
    io,
    path::{Path, PathBuf},
};

use windows_sys::Win32::{
    Foundation::{ERROR_ALREADY_EXISTS, ERROR_PATH_NOT_FOUND, INVALID_HANDLE_VALUE},
    Storage::FileSystem::{
        CreateDirectoryW, DeleteFileW, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_NORMAL, FILE_ATTRIBUTE_READONLY,
        FILE_ATTRIBUTE_REPARSE_POINT, FindClose, FindFirstFileW, FindNextFileW, RemoveDirectoryW, SetFileAttributesW,
        WIN32_FIND_DATAW,
    },
};

use super::sys;

/// Paths given back that are kept for the next runs.
const KEPT: usize = 256;

const BACKSLASH: u16 = b'\\' as u16;

pub(super) struct RunDirs {
    /// `root/.blaze-spawn`, as the confine is given each run's under it.
    dir: PathBuf,
    /// The same, absolute, verbatim and wide, with no NUL.
    prefix: Vec<u16>,
    /// Where the wide path of a run's directory, and of what is in it, is made (no NUL kept).
    wide: Vec<u16>,
    free: Vec<PathBuf>,
}

impl RunDirs {
    pub(super) fn new(root: &Path) -> io::Result<RunDirs> {
        // alloc: setup, once per driver.
        RunDirs::under(root.join(".blaze-spawn"))
    }

    /// Each run's directory made in `dir` rather than `root/.blaze-spawn`.
    pub(super) fn under(dir: PathBuf) -> io::Result<RunDirs> {
        let mut prefix = sys::path(&dir)?;
        prefix.pop();
        Ok(RunDirs {
            dir,
            prefix,
            // alloc: setup, the room of every path made, grown to the longest.
            wide: Vec::with_capacity(1024),
            // alloc: setup.
            free: Vec::with_capacity(KEPT),
        })
    }

    /// The directory of the run of serial `n`, made empty.
    pub(super) fn make(&mut self, n: u64) -> io::Result<PathBuf> {
        let mut name = [0u8; 48];
        let name = name_of(std::process::id(), n, &mut name);
        // alloc: none once as many runs have ended as run at once; their paths are kept.
        let mut path = self.free.pop().unwrap_or_default();
        path.as_mut_os_string().clear();
        path.push(&self.dir);
        path.push(name);
        let mut tries = 0;
        loop {
            self.wide_of(name);
            self.wide.push(0);
            // SAFETY: a NUL-terminated path; no security attributes.
            let made = sys::check(unsafe { CreateDirectoryW(self.wide.as_ptr(), std::ptr::null()) });
            self.wide.pop();
            let error = match made {
                Ok(()) => return Ok(path),
                Err(error) => error,
            };
            tries += 1;
            match error.raw_os_error().map(|it| it as u32) {
                // Left by a driver of this process id that ended before removing it.
                Some(ERROR_ALREADY_EXISTS) if tries < 3 => self.remove_tree(),
                // alloc: the first run's, once: `.blaze-spawn` and the root.
                Some(ERROR_PATH_NOT_FOUND) if tries < 3 => std::fs::create_dir_all(&self.dir)?,
                _ => {
                    // alloc: an error path.
                    return Err(io::Error::new(error.kind(), format!("{}: {error}", path.display())));
                }
            }
        }
    }

    /// `path`, a run's directory [`RunDirs::make`] made, removed with what is in it; what can't
    /// be removed is left. The path's room is kept for a run after.
    pub(super) fn remove(&mut self, path: PathBuf) {
        if let Some(name) = path.file_name().and_then(|it| it.to_str()) {
            self.wide_of(name);
            self.remove_tree();
        }
        if self.free.len() < KEPT {
            self.free.push(path);
        }
    }

    /// The prefix and then `name`, in `wide`.
    fn wide_of(&mut self, name: &str) {
        self.wide.clear();
        self.wide.extend_from_slice(&self.prefix);
        self.wide.push(BACKSLASH);
        self.wide.extend(name.encode_utf16());
    }

    /// The directory `wide` names removed, with everything under it. A link inside (a junction,
    /// a symbolic link) is removed itself, never what it points at.
    fn remove_tree(&mut self) {
        let at = self.wide.len();
        self.wide.extend_from_slice(&[BACKSLASH, b'*' as u16, 0]);
        // SAFETY: all zeros is a valid buffer for the call to fill.
        let mut found: WIN32_FIND_DATAW = unsafe { std::mem::zeroed() };
        // SAFETY: a NUL-terminated pattern and a buffer to fill.
        let find = unsafe { FindFirstFileW(self.wide.as_ptr(), &mut found) };
        self.wide.truncate(at);
        if find != INVALID_HANDLE_VALUE {
            loop {
                let len = found.cFileName.iter().position(|&it| it == 0).unwrap_or(found.cFileName.len());
                let name = &found.cFileName[..len];
                if name != [b'.' as u16] && name != [b'.' as u16, b'.' as u16] {
                    self.wide.push(BACKSLASH);
                    self.wide.extend_from_slice(name);
                    let attributes = found.dwFileAttributes;
                    match (attributes & FILE_ATTRIBUTE_DIRECTORY != 0, attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0) {
                        (true, false) => self.remove_tree(),
                        (true, true) => self.with_nul(|wide| {
                            // SAFETY: a NUL-terminated path.
                            unsafe { RemoveDirectoryW(wide) };
                        }),
                        (false, _) => self.with_nul(|wide| {
                            // SAFETY: `wide` is NUL-terminated and borrowed across the call,
                            // which keeps no pointer to it; a missing NUL would read past it.
                            let deleted = unsafe { DeleteFileW(wide) } != 0;
                            if !deleted && attributes & FILE_ATTRIBUTE_READONLY != 0 {
                                // SAFETY: as above.
                                unsafe { SetFileAttributesW(wide, FILE_ATTRIBUTE_NORMAL) };
                                // SAFETY: as above.
                                unsafe { DeleteFileW(wide) };
                            }
                        }),
                    }
                    self.wide.truncate(at);
                }
                // SAFETY: the search `FindFirstFileW` opened, and a buffer to fill.
                if unsafe { FindNextFileW(find, &mut found) } == 0 {
                    break;
                }
            }
            // SAFETY: the search opened above, closed once.
            unsafe { FindClose(find) };
        }
        self.with_nul(|wide| {
            // SAFETY: a NUL-terminated path.
            unsafe { RemoveDirectoryW(wide) };
        });
    }

    /// `call` given `wide`, NUL-terminated for it.
    fn with_nul(&mut self, call: impl FnOnce(*const u16)) {
        self.wide.push(0);
        call(self.wide.as_ptr());
        self.wide.pop();
    }
}

/// `<pid>-<n>`, in `into`.
fn name_of(pid: u32, n: u64, into: &mut [u8; 48]) -> &str {
    let mut len = 0;
    for (at, number) in [u64::from(pid), n].into_iter().enumerate() {
        if at > 0 {
            into[len] = b'-';
            len += 1;
        }
        let mut digits = [0u8; 20];
        let mut first = digits.len();
        let mut rest = number;
        loop {
            first -= 1;
            digits[first] = b'0' + (rest % 10) as u8;
            rest /= 10;
            if rest == 0 {
                break;
            }
        }
        into[len..len + digits.len() - first].copy_from_slice(&digits[first..]);
        len += digits.len() - first;
    }
    std::str::from_utf8(&into[..len]).expect("ASCII digits and a dash")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_run_directory_is_named_as_before_and_removed_with_what_is_in_it() {
        let mut into = [0u8; 48];
        assert_eq!(name_of(4242, 7, &mut into), "4242-7");
        assert_eq!(name_of(u32::MAX, u64::MAX, &mut into), format!("{}-{}", u32::MAX, u64::MAX));
        assert_eq!(name_of(0, 0, &mut into), "0-0");
        let root = std::env::temp_dir().join(format!("blaze-iocp-rundirs-{}", std::process::id()));
        _ = std::fs::remove_dir_all(&root);
        let mut dirs = RunDirs::new(&root).unwrap();
        let made = dirs.make(3).unwrap();
        assert_eq!(made, root.join(".blaze-spawn").join(format!("{}-3", std::process::id())));
        std::fs::create_dir_all(made.join("deep/er")).unwrap();
        std::fs::write(made.join("deep/er/f"), b"x").unwrap();
        std::fs::write(made.join("record"), b"read").unwrap();
        let mut readonly = std::fs::metadata(made.join("record")).unwrap().permissions();
        readonly.set_readonly(true);
        std::fs::set_permissions(made.join("record"), readonly).unwrap();
        // Left over: made again, empty.
        let again = dirs.make(3).unwrap();
        assert_eq!(again, made);
        assert_eq!(std::fs::read_dir(&again).unwrap().count(), 0);
        std::fs::write(again.join("report"), b"r").unwrap();
        dirs.remove(again);
        assert!(!made.exists());
        assert_eq!(std::fs::read_dir(root.join(".blaze-spawn")).unwrap().count(), 0);
        // A warm make and remove: the path given back is the next one's.
        for n in 0..4 {
            let ((), used) = blaze_alloc_count::measure(|| {
                let dir = dirs.make(10 + n).unwrap();
                dirs.remove(dir);
            });
            assert_eq!(used.allocations, 0, "a warm run directory's make and remove");
        }
        std::fs::remove_dir_all(root).unwrap();
    }
}
