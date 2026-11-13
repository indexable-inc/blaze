//! blaze's launcher: the process a program built for `edit` runs in
//! (docs/v2/plan/design/launcher.md, section 2).
//!
//! The file at cargo's path (`target/debug/hello`) is a clone of this binary; beside it, the
//! sidecar `hello.blaze-program` names the program and the socket of the daemon that built
//! it. Started, with the program's own arguments and environment, the launcher:
//!
//! 1. reads the sidecar and the program's start file (`hello.blaze-start`, the image of its
//!    start the daemon placed beside it, where there is one) and connects to the daemon. With
//!    no daemon listening it starts from the start file and asks no one until a first call
//!    the image does not hold, which starts the daemon the sidecar names; with no start file
//!    either, it starts that daemon now; with a sidecar that names none, it stops and says so;
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
use blaze_launch_rt::control::{Held, Join};
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

/// What a sidecar says.
struct Sidecar {
    /// The socket of the daemon that built the program.
    socket: PathBuf,
    program: ProgramId,
    /// The daemon's own `blaze`, which starts one again; `None` in a sidecar before it said.
    blaze: Option<PathBuf>,
    /// The daemon's state directory and the `BLAZE_HOME` it was started with (`None`: none).
    state: Option<PathBuf>,
    home: Option<PathBuf>,
}

/// The sidecar beside `exe`, `<exe>.blaze-program`: one `name=value` a line.
fn sidecar(exe: &Path) -> Result<Sidecar, String> {
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
    let line = |name: &str| text.lines().find_map(|line| line.strip_prefix(name)?.strip_prefix('='));
    let field = |name: &str| {
        line(name).ok_or_else(|| {
            format!("the sidecar {} has no `{name}`: build the program again with blaze", path.display())
        })
    };
    let socket = PathBuf::from(field("socket")?);
    let hex = field("program")?;
    let Some(id) = hex32(hex) else {
        return Err(format!(
            "the sidecar {} names program `{hex}`, which is no 64 hex digits: build the program again with blaze",
            path.display()
        ));
    };
    // The three a daemon writes since a launcher could start one: a sidecar without them is
    // read as before, and a launcher of it needs its daemon listening.
    let named = |name: &str| line(name).filter(|it| !it.is_empty()).map(PathBuf::from);
    Ok(Sidecar { socket, program: ProgramId(id), blaze: named("blaze"), state: named("state"), home: named("home") })
}

/// 64 hex digits as 32 bytes.
fn hex32(hex: &str) -> Option<[u8; 32]> {
    let mut id = [0u8; 32];
    let digits = hex.as_bytes();
    let digit = |it: u8| (it as char).to_digit(16).map(|it| it as u8);
    let whole = digits.len() == 64
        && id.iter_mut().zip(digits.chunks(2)).all(|(byte, pair)| match (digit(pair[0]), digit(pair[1])) {
            (Some(high), Some(low)) => {
                *byte = high << 4 | low;
                true
            }
            _ => false,
        });
    whole.then_some(id)
}

/// The start file's head: `"bzstart1" | [32] check | [32] program | [32] record`
/// (blaze-launch-host's `start::HEAD`).
const START_HEAD: usize = 104;

/// The start file's trailer: `u64 at | u64 len | "bzstartE"`, the frame's place in it.
const START_TRAILER: usize = 24;

/// A start file read whole: its bytes, where its `Tell::Image` frame is in them, and its
/// image's check word.
struct StartFile {
    bytes: Vec<u8>,
    frame: std::ops::Range<usize>,
    check: [u8; 32],
}

/// The program's start file, `<exe>.blaze-start`, read whole where it is there: the image the
/// daemon placed beside the program once it was durable, of the build the program's file is
/// of (the daemon removes it when a build gives the program another). `None`: no file. A file
/// of another program, or whose trailer does not name a frame inside it, stops the launcher
/// naming why: the daemon writes it whole or not at all.
fn start_file(exe: &Path, program: &ProgramId) -> Result<Option<StartFile>, String> {
    let mut path = exe.as_os_str().to_owned();
    path.push(".blaze-start");
    let path = PathBuf::from(path);
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(why) if why.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(why) => return Err(format!("the program's start file {} does not read: {why}", path.display())),
    };
    let bad = |what: String| format!("the program's start file {} {what}: build the program again with blaze", path.display());
    if bytes.len() < START_HEAD + START_TRAILER || &bytes[..8] != b"bzstart1" {
        return Err(bad(format!("is no start image of this layout ({} bytes)", bytes.len())));
    }
    let id = |at: usize| <[u8; 32]>::try_from(&bytes[at..at + 32]).expect("32 bytes");
    let check = id(8);
    if id(40) != program.0 {
        return Err(bad(format!(
            "is of program {}, and its sidecar names program {}",
            hex(&id(40)),
            hex(&program.0)
        )));
    }
    let tail = bytes.len() - START_TRAILER;
    let word = |at: usize| u64::from_le_bytes(bytes[at..at + 8].try_into().expect("8 bytes"));
    if &bytes[tail + 16..] != b"bzstartE" {
        return Err(bad(format!("has no trailer at byte {tail}")));
    }
    let (at, len) = (word(tail), word(tail + 8));
    let frame = usize::try_from(at)
        .ok()
        .zip(usize::try_from(len).ok())
        .and_then(|(at, len)| Some(at..at.checked_add(len)?))
        .filter(|frame| frame.start >= START_HEAD && frame.end <= tail);
    match frame {
        Some(frame) => Ok(Some(StartFile { bytes, frame, check })),
        None => Err(bad(format!("names a frame of {len} bytes at byte {at}, which is not between its head and its trailer at {tail}"))),
    }
}

/// 32 bytes as hex.
fn hex(id: &[u8; 32]) -> String {
    id.iter().map(|it| format!("{it:02x}")).collect() // alloc: an error's name, once
}

/// What starts the daemon the sidecar names and connects to it: `None` where the sidecar
/// does not say how (one written before a launcher could).
fn joiner(side: &Sidecar) -> Option<Box<dyn FnMut() -> Result<UnixStream, String> + Send>> {
    let (blaze, state) = (side.blaze.clone()?, side.state.clone()?); // clone: the closure's own, once a process
    let homes = blaze_client::start::Homes {
        socket: side.socket.clone(), // clone: as above
        state,
        home: side.home.clone(), // clone: as above
    };
    Some(Box::new(move || {
        use blaze_core::wait::{Clock as _, Deadline, Monotonic};
        let clock = Monotonic::new();
        let deadline = Deadline::after(clock.now(), blaze_launch_rt::control::JOIN_WITHIN);
        blaze_client::start::daemon(&homes, &blaze, deadline, &clock)
            .map_err(|why| format!("starting the daemon with {}: {why}", blaze.display()))?;
        UnixStream::connect(&homes.socket)
            .map_err(|why| format!("the daemon started and does not answer at {}: {why}", homes.socket.display()))
    }))
}

/// The path this process was started from, read once at its start: a build places a new
/// file at the path while the program runs, and the file this process maps is then one the
/// system no longer names.
static STARTED_FROM: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

/// Starts the program again in place: the file at the path this process was started from,
/// with this process's arguments and environment. It does not return: where the program
/// cannot be started again the process ends, saying why, since what it would run on is code
/// the daemon has replaced (rule `no-fallback`).
fn restart(why: &str) -> String {
    let _ = writeln!(std::io::stderr().lock(), "blaze: restart needed: {why}");
    let Some(exe) = STARTED_FROM.get() else {
        stop(101, "the program must start again, and the path it was started from was never read");
    };
    let c = |text: OsString| CString::new(text.into_vec()).ok();
    let Some(path) = c(exe.clone().into_os_string()) else {
        stop(101, &format!("the program must start again, and {} has a NUL in its path", exe.display()));
    };
    // alloc: the arguments of the one `execv`
    let args: Option<Vec<CString>> = std::env::args_os().map(c).collect();
    let Some(args) = args else {
        stop(101, "the program must start again, and an argument of this process has a NUL in it");
    };
    let mut argv: Vec<*const c_char> = args.iter().map(|it| it.as_ptr()).collect(); // alloc: as above
    argv.push(core::ptr::null());
    // SAFETY: `path` and every element of `argv` are C strings that live across the call,
    // and `argv` ends in a null. The environment is the process's own.
    unsafe { libc::execv(path.as_ptr(), argv.as_ptr()) };
    let failed = std::io::Error::last_os_error();
    stop(101, &format!("the program must start again, and starting {} failed: {failed}", exe.display()))
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
    // Kept for a restart in place; set once, here.
    let _ = STARTED_FROM.set(exe.clone()); // clone: the path, once a process
    let side = match sidecar(&exe) {
        Ok(found) => found,
        Err(why) => stop(CANNOT_RUN, &why),
    };
    let program = side.program;
    let start_file = match start_file(&exe, &program) {
        Ok(found) => found,
        Err(why) => stop(CANNOT_RUN, &why),
    };
    // A daemon listening is asked; with none, the program starts from its start file and a
    // daemon is started at its first call the image does not hold; with no start file either,
    // one is started now, where the sidecar says how.
    let join = match UnixStream::connect(&side.socket) {
        Ok(socket) => Join::Now(socket),
        Err(why) => match (blaze_client::start::nothing_listens(&why), &start_file, joiner(&side)) {
            (true, Some(_), Some(join)) => Join::Later(join),
            (true, None, Some(mut join)) => match join() {
                Ok(socket) => Join::Now(socket),
                Err(started) => stop(
                    CANNOT_RUN,
                    &format!("{} runs under blaze's launcher, no daemon answers at {}, and {started}", exe.display(), side.socket.display()),
                ),
            },
            _ => stop(
                CANNOT_RUN,
                &format!(
                    "{} runs under blaze's launcher, and the daemon that built it does not answer at {}: {why}. \
                     Build it again (`blaze build` or `blaze run`), which starts the daemon",
                    exe.display(),
                    side.socket.display()
                ),
            ),
        },
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
        let held = start_file.as_ref().map(|file| Held { frame: &file.bytes[file.frame.clone()], check: file.check });
        let (_driver, served) = blaze_launch_rt::run(driver, join, held, program, &mut records, ready, restart);
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
