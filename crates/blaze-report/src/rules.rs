//! Every rule a [`crate::Report`] can name (`Report::rule`), with the long text `blaze explain
//! <rule>` prints. One home: a refusal names its rule by id here and nowhere spells its long
//! text itself. Ids are `area:name`, sorted, so [`find`] searches them.

/// One rule: its id, a line saying what it holds, and the long text: why, and the ways out.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Rule {
    pub id: &'static str,
    pub title: &'static str,
    pub text: &'static str,
}

/// Sorted by id.
pub const RULES: &[Rule] = &[
    Rule {
        id: "build-script:error",
        title: "a build script logged an error, which fails its package's build",
        text: "A build script that prints `cargo::error=MESSAGE` (or `cargo:error=`) fails the build of its \
               package, as cargo fails it, even when it exits 0. blaze shows each message it logged; a script \
               that also prints `cargo:blaze-report=` lines says its failure as a report of its own, with the \
               facts and fixes it knows (docs/v2/plan/design/structured-output.md, \"Build scripts\").\n\n\
               Everything that depends on the package is held back: `blaze explain <package>` lists it.",
    },
    Rule {
        id: "build-script:failed",
        title: "a build script exited with an error or was killed",
        text: "A build script that exits with a code other than 0, or is killed by a signal, fails the build of \
               its package. blaze shows how it ended and the last lines it wrote to stderr and stdout; what it \
               printed in full is in its log when the report names one.\n\n\
               Everything that depends on the package is held back: `blaze explain <package>` lists it.",
    },
    Rule {
        id: "build-script:output",
        title: "blaze can't read what a build script printed",
        text: "A build script's `cargo:` and `cargo::` lines are read as cargo reads them; a line cargo would \
               refuse (an unknown `cargo::` key, a malformed value) fails the build of its package, naming the \
               line.",
    },
    Rule {
        id: "llvm:pin",
        title: "blaze links only the LLVM crates/hir-ty-codegen/llvm.json pins",
        text: "blaze links exactly one LLVM: the version and commit crates/hir-ty-codegen/llvm.json pins, built \
               by `.github/scripts/llvm.py build`, which writes `blaze-llvm.json` into the prefix it makes. \
               llvm-sys's build script finds it only through LLVM_SYS_231_PREFIX: no other variable, no PATH, \
               no Homebrew. A stock LLVM, or a build at another commit, is refused, since code generated against \
               another LLVM is not the code blaze's tests and caches are of.\n\n\
               Ways out: build inside `nix develop`, which sets LLVM_SYS_231_PREFIX to the flake's blaze-llvm; or \
               unpack the release for your target with `python3 .github/scripts/llvm.py '{\"command\": \"fetch\", \
               \"target\": \"<triple>\", \"dir\": \"<dir>\"}'` and set LLVM_SYS_231_PREFIX to <dir>; for a target \
               llvm.json has no release for, build one with the `llvm` workflow (.github/workflows/llvm.yaml).",
    },
    Rule {
        id: "sandbox:link-arg",
        title: "a sandboxed build script may give the linker only listed arguments",
        text: "A build script runs in the sandbox (docs/cargo.md, \"Build scripts and proc macros run in a \
               sandbox\"), and what it prints reaches every link of its package and of what links it, so its \
               `rustc-link-arg` lines (and their -bins, -bin=, -tests, -benches, -examples and -cdylib forms) \
               may give only arguments that change how the output is linked, never which program links it or \
               what it loads: the list in crates/blaze-cargo/src/directives.rs (LINK_ARGS), `-lNAME`, and an \
               rpath to $ORIGIN or to a directory `rustc-link-search` may name.\n\n\
               Way out: a script that needs more runs outside the sandbox, by name in \
               `unsandboxed-build-scripts` under [workspace.metadata.blaze.sandbox] in the workspace's root \
               manifest.",
    },
    Rule {
        id: "sandbox:link-dir",
        title: "a sandboxed build script may name only its own, its build's and the system's library directories",
        text: "A build script runs in the sandbox, and a `rustc-link-search` (or a `-L` in `rustc-flags`) it \
               prints reaches every link of its package and of what links it. So the directory must be inside \
               its OUT_DIR, its package, the build directory (its dependencies' OUT_DIRs), a system directory \
               the linker searches anyway (/usr/lib, /nix/store, Homebrew's and Apple's SDKs; the list is \
               SYSTEM in crates/blaze-cargo/src/directives.rs), or a layout of blaze's own pinned native \
               inputs (a `sha256-<digest>` directory under `<BLAZE_HOME>/native`).\n\n\
               Ways out: a directory of the system it links from goes in `build-script-link-dirs = [...]` under \
               [workspace.metadata.blaze.sandbox] in the workspace's root manifest (`${VAR}/lib` names one \
               under a variable the script is given); a script that needs more runs outside the sandbox, by name \
               in `unsandboxed-build-scripts` there.",
    },
    Rule {
        id: "sandbox:rustc-env",
        title: "a sandboxed build script may not set a variable cargo or the toolchain owns",
        text: "A build script's `rustc-env=NAME=VALUE` sets NAME for its package's compilation. A sandboxed \
               script may not set what cargo gives a crate itself (CARGO_PKG_*, CARGO_BIN_EXE_*, CARGO_MANIFEST_DIR \
               and the rest) or what rustc, rustdoc or cargo read while building it (RUSTC_BOOTSTRAP, RUSTFLAGS, \
               RUSTC_WRAPPER and the rest; GIVEN in crates/blaze-cargo/src/directives.rs): that would change \
               another tool's behaviour or a crate's identity from outside the sandbox.\n\n\
               Way out: a script that needs to runs outside the sandbox, by name in `unsandboxed-build-scripts` \
               under [workspace.metadata.blaze.sandbox] in the workspace's root manifest.",
    },
];

/// The rule `id` names.
pub fn find(id: &str) -> Option<&'static Rule> {
    RULES.binary_search_by(|rule| rule.id.cmp(id)).ok().map(|at| &RULES[at])
}

/// `rule` as `blaze explain` prints it: its id and title, then its long text.
pub fn explain_into(out: &mut String, rule: &Rule) {
    out.push_str(rule.id);
    out.push_str(": ");
    out.push_str(rule.title);
    out.push_str("\n\n");
    out.push_str(rule.text);
    out.push('\n');
}
