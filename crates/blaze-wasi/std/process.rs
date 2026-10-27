//! blaze's `std::process` for build scripts compiled for wasm32-wasip1 (crates/blaze-wasi). It
//! stands for `library/std/src/sys/process/unsupported.rs` in the standard library ra-jit
//! compiles for such a script (`ra-jit --run-tool`), and nowhere else: a program built for wasip1
//! keeps the toolchain's file, whose commands are unsupported, as rustc's has.
//!
//! A command runs on the host through the import `blaze.run_tool`, which runs only the tools the
//! host allows (the toolchain, the C compiler, pkg-config) and refuses any other by name. WASI has
//! no processes, so a child runs whole when its outcome is first needed (`wait`, `output`, a read
//! of its output, its handle dropped): what was written to its stdin until then is its input.
//!
//! Request and response, fields little-endian, a field of bytes its `u32` length first:
//!   request:  program, cwd (empty: none), arg count `u32` and each arg, env count `u32` and
//!             each name and value, stdin
//!   response: `0u8`, status `i32`, stdout, stderr; or `1u8` (refused) or `2u8` (not started)
//!             and a message

use super::env::{CommandEnv, CommandEnvs, CommandResolvedEnvs};
pub use crate::ffi::OsString as EnvKey;
use crate::ffi::{OsStr, OsString};
use crate::io::{BorrowedCursor, IoSlice, IoSliceMut, Read, Write};
use crate::num::NonZero;
use crate::path::Path;
use crate::process::StdioPipes;
use crate::sync::{Arc, Mutex};
use crate::sys::fs::File;
use crate::{fmt, io};

#[link(wasm_import_module = "blaze")]
unsafe extern "C" {
    /// Runs the request at `request`, `len` bytes, and keeps the response for
    /// `run_tool_response`: its length, or -1 when the host could not read the request.
    #[link_name = "run_tool"]
    fn blaze_run_tool(request: *const u8, len: usize) -> i64;
    /// Writes the response of the last `run_tool` to `to`, `len` bytes.
    #[link_name = "run_tool_response"]
    fn blaze_run_tool_response(to: *mut u8, len: usize);
}

////////////////////////////////////////////////////////////////////////////////
// Command
////////////////////////////////////////////////////////////////////////////////

pub struct Command {
    program: OsString,
    args: Vec<OsString>,
    env: CommandEnv,

    cwd: Option<OsString>,
    stdin: Option<Stdio>,
    stdout: Option<Stdio>,
    stderr: Option<Stdio>,
}

#[derive(Debug)]
pub enum Stdio {
    Inherit,
    Null,
    MakePipe,
    ParentStdout,
    ParentStderr,
    InheritFile(File),
    /// The output of another child, read whole as this one's input.
    Child(ChildPipe),
}

impl Command {
    pub fn new(program: &OsStr) -> Command {
        Command {
            program: program.to_owned(),
            args: vec![program.to_owned()],
            env: Default::default(),
            cwd: None,
            stdin: None,
            stdout: None,
            stderr: None,
        }
    }

    pub fn arg(&mut self, arg: &OsStr) {
        self.args.push(arg.to_owned());
    }

    pub fn env_mut(&mut self) -> &mut CommandEnv {
        &mut self.env
    }

    pub fn cwd(&mut self, dir: &OsStr) {
        self.cwd = Some(dir.to_owned());
    }

    pub fn stdin(&mut self, stdin: Stdio) {
        self.stdin = Some(stdin);
    }

    pub fn stdout(&mut self, stdout: Stdio) {
        self.stdout = Some(stdout);
    }

    pub fn stderr(&mut self, stderr: Stdio) {
        self.stderr = Some(stderr);
    }

    pub fn get_program(&self) -> &OsStr {
        &self.program
    }

    pub fn get_args(&self) -> CommandArgs<'_> {
        let mut iter = self.args.iter();
        iter.next();
        CommandArgs { iter }
    }

    pub fn get_envs(&self) -> CommandEnvs<'_> {
        self.env.iter()
    }

    pub fn get_env_clear(&self) -> bool {
        self.env.does_clear()
    }

    pub fn get_resolved_envs(&self) -> CommandResolvedEnvs {
        CommandResolvedEnvs::new(self.env.capture())
    }

    pub fn get_current_dir(&self) -> Option<&Path> {
        self.cwd.as_ref().map(|cs| Path::new(cs))
    }

    pub fn spawn(
        &mut self,
        default: Stdio,
        needs_stdin: bool,
    ) -> io::Result<(Process, StdioPipes)> {
        let null = || if needs_stdin { None } else { Some(Stdio::Null) };
        let stdin = self.stdin.take().or_else(null).unwrap_or_else(|| default.again());
        let stdout = self.stdout.take().unwrap_or_else(|| default.again());
        let stderr = self.stderr.take().unwrap_or(default);
        let piped = [matches!(stdin, Stdio::MakePipe), matches!(stdout, Stdio::MakePipe)];
        let piped_err = matches!(stderr, Stdio::MakePipe);
        let run = Arc::new(Mutex::new(Run {
            pending: Some(Pending {
                request: self.request(),
                stdin_from: stdin,
                stdout_to: stdout,
                stderr_to: stderr,
            }),
            stdin: Vec::new(),
            outcome: None,
            read: [0, 0],
        }));
        let pipe = |side: Side| ChildPipe { run: run.clone(), side };
        let pipes = StdioPipes {
            stdin: piped[0].then(|| pipe(Side::In)),
            stdout: piped[1].then(|| pipe(Side::Out)),
            stderr: piped_err.then(|| pipe(Side::Err)),
        };
        Ok((Process { run, id: next_id() }, pipes))
    }

    /// The request of [`blaze_run_tool`] without its stdin, which is put last when it runs.
    fn request(&self) -> Vec<u8> {
        let mut out = Vec::new();
        field(&mut out, self.program.as_encoded_bytes());
        field(&mut out, self.cwd.as_deref().map_or(&[][..], OsStr::as_encoded_bytes));
        out.extend((self.args.len() as u32 - 1).to_le_bytes());
        for arg in &self.args[1..] {
            field(&mut out, arg.as_encoded_bytes());
        }
        let env = self.env.capture();
        out.extend((env.len() as u32).to_le_bytes());
        for (name, value) in &env {
            field(&mut out, name.as_encoded_bytes());
            field(&mut out, value.as_encoded_bytes());
        }
        out
    }
}

impl Stdio {
    /// The same choice for another of the child's streams: [`Command::spawn`]'s default.
    fn again(&self) -> Stdio {
        match self {
            Stdio::Inherit => Stdio::Inherit,
            Stdio::MakePipe => Stdio::MakePipe,
            _ => Stdio::Null,
        }
    }
}

fn field(out: &mut Vec<u8>, bytes: &[u8]) {
    out.extend((bytes.len() as u32).to_le_bytes());
    out.extend_from_slice(bytes);
}

fn next_id() -> u32 {
    use crate::sync::atomic::{AtomicU32, Ordering};
    static NEXT: AtomicU32 = AtomicU32::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

pub fn output(cmd: &mut Command) -> io::Result<(ExitStatus, Vec<u8>, Vec<u8>)> {
    let (mut process, mut pipes) = cmd.spawn(Stdio::MakePipe, false)?;
    drop(pipes.stdin.take());
    let status = process.wait()?;
    let (mut stdout, mut stderr) = (Vec::new(), Vec::new());
    if let Some(out) = pipes.stdout.take() {
        out.read_to_end(&mut stdout)?;
    }
    if let Some(err) = pipes.stderr.take() {
        err.read_to_end(&mut stderr)?;
    }
    Ok((status, stdout, stderr))
}

/// A child that has not run yet: its request, and where its streams come from and go.
struct Pending {
    request: Vec<u8>,
    stdin_from: Stdio,
    stdout_to: Stdio,
    stderr_to: Stdio,
}

struct Ran {
    status: i32,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

/// One child: what it is to run, what was written to its stdin, how it ended and how far each
/// of its piped outputs has been read.
struct Run {
    pending: Option<Pending>,
    stdin: Vec<u8>,
    outcome: Option<Result<Ran, (io::ErrorKind, String)>>,
    read: [usize; 2],
}

impl Run {
    /// Runs the child when it has not run, and how it ended.
    fn finish(&mut self) -> io::Result<&Ran> {
        if let Some(pending) = self.pending.take() {
            self.outcome = Some(self.start(pending));
        }
        match self.outcome.as_ref().expect("a child runs before its outcome is asked") {
            Ok(ran) => Ok(ran),
            Err((kind, message)) => Err(io::Error::new(*kind, message.clone())),
        }
    }

    fn start(&mut self, pending: Pending) -> Result<Ran, (io::ErrorKind, String)> {
        let failed = |e: io::Error| (e.kind(), e.to_string());
        let stdin = match pending.stdin_from {
            Stdio::MakePipe => crate::mem::take(&mut self.stdin),
            Stdio::Inherit => {
                let mut bytes = Vec::new();
                io::stdin().read_to_end(&mut bytes).map_err(failed)?;
                bytes
            }
            Stdio::Child(pipe) => {
                let mut bytes = Vec::new();
                pipe.read_to_end(&mut bytes).map_err(failed)?;
                bytes
            }
            Stdio::InheritFile(file) => {
                let (mut bytes, mut chunk) = (Vec::new(), [0u8; 8192]);
                loop {
                    match file.read(&mut chunk).map_err(failed)? {
                        0 => break bytes,
                        n => bytes.extend_from_slice(&chunk[..n]),
                    }
                }
            }
            Stdio::Null | Stdio::ParentStdout | Stdio::ParentStderr => Vec::new(),
        };
        let mut request = pending.request;
        field(&mut request, &stdin);
        let ran = call(&request)?;
        // What the child wrote to a stream it shares with this process, or to a file.
        let to = |to: Stdio, inherited: &mut dyn Write, bytes: &[u8]| -> io::Result<()> {
            match to {
                Stdio::Inherit => inherited.write_all(bytes),
                Stdio::ParentStdout => io::stdout().write_all(bytes),
                Stdio::ParentStderr => io::stderr().write_all(bytes),
                Stdio::InheritFile(file) => {
                    let mut written = 0;
                    while written < bytes.len() {
                        written += file.write(&bytes[written..])?;
                    }
                    Ok(())
                }
                Stdio::MakePipe | Stdio::Null | Stdio::Child(_) => Ok(()),
            }
        };
        to(pending.stdout_to, &mut io::stdout(), &ran.stdout).map_err(failed)?;
        to(pending.stderr_to, &mut io::stderr(), &ran.stderr).map_err(failed)?;
        Ok(ran)
    }
}

/// Has the host run `request` ([`blaze_run_tool`]).
fn call(request: &[u8]) -> Result<Ran, (io::ErrorKind, String)> {
    // SAFETY: `request` is readable for its length.
    let len = unsafe { blaze_run_tool(request.as_ptr(), request.len()) };
    let Ok(len) = usize::try_from(len) else {
        return Err((io::ErrorKind::InvalidInput, "blaze: the host could not read the command".into()));
    };
    let mut response = Vec::with_capacity(len);
    // SAFETY: `response` has room for `len` bytes, which the host writes.
    unsafe {
        blaze_run_tool_response(response.as_mut_ptr(), len);
        response.set_len(len);
    }
    let bad = || (io::ErrorKind::InvalidData, "blaze: the host's response is not one".to_owned());
    let rest = &mut &response[..];
    let mut take = |n: usize| -> Result<&[u8], (io::ErrorKind, String)> {
        let (head, tail) = rest.split_at_checked(n).ok_or_else(bad)?;
        *rest = tail;
        Ok(head)
    };
    let kind = take(1)?[0];
    let mut bytes = || -> Result<Vec<u8>, (io::ErrorKind, String)> {
        let len = u32::from_le_bytes(take(4)?.try_into().map_err(|_| bad())?) as usize;
        Ok(take(len)?.to_vec())
    };
    match kind {
        0 => {
            let status = i32::from_le_bytes(take(4)?.try_into().map_err(|_| bad())?);
            let (stdout, stderr) = (bytes()?, bytes()?);
            Ok(Ran { status, stdout, stderr })
        }
        1 => Err((io::ErrorKind::PermissionDenied, String::from_utf8_lossy(&bytes()?).into_owned())),
        _ => Err((io::ErrorKind::NotFound, String::from_utf8_lossy(&bytes()?).into_owned())),
    }
}

impl From<ChildPipe> for Stdio {
    fn from(pipe: ChildPipe) -> Stdio {
        Stdio::Child(pipe)
    }
}

impl From<crate::sys::pipe::Pipe> for Stdio {
    fn from(pipe: crate::sys::pipe::Pipe) -> Stdio {
        pipe.diverge()
    }
}

impl From<io::Stdout> for Stdio {
    fn from(_: io::Stdout) -> Stdio {
        Stdio::ParentStdout
    }
}

impl From<io::Stderr> for Stdio {
    fn from(_: io::Stderr) -> Stdio {
        Stdio::ParentStderr
    }
}

impl From<File> for Stdio {
    fn from(file: File) -> Stdio {
        Stdio::InheritFile(file)
    }
}

impl fmt::Debug for Command {
    // show all attributes
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if f.alternate() {
            let mut debug_command = f.debug_struct("Command");
            debug_command.field("program", &self.program).field("args", &self.args);
            if !self.env.is_unchanged() {
                debug_command.field("env", &self.env);
            }

            if self.cwd.is_some() {
                debug_command.field("cwd", &self.cwd);
            }

            if self.stdin.is_some() {
                debug_command.field("stdin", &self.stdin);
            }
            if self.stdout.is_some() {
                debug_command.field("stdout", &self.stdout);
            }
            if self.stderr.is_some() {
                debug_command.field("stderr", &self.stderr);
            }

            debug_command.finish()
        } else {
            if let Some(ref cwd) = self.cwd {
                write!(f, "cd {cwd:?} && ")?;
            }
            if self.env.does_clear() {
                write!(f, "env -i ")?;
                // Altered env vars will be printed next, that should exactly work as expected.
            } else {
                // Removed env vars need the command to be wrapped in `env`.
                let mut any_removed = false;
                for (key, value_opt) in self.get_envs() {
                    if value_opt.is_none() {
                        if !any_removed {
                            write!(f, "env ")?;
                            any_removed = true;
                        }
                        write!(f, "-u {} ", key.to_string_lossy())?;
                    }
                }
            }
            // Altered env vars can just be added in front of the program.
            for (key, value_opt) in self.get_envs() {
                if let Some(value) = value_opt {
                    write!(f, "{}={value:?} ", key.to_string_lossy())?;
                }
            }
            if self.program != self.args[0] {
                write!(f, "[{:?}] ", self.program)?;
            }
            write!(f, "{:?}", self.args[0])?;

            for arg in &self.args[1..] {
                write!(f, " {:?}", arg)?;
            }
            Ok(())
        }
    }
}

#[derive(PartialEq, Eq, Clone, Copy, Debug, Default)]
pub struct ExitStatus(i32);

impl ExitStatus {
    pub fn exit_ok(&self) -> Result<(), ExitStatusError> {
        match NonZero::new(self.0) {
            None => Ok(()),
            Some(code) => Err(ExitStatusError(code)),
        }
    }

    pub fn code(&self) -> Option<i32> {
        Some(self.0)
    }
}

impl fmt::Display for ExitStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "exit status: {}", self.0)
    }
}

#[derive(PartialEq, Eq, Clone, Copy, Debug)]
pub struct ExitStatusError(NonZero<i32>);

impl From<ExitStatusError> for ExitStatus {
    fn from(error: ExitStatusError) -> ExitStatus {
        ExitStatus(error.0.get())
    }
}

impl ExitStatusError {
    pub fn code(self) -> Option<NonZero<i32>> {
        Some(self.0)
    }
}

#[derive(PartialEq, Eq, Clone, Copy, Debug)]
pub struct ExitCode(u8);

impl ExitCode {
    pub const SUCCESS: ExitCode = ExitCode(0);
    pub const FAILURE: ExitCode = ExitCode(1);

    pub fn as_i32(&self) -> i32 {
        self.0 as i32
    }
}

impl From<u8> for ExitCode {
    fn from(code: u8) -> Self {
        Self(code)
    }
}

pub struct Process {
    run: Arc<Mutex<Run>>,
    id: u32,
}

impl Process {
    pub fn id(&self) -> u32 {
        self.id
    }

    pub fn kill(&mut self) -> io::Result<()> {
        let mut run = self.run.lock().unwrap_or_else(|it| it.into_inner());
        if run.pending.take().is_some() {
            // A child killed before it ran never runs, and ends as a killed one does.
            run.outcome = Some(Ok(Ran { status: 128 + 9, stdout: Vec::new(), stderr: Vec::new() }));
        }
        Ok(())
    }

    pub fn wait(&mut self) -> io::Result<ExitStatus> {
        let mut run = self.run.lock().unwrap_or_else(|it| it.into_inner());
        run.finish().map(|ran| ExitStatus(ran.status))
    }

    pub fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        self.wait().map(Some)
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        // A child that was started runs, whether or not it is waited for.
        let mut run = self.run.lock().unwrap_or_else(|it| it.into_inner());
        let _ = run.finish();
    }
}

pub struct CommandArgs<'a> {
    iter: crate::slice::Iter<'a, OsString>,
}

impl<'a> Iterator for CommandArgs<'a> {
    type Item = &'a OsStr;
    fn next(&mut self) -> Option<&'a OsStr> {
        self.iter.next().map(|os| &**os)
    }
    fn size_hint(&self) -> (usize, Option<usize>) {
        self.iter.size_hint()
    }
}

impl<'a> ExactSizeIterator for CommandArgs<'a> {
    fn len(&self) -> usize {
        self.iter.len()
    }
    fn is_empty(&self) -> bool {
        self.iter.is_empty()
    }
}

impl<'a> fmt::Debug for CommandArgs<'a> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list().entries(self.iter.clone()).finish()
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Side {
    In,
    Out,
    Err,
}

/// One of a child's piped streams: writes to its stdin are its input when it runs; a read of
/// its stdout or stderr runs it and reads what it wrote.
pub struct ChildPipe {
    run: Arc<Mutex<Run>>,
    side: Side,
}

impl ChildPipe {
    pub fn read(&self, buf: &mut [u8]) -> io::Result<usize> {
        let at = match self.side {
            Side::Out => 0,
            Side::Err => 1,
            Side::In => return Err(io::Error::from(io::ErrorKind::Unsupported)),
        };
        let mut run = self.run.lock().unwrap_or_else(|it| it.into_inner());
        let ran = run.finish()?;
        let all = if at == 0 { &ran.stdout } else { &ran.stderr };
        let rest = &all[run.read[at].min(all.len())..];
        let n = rest.len().min(buf.len());
        buf[..n].copy_from_slice(&rest[..n]);
        run.read[at] += n;
        Ok(n)
    }

    pub fn read_buf(&self, mut buf: BorrowedCursor<'_, u8>) -> io::Result<()> {
        let n = self.read(buf.ensure_init())?;
        buf.advance_checked(n);
        Ok(())
    }

    pub fn read_vectored(&self, bufs: &mut [IoSliceMut<'_>]) -> io::Result<usize> {
        match bufs.iter_mut().find(|it| !it.is_empty()) {
            Some(buf) => self.read(buf),
            None => Ok(0),
        }
    }

    pub fn is_read_vectored(&self) -> bool {
        false
    }

    pub fn read_to_end(&self, buf: &mut Vec<u8>) -> io::Result<usize> {
        let start = buf.len();
        let mut chunk = [0u8; 8192];
        loop {
            match self.read(&mut chunk)? {
                0 => return Ok(buf.len() - start),
                n => buf.extend_from_slice(&chunk[..n]),
            }
        }
    }

    pub fn write(&self, buf: &[u8]) -> io::Result<usize> {
        let mut run = self.run.lock().unwrap_or_else(|it| it.into_inner());
        if self.side != Side::In || run.pending.is_none() {
            return Err(io::Error::from(io::ErrorKind::BrokenPipe));
        }
        run.stdin.extend_from_slice(buf);
        Ok(buf.len())
    }

    pub fn write_vectored(&self, bufs: &[IoSlice<'_>]) -> io::Result<usize> {
        let mut written = 0;
        for buf in bufs {
            written += self.write(buf)?;
        }
        Ok(written)
    }

    pub fn is_write_vectored(&self) -> bool {
        true
    }
}

impl fmt::Debug for ChildPipe {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ChildPipe").finish_non_exhaustive()
    }
}

pub fn read_output(
    out: ChildPipe,
    stdout: &mut Vec<u8>,
    err: ChildPipe,
    stderr: &mut Vec<u8>,
) -> io::Result<()> {
    out.read_to_end(stdout)?;
    err.read_to_end(stderr)?;
    Ok(())
}

pub fn getpid() -> u32 {
    panic!("no pids on this platform")
}
