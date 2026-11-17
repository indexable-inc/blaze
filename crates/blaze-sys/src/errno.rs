//! [`Errno`]: the kernel's error numbers as a typed enum, with each target's numbers as data.

/// The number of a target: Linux's (the same on aarch64 and x86-64) or macOS's.
#[cfg(not(target_os = "macos"))]
const fn pick(linux: u16, _macos: u16) -> u16 {
    linux
}

/// The number of a target: Linux's (the same on aarch64 and x86-64) or macOS's.
#[cfg(target_os = "macos")]
const fn pick(_linux: u16, macos: u16) -> u16 {
    macos
}

macro_rules! errnos {
    ($($variant:ident = $name:literal, $linux:literal, $macos:literal, $what:literal;)*) => {
        /// A system call's error. A number the table does not list is [`Errno::Unlisted`]
        /// with the number, so no error is lost or renamed.
        #[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
        pub enum Errno {
            $(#[doc = concat!("`", $name, "`: ", $what, ".")] $variant,)*
            /// A number this table does not list.
            Unlisted(u16),
        }

        impl Errno {
            /// Every listed error with its C name and its number on this target.
            pub const LISTED: &[(Errno, &str, u16)] =
                &[$((Errno::$variant, $name, pick($linux, $macos)),)*];

            /// The error's number on this target.
            #[blaze::noalloc]
            #[blaze::nopanic]
            pub const fn raw(self) -> u16 {
                match self {
                    $(Errno::$variant => pick($linux, $macos),)*
                    Errno::Unlisted(code) => code,
                }
            }

            /// The error's C name; `"E?"` for an unlisted number.
            #[blaze::noalloc]
            #[blaze::nopanic]
            pub const fn name(self) -> &'static str {
                match self {
                    $(Errno::$variant => $name,)*
                    Errno::Unlisted(_) => "E?",
                }
            }

            /// What the error says.
            #[blaze::noalloc]
            #[blaze::nopanic]
            pub const fn what(self) -> &'static str {
                match self {
                    $(Errno::$variant => $what,)*
                    Errno::Unlisted(_) => "an error this table does not list",
                }
            }

            /// The error a number names: the listed one, or [`Errno::Unlisted`] with the
            /// number. `from_raw(e.raw()) == e` for every listed `e`, and
            /// `from_raw(n).raw() == n` for every `n` (the round trip the tests check).
            #[blaze::noalloc]
            #[blaze::nopanic]
            pub const fn from_raw(code: u16) -> Errno {
                // One comparison per listed error, in the table's order; no two share a
                // number on a target (the tests check), so the order decides nothing.
                $(if code == pick($linux, $macos) {
                    return Errno::$variant;
                })*
                Errno::Unlisted(code)
            }
        }
    };
}

errnos! {
    Perm = "EPERM", 1, 1, "the operation is not permitted";
    NoEnt = "ENOENT", 2, 2, "no such file or directory";
    Srch = "ESRCH", 3, 3, "no such process";
    Intr = "EINTR", 4, 4, "a signal interrupted the call";
    Io = "EIO", 5, 5, "the device failed";
    Nxio = "ENXIO", 6, 6, "no such device or address";
    TooBig = "E2BIG", 7, 7, "the argument list is too long";
    NoExec = "ENOEXEC", 8, 8, "not an executable";
    BadF = "EBADF", 9, 9, "not an open descriptor, or not open for this";
    Child = "ECHILD", 10, 10, "no such child";
    Again = "EAGAIN", 11, 35, "nothing now: try again";
    NoMem = "ENOMEM", 12, 12, "out of memory";
    Acces = "EACCES", 13, 13, "permission denied";
    Fault = "EFAULT", 14, 14, "a bad address";
    Busy = "EBUSY", 16, 16, "busy";
    Exist = "EEXIST", 17, 17, "it exists already";
    XDev = "EXDEV", 18, 18, "across file systems, or out of a beneath-only resolution";
    NoDev = "ENODEV", 19, 19, "no such device";
    NotDir = "ENOTDIR", 20, 20, "not a directory";
    IsDir = "EISDIR", 21, 21, "a directory";
    Inval = "EINVAL", 22, 22, "an invalid argument";
    NFile = "ENFILE", 23, 23, "the system's file table is full";
    MFile = "EMFILE", 24, 24, "the process has too many descriptors open";
    NoTty = "ENOTTY", 25, 25, "the descriptor does not take this ioctl";
    TxtBsy = "ETXTBSY", 26, 26, "the file is being executed";
    FBig = "EFBIG", 27, 27, "the file would be too large";
    NoSpc = "ENOSPC", 28, 28, "no space left on the device";
    SPipe = "ESPIPE", 29, 29, "the descriptor cannot seek";
    RoFs = "EROFS", 30, 30, "the file system is read-only";
    MLink = "EMLINK", 31, 31, "too many links";
    Pipe = "EPIPE", 32, 32, "the other end is closed";
    Range = "ERANGE", 34, 34, "the result does not fit";
    Deadlk = "EDEADLK", 35, 11, "the lock would deadlock";
    NameTooLong = "ENAMETOOLONG", 36, 63, "the name is too long";
    NoLck = "ENOLCK", 37, 77, "no locks left";
    NoSys = "ENOSYS", 38, 78, "the kernel has no such call, or a filter refused it";
    NotEmpty = "ENOTEMPTY", 39, 66, "the directory is not empty";
    Loop = "ELOOP", 40, 62, "too many symbolic links, or one where none may be";
    Overflow = "EOVERFLOW", 75, 84, "the value does not fit its field";
    NotSock = "ENOTSOCK", 88, 38, "not a socket";
    MsgSize = "EMSGSIZE", 90, 40, "the message is too long";
    ProtoNoSupport = "EPROTONOSUPPORT", 93, 43, "the protocol is not supported";
    OpNotSupp = "EOPNOTSUPP", 95, 102, "the operation is not supported here";
    AfNoSupport = "EAFNOSUPPORT", 97, 47, "the address family is not supported";
    AddrInUse = "EADDRINUSE", 98, 48, "the address is in use";
    AddrNotAvail = "EADDRNOTAVAIL", 99, 49, "the address is not available";
    NetDown = "ENETDOWN", 100, 50, "the network is down";
    NetUnreach = "ENETUNREACH", 101, 51, "the network is unreachable";
    ConnAborted = "ECONNABORTED", 103, 53, "the connection was aborted";
    ConnReset = "ECONNRESET", 104, 54, "the peer reset the connection";
    NoBufs = "ENOBUFS", 105, 55, "no buffer space";
    IsConn = "EISCONN", 106, 56, "the socket is connected already";
    NotConn = "ENOTCONN", 107, 57, "the socket is not connected";
    TimedOut = "ETIMEDOUT", 110, 60, "the time ran out";
    ConnRefused = "ECONNREFUSED", 111, 61, "the peer refused the connection";
    HostUnreach = "EHOSTUNREACH", 113, 65, "the host is unreachable";
    Already = "EALREADY", 114, 37, "the operation is under way already";
    InProgress = "EINPROGRESS", 115, 36, "the operation is now under way";
    Stale = "ESTALE", 116, 70, "a stale file handle";
    DQuot = "EDQUOT", 122, 69, "over the disk quota";
    Canceled = "ECANCELED", 125, 89, "the operation was cancelled";
}

impl core::fmt::Display for Errno {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{} ({}): {}", self.name(), self.raw(), self.what())
    }
}

impl core::error::Error for Errno {}
