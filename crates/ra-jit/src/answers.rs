//! The questions cargo, rust-analyzer and ra-jit ask rustc, answered by ra-jit: `-vV`,
//! `--print cfg`, `sysroot`, `target-libdir`, `target-spec-json`, the names of the files of
//! each kind of crate, and the rest of what a release of rustc says the same way every time
//! it is asked. ra-jit answers from what it knows of the release and its targets ([`own`]),
//! running no rustc. A question it does not know the answer of is asked of rustc once per
//! toolchain and the answer is kept, byte for byte; every later asker, the daemon's workers
//! and spares among them, is given that.
//!
//! cargo asks through its wrapper, which is ra-jit's in the builds of `cargo ra-jit` and in
//! the cargos ra-jit runs only to ask; rust-analyzer asks through [`ask`], which ra-jit hands
//! it; ra-jit itself through [`answer`].

use std::{
    ffi::{OsStr, OsString},
    io::{self, Read as _, Write as _},
    path::{Path, PathBuf},
    process::{Command, ExitStatus, Output, Stdio},
};

use sha2::{Digest as _, Sha256};

mod own;

/// What rustc can be asked to print without compiling, whatever the input is.
const ABOUT_RUSTC: [&str; 20] = [
    "all-target-specs-json",
    "backend-has-zstd",
    "calling-conventions",
    "cfg",
    "check-cfg",
    "code-models",
    "deployment-target",
    "host-tuple",
    "relocation-models",
    "split-debuginfo",
    "stack-protector-strategies",
    "supported-crate-types",
    "sysroot",
    "target-cpus",
    "target-features",
    "target-libdir",
    "target-list",
    "target-spec-json",
    "target-spec-json-schema",
    "tls-models",
];

/// The flags of rustc that take a value, beyond those a crate's build is given.
const ALSO_VALUED: [&str; 9] = [
    "-o",
    "--sysroot",
    "--allow",
    "--warn",
    "--deny",
    "--forbid",
    "--force-warn",
    "--codegen",
    "--env-set",
];

/// What rustc prints of its input without compiling it: asked of standard input only, which
/// is part of the question.
const ABOUT_INPUT: [&str; 2] = ["file-names", "crate-name"];

/// The variables that change what rustc answers.
const READ_BY_RUSTC: [&str; 9] = [
    "RUSTC_BOOTSTRAP",
    "RUSTC_OVERRIDE_VERSION_STRING",
    "RUST_TARGET_PATH",
    "SDKROOT",
    "MACOSX_DEPLOYMENT_TARGET",
    "IPHONEOS_DEPLOYMENT_TARGET",
    "TVOS_DEPLOYMENT_TARGET",
    "WATCHOS_DEPLOYMENT_TARGET",
    "XROS_DEPLOYMENT_TARGET",
];

/// The variable that names the wrapper cargo runs every rustc through.
const WRAPPER: &str = "RUSTC_WRAPPER";

/// The variable that tells ra-jit as cargo's wrapper only to answer questions, and to leave
/// what cargo compiles as it would be without ra-jit.
pub(crate) const ONLY_ASKS: &str = "RA_JIT_ONLY_ASKS";

/// The variable that says which answers are given: `0` none, so rustc is asked every time;
/// `own` only ra-jit's own, so a question it has no answer of fails.
const OFF: &str = "RA_JIT_ANSWERS";

/// Whether rustc run with `args` compiles nothing and says only what its release, the
/// arguments and standard input decide.
pub(crate) fn is_question(args: &[OsString]) -> bool {
    let Some(args) = args.iter().map(|it| it.to_str()).collect::<Option<Vec<_>>>() else {
        return false;
    };
    if args.iter().any(|it| it.starts_with('@')) {
        return false;
    }
    if args.iter().any(|it| matches!(*it, "-V" | "-vV" | "--version")) {
        return true;
    }
    let mut kinds = Vec::new();
    let mut from_stdin = false;
    let mut inputs = 0;
    let mut words = args.iter().copied();
    while let Some(word) = words.next() {
        let (flag, joined) = match word.split_once('=') {
            Some((flag, value)) if flag.starts_with("--") => (flag, Some(value)),
            _ => (word, None),
        };
        let mut value = || joined.or_else(|| words.next());
        match flag {
            "--print" => match value() {
                // A print to a file writes it.
                Some(kind) if !kind.contains('=') => kinds.push(kind),
                _ => return false,
            },
            "--target" => match value() {
                // A target of the user's own is a file that can change.
                Some(target) if !target.ends_with(".json") => {}
                _ => return false,
            },
            "-" => from_stdin = true,
            _ if !flag.starts_with('-') => inputs += 1,
            _ if joined.is_none()
                && (crate::cargo::wrapper::VALUED.contains(&flag) || ALSO_VALUED.contains(&flag)) =>
            {
                words.next();
            }
            _ => {}
        }
    }
    let about_rustc = |kind: &&str| ABOUT_RUSTC.contains(kind);
    let about_input = |kind: &&str| ABOUT_INPUT.contains(kind);
    !kinds.is_empty()
        && kinds.iter().all(|it| about_rustc(it) || about_input(it))
        && (kinds.iter().all(about_rustc) || (from_stdin && inputs == 0))
}

/// What ra-jit itself answers to what `command` asks rustc, given `stdin`, or `None` when it
/// has no answer of its own. No rustc runs.
pub(crate) fn own_answer(command: &Command, stdin: &[u8]) -> Option<Vec<u8>> {
    let var = |name: &str| var_of(command, name);
    let args: Vec<OsString> = command.get_args().map(OsStr::to_owned).collect();
    let rustc = toolchain_rustc(command.get_program(), &var)?;
    own::answer(&rustc, &args, stdin, &var)
}

/// rustc's answer to what `command` asks it, given `stdin`: ra-jit's own, the kept answer, or
/// rustc's now, kept when it is one. `command` runs rustc and asks a question ([`is_question`]).
pub(crate) fn answer(command: &mut Command, stdin: &[u8]) -> io::Result<Output> {
    let given = std::env::var_os(OFF);
    if given.as_ref().is_none_or(|it| it != "0")
        && let Some(stdout) = own_answer(command, stdin)
    {
        return Ok(Output { status: ExitStatus::default(), stdout, stderr: Vec::new() });
    }
    if given.is_some_and(|it| it == "own") {
        return Err(io::Error::other("ra-jit has no answer of its own to this question"));
    }
    let key = key(command, stdin);
    let file = key.as_ref().and_then(|key| Some(dir()?.join(key)));
    if let Some(stdout) = file.as_deref().and_then(kept) {
        return Ok(Output { status: ExitStatus::default(), stdout, stderr: Vec::new() });
    }
    // The workers and spares of one load ask the same questions at once: one asks rustc, and
    // the others wait for its answer.
    let _held = file.as_deref().and_then(hold);
    if let Some(stdout) = file.as_deref().and_then(kept) {
        return Ok(Output { status: ExitStatus::default(), stdout, stderr: Vec::new() });
    }
    let mut child =
        command.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn()?;
    if let Some(mut input) = child.stdin.take() {
        // rustc may answer without reading what it was given.
        _ = input.write_all(stdin);
    }
    let output = child.wait_with_output()?;
    // A question rustc did not answer is asked again, and what it warns of is said each time.
    if let Some(file) = file
        && output.status.success()
        && output.stderr.is_empty()
    {
        keep(&file, &output.stdout);
    }
    Ok(output)
}

/// What rust-analyzer runs to learn of the toolchain, answered here when it asks rustc.
/// A cargo it runs only to ask is given ra-jit as the workspace's wrapper, which answers
/// what cargo asks rustc in turn.
pub(crate) fn ask(command: &mut Command) -> Option<io::Result<Output>> {
    let program = Path::new(command.get_program()).file_stem()?.to_str()?.to_owned();
    match program.as_str() {
        "cargo" => {
            ready_cargo(command);
            None
        }
        "rustc" => {
            let args: Vec<OsString> = command.get_args().map(OsStr::to_owned).collect();
            is_question(&args).then(|| answer(command, &[]))
        }
        _ => None,
    }
}

/// Has a cargo ask rustc through ra-jit, unless the user named a wrapper of their own: one
/// that only asks (`cargo metadata`, `cargo rustc --print`, `--unit-graph`), and one whose
/// build is not ra-jit's (rust-analyzer's `cargo check` of build scripts, `cargo ra-jit
/// check`), which compiles as it would without ra-jit. It is `RUSTC_WRAPPER`, not the
/// workspace's: cargo runs the print of `cargo rustc --print` through that one alone.
pub(crate) fn ready_cargo(command: &mut Command) {
    let named = [WRAPPER, "CARGO_BUILD_RUSTC_WRAPPER"].iter().any(|name| {
        command.get_envs().any(|(it, value)| it == *name && value.is_some())
            || std::env::var_os(name).is_some_and(|it| !it.is_empty())
    });
    let wrapper = crate::cargo::switch::wrapper_path().ok().filter(|it| it.is_file());
    if let Some(wrapper) = wrapper
        && !named
    {
        command.env(WRAPPER, wrapper).env(ONLY_ASKS, "1");
    }
}

/// Answers the question cargo asked the wrapper, and ends as rustc would have. `None` when
/// it is no question.
pub(crate) fn reply(rustc: &OsStr, args: &[OsString]) -> Option<io::Result<i32>> {
    if !is_question(args) {
        return None;
    }
    let mut stdin = Vec::new();
    if args.iter().any(|it| it == "-")
        && let Err(e) = io::stdin().read_to_end(&mut stdin)
    {
        return Some(Err(e));
    }
    let output = answer(Command::new(rustc).args(args), &stdin);
    Some(output.and_then(|output| {
        io::stdout().write_all(&output.stdout)?;
        io::stderr().write_all(&output.stderr)?;
        Ok(output.status.code().unwrap_or(1))
    }))
}

/// Where the answers are kept: in `RA_JIT_HOME` when it is set, else in the user's cache
/// directory, which outlives the session.
fn dir() -> Option<PathBuf> {
    if std::env::var_os(OFF).is_some_and(|it| it == "0") {
        return None;
    }
    if let Some(home) = std::env::var_os("RA_JIT_HOME").filter(|it| !it.is_empty()) {
        return Some(Path::new(&home).join("answers"));
    }
    let caches = std::env::var_os("XDG_CACHE_HOME").filter(|it| !it.is_empty()).map(PathBuf::from);
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"));
    let caches = caches.or_else(|| Some(PathBuf::from(home?).join(".cache")))?;
    Some(caches.join("ra-jit/answers"))
}

/// The name of the question: the rustc that is asked, its arguments, what it reads of its
/// environment and of standard input. `None` when which rustc runs is not known.
fn key(command: &Command, stdin: &[u8]) -> Option<String> {
    let var = |name: &str| var_of(command, name);
    let rustc = toolchain_rustc(command.get_program(), &var)?;
    let is = std::fs::metadata(&rustc).ok()?;
    let mut hasher = Sha256::new();
    hasher.update(b"ra-jit answer 1\0");
    hasher.update(rustc.as_os_str().as_encoded_bytes());
    hasher.update([0]);
    hasher.update(is.len().to_le_bytes());
    let modified = is.modified().ok()?.duration_since(std::time::UNIX_EPOCH).ok()?;
    hasher.update(modified.as_nanos().to_le_bytes());
    for arg in command.get_args() {
        hasher.update(arg.as_encoded_bytes());
        hasher.update([0]);
    }
    for name in READ_BY_RUSTC {
        hasher.update(name);
        match var(name) {
            Some(value) => {
                hasher.update([1]);
                hasher.update(value.as_encoded_bytes());
            }
            None => hasher.update([0]),
        }
    }
    hasher.update(stdin);
    Some(hasher.finalize().iter().map(|it| format!("{it:02x}")).collect())
}

/// The variable `name` as `command` will have it.
fn var_of(command: &Command, name: &str) -> Option<OsString> {
    match command.get_envs().find(|(it, _)| *it == name) {
        Some((_, value)) => value.map(OsStr::to_owned),
        None => std::env::var_os(name),
    }
}

/// The rustc of the toolchain that `program`, as this process would run it, is.
pub(crate) fn toolchain_rustc_of(program: &OsStr) -> Option<PathBuf> {
    toolchain_rustc(program, &|name| std::env::var_os(name))
}

/// The executable of the toolchain that `program` runs. rustup's proxy runs the toolchain
/// rustup set for the process it started, and is not known without that.
fn toolchain_rustc(program: &OsStr, var: &dyn Fn(&str) -> Option<OsString>) -> Option<PathBuf> {
    let found = if Path::new(program).components().count() > 1 {
        PathBuf::from(program)
    } else {
        let paths = var("PATH")?;
        let name = Path::new(program).with_extension(std::env::consts::EXE_EXTENSION);
        std::env::split_paths(&paths).map(|dir| dir.join(&name)).find(|it| it.is_file())?
    };
    let real = std::fs::canonicalize(found).ok()?;
    let proxy = real.file_stem().and_then(OsStr::to_str).is_some_and(|it| it.starts_with("rustup"));
    if !proxy {
        return Some(real);
    }
    let toolchain = PathBuf::from(var("RUSTUP_TOOLCHAIN")?);
    let dir = if toolchain.is_absolute() {
        toolchain
    } else {
        PathBuf::from(var("RUSTUP_HOME")?).join("toolchains").join(toolchain)
    };
    let rustc = dir.join("bin").join(format!("rustc{}", std::env::consts::EXE_SUFFIX));
    std::fs::canonicalize(rustc).ok()
}

fn kept(file: &Path) -> Option<Vec<u8>> {
    std::fs::read(file).ok()
}

/// The lock of the question whose answer is `file`, held until it is dropped.
fn hold(file: &Path) -> Option<std::fs::File> {
    std::fs::create_dir_all(file.parent()?).ok()?;
    let lock = std::fs::File::create(file.with_extension("lock")).ok()?;
    lock.lock().ok()?;
    Some(lock)
}

fn keep(file: &Path, stdout: &[u8]) {
    let Some(dir) = file.parent() else { return };
    if std::fs::create_dir_all(dir).is_ok() {
        // An answer not kept is asked again.
        _ = ra_jit_store::write_file_whole(file, stdout);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn question(args: &str) -> bool {
        let args: Vec<OsString> = args.split_whitespace().map(OsString::from).collect();
        is_question(&args)
    }

    #[test]
    fn what_is_a_question() {
        assert!(question("-vV"));
        assert!(question("--version"));
        assert!(question("--print sysroot"));
        assert!(question("--print=target-libdir"));
        assert!(question("--print cfg --target aarch64-apple-darwin -C opt-level=3"));
        assert!(question(
            "-Z unstable-options --print target-spec-json --target x86_64-unknown-linux-gnu"
        ));
        assert!(question(
            "- --crate-name ___ --print=file-names --crate-type bin --crate-type rlib \
             --print=sysroot --print=split-debuginfo --print=crate-name --print=cfg -Wwarnings"
        ));
        // rust-analyzer's `cargo rustc -- --print cfg` names the library, which rustc does
        // not read for that.
        assert!(question(
            "--crate-name p --edition=2024 src/lib.rs --crate-type lib --emit=dep-info,metadata,link \
             -C metadata=d4ed28d9543870f2 --out-dir target/debug/deps --print cfg"
        ));
    }

    #[test]
    fn what_is_not() {
        assert!(!question("--crate-name p src/lib.rs --crate-type lib --emit=link"));
        assert!(!question("src/lib.rs --print file-names"));
        assert!(!question("--print link-args src/main.rs"));
        assert!(!question("--print=link-args=out.txt src/main.rs"));
        assert!(!question("--print cfg --target my-target.json"));
        assert!(!question("@args.txt"));
        assert!(!question(""));
    }
}
