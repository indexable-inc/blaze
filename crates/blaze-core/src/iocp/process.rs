//! Starting the process of an [`crate::Effect::Spawn`] on Windows and making its [`Exit`].
//!
//! Each process is made suspended in a job object of its own, which kills whatever is left of
//! it when the job closes and, where a limit is asked for, holds its memory (the commit of the
//! whole job, `JOB_OBJECT_LIMIT_JOB_MEMORY`: Windows' hard backstop). Then it runs. Its pipes
//! are named pipes whose ends of the driver's are overlapped, read and written through the
//! port; the child inherits its three ends and nothing else (`PROC_THREAD_ATTRIBUTE_HANDLE_LIST`).
//!
//! Its end is heard from a wait of the thread pool's on the process, which posts to the port:
//! the job's messages are, as Windows documents them, not sure to arrive, so they say only
//! that the job went past its memory, which kills it.
//!
//! The process is made suspended and runs only once the driver follows it ([`follow`], then
//! [`resume`]): its job's messages and its end come to the port with its spawn's handle, which
//! names it only once it is in the driver's slab.
//!
//! A run handed to the warm launcher ([`super::warm`]) has its job and pipes made here as a
//! started one's are, and no process of the driver's.

use std::{
    cell::UnsafeCell,
    ffi::{OsString, c_void},
    io,
    marker::PhantomPinned,
    os::windows::io::{AsRawHandle as _, OwnedHandle},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use windows_sys::Win32::{
    Foundation::{GENERIC_READ, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE, STATUS_COMMITMENT_LIMIT},
    Security::SECURITY_ATTRIBUTES,
    Storage::FileSystem::{
        CreateFileW, FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_FLAG_OVERLAPPED, FILE_SHARE_READ,
        FILE_SHARE_WRITE, FILE_WRITE_ATTRIBUTES, OPEN_EXISTING, PIPE_ACCESS_INBOUND,
        PIPE_ACCESS_OUTBOUND,
    },
    System::{
        JobObjects::{
            AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_ACTIVE_PROCESS,
            JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION, JOB_OBJECT_LIMIT_JOB_MEMORY,
            JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
            JOBOBJECT_ASSOCIATE_COMPLETION_PORT, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
            JobObjectAssociateCompletionPortInformation, JobObjectExtendedLimitInformation,
            QueryInformationJobObject, SetInformationJobObject, TerminateJobObject,
        },
        Pipes::{CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_WAIT},
        Threading::{
            CREATE_NO_WINDOW, CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT, CreateProcessW,
            DeleteProcThreadAttributeList, EXTENDED_STARTUPINFO_PRESENT, GetExitCodeProcess, INFINITE,
            InitializeProcThreadAttributeList, LPPROC_THREAD_ATTRIBUTE_LIST,
            PROC_THREAD_ATTRIBUTE_HANDLE_LIST, PROCESS_CREATION_FLAGS, PROCESS_INFORMATION, RegisterWaitForSingleObject,
            ResumeThread, STARTF_USESTDHANDLES, STARTUPINFOEXW, STARTUPINFOW, TerminateProcess,
            UnregisterWaitEx, UpdateProcThreadAttribute, WT_EXECUTEONLYONCE,
        },
    },
};

use super::{
    rundirs::RunDirs,
    sys::{self, KEY_JOB, Port, WideName},
};
use crate::{Command, Confine, Exit, Sandbox};

/// What a pipe holds before its writer waits.
const PIPE: u32 = 64 << 10;

/// The exit code of a process [`crate::Effect::Kill`] ended (`TerminateJobObject`): Windows
/// has no signals, so a kill is told by its code, the one `taskkill /f` gives.
pub(super) const KILLED: u32 = 1;
/// The exit code of a process killed for going past its memory: the status Windows refuses
/// the commit with.
pub(super) const OUT_OF_MEMORY: u32 = STATUS_COMMITMENT_LIMIT as u32;

static RUNS: AtomicU64 = AtomicU64::new(0);

/// The serial of a spawn, which its pipes' and run directory's names carry.
pub(super) fn serial() -> u64 {
    RUNS.fetch_add(1, Ordering::Relaxed)
}

/// A process made, suspended until [`resume`], its pipes the driver's.
pub(super) struct Started {
    pub(super) process: OwnedHandle,
    /// Its first thread, suspended.
    pub(super) thread: OwnedHandle,
    pub(super) job: OwnedHandle,
    /// Where its input goes, of a command with input: closing it is the end of the input.
    pub(super) stdin: Option<OwnedHandle>,
    pub(super) stdout: OwnedHandle,
    pub(super) stderr: OwnedHandle,
    pub(super) finish: Finish,
}

/// What is needed to say how the process ended, once it has.
pub(super) struct Finish {
    pub(super) command: Box<Command>,
    // dyn: the caller's sandbox maker, asked once per sandboxed process.
    pub(super) confine: Option<Arc<dyn Confine>>,
    /// The run directory of a sandboxed process ([`RunDirs`]): the driver removes it once the
    /// process has ended and its sandbox has been asked what it read.
    pub(super) scratch: Option<PathBuf>,
}

/// `command` made in its sandbox, made by `confine`, unless the sandbox is off; in a job of its
/// own, suspended until the driver follows it.
// dyn: as above.
pub(super) fn start(dirs: &mut RunDirs, command: Box<Command>, confine: Option<&Arc<dyn Confine>>) -> io::Result<Started> {
    let n = serial();
    let (confined, scratch, confine) = match &command.sandbox {
        Sandbox::Off => (None, None, None),
        Sandbox::On { .. } => {
            let Some(confine) = confine else {
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    // alloc: an error path.
                    format!(
                        "{}: a sandbox is asked for and this driver was given nothing to make \
                         one (no Confine); nothing runs outside one",
                        command.program.display()
                    ),
                ));
            };
            let scratch = dirs.make(n)?;
            match confine.confine(&command, &scratch) {
                // clone: one reference count, the sandbox's to ask what the process read.
                Ok(confined) => (Some(confined), Some(scratch), Some(confine.clone())),
                Err(why) => {
                    dirs.remove(scratch);
                    // alloc: an error path.
                    return Err(io::Error::other(format!(
                        "{}: no sandbox can be made here: {why}",
                        command.program.display()
                    )));
                }
            }
        }
    };
    let launch = match &confined {
        Some(confined) => Launch {
            program: &confined.program,
            args: &confined.args,
            env: &confined.env,
            dir: &confined.dir,
        },
        None => Launch { program: &command.program, args: &command.args, env: &command.env, dir: &command.dir },
    };
    let made = run(&launch, &command, n);
    let started = match made {
        Ok(started) => started,
        Err(error) => {
            if let Some(scratch) = scratch {
                dirs.remove(scratch);
            }
            // alloc: an error path.
            return Err(io::Error::new(error.kind(), format!("starting {}: {error}", launch.program.display())));
        }
    };
    let (process, thread, job, stdin, stdout, stderr) = started;
    Ok(Started { process, thread, job, stdin, stdout, stderr, finish: Finish { command, confine, scratch } })
}

/// What a process is started as: its command's own, what its sandbox made of it, or the warm
/// launcher.
pub(super) struct Launch<'a> {
    pub(super) program: &'a Path,
    pub(super) args: &'a [OsString],
    pub(super) env: &'a [(OsString, OsString)],
    pub(super) dir: &'a Path,
}

type Handles = (OwnedHandle, OwnedHandle, OwnedHandle, Option<OwnedHandle>, OwnedHandle, OwnedHandle);

/// A run's standard handles, as a started process and a handed run have them: the driver's ends
/// (overlapped, kept from children) and the child's (inheritable), its input's the null device
/// where it is given none.
pub(super) struct Standard {
    pub(super) stdin: Option<OwnedHandle>,
    pub(super) stdout: OwnedHandle,
    pub(super) stderr: OwnedHandle,
    /// The child's input, output and error.
    pub(super) theirs: [OwnedHandle; 3],
}

pub(super) fn standard(command: &Command, n: u64) -> io::Result<Standard> {
    let (stdout, child_stdout) = pipe(true, n, "out")?;
    let (stderr, child_stderr) = pipe(true, n, "err")?;
    let (stdin, child_stdin) = match command.stdin.is_empty() {
        true => (None, nothing()?),
        false => {
            let (ours, theirs) = pipe(false, n, "in")?;
            (Some(ours), theirs)
        }
    };
    Ok(Standard { stdin, stdout, stderr, theirs: [child_stdin, child_stdout, child_stderr] })
}

fn run(launch: &Launch<'_>, command: &Command, n: u64) -> io::Result<Handles> {
    let job = job(command.memory)?;
    let Standard { stdin, stdout, stderr, theirs } = standard(command, n)?;
    let inherited = theirs.each_ref().map(|it| it.as_raw_handle());
    let flags = CREATE_UNICODE_ENVIRONMENT | EXTENDED_STARTUPINFO_PRESENT | CREATE_SUSPENDED | CREATE_NO_WINDOW;
    let (process, thread) = create(launch, inherited, &inherited, flags)?;
    // SAFETY: the job and the process just made, which has not run.
    if let Err(error) = sys::check(unsafe { AssignProcessToJobObject(job.as_raw_handle(), process.as_raw_handle()) }) {
        // SAFETY: the process just made.
        unsafe { TerminateProcess(process.as_raw_handle(), KILLED) };
        // alloc: an error path.
        return Err(io::Error::new(error.kind(), format!("into its job: {error}")));
    }
    // Its ends are its own now: the pipes end when it and what it starts close them.
    drop(theirs);
    Ok((process, thread, job, stdin, stdout, stderr))
}

/// The process `launch` says, made with `flags`: its standard handles `standard` (null for
/// none), inheriting `inherited` and nothing else (each inheritable, and open until the call
/// returns). Its process and first thread.
pub(super) fn create(
    launch: &Launch<'_>,
    standard: [HANDLE; 3],
    inherited: &[HANDLE],
    flags: PROCESS_CREATION_FLAGS,
) -> io::Result<(OwnedHandle, OwnedHandle)> {
    // alloc: what the process is started with, made once as it starts.
    let program = sys::wide(launch.program.as_os_str())?;
    let mut line = crate::create_process::command_line(launch.program, launch.args)?;
    let env = crate::create_process::environment(launch.env)?;
    let dir = sys::wide(launch.dir.as_os_str())?;
    let attributes = Attributes::new(inherited)?;
    // SAFETY: all zeros is a valid `STARTUPINFOEXW` before its fields are set.
    let mut startup: STARTUPINFOEXW = unsafe { std::mem::zeroed() };
    startup.StartupInfo.cb = size_of::<STARTUPINFOEXW>() as u32;
    startup.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
    startup.StartupInfo.hStdInput = standard[0];
    startup.StartupInfo.hStdOutput = standard[1];
    startup.StartupInfo.hStdError = standard[2];
    startup.lpAttributeList = attributes.list();
    // SAFETY: all zeros is a valid `PROCESS_INFORMATION` for the call to fill.
    let mut info: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: NUL-terminated strings, a writable command line, a Unicode environment block
    // ending in two NULs, and the attribute list and the handles it names, all of which
    // outlive the call (the handles are the caller's, open until it returns).
    sys::check(unsafe {
        CreateProcessW(
            program.as_ptr(),
            line.as_mut_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            1,
            flags,
            env.as_ptr().cast(),
            dir.as_ptr(),
            (&raw const startup).cast::<STARTUPINFOW>(),
            &mut info,
        )
    })?;
    drop(attributes);
    let process = sys::owned(info.hProcess)?;
    let thread = sys::owned(info.hThread)?;
    Ok((process, thread))
}

/// The process of `thread`, made suspended in `job`, run; or, refused, its job ended.
pub(super) fn resume(thread: OwnedHandle, job: &OwnedHandle) -> io::Result<()> {
    // SAFETY: the process's thread, suspended once at its making.
    if unsafe { ResumeThread(thread.as_raw_handle()) } == u32::MAX {
        let error = io::Error::last_os_error();
        terminate(job, KILLED);
        return Err(error);
    }
    Ok(())
}

/// The job's messages posted to `port` with `key` (at least [`KEY_JOB`]).
pub(super) fn follow(job: &OwnedHandle, port: &Port, key: usize) -> io::Result<()> {
    debug_assert!(key >= KEY_JOB);
    let associate = JOBOBJECT_ASSOCIATE_COMPLETION_PORT {
        CompletionKey: key as *mut c_void,
        CompletionPort: port.raw(),
    };
    // SAFETY: an open job, and information of the class named, of its size; the key is a
    // number, handed back with each message.
    sys::check(unsafe {
        SetInformationJobObject(
            job.as_raw_handle(),
            JobObjectAssociateCompletionPortInformation,
            (&raw const associate).cast(),
            size_of::<JOBOBJECT_ASSOCIATE_COMPLETION_PORT>() as u32,
        )
    })
}

/// A job that kills what is left in it when it closes and holds the memory of everything in
/// it to `memory`.
pub(super) fn job(memory: Option<u64>) -> io::Result<OwnedHandle> {
    // SAFETY: no security attributes or name.
    let job = sys::owned(unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) })?;
    // SAFETY: all zeros is a valid limit of nothing.
    let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
    limits.BasicLimitInformation.LimitFlags =
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE | JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION;
    if let Some(bytes) = memory {
        limits.BasicLimitInformation.LimitFlags |= JOB_OBJECT_LIMIT_JOB_MEMORY;
        limits.JobMemoryLimit = usize::try_from(bytes).unwrap_or(usize::MAX);
    }
    // SAFETY: the job, and information of the class named, of its size.
    sys::check(unsafe {
        SetInformationJobObject(
            job.as_raw_handle(),
            JobObjectExtendedLimitInformation,
            (&raw const limits).cast(),
            size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        )
    })?;
    Ok(job)
}

/// The job, and with it everything in it, ended with `code`.
pub(super) fn terminate(job: &OwnedHandle, code: u32) {
    // SAFETY: an open job.
    unsafe { TerminateJobObject(job.as_raw_handle(), code) };
}

/// No process may join `job` from here (`JOB_OBJECT_LIMIT_ACTIVE_PROCESS` of 0), its other
/// limits and its memory kept: so a run handed to the warm launcher and killed before its
/// program started can't start it after.
pub(super) fn seal(job: &OwnedHandle) -> io::Result<()> {
    // SAFETY: all zeros is a valid buffer for the call to fill.
    let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: an open job and a buffer of the class named, of its size.
    sys::check(unsafe {
        QueryInformationJobObject(
            job.as_raw_handle(),
            JobObjectExtendedLimitInformation,
            (&raw mut limits).cast(),
            size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            std::ptr::null_mut(),
        )
    })?;
    limits.BasicLimitInformation.LimitFlags |= JOB_OBJECT_LIMIT_ACTIVE_PROCESS;
    limits.BasicLimitInformation.ActiveProcessLimit = 0;
    // SAFETY: the job, and information of the class named, of its size.
    sys::check(unsafe {
        SetInformationJobObject(
            job.as_raw_handle(),
            JobObjectExtendedLimitInformation,
            (&raw const limits).cast(),
            size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        )
    })
}

/// The code `process`, which has ended, ended with.
pub(super) fn code(process: &OwnedHandle) -> io::Result<i32> {
    let mut code = 0u32;
    // SAFETY: an open process and a code to write.
    sys::check(unsafe { GetExitCodeProcess(process.as_raw_handle(), &mut code) })?;
    Ok(code as i32)
}

static PIPES: AtomicU64 = AtomicU64::new(0);

/// A pipe: the driver's end (overlapped, kept from children) and the child's (inheritable).
/// `inbound` is a pipe the child writes. Its name is made on the stack.
pub(super) fn pipe(inbound: bool, n: u64, which: &str) -> io::Result<(OwnedHandle, OwnedHandle)> {
    let mut wide = WideName::<128>::new();
    wide.text(r"\\.\pipe\blaze-")
        .number(u64::from(std::process::id()))
        .text("-")
        .number(n)
        .text("-")
        .text(which)
        .text("-")
        .number(PIPES.fetch_add(1, Ordering::Relaxed));
    let access = match inbound {
        true => PIPE_ACCESS_INBOUND,
        false => PIPE_ACCESS_OUTBOUND,
    };
    // SAFETY: a NUL-terminated name; no security attributes, so it is not inherited.
    let ours = unsafe {
        CreateNamedPipeW(
            wide.as_ptr(),
            access | FILE_FLAG_OVERLAPPED | FILE_FLAG_FIRST_PIPE_INSTANCE,
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
            1,
            PIPE,
            PIPE,
            0,
            std::ptr::null(),
        )
    };
    let ours = sys::owned(ours)?;
    let inherit = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: std::ptr::null_mut(),
        bInheritHandle: 1,
    };
    // The child's reading end may set its own pipe's state, as a console program does.
    let rights = match inbound {
        true => GENERIC_WRITE,
        false => GENERIC_READ | FILE_WRITE_ATTRIBUTES,
    };
    // SAFETY: the name just made and attributes that outlive the call.
    let theirs = unsafe {
        CreateFileW(wide.as_ptr(), rights, 0, &inherit, OPEN_EXISTING, 0, std::ptr::null_mut())
    };
    Ok((ours, sys::owned(theirs)?))
}

/// The child's input where it is given none: the null device, as on Unix.
fn nothing() -> io::Result<OwnedHandle> {
    const NUL: [u16; 4] = [b'N' as u16, b'U' as u16, b'L' as u16, 0];
    let inherit = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: std::ptr::null_mut(),
        bInheritHandle: 1,
    };
    // SAFETY: a NUL-terminated name and attributes that outlive the call.
    let handle = unsafe {
        CreateFileW(
            NUL.as_ptr(),
            GENERIC_READ,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            &inherit,
            OPEN_EXISTING,
            0,
            std::ptr::null_mut(),
        )
    };
    sys::owned(handle)
}

/// A list of one attribute: the handles the child inherits, which outlive it.
struct Attributes {
    memory: Vec<usize>,
}

impl Attributes {
    fn new(handles: &[HANDLE]) -> io::Result<Attributes> {
        let mut size = 0;
        // SAFETY: a query of the size, writing nothing but `size`; it fails, as it must.
        unsafe { InitializeProcThreadAttributeList(std::ptr::null_mut(), 1, 0, &mut size) };
        // alloc: per process, made once as it starts (CreateProcessW costs a thousand times more).
        let mut memory = vec![0usize; size.div_ceil(size_of::<usize>())];
        let list: LPPROC_THREAD_ATTRIBUTE_LIST = memory.as_mut_ptr().cast();
        // SAFETY: memory of the size asked for, aligned for the pointers it holds.
        sys::check(unsafe { InitializeProcThreadAttributeList(list, 1, 0, &mut size) })?;
        let attributes = Attributes { memory };
        // SAFETY: the list just made; the handles live in the caller's array for as long as
        // the list is used.
        sys::check(unsafe {
            UpdateProcThreadAttribute(
                attributes.list(),
                0,
                PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,
                handles.as_ptr().cast(),
                size_of_val(handles),
                std::ptr::null_mut(),
                std::ptr::null(),
            )
        })?;
        Ok(attributes)
    }

    fn list(&self) -> LPPROC_THREAD_ATTRIBUTE_LIST {
        self.memory.as_ptr().cast_mut().cast()
    }
}
impl Drop for Attributes {
    fn drop(&mut self) {
        // SAFETY: the list `new` made.
        unsafe { DeleteProcThreadAttributeList(self.list()) };
    }
}

/// What the thread pool's wait posts once the process has ended: its key
/// ([`super::sys::KEY_EXIT`] with the bits of its spawn's handle, or
/// [`super::sys::KEY_WARM`] with the warm launcher's), to the driver's port. Inline in the
/// entry of the driver's slab it is in, which never moves while it is there, and read by a
/// thread of the pool's while the wait is registered (`UnsafeCell`; no `&mut` of the entry may
/// assume it alone reaches it: `PhantomPinned`). The driver drops the entry's [`Waiting`],
/// which waits out a call in progress, before the entry leaves the slab.
pub(super) struct Waiter {
    posted: UnsafeCell<Posted>,
    _pinned: PhantomPinned,
}

#[derive(Clone, Copy)]
struct Posted {
    port: HANDLE,
    key: usize,
    id: u64,
}

impl Waiter {
    /// Posting to `port`, which outlives every wait of the driver's, `id` with `key`.
    pub(super) fn new(port: &Port, key: usize, id: u64) -> Waiter {
        Waiter { posted: UnsafeCell::new(Posted { port: port.raw(), key, id }), _pinned: PhantomPinned }
    }
}

/// Asks the thread pool to post as its [`Waiter`] says once the process has ended;
/// unregistered, and waited out, when dropped.
pub(super) struct Waiting {
    wait: HANDLE,
}

/// # Safety
/// `context` is a [`Waiter`] that stays where it is until the wait that calls this is
/// unregistered by `Waiting::drop`, which waits for a call in progress.
unsafe extern "system" fn ended(context: *mut c_void, _timed_out: bool) {
    // SAFETY: as above; the waiter is written only as it is made, before its wait.
    let Posted { port, key, id } = unsafe { *context.cast::<Posted>() };
    _ = sys::post(port, key, 0, id as usize);
}

impl Waiting {
    /// # Safety
    ///
    /// `waiter` stays where it is, alive, until the `Waiting` is dropped.
    pub(super) unsafe fn new(process: &OwnedHandle, waiter: &Waiter) -> io::Result<Waiting> {
        let context = waiter.posted.get();
        let mut wait = std::ptr::null_mut();
        // SAFETY: an open process, a callback of the signature asked for, and a context that
        // lives until the wait is unregistered (the caller's word).
        let ok = unsafe {
            RegisterWaitForSingleObject(
                &mut wait,
                process.as_raw_handle(),
                Some(ended),
                context.cast_const().cast(),
                INFINITE,
                WT_EXECUTEONLYONCE,
            )
        };
        match ok {
            0 => Err(io::Error::last_os_error()),
            _ => Ok(Waiting { wait }),
        }
    }
}

impl Drop for Waiting {
    fn drop(&mut self) {
        // SAFETY: the wait `new` registered; `INVALID_HANDLE_VALUE` waits for a callback in
        // progress to return, after which no call reads the waiter.
        unsafe { UnregisterWaitEx(self.wait, INVALID_HANDLE_VALUE) };
    }
}

impl Finish {
    pub(super) fn program(&self) -> &Path {
        &self.command.program
    }

    /// How the run ended: `code` (its process's, or its warm launcher's answer), the most
    /// memory its job held, what its sandbox says it read. The caller removes its run
    /// directory after.
    pub(super) fn exited(
        &self,
        code: io::Result<i32>,
        job: &OwnedHandle,
        stdout: Vec<u8>,
        stderr: Vec<u8>,
    ) -> io::Result<Exit> {
        let ended = code.and_then(|code| Ok((code, peak(job)?)));
        let traced = match (&ended, &self.command.sandbox, &self.confine, &self.scratch) {
            (Ok(_), Sandbox::On { trace: true, .. }, Some(confine), Some(scratch)) => {
                Some(confine.traced(&self.command, scratch).map_err(|why| {
                    // alloc: an error path.
                    io::Error::other(format!(
                        "{}: its sandbox's record of what it read: {why}",
                        self.command.program.display()
                    ))
                }))
            }
            _ => None,
        };
        let (code, peak_memory) = ended?;
        let traced = traced.transpose()?;
        Ok(Exit { code: Some(code), signal: None, stdout, stderr, traced, peak_memory })
    }
}

/// The most memory (commit) everything in the job held at once.
fn peak(job: &OwnedHandle) -> io::Result<u64> {
    // SAFETY: all zeros is a valid buffer for the call to fill.
    let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: an open job and a buffer of the class named, of its size.
    sys::check(unsafe {
        QueryInformationJobObject(
            job.as_raw_handle(),
            JobObjectExtendedLimitInformation,
            (&raw mut limits).cast(),
            size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            std::ptr::null_mut(),
        )
    })?;
    Ok(limits.PeakJobMemoryUsed as u64)
}
