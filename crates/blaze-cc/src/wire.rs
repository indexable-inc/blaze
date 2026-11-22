//! The messages between blaze and the blaze-cfront helper, version 2. Each travels as one
//! message of bstd's served channel (a body behind a 4-byte big-endian length: blaze-core's
//! `linked::channel`); this module is the bodies. Every integer is big-endian, like the
//! prefix. The helper's side is helper/wire.h, held byte-equal to this by the sample bodies in
//! tests.rs.
//!
//! The helper speaks first, then asks for every file it opens and every directory it stats or
//! lists (its virtual file system has nothing else), sends its outputs in pieces and its
//! diagnostics as rows, and says it is done:
//!
//! | tag    | from   | body                                                                   |
//! |--------|--------|------------------------------------------------------------------------|
//! | `0x01` | helper | Hello: u16 version, u8 [`Confinement`]                                 |
//! | `0x02` | helper | Look: u32 id, u8 [`LookOp`], the guest path (UTF-8, absolute)          |
//! | `0x03` | helper | Out: u8 [`OutKind`], bytes (appended to that output)                   |
//! | `0x04` | helper | Done: u8 0 (the unit compiled) or 1 (its source has errors)            |
//! | `0x05` | helper | Diagnostic: u8 [`Level`], u32 clang's diagnostic id, u32 byte offset,  |
//! |        |        | u16 length + the guest path of its file (empty for none), the message |
//! | `0x81` | blaze  | Job: u8 [`Mode`], u16 count, each argument as u32 length + bytes       |
//! | `0x82` | blaze  | Answer: u32 id, u8 kind (0 missing, 1 file, 2 directory, 3 listing,    |
//! |        |        | 4 no file there, taken as a directory: [`Answer::NotAFile`]),          |
//! |        |        | then for a file u64 size and, for a read, its bytes; for a listing    |
//! |        |        | u32 count and each name as u16 length + bytes, a `/` after a          |
//! |        |        | directory's                                                            |
//!
//! Version 1 had no listing and no diagnostic rows (diagnostics were an output of text).

#![blaze::sans_io]

use blaze_own::{AVec, Alloc, Vec};

/// The protocol version this side speaks.
pub const VERSION: u16 = 2;

/// The channel's length prefix (bstd's `procs::PREFIX`, blaze-core's channel): written here
/// so a message is whole before it is handed to the driver.
pub const PREFIX: usize = 4;

/// What the helper did to its own process before it read its first message.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Confinement {
    /// Nothing: refused wherever the system has a means (the machine's
    /// [`crate::job::JobOpts::confinement`]).
    None = 0,
    /// macOS: a Seatbelt profile denying every file open, write and network use.
    Seatbelt = 1,
    /// Linux: a seccomp filter refusing every call that opens a path or a socket.
    Seccomp = 2,
    /// Linux: that filter and a Landlock ruleset granting no path.
    SeccompLandlock = 3,
}

impl Confinement {
    fn from_byte(byte: u8) -> Option<Confinement> {
        Some(match byte {
            0 => Confinement::None,
            1 => Confinement::Seatbelt,
            2 => Confinement::Seccomp,
            3 => Confinement::SeccompLandlock,
            _ => return None,
        })
    }
}

/// A lookup the helper's file system makes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LookOp {
    /// Whether a path is a file (and its size) or a directory (`status`, `getRealPath`).
    Stat = 1,
    /// A file's bytes (`openFileForRead`).
    Read = 2,
    /// A directory's names (`dir_begin`).
    List = 3,
}

/// Which output an `Out` message adds to.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum OutKind {
    /// The relocatable object (Mach-O or ELF, one section a function and a datum).
    Object = 1,
    /// The `cfront-decls 2` text (decls.rs).
    Decls = 2,
}

/// A diagnostic's level, as clang's `DiagnosticsEngine::Level` (ignored ones are never sent).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Level {
    Note = 1,
    Remark = 2,
    Warning = 3,
    Error = 4,
    Fatal = 5,
}

/// What the helper is asked to do.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    /// The front end alone; the declarations written as `cfront-decls 2`.
    Decls = 1,
    /// The whole compile to an object, emitted to memory and sent.
    Compile = 2,
}

/// A message from the helper, its bytes borrowed from the body.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FromHelper<'b> {
    Hello { version: u16, confinement: Confinement },
    Look { id: u32, op: LookOp, path: &'b str },
    Out { kind: OutKind, bytes: &'b [u8] },
    Done { errors: bool },
    Diagnostic { level: Level, code: u32, offset: u32, path: &'b str, message: &'b str },
}

/// Why a body from the helper was refused.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum WireFault {
    Empty,
    UnknownTag(u8),
    /// The body ends before its fields do.
    Short { tag: u8 },
    /// It has bytes past its fields.
    Long { tag: u8 },
    UnknownConfinement(u8),
    UnknownOp(u8),
    UnknownOut(u8),
    UnknownDone(u8),
    UnknownLevel(u8),
    /// A path or a message that is not UTF-8.
    NotUtf8 { tag: u8 },
}

/// `body` read as a message from the helper.
pub fn decode(body: &[u8]) -> Result<FromHelper<'_>, WireFault> {
    let (&tag, rest) = body.split_first().ok_or(WireFault::Empty)?;
    let short = WireFault::Short { tag };
    let exact = |rest: &[u8], len: usize| match rest.len() {
        n if n < len => Err(WireFault::Short { tag }),
        n if n > len => Err(WireFault::Long { tag }),
        _ => Ok(()),
    };
    match tag {
        0x01 => {
            exact(rest, 3)?;
            let version = u16::from_be_bytes([rest[0], rest[1]]);
            let confinement = Confinement::from_byte(rest[2]).ok_or(WireFault::UnknownConfinement(rest[2]))?;
            Ok(FromHelper::Hello { version, confinement })
        }
        0x02 => {
            let (id, rest) = rest.split_first_chunk::<4>().ok_or(short)?;
            let (&op, path) = rest.split_first().ok_or(short)?;
            let op = match op {
                1 => LookOp::Stat,
                2 => LookOp::Read,
                3 => LookOp::List,
                other => return Err(WireFault::UnknownOp(other)),
            };
            Ok(FromHelper::Look { id: u32::from_be_bytes(*id), op, path: utf8(tag, path)? })
        }
        0x03 => {
            let (&kind, bytes) = rest.split_first().ok_or(short)?;
            let kind = match kind {
                1 => OutKind::Object,
                2 => OutKind::Decls,
                other => return Err(WireFault::UnknownOut(other)),
            };
            Ok(FromHelper::Out { kind, bytes })
        }
        0x04 => {
            exact(rest, 1)?;
            match rest[0] {
                0 => Ok(FromHelper::Done { errors: false }),
                1 => Ok(FromHelper::Done { errors: true }),
                other => Err(WireFault::UnknownDone(other)),
            }
        }
        0x05 => {
            let (&level, rest) = rest.split_first().ok_or(short)?;
            let level = match level {
                1 => Level::Note,
                2 => Level::Remark,
                3 => Level::Warning,
                4 => Level::Error,
                5 => Level::Fatal,
                other => return Err(WireFault::UnknownLevel(other)),
            };
            let (code, rest) = rest.split_first_chunk::<4>().ok_or(short)?;
            let (offset, rest) = rest.split_first_chunk::<4>().ok_or(short)?;
            let (len, rest) = rest.split_first_chunk::<2>().ok_or(short)?;
            let len = usize::from(u16::from_be_bytes(*len));
            if rest.len() < len {
                return Err(short);
            }
            let (path, message) = rest.split_at(len);
            Ok(FromHelper::Diagnostic {
                level,
                code: u32::from_be_bytes(*code),
                offset: u32::from_be_bytes(*offset),
                path: utf8(tag, path)?,
                message: utf8(tag, message)?,
            })
        }
        other => Err(WireFault::UnknownTag(other)),
    }
}

fn utf8(tag: u8, bytes: &[u8]) -> Result<&str, WireFault> {
    core::str::from_utf8(bytes).map_err(|_| WireFault::NotUtf8 { tag })
}

/// How a lookup is answered.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Answer<'b> {
    Missing,
    /// A file: for a stat its size alone, for a read its bytes.
    File { digest: [u8; 32], size: u64, bytes: Option<&'b [u8]> },
    Dir,
    /// No file is there, and the compile is to take it as a directory: the import's answer to
    /// a stat, whose seam asks for files alone (front.rs). Every file beneath it is asked for
    /// on its own, so what the compile finds is decided by file answers alone; the read list
    /// holds it as absent, which is true.
    NotAFile,
    /// A directory's names, sorted bytewise, each once, only those its root admits, a
    /// directory's with a `/` after it (the job checks all three and digests them itself:
    /// `blaze_roots::listing_digest`).
    Listing { names: &'b [&'b str] },
}

/// The body of the Job message, behind its prefix, added to `out`.
pub fn job(out: &mut AVec<'_, u8>, mode: Mode, args: &[&str]) -> Result<(), JobFault> {
    let count = u16::try_from(args.len()).map_err(|_| JobFault::TooManyArgs { count: args.len() })?;
    let start = open(out);
    out.push(0x81);
    out.push(mode as u8);
    out.extend_from_slice(&count.to_be_bytes());
    for arg in args {
        let len = u32::try_from(arg.len()).map_err(|_| JobFault::ArgTooLong)?;
        out.extend_from_slice(&len.to_be_bytes());
        out.extend_from_slice(arg.as_bytes());
    }
    close(out, start)
}

/// The body of an Answer to lookup `id`, behind its prefix, added to `out`.
pub fn answer(out: &mut AVec<'_, u8>, id: u32, answer: &Answer<'_>) -> Result<(), JobFault> {
    let start = open(out);
    out.push(0x82);
    out.extend_from_slice(&id.to_be_bytes());
    match answer {
        Answer::Missing => out.push(0),
        Answer::File { size, bytes, .. } => {
            out.push(1);
            out.extend_from_slice(&size.to_be_bytes());
            if let Some(bytes) = bytes {
                out.extend_from_slice(bytes);
            }
        }
        Answer::Dir => out.push(2),
        Answer::NotAFile => out.push(4),
        Answer::Listing { names } => {
            out.push(3);
            let count = u32::try_from(names.len()).map_err(|_| JobFault::MessageTooLong)?;
            out.extend_from_slice(&count.to_be_bytes());
            for name in names.iter() {
                let len = u16::try_from(name.len()).map_err(|_| JobFault::NameTooLong)?;
                out.extend_from_slice(&len.to_be_bytes());
                out.extend_from_slice(name.as_bytes());
            }
        }
    }
    close(out, start)
}

/// Why a message to the helper could not be written.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum JobFault {
    TooManyArgs { count: usize },
    ArgTooLong,
    /// A listed name past a u16's count of bytes.
    NameTooLong,
    /// A body past what one message's u32 prefix counts.
    MessageTooLong,
}

fn open(out: &mut AVec<'_, u8>) -> usize {
    let start = out.len();
    out.extend_from_slice(&[0; PREFIX]);
    start
}

fn close(out: &mut AVec<'_, u8>, start: usize) -> Result<(), JobFault> {
    let len = u32::try_from(out.len() - start - PREFIX).map_err(|_| JobFault::MessageTooLong)?;
    out.as_mut_slice()[start..start + PREFIX].copy_from_slice(&len.to_be_bytes());
    Ok(())
}

/// The Job message as the helper reads it, for the tests and the conformance case: its mode
/// and arguments.
pub fn read_job<'b, 'x>(body: &'b [u8], alloc: Alloc<'x>) -> Option<(Mode, AVec<'x, &'b str>)> {
    let (&tag, rest) = body.split_first()?;
    if tag != 0x81 {
        return None;
    }
    let (&mode, rest) = rest.split_first()?;
    let mode = match mode {
        1 => Mode::Decls,
        2 => Mode::Compile,
        _ => return None,
    };
    let (count, mut rest) = rest.split_first_chunk::<2>()?;
    let mut args = Vec::new_in(alloc);
    for _ in 0..u16::from_be_bytes(*count) {
        let (len, more) = rest.split_first_chunk::<4>()?;
        let len = usize::try_from(u32::from_be_bytes(*len)).ok()?;
        if more.len() < len {
            return None;
        }
        let (arg, more) = more.split_at(len);
        args.push(core::str::from_utf8(arg).ok()?);
        rest = more;
    }
    rest.is_empty().then_some((mode, args))
}
