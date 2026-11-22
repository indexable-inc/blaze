//! The two systems a compile-time child runs on, and their error numbers: a refusal answers the
//! number the real system gives for the same refusal, so an unchanged crate that probes takes
//! the branch it takes today. The numbers are the systems' own (macOS `sys/errno.h`; Linux
//! `asm-generic/errno-base.h` and `errno.h`, which arm64 uses unchanged).

#![blaze::sans_io]

use blaze_comptime::SysError;

/// The system the child's code was built for: a value, never read from the host (rule 75).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Os {
    /// macOS on arm64.
    Mac,
    /// Linux on arm64 (glibc).
    Linux,
}

/// An error number, by name.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Errno {
    Perm,
    NoEnt,
    Srch,
    Intr,
    Io,
    BadF,
    Child,
    Again,
    NoMem,
    Acces,
    Exist,
    NotDir,
    IsDir,
    Inval,
    MFile,
    NotTy,
    FBig,
    NoSpc,
    SPipe,
    RoFs,
    Pipe,
    Range,
    NameTooLong,
    NoSys,
    NotEmpty,
    NotSup,
    AfNoSupport,
    NotSock,
}

impl Errno {
    /// The number on `os`.
    pub const fn code(self, os: Os) -> i32 {
        match (self, os) {
            (Errno::Perm, _) => 1,
            (Errno::NoEnt, _) => 2,
            (Errno::Srch, _) => 3,
            (Errno::Intr, _) => 4,
            (Errno::Io, _) => 5,
            (Errno::BadF, _) => 9,
            (Errno::Child, _) => 10,
            (Errno::Again, Os::Mac) => 35,
            (Errno::Again, Os::Linux) => 11,
            (Errno::NoMem, _) => 12,
            (Errno::Acces, _) => 13,
            (Errno::Exist, _) => 17,
            (Errno::NotDir, _) => 20,
            (Errno::IsDir, _) => 21,
            (Errno::Inval, _) => 22,
            (Errno::MFile, _) => 24,
            (Errno::NotTy, _) => 25,
            (Errno::FBig, _) => 27,
            (Errno::NoSpc, _) => 28,
            (Errno::SPipe, _) => 29,
            (Errno::RoFs, _) => 30,
            (Errno::Pipe, _) => 32,
            (Errno::Range, _) => 34,
            (Errno::NameTooLong, Os::Mac) => 63,
            (Errno::NameTooLong, Os::Linux) => 36,
            (Errno::NoSys, Os::Mac) => 78,
            (Errno::NoSys, Os::Linux) => 38,
            (Errno::NotEmpty, Os::Mac) => 66,
            (Errno::NotEmpty, Os::Linux) => 39,
            (Errno::NotSup, Os::Mac) => 45,
            (Errno::NotSup, Os::Linux) => 95,
            (Errno::AfNoSupport, Os::Mac) => 47,
            (Errno::AfNoSupport, Os::Linux) => 97,
            (Errno::NotSock, Os::Mac) => 38,
            (Errno::NotSock, Os::Linux) => 88,
        }
    }

    /// The number the host's refusal stands for.
    pub const fn of(error: SysError) -> Errno {
        match error {
            SysError::NotFound => Errno::NoEnt,
            SysError::NotADir => Errno::NotDir,
            SysError::IsADir => Errno::IsDir,
            SysError::ReadOnly => Errno::RoFs,
            SysError::Unsupported => Errno::NotSup,
            SysError::Invalid => Errno::Inval,
            SysError::NoSpace => Errno::NoSpc,
            SysError::TooLarge => Errno::FBig,
            SysError::Exists => Errno::Exist,
            SysError::NotEmpty => Errno::NotEmpty,
        }
    }
}
