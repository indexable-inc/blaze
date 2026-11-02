//! The questions cargo, rust-analyzer, build scripts and blaze ask rustc, answered by blaze:
//! `-vV`, `--print cfg`, `sysroot`, `target-libdir`, `target-spec-json`, the names of the files
//! of each kind of crate, and the rest of what a release of rustc says the same way every time
//! it is asked. blaze answers them for its own release, whichever rustc was named, from what it
//! knows of that release and its targets ([`own`]). No rustc is ever run to answer: a question
//! blaze does not know the answer of fails, saying which.
//!
//! A build script asks through blaze's rustc stand-in ([`crate::as_rustc`]); blaze itself
//! through [`ours`].

use std::ffi::OsString;

pub mod own;

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

/// The flags of rustc that cargo passes with their value as the next argument.
const VALUED: [&str; 23] = [
    "-C",
    "-L",
    "-Z",
    "-l",
    "-A",
    "-W",
    "-D",
    "-F",
    "--cfg",
    "--check-cfg",
    "--extern",
    "--out-dir",
    "--target",
    "--crate-name",
    "--crate-type",
    "--cap-lints",
    "--emit",
    "--edition",
    "--error-format",
    "--json",
    "--color",
    "--diagnostic-width",
    "--remap-path-prefix",
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
                && (VALUED.contains(&flag) || ALSO_VALUED.contains(&flag)) =>
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

/// What blaze answers to rustc asked `args` with `stdin`, in this process's environment;
/// `None` when it has no answer. Nothing runs.
pub fn ours(args: &[OsString], stdin: &[u8]) -> Option<Vec<u8>> {
    own::answer(args, stdin, own::Vars::Process)
}

/// [`ours`] with the warnings rustc shows answering.
pub fn ours_told(args: &[OsString], stdin: &[u8]) -> Option<own::Answer> {
    own::answered(args, stdin, own::Vars::Process)
}

/// What rustc prints to standard output for text arguments, as text, failing with the
/// question blaze has no answer to; the warnings rustc prints with it are left out.
pub fn ours_said(args: &[&str]) -> anyhow::Result<String> {
    let asked: Vec<OsString> = args.iter().map(OsString::from).collect();
    let answer = ours_told(&asked, &[]).ok_or_else(|| no_answer(&asked))?;
    Ok(String::from_utf8_lossy(&answer.stdout).trim().to_owned())
}

fn no_answer(args: &[OsString]) -> anyhow::Error {
    let shown: Vec<_> = args.iter().map(|it| it.to_string_lossy()).collect();
    anyhow::anyhow!(
        "blaze has no answer to `rustc {}`, and runs no rustc to ask (docs/no-rustc.md)",
        shown.join(" ")
    )
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

    /// rustc is answered as blaze's release, whatever toolchain rustup, `RUSTC` or a toolchain
    /// file would pick.
    #[test]
    fn every_rustc_is_answered_as_blazes_release() {
        let vars = own::Vars::Only(&[("RUSTUP_TOOLCHAIN", "1.70.0"), ("RUSTC", "/no/such/rustc")]);
        let said = own::answered(&[OsString::from("--version")], &[], vars).unwrap();
        let said = String::from_utf8(said.stdout).unwrap();
        assert_eq!(said.trim(), format!("rustc {}", crate::toolchain::ours_says()));
    }

    /// The data layout of the target proc macros are built for as wasm, asked as a build
    /// script asks it, with a deployment target set as the release workflow sets it for every
    /// host: it says nothing of wasm's spec.
    #[test]
    fn the_spec_of_the_wasm_target_is_answered() {
        const WASM_TARGET: &str = "wasm32-wasip1";
        let spec_of = |target: &str, deployment: Option<&str>| {
            let mut vars = vec![("RUSTC_BOOTSTRAP", "1"), ("RUSTUP_AUTO_INSTALL", "0")];
            vars.extend(deployment.map(|it| ("MACOSX_DEPLOYMENT_TARGET", it)));
            let args = ["-Z", "unstable-options", "--print", "target-spec-json", "--target", target];
            let args: Vec<OsString> = args.iter().map(OsString::from).collect();
            own::answered(&args, &[], own::Vars::Only(&vars))
        };
        // The specs are recorded by the host rustc ran on, and not of every host.
        let Some(host) = crate::toolchain::host() else { return };
        if spec_of(host, None).is_none() {
            return;
        }
        let output = spec_of(WASM_TARGET, Some("13.0")).expect("an answer");
        let spec: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(spec["arch"], "wasm32");
        assert!(spec["data-layout"].as_str().is_some_and(|it| it.starts_with("e-m:e-p:32:32")));
        // The -linux-gnu specs are each host's own (a Linux rustc links with its lld), so a host
        // that was not recorded has no answer to them.
        assert!(spec_of("aarch64-unknown-linux-musl", Some("13.0")).is_some());
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
