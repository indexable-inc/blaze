//! The runner's side of the deterministic sandbox (READINESS.md 1b, prompt W3): an
//! [`Executor`] that answers a [`SpawnRequest`] by starting the hvbox helper once per request
//! (`<helper> spawn --request <scratch>/<key hex>.req --answer <scratch>/<key hex>.ans`), waiting
//! for it by a deadline, and reading the [`SpawnAnswer`] it wrote.
//!
//! THIS CODE STARTS THE HELPER, WHICH USES Hypervisor.framework (D94: a run of it kernel-panicked
//! the owner's Mac on 2026-11-16). NO TEST MAY START IT, and nothing chooses this executor until
//! the owner's go-ahead. The tests (`spawn_hvbox_tests.rs`) cover only the pure parts: the
//! helper's statuses ([`status_to_result`]) and the file names under the scratch folder
//! ([`key_file`]).
//!
//! - The helper's path is absolute, from the command's setting; never found on `PATH`. Its
//!   environment is empty: everything the guest sees is in the request.
//! - The wait is blaze-core's ([`blaze_core::linked`], over `blaze_core::wait`), inside a span
//!   `wait: sandbox spawn` naming the program (rules 15, 44), by `deadline`. At the deadline the
//!   helper's whole process group is killed and the spawn is refused as
//!   [`SpawnRefused::Deadline`] (rule 18). The wait is cut in slices of [`DRAIN_EVERY`] so the
//!   helper's standard error and output are read while it runs (an unread pipe stops a writer).
//! - The helper's statuses (READINESS.md 2d): 0 answered; 121 the owner has not allowed hvbox
//!   (the guard file is absent); 120 the helper refused or failed the run (its last standard
//!   error line says what); 2 a usage error (a bug in blaze); 124 a timeout; a signal; anything
//!   else. Each is a typed refusal ([`StatusMeaning`]); none is retried or run another way
//!   (rule 3).
//! - The request and answer files stay in the scratch folder, which is the command's and goes
//!   with it; an answer file left from an earlier spawn of the same key is removed before the
//!   helper starts, so an answer read is always this run's.

use core::{fmt, fmt::Write as _, time::Duration};
use std::{
    ffi::OsString,
    os::fd::{AsRawFd as _, OwnedFd},
    path::PathBuf,
};

use blaze_core::{
    Command, Sandbox,
    linked::{Exit, Link, LinkedChild, Piped, Waited, start_linked},
    wait::{Clock as _, Deadline},
};
use blaze_ids::ContentId;
use blaze_own::{AString, AVec, Alloc, String, Vec};
use blaze_spawn::{Executor, Files, Malformed, NoSandbox, SpawnAnswer, SpawnRefused, SpawnRequest};
use bstd::{
    fs::{Content, Durable},
    mem::{Bytes, TASK_FRAME},
    path::Path as BPath,
};

use crate::record::native_world;

/// The runner's hvbox executor: the helper's absolute path (the command's setting, never
/// `PATH`), the command's scratch folder (absolute) where the request and answer files are
/// written, and how long one spawn may take.
pub struct HvboxExecutor<'s> {
    pub helper: &'s str,
    pub scratch: &'s str,
    pub deadline: Duration,
}

/// The helper's statuses (READINESS.md 2d; crates/blaze-hvbox's bin).
pub(crate) const HELPER_USAGE: i32 = 2;
pub(crate) const HELPER_REFUSED: i32 = 120;
pub(crate) const HELPER_NOT_ALLOWED: i32 = 121;
pub(crate) const HELPER_TIMED_OUT: i32 = 124;

/// How often the wait stops to read the helper's pipes. A pipe holds 16 to 64 KiB; the helper
/// writes a line or two, so it never blocks on a full pipe for longer than this.
pub(crate) const DRAIN_EVERY: Duration = Duration::from_millis(50);

/// The most `read` calls in one drain of one pipe (4 KiB each: 256 KiB a slice).
const MOST_READS_PER_DRAIN: usize = 64;

/// The most drains after the helper ended before its pipes must have ended: 4,096 of 256 KiB.
const MOST_FINAL_DRAINS: usize = 4096;

/// The most bytes of the helper's last standard error line kept.
pub(crate) const LINE_BYTES: usize = 240;

/// The two files of one spawn under the scratch folder, named by the request's key.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum KeyFileEnd {
    /// `<key hex>.req`: the request's encoding, which the helper reads.
    Request,
    /// `<key hex>.ans`: the answer's encoding, which the helper writes.
    Answer,
}

/// A file name of one spawn: the key's 64 hex digits and a four-byte end, held in place.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct KeyFile {
    bytes: [u8; 68],
}

impl KeyFile {
    pub(crate) fn as_str(&self) -> &str {
        // Only ASCII hex digits and the ends' ASCII bytes are ever written (`key_file`).
        match core::str::from_utf8(&self.bytes) {
            Ok(name) => name,
            Err(_) => unreachable!("a key file name is ASCII by construction"),
        }
    }
}

impl fmt::Debug for KeyFile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The name of `key`'s request or answer file: all 64 lowercase hex digits of the key, then
/// `.req` or `.ans`.
pub(crate) fn key_file(key: ContentId, end: KeyFileEnd) -> KeyFile {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut bytes = [0u8; 68];
    for (at, byte) in key.0.iter().enumerate() {
        bytes[2 * at] = DIGITS[usize::from(byte >> 4)];
        bytes[2 * at + 1] = DIGITS[usize::from(byte & 0x0f)];
    }
    let tail: &[u8; 4] = match end {
        KeyFileEnd::Request => b".req",
        KeyFileEnd::Answer => b".ans",
    };
    bytes[64..].copy_from_slice(tail);
    KeyFile { bytes }
}

/// A line of the helper's standard error, its first [`LINE_BYTES`] bytes held in place, cut at a
/// character boundary; `cut` says bytes were left out.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct HelperLine {
    bytes: [u8; LINE_BYTES],
    len: usize,
    cut: bool,
}

impl HelperLine {
    pub(crate) const EMPTY: HelperLine = HelperLine { bytes: [0; LINE_BYTES], len: 0, cut: false };

    /// `text`'s first [`LINE_BYTES`] bytes, cut back to a character boundary.
    pub(crate) fn of(text: &str) -> HelperLine {
        let mut len = text.len().min(LINE_BYTES);
        while !text.is_char_boundary(len) {
            len -= 1;
        }
        let mut line = HelperLine { bytes: [0; LINE_BYTES], len, cut: len < text.len() };
        line.bytes[..len].copy_from_slice(&text.as_bytes()[..len]);
        line
    }

    /// The line's text: the longest prefix that is UTF-8 (bytes from a pipe may not be).
    pub(crate) fn as_str(&self) -> &str {
        let held = &self.bytes[..self.len];
        match core::str::from_utf8(held) {
            Ok(text) => text,
            Err(error) => match core::str::from_utf8(&held[..error.valid_up_to()]) {
                Ok(text) => text,
                Err(_) => unreachable!("a prefix up to `valid_up_to` is UTF-8"),
            },
        }
    }

    /// Whether bytes of the line were left out (past [`LINE_BYTES`], or not UTF-8).
    pub(crate) fn cut(&self) -> bool {
        self.cut || self.as_str().len() < self.len
    }

    fn push(&mut self, byte: u8) {
        match self.len < LINE_BYTES {
            true => {
                self.bytes[self.len] = byte;
                self.len += 1;
            }
            false => self.cut = true,
        }
    }
}

/// The last non-empty line of a stream fed in pieces, held in place.
pub(crate) struct Tail {
    open: HelperLine,
    last: HelperLine,
}

impl Tail {
    pub(crate) fn new() -> Tail {
        Tail { open: HelperLine::EMPTY, last: HelperLine::EMPTY }
    }

    pub(crate) fn feed(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            match byte {
                b'\n' => {
                    if self.open.len > 0 {
                        self.last = self.open;
                    }
                    self.open = HelperLine::EMPTY;
                }
                byte => self.open.push(byte),
            }
        }
    }

    /// The last non-empty line: the one still open (no newline after it), or the last ended.
    pub(crate) fn line(&self) -> &HelperLine {
        match self.open.len > 0 {
            true => &self.open,
            false => &self.last,
        }
    }
}

/// Why the helper's run gave no answer, from its status alone.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum StatusMeaning {
    /// 121: the guard file the owner makes by hand is absent (D94).
    NotAllowed,
    /// 124: the helper's own run was timed out.
    Deadline,
    /// Every other failure: the helper's status (for a signal, 128 and its number, as a shell
    /// says it), what it means, and the last line of its standard error.
    Helper { status: i32, why: HelperWhy, line: HelperLine },
}

/// Which failure of the helper a [`StatusMeaning::Helper`] is.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum HelperWhy {
    /// 120: the helper refused the request or its run failed; its last line says what.
    Refused,
    /// 2: a usage error: blaze started the helper wrongly, a bug in blaze.
    Usage,
    /// A signal of this number ended it (an abort is a panic in the helper).
    Signal(i32),
    /// It ended with neither an exit status nor a signal.
    NoStatus,
    /// An exit status the helper's protocol does not have.
    Other,
}

/// The helper's end mapped to its meaning (pure: no process): `Ok` only for exit status 0,
/// after which the answer file is read; a signal is a failure whatever the code.
pub(crate) fn status_to_result(
    code: Option<i32>,
    signal: Option<i32>,
    stderr_last_line: &str,
) -> Result<(), StatusMeaning> {
    let line = HelperLine::of(stderr_last_line);
    let helper = |status: i32, why: HelperWhy| Err(StatusMeaning::Helper { status, why, line });
    match (code, signal) {
        (_, Some(signal)) => helper(signal.saturating_add(128), HelperWhy::Signal(signal)),
        (Some(0), None) => Ok(()),
        (Some(HELPER_NOT_ALLOWED), None) => Err(StatusMeaning::NotAllowed),
        (Some(HELPER_REFUSED), None) => helper(HELPER_REFUSED, HelperWhy::Refused),
        (Some(HELPER_USAGE), None) => helper(HELPER_USAGE, HelperWhy::Usage),
        (Some(HELPER_TIMED_OUT), None) => Err(StatusMeaning::Deadline),
        (Some(code), None) => helper(code, HelperWhy::Other),
        (None, None) => helper(-1, HelperWhy::NoStatus),
    }
}

impl StatusMeaning {
    /// The refusal the executor returns for this meaning, for `program`, in `alloc`.
    pub(crate) fn refusal<'a>(self, program: &str, deadline: Duration, alloc: Alloc<'a>) -> SpawnRefused<'a> {
        let program = AString::from_str_in(program, alloc);
        match self {
            StatusMeaning::NotAllowed => SpawnRefused::NoSandbox { program, why: NoSandbox::OwnerHasNotAllowed },
            StatusMeaning::Deadline => SpawnRefused::Deadline { program, after_ms: millis(deadline) },
            StatusMeaning::Helper { status, why, line } => {
                let cut = if line.cut() { " [cut]" } else { "" };
                let text = line.as_str();
                let last_line = match why {
                    HelperWhy::Refused => words(alloc, format_args!("{text}{cut}")),
                    HelperWhy::Usage => words(
                        alloc,
                        format_args!("the hvbox helper said blaze started it wrongly (a bug in blaze): {text}{cut}"),
                    ),
                    HelperWhy::Signal(signal) => words(
                        alloc,
                        format_args!(
                            "the hvbox helper was ended by signal {signal} ({}); its standard error ends: {text}{cut}",
                            signal_name(signal)
                        ),
                    ),
                    HelperWhy::NoStatus => {
                        words(alloc, format_args!("the hvbox helper ended with neither an exit status nor a signal"))
                    }
                    HelperWhy::Other => words(
                        alloc,
                        format_args!(
                            "the hvbox helper exited {status}, a status its protocol does not have; its standard error ends: {text}{cut}"
                        ),
                    ),
                };
                SpawnRefused::Helper { program, status, last_line }
            }
        }
    }
}

/// A signal's name, for the words of a refusal.
fn signal_name(signal: i32) -> &'static str {
    match signal {
        libc::SIGHUP => "SIGHUP",
        libc::SIGINT => "SIGINT",
        libc::SIGQUIT => "SIGQUIT",
        libc::SIGILL => "SIGILL",
        libc::SIGTRAP => "SIGTRAP",
        libc::SIGABRT => "SIGABRT, a panic in the helper",
        libc::SIGBUS => "SIGBUS",
        libc::SIGFPE => "SIGFPE",
        libc::SIGKILL => "SIGKILL",
        libc::SIGSEGV => "SIGSEGV",
        libc::SIGPIPE => "SIGPIPE",
        libc::SIGTERM => "SIGTERM",
        libc::SIGSYS => "SIGSYS",
        _ => "a signal blaze does not name",
    }
}

fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

/// `args` written into a string of `alloc`. A `String`'s `fmt::Write` never fails; a `Display`
/// of an argument that fails leaves the words cut there, which is all a refusal's words can do.
fn words<'a>(alloc: Alloc<'a>, args: fmt::Arguments<'_>) -> AString<'a> {
    let mut out = String::new_in(alloc);
    _ = out.write_fmt(args);
    out
}

/// A malformed answer's words, by the variant (rule 19).
fn malformed_words<'a>(malformed: &Malformed, alloc: Alloc<'a>) -> AString<'a> {
    match malformed {
        Malformed::Layout(word) => words(alloc, format_args!("its layout word is {word:#018x}, not `bzspawn1`")),
        Malformed::EndsAt(at) => words(alloc, format_args!("it ends early, at byte {at}")),
        Malformed::NotUtf8 { field } => words(alloc, format_args!("its field `{field}` is not UTF-8")),
        Malformed::Unsorted { field } => words(alloc, format_args!("its list `{field}` is not in canonical order")),
        Malformed::Trailing(at) => words(alloc, format_args!("bytes follow its end, from byte {at}")),
    }
}

/// What one drain of a non-blocking pipe found.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Drained {
    /// The writer closed it: nothing more will come.
    Ended,
    /// Nothing more to read now.
    Empty,
    /// [`MOST_READS_PER_DRAIN`] reads were made and more may wait.
    More,
}

/// The calling thread's last system error number.
fn last_errno() -> i32 {
    std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
}

/// `fd` made non-blocking, so a drain never waits.
#[allow(unsafe_code)]
fn non_blocking(fd: &OwnedFd) -> Result<(), i32> {
    // SAFETY: `fcntl` with `F_GETFL`/`F_SETFL` takes integers only, on a descriptor this
    // process owns (`fd` is borrowed for the call).
    let flags = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFL) };
    if flags < 0 {
        return Err(last_errno());
    }
    // SAFETY: as above.
    let set = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) };
    match set < 0 {
        true => Err(last_errno()),
        false => Ok(()),
    }
}

/// Reads what the non-blocking pipe `fd` holds now, each piece to `take`, at most
/// [`MOST_READS_PER_DRAIN`] reads; an interrupted read counts as one.
#[allow(unsafe_code)]
fn drain(fd: &OwnedFd, mut take: impl FnMut(&[u8])) -> Result<Drained, i32> {
    let mut buffer = [0u8; 4096];
    for _ in 0..MOST_READS_PER_DRAIN {
        // SAFETY: `buffer` is writable for its whole length for the call, and `fd` is a
        // descriptor this process owns, borrowed for the call.
        let read = unsafe { libc::read(fd.as_raw_fd(), buffer.as_mut_ptr().cast(), buffer.len()) };
        match usize::try_from(read) {
            Ok(0) => return Ok(Drained::Ended),
            Ok(count) => take(&buffer[..count]),
            Err(_) => match last_errno() {
                libc::EINTR => {}
                os if os == libc::EAGAIN || os == libc::EWOULDBLOCK => return Ok(Drained::Empty),
                os => return Err(os),
            },
        }
    }
    Ok(Drained::More)
}

/// The helper's two pipes and what was read from them.
struct Pipes {
    out: OwnedFd,
    err: OwnedFd,
    tail: Tail,
    out_bytes: u64,
}

impl Pipes {
    /// One drain of each pipe: whether both have ended.
    fn drain_both(&mut self) -> Result<(Drained, Drained), (&'static str, i32)> {
        let tail = &mut self.tail;
        let err = drain(&self.err, |piece| tail.feed(piece)).map_err(|os| ("its standard error", os))?;
        let out_bytes = &mut self.out_bytes;
        let out = drain(&self.out, |piece| *out_bytes += piece.len() as u64).map_err(|os| ("its standard output", os))?;
        Ok((err, out))
    }
}

impl HvboxExecutor<'_> {
    /// A refusal of this spawn naming `program`, with `args` as its words.
    fn refused<'a>(program: &str, alloc: Alloc<'a>, args: fmt::Arguments<'_>) -> SpawnRefused<'a> {
        SpawnRefused::Refused { program: AString::from_str_in(program, alloc), what: words(alloc, args) }
    }

    /// `<scratch>/<name>`, in `alloc`.
    fn under_scratch<'a>(&self, name: &KeyFile, alloc: Alloc<'a>) -> AString<'a> {
        let mut at = String::with_capacity_in(self.scratch.len() + 1 + 68, alloc);
        at.push_str(self.scratch.trim_end_matches('/'));
        at.push('/');
        at.push_str(name.as_str());
        at
    }

    /// Waits for the helper by `self.deadline` on its own clock, reading its pipes every
    /// [`DRAIN_EVERY`]: how it ended, or the child back still running at the deadline (the
    /// caller kills it). The loop is bounded by the deadline: each turn waits until the next
    /// slice or the deadline.
    fn wait_draining(&self, mut child: LinkedChild, pipes: &mut Pipes) -> Result<Waited<LinkedChild>, WaitFailed> {
        loop {
            if let Err((what, os)) = pipes.drain_both() {
                return Err(WaitFailed::Pipe { what, os, child });
            }
            let now = child.clock().now();
            let slice = now.saturating_add(DRAIN_EVERY).min(self.deadline);
            child = match child.wait_until(Deadline::at(slice)) {
                Ok(Waited::Running(child)) if slice < self.deadline => child,
                Ok(waited) => return Ok(waited),
                // The child dropped with the failed wait: killed and reaped.
                Err(error) => return Err(WaitFailed::Link(error)),
            };
        }
    }
}

/// Why waiting for the helper failed (not how it ended). A failed read of its pipes gives the
/// child back, still running, for its group's kill.
enum WaitFailed {
    Pipe { what: &'static str, os: i32, child: LinkedChild },
    Link(blaze_core::linked::LinkError),
}

impl Executor for HvboxExecutor<'_> {
    fn name(&self) -> &'static str {
        "hvbox"
    }

    /// The request written to `<scratch>/<key hex>.req`, the helper started on it with an empty
    /// environment, waited for by `deadline`, its status mapped ([`status_to_result`]), and on
    /// status 0 its answer file read and decoded.
    ///
    /// `files` is not read here: how the helper receives the bytes behind `reads` and `present`
    /// is not yet a protocol (prompts W3 and H6 give only the request and answer files; H6's
    /// helper grants the request's paths). Named for the integrator in W3's report.
    fn answer<'a>(
        &self,
        request: &SpawnRequest<'_>,
        _files: &dyn Files,
        alloc: Alloc<'a>,
    ) -> Result<SpawnAnswer<'a>, SpawnRefused<'a>> {
        let program = request.program.name.as_str();
        let helper = self.helper;
        let scratch = self.scratch;
        if !helper.starts_with('/') {
            return Err(Self::refused(
                program,
                alloc,
                format_args!("the hvbox helper's path `{helper}` is not absolute; it is never looked up on PATH"),
            ));
        }
        if !scratch.starts_with('/') {
            return Err(Self::refused(
                program,
                alloc,
                format_args!("the sandbox's scratch folder `{scratch}` is not absolute"),
            ));
        }

        let key = request.key(alloc);
        let request_name = key_file(key, KeyFileEnd::Request);
        let answer_name = key_file(key, KeyFileEnd::Answer);
        let mut encoded: AVec<'a, u8> = Vec::new_in(alloc);
        request.encode(&mut encoded);

        // The request file written, and an answer left by an earlier spawn of this key removed.
        let (io, dir) = native_world(std::path::Path::new(scratch))
            .map_err(|e| Self::refused(program, alloc, format_args!("{scratch}: the sandbox's scratch folder: {e}")))?;
        let request_path = BPath::new(request_name.as_str())
            .map_err(|_| Self::refused(program, alloc, format_args!("{request_name:?}: not a relative path")))?;
        let answer_path = BPath::new(answer_name.as_str())
            .map_err(|_| Self::refused(program, alloc, format_args!("{answer_name:?}: not a relative path")))?;
        {
            let cx = io
                .cx(Duration::from_secs(60), TASK_FRAME)
                .map_err(|_| Self::refused(program, alloc, format_args!("{scratch}: its world stopped")))?;
            dir.publish(&cx, &request_path, Content::bytes(encoded.as_slice()), Durable::No).map_err(|e| {
                Self::refused(program, alloc, format_args!("{scratch}/{request_name:?}: writing the request: {e}"))
            })?;
            let stale = dir.stat(&cx, &answer_path).map_err(|e| {
                Self::refused(program, alloc, format_args!("{scratch}/{answer_name:?}: looking for an old answer: {e}"))
            })?;
            if stale.is_some() {
                dir.remove(&cx, &answer_path, Durable::No).map_err(|e| {
                    Self::refused(program, alloc, format_args!("{scratch}/{answer_name:?}: removing an old answer: {e}"))
                })?;
            }
        }

        // The helper: absolute path, empty environment, its own process group (killed whole at
        // the deadline). `Sandbox::Off` because the helper IS the sandbox: it runs no user code
        // on the host, only inside its VM.
        let request_at = self.under_scratch(&request_name, alloc);
        let answer_at = self.under_scratch(&answer_name, alloc);
        let command = Command {
            program: PathBuf::from(helper),
            args: vec![
                OsString::from("spawn"),
                OsString::from("--request"),
                OsString::from(request_at.as_str()),
                OsString::from("--answer"),
                OsString::from(answer_at.as_str()),
            ],
            env: Default::default(),
            dir: PathBuf::from(scratch),
            stdin: Default::default(),
            sandbox: Sandbox::Off,
            memory: None,
        };

        let span = tracing::info_span!("wait: sandbox spawn", program = %program).entered();
        let Piped { child, stdout: out, stderr: err } =
            start_linked(&command, &Link { inherit: &[], own_group: true }).map_err(|e| {
                Self::refused(program, alloc, format_args!("starting the hvbox helper {helper}: {e}"))
            })?;
        for (fd, what) in [(&out, "its standard output"), (&err, "its standard error")] {
            non_blocking(fd).map_err(|os| {
                Self::refused(
                    program,
                    alloc,
                    format_args!("the hvbox helper's {what}: making it non-blocking: {}", std::io::Error::from_raw_os_error(os)),
                )
            })?;
        }
        let mut pipes = Pipes { out, err, tail: Tail::new(), out_bytes: 0 };

        let ended = match self.wait_draining(child, &mut pipes) {
            Ok(ended) => ended,
            Err(failed) => {
                // Nothing is left running when the wait itself failed: a failed read gives the
                // child back for its group's kill; a failed wait dropped it (killed, reaped).
                let (why, killed) = match failed {
                    WaitFailed::Pipe { what, os, child } => (
                        words(alloc, format_args!("reading {what}: {}", std::io::Error::from_raw_os_error(os))),
                        child.kill_group().map(|_exit| ()),
                    ),
                    WaitFailed::Link(error) => (words(alloc, format_args!("{error}")), Ok(())),
                };
                return Err(Self::refused(
                    program,
                    alloc,
                    format_args!(
                        "waiting for the hvbox helper: {}{}",
                        why.as_str(),
                        match killed {
                            Ok(()) => "",
                            Err(_) => "; and killing its group failed",
                        }
                    ),
                ));
            }
        };
        let (code, signal) = match ended {
            Waited::Running(child) => {
                child.kill_group().map_err(|e| {
                    Self::refused(program, alloc, format_args!("killing the hvbox helper at its deadline: {e}"))
                })?;
                return Err(SpawnRefused::Deadline {
                    program: AString::from_str_in(program, alloc),
                    after_ms: millis(self.deadline),
                });
            }
            Waited::Ended(Exit::Exited(code)) => (Some(code), None),
            Waited::Ended(Exit::Signaled(signal)) => (None, Some(signal)),
        };

        // The helper has ended and been reaped: its pipes end once what it wrote is read. Bounded
        // by a count, as a pipe some other process still held would never end.
        let mut drains = 0usize;
        loop {
            let (err_drained, out_drained) = pipes.drain_both().map_err(|(what, os)| {
                Self::refused(
                    program,
                    alloc,
                    format_args!("reading the hvbox helper's {what}: {}", std::io::Error::from_raw_os_error(os)),
                )
            })?;
            if err_drained != Drained::More && out_drained != Drained::More {
                break;
            }
            drains += 1;
            if drains >= MOST_FINAL_DRAINS {
                return Err(Self::refused(
                    program,
                    alloc,
                    format_args!("the hvbox helper's pipes did not end after {MOST_FINAL_DRAINS} drains"),
                ));
            }
        }
        drop(span);

        status_to_result(code, signal, pipes.tail.line().as_str())
            .map_err(|meaning| meaning.refusal(program, self.deadline, alloc))?;
        if pipes.out_bytes != 0 {
            let out_bytes = pipes.out_bytes;
            return Err(SpawnRefused::Helper {
                program: AString::from_str_in(program, alloc),
                status: 0,
                last_line: words(
                    alloc,
                    format_args!(
                        "the hvbox helper wrote {out_bytes} bytes to its standard output, which the spawn protocol keeps empty"
                    ),
                ),
            });
        }

        // Status 0: the answer file is this run's (any older one was removed above).
        let cx = io
            .cx(Duration::from_secs(60), TASK_FRAME)
            .map_err(|_| Self::refused(program, alloc, format_args!("{scratch}: its world stopped")))?;
        let mut got = Bytes::new_in(cx.frame(), "an hvbox helper's spawn answer");
        if let Err(e) = dir.read_all(&cx, &answer_path, &mut got) {
            return Err(SpawnRefused::Helper {
                program: AString::from_str_in(program, alloc),
                status: 0,
                last_line: words(
                    alloc,
                    format_args!("the hvbox helper exited 0 and its answer {} could not be read: {e}", answer_at.as_str()),
                ),
            });
        }
        SpawnAnswer::decode(got.as_slice(), alloc).map_err(|malformed| {
            let what = malformed_words(&malformed, alloc);
            Self::refused(
                program,
                alloc,
                format_args!("{}: the hvbox helper's answer is malformed: {}", answer_at.as_str(), what.as_str()),
            )
        })
    }
}
