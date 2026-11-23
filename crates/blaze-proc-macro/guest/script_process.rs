// blaze's process shim for build scripts (docs/v2/plan/design/build-scripts.md).
//
// This file takes the place of the pinned nightly's `library/std/src/sys/process/unsupported.rs`
// in the std a build script's `wasm32-wasip1` module is built with (the bootstrap builder copies
// the toolchain's library and puts this file there; `-Zbuild-std` builds from the copy). Every
// other platform piece of that std is the toolchain's own.
//
// A spawn is one host call, `blaze_build_script.spawn`, made when the child's result is first
// needed (a read of its output, a wait): the program, its arguments, the environment changes,
// the folder, the bytes written to its stdin and how its output is taken. The host (blaze's
// runner, crates/blaze-proc-macro/src/script.rs) answers `rustc` itself from the pinned
// toolchain's facts and defers a C compiler in `check`; anything it does not answer STOPS the
// script there, so the call never returns an error a script could swallow and carry on wrong.
//
// One answer is an error, because it is the one every machine gives: before a child exists,
// `Command::spawn` asks the host `blaze_build_script.found` whether the machine has the program
// (D192: the pinned base tool set and the programs blaze answers). A program it does not have is
// `io::ErrorKind::NotFound` from `spawn`, `output` and `status`, as from the OS's `execvp` (not
// exit 127: that is a shell's answer). libc's `Command::new("emcc").output().ok()?` on every
// target reads it as "no Emscripten" and carries on.
//
// The request, every number little-endian u32, a string its length then its bytes:
//   argc, argv (the program first), env_clear u8, env count, (u8 set, key, value) each,
//   cwd u8 has + string, stdin string, stdout mode u8, stderr mode u8
// (a mode: 0 inherit, 1 null, 2 pipe, 3 a file, 4 the parent's stdout, 5 the parent's stderr).
// The reply: the exit code as i32, the stdout string, the stderr string.

use super::env::{CommandEnv, CommandEnvs, CommandResolvedEnvs};
pub use crate::ffi::OsString as EnvKey;
use crate::ffi::{OsStr, OsString};
use crate::io::{BorrowedCursor, IoSlice, IoSliceMut};
use crate::num::NonZero;
use crate::path::Path;
use crate::process::StdioPipes;
use crate::sync::{Arc, Mutex};
use crate::sys::fs::File;
use crate::{fmt, io};

#[link(wasm_import_module = "blaze_build_script")]
unsafe extern "C" {
    /// Runs the request at `request` (`len` bytes): the reply's length. Never returns when
    /// the host refuses the spawn: the script stops there.
    #[link_name = "spawn"]
    fn blaze_spawn(request: *const u8, len: usize) -> usize;
    /// Whether the machine has the program named by the `len` bytes at `program`: 1 it does,
    /// 0 it does not (the OS's `NotFound`). Never returns when the host cannot say: the script
    /// stops there.
    #[link_name = "found"]
    fn blaze_found(program: *const u8, len: usize) -> u32;
    /// Copies the last reply to `into`.
    #[link_name = "reply"]
    fn blaze_reply(into: *mut u8);
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
    #[allow(dead_code)] // This variant exists only for the Debug impl
    InheritFile(File),
}

impl Stdio {
    fn mode(&self) -> u8 {
        match self {
            Stdio::Inherit => 0,
            Stdio::Null => 1,
            Stdio::MakePipe => 2,
            Stdio::InheritFile(_) => 3,
            Stdio::ParentStdout => 4,
            Stdio::ParentStderr => 5,
        }
    }
}

fn put_u32(out: &mut Vec<u8>, value: usize) {
    out.extend_from_slice(&(value as u32).to_le_bytes());
}

fn put_str(out: &mut Vec<u8>, bytes: &[u8]) {
    put_u32(out, bytes.len());
    out.extend_from_slice(bytes);
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

    /// The request without its stdin and modes: what every spawn of this command sends.
    fn request(&self) -> Vec<u8> {
        let mut out = Vec::new();
        put_u32(&mut out, self.args.len());
        put_str(&mut out, self.program.as_encoded_bytes());
        for arg in &self.args[1..] {
            put_str(&mut out, arg.as_encoded_bytes());
        }
        out.push(u8::from(self.env.does_clear()));
        let envs: Vec<(&OsStr, Option<&OsStr>)> = self.env.iter().collect();
        put_u32(&mut out, envs.len());
        for (key, value) in envs {
            out.push(u8::from(value.is_some()));
            put_str(&mut out, key.as_encoded_bytes());
            put_str(&mut out, value.map_or(&[][..], |v| v.as_encoded_bytes()));
        }
        match &self.cwd {
            Some(cwd) => {
                out.push(1);
                put_str(&mut out, cwd.as_encoded_bytes());
            }
            None => {
                out.push(0);
                put_str(&mut out, b"");
            }
        }
        out
    }

    pub fn spawn(&mut self, default: Stdio, needs_stdin: bool) -> io::Result<(Process, StdioPipes)> {
        let program = self.program.as_encoded_bytes();
        // SAFETY: `program` is live for the call; the host reads `len` bytes of it.
        if unsafe { blaze_found(program.as_ptr(), program.len()) } == 0 {
            return Err(io::const_error!(io::ErrorKind::NotFound, "No such file or directory"));
        }
        let take =|set: &Option<Stdio>, default: &Stdio| set.as_ref().map_or(default.mode(), Stdio::mode);
        let stdin = take(&self.stdin, if needs_stdin { &default } else { &Stdio::Null });
        let stdout = take(&self.stdout, &default);
        let stderr = take(&self.stderr, &default);
        let child = Arc::new(Mutex::new(Child {
            request: self.request(),
            stdin: Vec::new(),
            modes: [stdout, stderr],
            result: None,
            read: [0, 0],
        }));
        let pipe = |which: Which| ChildPipe { child: Arc::clone(&child), which };
        let pipes = StdioPipes {
            stdin: (stdin == 2).then(|| pipe(Which::Stdin)),
            stdout: (stdout == 2).then(|| pipe(Which::Stdout)),
            stderr: (stderr == 2).then(|| pipe(Which::Stderr)),
        };
        Ok((Process { child }, pipes))
    }
}

pub fn output(cmd: &mut Command) -> io::Result<(ExitStatus, Vec<u8>, Vec<u8>)> {
    let (process, _pipes) = cmd.spawn(Stdio::MakePipe, false)?;
    let mut child = process.child.lock().unwrap_or_else(|e| e.into_inner());
    child.modes = [2, 2];
    child.run();
    let (code, stdout, stderr) = child.result.take().unwrap_or((1, Vec::new(), Vec::new()));
    Ok((ExitStatus(code), stdout, stderr))
}

/// One spawned child: its request, what was written to its stdin, and its result once run.
struct Child {
    request: Vec<u8>,
    stdin: Vec<u8>,
    modes: [u8; 2],
    result: Option<(i32, Vec<u8>, Vec<u8>)>,
    /// How much of its stdout and stderr was read.
    read: [usize; 2],
}

fn take_u32(bytes: &[u8], at: &mut usize) -> usize {
    let mut word = [0; 4];
    word.copy_from_slice(&bytes[*at..*at + 4]);
    *at += 4;
    u32::from_le_bytes(word) as usize
}

impl Child {
    /// Its result: asked of the host the first time.
    fn run(&mut self) -> &(i32, Vec<u8>, Vec<u8>) {
        if self.result.is_none() {
            let mut request = core::mem::take(&mut self.request);
            put_str(&mut request, &self.stdin);
            request.extend_from_slice(&self.modes);
            // SAFETY: `request` is live for the call; the host reads `len` bytes of it.
            let len = unsafe { blaze_spawn(request.as_ptr(), request.len()) };
            let mut reply = vec![0u8; len];
            // SAFETY: the host writes exactly the `len` bytes it said its reply holds.
            unsafe { blaze_reply(reply.as_mut_ptr()) };
            let mut at = 0;
            let code = take_u32(&reply, &mut at) as u32 as i32;
            let n = take_u32(&reply, &mut at);
            let stdout = reply[at..at + n].to_vec();
            at += n;
            let n = take_u32(&reply, &mut at);
            let stderr = reply[at..at + n].to_vec();
            self.result = Some((code, stdout, stderr));
        }
        self.result.as_ref().unwrap()
    }
}

impl From<ChildPipe> for Stdio {
    fn from(_pipe: ChildPipe) -> Stdio {
        // A child's pipe handed to another child: the sandbox runs one child at a time and
        // passes nothing between them.
        Stdio::Null
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
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}", self.args[0])?;
        for arg in &self.args[1..] {
            write!(f, " {:?}", arg)?;
        }
        Ok(())
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

impl Into<ExitStatus> for ExitStatusError {
    fn into(self) -> ExitStatus {
        ExitStatus(self.0.get())
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
    child: Arc<Mutex<Child>>,
}

impl Process {
    pub fn id(&self) -> u32 {
        1
    }

    pub fn kill(&mut self) -> io::Result<()> {
        Ok(())
    }

    pub fn wait(&mut self) -> io::Result<ExitStatus> {
        let mut child = self.child.lock().unwrap_or_else(|e| e.into_inner());
        Ok(ExitStatus(child.run().0))
    }

    pub fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        self.wait().map(Some)
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
enum Which {
    Stdin,
    Stdout,
    Stderr,
}

/// One end of a child's stdio: stdin gathers what is written until the child runs; stdout and
/// stderr are read from its result.
pub struct ChildPipe {
    child: Arc<Mutex<Child>>,
    which: Which,
}

impl fmt::Debug for ChildPipe {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ChildPipe")
    }
}

impl ChildPipe {
    pub fn read(&self, buf: &mut [u8]) -> io::Result<usize> {
        let at = match self.which {
            Which::Stdin => return Err(io::const_error!(io::ErrorKind::Unsupported, "a child's stdin is not read")),
            Which::Stdout => 0,
            Which::Stderr => 1,
        };
        let mut child = self.child.lock().unwrap_or_else(|e| e.into_inner());
        let read = child.read[at];
        let (_, stdout, stderr) = child.run();
        let bytes = if at == 0 { stdout } else { stderr };
        let n = buf.len().min(bytes.len() - read);
        buf[..n].copy_from_slice(&bytes[read..read + n]);
        child.read[at] = read + n;
        Ok(n)
    }

    pub fn read_buf(&self, mut buf: BorrowedCursor<'_, u8>) -> io::Result<()> {
        let mut bytes = vec![0u8; buf.capacity()];
        let n = self.read(&mut bytes)?;
        buf.append(&bytes[..n]);
        Ok(())
    }

    pub fn read_vectored(&self, bufs: &mut [IoSliceMut<'_>]) -> io::Result<usize> {
        match bufs.iter_mut().find(|b| !b.is_empty()) {
            Some(buf) => self.read(buf),
            None => Ok(0),
        }
    }

    pub fn is_read_vectored(&self) -> bool {
        false
    }

    pub fn read_to_end(&self, buf: &mut Vec<u8>) -> io::Result<usize> {
        let mut chunk = [0u8; 4096];
        let mut total = 0;
        loop {
            let n = self.read(&mut chunk)?;
            if n == 0 {
                return Ok(total);
            }
            buf.extend_from_slice(&chunk[..n]);
            total += n;
        }
    }

    pub fn write(&self, buf: &[u8]) -> io::Result<usize> {
        if self.which != Which::Stdin {
            return Err(io::const_error!(io::ErrorKind::Unsupported, "a child's output is not written"));
        }
        let mut child = self.child.lock().unwrap_or_else(|e| e.into_inner());
        if child.result.is_some() {
            return Err(io::const_error!(io::ErrorKind::BrokenPipe, "the child has run"));
        }
        child.stdin.extend_from_slice(buf);
        Ok(buf.len())
    }

    pub fn write_vectored(&self, bufs: &[IoSlice<'_>]) -> io::Result<usize> {
        let mut total = 0;
        for buf in bufs {
            total += self.write(buf)?;
        }
        Ok(total)
    }

    pub fn is_write_vectored(&self) -> bool {
        false
    }
}

pub fn read_output(out: ChildPipe, stdout: &mut Vec<u8>, err: ChildPipe, stderr: &mut Vec<u8>) -> io::Result<()> {
    out.read_to_end(stdout)?;
    err.read_to_end(stderr)?;
    Ok(())
}

pub fn getpid() -> u32 {
    panic!("no pids on this platform")
}
