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
//! What a traced run reads of its environment is the whole of it (`E\t\t*`, every scope's):
//! a process may walk `environ` (Rust's `env::vars`, C's `envp` and `environ`, a child given
//! `environ` whole), which no call shows, so reads by name (the `E` lines the interposers add)
//! can never be all of it. The variables a run is keyed by are then never fewer than it could
//! have seen; what keys it by fewer has to make the walk seen first (a `std` that says so).

#![deny(unsafe_code)]

use std::{
    ffi::OsString,
    io::Write as _,
    path::{Path, PathBuf},
};

use blaze_usercode::{
    confine,
    inputs::Disk,
    trace::{self, Report, Seen},
};

use super::{COULD_NOT, Live};

#[cfg(unix)]
#[allow(unsafe_code, reason = "fstat and fcntl of a descriptor given by number")]
mod raw;

/// Why a traced run on Windows is not whole: what it reads is not seen there.
pub const NOT_TRACED_ON_WINDOWS: &str = "what a program reads is not traced on Windows";

/// `blaze sandbox --v2 SPEC_FILE`, the rest of the arguments after `--v2`.
pub fn command(mut args: impl Iterator<Item = OsString>) -> ! {
    let (Some(file), None) = (args.next(), args.next()) else {
        eprintln!("blaze sandbox: needs one spec: blaze sandbox --v2 SPEC_FILE");
        std::process::exit(COULD_NOT);
    };
    let spec = match read_spec(Path::new(&file)) {
        Ok(spec) => spec,
        Err(why) => {
            eprintln!("blaze sandbox: {why}");
            std::process::exit(COULD_NOT);
        }
    };
    std::process::exit(launch(&spec))
}

/// The spec in `file`, which has no field [`confine::Spec`] does not know. The file is removed
/// as it is read, before anything runs: it holds the program's whole environment, secrets
/// too, which outlive the run nowhere. One that can't be removed is an error.
pub fn read_spec(file: &Path) -> Result<confine::Spec, String> {
    // alloc: the error, once, and nothing runs.
    let text = std::fs::read(file).map_err(|error| format!("the spec {}: {error}", file.display()))?;
    // alloc: the error, once, and nothing runs.
    std::fs::remove_file(file).map_err(|error| format!("removing the spec {} once read: {error}", file.display()))?;
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
        eprintln!("blaze sandbox: --v2-serve takes nothing after it: its runs come on its standard input");
        std::process::exit(COULD_NOT);
    }
    #[cfg(windows)]
    super::windows::serve::serve();
    #[cfg(not(windows))]
    {
        eprintln!(
            "blaze sandbox: there is no warm launcher on {}: its driver starts `blaze sandbox --v2 SPEC_FILE` for each run",
            std::env::consts::OS
        );
        std::process::exit(COULD_NOT)
    }
}

/// Runs `spec` and writes its report: what the launcher ends with, the program's status (128
/// and the signal that ended it), or [`COULD_NOT`] when the sandbox could not be made.
pub fn launch(spec: &confine::Spec) -> i32 {
    launch_with(spec, run)
}

/// [`launch`], with `run` running the program in its sandbox: the warm launcher's runs are given
/// the driver's handles (`windows::Given`).
pub(super) fn launch_with(spec: &confine::Spec, run: impl FnOnce(&confine::Spec, &Live) -> Result<i32, String>) -> i32 {
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
    COULD_NOT
}

/// Opens the live record, where this run's lines start at its present end, and appends what is
/// known before anything runs: when the run traces, that it reads the whole environment, and
/// what it was given that it may read without a call the sandbox sees.
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
        first.push(trace::Line::new(Seen::Env, "", trace::ALL_ENV));
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
    let also = super::Also { network: spec.network, live: Some(live), env: Some(&spec.env), services: &spec.services };
    super::backend(&made, Some(&program), also)
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
/// they are taken as text.
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
        return Err(format!("a line could not be added to the live record {}: {why}", live.path.display()));
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
        }
    }

    #[test]
    fn a_spec_with_a_field_it_does_not_know_is_refused() {
        let dir = scratch("spec");
        let file = dir.join("spec.json");
        let mut value = serde_json::to_value(spec(&dir, "/bin/true", &[], true)).unwrap();
        std::fs::write(&file, serde_json::to_vec(&value).unwrap()).unwrap();
        assert!(super::read_spec(&file).is_ok());
        assert!(!file.exists(), "a spec, which holds the program's environment, is removed once read");
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
        let left: Vec<_> = std::fs::read_dir(&dir).unwrap().flatten().map(|it| it.file_name()).collect();
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
        assert_eq!(names, [std::ffi::OsString::from(trace::ALL_ENV_NAME), "OUT".into(), "PATH".into()]);
        assert!(!traced.files.iter().any(|it| it.0.as_path() == Path::new("/x")), "not this run's line");
        let untraced = self::spec(&dir, "/bin/true", &[], false);
        std::fs::write(&untraced.live, "").unwrap();
        let live = super::begin(&untraced).unwrap();
        assert_eq!(std::fs::read_to_string(&untraced.live).unwrap(), "");
        assert_eq!(super::finish(&untraced, &live), Ok(Report::Ran(None)));
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
        spec.env.push(("TMPDIR".into(), <std::ffi::OsStr as std::os::unix::ffi::OsStrExt>::from_bytes(b"/tmp/\xff").into()));
        assert_eq!(super::launch(&spec), super::COULD_NOT);
        let report = trace::decode(&std::fs::read(&spec.report).unwrap()).unwrap();
        let Report::NotMade(why) = report else { panic!("{report:?}") };
        assert!(why.contains("TMPDIR"), "{why}");
        _ = std::fs::remove_dir_all(&dir);
    }
}
