//! The questions cargo, rust-analyzer, build scripts and blaze ask rustc, answered by blaze:
//! `-vV`, `--print cfg`, `sysroot`, `target-libdir`, `target-spec-json`, the names of the files
//! of each kind of crate, and the rest of what a release of rustc says the same way every time
//! it is asked. blaze answers them for its own release, whichever rustc was named, from what it
//! knows of that release and its targets ([`own`]). No rustc is ever run to answer: a question
//! blaze does not know the answer of fails, saying which.
//!
//! cargo asks through its wrapper, which is blaze's in the builds of `blaze` and in the cargos
//! blaze runs only to ask; rust-analyzer asks through [`ask`], which blaze hands it; a build
//! script through blaze's rustc stand-in ([`crate::as_rustc`]); blaze itself through [`ours`].

use std::{
    ffi::{OsStr, OsString},
    io::{self, Read as _, Write as _},
    path::Path,
    process::{Command, ExitStatus, Output},
};

pub mod cargo;
pub mod own;

pub use cargo::{answer as cargo_answer, rustflags as cargo_rustflags};

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

/// The variable that names the wrapper cargo runs every rustc through.
const WRAPPER: &str = "RUSTC_WRAPPER";

/// The variable that tells blaze as cargo's wrapper only to answer questions, and to leave
/// what cargo compiles as it would be without blaze.
pub const ONLY_ASKS: &str = "BLAZE_ONLY_ASKS";

/// Whether rustc run with `args` compiles nothing and says only what its release, the
/// arguments and standard input decide.
pub fn is_question(args: &[OsString]) -> bool {
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
                // Beside another print rustc stops before it links, so prints nothing of it.
                Some(kind) if own::WHEN_LINKING.contains(&kind) => {}
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
                && (crate::stub::VALUED.contains(&flag) || ALSO_VALUED.contains(&flag)) =>
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

/// What blaze answers to what `command` asks rustc, given `stdin`, with the variables
/// `command` has; `None` when it has no answer. Nothing runs.
pub fn own_answer(command: &Command, stdin: &[u8]) -> Option<own::Answer> {
    let var = |name: &str| var_of(command, name);
    let args: Vec<OsString> = command.get_args().map(OsStr::to_owned).collect();
    answered_as(command.get_program(), &args, stdin, &var)
}

/// What blaze answers to rustc asked `args` with `stdin`, in this process's environment;
/// `None` when it has no answer. Nothing runs.
pub fn ours(args: &[OsString], stdin: &[u8]) -> Option<Vec<u8>> {
    own::answer(args, stdin, &|name| std::env::var_os(name))
}

/// [`ours`] with the warnings rustc shows answering.
pub fn ours_told(args: &[OsString], stdin: &[u8]) -> Option<own::Answer> {
    own::answered(args, stdin, &|name| std::env::var_os(name))
}

/// What rustc prints to standard output for text arguments, as text, failing with the
/// question blaze has no answer to; the warnings rustc prints with it are left out.
pub fn ours_said(args: &[&str]) -> anyhow::Result<String> {
    let asked: Vec<OsString> = args.iter().map(OsString::from).collect();
    let answer = ours_told(&asked, &[]).ok_or_else(|| no_answer(&asked))?;
    Ok(String::from_utf8_lossy(&answer.stdout).trim().to_owned())
}

/// [`own::answer`] of `program`, which is answered only as rustc: a driver of rustc's
/// (clippy's) may answer otherwise.
fn answered_as(
    program: &OsStr,
    args: &[OsString],
    stdin: &[u8],
    var: &dyn Fn(&str) -> Option<OsString>,
) -> Option<own::Answer> {
    if Path::new(program).file_stem()? != "rustc" {
        return None;
    }
    own::answered(args, stdin, var)
}

fn no_answer(args: &[OsString]) -> anyhow::Error {
    let shown: Vec<_> = args.iter().map(|it| it.to_string_lossy()).collect();
    anyhow::anyhow!(
        "blaze has no answer to `rustc {}`, and runs no rustc to ask (docs/no-rustc.md)",
        shown.join(" ")
    )
}

/// rustc's answer to what `command` asks it, given `stdin`, as blaze answers it. `command`
/// asks a question ([`is_question`]); one blaze has no answer to is an error, and nothing runs.
pub fn answer(command: &Command, stdin: &[u8]) -> io::Result<Output> {
    match own_answer(command, stdin) {
        Some(own::Answer { stdout, stderr }) => {
            Ok(Output { status: ExitStatus::default(), stdout, stderr })
        }
        None => {
            let args: Vec<OsString> = command.get_args().map(OsStr::to_owned).collect();
            Err(io::Error::other(no_answer(&args).to_string()))
        }
    }
}

/// What rust-analyzer runs to learn of the toolchain, answered here when it asks rustc.
/// A cargo it runs only to ask is given blaze as the workspace's wrapper, which answers
/// what cargo asks rustc in turn.
pub fn ask(command: &mut Command) -> Option<io::Result<Output>> {
    let program = Path::new(command.get_program()).file_stem()?.to_str()?.to_owned();
    match program.as_str() {
        "cargo" => {
            // What it asks of the toolchain and the configuration, blaze answers.
            if let Some(answered) = cargo::answer(command) {
                return Some(answered);
            }
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

/// Has a cargo ask rustc through blaze, unless the user named a wrapper of their own: one
/// that only asks (`cargo metadata`, `cargo rustc --print`, `--unit-graph`), and one whose
/// build is not blaze's (rust-analyzer's `cargo check` of build scripts, `blaze
/// check`), which compiles as it would without blaze. It is `RUSTC_WRAPPER`, not the
/// workspace's: cargo runs the print of `cargo rustc --print` through that one alone.
pub fn ready_cargo(command: &mut Command) {
    let named = [WRAPPER, "CARGO_BUILD_RUSTC_WRAPPER"].iter().any(|name| {
        command.get_envs().any(|(it, value)| it == *name && value.is_some())
            || std::env::var_os(name).is_some_and(|it| !it.is_empty())
    });
    let wrapper = crate::cargo::load_wrapper::path().ok().filter(|it| it.is_file());
    if let Some(wrapper) = wrapper
        && !named
    {
        command.env(WRAPPER, wrapper).env(ONLY_ASKS, "1");
    }
}

/// Answers the question cargo asked the wrapper, and ends as rustc would have. `None` when
/// it is no question.
pub fn reply(rustc: &OsStr, args: &[OsString]) -> Option<io::Result<i32>> {
    if !is_question(args) {
        return None;
    }
    let mut stdin = Vec::new();
    if args.iter().any(|it| it == "-")
        && let Err(e) = io::stdin().read_to_end(&mut stdin)
    {
        return Some(Err(e));
    }
    let var = |name: &str| std::env::var_os(name);
    Some(match answered_as(rustc, args, &stdin, &var) {
        Some(answer) => io::stderr()
            .write_all(&answer.stderr)
            .and_then(|()| io::stdout().write_all(&answer.stdout))
            .map(|()| 0),
        None => {
            eprintln!("error: {}", no_answer(args));
            Ok(1)
        }
    })
}

/// The variable `name` as `command` will have it.
fn var_of(command: &Command, name: &str) -> Option<OsString> {
    match command.get_envs().find(|(it, _)| *it == name) {
        Some((_, value)) => value.map(OsStr::to_owned),
        None => std::env::var_os(name),
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
        // cargo's `--print cfg` under uitest.py's RUSTFLAGS.
        assert!(question(
            "--print link-args -C save-temps -C link-dead-code --cap-lints allow \
             -L native=/ui/native --cfg e2024 --target x86_64-unknown-linux-gnu --print cfg"
        ));
        // rust-analyzer's `cargo rustc -- --print cfg` names the library, which rustc does
        // not read for that.
        assert!(question(
            "--crate-name p --edition=2024 src/lib.rs --crate-type lib --emit=dep-info,metadata,link \
             -C metadata=d4ed28d9543870f2 --out-dir target/debug/deps --print cfg"
        ));
    }

    /// Any rustc is answered as blaze's release, whatever toolchain rustup, `RUSTC` or a
    /// toolchain file would pick, and a rustc that is no toolchain's is answered too.
    #[test]
    fn every_rustc_is_answered_as_blazes_release() {
        let var = |name: &str| match name {
            "RUSTUP_TOOLCHAIN" => Some(OsString::from("1.70.0")),
            "RUSTC" => Some(OsString::from("/no/such/rustc")),
            _ => None,
        };
        let version = [OsString::from("--version")];
        for program in ["rustc", "/no/such/toolchain/bin/rustc", "/usr/bin/rustc"] {
            let said = answered_as(OsStr::new(program), &version, &[], &var).unwrap();
            let said = String::from_utf8(said.stdout).unwrap();
            assert_eq!(said.trim(), format!("rustc {}", crate::cargo::toolchain::ours_says()));
        }
        assert_eq!(answered_as(OsStr::new("clippy-driver"), &version, &[], &var), None);
    }

    /// What rust-analyzer asks for the data layout of the target a proc macro's module is
    /// built for (`project-model`'s `target_data`), as it asks it, with a deployment target
    /// set as the release workflow sets it for every host: it says nothing of wasm's spec.
    #[test]
    fn the_spec_of_the_wasm_target_is_answered() {
        let spec_of = |target: &str, deployment: Option<&str>| {
            let program = Path::new("/no/such/toolchain/bin/rustc");
            let mut asked = Command::new(program);
            asked.current_dir("/").env("RUSTC_BOOTSTRAP", "1").env("RUSTUP_AUTO_INSTALL", "0");
            asked.env_remove("SDKROOT");
            match deployment {
                Some(it) => asked.env("MACOSX_DEPLOYMENT_TARGET", it),
                None => asked.env_remove("MACOSX_DEPLOYMENT_TARGET"),
            };
            asked.args(["-Z", "unstable-options", "--print", "target-spec-json"]);
            asked.args(["--target", target]);
            ask(&mut asked).expect("a question")
        };
        // The specs are recorded by the host rustc ran on, and not of every host.
        let Some(host) = crate::cargo::toolchain::host() else { return };
        if spec_of(host, None).is_err() {
            return;
        }
        let output =
            spec_of(crate::cargo::own_macro::WASM_TARGET, Some("13.0")).expect("an answer");
        let spec: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(spec["arch"], "wasm32");
        assert!(spec["data-layout"].as_str().is_some_and(|it| it.starts_with("e-m:e-p:32:32")));
        // The -linux-gnu specs are each host's own (a Linux rustc links with its lld), so a host
        // that was not recorded has no answer to them.
        assert!(spec_of("aarch64-unknown-linux-musl", Some("13.0")).is_ok());
        // An Apple target's spec names no deployment target: it is the same under one.
        let apple = spec_of("aarch64-apple-darwin", None).expect("an answer");
        let under = spec_of("aarch64-apple-darwin", Some("13.0")).expect("an answer");
        assert_eq!(under.stdout, apple.stdout);
    }

    #[test]
    fn what_is_not() {
        assert!(!question("--crate-name p src/lib.rs --crate-type lib --emit=link"));
        assert!(!question("src/lib.rs --print file-names"));
        assert!(!question("--print link-args src/main.rs"));
        assert!(!question("--print link-args"));
        assert!(!question("--print=link-args=out.txt src/main.rs"));
        assert!(!question("--print cfg --target my-target.json"));
        assert!(!question("@args.txt"));
        assert!(!question(""));
    }
}
