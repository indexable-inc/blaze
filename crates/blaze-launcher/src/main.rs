//! blaze's launcher: the process a program built for `edit` runs in
//! (docs/v2/plan/design/launcher.md, section 2).
//!
//! The file at cargo's path (`target/debug/hello`) is a clone of this binary; beside it, the
//! sidecar `hello.blaze-program` names the program and the socket of the daemon that built
//! it. Started, with the program's own arguments and environment, the launcher:
//!
//! 1. reads the sidecar and connects to the daemon (with neither, it stops and says which);
//! 2. maps its code area and starts its control thread, which says `Hello` and from then on
//!    is the one thread that writes code (`blaze_launch_rt::control`);
//! 3. waits for the daemon's word to start, runs the program's initializers as the system's
//!    loader would (`argc`, `argv`, `envp`), and calls its `main(argc, argv)` on this, the
//!    process's first thread;
//! 4. ends with what `main` returned, through `exit`, as a linked program's start code does.
//!
//! The program is this process: its arguments, environment, descriptors, signals and exit
//! status are the process's own, and nothing of it runs anywhere else. No function of it has
//! code here until it is first called.
//!
//! Told to start again (an edit that cannot be applied to a running program), the control
//! thread `execv`s this file with the same arguments: the same process id and terminal, on
//! the code the daemon has now.

// The launcher's own messages go to its stderr, as a loader's do: it has no other channel.
#![cfg(any(
    all(target_os = "macos", target_arch = "aarch64"),
    all(target_os = "linux", any(target_arch = "x86_64", target_arch = "aarch64")),
))]

use std::ffi::{CString, OsString, c_char, c_int};
use std::io::Write;
use std::os::unix::ffi::OsStringExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};

use blaze_launch::wire::ProgramId;
use blaze_launch_rt::{Config, Driver, Loaded};

/// The exit status of a launcher that could not start its program: as a shell's for a
/// command it could not run.
const CANNOT_RUN: i32 = 127;

/// Says `why` and ends the process with `status`.
fn stop(status: i32, why: &str) -> ! {
    // The process ends below whether or not its last words could be written.
    let _ = writeln!(std::io::stderr().lock(), "blaze: {why}");
    std::process::exit(status)
}

/// What a sidecar says: the daemon's socket and the program's id.
fn sidecar(exe: &Path) -> Result<(PathBuf, ProgramId), String> {
    let mut path = exe.as_os_str().to_owned();
    path.push(".blaze-program");
    let path = PathBuf::from(path);
    let text = std::fs::read_to_string(&path).map_err(|why| {
        format!(
            "{} is a program built by blaze for `edit` (a clone of blaze's launcher) and its sidecar {} \
             does not read: {why}. Build it again with blaze",
            exe.display(),
            path.display()
        )
    })?;
    let field = |name: &str| {
        text.lines().find_map(|line| line.strip_prefix(name)?.strip_prefix('=')).ok_or_else(|| {
            format!("the sidecar {} has no `{name}`: build the program again with blaze", path.display())
        })
    };
    let socket = PathBuf::from(field("socket")?);
    let hex = field("program")?;
    let mut id = [0u8; 32];
    let digits = hex.as_bytes();
    let digit = |it: u8| (it as char).to_digit(16).map(|it| it as u8);
    let whole = digits.len() == 64
        && id.iter_mut().zip(digits.chunks(2)).all(|(byte, pair)| {
            match (digit(pair[0]), digit(pair[1])) {
                (Some(high), Some(low)) => {
                    *byte = high << 4 | low;
                    true
                }
                _ => false,
            }
        });
    match whole {
        true => Ok((socket, ProgramId(id))),
        false => Err(format!(
            "the sidecar {} names program `{hex}`, which is no 64 hex digits: build the program again with blaze",
            path.display()
        )),
    }
}

/// Starts the program again in place: this file, with this process's arguments and
/// environment. Returns only when it could not, with why.
fn restart(why: &str) -> String {
    let _ = writeln!(std::io::stderr().lock(), "blaze: restart needed: {why}");
    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(error) => return format!("{why}; and this process's own path does not read: {error}"),
    };
    let c = |text: OsString| CString::new(text.into_vec()).ok();
    let Some(path) = c(exe.clone().into_os_string()) else {
        return format!("{why}; and {} has a NUL in its path", exe.display());
    };
    // alloc: the arguments of the one `execv`
    let args: Option<Vec<CString>> = std::env::args_os().map(c).collect();
    let Some(args) = args else {
        return format!("{why}; and an argument of this process has a NUL in it");
    };
    let mut argv: Vec<*const c_char> = args.iter().map(|it| it.as_ptr()).collect(); // alloc: as above
    argv.push(core::ptr::null());
    // SAFETY: `path` and every element of `argv` are C strings that live across the call,
    // and `argv` ends in a null. The environment is the process's own.
    unsafe { libc::execv(path.as_ptr(), argv.as_ptr()) };
    format!("{why}; and starting {} again failed: {}", exe.display(), std::io::Error::last_os_error())
}

unsafe extern "C" {
    /// The process's environment, as the C library keeps it.
    static environ: *const *const c_char;
}

fn main() {
    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(why) => stop(CANNOT_RUN, &format!("this process's own path does not read: {why}")),
    };
    let (socket_path, program) = match sidecar(&exe) {
        Ok(found) => found,
        Err(why) => stop(CANNOT_RUN, &why),
    };
    let socket = match UnixStream::connect(&socket_path) {
        Ok(socket) => socket,
        Err(why) => stop(
            CANNOT_RUN,
            &format!(
                "{} runs under blaze's launcher, and the daemon that built it does not answer at {}: {why}. \
                 Build it again (`blaze build` or `blaze run`), which starts the daemon",
                exe.display(),
                socket_path.display()
            ),
        ),
    };
    let driver = match Driver::new(Config::PROGRAM) {
        Ok(driver) => driver,
        Err(why) => stop(CANNOT_RUN, &format!("the launcher could not map its code area: {why}")),
    };
    let shared = driver.shared();
    let me = match blaze_launch_rt::threads::adopt(&shared) {
        Ok(me) => me,
        Err(errno) => stop(CANNOT_RUN, &format!("pthread_key_create failed with errno {errno}")),
    };
    let (ready, start) = crossbeam_channel::bounded(1);
    // The control thread: the one writer of code, for the life of the process. It is never
    // joined: the process ends through the program's `main` or an `exit` of its own.
    let control = std::thread::Builder::new().name("blaze-launch".to_owned()).spawn(move || {
        let mut records = match Loaded::new() {
            Ok(records) => records,
            Err(why) => stop(CANNOT_RUN, &format!("the launcher could not map the program's data: {why}")),
        };
        let (_driver, served) = blaze_launch_rt::run(driver, socket, program, &mut records, ready, restart);
        if let Err(why) = served {
            stop(101, &format!("the launcher stopped serving code: {why}"));
        }
    });
    if let Err(why) = control {
        stop(CANNOT_RUN, &format!("the launcher's control thread did not start: {why}"));
    }
    // The wait: for the daemon's word to start the program (its statics are defined), or
    // for why it cannot; it ends when the daemon answers or its socket closes.
    let start = match start.recv() {
        Ok(Ok(start)) => start,
        Ok(Err(why)) => stop(CANNOT_RUN, &format!("{} cannot start: {why}", exe.display())),
        Err(_) => stop(CANNOT_RUN, "the launcher's control thread ended before the program could start"),
    };

    // The program's arguments, as `main` takes them: this process's, leaked for its life.
    // alloc: the program's `argv`, once
    let args: Vec<*const c_char> = std::env::args_os()
        .map(|it| match CString::new(it.into_vec()) {
            Ok(it) => it.into_raw().cast_const(),
            // The kernel hands a process its arguments as C strings: none holds a NUL.
            Err(_) => stop(CANNOT_RUN, "an argument of this process has a NUL in it"),
        })
        .chain([core::ptr::null()])
        .collect();
    let argc = (args.len() - 1) as c_int;
    let argv = args.leak().as_ptr();
    // SAFETY: reads the C library's pointer to the environment, as a program's start does.
    let envp = unsafe { environ };
    for init in &start.inits {
        // SAFETY: `init` is the address of a static of the program's initializer section,
        // which the launcher placed and never unmaps: one word, the address of a function
        // (its entry thunk) that takes what the system's loader passes an initializer.
        let function: extern "C" fn(c_int, *const *const c_char, *const *const c_char) =
            unsafe { core::mem::transmute((*init as *const usize).read()) };
        function(argc, argv, envp);
    }
    // SAFETY: `start.entry` is the entry slot's thunk, in a mapping that is never unmapped:
    // the program's C `main`, compiled when this first call fires its stub.
    let entry: extern "C" fn(c_int, *const *const c_char) -> c_int =
        unsafe { core::mem::transmute(start.entry) };
    let status = entry(argc, argv);
    // The main thread stays a mutator to the end: `exit` runs the program's `atexit`
    // functions, which are its code.
    core::mem::forget(me);
    // SAFETY: ends the process as a C program's return from `main` does.
    unsafe { libc::exit(status) }
}
