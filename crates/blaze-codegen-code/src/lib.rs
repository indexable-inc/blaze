//! The content identity of blaze's code generator (`hir_ty_codegen::v2::CodegenCode`): what
//! `Toolchain::id`, and so every stored dev template, is keyed by in place of the bytes of the
//! blaze executable. The build script of the program that runs the code generator works it out
//! ([`emit`]) and sets `BLAZE_CODEGEN_CODE`; the program hands it over before any command runs.
//! An edit outside the code generator's packages (the CLI, the daemon, the lints) keeps it, so a
//! rebuilt blaze reads the templates the last one kept.
//!
//! The same walk works out the front end's code identity ([`FRONT_END`]), which blaze-engine's
//! build script sets as `BLAZE_FRONT_END_CODE` beside it ([`emit_all`], one walk for both) and
//! what the front end computes once for everyone is stored by: dependency crates' images, their
//! check outcomes, checked bodies and constants, the goal and item stores, and kept proc-macro
//! expansions (blaze-cargo's `serve::unit::deps::front_end`). An edit outside the front end's
//! packages (the code generator, the CLI, the daemon, the lints) keeps it, so a rebuilt blaze
//! serves the front end the last one stored. The trait solver is of it (hir-ty is built from the
//! vendored `rustc_next_trait_solver`, `rustc_type_ir` and `rustc_type_ir_macros`, their
//! `PATCHES.md` and patches among their files), so a solver edit misses every stored answer. Each
//! is an [`Identity`]; [`emit_of`], [`emit_all`] and [`work_out_of`] take them. Only the program's
//! build script works one out: no library of the front end has one, so an edit of the lock or of a
//! vendored file reruns no build script but the program's, and recompiles the program alone.
//!
//! The same walk works out a third, [`RECORD`]: of what makes a unit's record (blaze-cargo, and
//! every package it is built from but those [`RECORD_CUT`] names), which blaze-engine's build
//! script sets as `BLAZE_RECORD_CODE`. A unit's record and the standard library's shared directory
//! are keyed by it with the other two (blaze-cargo's `serve::RecordMakers`), never by the
//! executable's bytes, so an edit of blaze-engine, the explorer, the plane's history or the CLI's
//! rendering keeps every record. A closure leaves out a package its roots are built from only by
//! an [`Identity::cut`], which says why no value keyed by the identity depends on its code and
//! lists each file of a root that names it ([`Cut::named_in`], held both ways by `tests::cuts`);
//! a cut that a package of the closure other than a root is built from is refused. What the
//! executable's bytes covered and none of the three does: the C compiler, its flags and its SDK
//! (below), and `--features` blaze is built with past what a build script is told.
//!
//! A package cut from the checkout (published, or a Nix unit's source) has no checkout to work an
//! identity out of. The checkout ships what of it the checkout alone decides with the package
//! ([`Tree`], [`Identity::shipped`], written by the `ship-code-identity` tool), and the program's
//! build script finishes it with how it is built ([`work_out_shipped`]): the identity a build of
//! the checkout in that environment has. With neither a checkout nor a shipped tree the build is
//! refused, naming both ([`work_out_here`]).
//!
//! It is of every input the code generator's code is built from ([`Worked::inputs`] lists each):
//!
//! 1. Every package the [`ROOTS`] the program links are built from: their normal and build
//!    dependencies, transitively, never walking into a package the identity cuts
//!    ([`Identity::cut`]), read from `Cargo.lock` and the manifests of the checkout's
//!    packages, never by running cargo (a build script's `$CARGO` is blaze under blaze, AGENTS.md
//!    rule 2). The lock lists every platform's and every feature's dependencies, a superset of
//!    what the build compiles; a checkout package's dev-dependencies are left out by its
//!    manifest. A registry package by name, version and its checksum in the lock file; a package
//!    of the checkout by every file under its directory, by path relative to it and its bytes,
//!    but a name starting with `.` and, at its root, `target` and the `tests`, `benches` and
//!    `examples` directories when none of its library, binaries or build script is in them. A git
//!    package is refused, naming it: it has no checksum in the lock.
//! 2. Every manifest of the workspace's members, the root's and that of each workspace a package
//!    of the checkout inherits from: features one package turns on in another, profiles,
//!    patches; and cargo's configuration files (`.cargo/config.toml`, or `.cargo/config`) in the
//!    workspace's root and every directory above it, and in `$CARGO_HOME`. And every package
//!    outside the closure that depends on one of it by any edge of the lock, at any depth: cargo
//!    unifies features across the packages it builds together, so each can turn on a feature of
//!    the closure's; a registry one by its checksum, one of the checkout by its manifest. Not
//!    seen: `--config` on cargo's command line, which packages one cargo command builds together
//!    (`-p`, `--workspace`, `--features a/b`), and a configuration file made later where there was
//!    none (a build script is rerun only on files that exist).
//! 3. The files outside every package that the packages read ([`EXTRA`]) and the variables
//!    they or their build scripts read ([`READ_ENV`]).
//! 4. The stencil libraries, when blaze-stencil is a package of the closure: each by the id its
//!    pin (`crates/blaze-stencil/stencils.json`) names, which blaze-stencil's build script holds
//!    every library it includes to, made by LLVM or the committed ones in its `prebuilt/` (this
//!    crate includes its reader, crates/blaze-stencil/pin.rs). The pin and those libraries are
//!    files of blaze-stencil's too.
//! 5. LLVM, when `llvm-sys` is a package of the closure (blaze-stencil's build script makes the
//!    stencils with it in every build but the seed's; the `llvm` feature links it too), and the
//!    program is not built as the seed ([`SEED_FEATURE`], which has no LLVM and whose stencils
//!    are the pinned ones, step 4): the pin, as
//!    `crates/hir-ty-codegen/llvm.json` says it (version, commit and the sha256 of the release
//!    for `TARGET` and for `HOST`), since vendor/llvm-sys/pin.rs refuses every other LLVM; and,
//!    when `$LLVM_SYS_231_PREFIX` is set, its `blaze-llvm.json`, which pin.rs checks is the pin
//!    (this crate includes pin.rs: one check, one refusal). An unset prefix is no error here:
//!    llvm-sys's own build script stops the build that needs it.
//! 6. How blaze is built: `TARGET`, `HOST`, `PROFILE`, `OPT_LEVEL`, `DEBUG`, every
//!    `CARGO_CFG_*`, `CARGO_PROFILE_*` and `CARGO_FEATURE_*` the build script is given,
//!    `CARGO_ENCODED_RUSTFLAGS`, and the compiler that builds it (`$RUSTC -vV`). blaze, as
//!    `rustc`, ends its `-vV` with a `blaze-identity:` line naming its own code identity
//!    ([`BLAZE_IDENTITY`]), so a blaze built by blaze is another code than one rustc built from
//!    the same tree, and one built by another blaze another again.
//!
//! No absolute path is in it, so one tree checked out twice is one code generator. Not in it,
//! named: the C compiler, its flags and its SDK, which C of registry and vendored packages is
//! built with ([`unkeyed_env`]).
//!
//! An input that can't be read stops the build with an error naming it, never a coarser key
//! (AGENTS.md rule 3). [`coverage::hold`] holds the list to what rustc says each package read
//! (its dep-info) and to what each package's build script said it reads
//! (`rerun-if-env-changed`, `rerun-if-changed`): hir-ty-codegen's test
//! `the_code_identity_hashes_every_input_the_code_generator_read` for [`CODEGEN`], blaze-cargo's
//! `the_front_end_identity_hashes_every_input_the_front_end_read` for [`FRONT_END`] and
//! `the_record_identity_hashes_every_input_what_makes_a_record_read` for [`RECORD`].
//!
//! Code outside the closure that the code generator or front end runs through hooks: what
//! shapes the identity's values is a root ([`Identity::roots`]); the rest is listed in
//! [`Identity::reviewed`], each with why it can't change them, and a test holds every
//! implementation, in a package the program links, of a hook trait to one or the other: the
//! traits [`Identity::hooks`] lists, and every trait of the closure's packages that they name
//! `dyn`.
//!
//! The window it leaves, accepted (AGENTS.md rule 22): the build script hashes the tree once,
//! and cargo compiles the closure's crates before or after it. A file saved in the live tree
//! between the two gives one binary an identity of other sources than its code, in either order.
//! [`emit`] refuses a file, lock or manifest read, or a package directory listed, that changed
//! while it worked (each is stamped before it is read and checked at the end). The runner and
//! land.py build from snapshots, which nothing writes to, so only a direct build in a tree being
//! edited is open; working the identity out again over the exact tree a checkpoint or landing
//! built and comparing it with the binary's closes that (indexable-inc/blaze#1619).
//!
//! What a build script reruns on is exactly what is hashed ([`Worked::watched`]), so an edit of a
//! package's `tests/`, `benches/`, `examples/` or a dotfile reruns nothing (cargo has no early
//! cutoff: a rerun recompiles the package and everything after it). cargo watches a directory
//! only whole, so a directory holding a skipped entry (a package's root, which mostly holds
//! `tests/`) is watched by its files and its whole subdirectories, and a file newly made right in
//! it is hashed only when something else reruns the script; the same comparison over the exact
//! tree closes that. Code reaches a new file only through an edit of one already watched, but
//! for a `build.rs` cargo finds by itself. The lock is watched whole: cargo can't watch the lines
//! of the closure's packages alone.
//!
//! TEMPORARY in its granularity: a whole package is one input, so an edit anywhere in a package
//! of the code generator misses every template. It is replaced by the code generator's
//! behaviour identity, `core::reflect::fn_info(<codegen entry>).code_id`, once const eval
//! answers `fn_info` (docs/v2/plan/design/code-identity.md, "Toolchain::id").

pub mod coverage;
pub mod fixture;
#[cfg(test)]
mod tests;

use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    process::Command,
    time::SystemTime,
};

/// The domain of the code identity; another formula is another domain.
pub const DOMAIN: &str = "blaze v2 codegen code 3";
/// The domain of one input's digest.
const INPUT_DOMAIN: &str = "blaze v2 codegen code input 1";
/// The code generator: the program must link it.
pub const CODE_GENERATOR: &str = "hir-ty-codegen";

/// A package whose code shapes the code generator's output, and every package it is built from.
#[derive(Debug)]
pub struct Root {
    pub package: &'static str,
    /// Why its code shapes the bytes the code generator makes.
    pub why: &'static str,
}

/// The packages whose code shapes the generated code, when the program links them: the code
/// generator, and the code installed into the front end or the code generator through `dyn`
/// hooks from past it (`blaze-items`' `GoalFacts` is in the code generator's closure already).
pub const ROOTS: &[Root] = &[
    Root {
        package: CODE_GENERATOR,
        why: "the code generator, the front end it reads and the backends it drives",
    },
    Root {
        package: "blaze-item-hash",
        why: "`Bodies` is installed as `hir_ty::body_store::BodyFacts` (blaze-cargo \
              serve/unit/deps.rs): its `identity`, `classify`, `value` and `def_of` choose the \
              built MIR and inference results the code generator compiles",
    },
];

/// One content identity a build script works out over a closure of packages ([`work_out_of`],
/// [`emit_of`]): the code generator's ([`CODEGEN`]) or the front end's ([`FRONT_END`]). One
/// formula and one walk of the checkout for both, so what a package's files are, how a registry
/// package is keyed and which saves are refused are the same rule everywhere.
#[derive(Debug)]
pub struct Identity {
    /// What it is, for errors: "the code generator's code identity".
    pub what: &'static str,
    /// The domain of the identity; another formula is another domain.
    pub domain: &'static str,
    /// The variable the build script sets to the identity's hex.
    pub env: &'static str,
    /// The packages whose closure it is of, when the program links them.
    pub roots: &'static [Root],
    /// The packages the closure is not walked into: neither they nor what only they are built
    /// from is an input. Each says why no value keyed by the identity depends on its code, and
    /// lists the files of a root that name it ([`Cut::named_in`]). Their manifests stay inputs
    /// where they can turn on a feature of the closure's (step 2). Refused
    /// ([`Workspace::closure_names_of`]): a cut that is a root, one the program does not link,
    /// and one a package of the closure other than a root is built from (a cut is only what the
    /// roots call, through files a test holds to [`Cut::named_in`]).
    pub cut: &'static [Cut],
    /// The root the program must link: without it there is nothing to identify.
    pub required: &'static str,
    /// Files outside every package that the closure reads: the package that reads it and the
    /// path relative to that package's directory. A directory is every file under it, but those
    /// of a dot name, so a file added to it is an input with no list to keep.
    pub extra: &'static [(&'static str, &'static str)],
    /// Whether how the program is built is an input too: the workspace's manifests and cargo's
    /// configuration, the stencil libraries, LLVM, the build's variables and the compiler
    /// (steps 2, 4, 5 and 6 of the crate doc). Without them the identity is of the closure's
    /// sources alone, and a test can work it out again over the tree with nothing but the tree.
    pub how_built: bool,
    /// The file in `OUT_DIR` [`emit_of`] lists the inputs in.
    pub listing: &'static str,
    /// The file a package cut from the checkout carries its [`Tree`] in, beside its manifest
    /// ([`ship`], [`work_out_here`]).
    pub shipped: &'static str,
    /// The hook traits defined in the closure whose implementations past it the test checks,
    /// besides every trait of the closure it names `dyn`, which the test finds itself: each
    /// implementation in a package the program links is in the closure, or in `reviewed`.
    pub hooks: &'static [&'static str],
    /// Implementations of a hook past the closure, reviewed: none changes the identity's values.
    pub reviewed: &'static [Hook],
}

/// The code generator's code identity: what stored templates are keyed by.
pub const CODEGEN: Identity = Identity {
    what: "the code generator's code identity",
    domain: DOMAIN,
    env: ENV,
    roots: ROOTS,
    cut: &[],
    required: CODE_GENERATOR,
    extra: EXTRA,
    how_built: true,
    listing: "codegen-code-inputs.txt",
    shipped: "shipped-codegen-code.txt",
    hooks: HOOK_TRAITS,
    reviewed: REVIEWED_HOOKS,
};

/// The packages whose code computes what the front end stores for everyone, when the program
/// links them, and so every package they are built from: hir-expand, hir-ty-mir and the borrow
/// checker, base-db, the trait solver, proc-macro-api's server and Wasmtime among them.
///
/// Not a root: blaze-lints, whose clippy lints run on a checked crate after its check outcome is
/// kept (blaze-cargo's `compile::bodies` keeps it before `linted`), so what they find is in the
/// unit's record alone, which [`RECORD`] keys (blaze-lints is of its closure); the code generator; and
/// blaze-cargo, which drives the front end and keeps what it made by the keys it made.
pub const FRONT_END_ROOTS: &[Root] = &[
    Root {
        package: "hir-def",
        why: "resolves every crate: its def map, lang items and signatures are the dependency \
              images (`hir_def::deps`), and through hir-expand it expands every macro",
    },
    Root {
        package: "hir-ty",
        why: "type checks and infers: the checked bodies (`body_store`), constants \
              (`consteval::deps`) and trait goals' answers (`next_solver::goal_store`) are its",
    },
    Root {
        package: "blaze-diagnostics",
        why: "checks a crate (`check_planned`): its diagnostics are the check outcome kept for a \
              dependency crate (`blaze_diagnostics::cache`)",
    },
    Root {
        package: "blaze-items",
        why: "`goals::Goals` is installed as `hir_ty::next_solver::goal_store::GoalFacts` (blaze-cargo \
              serve/unit/deps.rs): the facts every kept answer is checked against",
    },
    Root {
        package: "blaze-item-hash",
        why: "`Bodies` is installed as `hir_ty::body_store::BodyFacts` (blaze-cargo \
              serve/unit/deps.rs): its identities and facts choose the bodies the item store keeps",
    },
    Root {
        package: "blaze-lower",
        why: "lowers cargo's units into the front end's database: the crates, files, cfgs and \
              variables every image is resolved from, and `serve_deps` and `store_deps` (hir-def's \
              serve and store)",
    },
    Root {
        package: "blaze-macro-eval",
        why: "runs proc macros (`Evaluated`, installed as hir-expand's `ProcMacroExpander`): \
              what a macro expands to is in every image of a crate that calls it, and is the \
              kept expansion itself (`proc_macro_api::kept`)",
    },
];

/// Every identity a build script of the checkout works out: what the `ship-code-identity` tool
/// ships with a package cut from it.
pub const IDENTITIES: &[&Identity] = &[&CODEGEN, &FRONT_END, &RECORD];

/// The front end's code identity: what the values the front end computes once for everyone are
/// stored by (blaze-cargo's `serve::unit::deps::front_end`): dependency crates' images, their
/// check outcomes, checked bodies and constants, trait goals' answers, the item store and kept
/// proc-macro expansions. Of every package [`FRONT_END_ROOTS`] are built from and how blaze is
/// built (`how_built`), as [`CODEGEN`] is, so an edit of the code generator, the CLI, the daemon
/// or the lints keeps every value stored.
///
/// Finer for images alone is not clean: what a crate resolves to is a function of what its
/// macros expand to, and the proc-macro engine (blaze-macro-eval) is built from hir-ty and
/// blaze-lower, so the closure of hir-def's and the engine's packages leaves out only
/// blaze-diagnostics and blaze-item-hash, and the check outcomes stored under the images' keys
/// would then need this identity in a key of their own.
pub const FRONT_END: Identity = Identity {
    what: "the front end's code identity",
    domain: "blaze v2 front end code 1",
    env: "BLAZE_FRONT_END_CODE",
    roots: FRONT_END_ROOTS,
    cut: &[],
    required: "hir-def",
    extra: FRONT_END_EXTRA,
    how_built: true,
    listing: "front-end-code-inputs.txt",
    shipped: "shipped-front-end-code.txt",
    hooks: FRONT_END_HOOKS,
    reviewed: FRONT_END_REVIEWED,
};

/// Files outside every package that the front end's closure reads (dep-info says so,
/// `blaze_cargo`'s `front_end_identity` test), and the pinned nightly: the nightly the vendored
/// trait solver is synced from (vendor/rustc_next_trait_solver/PATCHES.md), so a toolchain bump
/// is another solver even before its sources are synced; and the pinned nightly's proc-macro
/// bridge, which blaze-macro-eval serves, by its directory.
pub const FRONT_END_EXTRA: &[(&str, &str)] = &[
    ("hir-ty", "../../harness/toolchain"),
    ("blaze-macro-eval", "../../library/proc_macro/src/bridge"),
];

/// The front end's hook traits past [`FRONT_END`]'s closure: every trait of the closure it names
/// `dyn` (the test finds them: `ImageStore`, `Held`, `ProcMacroExpander`, `BodyFacts`,
/// `GoalFacts`, `Tree`, ...), and the MIR evaluator's machine, generic, which constants are
/// evaluated through.
pub const FRONT_END_HOOKS: &[&str] = &["Machine", "Foreign"];

/// Implementations of the front end's hooks past its closure, reviewed: none changes a value
/// the front end stores.
pub const FRONT_END_REVIEWED: &[Hook] = &[
    Hook {
        package: "blaze-cargo",
        hook: "ImageStore",
        why: "`Images` gets and puts, by key, the images and other values the front end makes, \
              and works out again what a kept expansion read (`reads_now`) for hir-def to check an \
              image still holds: a store, which makes no value",
    },
    Hook {
        package: "blaze-cargo",
        hook: "Held",
        why: "`InStore` holds the bytes of an image read from the store",
    },
    Hook {
        package: "blaze-cargo",
        hook: "Builder",
        why: "`Jobs` builds what a session of blaze-api asks for: the front end runs no session \
              (blaze-diagnostics is built from blaze-api for its `Diagnostic` alone)",
    },
    Hook {
        package: "blaze-cargo",
        hook: "Local",
        why: "`Once` carries the one-shot build's frames to the daemon in its own process: the \
              front end sends none",
    },
    Hook {
        package: "blaze-engine",
        hook: "Tree",
        why: "`ProbeFiles` gives a `rustc` probe's files by their content: an image is keyed by \
              its crate's files' content (`hir_def::deps::key`)",
    },
    Hook {
        package: "blaze-wasm-pack",
        hook: "ProcMacroExpander",
        why: "`Recorder` gives what the expander it wraps gives, unchanged, keeping a copy; \
              `Replay` is made only by `Pack::project`, for blaze-wasm, which stores no front-end \
              value",
    },
    Hook {
        package: "hir-ty-codegen",
        hook: "Profiler",
        why: "`Untimed` is Cranelift's profiler of a thread that reads no clock: it times passes \
              and makes nothing",
    },
    Hook {
        package: "hir-ty-ub",
        hook: "Machine",
        why: "`Checker` runs `blaze test --ub`'s tests: what it finds is the test run's outcome, \
              in the unit's record; no constant the front end stores is evaluated with it",
    },
];

/// What makes a unit's record, when the program links it, and so every package it is built
/// from but [`RECORD_CUT`]: the front end, the code generator and its backends, the lints, the
/// linker, user code's runners, the store and the encodings among them.
pub const RECORD_ROOTS: &[Root] = &[Root {
    package: "blaze-cargo",
    why: "makes a unit's record (`serve::unit::compile`): it plans the unit, drives the front end, the \
          lints (blaze-lints), the code generator and the linker (blaze-link) over it, renders their \
          diagnostics, writes the files cargo would and encodes the record (`Record`, its header)",
}];

/// The packages blaze-cargo is built from whose code shapes no record: each serves, orders or
/// shows what was made, and nothing that makes a record calls it.
pub const RECORD_CUT: &[Cut] = &[
    Cut {
        package: "blaze-explore",
        why: "the explorer's server, started by `serve::observe` on the first request for it: it serves \
              the history, spans and SQL of builds to a browser and hands blaze-cargo nothing back",
        named_in: &[NamedIn {
            package: "blaze-cargo",
            file: "src/serve/observe.rs",
            why: "starts the explorer for `blaze explore` and encodes `blaze debug sql`'s Arrow IPC: \
                  answers to a client about finished builds, past every record",
        }],
    },
    Cut {
        package: "blaze-web",
        why: "the build page blaze-explore serves: built into blaze-explore alone",
        named_in: &[],
    },
    Cut {
        package: "flecs-c",
        why: "the ECS blaze-web's page is made with: built into blaze-web alone",
        named_in: &[],
    },
    Cut {
        package: "blaze-history",
        why: "the plane's costs and peaks learned from past runs (`serve::plane`, kept in the daemon's \
              image by `serve::image`): it orders and admits work, and what a unit makes does not \
              depend on when or beside what it ran (AGENTS.md rule 25)",
        named_in: &[
            NamedIn {
                package: "blaze-cargo",
                file: "src/serve/job.rs",
                why: "names `History` as the plane's predictor type (`Plane<Step<S>, History>`): \
                      admission and order, through the reviewed hook `Predict`",
            },
            NamedIn {
                package: "blaze-cargo",
                file: "src/serve/daemon.rs",
                why: "makes the plane's `History` and reports its image section's faults: the \
                      daemon's state, never a unit's",
            },
            NamedIn {
                package: "blaze-cargo",
                file: "src/serve/image.rs",
                why: "writes and restores the history's section of the daemon image: what the plane \
                      learned, kept across restarts",
            },
        ],
    },
    Cut {
        package: "blaze-ui",
        why: "the client's rendering of what the daemon answered (`cli::live`, `cli::inspect`): it \
              shows records and makes none; the daemon (`serve`) does not call it",
        named_in: &[
            NamedIn {
                package: "blaze-cargo",
                file: "src/cli/print.rs",
                why: "the client's `Printer`: renders a request's answers (unchecked tree, work, links, \
                      timeline) to the terminal after the daemon sent them",
            },
            NamedIn {
                package: "blaze-cargo",
                file: "src/cli/live.rs",
                why: "the client's live test view (`blaze test`): what it draws, never what ran",
            },
            NamedIn {
                package: "blaze-cargo",
                file: "src/cli/inspect.rs",
                why: "`blaze inspect` writes what the daemon answered as JSON or CBOR, on the client",
            },
            NamedIn {
                package: "blaze-cargo",
                file: "src/cli/observe.rs",
                why: "`blaze why` and the hot list, rendered on the client from the daemon's answer",
            },
        ],
    },
];

/// Files outside every package that [`RECORD`]'s closure reads: the pinned nightly (blaze-units'
/// `lock::toolchain`, hir-ty's and the code generator's `NIGHTLY`), the digest and upstream of
/// blaze's own standard library (blaze-cargo's `toolchain::library`), and the pinned nightly's
/// proc-macro bridge, which blaze-macro-eval serves ([`FRONT_END_EXTRA`]).
pub const RECORD_EXTRA: &[(&str, &str)] = &[
    ("blaze-units", "../../harness/toolchain"),
    ("blaze-cargo", "../../library/DIGEST"),
    ("blaze-cargo", "../../library/UPSTREAM"),
    ("blaze-macro-eval", "../../library/proc_macro/src/bridge"),
];

/// [`RECORD`]'s hooks past its closure besides every trait of the closure it names `dyn`: the
/// generic ones blaze-engine and a cut package implement, the program's allocator's
/// (blaze-graph's `arena::Holders`) and the plane's predictor (blaze-graph's
/// `admission::Predict`).
pub const RECORD_HOOKS: &[&str] = &["Holders", "Predict"];

/// Implementations of [`RECORD`]'s hooks past its closure, reviewed: none changes a byte of a
/// record.
pub const RECORD_REVIEWED: &[Hook] = &[
    Hook {
        package: "blaze-engine",
        hook: "Tree",
        why: "`ProbeFiles` gives a `rustc` probe's files by their content, in the process of blaze run \
              as a build script's `rustc`: a build script's run is kept by what it read \
              (`blaze_usercode::run`), and what it printed reaches a unit by value, in its plan and \
              its crate's identity, which the record key hashes",
    },
    Hook {
        package: "blaze-engine",
        hook: "Holders",
        why: "`Heaps` is the program's global allocator: where a value lives, never its bytes (AGENTS.md \
              rule 25: no output depends on addresses)",
    },
    Hook {
        package: "blaze-history",
        hook: "Predict",
        why: "`History` predicts a node's time and peak for the plane's admission and critical path: \
              when work runs, never what it makes (AGENTS.md rule 25)",
    },
];

/// The code identity of what makes a unit's record (blaze-cargo's `serve::unit::compile`): what
/// records and the standard library's shared directory are keyed by, beside [`FRONT_END`] and
/// [`CODEGEN`] (blaze-cargo's `serve::RecordMakers`). Of every package [`RECORD_ROOTS`] are built
/// from but [`RECORD_CUT`], and how blaze is built (`how_built`), so an edit of blaze-engine (the
/// launcher, the pins, the as-`rustc` probes) or of a cut package keeps every record.
///
/// Its closure holds [`FRONT_END`]'s and [`CODEGEN`]'s packages too (blaze-cargo is built from
/// both), so it moves whenever either does; the record key takes all three by name all the same,
/// so `blaze why` says which moved, and a later narrowing of this one keeps them in the key.
pub const RECORD: Identity = Identity {
    what: "the code identity of what makes a unit's record",
    domain: "blaze v2 unit record code 1",
    env: "BLAZE_RECORD_CODE",
    roots: RECORD_ROOTS,
    cut: RECORD_CUT,
    required: "blaze-cargo",
    extra: RECORD_EXTRA,
    how_built: true,
    listing: "record-code-inputs.txt",
    shipped: "shipped-record-code.txt",
    hooks: RECORD_HOOKS,
    reviewed: RECORD_REVIEWED,
};

/// A package an identity's closure is not walked into ([`Identity::cut`]).
#[derive(Debug)]
pub struct Cut {
    pub package: &'static str,
    /// Why no value keyed by the identity depends on its code.
    pub why: &'static str,
    /// Every file of a root that names the cut's crate, compiled into the program (tests
    /// aside), each with why no keyed value depends on what it does with it. A test holds the
    /// roots' files to this list both ways (blaze-codegen-code's `tests::cuts`), and refuses
    /// one that makes a record (blaze-cargo's `src/serve/unit/`) whatever its reason.
    pub named_in: &'static [NamedIn],
}

/// A file of a root that names a cut package's crate ([`Cut::named_in`]).
#[derive(Debug)]
pub struct NamedIn {
    /// The root package the file is of.
    pub package: &'static str,
    /// The file, from the package's directory.
    pub file: &'static str,
    pub why: &'static str,
}

/// A `dyn` hook of the front end or code generator, implemented past the closure by code that
/// can't change the bytes the code generator makes.
#[derive(Debug)]
pub struct Hook {
    pub package: &'static str,
    /// The hook trait, by its name.
    pub hook: &'static str,
    pub why: &'static str,
}

/// The hook traits defined in the closure whose implementations the test checks: each one in the
/// checkout, past its tests, is in the closure, or listed in [`REVIEWED_HOOKS`].
pub const HOOK_TRAITS: &[&str] = &[
    "BodyFacts",
    "GoalFacts",
    "ImageStore",
    "Held",
    "BodyChecks",
    "Lookup",
    "ProcMacroExpander",
    "Frontend",
    "Compile",
    "Prepare",
    "Hashes",
    "SymbolResolver",
    "Machine",
];

/// Implementations of [`HOOK_TRAITS`] past the closure, reviewed: none changes the bytes made.
pub const REVIEWED_HOOKS: &[Hook] = &[
    Hook {
        package: "blaze-diagnostics",
        hook: "DiagnosticHost",
        why: "`Cx` answers what a diagnostic's printer reads of the checked crate (its trimmed and \
              visible paths, edition, a closure's span): text of a message to a person, which the \
              code generator neither prints nor makes a byte from",
    },
    Hook {
        package: "blaze-cargo",
        hook: "ImageStore",
        why: "`Images` gets and puts the dependency images hir-def writes and reads, by key: a \
              store, which makes no bytes",
    },
    Hook {
        package: "blaze-cargo",
        hook: "Held",
        why: "`InStore` holds the bytes of an image read from the store",
    },
    Hook {
        package: "blaze-cargo",
        hook: "BodyChecks",
        why: "`Checking` reports the diagnostics of the bodies the build reaches and stops it on an \
              error; it hands the code generator nothing",
    },
    Hook {
        package: "blaze-cargo",
        hook: "Lookup",
        why: "`Reading` reads templates from the store by the keys the code generator makes",
    },
    Hook {
        package: "blaze-macro-eval",
        hook: "ProcMacroExpander",
        why: "`Evaluated` expands proc macros: the tokens it gives are source of the HIR, which \
              the instance key hashes, so another expansion is another key",
    },
    Hook {
        package: "blaze-wasm-pack",
        hook: "ProcMacroExpander",
        why: "`Recorder` and `Replay` record and replay proc-macro expansions as tokens: source \
              the instance key hashes",
    },
    Hook {
        package: "blaze-wasm",
        hook: "Lookup",
        why: "`Stored` reads templates by key from its own store",
    },
    Hook {
        package: "blaze-macro-eval",
        hook: "Foreign",
        why: "`Host` answers the bridge calls of a proc macro the MIR evaluator runs: what the \
              macro expands to is source of the HIR, which the instance key hashes",
    },
    Hook {
        package: "blaze-engine",
        hook: "Tree",
        why: "`ProbeFiles` gives a `rustc` probe's files by their content: what the code \
              generator compiles of them is keyed by the HIR made of it",
    },
    Hook {
        package: "blaze-usercode",
        hook: "Confine",
        why: "`Launcher` starts a user program (a build script, a test) in blaze's sandbox: the \
              code generator starts none",
    },
    Hook {
        package: "hir-ty-ub",
        hook: "Machine",
        why: "`Checker` runs `blaze test --ub`'s tests: no constant the code generator compiles \
              is evaluated with it",
    },
];

/// The variable the build script sets to the identity's hex.
pub const ENV: &str = "BLAZE_CODEGEN_CODE";
/// Files outside every package that the code generator's packages read: the package that reads
/// it and the path relative to that package's directory.
pub const EXTRA: &[(&str, &str)] = &[(CODE_GENERATOR, "../../harness/toolchain")];
/// The variables the code generator's packages and their build scripts read, each an input by
/// its value: `CI` (stdx's `is_ci`), what llvm-sys's build script links LLVM's C++ runtime by
/// and builds its wrappers with, where cranelift-codegen's build script reads its ISLE (refused
/// when set: [`ISLE_SOURCE_DIR`]), and what libc's and zstd-sys's build scripts say they read
/// (`rerun-if-env-changed`); and, for [`RECORD`]'s closure, what ring's and winapi's build scripts
/// say they read (`RING_PREGENERATE_ASM` makes ring's assembly in place of the packaged one).
pub const READ_ENV: &[&str] = &[
    "CI",
    "CXXSTDLIB",
    "LLVM_SYS_LIBCPP",
    "LLVM_SYS_SYSTEM_LIBRARY_DIRS",
    "LLVM_SYS_231_NO_CLEAN_CFLAGS",
    "LLVM_SYS_231_FFI_WORKAROUND",
    "LLVM_SYS_231_USE_DEBUG_MSVCRT",
    "ISLE_SOURCE_DIR",
    "LIBC_BUILD_VERBOSE",
    "RUST_LIBC_UNSTABLE_FREEBSD_VERSION",
    "ZSTD_SYS_NO_HIDE_SYMBOLS",
    "ZSTD_SYS_USE_PKG_CONFIG",
    "RING_PREGENERATE_ASM",
    "WINAPI_NO_BUNDLED_LIBRARIES",
    "WINAPI_STATIC_NOBUNDLE",
];
/// Where cranelift-codegen's build script reads its ISLE from, when set, in place of its own
/// files: a directory the identity does not hash, so a build with it set is refused.
pub const ISLE_SOURCE_DIR: &str = "ISLE_SOURCE_DIR";
/// What blaze, answering `rustc -vV`, ends with: its own code identity in hex
/// (blaze-cargo's `answers::own`).
pub const BLAZE_IDENTITY: &str = "blaze-identity: ";

/// The one LLVM blaze links (crates/hir-ty-codegen/llvm.json), checked as llvm-sys checks it:
/// pin.rs is its one home, of the pin, the check of a prefix and the message refusing one.
#[allow(
    dead_code,
    reason = "`pinned_llvm_config` is llvm-sys's build script's; this crate uses the check alone"
)]
mod pin {
    include!("../../../vendor/llvm-sys/pin.rs");
}

/// The variable naming the pinned LLVM (vendor/llvm-sys/pin.rs).
pub const LLVM_PREFIX: &str = pin::PIN_PREFIX;
/// The package that links LLVM: LLVM is an input when it is one.
const LLVM_PACKAGE: &str = "llvm-sys";

/// The pin of the stencil libraries and its reader (crates/blaze-stencil/pin.rs), as
/// blaze-stencil's build script reads it: one reader, one pin.
#[allow(
    dead_code,
    reason = "the pin's writer and hex are blaze-stencil's build script's; this crate reads the pin"
)]
mod stencil_pin {
    include!("../../blaze-stencil/pin.rs");
}

/// The package whose stencil libraries are keyed by their pin.
const STENCIL_PACKAGE: &str = "blaze-stencil";
/// The program's feature that builds it as the seed (docs/future/bootstrap.md): no LLVM, its
/// stencils copied and held to their pin. blaze-engine's build script refuses it with any
/// feature that brings LLVM back.
pub const SEED_FEATURE: &str = "CARGO_FEATURE_SEED";

/// Why a variable a closure package's build script reads is no input, or `None` when it must
/// be one: the C compiler, its flags, archiver and SDK, which build the C of the closure's
/// packages (cc's variables, per target and per host), as the module doc says.
pub fn unkeyed_env(name: &str) -> Option<&'static str> {
    const C: &str =
        "the C compiler, its flags and SDK are no input (crates/blaze-codegen-code/src/lib.rs)";
    const TOOLS: [&str; 8] =
        ["CC", "CXX", "AR", "CFLAGS", "CXXFLAGS", "ARFLAGS", "RANLIB", "RANLIBFLAGS"];
    let base = name.strip_prefix("HOST_").or_else(|| name.strip_prefix("TARGET_")).unwrap_or(name);
    // `CC_aarch64-apple-darwin`, `CFLAGS_aarch64_apple_darwin`: a tool's variable for one target.
    let per_target = |tool: &str| {
        base.strip_prefix(tool)
            .and_then(|it| it.strip_prefix('_'))
            .is_some_and(|triple| triple.contains(['-', '_']))
    };
    let c = TOOLS.iter().any(|tool| base == *tool || per_target(tool))
        || matches!(
            base,
            "CRATE_CC_NO_DEFAULTS"
                | "CC_ENABLE_DEBUG_OUTPUT"
                | "CC_FORCE_DISABLE"
                | "CC_SHELL_ESCAPED_FLAGS"
                | "CC_KNOWN_WRAPPER_CUSTOM"
                | "MACOSX_DEPLOYMENT_TARGET"
                | "IPHONEOS_DEPLOYMENT_TARGET"
                | "SDKROOT"
        );
    c.then_some(C)
}

/// One input of the identity.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Input {
    /// What it is: `package`, `dependent`, `manifest`, `file`, `stencils`, `llvm`, `env`,
    /// `compiler`.
    pub kind: &'static str,
    /// Which: a package's `name version source`, a path relative to the workspace, a stencil
    /// library's file, a library's name, a variable's name.
    pub name: String,
    pub digest: [u8; 32],
}

/// What a file was when it was hashed: its length and modification time.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Stamp {
    pub len: u64,
    pub modified: Option<SystemTime>,
}

/// The identity, what it is of, and what a build script reruns on.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Worked {
    pub code: [u8; 32],
    /// In the order hashed: packages, dependents, manifests, files, stencils, LLVM, variables,
    /// the compiler.
    pub inputs: Vec<Input>,
    /// Every file hashed, by its absolute path: what the coverage check holds rustc's reads to.
    pub files: BTreeSet<PathBuf>,
    /// What each file hashed or parsed, and each package directory listed, was just before it
    /// was read ([`Worked::changed`]).
    pub stamps: BTreeMap<PathBuf, Stamp>,
    /// The directories of the packages of the checkout that are inputs.
    pub dirs: Vec<PathBuf>,
    /// What a build script reruns on besides the lock ([`emit_of`]): exactly what is hashed. Of
    /// a package of the checkout, each directory all of whose files, at any depth, are hashed,
    /// whole, and each other file hashed by itself, so an edit of a skipped `tests/`, `benches/`,
    /// `examples/` or dotfile reruns nothing; each directory [`Identity::extra`] names, whole; and
    /// every other file hashed outside the packages.
    pub watched: Vec<PathBuf>,
    /// The registry packages that are inputs (by their checksum), by name and version.
    pub registry: Vec<(String, String)>,
    /// Every package that is an input, by name.
    pub packages: BTreeSet<String>,
    /// The names of the libraries of every package that is an input, as rustc names a crate.
    pub crates: BTreeSet<String>,
    /// The variables whose value is an input and that cargo does not watch itself.
    pub rerun_env: Vec<String>,
    /// The pinned LLVM's prefix, checked to be the pin: everything under it is the pin's.
    pub llvm_prefix: Option<PathBuf>,
}

impl Worked {
    /// The inputs, one per line: kind, name and digest, tab-separated.
    pub fn listing(&self) -> String {
        let mut out = String::new();
        for input in &self.inputs {
            out.push_str(input.kind);
            out.push('\t');
            out.push_str(&input.name);
            out.push('\t');
            out.push_str(&hex(&input.digest));
            out.push('\n');
        }
        out
    }

    /// The files hashed or parsed and the directories listed that are not now what they were
    /// when read (or are gone): saved while the identity was worked out, so it is of no one tree.
    pub fn changed(&self) -> Vec<PathBuf> {
        changed(&self.stamps)
    }
}

/// The paths of `stamps` that are not now what they were (or are gone).
fn changed(stamps: &BTreeMap<PathBuf, Stamp>) -> Vec<PathBuf> {
    stamps
        .iter()
        .filter(|(path, was)| stamp(path).ok() != Some(**was))
        .map(|(path, _)| path.clone())
        .collect()
}

/// The hex of `bytes`, lowercase.
pub fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// What a build script does: works the code generator's identity out ([`CODEGEN`], [`emit_of`]).
pub fn emit() {
    emit_of(&CODEGEN);
}

/// What a build script does: works `identity` out for the package being built
/// (`CARGO_MANIFEST_DIR`, the program) in its workspace, prints
/// `cargo:rustc-env=<identity.env>=<hex>` and what to rerun on, and writes the listing of its
/// inputs to `OUT_DIR/<identity.listing>`. An input that can't be had, or a file that changed
/// while it was hashed, is one `cargo::error` naming it, and the build stops.
pub fn emit_of(identity: &Identity) {
    emit_all(&[identity]);
}

/// [`emit_of`] for each of `identities`, over one walk of the checkout ([`Walk`]): a package of
/// several closures is listed and read once, the compiler asked once, and every file read checked
/// once, after the last identity, to be what it was when read.
pub fn emit_all(identities: &[&Identity]) {
    if let Err(why) = emitted(identities) {
        let named: Vec<String> =
            identities.iter().map(|it| format!("{} ({})", it.what, it.env)).collect();
        println!("cargo::error={}: {}", named.join(" and "), why.replace('\n', " "));
    }
}

fn emitted(identities: &[&Identity]) -> Result<(), String> {
    let manifest_dir = var("CARGO_MANIFEST_DIR")?;
    let build = if identities.iter().any(|it| it.how_built) {
        let env: BTreeMap<String, String> = std::env::vars().collect();
        let mut build = BuildEnv::of(&env);
        // asks blaze: blaze sets a build script's `RUSTC` to its stand-in for rustc, so the
        // compiler asked is blaze, which answers `-vV` itself (docs/no-rustc.md, end state 3).
        build.compiler = Some(compiler(&var("RUSTC")?)?);
        build
    } else {
        BuildEnv::default()
    };
    let (all, lock) = work_out_here(Path::new(&manifest_dir), identities, &build)?;
    let mut stamps = BTreeMap::new();
    for worked in &all {
        for (path, was) in &worked.stamps {
            stamps.entry(path.clone()).or_insert(*was);
        }
    }
    let changed = changed(&stamps);
    if !changed.is_empty() {
        let named: Vec<String> =
            changed.iter().take(8).map(|it| it.display().to_string()).collect();
        return Err(format!(
            "{} file(s) changed while it was worked out, so it is of no one tree: {}; build again once \
             nothing writes to the tree",
            changed.len(),
            named.join(", ")
        ));
    }
    let watched: BTreeSet<&PathBuf> = all.iter().flat_map(|it| &it.watched).collect();
    for path in watched {
        println!("cargo:rerun-if-changed={path}", path = path.display());
    }
    // The registry packages' checksums and the closure are read from the lock. cargo watches a
    // file whole, so any change to the lock reruns this; a registry package's new version
    // changes nothing else this watches. Only the program has this script, so that rerun
    // recompiles the program alone. A shipped tree has no lock: it is what the lock said.
    if let Some(lock) = lock {
        println!("cargo:rerun-if-changed={}", lock.display());
    }
    let rerun_env: BTreeSet<&String> = all.iter().flat_map(|it| &it.rerun_env).collect();
    for name in rerun_env {
        println!("cargo:rerun-if-env-changed={name}");
    }
    for (identity, worked) in identities.iter().zip(&all) {
        let out = Path::new(&var("OUT_DIR")?).join(identity.listing);
        std::fs::write(&out, worked.listing())
            .map_err(|why| format!("writing {}: {why}", out.display()))?;
        println!("cargo:rustc-env={}={}", identity.env, hex(&worked.code));
    }
    Ok(())
}

/// Each of `identities` of the program at `program_dir`, built in `env`, and the lock to rerun on
/// when the checkout was read: an identity whose tree the program's package carries
/// ([`Identity::shipped`]) from it, which is a package cut from a checkout, whose surroundings
/// (a staging workspace, a Nix source, a vendor directory) are no checkout of its own; else from
/// its checkout, the workspace at or above it, in one walk ([`Walk`]). With neither it is refused,
/// naming both: there is nothing to work it out from, and no identity stands in for one
/// (AGENTS.md rule 3).
pub fn work_out_here(
    program_dir: &Path,
    identities: &[&Identity],
    env: &BuildEnv,
) -> Result<(Vec<Worked>, Option<PathBuf>), String> {
    let mut checkout: Option<(Workspace, Walk)> = None;
    let mut all = Vec::new();
    for identity in identities {
        let shipped = program_dir.join(identity.shipped);
        let worked = if shipped.is_file() {
            work_out_shipped(&shipped, identity, env)
        } else {
            if checkout.is_none() {
                if checkout_of(program_dir)?.is_none() {
                    return Err(format!(
                        "{} has no checkout (no Cargo.toml at or above it has a `[workspace]` table) and \
                         no shipped tree of {} ({}, which the `ship-code-identity` tool of \
                         crates/blaze-codegen-code writes from the checkout, as \
                         .github/scripts/publish_crates.py does into the package it publishes)",
                        program_dir.display(),
                        identity.what,
                        shipped.display()
                    ));
                }
                checkout = Some((Workspace::read(program_dir)?, Walk::default()));
            }
            let (workspace, walk) = checkout.as_mut().ok_or("the checkout was read just above")?;
            walk.work_out(workspace, identity, env)
        };
        all.push(worked.map_err(|why| format!("{}: {why}", identity.what))?);
    }
    Ok((all, checkout.map(|(workspace, _)| workspace.lock_path)))
}

/// The manifest of the workspace `dir` is in, if any: its checkout.
pub fn checkout_of(dir: &Path) -> Result<Option<PathBuf>, String> {
    workspace_of(&canonical(dir)?, &mut BTreeMap::new(), &mut BTreeMap::new())
}

fn var(name: &str) -> Result<String, String> {
    std::env::var(name).map_err(|why| format!("${name}, which cargo gives a build script: {why}"))
}

/// What `rustc -vV` says of the compiler that builds the program: its version, commit and host,
/// and for blaze as `rustc`, which blaze it is.
fn compiler(rustc: &str) -> Result<String, String> {
    // asks blaze: `rustc` is the build script's `$RUSTC`, blaze's stand-in, so blaze answers.
    let out = Command::new(rustc)
        .arg("-vV")
        .output()
        .map_err(|why| format!("running `{rustc} -vV`: {why}"))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        return Err(format!("`{rustc} -vV` exited with {}: {}", out.status, stderr.trim()));
    }
    let said =
        String::from_utf8(out.stdout).map_err(|_| format!("`{rustc} -vV` printed no UTF-8"))?;
    check_compiler(rustc, said)
}

/// `said`, what `{rustc} -vV` printed, when it is a compiler's version: a rustc's, or blaze's
/// with one [`BLAZE_IDENTITY`] line of 64 hex digits. A program that is no rustc (blaze itself,
/// not under the name `rustc`, prints its usage and succeeds) would key the identity by text
/// that names no compiler. A blaze from before the line answers as the nightly does and can't
/// be told from it; no blaze built blaze before it.
pub fn check_compiler(rustc: &str, said: String) -> Result<String, String> {
    if !said.starts_with("rustc ") || !said.lines().any(|it| it.starts_with("release: ")) {
        let first = said.lines().next().unwrap_or("");
        return Err(format!("`{rustc} -vV` printed `{first}`, which is no rustc's version"));
    }
    let named: Vec<&str> = said.lines().filter_map(|it| it.strip_prefix(BLAZE_IDENTITY)).collect();
    match named[..] {
        [] => {}
        [hex] if hex.len() == 64 && hex.bytes().all(|it| it.is_ascii_hexdigit()) => {}
        _ => {
            return Err(format!(
                "`{rustc} -vV` names which blaze it is in {} `{BLAZE_IDENTITY}` line(s) ({}), not one of 64 hex digits",
                named.len(),
                named.join(", ")
            ));
        }
    }
    Ok(said)
}

/// What of the build script's environment is an input.
#[derive(Clone, Debug, Default)]
pub struct BuildEnv {
    /// Every variable of the build that is an input, by name.
    pub vars: BTreeMap<String, Option<String>>,
    /// The pinned LLVM's prefix, if set.
    pub llvm_prefix: Option<String>,
    /// What `$RUSTC -vV` printed; `None` where no compiler is asked (the coverage test).
    pub compiler: Option<String>,
    /// Where cargo's own configuration is: `$CARGO_HOME`, else `$HOME/.cargo`.
    pub cargo_home: Option<PathBuf>,
}

impl BuildEnv {
    /// Of a build script's environment `env`.
    pub fn of(env: &BTreeMap<String, String>) -> BuildEnv {
        let mut vars = BTreeMap::new();
        for (name, value) in env {
            let keyed =
                matches!(name.as_str(), "TARGET" | "HOST" | "PROFILE" | "OPT_LEVEL" | "DEBUG")
                    || name == "CARGO_ENCODED_RUSTFLAGS"
                    || name.starts_with("CARGO_CFG_")
                    || name.starts_with("CARGO_PROFILE_")
                    || name.starts_with("CARGO_FEATURE_");
            if keyed {
                vars.insert(name.clone(), Some(value.clone()));
            }
        }
        for name in READ_ENV {
            vars.insert((*name).to_owned(), env.get(*name).cloned());
        }
        let llvm_prefix = env.get(LLVM_PREFIX).filter(|it| !it.is_empty()).cloned();
        let cargo_home = match env.get("CARGO_HOME").filter(|it| !it.is_empty()) {
            Some(home) => Some(PathBuf::from(home)),
            None => {
                env.get("HOME").filter(|it| !it.is_empty()).map(|it| Path::new(it).join(".cargo"))
            }
        };
        BuildEnv { vars, llvm_prefix, compiler: None, cargo_home }
    }

    /// Whether the program is built as the seed ([`SEED_FEATURE`]).
    pub fn seed(&self) -> bool {
        self.vars.contains_key(SEED_FEATURE)
    }
}

/// A package of the checkout: its manifest and where it is.
#[derive(Debug)]
struct Local {
    name: String,
    version: String,
    dir: PathBuf,
    manifest: toml::Table,
    /// The manifest of the workspace it inherits from (its own, or the nearest above it).
    workspace: Option<PathBuf>,
}

/// One `[[package]]` of `Cargo.lock`.
#[derive(Debug)]
struct Locked {
    name: String,
    version: String,
    source: Option<String>,
    checksum: Option<String>,
    dependencies: Vec<String>,
}

/// A workspace as its lock file and its packages' manifests say, read without cargo.
#[derive(Debug)]
pub struct Workspace {
    /// Its root directory, canonical.
    pub root: PathBuf,
    pub lock_path: PathBuf,
    /// The package being built, by name.
    pub program: String,
    lock: Vec<Locked>,
    locals: Vec<Local>,
    /// The manifests of its members.
    members: BTreeSet<PathBuf>,
    /// Workspace manifests by path, as read.
    workspaces: BTreeMap<PathBuf, toml::Table>,
    /// What the lock and every manifest read were just before they were read.
    stamps: BTreeMap<PathBuf, Stamp>,
}

fn read(path: &Path) -> Result<Vec<u8>, String> {
    std::fs::read(path).map_err(|why| format!("reading {}: {why}", path.display()))
}

fn read_toml(path: &Path, stamps: &mut BTreeMap<PathBuf, Stamp>) -> Result<toml::Table, String> {
    let bytes = read_stamped(path, stamps)?;
    let text = String::from_utf8(bytes).map_err(|_| format!("{} is not UTF-8", path.display()))?;
    text.parse::<toml::Table>()
        .map_err(|why| format!("{} is no TOML: {}", path.display(), why.to_string().trim()))
}

fn canonical(path: &Path) -> Result<PathBuf, String> {
    std::fs::canonicalize(path).map_err(|why| format!("{}: {why}", path.display()))
}

/// The tables of `manifest` that list dependencies, with whether they are built into the
/// package (normal and build dependencies) or only into its tests (dev-dependencies), every
/// platform's.
fn dependency_tables(manifest: &toml::Table) -> Vec<(bool, &toml::Table)> {
    const KINDS: [(&str, bool); 5] = [
        ("dependencies", true),
        ("build-dependencies", true),
        ("build_dependencies", true),
        ("dev-dependencies", false),
        ("dev_dependencies", false),
    ];
    let targets = manifest
        .get("target")
        .and_then(toml::Value::as_table)
        .into_iter()
        .flat_map(|it| it.values())
        .filter_map(toml::Value::as_table);
    let mut out = Vec::new();
    for table in std::iter::once(manifest).chain(targets) {
        for (kind, built) in KINDS {
            if let Some(deps) = table.get(kind).and_then(toml::Value::as_table) {
                out.push((built, deps));
            }
        }
    }
    out
}

/// The workspace's `[workspace.dependencies]` entry `key` names, if any.
fn workspace_dependency<'a>(
    workspace: Option<&'a toml::Table>,
    key: &str,
) -> Option<&'a toml::Value> {
    workspace?.get("workspace")?.get("dependencies")?.get(key)
}

/// Whether a dependency entry inherits from the workspace (`key.workspace = true`).
fn inherits(value: &toml::Value) -> bool {
    value.get("workspace").and_then(toml::Value::as_bool) == Some(true)
}

/// The package a dependency entry `key = value` names: its `package`, the workspace entry's
/// `package` when it inherits, else `key`.
fn dependency_name(key: &str, value: &toml::Value, workspace: Option<&toml::Table>) -> String {
    if let Some(package) = value.get("package").and_then(toml::Value::as_str) {
        return package.to_owned();
    }
    if inherits(value)
        && let Some(package) = workspace_dependency(workspace, key)
            .and_then(|it| it.get("package"))
            .and_then(toml::Value::as_str)
    {
        return package.to_owned();
    }
    key.to_owned()
}

/// The directory a dependency entry's `path` names, relative to the manifest at `dir` (or the
/// workspace's directory when it inherits), if it names one.
fn dependency_path(
    dir: &Path,
    key: &str,
    value: &toml::Value,
    workspace: Option<(&Path, &toml::Table)>,
) -> Option<PathBuf> {
    if let Some(path) = value.get("path").and_then(toml::Value::as_str) {
        return Some(dir.join(path));
    }
    if inherits(value) {
        let (at, table) = workspace?;
        let path = workspace_dependency(Some(table), key)?.get("path")?.as_str()?;
        return Some(at.join(path));
    }
    None
}

impl Workspace {
    /// The workspace of the package at `program_dir`: the nearest manifest at or above it with
    /// a `[workspace]` table, its `Cargo.lock`, its members and every package of the checkout a
    /// manifest names by `path`. Nothing is run and nothing is written.
    pub fn read(program_dir: &Path) -> Result<Workspace, String> {
        let program_dir = canonical(program_dir)?;
        let mut stamps = BTreeMap::new();
        let program_manifest = read_toml(&program_dir.join("Cargo.toml"), &mut stamps)?;
        let program = program_manifest
            .get("package")
            .and_then(|it| it.get("name"))
            .and_then(toml::Value::as_str)
            .ok_or_else(|| {
                format!("{} names no package", program_dir.join("Cargo.toml").display())
            })?
            .to_owned();
        let mut workspaces = BTreeMap::new();
        let root_manifest =
            workspace_of(&program_dir, &mut workspaces, &mut stamps)?.ok_or_else(|| {
                format!(
                    "no manifest at or above {} has a `[workspace]` table",
                    program_dir.display()
                )
            })?;
        let root =
            root_manifest.parent().map(Path::to_path_buf).ok_or("a manifest has no directory")?;
        let lock_path = root.join("Cargo.lock");
        let lock = locked(&lock_path, &mut stamps)?;

        // The members: `members` globs, less `exclude`.
        let table = workspaces[&root_manifest].clone();
        let section =
            table.get("workspace").and_then(toml::Value::as_table).cloned().unwrap_or_default();
        let excluded: Vec<PathBuf> = section
            .get("exclude")
            .and_then(toml::Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(toml::Value::as_str)
            .map(|it| root.join(it))
            .collect();
        let mut members = BTreeSet::new();
        let mut queue: Vec<PathBuf> = vec![program_dir.clone()];
        if table.contains_key("package") {
            queue.push(root.clone());
            members.insert(root_manifest.clone());
        }
        for pattern in section
            .get("members")
            .and_then(toml::Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(toml::Value::as_str)
        {
            let full = root.join(pattern);
            let full = full.to_str().ok_or_else(|| format!("{} is not UTF-8", full.display()))?;
            let paths = glob::glob(full).map_err(|why| {
                format!("member pattern `{pattern}` of {}: {why}", root_manifest.display())
            })?;
            for path in paths {
                let path = path.map_err(|why| format!("member pattern `{pattern}`: {why}"))?;
                if !path.join("Cargo.toml").is_file()
                    || excluded.iter().any(|it| path.starts_with(it))
                {
                    continue;
                }
                let path = canonical(&path)?;
                members.insert(path.join("Cargo.toml"));
                queue.push(path);
            }
        }
        // `path` entries of the root's workspace dependencies, patches and replacements.
        let mut tables: Vec<&toml::Table> = Vec::new();
        if let Some(deps) = section.get("dependencies").and_then(toml::Value::as_table) {
            tables.push(deps);
        }
        for patches in table
            .get("patch")
            .and_then(toml::Value::as_table)
            .into_iter()
            .flat_map(|it| it.values())
        {
            if let Some(patches) = patches.as_table() {
                tables.push(patches);
            }
        }
        if let Some(replace) = table.get("replace").and_then(toml::Value::as_table) {
            tables.push(replace);
        }
        for deps in tables {
            for value in deps.values() {
                if let Some(path) = value.get("path").and_then(toml::Value::as_str) {
                    queue.push(root.join(path));
                }
            }
        }

        let mut locals: Vec<Local> = Vec::new();
        let mut seen = BTreeSet::new();
        while let Some(dir) = queue.pop() {
            let dir = canonical(&dir)?;
            if !seen.insert(dir.clone()) {
                continue;
            }
            let manifest_path = dir.join("Cargo.toml");
            let manifest = read_toml(&manifest_path, &mut stamps)?;
            let workspace = workspace_of(&dir, &mut workspaces, &mut stamps)?;
            let inherited = workspace
                .as_ref()
                .map(|path| (path.parent().unwrap_or(Path::new("")), &workspaces[path]));
            for (_, deps) in dependency_tables(&manifest) {
                for (key, value) in deps {
                    if let Some(path) = dependency_path(&dir, key, value, inherited) {
                        queue.push(path);
                    }
                }
            }
            let Some(package) = manifest.get("package").and_then(toml::Value::as_table) else {
                continue;
            };
            let name = package
                .get("name")
                .and_then(toml::Value::as_str)
                .ok_or_else(|| format!("{} names no package", manifest_path.display()))?
                .to_owned();
            let version = match package.get("version") {
                None => "0.0.0".to_owned(),
                Some(version) if inherits(version) => inherited
                    .and_then(|(_, table)| {
                        table.get("workspace")?.get("package")?.get("version")?.as_str()
                    })
                    .ok_or_else(|| {
                        format!(
                            "{} inherits its version from a workspace that sets none",
                            manifest_path.display()
                        )
                    })?
                    .to_owned(),
                Some(version) => version
                    .as_str()
                    .ok_or_else(|| {
                        format!("the version in {} is no string", manifest_path.display())
                    })?
                    .to_owned(),
            };
            locals.push(Local { name, version, dir, manifest, workspace });
        }
        locals.sort_by(|a, b| a.dir.cmp(&b.dir));
        Ok(Workspace { root, lock_path, program, lock, locals, members, workspaces, stamps })
    }

    /// The package of the checkout the lock's path package `locked` is.
    fn local(&self, locked: &Locked) -> Result<&Local, String> {
        let found: Vec<&Local> = self
            .locals
            .iter()
            .filter(|it| it.name == locked.name && it.version == locked.version)
            .collect();
        match found[..] {
            [one] => Ok(one),
            [] => Err(format!(
                "{} {} is a package of the checkout in {}, and no manifest a member or `path` names is it",
                locked.name,
                locked.version,
                self.lock_path.display()
            )),
            _ => Err(format!(
                "{} {} is the package of {} manifests: {}",
                locked.name,
                locked.version,
                found.len(),
                found.iter().map(|it| it.dir.display().to_string()).collect::<Vec<_>>().join(", ")
            )),
        }
    }

    /// The lock's package a `dependencies` entry of the lock names: `name`, `name version` or
    /// `name version (source)`.
    fn resolve(&self, of: &Locked, entry: &str) -> Result<usize, String> {
        let mut parts = entry.split_whitespace();
        let name = parts.next().unwrap_or("");
        let version = parts.next();
        let source =
            parts.next().and_then(|it| it.strip_prefix('(')).and_then(|it| it.strip_suffix(')'));
        let found: Vec<usize> = (0..self.lock.len())
            .filter(|at| {
                let it = &self.lock[*at];
                it.name == name
                    && version.is_none_or(|version| it.version == version)
                    && source.is_none_or(|source| it.source.as_deref() == Some(source))
            })
            .collect();
        match found[..] {
            [one] => Ok(one),
            _ => Err(format!(
                "`{entry}`, a dependency of {} {} in {}, names {} packages of the lock, not one",
                of.name,
                of.version,
                self.lock_path.display(),
                found.len()
            )),
        }
    }

    /// The packages `at` is built from by the lock: every dependency the lock lists, but those a
    /// package of the checkout names only as a dev-dependency.
    fn built_from(&self, at: usize) -> Result<Vec<usize>, String> {
        let locked = &self.lock[at];
        let built: Option<BTreeSet<String>> = match locked.source {
            Some(_) => None,
            None => {
                let local = self.local(locked)?;
                let workspace = local.workspace.as_ref().map(|it| &self.workspaces[it]);
                let mut names = BTreeSet::new();
                for (built, deps) in dependency_tables(&local.manifest) {
                    if built {
                        names.extend(
                            deps.iter().map(|(key, value)| dependency_name(key, value, workspace)),
                        );
                    }
                }
                Some(names)
            }
        };
        let mut out = Vec::new();
        for entry in &locked.dependencies {
            let dep = self.resolve(locked, entry)?;
            if built.as_ref().is_none_or(|names| names.contains(&self.lock[dep].name)) {
                out.push(dep);
            }
        }
        Ok(out)
    }

    /// Every package reachable from `from` by [`Workspace::built_from`], `from` too.
    fn reach(&self, from: &[usize]) -> Result<BTreeSet<usize>, String> {
        self.reach_past(from, &BTreeSet::new())
    }

    /// [`Workspace::reach`], never walking into a package of `cut`.
    fn reach_past(&self, from: &[usize], cut: &BTreeSet<usize>) -> Result<BTreeSet<usize>, String> {
        let mut seen = BTreeSet::new();
        let mut stack = from.to_vec();
        while let Some(at) = stack.pop() {
            if !cut.contains(&at) && seen.insert(at) {
                stack.extend(self.built_from(at)?);
            }
        }
        Ok(seen)
    }

    /// The packages of the code generator's closure, by name.
    pub fn closure_names(&self) -> Result<BTreeSet<String>, String> {
        self.closure_names_of(&CODEGEN)
    }

    /// The packages of `identity`'s closure, by name.
    pub fn closure_names_of(&self, identity: &Identity) -> Result<BTreeSet<String>, String> {
        Ok(self.closure(identity)?.into_iter().map(|at| self.lock[at].name.clone()).collect())
    }

    /// The packages the program is built from, itself among them, by name.
    pub fn linked_names(&self) -> Result<BTreeSet<String>, String> {
        Ok(self
            .reach(&[self.program_at()?])?
            .into_iter()
            .map(|at| self.lock[at].name.clone())
            .collect())
    }

    /// The packages the package of the checkout `name` is built from, itself among them, by name.
    pub fn built_from_names(&self, name: &str) -> Result<BTreeSet<String>, String> {
        let found: Vec<usize> = (0..self.lock.len())
            .filter(|at| self.lock[*at].source.is_none() && self.lock[*at].name == name)
            .collect();
        match found[..] {
            [one] => {
                Ok(self.reach(&[one])?.into_iter().map(|at| self.lock[at].name.clone()).collect())
            }
            _ => Err(format!(
                "{} packages of the checkout in {} are named `{name}`, not one",
                found.len(),
                self.lock_path.display()
            )),
        }
    }

    /// The program's package in the lock.
    fn program_at(&self) -> Result<usize, String> {
        (0..self.lock.len())
            .find(|at| self.lock[*at].source.is_none() && self.lock[*at].name == self.program)
            .ok_or_else(|| {
                format!(
                    "the program `{}` is no package of {}",
                    self.program,
                    self.lock_path.display()
                )
            })
    }

    /// The packages of the checkout, by name, with their directories.
    pub fn checkout(&self) -> Vec<(&str, &Path)> {
        self.locals.iter().map(|it| (it.name.as_str(), it.dir.as_path())).collect()
    }

    /// Every package of the lock outside `closure` that depends on one of it, at any depth, by
    /// any dependency the lock lists (normal, build, dev, optional, any platform's): what can
    /// turn on a feature of a package of `closure`.
    fn dependents(&self, closure: &BTreeSet<usize>) -> Result<BTreeSet<usize>, String> {
        let mut depended_on_by: Vec<Vec<usize>> = vec![Vec::new(); self.lock.len()];
        for (at, locked) in self.lock.iter().enumerate() {
            for entry in &locked.dependencies {
                depended_on_by[self.resolve(locked, entry)?].push(at);
            }
        }
        let mut seen = closure.clone();
        let mut stack: Vec<usize> = closure.iter().copied().collect();
        while let Some(at) = stack.pop() {
            for &by in &depended_on_by[at] {
                if seen.insert(by) {
                    stack.push(by);
                }
            }
        }
        Ok(seen.difference(closure).copied().collect())
    }

    /// Every package of the lock the roots of `identity` the program links are built from, never
    /// walking into one it cuts.
    fn closure(&self, identity: &Identity) -> Result<BTreeSet<usize>, String> {
        let linked = self.reach(&[self.program_at()?])?;
        // `blaze-hir-ty-codegen` is the name .github/scripts/publish_crates.py publishes it under.
        let named = |at: usize, package: &str| {
            let name = self.lock[at].name.as_str();
            name == package || name.strip_prefix("blaze-") == Some(package)
        };
        // Two names of one package: `ui` and `blaze-ui`, as `named` reads a lock's.
        let same = |a: &str, b: &str| {
            a == b || a.strip_prefix("blaze-") == Some(b) || b.strip_prefix("blaze-") == Some(a)
        };
        let mut cut = BTreeSet::new();
        for it in identity.cut {
            if identity.roots.iter().any(|root| same(root.package, it.package)) {
                return Err(format!("{} both roots and cuts `{}`", identity.what, it.package));
            }
            let found: Vec<usize> =
                linked.iter().copied().filter(|at| named(*at, it.package)).collect();
            if found.is_empty() {
                return Err(format!(
                    "{} cuts `{}` from its closure, and the program `{}` does not link it: drop the cut",
                    identity.what, it.package, self.program
                ));
            }
            cut.extend(found);
        }
        let mut roots = Vec::new();
        for root in identity.roots {
            let found: Vec<usize> =
                linked.iter().copied().filter(|at| named(*at, root.package)).collect();
            match found[..] {
                [] if root.package == identity.required => {
                    return Err(format!(
                        "the program `{}` does not link `{}`, the root of {}",
                        self.program, identity.required, identity.what
                    ));
                }
                [] => {}
                [one] => roots.push(one),
                _ => {
                    return Err(format!(
                        "the program `{}` links {} packages named `{}`",
                        self.program,
                        found.len(),
                        root.package
                    ));
                }
            }
        }
        let closure = self.reach_past(&roots, &cut)?;
        // A cut reached past the roots would be code the closure runs that no one reviewed: only a
        // root may be built from a cut, and the roots' files that name it are held to
        // `Cut::named_in` by a test.
        let mut reached: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
        for &at in closure.iter().filter(|at| !roots.contains(at)) {
            for dep in self.built_from(at)? {
                if cut.contains(&dep) {
                    reached
                        .entry(self.lock[dep].name.as_str())
                        .or_default()
                        .insert(self.lock[at].name.as_str());
                }
            }
        }
        if let Some((cut, by)) = reached.into_iter().next() {
            let by: Vec<&str> = by.into_iter().collect();
            return Err(format!(
                "{} cuts `{cut}`, and `{}`, of its closure and no root, is built from it: what a cut \
                 does could reach a keyed value unreviewed; drop the cut, or make the package a root",
                identity.what,
                by.join("`, `")
            ));
        }
        Ok(closure)
    }
}

/// The manifest of the workspace `dir` is in: the nearest at or above it with a `[workspace]`
/// table, read into `cache`.
fn workspace_of(
    dir: &Path,
    cache: &mut BTreeMap<PathBuf, toml::Table>,
    stamps: &mut BTreeMap<PathBuf, Stamp>,
) -> Result<Option<PathBuf>, String> {
    for ancestor in dir.ancestors() {
        let manifest = ancestor.join("Cargo.toml");
        if cache.get(&manifest).is_some_and(|it| it.contains_key("workspace")) {
            return Ok(Some(manifest));
        }
        if !manifest.is_file() {
            continue;
        }
        let table = read_toml(&manifest, stamps)?;
        let is = table.contains_key("workspace");
        cache.insert(manifest.clone(), table);
        if is {
            return Ok(Some(manifest));
        }
    }
    Ok(None)
}

/// The packages of `Cargo.lock`.
fn locked(path: &Path, stamps: &mut BTreeMap<PathBuf, Stamp>) -> Result<Vec<Locked>, String> {
    let table = read_toml(path, stamps)?;
    let mut out = Vec::new();
    for package in table
        .get("package")
        .and_then(toml::Value::as_array)
        .ok_or_else(|| format!("{} lists no `[[package]]`", path.display()))?
    {
        let field = |name: &str| package.get(name).and_then(toml::Value::as_str).map(str::to_owned);
        let name =
            field("name").ok_or_else(|| format!("a package of {} has no name", path.display()))?;
        let version = field("version")
            .ok_or_else(|| format!("{name} in {} has no version", path.display()))?;
        let dependencies = package
            .get("dependencies")
            .and_then(toml::Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(toml::Value::as_str)
            .map(str::to_owned)
            .collect();
        out.push(Locked {
            name,
            version,
            source: field("source"),
            checksum: field("checksum"),
            dependencies,
        });
    }
    Ok(out)
}

fn stamp(path: &Path) -> Result<Stamp, String> {
    let meta = std::fs::metadata(path).map_err(|why| format!("{}: {why}", path.display()))?;
    Ok(Stamp { len: meta.len(), modified: meta.modified().ok() })
}

/// Reads `path`, noting what it was just before ([`Worked::changed`]). A file read twice (a
/// manifest, parsed for the closure and later hashed) keeps its first stamp, so a save between
/// the two reads is seen.
fn read_stamped(path: &Path, stamps: &mut BTreeMap<PathBuf, Stamp>) -> Result<Vec<u8>, String> {
    let was = stamp(path)?;
    let bytes = read(path)?;
    stamps.entry(path.to_path_buf()).or_insert(was);
    Ok(bytes)
}

/// The identity of the code generator `workspace`'s program runs, built in `env` ([`CODEGEN`]).
pub fn work_out(workspace: &Workspace, env: &BuildEnv) -> Result<Worked, String> {
    work_out_of(workspace, &CODEGEN, env)
}

/// `identity` of `workspace`'s program, built in `env` (which is read only when
/// `identity.how_built`).
pub fn work_out_of(
    workspace: &Workspace,
    identity: &Identity,
    env: &BuildEnv,
) -> Result<Worked, String> {
    Walk::default().work_out(workspace, identity, env)
}

/// What a walk of the checkout hashed of its packages, kept for every identity worked out over
/// the same tree ([`emit_all`]): each package of the checkout is listed and read once, whichever
/// closures it is in, so a second identity reads only the packages the first had not.
#[derive(Default)]
pub struct Walk {
    /// Each package of the checkout hashed, by its directory.
    hashed: BTreeMap<PathBuf, Hashed>,
}

/// A package of the checkout, hashed: its digest, its files, what a build script watches for
/// them, and what each file and directory was just before it was read.
struct Hashed {
    digest: [u8; 32],
    files: Vec<PathBuf>,
    watch: Vec<PathBuf>,
    stamps: BTreeMap<PathBuf, Stamp>,
}

impl Walk {
    /// The package of the checkout `local`, hashed by every file of it ([`package_files`]),
    /// once.
    fn package(&mut self, local: &Local) -> Result<&Hashed, String> {
        if !self.hashed.contains_key(&local.dir) {
            let mut stamps = BTreeMap::new();
            let mut files = Vec::new();
            let mut hasher = hasher("package checkout");
            let listed = package_files(local, &mut stamps)?;
            for (relative, path) in listed.files {
                let bytes = read_stamped(&path, &mut stamps)?;
                hasher.update(&(relative.len() as u64).to_le_bytes()).update(relative.as_bytes());
                hasher.update(&(bytes.len() as u64).to_le_bytes()).update(&bytes);
                files.push(path);
            }
            let hashed = Hashed {
                digest: *hasher.finalize().as_bytes(),
                files,
                watch: listed.watch,
                stamps,
            };
            self.hashed.insert(local.dir.clone(), hashed);
        }
        Ok(&self.hashed[&local.dir])
    }

    /// [`work_out_of`], reading each package of the checkout this walk has not yet.
    pub fn work_out(
        &mut self,
        workspace: &Workspace,
        identity: &Identity,
        env: &BuildEnv,
    ) -> Result<Worked, String> {
        let (tree, seen) = self.tree(workspace, identity)?;
        finish(identity, &tree, env, seen)
    }

    /// The tree of `identity` of `workspace`'s program ([`Tree`]), and what working it out saw.
    fn tree(&mut self, workspace: &Workspace, identity: &Identity) -> Result<(Tree, Seen), String> {
        let root = workspace.root.as_path();
        let closure = workspace.closure(identity)?;

        let mut inputs = Vec::new();
        let mut seen = Seen { stamps: workspace.stamps.clone(), ..Seen::default() };
        let mut inherited = BTreeSet::new();
        let mut links_llvm = false;

        // 1. The packages, by name, version and source.
        let mut chosen: Vec<&Locked> = closure.iter().map(|at| &workspace.lock[*at]).collect();
        chosen.sort_by(|a, b| {
            (&a.name, &a.version, &a.source).cmp(&(&b.name, &b.version, &b.source))
        });
        for package in chosen {
            links_llvm |= package.name == LLVM_PACKAGE;
            seen.packages.insert(package.name.clone());
            let name = locked_name(package);
            let digest = match package.source.as_deref() {
                Some(_) => {
                    let digest = registry_digest(workspace, package)?;
                    seen.registry.push((package.name.clone(), package.version.clone()));
                    seen.crates.insert(package.name.replace('-', "_"));
                    digest
                }
                None => {
                    let local = workspace.local(package)?;
                    seen.crates.insert(lib_name(local));
                    let hashed = self.package(local)?;
                    // A file read before (a manifest the closure was read from) keeps its first stamp.
                    for (path, was) in &hashed.stamps {
                        seen.stamps.entry(path.clone()).or_insert(*was);
                    }
                    seen.files.extend(hashed.files.iter().cloned());
                    seen.watched.extend(hashed.watch.iter().cloned());
                    seen.dirs.push(local.dir.clone());
                    if let Some(manifest) = &local.workspace
                        && manifest.parent() != Some(root)
                    {
                        inherited.insert(manifest.clone());
                    }
                    hashed.digest
                }
            };
            inputs.push(Input { kind: "package", name, digest });
        }
        // What the packages' watches cover; every file hashed past them is watched by itself, but
        // those under a directory of `extra`, watched whole.
        seen.package_files = seen.files.clone();

        // 2. The manifests of the workspace and of the workspaces packages inherit from, every
        // package that can turn on a feature of one of the closure's, and cargo's configuration
        // in the workspace's root and every directory above it, as cargo run in the workspace
        // reads them (`$CARGO_HOME`'s is the build's, [`finish`]).
        if identity.how_built {
            let mut manifests = BTreeSet::new();
            manifests.insert(root.join("Cargo.toml"));
            manifests.extend(workspace.members.iter().cloned());
            manifests.extend(inherited);
            // cargo unifies features across every package it builds together, so a package
            // outside the closure that depends on one of it (at any depth, by any edge the lock
            // lists: build, dev, optional, any platform's) can turn on a feature of it: a
            // registry one by its checksum, which is its manifest; one of the checkout by its
            // manifest and the workspace's it inherits from.
            for at in workspace.dependents(&closure)? {
                let package = &workspace.lock[at];
                match package.source {
                    Some(_) => {
                        let digest = registry_digest(workspace, package)?;
                        inputs.push(Input {
                            kind: "dependent",
                            name: locked_name(package),
                            digest,
                        });
                    }
                    None => {
                        let local = workspace.local(package)?;
                        manifests.insert(local.dir.join("Cargo.toml"));
                        manifests.extend(local.workspace.iter().cloned());
                    }
                }
            }
            for dir in root.ancestors() {
                for config in [".cargo/config.toml", ".cargo/config"] {
                    let path = dir.join(config);
                    if path.is_file() {
                        manifests.insert(path);
                    }
                }
            }
            for path in manifests {
                let bytes = read_stamped(&path, &mut seen.stamps)?;
                inputs.push(Input {
                    kind: "manifest",
                    name: relative_name(&path, root),
                    digest: digest_of("manifest", &[&bytes]),
                });
                seen.files.insert(path);
            }
        }

        // 3. Files outside the packages, and the variables they read. Every reader is in the
        // closure: the identity's required root is linked (`closure` refuses a program that does
        // not link it), and a reader outside the closure, or renamed away, is an extra the
        // identity names wrongly, never one to leave out.
        for (reader, extra) in identity.extra {
            let local = closure
                .iter()
                .map(|at| &workspace.lock[*at])
                .find(|it| it.source.is_none() && it.name == *reader)
                .map(|it| workspace.local(it))
                .transpose()?
                .ok_or_else(|| format!("`{reader}`, which reads {extra}, is no package of the checkout in the closure"))?;
            let path = local.dir.join(extra);
            let path = canonical(&path)
                .map_err(|why| format!("{extra}, which `{reader}` reads: {why}"))?;
            for file in extra_files(&path, &mut seen.stamps)? {
                let bytes = read_stamped(&file, &mut seen.stamps)?;
                inputs.push(Input {
                    kind: "file",
                    name: relative_name(&file, root),
                    digest: digest_of("file", &[&bytes]),
                });
                seen.files.insert(file);
            }
            if path.is_dir() {
                seen.extra_dirs.push(path);
            }
        }

        // 4. The stencil libraries, by their pin.
        let stencils = closure
            .iter()
            .map(|at| &workspace.lock[*at])
            .find(|it| identity.how_built && it.source.is_none() && it.name == STENCIL_PACKAGE);
        if let Some(stencils) = stencils {
            let path = workspace.local(stencils)?.dir.join(stencil_pin::PIN_FILE);
            let text = String::from_utf8(read_stamped(&path, &mut seen.stamps)?)
                .map_err(|_| format!("{} is not UTF-8", path.display()))?;
            let pinned = stencil_pin::pin_parse(&text)
                .map_err(|why| format!("the stencil libraries' pin {}: {why}", path.display()))?;
            for (name, id) in pinned {
                inputs.push(Input { kind: "stencils", name, digest: id });
            }
            seen.files.insert(path);
        }

        // 5's pin: read here, used by the build ([`finish`]) unless it is the seed's.
        let llvm_pin = if identity.how_built && links_llvm {
            let (path, json) = llvm_pin(workspace, &closure, &mut seen.stamps)?;
            seen.llvm_pin = Some(path);
            Some(json)
        } else {
            None
        };
        Ok((Tree { domain: identity.domain.to_owned(), inputs, llvm_pin }, seen))
    }
}

/// What of an identity the checkout alone decides: steps 1 to 4 of the crate doc (the packages,
/// the checkout's manifests and cargo configuration, the files outside the packages, the stencil
/// pin) and the text of the LLVM pin step 5 reads for the build's `TARGET` and `HOST`. The rest
/// (`$CARGO_HOME`'s configuration, the pinned LLVM's prefix, the build's variables and its
/// compiler) is the build's, so a build worked out from a shipped tree ([`Tree::shipped`]) has the
/// identity a build of the checkout it was cut from has in the same environment.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Tree {
    /// The identity's domain: which identity, by which formula.
    pub domain: String,
    /// Steps 1 to 4, in the order hashed: `package`, `dependent`, `manifest`, `file` and
    /// `stencils` inputs.
    pub inputs: Vec<Input>,
    /// The LLVM pin's text (crates/hir-ty-codegen/llvm.json), when `llvm-sys` is a package of the
    /// closure and how blaze is built is an input.
    pub llvm_pin: Option<String>,
}

/// The first line of a shipped tree: what it is and the format's version.
const SHIPPED_HEADER: &str = "blaze shipped code identity 1";
/// The kinds of input a tree holds.
const TREE_KINDS: [&str; 5] = ["package", "dependent", "manifest", "file", "stencils"];

impl Tree {
    /// The tree as a package carries it ([`Identity::shipped`]): a header, its domain, then one
    /// line per input (kind, name and digest in hex, tab-separated) and the LLVM pin's text in hex.
    pub fn shipped(&self) -> Result<String, String> {
        let mut out = format!("{SHIPPED_HEADER}\ndomain\t{}\n", self.domain);
        for input in &self.inputs {
            if input.name.contains(['\t', '\n']) {
                return Err(format!(
                    "the input `{}` has a tab or a line break in its name, which a shipped tree can't hold",
                    input.name
                ));
            }
            out.push_str(&format!("{}\t{}\t{}\n", input.kind, input.name, hex(&input.digest)));
        }
        if let Some(pin) = &self.llvm_pin {
            out.push_str(&format!("llvm-pin\t{}\n", hex(pin.as_bytes())));
        }
        Ok(out)
    }

    /// A shipped tree of `identity`, read from `from` (named in every error).
    pub fn parse(text: &str, identity: &Identity, from: &Path) -> Result<Tree, String> {
        let refused =
            |line: usize, why: String| format!("{}, line {}: {why}", from.display(), line + 1);
        let mut lines = text.lines().enumerate();
        match lines.next() {
            Some((_, SHIPPED_HEADER)) => {}
            other => {
                return Err(refused(
                    0,
                    format!("starts `{}`, not `{SHIPPED_HEADER}`", other.map_or("", |(_, it)| it)),
                ));
            }
        }
        let domain = match lines.next() {
            Some((_, line)) if line.strip_prefix("domain\t") == Some(identity.domain) => {
                identity.domain.to_owned()
            }
            Some((at, line)) => {
                return Err(refused(
                    at,
                    format!(
                        "`{line}` names another identity or formula than {}'s `{}`",
                        identity.what, identity.domain
                    ),
                ));
            }
            None => return Err(refused(1, "it ends before its domain".to_owned())),
        };
        let mut inputs = Vec::new();
        let mut llvm_pin = None;
        for (at, line) in lines {
            let fields: Vec<&str> = line.split('\t').collect();
            match fields[..] {
                ["llvm-pin", text] if llvm_pin.is_none() => {
                    let bytes = unhex(text)
                        .ok_or_else(|| refused(at, "the LLVM pin is not hex".to_owned()))?;
                    llvm_pin = Some(
                        String::from_utf8(bytes)
                            .map_err(|_| refused(at, "the LLVM pin is not UTF-8".to_owned()))?,
                    );
                }
                [kind, name, digest] => {
                    let kind = TREE_KINDS.into_iter().find(|it| *it == kind).ok_or_else(|| {
                        refused(at, format!("`{kind}` is no kind of input a tree holds"))
                    })?;
                    let digest: [u8; 32] =
                        unhex(digest).and_then(|it| it.try_into().ok()).ok_or_else(|| {
                            refused(at, format!("the digest of `{name}` is not 64 hex digits"))
                        })?;
                    inputs.push(Input { kind, name: name.to_owned(), digest });
                }
                _ => return Err(refused(at, format!("`{line}` is no input of a shipped tree"))),
            }
        }
        Ok(Tree { domain, inputs, llvm_pin })
    }
}

/// The bytes of `text`, two lowercase hex digits each.
fn unhex(text: &str) -> Option<Vec<u8>> {
    if text.len() % 2 != 0 || !text.bytes().all(|it| matches!(it, b'0'..=b'9' | b'a'..=b'f')) {
        return None;
    }
    (0..text.len()).step_by(2).map(|at| u8::from_str_radix(&text[at..at + 2], 16).ok()).collect()
}

/// What working a tree out of the checkout saw besides the tree: what [`Worked`] says of the
/// checkout's files and packages.
#[derive(Default)]
struct Seen {
    files: BTreeSet<PathBuf>,
    stamps: BTreeMap<PathBuf, Stamp>,
    dirs: Vec<PathBuf>,
    watched: Vec<PathBuf>,
    registry: Vec<(String, String)>,
    packages: BTreeSet<String>,
    crates: BTreeSet<String>,
    /// The files hashed as files of packages, which the packages' watches cover.
    package_files: BTreeSet<PathBuf>,
    /// The directories of [`Identity::extra`], watched whole.
    extra_dirs: Vec<PathBuf>,
    /// Where the LLVM pin was read, when it was.
    llvm_pin: Option<PathBuf>,
}

/// `identity` of a package cut from a checkout, from the tree the checkout shipped with it
/// (`shipped`, [`Identity::shipped`]), built in `env`.
pub fn work_out_shipped(
    shipped: &Path,
    identity: &Identity,
    env: &BuildEnv,
) -> Result<Worked, String> {
    let mut seen = Seen::default();
    let bytes = read_stamped(shipped, &mut seen.stamps)?;
    let text =
        String::from_utf8(bytes).map_err(|_| format!("{} is not UTF-8", shipped.display()))?;
    let tree = Tree::parse(&text, identity, shipped)?;
    seen.files.insert(shipped.to_path_buf());
    finish(identity, &tree, env, seen)
}

/// `identity` from `tree` (and, from a checkout, what working the tree out saw), built in `env`:
/// the build's part of step 2 (`$CARGO_HOME`'s configuration), steps 5 and 6, then the hash.
fn finish(identity: &Identity, tree: &Tree, env: &BuildEnv, seen: Seen) -> Result<Worked, String> {
    // Of the sources alone: no variable, profile, compiler or configuration is an input.
    let sources_only = BuildEnv::default();
    let env = if identity.how_built { env } else { &sources_only };
    let Seen {
        mut files,
        mut stamps,
        dirs,
        mut watched,
        registry,
        packages,
        crates,
        package_files,
        extra_dirs,
        llvm_pin: llvm_pin_path,
    } = seen;
    let mut inputs = tree.inputs.clone();

    // 2. `$CARGO_HOME`'s configuration, after the checkout's (one already read as the checkout's
    // is not read twice).
    if identity.how_built {
        let at = inputs
            .iter()
            .position(|it| matches!(it.kind, "file" | "stencils"))
            .unwrap_or(inputs.len());
        let mut configs = Vec::new();
        for config in ["config.toml", "config"] {
            if let Some(path) = env.cargo_home.as_ref().map(|it| it.join(config))
                && path.is_file()
                && !files.contains(&path)
            {
                let bytes = read_stamped(&path, &mut stamps)?;
                configs.push(Input {
                    kind: "manifest",
                    name: format!("$CARGO_HOME/{config}"),
                    digest: digest_of("manifest", &[&bytes]),
                });
                files.insert(path);
            }
        }
        inputs.splice(at..at, configs);
    }
    let seed = env.seed();

    // 5. LLVM: the pin, and the prefix checked to be it; none in the seed, which links no LLVM
    // and makes no stencils with it.
    let mut rerun_env: Vec<String> = if identity.how_built {
        READ_ENV.iter().map(|it| (*it).to_owned()).collect()
    } else {
        Vec::new()
    };
    let mut llvm_prefix = None;
    if let Some(json) = tree.llvm_pin.as_deref().filter(|_| identity.how_built && !seed) {
        rerun_env.push(LLVM_PREFIX.to_owned());
        let from = llvm_pin_path
            .as_ref()
            .map_or_else(|| "the shipped tree's".to_owned(), |it| it.display().to_string());
        // The types written out and `refusal` called by its path: the stage 0 blaze's check
        // (0.20261030) reads no `include!` from another package, so `pin`'s items are unknown to
        // it, and a method of an unknown type is an error there. Plain again once the pinned
        // release's check is the v2 front end, which reads it (`Db::resolve_path`).
        let pinned: pin::Pin = pin::Pin::parse(json)
            .map_err(|why| format!("blaze's LLVM pin `{from}` can't be read: {why}"))?;
        if let Some(path) = llvm_pin_path {
            files.insert(path);
        }
        let mut built_for = BTreeSet::new();
        for name in ["TARGET", "HOST"] {
            let tuple = env.vars.get(name).cloned().flatten().ok_or_else(|| {
                format!("${name}, which cargo gives a build script, names whose release of the pinned LLVM is linked")
            })?;
            built_for.insert(tuple);
        }
        for tuple in built_for {
            let sha: Option<String> =
                pin::pin_sha256(json, &tuple).map_err(|why| format!("blaze's LLVM pin: {why}"))?;
            let released = sha.as_deref().map_or(&b"none"[..], str::as_bytes);
            let digest = digest_of(
                "llvm pin",
                &[pinned.version.as_bytes(), pinned.commit.as_bytes(), tuple.as_bytes(), released],
            );
            inputs.push(Input { kind: "llvm", name: format!("pin {tuple}"), digest });
        }
        if let Some(prefix) = env.llvm_prefix.as_deref() {
            pin::pin_check(
                &pinned,
                Some(std::ffi::OsStr::new(prefix)),
                std::env::consts::EXE_SUFFIX,
            )
            .map_err(|found| pin::Pin::refusal(&pinned, &found))?;
            let prefix =
                canonical(Path::new(prefix)).map_err(|why| format!("${LLVM_PREFIX}: {why}"))?;
            let marker = prefix.join(pin::PIN_MARKER);
            let bytes = read_stamped(&marker, &mut stamps)?;
            inputs.push(Input {
                kind: "llvm",
                name: pin::PIN_MARKER.to_owned(),
                digest: digest_of("llvm marker", &[&bytes]),
            });
            files.insert(marker);
            llvm_prefix = Some(prefix);
        }
    }

    // 6. How blaze is built.
    if let Some(Some(dir)) = env.vars.get(ISLE_SOURCE_DIR) {
        return Err(format!(
            "${ISLE_SOURCE_DIR} is `{dir}`: cranelift-codegen's build script then makes its code \
             from the ISLE files there, which the code identity does not hash; unset it"
        ));
    }
    for (name, value) in &env.vars {
        let digest = match value {
            Some(value) => digest_of("env set", &[value.as_bytes()]),
            None => digest_of("env unset", &[]),
        };
        inputs.push(Input { kind: "env", name: name.clone(), digest });
    }
    for name in env.vars.keys().filter(|it| it.starts_with("CARGO_PROFILE_")) {
        rerun_env.push(name.clone());
    }
    if let Some(compiler) = &env.compiler {
        inputs.push(Input {
            kind: "compiler",
            name: "rustc -vV".to_owned(),
            digest: digest_of("compiler", &[compiler.as_bytes()]),
        });
    }

    let past_packages = files.difference(&package_files);
    watched.extend(
        past_packages.filter(|file| !extra_dirs.iter().any(|dir| file.starts_with(dir))).cloned(),
    );
    watched.extend(extra_dirs);
    watched.sort();
    watched.dedup();

    let mut hasher = hasher_of(&tree.domain);
    hasher.update(&(inputs.len() as u64).to_le_bytes());
    for input in &inputs {
        hasher.update(&(input.kind.len() as u64).to_le_bytes()).update(input.kind.as_bytes());
        hasher.update(&(input.name.len() as u64).to_le_bytes()).update(input.name.as_bytes());
        hasher.update(&input.digest);
    }
    Ok(Worked {
        code: *hasher.finalize().as_bytes(),
        inputs,
        files,
        stamps,
        dirs,
        watched,
        registry,
        packages,
        crates,
        rerun_env,
        llvm_prefix,
    })
}

/// What a checkout ships with a package cut from it for `identity` of `workspace`'s program
/// ([`Tree::shipped`]); refused, naming them, when files it was worked out from changed while it
/// was.
pub fn ship(workspace: &Workspace, identity: &Identity) -> Result<String, String> {
    let (tree, seen) = Walk::default().tree(workspace, identity)?;
    let changed = changed(&seen.stamps);
    if !changed.is_empty() {
        let named: Vec<String> = changed.iter().map(|it| it.display().to_string()).collect();
        return Err(format!(
            "{} file(s) changed while {} was worked out, so it is of no one tree: {}",
            changed.len(),
            identity.what,
            named.join(", ")
        ));
    }
    tree.shipped()
}

/// A package of the lock as an input names it: name, version and source.
fn locked_name(package: &Locked) -> String {
    format!(
        "{} {} {}",
        package.name,
        package.version,
        package.source.as_deref().unwrap_or("checkout")
    )
}

/// The digest of `package`, a package of the lock from outside the checkout: a registry package
/// by its checksum. A git package, or another source, is refused, naming it.
fn registry_digest(workspace: &Workspace, package: &Locked) -> Result<[u8; 32], String> {
    let name = locked_name(package);
    match package.source.as_deref() {
        Some(source) if source.starts_with("registry+") || source.starts_with("sparse+") => {
            let checksum = package.checksum.as_deref().ok_or_else(|| {
                format!(
                    "{name} has no checksum in {}: a registry package is an input by its checksum",
                    workspace.lock_path.display()
                )
            })?;
            Ok(digest_of("package registry", &[checksum.as_bytes()]))
        }
        Some(source) if source.starts_with("git+") => Err(format!(
            "{} {} comes from git ({source}), and the code identity keys a package by its lock \
             checksum or, in the checkout, by its files: a git package has neither (vendor it \
             under the checkout, or key git sources by their commit in blaze-codegen-code)",
            package.name, package.version
        )),
        Some(source) => Err(format!("{name}: the code identity knows no source `{source}`")),
        None => Err(format!("{name} is a package of the checkout, keyed by its files")),
    }
}

/// Where the pin llvm-sys checks LLVM against is, as pin.rs reads it from llvm-sys's directory,
/// and its text, checked to be a pin.
fn llvm_pin(
    workspace: &Workspace,
    closure: &BTreeSet<usize>,
    stamps: &mut BTreeMap<PathBuf, Stamp>,
) -> Result<(PathBuf, String), String> {
    let sys = closure
        .iter()
        .map(|at| &workspace.lock[*at])
        .find(|it| it.name == LLVM_PACKAGE)
        .ok_or_else(|| format!("{LLVM_PACKAGE} is no package of the closure"))?;
    if let Some(source) = &sys.source {
        return Err(format!(
            "{LLVM_PACKAGE} {} is from {source}: blaze links only its own (vendor/llvm-sys), which checks the pin",
            sys.version
        ));
    }
    let path = workspace.local(sys)?.dir.join(pin::PIN_JSON);
    let path = canonical(&path).map_err(|why| format!("blaze's LLVM pin: {why}"))?;
    let json = String::from_utf8(read_stamped(&path, stamps)?)
        .map_err(|_| format!("{} is not UTF-8", path.display()))?;
    pin::Pin::parse(&json)
        .map_err(|why| format!("blaze's LLVM pin `{}` can't be read: {why}", path.display()))?;
    Ok((path, json))
}

/// The name rustc gives the library of `local`: its `[lib] name`, else its package name.
fn lib_name(local: &Local) -> String {
    local
        .manifest
        .get("lib")
        .and_then(|it| it.get("name"))
        .and_then(toml::Value::as_str)
        .unwrap_or(&local.name)
        .replace('-', "_")
}

/// The directories of `local`'s library, binaries and build script, where its manifest names
/// them; the default ones (`src/`, `build.rs`) are never below `tests`, `benches` or `examples`.
fn built_dirs(local: &Local) -> Vec<PathBuf> {
    let mut paths: Vec<&str> = Vec::new();
    if let Some(path) =
        local.manifest.get("lib").and_then(|it| it.get("path")).and_then(toml::Value::as_str)
    {
        paths.push(path);
    }
    for bin in local.manifest.get("bin").and_then(toml::Value::as_array).into_iter().flatten() {
        if let Some(path) = bin.get("path").and_then(toml::Value::as_str) {
            paths.push(path);
        }
    }
    if let Some(path) =
        local.manifest.get("package").and_then(|it| it.get("build")).and_then(toml::Value::as_str)
    {
        paths.push(path);
    }
    paths.into_iter().filter_map(|it| local.dir.join(it).parent().map(Path::to_path_buf)).collect()
}

fn hasher_of(domain: &str) -> blake3::Hasher {
    blake3::Hasher::new_derive_key(domain)
}

fn hasher(kind: &str) -> blake3::Hasher {
    let mut hasher = hasher_of(INPUT_DOMAIN);
    hasher.update(&(kind.len() as u64).to_le_bytes()).update(kind.as_bytes());
    hasher
}

fn digest_of(kind: &str, parts: &[&[u8]]) -> [u8; 32] {
    let mut hasher = hasher(kind);
    for part in parts {
        hasher.update(&(part.len() as u64).to_le_bytes()).update(part);
    }
    *hasher.finalize().as_bytes()
}

/// `path` relative to `root`, with `/`, or as it is outside it.
fn relative_name(path: &Path, root: &Path) -> String {
    match path.strip_prefix(root) {
        Ok(relative) => slashed(relative),
        // Above the workspace (an inherited root): by its depth above it, never its absolute
        // path, so the checkout's place is no input.
        Err(_) => {
            let common = root.ancestors().find(|it| path.starts_with(it)).unwrap_or(Path::new(""));
            let up = root.strip_prefix(common).map_or(0, |it| it.components().count());
            let rest = path.strip_prefix(common).map(slashed).unwrap_or_default();
            format!("{}{rest}", "../".repeat(up))
        }
    }
}

fn slashed(path: &Path) -> String {
    path.components().map(|it| it.as_os_str().to_string_lossy()).collect::<Vec<_>>().join("/")
}

/// The files of an extra input at `path`: the file itself, or every file under the directory
/// but those of a dot name, sorted. Each directory listed is stamped, so a file added or
/// removed while it works is seen. A link to a file is the file; a link to a directory is
/// refused, as in a package ([`walk`]): followed, a link to a directory above it never ends.
fn extra_files(path: &Path, stamps: &mut BTreeMap<PathBuf, Stamp>) -> Result<Vec<PathBuf>, String> {
    if !path.is_dir() {
        return Ok(vec![path.to_path_buf()]);
    }
    let mut out = Vec::new();
    let mut stack = vec![path.to_path_buf()];
    while let Some(dir) = stack.pop() {
        stamps.entry(dir.clone()).or_insert(stamp(&dir)?);
        let entries =
            std::fs::read_dir(&dir).map_err(|why| format!("listing {}: {why}", dir.display()))?;
        for entry in entries {
            let entry = entry.map_err(|why| format!("listing {}: {why}", dir.display()))?;
            if entry.file_name().to_string_lossy().starts_with('.') {
                continue;
            }
            let path = entry.path();
            let kind = entry.file_type().map_err(|why| format!("{}: {why}", path.display()))?;
            if kind.is_symlink() {
                let meta =
                    std::fs::metadata(&path).map_err(|why| format!("{}: {why}", path.display()))?;
                if meta.is_dir() {
                    return Err(format!(
                        "{} is a link to a directory: its files are not hashed",
                        path.display()
                    ));
                }
                out.push(path);
            } else if kind.is_dir() {
                stack.push(path);
            } else {
                out.push(path);
            }
        }
    }
    out.sort();
    Ok(out)
}

/// What [`package_files`] found of a package of the checkout.
struct Listed {
    /// Every file hashed, sorted by its path relative to the package's directory, with `/`.
    files: Vec<(String, PathBuf)>,
    /// What a build script watches for them: each directory every entry under which, at any
    /// depth, is hashed, whole (a file added or removed in it changes its mtime, which cargo's
    /// watch of a directory counts); and each other file hashed, by itself.
    watch: Vec<PathBuf>,
}

/// How [`walk`] found a directory.
enum Walked {
    /// Every entry under it, at any depth, is hashed: watched whole.
    Whole,
    /// It holds a skipped entry: watched by these paths, its files and its subdirectories that are whole.
    Parts(Vec<PathBuf>),
}

/// Every file of a package of the checkout, and what to watch for them: every file under its
/// directory but a name starting with `.` and, at its root, `target` and the `tests`, `benches`
/// and `examples` directories when none of its library, binaries or build script is in them.
/// Each directory listed is stamped, so a file added or removed while it works is seen.
fn package_files(local: &Local, stamps: &mut BTreeMap<PathBuf, Stamp>) -> Result<Listed, String> {
    let built = built_dirs(local);
    let mut files = Vec::new();
    let watch = match walk(local, &built, &local.dir, stamps, &mut files)? {
        Walked::Whole => vec![local.dir.clone()],
        Walked::Parts(parts) => parts,
    };
    files.sort();
    Ok(Listed { files, watch })
}

/// Lists `dir`, a directory of `local` whose library, binaries and build script are in `built`,
/// into `files`, and says what watches it.
fn walk(
    local: &Local,
    built: &[PathBuf],
    dir: &Path,
    stamps: &mut BTreeMap<PathBuf, Stamp>,
    files: &mut Vec<(String, PathBuf)>,
) -> Result<Walked, String> {
    stamps.entry(dir.to_path_buf()).or_insert(stamp(dir)?);
    let entries =
        std::fs::read_dir(dir).map_err(|why| format!("listing {}: {why}", dir.display()))?;
    let at_root = dir == local.dir;
    let mut whole = true;
    let mut parts = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|why| format!("listing {}: {why}", dir.display()))?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with('.') {
            whole = false;
            continue;
        }
        let path = entry.path();
        let kind = entry.file_type().map_err(|why| format!("{}: {why}", path.display()))?;
        if kind.is_symlink() {
            let meta =
                std::fs::metadata(&path).map_err(|why| format!("{}: {why}", path.display()))?;
            if meta.is_dir() {
                return Err(format!(
                    "{} of package {} is a link to a directory: its files are not hashed",
                    path.display(),
                    local.name
                ));
            }
        } else if kind.is_dir() {
            let skipped = at_root
                && (name == "target"
                    || (matches!(&*name, "tests" | "benches" | "examples")
                        && !built.iter().any(|it| it.starts_with(&path))));
            if skipped {
                whole = false;
                continue;
            }
            match walk(local, built, &path, stamps, files)? {
                Walked::Whole => parts.push(path),
                Walked::Parts(below) => {
                    whole = false;
                    parts.extend(below);
                }
            }
            continue;
        }
        let relative = path
            .strip_prefix(&local.dir)
            .map_err(|_| format!("{} is outside its package", path.display()))?;
        files.push((slashed(relative), path.clone()));
        parts.push(path);
    }
    Ok(if whole { Walked::Whole } else { Walked::Parts(parts) })
}
