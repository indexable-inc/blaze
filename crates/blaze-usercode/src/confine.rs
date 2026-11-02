//! The sandbox of a spawn ([`blaze_core::Confine`]): what a driver starts in place of a
//! command whose [`Sandbox`] is on. That is blaze's launcher (`blaze sandbox --v2 SPEC`, in
//! `blaze-cargo/src/sandbox`), given a [`Spec`] in the run's scratch directory; it makes the
//! platform's sandbox (namespaces and seccomp on Linux, Seatbelt on macOS, an AppContainer in
//! a job object on Windows), runs the program in it, appends what it sees to the live record
//! as it sees it ([`crate::trace::Line`]), and as it ends leaves its [`crate::trace::Report`]:
//! that it could not make the sandbox, or that the program ran and, when it traces, what it
//! read, hashed as the files are then ([`crate::inputs::Disk`]).
//!
//! Where the platform has a warm launcher ([`Confine::warm`]: `blaze sandbox --v2-serve`, on
//! Windows), the driver starts it once and hands it each run instead: the frame
//! [`Confine::hand`] makes, the spec and the handles the driver passed it, written to its standard
//! input, and read back by it with [`read_hand`]. Its [`blaze_core::Answer`]s, one a run, come on
//! its standard output. The run's scratch directory, report and live record are the same either
//! way.

use std::{
    borrow::Cow,
    ffi::OsString,
    io::{ErrorKind, Read},
    path::{Path, PathBuf},
};

use blaze_core::{Command, Confine, Confined, Handed, Sandbox, Traced};
use serde::{Deserialize, Serialize, Serializer};

use crate::trace::{self, Report};

/// The variable of a [`Command`]'s environment that names, for a warm process, where its live
/// record goes: the confine takes it out of what the program sees and gives it the launcher
/// as [`Spec::live`]. A process that serves many requests is read after each answer, by
/// scope ([`crate::pool`]).
pub const LIVE: &str = "__BLAZE_SANDBOX_LIVE";

/// The subcommand and flag of blaze's launcher.
pub const LAUNCH: [&str; 2] = ["sandbox", "--v2"];

/// The subcommand and flag of blaze's warm launcher, which reads its runs on its standard input.
pub const SERVE: [&str; 2] = ["sandbox", "--v2-serve"];

/// The version of the frame [`Confine::hand`] makes, its second field.
pub const HAND_VERSION: u32 = 1;

/// A frame's fixed part: its length, its version, and [`Handed`]'s five fields.
pub const HAND_HEADER: usize = 4 + 4 + 5 * 8;

/// The longest frame either side takes: a spec is kilobytes, so one past this is a fault.
pub const HAND_MOST: usize = 16 << 20;

/// The variables the launcher reads itself, of the daemon's environment, on Windows: where the
/// AppContainer profile's folder is (`LOCALAPPDATA`), where a run's temporary directory goes
/// (`TMP`, `TEMP` and `USERPROFILE`, as `GetTempPath2W` looks them up), and `SystemRoot`, which
/// the loader of any process needs. Without them a run's temporary directory would be made in
/// the Windows directory, which a user can't write.
#[cfg(windows)]
const LAUNCHER_READS: &[&str] = &["SystemRoot", "LOCALAPPDATA", "TMP", "TEMP", "USERPROFILE"];

/// The Linux and macOS launchers read no variable: the program's environment is in the spec.
#[cfg(not(windows))]
const LAUNCHER_READS: &[&str] = &[];

/// The launcher's own environment: of the daemon's, [`LAUNCHER_READS`].
fn launcher_env() -> Vec<(OsString, OsString)> {
    // alloc: the edge: `Confined` owns the process it starts.
    let mut env = Vec::with_capacity(LAUNCHER_READS.len());
    env.extend(LAUNCHER_READS.iter().filter_map(|name| Some((OsString::from(name), std::env::var_os(name)?))));
    env
}

/// The spec's and the report's names in the run's scratch directory.
pub const SPEC_FILE: &str = "spec.json";
pub const REPORT_FILE: &str = "report";
/// The live record's name in the scratch directory, of a run that names none of its own.
pub const LIVE_FILE: &str = "live";

/// What the launcher is told to run, as JSON, refusing fields it does not know.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
#[serde(deny_unknown_fields)]
pub struct Spec {
    pub program: PathBuf,
    pub args: Vec<OsString>,
    /// All of its environment: the launcher's own is not passed on.
    pub env: Vec<(OsString, OsString)>,
    pub dir: PathBuf,
    /// Seen read-only, with what is under them; the rest of the machine is seen read-only
    /// too where the platform can't hide it (macOS, Windows), and not at all in a Linux view.
    pub read: Vec<PathBuf>,
    /// Written for real, with what is under them.
    pub write: Vec<PathBuf>,
    pub network: bool,
    /// Whether to record what it read; what was refused it is recorded either way.
    pub trace: bool,
    /// Mach global names it may look up beyond the sandbox's own few (`Sandbox::On`'s
    /// `services`); read on macOS only.
    #[serde(default)]
    pub services: Vec<String>,
    /// Where the live record goes, appended a line at a time before the call it notes goes
    /// on.
    pub live: PathBuf,
    /// Where the report goes as it ends ([`trace::encode`]), whole or not at all.
    pub report: PathBuf,
    /// Paths that are no inputs of what runs: the program itself, whose code is its key.
    pub not_inputs: Vec<PathBuf>,
}

/// A [`Spec`] lent from the command it runs: what [`Launcher::confine`] writes, the same JSON
/// as the `Spec` the launcher reads back, with nothing of the command copied.
#[derive(Serialize, Debug)]
pub struct SpecOf<'a> {
    program: &'a Path,
    args: &'a [OsString],
    env: Passed<'a>,
    dir: &'a Path,
    read: &'a [PathBuf],
    write: &'a [PathBuf],
    network: bool,
    trace: bool,
    services: &'a [String],
    live: Cow<'a, Path>,
    report: PathBuf,
    not_inputs: &'a [PathBuf],
}

/// A command's environment as the program sees it: without [`LIVE`], which is the launcher's.
#[derive(Debug)]
struct Passed<'a>(&'a [(OsString, OsString)]);

impl Serialize for Passed<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_seq(self.0.iter().filter(|(name, _)| name != LIVE))
    }
}

/// The sandbox blaze makes: its launcher at `program`, which is blaze itself.
#[derive(Debug)]
pub struct Launcher {
    pub program: PathBuf,
}

impl Launcher {
    /// The spec of `command`, run with `scratch` as its scratch directory.
    pub fn spec<'a>(command: &'a Command, scratch: &Path) -> Result<SpecOf<'a>, String> {
        let Sandbox::On { read, write, network, trace, services } = &command.sandbox else {
            return Err(format!("{}: its sandbox is off", command.program.display())); // alloc: error path
        };
        let live = command.env.iter().rev().find(|(name, _)| name == LIVE);
        Ok(SpecOf {
            program: &command.program,
            args: &command.args,
            env: Passed(&command.env),
            dir: &command.dir,
            read,
            write,
            network: *network,
            trace: *trace,
            services,
            live: live.map_or_else(|| Cow::Owned(scratch.join(LIVE_FILE)), |(_, value)| Cow::Borrowed(Path::new(value))),
            report: scratch.join(REPORT_FILE),
            not_inputs: std::slice::from_ref(&command.program),
        })
    }

    /// What the report in `scratch` says, or why there is none.
    pub fn report(scratch: &Path) -> Result<Report, String> {
        let file = scratch.join(REPORT_FILE);
        let bytes = std::fs::read(&file).map_err(|error| {
            format!("the launcher left no report at {} ({error}): it did not end as it should", file.display()) // alloc: error path
        })?;
        trace::decode(&bytes).map_err(|why| format!("{}: {why}", file.display())) // alloc: error path
    }
}

impl Confine for Launcher {
    fn confine(&self, command: &Command, scratch: &Path) -> Result<Confined, String> {
        let spec = Launcher::spec(command, scratch)?;
        let file = scratch.join(SPEC_FILE);
        blaze_mem::scratch::with_bytes(|text| {
            serde_json::to_writer(&mut *text, &spec).map_err(|error| format!("the spec: {error}"))?; // alloc: error path
            write_private(&file, text).map_err(|error| format!("{}: {error}", file.display())) // alloc: error path
        })?;
        let mut args = Vec::with_capacity(LAUNCH.len() + 1);
        args.extend(LAUNCH.map(OsString::from));
        args.push(file.into_os_string());
        Ok(Confined {
            program: self.program.clone(), // clone: the edge: `Confined` owns the process it starts
            args,
            env: launcher_env(),
            dir: scratch.to_owned(), // alloc: the edge: `Confined` owns the process it starts
        })
    }

    /// `blaze sandbox --v2-serve` on Windows, where a run's second process creation is the
    /// largest cost it has (docs/v2/plan/lanes/usercode/speed-of-light.md, "The Windows
    /// sandbox"). Linux and macOS have no warm launcher: their driver starts `--v2` a run.
    fn warm(&self) -> Option<Confined> {
        if !cfg!(windows) {
            return None;
        }
        // Where it stays: blaze's own directory, which is there as long as blaze is.
        let dir = self.program.parent().filter(|it| !it.as_os_str().is_empty()).unwrap_or(Path::new("."));
        Some(Confined {
            program: self.program.clone(), // clone: the edge, once per launcher started
            // alloc: as above.
            args: SERVE.iter().map(OsString::from).collect(),
            env: launcher_env(),
            dir: dir.to_owned(), // alloc: as above.
        })
    }

    /// The frame of `command`'s run: [`HAND_HEADER`] bytes, then the spec as JSON.
    ///
    /// | bytes | what |
    /// |---|---|
    /// | 4 | how many bytes follow, little-endian |
    /// | 4 | [`HAND_VERSION`] |
    /// | 8 each | [`Handed`]'s `run`, `stdin`, `stdout`, `stderr` and `job` |
    /// | the rest | the [`Spec`], as [`Launcher::spec`] makes it |
    fn hand(&self, command: &Command, scratch: &Path, handed: &Handed) -> Result<Vec<u8>, String> {
        let spec = Launcher::spec(command, scratch)?;
        // alloc: the edge: the bytes the driver writes to the launcher, once per run.
        let mut frame = Vec::with_capacity(HAND_HEADER + 1024 + 64 * command.env.len());
        frame.extend_from_slice(&[0; 4]);
        frame.extend_from_slice(&HAND_VERSION.to_le_bytes());
        for field in [handed.run, handed.stdin, handed.stdout, handed.stderr, handed.job] {
            frame.extend_from_slice(&field.to_le_bytes());
        }
        serde_json::to_writer(&mut frame, &spec).map_err(|error| format!("the spec: {error}"))?; // alloc: error path
        let len = frame.len() - 4;
        if len > HAND_MOST {
            // alloc: error path
            return Err(format!("{}: its spec is {len} bytes, past the launcher's {HAND_MOST}", command.program.display()));
        }
        frame[..4].copy_from_slice(&(len as u32).to_le_bytes());
        Ok(frame)
    }

    fn traced(&self, command: &Command, scratch: &Path) -> Result<Traced, String> {
        match Launcher::report(scratch)? {
            Report::NotMade(why) => {
                Err(format!("{}: no sandbox could be made: {why}", command.program.display())) // alloc: error path
            }
            Report::Ran(Some(traced)) => Ok(traced),
            Report::Ran(None) => match &command.sandbox {
                Sandbox::On { trace: false, .. } => Ok(Traced::default()),
                // alloc: error path
                _ => Err(format!(
                    "{}: the sandbox traces, and its report has no record of what was read",
                    command.program.display()
                )),
            },
        }
    }
}

/// Writes `bytes` to the new file `file`, readable by its owner alone: a spec holds the
/// program's whole environment, its secrets too, so it is made so and lives only until the
/// launcher has read it, which removes it before anything runs (`blaze sandbox --v2`).
fn write_private(file: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write as _;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    options.open(file)?.write_all(bytes)
}

/// The next run on `input`, a frame [`Confine::hand`] made, read into `buffer` (kept for the
/// next): `None` when the driver closed `input` between two frames. A frame cut short, of
/// another version, too long or whose spec does not parse is an error: the launcher's fault.
pub fn read_hand(input: &mut impl Read, buffer: &mut Vec<u8>) -> Result<Option<(Handed, Spec)>, String> {
    let mut len = [0u8; 4];
    let mut got = 0;
    while got < len.len() {
        match input.read(&mut len[got..]) {
            Ok(0) if got == 0 => return Ok(None),
            // alloc: error path
            Ok(0) => return Err("the driver's frames end inside a frame's length".to_owned()),
            Ok(read) => got += read,
            Err(error) if error.kind() == ErrorKind::Interrupted => {}
            Err(error) => return Err(format!("reading the driver's frames: {error}")), // alloc: error path
        }
    }
    let len = u32::from_le_bytes(len) as usize;
    if !(HAND_HEADER - 4..=HAND_MOST).contains(&len) {
        // alloc: error path
        return Err(format!("the driver's frame says it is {len} bytes, not between {} and {HAND_MOST}", HAND_HEADER - 4));
    }
    buffer.clear();
    buffer.resize(len, 0);
    // alloc: error path
    input.read_exact(buffer).map_err(|error| format!("reading a frame of {len} bytes from the driver: {error}"))?;
    let word = |at: usize| u64::from_le_bytes(buffer[at..at + 8].try_into().expect("8 bytes, inside the header"));
    let version = u32::from_le_bytes(buffer[..4].try_into().expect("4 bytes, inside the header"));
    if version != HAND_VERSION {
        // alloc: error path
        return Err(format!("the driver's frame is of version {version}, and this launcher reads {HAND_VERSION}"));
    }
    let handed = Handed { run: word(4), stdin: word(12), stdout: word(20), stderr: word(28), job: word(36) };
    let spec = serde_json::from_slice(&buffer[HAND_HEADER - 4..])
        // alloc: error path
        .map_err(|error| format!("the spec of run {}: {error}", handed.run))?;
    Ok(Some((handed, spec)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn command(trace: bool) -> Command {
        Command {
            program: "/build/script".into(),
            args: vec!["a".into()],
            env: vec![("CC".into(), "cc".into()), (LIVE.into(), "/root/live/7".into())],
            dir: "/ws".into(),
            stdin: Vec::new(),
            sandbox: Sandbox::On { read: vec!["/ws".into()], write: vec!["/out".into()], network: false, trace, services: Vec::new() },
            memory: None,
        }
    }

    /// The spec of `command` as the launcher reads it back.
    fn read_back(command: &Command, scratch: &Path) -> Spec {
        let text = serde_json::to_vec(&Launcher::spec(command, scratch).unwrap()).unwrap();
        serde_json::from_slice(&text).unwrap()
    }

    #[test]
    fn the_live_variable_is_the_launchers_not_the_programs() {
        let spec = read_back(&command(true), Path::new("/s"));
        assert_eq!(spec.env, vec![("CC".into(), "cc".into())]);
        assert_eq!(spec.live, PathBuf::from("/root/live/7"));
        assert_eq!(spec.report, PathBuf::from("/s/report"));
        assert_eq!(spec.not_inputs, vec![PathBuf::from("/build/script")]);
        assert_eq!(
            (spec.program, spec.args, spec.dir, spec.read, spec.write, spec.network, spec.trace),
            ("/build/script".into(), vec!["a".into()], "/ws".into(), vec!["/ws".into()], vec!["/out".into()], false, true)
        );
        let mut unnamed = command(false);
        unnamed.env.retain(|(name, _)| name != LIVE);
        let spec = read_back(&unnamed, Path::new("/s"));
        assert_eq!((spec.live, spec.trace), (PathBuf::from("/s/live"), false));
        let mut off = command(true);
        off.sandbox = Sandbox::Off;
        assert!(Launcher::spec(&off, Path::new("/s")).is_err());
    }

    #[test]
    fn the_launcher_is_given_its_spec_and_the_report_is_read_back() {
        let scratch = std::env::temp_dir().join(format!("blaze-confine-test-{}", std::process::id()));
        _ = std::fs::remove_dir_all(&scratch);
        std::fs::create_dir_all(&scratch).unwrap();
        let launcher = Launcher { program: "/bin/blaze".into() };
        let confined = launcher.confine(&command(true), &scratch).unwrap();
        assert_eq!(confined.program, PathBuf::from("/bin/blaze"));
        assert_eq!(confined.args[..2], [OsString::from("sandbox"), OsString::from("--v2")]);
        let text = std::fs::read(scratch.join(SPEC_FILE)).unwrap();
        let spec: Spec = serde_json::from_slice(&text).unwrap();
        assert_eq!(spec, read_back(&command(true), &scratch));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(scratch.join(SPEC_FILE)).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "the spec holds the environment: its owner's alone");
        }
        assert!(launcher.traced(&command(true), &scratch).is_err());
        std::fs::write(scratch.join(REPORT_FILE), trace::encode(&Report::NotMade("no".into()))).unwrap();
        assert!(launcher.traced(&command(true), &scratch).unwrap_err().contains("no sandbox"));
        std::fs::write(scratch.join(REPORT_FILE), trace::encode(&Report::Ran(None))).unwrap();
        assert!(launcher.traced(&command(true), &scratch).is_err());
        assert_eq!(launcher.traced(&command(false), &scratch), Ok(Traced::default()));
        let traced = Traced { env: vec![("CC".into(), Some("cc".into()))], ..Traced::default() };
        std::fs::write(scratch.join(REPORT_FILE), trace::encode(&Report::Ran(Some(traced.clone())))).unwrap();
        assert_eq!(launcher.traced(&command(true), &scratch), Ok(traced));
        _ = std::fs::remove_dir_all(&scratch);
    }

    #[test]
    fn a_handed_run_is_read_back_as_the_spec_and_the_handles_the_driver_passed() {
        let launcher = Launcher { program: "/bin/blaze".into() };
        let handed = Handed { run: 7, stdin: 0x10, stdout: 0x14, stderr: 0x18, job: 0x1c };
        let mut stream = launcher.hand(&command(true), Path::new("/s"), &handed).unwrap();
        assert_eq!(u32::from_le_bytes(stream[..4].try_into().unwrap()) as usize, stream.len() - 4);
        let second = Handed { run: 8, job: 0, ..handed };
        stream.extend(launcher.hand(&command(false), Path::new("/t"), &second).unwrap());
        let mut input = &stream[..];
        let mut buffer = Vec::new();
        assert_eq!(read_hand(&mut input, &mut buffer), Ok(Some((handed, read_back(&command(true), Path::new("/s"))))));
        assert_eq!(read_hand(&mut input, &mut buffer), Ok(Some((second, read_back(&command(false), Path::new("/t"))))));
        assert_eq!(read_hand(&mut input, &mut buffer), Ok(None), "closed between frames");
    }

    #[test]
    fn a_frame_cut_short_of_another_version_or_too_long_is_a_fault() {
        let launcher = Launcher { program: "/bin/blaze".into() };
        let handed = Handed { run: 1, stdin: 1, stdout: 2, stderr: 3, job: 4 };
        let frame = launcher.hand(&command(true), Path::new("/s"), &handed).unwrap();
        let mut buffer = Vec::new();
        for cut in [2, HAND_HEADER - 1, frame.len() - 1] {
            assert!(read_hand(&mut &frame[..cut], &mut buffer).is_err(), "cut at {cut}");
        }
        let mut other = frame.clone();
        other[4..8].copy_from_slice(&(HAND_VERSION + 1).to_le_bytes());
        assert!(read_hand(&mut &other[..], &mut buffer).unwrap_err().contains("version"));
        let mut long = frame.clone();
        long[..4].copy_from_slice(&(HAND_MOST as u32 + 1).to_le_bytes());
        assert!(read_hand(&mut &long[..], &mut buffer).is_err());
        let mut bad = frame;
        let at = bad.len() - 1;
        bad[at] = b'!';
        assert!(read_hand(&mut &bad[..], &mut buffer).unwrap_err().contains("run 1"));
    }

    #[test]
    fn only_windows_has_a_warm_launcher_and_it_serves() {
        let launcher = Launcher { program: "/opt/blaze/bin/blaze".into() };
        match launcher.warm() {
            Some(warm) => {
                assert!(cfg!(windows));
                assert_eq!(warm.program, PathBuf::from("/opt/blaze/bin/blaze"));
                assert_eq!(warm.args, [OsString::from("sandbox"), OsString::from("--v2-serve")]);
                assert_eq!(warm.dir, PathBuf::from("/opt/blaze/bin"));
                assert!(warm.env.iter().all(|(name, _)| LAUNCHER_READS.iter().any(|it| name == it)));
            }
            None => assert!(!cfg!(windows)),
        }
    }
}
