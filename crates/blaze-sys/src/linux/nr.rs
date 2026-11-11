//! The system call numbers blaze uses, per architecture, as data.
//!
//! Linux's system call ABI is stable: a number never changes once released. The values are
//! the kernel's `arch/x86/entry/syscalls/syscall_64.tbl` and `include/uapi/asm-generic/unistd.h`
//! (aarch64). Every row is checked against the `libc` crate's `SYS_*` constant by the test
//! `the_number_table_is_libcs`, so a wrong number fails a test and never a build of user code.

macro_rules! numbers {
    ($($name:ident = $x86_64:literal, $aarch64:literal;)*) => {
        $(
            #[doc = concat!("`", stringify!($name), "`.")]
            #[cfg(target_arch = "x86_64")]
            pub const $name: usize = $x86_64;
            #[doc = concat!("`", stringify!($name), "`.")]
            #[cfg(target_arch = "aarch64")]
            pub const $name: usize = $aarch64;
        )*

        /// Every call with its name and its number on this architecture.
        pub const TABLE: &[(&str, usize)] = &[$((stringify!($name), $name),)*];
    };
}

numbers! {
    //                    x86-64  aarch64
    READ                = 0,      63;
    WRITE               = 1,      64;
    CLOSE               = 3,      57;
    PREAD64             = 17,     67;
    PWRITE64            = 18,     68;
    OPENAT              = 257,    56;
    OPENAT2             = 437,    437;
    FSYNC               = 74,     82;
    FDATASYNC           = 75,     83;
    SYNC_FILE_RANGE     = 277,    84;
    FTRUNCATE           = 77,     46;
    RENAMEAT2           = 316,    276;
    UNLINKAT            = 263,    35;
    MKDIRAT             = 258,    34;
    STATX               = 332,    291;
    GETDENTS64          = 217,    61;
    IOCTL               = 16,     29;
    MMAP                = 9,      222;
    MUNMAP              = 11,     215;
    MADVISE             = 28,     233;
    MPROTECT            = 10,     226;
    FUTEX               = 202,    98;
    PPOLL               = 271,    73;
    PIDFD_OPEN          = 434,    434;
    WAITID              = 247,    95;
    WAIT4               = 61,     260;
    CLOCK_GETTIME       = 228,    113;
    CLOCK_NANOSLEEP     = 230,    115;
    CLONE3              = 435,    435;
    EXIT                = 60,     93;
    EXIT_GROUP          = 231,    94;
    GETRANDOM           = 318,    278;
    IO_URING_SETUP      = 425,    425;
    IO_URING_ENTER      = 426,    426;
    IO_URING_REGISTER   = 427,    427;
    SOCKET              = 41,     198;
    CONNECT             = 42,     203;
    BIND                = 49,     200;
    LISTEN              = 50,     201;
    ACCEPT4             = 288,    242;
    SENDMSG             = 46,     211;
    RECVMSG             = 47,     212;
    SHUTDOWN            = 48,     210;
    PIPE2               = 293,    59;
    EVENTFD2            = 290,    19;
    PRCTL               = 157,    167;
    SECCOMP             = 317,    277;
    RT_SIGACTION        = 13,     134;
    RT_SIGPROCMASK      = 14,     135;
    GETPID              = 39,     172;
    KILL                = 62,     129;
}
