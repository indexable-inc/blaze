//! `<IX_PROF>/http.endpoint`: where a running program's `/debug/pprof` is, its address and its
//! token in one file written whole (one rename), so a reader never pairs a new token with an
//! old address. And the checks both sides make on that directory and file.
//!
//! The threat is another local user. Whoever owns a directory, or can write to it, can rename
//! and replace any file in it (sticky bit or not), so in a directory someone else made first
//! (`mkdir -m 0777 /tmp/prof`) they could put their own port in the address and collect the
//! token from the next client. So the directory must be this user's own, not a symbolic link,
//! with no group or other mode bits; the file regular, this user's, mode 0600 and at most
//! [`MAX_FILE`] bytes; and the address a loopback one. ix-prof never changes the mode of a
//! directory it did not make: it refuses it, naming the rule. Every file operation goes through
//! the directory's descriptor, opened without following a link and checked once, so a path
//! swapped after the check is never followed.

use std::{
    net::SocketAddr,
    path::{Path, PathBuf},
};

/// The endpoint's file in the program's `IX_PROF` directory.
pub const FILE: &str = "http.endpoint";
/// The most a token or endpoint file may hold. A loopback address and a token fit in 55.
pub const MAX_FILE: usize = 64;

/// A running program's `/debug/pprof`: its address, always loopback, and its token.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Endpoint {
    pub addr: SocketAddr,
    pub token: String,
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
        let text = std::str::from_utf8(bytes).map_err(|_| format!("{shown} is not UTF-8: refused"))?;
        let mut lines = text.lines();
        let (Some(addr), Some(token), None) = (lines.next(), lines.next(), lines.next()) else {
            return Err(format!("{shown} is not an address line and a token line: refused"));
        };
        let addr: SocketAddr =
            addr.trim().parse().map_err(|_| format!("{shown}: {addr:?} is not an IP address and port: refused"))?;
        if !addr.ip().is_loopback() {
            return Err(format!(
                "{shown}: {addr} is not a loopback address: refused (the token goes to this machine's loopback only, never to another host)"
            ));
        }
        Ok(Endpoint { addr, token: header_word(token, path)? })
    }
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
/// its directory this user's own and closed to group and other, the file regular, this
/// user's, 0600 and at most [`MAX_FILE`] bytes.
pub fn read_token_file(file: &Path) -> Result<String, String> {
    let (Some(name), parent) = (file.file_name(), file.parent()) else {
        return Err(format!("{} names no file", file.display()));
    };
    let parent = parent.filter(|it| !it.as_os_str().is_empty()).unwrap_or(Path::new("."));
    let name = name.to_str().ok_or_else(|| format!("{} is not UTF-8", file.display()))?;
    let bytes = PrivateDir::open(parent)?.read_small(name)?.ok_or_else(|| format!("{}: no such file", file.display()))?;
    let text = std::str::from_utf8(&bytes).map_err(|_| format!("{} is not UTF-8: refused", file.display()))?;
    header_word(text, file)
}

/// A directory opened without following a link at its last component and checked to be this
/// user's own with no group or other mode bits; files are read, written and removed through
/// its descriptor.
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
        path::Path,
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

    impl PrivateDir {
        /// Opens `path` and checks it: a directory (not a symbolic link) owned by this user, with
        /// no group or other mode bits. A refusal names the path, what it is and the rule.
        pub fn open(path: &Path) -> Result<PrivateDir, String> {
            use std::os::unix::ffi::OsStrExt as _;
            let shown = path.display();
            let rule = "the directory holding a profile and the endpoint's token must be this user's own: whoever owns or can write a directory can replace the files in it";
            let link = std::fs::symlink_metadata(path).map_err(|why| format!("{shown}: {why}"))?;
            if link.file_type().is_symlink() {
                return Err(format!("{shown} is a symbolic link: refused ({rule}, and a link can point into another user's)"));
            }
            let cpath = c(path.as_os_str().as_bytes(), path)?;
            // SAFETY: a NUL-terminated path; the descriptor, if any, is owned below.
            let fd = unsafe { libc::open(cpath.as_ptr(), libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC) };
            if fd < 0 {
                return Err(format!("opening the directory {shown}: {}", std::io::Error::last_os_error()));
            }
            // SAFETY: `fd` was just opened and nothing else owns it.
            let fd = unsafe { OwnedFd::from_raw_fd(fd) };
            let st = fstat(fd.as_raw_fd()).map_err(|why| format!("{shown}: {why}"))?;
            let mode = st.st_mode as u32;
            if mode & libc::S_IFMT as u32 != libc::S_IFDIR as u32 {
                return Err(format!("{shown} is {}, not a directory: refused", kind(mode)));
            }
            if st.st_uid != me() {
                return Err(format!(
                    "{shown} belongs to uid {}, not this user (uid {}): refused ({rule}; use a directory of your own, e.g. under your home or target/)",
                    st.st_uid,
                    me()
                ));
            }
            if mode & 0o077 != 0 {
                return Err(format!(
                    "{shown} has mode {:04o}, open to group or other: refused ({rule}; `chmod 700 {shown}`, or name a new directory, which ix-prof makes 0700)",
                    mode & 0o7777
                ));
            }
            Ok(PrivateDir { fd, path: path.to_owned() })
        }

        fn at(&self, name: &str) -> std::path::PathBuf {
            self.path.join(name)
        }

        /// `name` in this directory, if there: a regular file (never followed through a link,
        /// and never a FIFO or device, so the read cannot block or run on) owned by this user,
        /// mode 0600, at most [`MAX_FILE`] bytes.
        pub fn read_small(&self, name: &str) -> Result<Option<Vec<u8>>, String> {
            let path = self.at(name);
            let shown = path.display();
            let cname = c(name.as_bytes(), &path)?;
            // SAFETY: `self.fd` is an open directory and `cname` NUL-terminated.
            let fd = unsafe {
                libc::openat(
                    self.fd.as_raw_fd(),
                    cname.as_ptr(),
                    libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_NOCTTY | libc::O_CLOEXEC,
                )
            };
            if fd < 0 {
                let why = std::io::Error::last_os_error();
                return match why.raw_os_error() {
                    Some(libc::ENOENT) => Ok(None),
                    Some(libc::ELOOP) => Err(format!("{shown} is a symbolic link: refused (it is read only as this user's own regular file)")),
                    _ => Err(format!("opening {shown}: {why}")),
                };
            }
            // SAFETY: `fd` was just opened and nothing else owns it.
            let file = std::fs::File::from(unsafe { OwnedFd::from_raw_fd(fd) });
            let st = fstat(file.as_raw_fd()).map_err(|why| format!("{shown}: {why}"))?;
            let mode = st.st_mode as u32;
            if mode & libc::S_IFMT as u32 != libc::S_IFREG as u32 {
                return Err(format!("{shown} is {}: refused (it is read only as a regular file)", kind(mode)));
            }
            if st.st_uid != me() {
                return Err(format!("{shown} belongs to uid {}, not this user (uid {}): refused", st.st_uid, me()));
            }
            if mode & 0o777 != 0o600 {
                return Err(format!("{shown} has mode {:04o}, not 0600: refused (a token must be readable by this user only)", mode & 0o7777));
            }
            if st.st_size as u64 > MAX_FILE as u64 {
                return Err(format!("{shown} is {} bytes, over {MAX_FILE}: refused", st.st_size));
            }
            let mut bytes = Vec::with_capacity(MAX_FILE);
            (&file).take(MAX_FILE as u64 + 1).read_to_end(&mut bytes).map_err(|why| format!("reading {shown}: {why}"))?;
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
                    libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                    0o600 as libc::c_uint,
                )
            };
            if fd < 0 {
                return Err(format!("making {}: {}", self.at(part).display(), std::io::Error::last_os_error()));
            }
            // SAFETY: `fd` was just opened and nothing else owns it.
            let mut file = std::fs::File::from(unsafe { OwnedFd::from_raw_fd(fd) });
            let written = (|| -> std::io::Result<()> {
                // Exactly 0600 under any umask: a reader refuses any other mode.
                // SAFETY: an open descriptor.
                if unsafe { libc::fchmod(file.as_raw_fd(), 0o600) } != 0 {
                    return Err(std::io::Error::last_os_error());
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
                return Err(format!("removing {}: {}", path.display(), std::io::Error::last_os_error()));
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

#[cfg(all(test, unix))]
mod tests {
    use std::os::unix::fs::PermissionsExt as _;

    use super::{Endpoint, FILE, PrivateDir};

    struct Tmp(std::path::PathBuf);

    impl Tmp {
        fn new(what: &str, mode: u32) -> Tmp {
            let dir = std::env::temp_dir().join(format!("ix-prof-endpoint-{what}-{}-{}", std::process::id(), crate::nonce()));
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
        assert!(why.contains("open to group or other") && why.contains(&dir.0.display().to_string()), "{why}");
    }

    /// What ix-prof writes it can read back, mode 0600, and a writer never leaves its part file.
    #[test]
    fn an_endpoint_written_whole_reads_back() {
        let dir = Tmp::new("write", 0o700);
        let e = Endpoint { addr: "[::1]:7".parse().unwrap(), token: "t0k".to_owned() };
        let d = PrivateDir::open(&dir.0).unwrap();
        d.write_whole(FILE, "x.part", e.encode().as_bytes()).unwrap();
        assert_eq!(super::read(&dir.0), Ok(Some(e)));
        let names: Vec<_> = std::fs::read_dir(&dir.0).unwrap().map(|it| it.unwrap().file_name()).collect();
        assert_eq!(names, [FILE]);
    }
}
