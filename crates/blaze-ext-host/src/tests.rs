//! The host end to end: the worked example against blaze's own lint, every refusal of the
//! checks, and jobs that panic, spin, hoard and read an address.
//!
//! Extensions here are linked into the test ([`Loaded::linked`]): the worked example is the
//! crate a user would write (`extensions/panic-in-drop`), and the troublemakers are written
//! below. Code linked into a test is not metered, so the spinning one ticks by hand
//! (`blaze_ext::host::tick`), as compiled code does at every loop head.

use std::path::Path;

use base_db::{Db, FileId};
use blaze_diagnostics::model::{Diag, Level as DiagLevel};
use blaze_ext::{
    Body, Cx, Extension, Level, Lint,
    host::{Limits, Stop},
    mir::MirSpan,
};
use blaze_ids::ContentId;

use crate::{
    PrepareError, build, check,
    check::Rule,
    facts::Cache,
    job::{Failure, What},
    key,
    load::{self, LoadError, Loaded},
    manifest::{Declared, Source},
    run::{self, Checked, Using},
    source::{self, Sources},
};

// --- Fixtures --------------------------------------------------------------------------------

const HEAD: &str = r#"//- minicore: sized, copy, clone, option, result, fn, deref, drop, builtin_impls
//- /std/lib.rs crate:std
pub mod prelude {
    pub mod v1 { pub use core::prelude::v1::*; }
    pub mod rust_2015 { pub use super::v1::*; }
    pub mod rust_2018 { pub use super::v1::*; }
    pub mod rust_2021 { pub use super::v1::*; }
    pub mod rust_2024 { pub use super::v1::*; }
}
pub mod thread {
    pub fn panicking() -> bool { false }
}
"#;

/// What the in-tree lint's own test checks, and more of the same.
const DROPS: &str = r#"
pub struct Guard { v: Option<u8> }
impl Drop for Guard {
    fn drop(&mut self) { self.v.unwrap(); }
}
pub struct Careful { v: Option<u8> }
impl Drop for Careful {
    fn drop(&mut self) {
        if std::thread::panicking() { return; }
        self.v.unwrap();
    }
}
pub struct Twice { a: Option<u8>, b: Option<u8> }
impl Drop for Twice {
    fn drop(&mut self) {
        self.a.unwrap();
        self.b.unwrap();
    }
}
pub struct Plain { v: Option<u8> }
impl Plain {
    pub fn drop(&mut self) { self.v.unwrap(); }
}
pub fn free(v: Option<u8>) -> u8 { v.unwrap() }
"#;

/// A `drop` that panics only through what it calls, some of it recursive.
const DEEP: &str = r#"
pub struct Guard { v: Option<u8> }
fn inner(v: Option<u8>) -> u8 { v.unwrap() }
fn ping(v: Option<u8>, again: bool) -> u8 { if again { pong(v) } else { inner(v) } }
fn pong(v: Option<u8>) -> u8 { ping(v, false) }
fn quiet(v: Option<u8>) -> Option<u8> { v }
impl Drop for Guard {
    fn drop(&mut self) {
        ping(self.v, true);
        quiet(self.v);
    }
}
"#;

/// Three plain functions, for the troublemakers.
const PLAIN: &str = r#"
pub fn boom() -> u8 { 1 }
pub fn calm() -> u8 { 2 }
pub fn spins() -> u8 { 3 }
"#;

fn db_of(main: &str) -> Db {
    let fixture =
        format!("{HEAD}//- /src/lib.rs crate:main deps:std\n{}", main.trim_start_matches('\n'));
    hir_expand::fixture::db(&fixture).into_db()
}

fn main_crate(db: &Db) -> hir::Crate {
    hir::Crate::all(db)
        .into_iter()
        .find(|it| it.display_name(db).is_some_and(|it| it.to_string() == "main"))
        .expect("the fixture has a crate `main`")
}

fn name_of(file: FileId) -> Option<String> {
    Some(format!("file{}", file.index()))
}

fn checked_with(db: &Db, using: &[Using<'_>], limits: Limits, cache: &mut Cache) -> Checked {
    blaze_query::db::attach(db, || {
        run::check(db, main_crate(db), using, limits, cache, &name_of)
            .expect("the crate is checked")
    })
}

fn checked(main: &str, using: &[Using<'_>], limits: Limits) -> Checked {
    checked_with(&db_of(main), using, limits, &mut Cache::default())
}

fn example() -> Loaded {
    Loaded::linked(panic_in_drop::extension(), ContentId::of("test extension", b"panic-in-drop"))
        .expect("the worked example's descriptor is one blaze can run")
}

/// A diagnostic as the comparisons read it: what it says and where.
fn said(diag: &Diag) -> (String, String, u32, u32) {
    let span = diag.span.primary_span().expect("a lint's diagnostic has a place");
    (diag.message.clone(), span.file.clone(), span.lo, span.hi)
}

// --- The worked example ----------------------------------------------------------------------

/// What blaze's own `blaze::panic_in_drop` says of the crate of `db`, and where, sorted.
fn in_tree(db: &Db) -> Vec<(String, String, u32, u32)> {
    let found = blaze_query::db::attach(db, || {
        let flags: Vec<String> =
            ["-A", "clippy::all", "--warn=blaze::panic_in_drop"].map(str::to_owned).to_vec();
        let setup = blaze_lints::Setup {
            command_line: blaze_lints::level::CommandLine::parse(&flags).expect("the flags read"),
            conf: blaze_lints::conf::Conf::default(),
        };
        blaze_lints::check(db, main_crate(db), &setup, &name_of).expect("the crate's lints run")
    });
    let mut said: Vec<_> = found
        .iter()
        .filter(|it| it.diagnostic.lint_name() == Some("blaze::panic_in_drop"))
        .map(|it| said(&it.diagnostic))
        .collect();
    said.sort();
    said
}

/// What the extension `loaded` says of the crate of `db`, and where, sorted.
fn by_extension(db: &Db, loaded: &Loaded) -> Vec<(String, String, u32, u32)> {
    let using = [Using { loaded, levels: &[], replay: false }];
    let ours = checked_with(db, &using, Limits::DEFAULT, &mut Cache::default());
    assert!(ours.failed.is_empty(), "{:?}", ours.failed);
    assert!(ours.unbuilt.is_empty(), "{:?}", ours.unbuilt);
    let mut said: Vec<_> = ours.diagnostics.iter().map(said).collect();
    said.sort();
    said
}

/// The refinement: over the same source, the extension `panic-in-drop` reports exactly what
/// blaze's own `blaze::panic_in_drop` does: the same messages at the same places.
#[test]
fn the_worked_example_finds_what_the_in_tree_lint_finds() {
    let db = db_of(DROPS);
    let theirs = in_tree(&db);
    let ours = by_extension(&db, &example());
    assert_eq!(theirs.len(), 3, "`Guard`'s unwrap and `Twice`'s two, in-tree: {theirs:?}");
    assert_eq!(
        ours.iter().map(|it| &it.0).collect::<Vec<_>>(),
        theirs.iter().map(|it| &it.0).collect::<Vec<_>>(),
        "the same messages"
    );
    assert_eq!(ours, theirs, "at the same places");
    assert!(ours.iter().any(|it| it.0 == "this `unwrap` can panic in `Drop::drop` of `Guard`"));
}

/// The worked example's lint with a planted bug: it stops at a `drop`'s first `unwrap`.
fn first_only(cx: &mut Cx<'_>, body: &Body<'_, '_>) {
    let Some(of) = body.impl_of().filter(|it| it.is_std_trait("ops", "Drop")) else { return };
    for (block, data) in body.mir().basic_blocks.iter() {
        let (Some(end), Some(callee)) = (&data.terminator, body.callee(block)) else { continue };
        if callee.name() == "unwrap" && callee.is_std_method("option", "Option") {
            let of = of.self_ty();
            cx.report(end.span, format_args!("this `unwrap` can panic in `Drop::drop` of `{of}`"));
            return;
        }
    }
}

#[test]
fn the_oracle_catches_an_extension_that_misses_a_finding() {
    const FIRST: Lint = Lint {
        name: "panic_in_drop",
        level: Level::Warn,
        summary: "",
        requires: &[],
        run: first_only,
    };
    let extension =
        Extension { name: "first-only", api: blaze_ext::VERSION, lints: &[FIRST], facts: &[] };
    let loaded =
        Loaded::linked(extension, ContentId::of("test extension", b"first-only")).expect("loads");
    // `Careful` checks `panicking()` first, which the plant does not look for, and `Twice`
    // has a second `unwrap` it never reaches: the comparison the refinement makes differs.
    let db = db_of(DROPS);
    assert_ne!(by_extension(&db, &loaded), in_tree(&db));
}

#[test]
fn a_diagnostic_names_its_extension_and_takes_the_workspace_s_level() {
    let loaded = example();
    let warn =
        checked(DROPS, &[Using { loaded: &loaded, levels: &[], replay: false }], Limits::DEFAULT);
    let first = &warn.diagnostics[0];
    assert_eq!(first.lint_name(), Some("panic_in_drop::panic_in_drop"));
    assert_eq!(first.level, DiagLevel::Warning);
    let text = format!("{first:?}");
    assert!(text.contains("reported by the extension `panic-in-drop`"), "{text}");
    assert!(text.contains("a second panic then aborts the process"), "the lint's own note: {text}");

    let deny = [("panic_in_drop".to_owned(), Level::Deny)];
    let denied =
        checked(DROPS, &[Using { loaded: &loaded, levels: &deny, replay: false }], Limits::DEFAULT);
    assert!(denied.diagnostics.iter().all(|it| it.level == DiagLevel::Error));
    let allow = [("panic_in_drop".to_owned(), Level::Allow)];
    let allowed = checked(
        DROPS,
        &[Using { loaded: &loaded, levels: &allow, replay: false }],
        Limits::DEFAULT,
    );
    assert!(allowed.diagnostics.is_empty(), "an allowed lint is not run");
}

/// The fact `may_panic` through the real path: a recursion solved to its fixpoint, asked for
/// by a lint, and found, not computed, when the same source is checked again.
#[test]
fn a_lint_reads_a_fact_solved_over_a_recursion_and_a_second_check_finds_it() {
    let loaded = example();
    let deep = [("panic_in_drop_deep".to_owned(), Level::Warn)];
    let using = [Using { loaded: &loaded, levels: &deep, replay: false }];
    let mut cache = Cache::default();
    let first = checked_with(&db_of(DEEP), &using, Limits::DEFAULT, &mut cache);
    assert!(first.failed.is_empty(), "{:?}", first.failed);
    assert!(first.unsettled.is_empty(), "{:?}", first.unsettled);
    let messages: Vec<&str> = first.diagnostics.iter().map(|it| it.message.as_str()).collect();
    assert_eq!(messages, ["`ping` can panic, and is called in `Drop::drop` of `Guard`"]);
    assert!(
        first.computed.contains(
            &"fact `may_panic` of `inner` (extension `panic-in-drop`) was computed again: it had not been computed before"
                .to_owned()
        ),
        "{:?}",
        first.computed
    );

    // Another database of the same source: every key is of content, so every fact is found.
    let second = checked_with(&db_of(DEEP), &using, Limits::DEFAULT, &mut cache);
    assert_eq!(second.computed, Vec::<String>::new());
    assert_eq!(
        second.diagnostics.iter().map(said).collect::<Vec<_>>(),
        first.diagnostics.iter().map(said).collect::<Vec<_>>()
    );

    // An edit of `inner` that takes its panic away: its fact is computed again and says why,
    // and the finding goes.
    let edited = DEEP.replace("{ v.unwrap() }", "{ let _unused = v; 0 }");
    let third = checked_with(&db_of(&edited), &using, Limits::DEFAULT, &mut cache);
    assert!(third.diagnostics.is_empty(), "{:?}", third.diagnostics);
    assert!(
        third.computed.iter().any(|it| it.starts_with("fact `may_panic` of `inner`")
            && it.ends_with("the code of `inner` changed")),
        "{:?}",
        third.computed
    );
}

// --- Determinism -----------------------------------------------------------------------------

/// The invariant: two checks of the same source, on two databases, give the same
/// diagnostics, byte for byte, in the same order; and so does a check that runs every job
/// twice with its arena elsewhere.
#[test]
fn two_builds_give_the_same_diagnostics() {
    let loaded = example();
    let deep = [("panic_in_drop_deep".to_owned(), Level::Warn)];
    let render = |replay: bool, main: &str| {
        let using = [Using { loaded: &loaded, levels: &deep, replay }];
        let found = checked(main, &using, Limits::DEFAULT);
        assert!(found.failed.is_empty(), "{:?}", found.failed);
        format!("{:?}\n{:?}", found.diagnostics, found.computed)
    };
    for main in [DROPS, DEEP] {
        let once = render(false, main);
        assert_eq!(once, render(false, main), "a second build");
        assert_eq!(once, render(true, main), "a build that replays every job");
    }
}

fn leaky(cx: &mut Cx<'_>, body: &Body<'_, '_>) {
    if body.name() != "calm" {
        return;
    }
    let mut held = cx.vec::<u8>();
    held.push(1);
    cx.report(MirSpan::Unknown, format_args!("a byte at {:p}", held.as_ptr()));
}

#[test]
fn the_oracle_catches_a_result_that_depends_on_an_address() {
    const LEAKY: Lint =
        Lint { name: "leaky", level: Level::Warn, summary: "", requires: &[], run: leaky };
    let extension =
        Extension { name: "leak", api: blaze_ext::VERSION, lints: &[LEAKY], facts: &[] };
    let loaded =
        Loaded::linked(extension, ContentId::of("test extension", b"leak")).expect("loads");
    // Run once, an address in a message is just text.
    let once =
        checked(PLAIN, &[Using { loaded: &loaded, levels: &[], replay: false }], Limits::DEFAULT);
    assert_eq!((once.diagnostics.len(), once.failed.len()), (1, 0));
    // Replayed, the two runs differ, and the job is the error.
    let replayed =
        checked(PLAIN, &[Using { loaded: &loaded, levels: &[], replay: true }], Limits::DEFAULT);
    assert!(replayed.diagnostics.is_empty());
    let [failed] = &replayed.failed[..] else { panic!("one job failed: {:?}", replayed.failed) };
    assert_eq!(failed.failure, Failure::Unrepeatable);
    assert!(
        failed
            .to_string()
            .starts_with("extension `leak`: lint `leaky` gave two results for `calm` on two runs")
    );
}

// --- Jobs that go wrong ----------------------------------------------------------------------

fn trouble(cx: &mut Cx<'_>, body: &Body<'_, '_>) {
    match body.name() {
        "boom" => panic!("planted in the extension"),
        "spins" => loop {
            blaze_ext::host::tick();
        },
        name => {
            cx.report(MirSpan::Unknown, format_args!("`{name}` is fine"));
        }
    }
}

fn hoard(cx: &mut Cx<'_>, body: &Body<'_, '_>) {
    if body.name() == "calm" {
        let mut held = cx.vec::<u64>();
        loop {
            held.push(0);
        }
    }
}

const TROUBLE: Lint =
    Lint { name: "trouble", level: Level::Warn, summary: "", requires: &[], run: trouble };
const HOARD: Lint =
    Lint { name: "hoard", level: Level::Warn, summary: "", requires: &[], run: hoard };

fn troublemaker() -> Loaded {
    let extension = Extension {
        name: "trouble",
        api: blaze_ext::VERSION,
        lints: &[TROUBLE, HOARD],
        facts: &[],
    };
    Loaded::linked(extension, ContentId::of("test extension", b"trouble")).expect("loads")
}

/// A panic fails its job, named; the same lint's other jobs, the extension's other lints and
/// the other extensions go on.
#[test]
fn a_panicking_extension_fails_one_job_and_the_check_goes_on() {
    let bad = troublemaker();
    let good = example();
    let limits = Limits { fuel: 10_000, arena: 1 << 16, ..Limits::DEFAULT };
    let main = format!("{PLAIN}{DROPS}");
    let using = [
        Using { loaded: &bad, levels: &[], replay: false },
        Using { loaded: &good, levels: &[], replay: false },
    ];
    let found = checked(&main, &using, limits);

    let panicked: Vec<_> =
        found.failed.iter().filter(|it| matches!(it.failure, Failure::Panicked(_))).collect();
    let [panicked] = &panicked[..] else { panic!("one job panicked: {:?}", found.failed) };
    assert_eq!(panicked.what, What::Lint("trouble".to_owned()));
    assert_eq!(
        panicked.to_string(),
        "extension `trouble`: lint `trouble` panicked on `boom`: planted in the extension"
    );

    let messages: Vec<&str> = found.diagnostics.iter().map(|it| it.message.as_str()).collect();
    assert!(messages.contains(&"`calm` is fine"), "the same lint, the next body: {messages:?}");
    assert!(!messages.iter().any(|it| it.contains("`boom`")), "{messages:?}");
    assert!(
        messages.contains(&"this `unwrap` can panic in `Drop::drop` of `Guard`"),
        "the other extension: {messages:?}"
    );
    assert_eq!(run::failures(&found).len(), found.failed.len(), "each failure is a diagnostic too");
}

/// A loop that does not end is stopped at its fuel, naming the extension, the lint and the
/// item; so is a job that asks its arena for more than its cap.
#[test]
fn a_looping_extension_is_stopped_at_its_bound_naming_it() {
    let bad = troublemaker();
    let limits = Limits { fuel: 10_000, arena: 1 << 16, ..Limits::DEFAULT };
    let found = checked(PLAIN, &[Using { loaded: &bad, levels: &[], replay: false }], limits);
    let said: Vec<String> = found.failed.iter().map(ToString::to_string).collect();
    assert!(
        said.contains(
            &"extension `trouble`: lint `trouble` was stopped on `spins`: it ran out of fuel (10000 steps): a loop that \
              does not end, or work past the bound"
                .to_owned()
        ),
        "{said:?}"
    );
    let hoarded = found
        .failed
        .iter()
        .find(|it| it.what == What::Lint("hoard".to_owned()))
        .expect("the hoarding job failed");
    assert_eq!(hoarded.item, "calm");
    assert!(
        matches!(hoarded.failure, Failure::Stopped(Stop::Memory { cap: 65536, .. })),
        "{hoarded:?}"
    );
    // The bound is a count of steps, so the stop is at the same step every run.
    let again = checked(PLAIN, &[Using { loaded: &bad, levels: &[], replay: false }], limits);
    assert_eq!(again.failed, found.failed);
}

// --- The compile-time checks -----------------------------------------------------------------

const MANIFEST: &str = "[package]\nname = \"x\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[dependencies]\nblaze-ext = \"0.1\"\n";

const LIB: &str = "#![no_std]\nuse blaze_ext::Extension;\npub fn extension() -> Extension {\n    Extension { name: \"x\", api: blaze_ext::VERSION, lints: &[], facts: &[] }\n}\n";

fn extension_of(manifest: &str, lib: &str) -> Sources {
    Sources {
        name: "x".to_owned(),
        manifest: manifest.to_owned(),
        files: vec![("src/lib.rs".to_owned(), lib.to_owned())],
    }
}

/// The one refusal of `LIB` with `more` after it.
fn refused(more: &str) -> check::Refusal {
    let found = check::check(&extension_of(MANIFEST, &format!("{LIB}{more}\n")));
    let [refusal] = &found[..] else { panic!("one refusal of `{more}`: {found:?}") };
    refusal.clone()
}

#[test]
fn a_well_formed_extension_passes_and_so_does_the_worked_example() {
    assert_eq!(check::check(&extension_of(MANIFEST, LIB)), []);
    // What is not the keyword: a lifetime, a raw identifier, a word in a comment or a string.
    let fine = "pub fn name() -> &'static str { \"unsafe static extern std::fs\" }\n// unsafe { }\npub fn r#unsafe() {}\n#![feature(allocator_api)]\n";
    assert_eq!(check::check(&extension_of(MANIFEST, &format!("{LIB}{fine}"))), []);

    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let declared = Declared {
        name: "panic-in-drop".to_owned(),
        source: Source::Path("extensions/panic-in-drop".to_owned()),
        grants: Vec::new(),
        levels: Vec::new(),
        replay: false,
    };
    let sources = source::find(&root, &declared).expect("the worked example is read");
    assert_eq!(check::check(&sources), [], "extensions/panic-in-drop passes every check");
}

#[test]
fn unsafe_is_refused_wherever_it_is_written() {
    let refusal = refused("pub fn read(p: *const u8) -> u8 { unsafe { *p } }");
    assert_eq!(refusal.rule, Rule::Unsafe);
    assert_eq!(refusal.at, Some((6, 35)));
    assert_eq!(
        refusal.to_string(),
        "extension `x`: src/lib.rs:6:35: `unsafe`: an extension is safe Rust only; what needs `unsafe` belongs behind \
         `blaze-ext` (rule `ext::unsafe`)"
    );
    // In a macro's definition, under a `cfg` that is off, in an attribute, as a forced keyword.
    assert_eq!(
        refused("macro_rules! m { () => { unsafe { core::hint::unreachable_unchecked() } } }").rule,
        Rule::Unsafe
    );
    assert_eq!(refused("#[cfg(any())] unsafe fn off() {}").rule, Rule::Unsafe);
    assert_eq!(refused("unsafe impl Send for T {}").rule, Rule::Unsafe);
    // Another file of the crate is checked too, and named.
    let mut sources = extension_of(MANIFEST, LIB);
    sources.files.push(("src/deep/mod.rs".to_owned(), "pub unsafe fn f() {}\n".to_owned()));
    let found = check::check(&sources);
    assert_eq!(
        (found.len(), found[0].file.as_str(), found[0].at),
        (1, "src/deep/mod.rs", Some((1, 5)))
    );
}

#[test]
fn extern_blocks_ffi_and_extern_crates_are_refused() {
    let block = refused("extern \"C\" { fn getpid() -> i32; }");
    assert_eq!(block.rule, Rule::Extern);
    assert!(block.what.contains("declares no foreign function, block or ABI"), "{block}");
    assert_eq!(refused("pub extern \"C\" fn callback() {}").rule, Rule::Extern);
    let krate = refused("extern crate alloc;");
    assert_eq!(krate.rule, Rule::Extern);
    assert!(krate.what.starts_with("`extern crate alloc`"), "{krate}");
}

#[test]
fn inline_asm_is_refused() {
    let refusal = refused("pub fn f() { core::arch::asm!(\"nop\"); }");
    assert_eq!(refusal.rule, Rule::Asm);
    assert!(refusal.what.starts_with("`asm!`"), "{refusal}");
    assert_eq!(refused("core::arch::global_asm!(\".globl x\");").rule, Rule::Asm);
}

#[test]
fn statics_and_thread_locals_are_refused() {
    let refusal = refused(
        "static COUNT: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);",
    );
    assert_eq!(refusal.rule, Rule::GlobalState);
    assert!(refusal.what.contains("keeps no state between jobs"), "{refusal}");
    assert_eq!(refused("static mut SEEN: u32 = 0;").rule, Rule::GlobalState);
    assert_eq!(refused("thread_local! { }").rule, Rule::GlobalState);
}

#[test]
fn std_is_refused_by_path_and_by_a_missing_no_std() {
    let path = refused("pub fn f() { std::process::exit(1) }");
    assert_eq!(path.rule, Rule::NoStd);
    assert!(path.what.starts_with("a path through `std`"), "{path}");
    assert_eq!(refused("use ::std::fs;").rule, Rule::NoStd);

    let found = check::check(&extension_of(MANIFEST, &LIB.replace("#![no_std]\n", "")));
    let [missing] = &found[..] else { panic!("one refusal: {found:?}") };
    assert_eq!((missing.rule, missing.at), (Rule::NoStd, Some((1, 1))));
    assert!(missing.what.starts_with("the crate root has no `#![no_std]`"), "{missing}");
}

#[test]
fn linker_attributes_feature_gates_and_includes_are_refused() {
    let mangle = refused("#[no_mangle] pub fn malloc() {}");
    assert_eq!(mangle.rule, Rule::Attribute);
    assert!(mangle.what.starts_with("the attribute `no_mangle`"), "{mangle}");
    assert_eq!(
        refused("#[cfg_attr(all(), link_section = \".init_array\")] pub fn early() {}").rule,
        Rule::Attribute
    );
    assert_eq!(refused("#[panic_handler] fn on_panic() {}").rule, Rule::Attribute);

    let gate = refused("#![feature(rustc_attrs)]");
    assert_eq!(gate.rule, Rule::Feature);
    assert!(gate.what.starts_with("the feature `rustc_attrs`"), "{gate}");

    let include = refused("const SECRET: &str = include_str!(\"/etc/passwd\");");
    assert_eq!(include.rule, Rule::Include);
    assert_eq!(refused("#[path = \"/elsewhere.rs\"] mod elsewhere;").rule, Rule::Include);
}

#[test]
fn a_crate_without_its_entry_is_refused() {
    let found = check::check(&extension_of(MANIFEST, "#![no_std]\nfn extension() {}\n"));
    let [refusal] = &found[..] else { panic!("one refusal: {found:?}") };
    assert_eq!(refusal.rule, Rule::Entry);
    assert!(refusal.what.contains("`pub fn extension() -> blaze_ext::Extension`"), "{refusal}");
}

#[test]
fn dependencies_build_scripts_and_another_api_version_are_refused() {
    let one = |manifest: &str| {
        let found = check::check(&extension_of(manifest, LIB));
        let [refusal] = &found[..] else { panic!("one refusal of {manifest:?}: {found:?}") };
        refusal.clone()
    };
    let libc = one(&format!("{MANIFEST}libc = \"0.2\"\n"));
    assert_eq!((libc.rule, libc.file.as_str()), (Rule::Deps, "Cargo.toml"));
    assert!(libc.what.starts_with("the dependency `libc`"), "{libc}");
    assert_eq!(one(&format!("{MANIFEST}\n[build-dependencies]\ncc = \"1\"\n")).rule, Rule::Deps);
    assert_eq!(one(&format!("{MANIFEST}\n[lib]\nproc-macro = true\n")).rule, Rule::Deps);
    assert_eq!(one(&MANIFEST.replace("edition = \"2024\"\n", "edition = \"2024\"\nbuild = \"build.rs\"\n")).rule, Rule::Deps);

    let newer = one(&MANIFEST.replace("blaze-ext = \"0.1\"", "blaze-ext = \"0.7\""));
    assert_eq!(newer.rule, Rule::Api);
    assert_eq!(
        newer.to_string(),
        format!(
            "extension `x`: Cargo.toml: it is written for blaze-ext 0.7, and this blaze has blaze-ext {}: the API is not \
             stable across blaze's versions, so its source may need edits (rule `ext::api`)",
            blaze_ext::VERSION
        )
    );
    let older = one(&MANIFEST.replace("blaze-ext = \"0.1\"", "blaze-ext = \"0.0\""));
    assert!(
        older.what.contains("changed since: 0.1.0: the first surface"),
        "an older one is told what changed: {older}"
    );
    assert_eq!(one(&MANIFEST.replace("blaze-ext = \"0.1\"\n", "")).rule, Rule::Api);
}

// --- Finding, building, loading --------------------------------------------------------------

struct Recording {
    asked: std::cell::RefCell<Vec<(ContentId, String, &'static str)>>,
}

impl build::Toolchain for Recording {
    fn build(&self, request: &build::Request<'_>) -> Result<build::Image, build::BuildError> {
        self.asked.borrow_mut().push((request.key, request.sources.name.clone(), request.entry));
        Err(build::BuildError {
            extension: request.sources.name.clone(),
            why: "the test builds nothing".to_owned(),
        })
    }
}

fn identity() -> key::Identity {
    key::Identity {
        blaze: ContentId::of("test blaze", b"1"),
        toolchain: "nightly-2026-10-27".to_owned(),
        target: "aarch64-apple-darwin".to_owned(),
    }
}

#[test]
fn an_extension_is_checked_then_built_under_its_key_and_a_grant_is_refused_before_any_build() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let toolchain = Recording { asked: std::cell::RefCell::new(Vec::new()) };
    let manifest = "[workspace.metadata.blaze.extensions.panic-in-drop]\npath = \"extensions/panic-in-drop\"\n";
    match crate::prepare(&root, manifest, "Cargo.toml", &identity(), &toolchain) {
        Err(PrepareError::Build(error)) => assert_eq!(error.why, "the test builds nothing"),
        other => panic!("the build's refusal: {:?}", other.err()),
    }
    let declared = &crate::manifest::declared(manifest, "Cargo.toml").expect("reads")[0];
    let sources = source::find(&root, declared).expect("found");
    let asked = toolchain.asked.borrow().clone();
    assert_eq!(
        asked,
        [(key::build_key(&identity(), &sources, &[]), "panic-in-drop".to_owned(), "extension")]
    );

    let granted = format!("{manifest}grants = [\"clock\"]\n");
    match crate::prepare(&root, &granted, "Cargo.toml", &identity(), &toolchain) {
        Err(PrepareError::Refused(refusals)) => {
            assert_eq!(refusals.len(), 1);
            assert_eq!(refusals[0].rule, Rule::Grant);
            assert!(refusals[0].what.starts_with("it is granted `clock`"), "{}", refusals[0]);
        }
        other => panic!("the grant's refusal: {:?}", other.err()),
    }
    assert_eq!(toolchain.asked.borrow().len(), 1, "a refused extension is never built");

    let fetched = "[workspace.metadata.blaze.extensions.audit]\nversion = \"1.2.3\"\n";
    match crate::prepare(&root, fetched, "Cargo.toml", &identity(), &toolchain) {
        Err(PrepareError::Find(error)) => {
            assert!(
                error
                    .to_string()
                    .starts_with("extension `audit`: its source is version 1.2.3 of a registry")
            );
        }
        other => panic!("the registry source's refusal: {:?}", other.err()),
    }
}

#[test]
fn a_descriptor_blaze_cannot_run_is_refused_by_name() {
    fn nothing(_: &mut Cx<'_>, _: &Body<'_, '_>) {}
    const A: Lint =
        Lint { name: "a", level: Level::Warn, summary: "", requires: &[], run: nothing };
    const NEEDY: Lint = Lint {
        name: "needy",
        level: Level::Warn,
        summary: "",
        requires: &["absent"],
        run: nothing,
    };
    const SHOUTY: Lint =
        Lint { name: "Shouty", level: Level::Warn, summary: "", requires: &[], run: nothing };
    let key = ContentId::of("test extension", b"d");
    let why = |extension: Extension| match Loaded::linked(extension, key) {
        Err(LoadError::Descriptor { why, .. }) => why,
        Err(other) => panic!("a descriptor's refusal: {other}"),
        Ok(_) => panic!("refused"),
    };
    let base = Extension { name: "d", api: blaze_ext::VERSION, lints: &[], facts: &[] };
    assert_eq!(
        why(Extension { api: "9.9.9", ..base }),
        format!(
            "it was compiled against blaze-ext 9.9.9, and this blaze has blaze-ext {}",
            blaze_ext::VERSION
        )
    );
    assert_eq!(why(Extension { lints: &[A, A], ..base }), "the lint `a` is declared twice");
    assert_eq!(
        why(Extension { lints: &[NEEDY], ..base }),
        "the lint `needy` requires the fact `absent`, which the extension does not declare"
    );
    assert_eq!(
        why(Extension { lints: &[SHOUTY], ..base }),
        "the lint `Shouty`: a lint's name is a lowercase identifier"
    );
}

#[test]
fn an_image_that_cannot_be_placed_is_refused_and_nothing_of_it_runs() {
    let key = ContentId::of("test image", b"1");
    let past = build::Image {
        key,
        code_len: 16,
        data_len: 16,
        entry: 16,
        place: Box::new(|_, _, _| panic!("an image whose entry is past its code is never placed")),
    };
    match load::load(past, "x") {
        Err(LoadError::Entry { entry: 16, code_len: 16, .. }) => {}
        Err(other) => panic!("{other}"),
        Ok(_) => panic!("refused"),
    }
    let unplaced = build::Image {
        key,
        code_len: 16,
        data_len: 16,
        entry: 0,
        place: Box::new(|code, data, imports| {
            assert_eq!((code.len(), data.len()), (16, 16));
            assert!(
                imports.fuel_cell != 0 && imports.fuel_out != 0,
                "the fuel runtime's addresses are given"
            );
            Err("planted: a relocation out of reach".to_owned())
        }),
    };
    match load::load(unplaced, "x") {
        Err(error @ LoadError::Map { .. }) => assert_eq!(
            error.to_string(),
            "extension `x`: its code could not be mapped: planted: a relocation out of reach"
        ),
        Err(other) => panic!("{other}"),
        Ok(_) => panic!("refused"),
    }
}
