//! What the daemon is asked to build a program from: the crates of the workspace in it, the
//! objects it is linked from and the command that links it, and the files beside it that
//! the daemon and `blaze test` read and write of a program of tests.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// What Apple's linker is told so that the loader finds what it fixes up in tables, as the
/// splice reads and writes them, and not in chains through the pointers themselves. The
/// linker makes chains for a deployment target of macOS 12 or later, as nix's shell sets, and
/// `cc` gives a line that names none the version of the Mac it runs on. The links of
/// [`crate::link`] ask for it too.
pub const NO_FIXUP_CHAINS: &str = "-Wl,-no_fixup_chains";

/// The room Apple's linker is told to leave after the load commands when nothing says more,
/// so that a splice has room there for its two segments and for the unwind table, which
/// makes way for the patch's frames in `__TEXT`: that of a program of some hundred thousand
/// functions. A program that needs more is linked again with what it needs, which is kept in
/// [`headerpad_file`].
const HEADERPAD: u64 = 0x100000;

/// How the room is asked for.
const HEADERPAD_FLAG: &str = "-Wl,-headerpad,";

/// A program as the daemon builds it.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Plan {
    /// The root files of the crates of the workspace that blaze compiles, the program's own
    /// first.
    pub crates: Vec<PathBuf>,
    /// The object with all of blaze's code for the program.
    pub object: PathBuf,
    /// The object with what an edit changed.
    pub patch: PathBuf,
    /// The executable the daemon links and splices.
    pub binary: PathBuf,
    /// The command that links `binary`, for a shell.
    pub link_cmd: String,
    pub symbol_files: Vec<PathBuf>,
    /// Whether the program's own crate is compiled with `cfg(test)` and is not a test target:
    /// the unit tests of a library or a binary.
    pub unit_test: bool,
    /// The source of the libraries of the workspace a unit test is linked with as rustc built
    /// them, without `cfg(test)`: a change to their tests alone leaves rustc's copy as it is.
    pub linked_as_built: Vec<PathBuf>,
    /// The copies of rustc's output the plan names, which have to stay.
    pub copies: Vec<PathBuf>,
    /// The package rustc wrote of what a debugger reads of the program it linked, when the
    /// profile keeps that in one (`split-debuginfo = "packed"`).
    #[serde(default)]
    pub package: Option<PathBuf>,
    /// The newest record the plan was made from.
    pub stamp: u128,
    /// Whether the program is built whole by the daemon, every crate of it compiled and
    /// `binary` linked by blaze, with nothing of rustc's (docs/no-rustc.md, step K).
    #[serde(default)]
    pub whole: bool,
    /// Whether `binary` is a shared library, a `cdylib`, which exports the functions and
    /// statics its crate names with `#[no_mangle]` or `#[export_name]` (#262). It is built
    /// again on what the worker has loaded and linked again each time, not patched: a patch
    /// spliced into it would not export a function an edit adds or the new value of an
    /// exported static, and the splice reads no Mach-O library.
    #[serde(default)]
    pub shared: bool,
    /// Whether `binary` is the archive of a `staticlib`, with `shared`: what a `cdylib` of the
    /// library exports, and what it reaches, built as for one, archived with the standard
    /// library's object and the static native libraries the crates ask for, as rustc's
    /// `staticlib` bundles them.
    #[serde(default)]
    pub staticlib: bool,
}

/// What a request to a worker says of a program that is the unit tests of its target
/// ([`Plan::unit_test`]): its crate is the one of the root file with `cfg(test)`, which the
/// target's own is not.
pub const UNIT_TEST: &str = "unit-test";

/// The file in a program's directory that says it is a program of tests, which the daemon
/// starts in blaze's test harness. A program's own file is never named so, as rustc takes no
/// `.` in a crate's name: a whole build links a program of tests in the same directory, and
/// one of an integration test `tests/tests.rs` is named `tests`.
pub const TESTS: &str = "tests.harness";

/// The file in a program of tests' directory that says `blaze test` runs only the
/// tests an edit reaches, for the daemon to write [`REACH`] at each build of it.
pub const REACH_WANTED: &str = "tests-reach-wanted";

/// What the daemon wrote of each test of the program at its last build: [`REACH_FORMAT`], then
/// one test a line, a hash of what decides how the test ends, a tab, libtest's name of it, a
/// tab and what it reaches that its code does not decide, two JSON lists of [`Reason`]s with
/// a tab between: those that [block](Reason::blocks), then the others. They are copied into
/// `tests.ndjson` as they are, and read only for its summary. The
/// hash of a test that is not hermetic has a mark before it: `~` when all that makes it so is
/// the world outside the program, which a sandbox records, and `!` otherwise. Or `-`, the
/// name, a tab and why the test runs every time, with no hash (a benchmark); or `*`, a tab and
/// why every test is. Written whole, or not there.
pub const REACH: &str = "tests-reach";

/// The first line of [`REACH`], which changes with what it holds.
pub const REACH_FORMAT: &str = "tests-reach 3";

/// Beside [`REACH`]: the build of blaze that wrote it, as the store names builds. A blaze of
/// another build tells tests by other rules, so it walks the tests again.
pub const REACH_BY: &str = "tests-reach-by";

/// Whether a sandbox that records what a test reads makes what a reason of `kind` reaches part
/// of the test's key: files, the environment, other processes and the network.
pub fn recorded_by_a_sandbox(kind: &str) -> bool {
    matches!(kind, "fs" | "env" | "net" | "process")
}

/// What a test reaches that its code does not decide, as [`REACH`] and `tests.ndjson` have it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reason {
    /// `fs`, `env`, `net`, `process`, `clock`, `random`, `thread`, `ffi`, `asm`, `indirect`,
    /// `uncompiled` or `global-state`.
    pub kind: String,
    /// What, by name: a symbol, a static, a function.
    pub what: String,
    /// What it reaches and what that does, said.
    pub why: String,
    /// The fewest calls from the test to it.
    pub chain: Vec<Call>,
}

impl Reason {
    /// Whether it makes a test that reaches it not hermetic, to run every time: all but the
    /// clock, randomness and threads, which make a test flaky at worst.
    pub fn blocks(&self) -> bool {
        !matches!(self.kind.as_str(), "clock" | "random" | "thread")
    }
}

/// A function on the way from a test to what it reaches, and where it calls the next one.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Call {
    #[serde(rename = "fn")]
    pub name: String,
    pub file: Option<String>,
    pub line: Option<u32>,
    pub col: Option<u32>,
}

impl Call {
    /// The call `name` makes at `at`, `file:line:column`.
    pub fn new(name: String, at: Option<&str>) -> Call {
        let parsed = at.and_then(|at| {
            let (rest, col) = at.rsplit_once(':')?;
            let (file, line) = rest.rsplit_once(':')?;
            Some((file.to_owned(), line.parse().ok()?, col.parse().ok()?))
        });
        match parsed {
            Some((file, line, col)) => {
                Call { name, file: Some(file), line: Some(line), col: Some(col) }
            }
            None => Call { name, file: at.map(str::to_owned), line: None, col: None },
        }
    }
}

/// The file the link of the Apple executable at `binary` reads the room after its load
/// commands from: a response file of the one argument that asks for it. The daemon makes it
/// more when a link leaves a splice too little, and the links after start from it.
pub fn headerpad_file(binary: &Path) -> PathBuf {
    let mut file = binary.as_os_str().to_owned();
    file.push(".headerpad");
    PathBuf::from(file)
}

/// The room the link of the executable at `binary` asks for, when it reads it from a file.
pub fn headerpad_of(binary: &Path) -> Option<u64> {
    headerpad_in(std::fs::read_to_string(headerpad_file(binary)).ok()?.trim())
}

/// The last argument of a link of the Apple executable at `binary`, which has it read the room
/// from [`headerpad_file`]: what the line `asked` for, or [`HEADERPAD`], and no less than the
/// file says already.
pub fn headerpad_read(binary: &Path, asked: Option<u64>) -> std::io::Result<String> {
    let kept = headerpad_of(binary);
    let pad = asked.unwrap_or(HEADERPAD).max(kept.unwrap_or(0));
    if kept != Some(pad) {
        write_headerpad(binary, pad)?;
    }
    Ok(format!("@{}", headerpad_file(binary).display()))
}

/// Writes again each [`headerpad_file`] the link command `cmd` reads that is gone, as
/// [`headerpad_read`] wrote it: the room the words before it ask for, its own or a response
/// file's. A worker makes its command once, and a clean of the target directory while it
/// runs takes the file from under it (#721).
pub fn headerpad_again(cmd: &str) -> std::io::Result<()> {
    if !cmd.contains(".headerpad") {
        return Ok(());
    }
    let Some(args) = shlex::split(cmd) else { return Ok(()) };
    let mut asked = None;
    for arg in &args {
        if let Some(pad) = headerpad_in(arg) {
            asked = Some(pad);
            continue;
        }
        let Some(file) = arg.strip_prefix('@') else { continue };
        match file.strip_suffix(".headerpad").map(Path::new) {
            Some(binary) if !headerpad_file(binary).exists() => {
                if let Some(dir) = binary.parent() {
                    std::fs::create_dir_all(dir)?;
                }
                headerpad_read(binary, asked)?;
            }
            Some(_) => {}
            None => {
                let words = std::fs::read_to_string(file).ok().and_then(|it| shlex::split(&it));
                let mut pads = words.into_iter().flatten().filter_map(|it| headerpad_in(&it));
                asked = pads.next_back().or(asked);
            }
        }
    }
    Ok(())
}

pub fn write_headerpad(binary: &Path, pad: u64) -> std::io::Result<()> {
    std::fs::write(headerpad_file(binary), format!("{HEADERPAD_FLAG}{pad:#x}\n"))
}

/// The room an argument of the driver asks Apple's linker for, which reads it in hexadecimal.
pub fn headerpad_in(arg: &str) -> Option<u64> {
    let digits = arg.strip_prefix(HEADERPAD_FLAG)?;
    let digits = digits.strip_prefix("0x").or_else(|| digits.strip_prefix("0X")).unwrap_or(digits);
    u64::from_str_radix(digits, 16).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The room after the load commands is what the line asks for, or a default, and never
    /// less than a link of the executable was found to need.
    #[test]
    fn headerpad_of_an_apple_link() {
        let dir = std::env::temp_dir().join(format!("blaze-plan-pad-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let binary = dir.join("bin");
        let pad = |asked: Option<u64>| {
            headerpad_read(&binary, asked).unwrap();
            headerpad_of(&binary)
        };
        assert_eq!(pad(None), Some(HEADERPAD));
        std::fs::remove_file(headerpad_file(&binary)).unwrap();
        assert_eq!(pad(Some(0x800000)), Some(0x800000));
        // What was found to be needed stays, over a line that asks for less.
        write_headerpad(&binary, 0x8000).unwrap();
        assert_eq!(pad(Some(0)), Some(0x8000));
        assert_eq!(pad(None), Some(HEADERPAD));
        // Hexadecimal, as ld64 reads it, with or without `0x`.
        assert_eq!(headerpad_in("-Wl,-headerpad,200"), Some(0x200));
        assert_eq!(headerpad_in("-Wl,-headerpad,0X10"), Some(0x10));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A clean of the target directory takes the file of the room that a link command made
    /// before it reads, and the link writes it again: what the line asks for, or the default.
    #[test]
    fn headerpad_written_again_after_a_clean() {
        let dir = std::env::temp_dir().join(format!("blaze-plan-again-{}", std::process::id()));
        let binary = dir.join("whole").join("bin");
        std::fs::create_dir_all(binary.parent().unwrap()).unwrap();
        // Quoted as a command quotes them: a Windows path's backslashes are not escapes.
        let quoted = |it: String| shlex::try_quote(&it).unwrap().into_owned();
        let (out, file) =
            (binary.display().to_string(), headerpad_file(&binary).display().to_string());
        let own = format!(
            "cc /t/a.o -Wl,-headerpad,0x200000 -o {} {}",
            quoted(out.clone()),
            quoted(format!("@{file}"))
        );
        headerpad_again(&own).unwrap();
        assert_eq!(headerpad_of(&binary), Some(0x200000));
        // Kept as it is when it is there.
        write_headerpad(&binary, 0x900000).unwrap();
        headerpad_again(&own).unwrap();
        assert_eq!(headerpad_of(&binary), Some(0x900000));
        std::fs::remove_dir_all(binary.parent().unwrap()).unwrap();
        headerpad_again(&format!(
            "cc -o {} {}",
            quoted(out.clone()),
            quoted(format!("@{out}.headerpad"))
        ))
        .unwrap();
        assert_eq!(headerpad_of(&binary), Some(HEADERPAD));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
