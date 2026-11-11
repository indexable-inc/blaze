//! `blaze sandbox --v2 SPEC_FILE`: blaze's launcher for a spawn whose sandbox is on
//! ([`blaze_usercode::confine`]). It reads the [`confine::Spec`], runs the program with
//! exactly the spec's environment, in its directory, in the platform's sandbox (the Linux and
//! macOS ones of [`super::backend`], [`super::windows`] on Windows), appends each line of what
//! the sandbox sees to [`confine::Spec::live`] as it sees it, and as it ends writes its
//! [`Report`] to [`confine::Spec::report`], whole or not at all, and ends as the program did.
//!
//! The report says the sandbox could not be made ([`Report::NotMade`], nothing ran), or that the
//! program ran and, when the spec traces, what it read: the live record's lines of this run,
//! hashed as the files are when it ends ([`Disk`]). A file read that can't be read then is no
//! report at all: the error is on stderr, and the driver finds none.
//!
//! What a traced run reads of its environment: a process may walk `environ` (Rust's
//! `env::vars`, C's `envp` and `environ`), which no call shows, so reads by name (the `E` lines
//! the interposers add) are all of it only for variables a walk can't find. A run whose spec
//! names the variables its key has ([`confine::Spec::given`]) is lent the rest on Linux and
//! macOS. Its program's environment holds the visible variables alone: the given ones, those
//! the sandbox reads itself (`READ_BY_THE_SANDBOX`) and those the C library and the loader read
//! with no call an interposer sees ([`READ_BY_THE_C_LIBRARY`]), which are in every such run's
//! key, set or not, so the program behaves as it would with its whole environment and its key
//! has what they change. Each other variable is an entry [`LENT`] (`__BLAZE_LENT=NAME=VALUE`),
//! which the interposer takes out of the environment as it loads and answers `getenv` from,
//! tracing the read. When the program walks its environment (std says so before it does,
//! through `__blaze_environ_walked`; `system` and `popen` on Linux run a shell the interposer
//! can't hand anything) the interposer puts every lent variable back and notes
//! `E\t<scope>\t* <why>`: from then on it read all of them. A walk the interposer is not told
//! of (C code reading `environ` itself) sees the visible variables alone, which the key has.
//!
//! A program the process runs with its own environment gets every lent variable back in its
//! environment, as it would with no sandbox, and the visible names ([`VISIBLE`]): running it is
//! no read by this process. The child's interposer lends itself the rest when its executable
//! proves std alone walks its environment (the linker's export `__blaze_std_says_walks`, link's
//! part of `fed1eb428d:docs/v2/requests/usercode.md` item 6); a child without it (a C compiler, a shell,
//! which imports its environment through `main`'s `envp`) notes `E\t<scope>\t* <why>` as it
//! loads. A child the interposer can't load in (a static program, one macOS protects) is the
//! record's `X` line, as before: the run is not whole and is keyed as cargo keys it.
//!
//! So such a run is keyed by the variables it and its children read, and a variable none of
//! them read (a shell's `OLDPWD`) is in no key. A run that names no given variables, and every
//! traced run on Windows, reads the whole of its environment (`E\t\t*`, every scope's) from
//! the start.

#![deny(unsafe_code)]

use std::{
    ffi::OsString,
    io::Write as _,
    path::{Path, PathBuf},
};

use blaze_core::exit::Exit;
use blaze_usercode::{
    confine,
    inputs::Disk,
    trace::{self, Report, Seen},
};

use super::Live;

#[cfg(unix)]
#[allow(unsafe_code, reason = "fstat and fcntl of a descriptor given by number")]
mod raw;

/// Why a traced run on Windows is not whole: what it reads is not seen there.
pub const NOT_TRACED_ON_WINDOWS: &str = "what a program reads is not traced on Windows";

/// The name of each entry a variable lent to the program is in, `__BLAZE_LENT=NAME=VALUE`,
/// which the interposers (`linux_interpose.c`, `macos_interpose.c`) take out of the program's
/// environment as they load.
pub const LENT: &str = "__BLAZE_LENT";

/// The variable that names, `NAME=NAME=...`, the variables a lending run keeps visible: the
/// given ones, `READ_BY_THE_SANDBOX` and [`READ_BY_THE_C_LIBRARY`]. The interposers pass it on
/// to each program a process runs, whose own interposer lends that program the rest when the
/// program proves it says its walks (the module's doc).
pub const VISIBLE: &str = "__BLAZE_VISIBLE";

/// The variables the C library and the loader read themselves, with calls no interposer sees:
/// the loader before the interposer loads, and the C library through its own `getenv`
/// (glibc's `localtime` reads `TZ`, `setlocale` reads `LANG` and `LC_*`, `execvp` and
/// `posix_spawnp` read `PATH`, libSystem likewise). A lending run keeps each in its program's
/// environment, so the program behaves as with its whole environment, and is keyed by each, set
/// or not. The one list of them, for glibc (its `unsecvars.h`, `LC_*` categories, locale,
/// time zone, `malloc` and resolver variables, the loader's `LD_*`), dyld (`DYLD_*`, man dyld)
/// and libmalloc (`Malloc*`); names the platform does not read are only in the key. A name
/// added to the C library the toolchain pins is added here.
pub const READ_BY_THE_C_LIBRARY: &[&str] = &[
    // Programs found along it: `execvp`, `execlp`, `posix_spawnp`, `system`'s shell.
    "PATH",
    // Time zone.
    "TZ",
    "TZDIR",
    // Locale, messages and character sets.
    "LANG",
    "LANGUAGE",
    "LC_ALL",
    "LC_ADDRESS",
    "LC_COLLATE",
    "LC_CTYPE",
    "LC_IDENTIFICATION",
    "LC_MEASUREMENT",
    "LC_MESSAGES",
    "LC_MONETARY",
    "LC_NAME",
    "LC_NUMERIC",
    "LC_PAPER",
    "LC_TELEPHONE",
    "LC_TIME",
    "LOCPATH",
    "NLSPATH",
    "OUTPUT_CHARSET",
    "GCONV_PATH",
    "GETCONF_DIR",
    "POSIXLY_CORRECT",
    "DATEMSK",
    "LIBC_FATAL_STDERR_",
    // glibc's malloc.
    "GLIBC_TUNABLES",
    "MALLOC_ARENA_MAX",
    "MALLOC_ARENA_TEST",
    "MALLOC_CHECK_",
    "MALLOC_MMAP_MAX_",
    "MALLOC_MMAP_THRESHOLD_",
    "MALLOC_PERTURB_",
    "MALLOC_TOP_PAD_",
    "MALLOC_TRACE",
    "MALLOC_TRIM_THRESHOLD_",
    // The resolver.
    "HOSTALIASES",
    "LOCALDOMAIN",
    "NIS_PATH",
    "RESOLV_HOST_CONF",
    "RES_OPTIONS",
    // glibc's loader.
    "LD_ASSUME_KERNEL",
    "LD_AUDIT",
    "LD_BIND_NOT",
    "LD_BIND_NOW",
    "LD_DEBUG",
    "LD_DEBUG_OUTPUT",
    "LD_DYNAMIC_WEAK",
    "LD_HWCAP_MASK",
    "LD_LIBRARY_PATH",
    "LD_ORIGIN_PATH",
    "LD_PROFILE",
    "LD_PROFILE_OUTPUT",
    "LD_SHOW_AUXV",
    "LD_USE_LOAD_BIAS",
    // dyld.
    "DYLD_FALLBACK_FRAMEWORK_PATH",
    "DYLD_FALLBACK_LIBRARY_PATH",
    "DYLD_FRAMEWORK_PATH",
    "DYLD_IMAGE_SUFFIX",
    "DYLD_LIBRARY_PATH",
    "DYLD_PRINT_APIS",
    "DYLD_PRINT_BINDINGS",
    "DYLD_PRINT_ENV",
    "DYLD_PRINT_INITIALIZERS",
    "DYLD_PRINT_LIBRARIES",
    "DYLD_PRINT_LINKS_WITH",
    "DYLD_PRINT_LOADERS",
    "DYLD_PRINT_SEARCHING",
    "DYLD_PRINT_SEGMENTS",
    "DYLD_PRINT_TO_FILE",
    "DYLD_SHARED_CACHE_DIR",
    "DYLD_SHARED_REGION",
    "DYLD_VERSIONED_FRAMEWORK_PATH",
    "DYLD_VERSIONED_LIBRARY_PATH",
    // libmalloc, which reads them from the start-up environment itself.
    "MallocCheckHeapAbort",
    "MallocCheckHeapEach",
    "MallocCheckHeapSleep",
    "MallocCheckHeapStart",
    "MallocCorruptionAbort",
    "MallocDebugReport",
    "MallocDoNotProtectPostlude",
    "MallocDoNotProtectPrelude",
    "MallocErrorAbort",
    "MallocGuardEdges",
    "MallocHelp",
    "MallocLogFile",
    "MallocNanoZone",
    "MallocPreScribble",
    "MallocProbGuard",
    "MallocScribble",
    "MallocSpaceEfficient",
    "MallocStackLogging",
    "MallocStackLoggingDirectory",
    "MallocStackLoggingNoCompact",
    "MallocZeroOnFree",
];

/// Whether the program of `spec` is lent the variables its key does not have, rather than
/// given its whole environment: when it is traced, its spec names them, and the platform's
/// interposer can lend them (Linux and macOS; Windows traces nothing).
pub fn lends(spec: &confine::Spec) -> bool {
    cfg!(unix) && spec.trace && spec.given.is_some()
}

/// `blaze sandbox --v2 SPEC_FILE`, the rest of the arguments after `--v2`.
pub fn command(mut args: impl Iterator<Item = OsString>) -> ! {
    let (Some(file), None) = (args.next(), args.next()) else {
        eprintln!("blaze sandbox: needs one spec: blaze sandbox --v2 SPEC_FILE");
        std::process::exit(Exit::CouldNot.code());
    };
    let spec = match read_spec(Path::new(&file)) {
        Ok(spec) => spec,
        Err(why) => {
            eprintln!("blaze sandbox: {why}");
            std::process::exit(Exit::CouldNot.code());
        }
    };
    std::process::exit(launch(&spec))
}

/// The spec in `file`, which has no field [`confine::Spec`] does not know. The file is removed
/// as it is read, before anything runs: it holds the program's whole environment, secrets
/// too, which outlive the run nowhere. One that can't be removed is an error.
pub fn read_spec(file: &Path) -> Result<confine::Spec, String> {
    // alloc: the error, once, and nothing runs.
    let text =
        std::fs::read(file).map_err(|error| format!("the spec {}: {error}", file.display()))?;
    // alloc: the error, once, and nothing runs.
    std::fs::remove_file(file)
        .map_err(|error| format!("removing the spec {} once read: {error}", file.display()))?;
    // alloc: the error, once, and nothing runs.
    serde_json::from_slice(&text).map_err(|error| format!("the spec {}: {error}", file.display()))
}

/// Whether `--v2` can make its sandbox here, and why not: the probe makes one around nothing
/// with a live record, in a process of its own, once per process that asks.
pub fn available() -> Result<(), String> {
    super::probed(super::Probe::V2)
}

/// `blaze sandbox --v2-serve`, with nothing after it: the warm launcher ([`confine::SERVE`]),
/// which reads its runs on its standard input. Windows' alone: on Linux and macOS the driver
/// starts `blaze sandbox --v2 SPEC_FILE` for each run.
pub fn serve(mut args: impl Iterator<Item = OsString>) -> ! {
    if args.next().is_some() {
        eprintln!(
            "blaze sandbox: --v2-serve takes nothing after it: its runs come on its standard input"
        );
        std::process::exit(Exit::CouldNot.code());
    }
    #[cfg(windows)]
    super::windows::serve::serve();
    #[cfg(not(windows))]
    {
        eprintln!(
            "blaze sandbox: there is no warm launcher on {}: its driver starts `blaze sandbox --v2 SPEC_FILE` for each run",
            std::env::consts::OS
        );
        std::process::exit(Exit::CouldNot.code())
    }
}

/// Runs `spec` and writes its report: what the launcher ends with, the program's status (128
/// and the signal that ended it), or [`Exit::CouldNot`] when the sandbox could not be made.
pub fn launch(spec: &confine::Spec) -> i32 {
    launch_with(spec, run)
}

/// [`launch`], with `run` running the program in its sandbox: the warm launcher's runs are given
/// the driver's handles (`windows::Given`).
pub(super) fn launch_with(
    spec: &confine::Spec,
    run: impl FnOnce(&confine::Spec, &Live) -> Result<i32, String>,
) -> i32 {
    let live = match begin(spec) {
        Ok(live) => live,
        Err(why) => return not_made(spec, &why),
    };
    let code = match run(spec, &live) {
        Ok(code) => code,
        Err(why) => return not_made(spec, &why),
    };
    match finish(spec, &live) {
        Ok(report) => {
            if let Err(why) = write_report(&spec.report, &report) {
                eprintln!("blaze: the sandbox's report: {why}");
            }
        }
        Err(why) => eprintln!("blaze: what the program read: {why}"),
    }
    code
}

/// Says why the sandbox could not be made, on stderr and in the report.
fn not_made(spec: &confine::Spec, why: &str) -> i32 {
    eprintln!("blaze: the sandbox could not be made: {why}");
    // alloc: the error, once, and nothing runs.
    if let Err(error) = write_report(&spec.report, &Report::NotMade(why.to_owned())) {
        eprintln!("blaze: the sandbox's report: {error}");
    }
    Exit::CouldNot.code()
}

/// Opens the live record, where this run's lines start at its present end, and appends what is
/// known before anything runs: when the run traces, that it reads the whole environment (or,
/// when it [`lends`], the variables the sandbox, the loader and the C library read), and what it was given that it may read
/// without a call the sandbox sees.
fn begin(spec: &confine::Spec) -> Result<Live, String> {
    let shown = spec.live.display();
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&spec.live)
        // alloc: the error, once, and nothing runs.
        .map_err(|error| format!("the live record {shown}: {error}"))?;
    // alloc: the error, once, and nothing runs.
    let start = file.metadata().map_err(|error| format!("the live record {shown}: {error}"))?.len();
    // Once per run, before anything runs: its first lines.
    let mut first = Vec::with_capacity(8);
    if spec.trace {
        if lends(spec) {
            // What the sandbox, the loader and the C library read with no call traced, set or
            // not: a value set later is one they read.
            #[cfg(unix)]
            first.extend(
                READ_BY_THE_SANDBOX
                    .iter()
                    .chain(READ_BY_THE_C_LIBRARY)
                    .map(|name| trace::Line::new(Seen::Env, "", name)),
            );
        } else {
            first.push(trace::Line::new(Seen::Env, "", trace::ALL_ENV));
        }
        first.extend(given().into_iter().map(|why| trace::Line::new(Seen::Untraceable, "", why)));
        if cfg!(windows) {
            first.push(trace::Line::new(Seen::Incomplete, "", NOT_TRACED_ON_WINDOWS));
        }
    }
    // alloc: as above.
    let text: String = first.iter().map(trace::Line::text).collect();
    // alloc: the error, once, and nothing runs.
    file.write_all(text.as_bytes()).map_err(|error| format!("the live record {shown}: {error}"))?;
    // clone: once per run, the live record's path for the sandbox.
    Ok(Live { path: spec.live.clone(), start, failed: Default::default() })
}

/// The `T` lines for the launcher's standard descriptors the program is given and may read
/// without naming a path: a file or a directory open for reading (a pipe, a terminal or
/// `/dev/null` is none).
#[cfg(unix)]
fn given() -> Vec<&'static str> {
    let mut out = Vec::with_capacity(3);
    for fd in 0..3 {
        let Some((kind, readable)) = raw::opened(fd) else { continue };
        if readable && (kind == libc::S_IFREG || kind == libc::S_IFDIR) {
            out.push(trace::untraceable_name(blaze_core::Untraceable::Descriptor));
        }
    }
    out
}

/// Windows traces nothing: the run is not whole either way ([`NOT_TRACED_ON_WINDOWS`]).
#[cfg(not(unix))]
fn given() -> Vec<&'static str> {
    Vec::with_capacity(0)
}

/// The program's run in the platform's sandbox, which appends to `live`.
#[cfg(unix)]
fn run(spec: &confine::Spec, live: &Live) -> Result<i32, String> {
    let made = backend_spec(spec)?;
    // The program's command line, once per run, for the kernel.
    let mut program = Vec::with_capacity(1 + spec.args.len());
    // clone: as above.
    program.push(spec.program.clone().into_os_string());
    program.extend(spec.args.iter().cloned());
    let env = program_env(spec);
    let also = super::Also {
        network: spec.network,
        live: Some(live),
        env: Some(&*env),
        services: &spec.services,
    };
    super::backend(&made, Some(&program), also)
}

/// The environment the program of `spec` is started in: [`confine::Spec::env`] as it is, or,
/// when it [`lends`], the visible variables as they are (the given ones,
/// `READ_BY_THE_SANDBOX`, [`READ_BY_THE_C_LIBRARY`]), each other one as an entry [`LENT`] for
/// the interposer to lend, and [`VISIBLE`], their names, for the programs it runs.
#[cfg(unix)]
fn program_env(spec: &confine::Spec) -> std::borrow::Cow<'_, [(OsString, OsString)]> {
    let Some(given) = spec.given.as_deref().filter(|_| lends(spec)) else {
        return std::borrow::Cow::Borrowed(&spec.env);
    };
    // The run's provider is in the program's environment as it is, never lent: std reads it
    // from `environ` itself before `main`, and it is no variable the program's key lacks
    // (`blaze_usercode::provider`).
    let ours = || {
        READ_BY_THE_SANDBOX
            .iter()
            .chain(READ_BY_THE_C_LIBRARY)
            .copied()
            .chain([blaze_usercode::provider::VAR])
    };
    // Once per run, as the sandbox is set up.
    let mut env = Vec::with_capacity(spec.env.len() + 1);
    for (name, value) in &spec.env {
        let visible = name.to_str().is_some_and(|it| {
            ours().any(|ours| ours == it) || given.iter().any(|given| given == it)
        });
        match visible {
            // clone: as above.
            true => env.push((name.clone(), value.clone())),
            false => {
                // alloc: as above.
                let mut entry = OsString::with_capacity(name.len() + 1 + value.len());
                entry.push(name);
                entry.push("=");
                entry.push(value);
                env.push((OsString::from(LENT), entry));
            }
        }
    }
    // alloc: as above. A given name has no `=` (`confine::given_value` refused it).
    let names: Vec<&str> = given.iter().map(String::as_str).chain(ours()).collect();
    env.push((OsString::from(VISIBLE), OsString::from(names.join("="))));
    std::borrow::Cow::Owned(env)
}

#[cfg(windows)]
fn run(spec: &confine::Spec, _live: &Live) -> Result<i32, String> {
    super::windows::run(spec, super::windows::Given::own())
}

#[cfg(not(any(unix, windows)))]
fn run(_spec: &confine::Spec, _live: &Live) -> Result<i32, String> {
    // alloc: the error, once, and nothing runs.
    Err(format!("there is no sandbox on {} yet", std::env::consts::OS))
}

/// The variables the Linux and macOS sandboxes read themselves, of the program's environment:
/// they are taken as text, and never lent (`program_env`): the program's loader reads them
/// before anything is traced. The C library's own are [`READ_BY_THE_C_LIBRARY`].
#[cfg(unix)]
const READ_BY_THE_SANDBOX: &[&str] = &["TMPDIR", "LD_PRELOAD", "DYLD_INSERT_LIBRARIES"];

/// What the Linux and macOS sandboxes are given for `spec`: its directories, and of its
/// environment what is text (the program is given all of it as it is, [`super::Also::env`]).
#[cfg(unix)]
fn backend_spec(spec: &confine::Spec) -> Result<super::Spec, String> {
    // Once per run, as the sandbox is set up.
    let mut env = Vec::with_capacity(spec.env.len());
    for (name, value) in &spec.env {
        match (name.to_str(), value.to_str()) {
            // alloc: as above.
            (Some(name), Some(value)) => env.push((name.to_owned(), value.to_owned())),
            (Some(name), None) if READ_BY_THE_SANDBOX.contains(&name) => {
                // alloc: the error, once, and nothing runs.
                return Err(format!("`{name}` is not UTF-8, and the sandbox reads it as a path"));
            }
            _ => {}
        }
    }
    Ok(super::Spec {
        // clone: once per run, as `env` is.
        cwd: spec.dir.clone(),
        env,
        overlays: Vec::with_capacity(0),
        // clone: as `cwd` is.
        writable: spec.write.clone(),
        // clone: as `cwd` is.
        readable: spec.read.clone(),
        record: None,
        record_reads: spec.trace,
        // clone: as `cwd` is.
        not_inputs: spec.not_inputs.clone(),
        view: None,
    })
}

/// What the run leaves for the driver: its lines of the live record, read back, and when it
/// traces what they read now. A line that is not one, or one cut short, makes the record not
/// whole. A file read that can't be read is an error: no report.
fn finish(spec: &confine::Spec, live: &Live) -> Result<Report, String> {
    if let Some(why) = live.failed.borrow().as_deref() {
        // alloc: the error, once, and no report.
        return Err(format!(
            "a line could not be added to the live record {}: {why}",
            live.path.display()
        ));
    }
    if !spec.trace {
        return Ok(Report::Ran(None));
    }
    let bytes = std::fs::read(&live.path)
        // alloc: the error, once, and no report.
        .map_err(|error| format!("the live record {}: {error}", live.path.display()))?;
    let ours = bytes.get(live.start as usize..).unwrap_or_default();
    // alloc: once, as the run ends: the record read as text, where it is not UTF-8.
    let lines = lines_of(&String::from_utf8_lossy(ours));
    // clone: once, as the run ends, the environment the files are hashed by.
    let traced = trace::assemble(&lines, None, &mut Disk::new(spec.env.clone()))
        // alloc: the error, once, and no report.
        .map_err(|error| error.to_string())?;
    Ok(Report::Ran(Some(traced)))
}

/// The lines of `text`, with an `X` line for each piece that is not a line and for a last one
/// with no line break.
fn lines_of(text: &str) -> Vec<trace::Line> {
    // Once, as the run ends.
    let mut lines = Vec::with_capacity(text.len() / 32);
    for piece in text.split_inclusive('\n') {
        let parsed = piece.strip_suffix('\n').map(|it| (it, trace::Line::parse(it)));
        match parsed {
            Some((_, Some(line))) => lines.push(line),
            Some((line, None)) => {
                // alloc: a line that is not one, and the record is not whole.
                let shown: String = line.chars().take(200).collect();
                // alloc: as above.
                let why = format!("the live record has a line that is not one: {shown:?}");
                lines.push(trace::Line::new(Seen::Incomplete, "", &why));
            }
            None => {
                let why = "the live record ends in a line cut short";
                lines.push(trace::Line::new(Seen::Incomplete, "", why));
            }
        }
    }
    lines
}

/// Writes `report` to `file` whole or not at all: beside it first, then moved there.
fn write_report(file: &Path, report: &Report) -> Result<(), String> {
    // Once, as the run ends: where the report is written first.
    // alloc: as above.
    let mut writing = file.as_os_str().to_owned();
    // alloc: as above.
    writing.push(format!(".writing-{}", std::process::id()));
    let writing = PathBuf::from(writing);
    std::fs::write(&writing, trace::encode(report))
        .and_then(|()| std::fs::rename(&writing, file))
        .map_err(|error| {
            _ = std::fs::remove_file(&writing);
            // alloc: the error, once, and no report.
            format!("{}: {error}", file.display())
        })
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use blaze_usercode::{
        confine,
        trace::{self, Report, Seen},
    };

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("blaze-v2-{name}-{}", std::process::id()));
        _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn spec(dir: &Path, program: &str, args: &[&str], trace: bool) -> confine::Spec {
        let out = dir.join("out");
        std::fs::create_dir_all(&out).unwrap();
        confine::Spec {
            program: program.into(),
            args: args.iter().map(Into::into).collect(),
            env: vec![("PATH".into(), "/usr/bin:/bin".into()), ("OUT".into(), out.clone().into())],
            dir: out.clone(),
            read: Vec::new(),
            write: vec![out],
            network: false,
            trace,
            services: Vec::new(),
            live: dir.join("live"),
            report: dir.join("report"),
            not_inputs: vec![program.into()],
            given: None,
        }
    }

    #[test]
    fn a_spec_with_a_field_it_does_not_know_is_refused() {
        let dir = scratch("spec");
        let file = dir.join("spec.json");
        let mut value = serde_json::to_value(spec(&dir, "/bin/true", &[], true)).unwrap();
        std::fs::write(&file, serde_json::to_vec(&value).unwrap()).unwrap();
        assert!(super::read_spec(&file).is_ok());
        assert!(
            !file.exists(),
            "a spec, which holds the program's environment, is removed once read"
        );
        assert!(super::read_spec(&file).unwrap_err().contains("spec"), "read once");
        value["extra"] = serde_json::Value::Bool(true);
        std::fs::write(&file, serde_json::to_vec(&value).unwrap()).unwrap();
        let why = super::read_spec(&file).unwrap_err();
        assert!(why.contains("extra"), "{why}");
        _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_report_is_written_whole_beside_and_moved_in() {
        let dir = scratch("report");
        let file = dir.join("report");
        std::fs::write(&file, b"old").unwrap();
        let report = Report::NotMade("no".into());
        super::write_report(&file, &report).unwrap();
        assert_eq!(trace::decode(&std::fs::read(&file).unwrap()), Ok(report));
        let left: Vec<_> =
            std::fs::read_dir(&dir).unwrap().flatten().map(|it| it.file_name()).collect();
        assert_eq!(left, ["report"], "nothing is left beside it");
        assert!(super::write_report(&dir.join("missing/report"), &Report::Ran(None)).is_err());
        _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_line_that_is_not_one_or_is_cut_short_makes_the_record_not_whole() {
        let lines = super::lines_of("R\ta\t/x\nnot a line\nD\t\t/d\nR\t\t/cut");
        let seen: Vec<Seen> = lines.iter().map(|it| it.seen).collect();
        assert_eq!(seen, [Seen::Read, Seen::Incomplete, Seen::Listed, Seen::Incomplete]);
        assert_eq!(lines[0].scope, "a");
        assert!(lines[1].what.contains("not a line"));
        assert!(lines[3].what.contains("cut short"));
    }

    #[test]
    fn a_traced_run_reads_the_whole_environment_from_the_start() {
        let dir = scratch("begin");
        let spec = spec(&dir, "/bin/true", &[], true);
        std::fs::write(&spec.live, "R\tearlier\t/x\n").unwrap();
        let live = super::begin(&spec).unwrap();
        assert_eq!(live.start, "R\tearlier\t/x\n".len() as u64);
        let text = std::fs::read_to_string(&spec.live).unwrap();
        let (lines, _) = trace::parse(&text[live.start as usize..]).unwrap();
        assert_eq!(lines[0], trace::Line::new(Seen::Env, "", trace::ALL_ENV));
        let report = super::finish(&spec, &live).unwrap();
        let Report::Ran(Some(traced)) = report else { panic!("{report:?}") };
        // Every variable it was given, and the names' hash: a variable added is one it sees.
        let names: Vec<_> = traced.env.iter().map(|it| it.0.clone()).collect();
        assert_eq!(
            names,
            [std::ffi::OsString::from(trace::ALL_ENV_NAME), "OUT".into(), "PATH".into()]
        );
        assert!(
            !traced.files.iter().any(|it| it.0.as_path() == Path::new("/x")),
            "not this run's line"
        );
        let untraced = self::spec(&dir, "/bin/true", &[], false);
        std::fs::write(&untraced.live, "").unwrap();
        let live = super::begin(&untraced).unwrap();
        assert_eq!(std::fs::read_to_string(&untraced.live).unwrap(), "");
        assert_eq!(super::finish(&untraced, &live), Ok(Report::Ran(None)));
        _ = std::fs::remove_dir_all(&dir);
    }

    /// A run whose spec names the variables its key has is lent the rest: they are no
    /// entries of its environment but [`super::LENT`] ones, for the interposer, and it reads
    /// none of them until it reads one by name or walks its environment. What the sandbox and
    /// the C library read themselves stays visible and is noted read from the start, set or
    /// not; the visible names go to the programs it runs.
    #[cfg(unix)]
    #[test]
    fn a_run_that_names_what_its_key_has_is_lent_the_rest() {
        let dir = scratch("lent");
        let mut spec = spec(&dir, "/bin/true", &[], true);
        spec.env.push(("OLDPWD".into(), "/home/u".into()));
        spec.env.push(("LD_PRELOAD".into(), "/l.so".into()));
        spec.env.push(("LANG".into(), "C.UTF-8".into()));
        spec.given = Some(vec!["OUT".into()]);
        assert!(super::lends(&spec));
        let env = super::program_env(&spec);
        let (path, out) = (spec.env[0].clone(), spec.env[1].clone());
        assert_eq!(
            env[..env.len() - 1],
            [
                path,
                out,
                (super::LENT.into(), "OLDPWD=/home/u".into()),
                ("LD_PRELOAD".into(), "/l.so".into()),
                ("LANG".into(), "C.UTF-8".into()),
            ]
        );
        let (name, visible) = &env[env.len() - 1];
        assert_eq!(name, super::VISIBLE);
        let visible: Vec<_> = visible.to_str().unwrap().split('=').collect();
        assert!(
            ["OUT", "PATH", "TZ", "LANG", "LC_ALL", "TMPDIR", "LD_PRELOAD"]
                .iter()
                .all(|it| visible.contains(it))
        );
        assert!(!visible.contains(&"OLDPWD"));
        let live = super::begin(&spec).unwrap();
        let text = std::fs::read_to_string(&spec.live).unwrap();
        let (lines, _) = trace::parse(&text[live.start as usize..]).unwrap();
        assert!(
            !lines.iter().any(|it| it.seen == Seen::Env && trace::whole_env(&it.what)),
            "{text}"
        );
        let report = super::finish(&spec, &live).unwrap();
        let Report::Ran(Some(traced)) = report else { panic!("{report:?}") };
        // The sandbox's and the C library's variables, set or not, and no other.
        let mut ours: Vec<&str> = super::READ_BY_THE_SANDBOX
            .iter()
            .chain(super::READ_BY_THE_C_LIBRARY)
            .copied()
            .collect();
        ours.sort_unstable();
        ours.dedup();
        let read: Vec<_> = traced.env.iter().map(|it| it.0.to_str().unwrap()).collect();
        assert_eq!(read, ours);
        let set: Vec<_> = traced
            .env
            .iter()
            .filter(|it| it.1.is_some())
            .map(|it| it.0.to_str().unwrap())
            .collect();
        assert_eq!(set, ["LANG", "LD_PRELOAD", "PATH"]);
        // Untraced, or naming nothing, it is given its environment as it is.
        spec.given = None;
        assert!(!super::lends(&spec) && *super::program_env(&spec) == *spec.env);
        _ = std::fs::remove_dir_all(&dir);
    }

    /// A run of `sh` in the sandbox, where one can be made: it writes where it may, its report
    /// is whole and has what it read, and its status is the program's. Skipped where no
    /// sandbox can be made, unless `BLAZE_VIEW_TESTS=require`.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn the_launcher_runs_the_program_and_reports_what_it_read() {
        // Not `available`, which runs `blaze sandbox --probe`: this is no blaze.
        if let Err(why) = crate::sandbox::alone::probe(crate::sandbox::Probe::V2) {
            let required = std::env::var("BLAZE_VIEW_TESTS").is_ok_and(|it| it == "require");
            assert!(!required, "{why}");
            eprintln!("skipped: {why}");
            return;
        }
        let dir = scratch("run");
        let data = dir.join("data.txt");
        std::fs::write(&data, "input\n").unwrap();
        let mut spec = spec(&dir, "/bin/sh", &["-c"], true);
        spec.read.push(dir.clone());
        let script = format!(
            "while IFS= read -r line; do printf '%s\\n' \"$line\"; done < '{}' > \"$OUT/copy\"; exit 3",
            data.display()
        );
        spec.args.push(script.into());
        assert_eq!(crate::sandbox::alone::launch(&spec), 3);
        assert_eq!(std::fs::read_to_string(dir.join("out/copy")).unwrap(), "input\n");
        let report = trace::decode(&std::fs::read(&spec.report).unwrap()).unwrap();
        let Report::Ran(Some(traced)) = report else { panic!("{report:?}") };
        let data = std::fs::canonicalize(&data).unwrap();
        let read = traced.files.iter().any(|(path, hash)| {
            hash.is_some() && std::fs::canonicalize(path).is_ok_and(|it| it == data)
        });
        // macOS's `/bin/sh` is a program it protects, which loads no library: what it read
        // is not known, and the report says so.
        let incomplete = traced.untraceable.contains(&blaze_core::Untraceable::Descriptor);
        assert!(read || incomplete, "{traced:?}");
        let text = std::fs::read_to_string(&spec.live).unwrap();
        assert!(text.starts_with("E\t\t*\n"), "{text}");
        _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn a_sandbox_that_cannot_be_made_is_a_report_that_says_why() {
        let dir = scratch("not-made");
        let mut spec = spec(&dir, "/bin/true", &[], true);
        spec.env.push((
            "TMPDIR".into(),
            <std::ffi::OsStr as std::os::unix::ffi::OsStrExt>::from_bytes(b"/tmp/\xff").into(),
        ));
        assert_eq!(super::launch(&spec), blaze_core::exit::Exit::CouldNot.code());
        let report = trace::decode(&std::fs::read(&spec.report).unwrap()).unwrap();
        let Report::NotMade(why) = report else { panic!("{report:?}") };
        assert!(why.contains("TMPDIR"), "{why}");
        _ = std::fs::remove_dir_all(&dir);
    }
}
