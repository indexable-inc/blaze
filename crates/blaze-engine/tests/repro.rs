//! Each failure blaze hit building a real crate, reduced to a minimal crate under
//! `tests/repro/<property>/` (by `tools/v2/repro/repro.py`) and a test named for the property it
//! broke. A test copies its crate out of the tree, runs this package's `blaze` on it with a
//! `BLAZE_HOME` of its own, and checks what makes the build right: it succeeds and the program
//! prints what the program says it prints.
//!
//! Every case builds std, which is compiled at opt-level 3, so only a blaze with LLVM can build
//! one: in the seed profile (no `native`) this target is empty.
#![cfg(all(unix, feature = "native"))]

use std::{
    fs,
    io::Read,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::atomic::{AtomicU32, Ordering},
    thread,
    time::{Duration, Instant},
};

/// Longer than any of these builds takes with the standard library built cold; a build still
/// running then is a hang, and fails the test instead of holding the run.
const DEADLINE: Duration = Duration::from_secs(900);

struct Case {
    root: PathBuf,
    dir: PathBuf,
    home: PathBuf,
    /// `XDG_CACHE_HOME` of every blaze it runs, under `root`: the machine's shared store
    /// (`<cache>/blaze`) is the case's own, never the real one.
    cache: PathBuf,
    /// Whether dropping it removes `root`: a [`Case::second_home`] shares its case's.
    owns_root: bool,
}

struct Out {
    ok: bool,
    stdout: String,
    stderr: String,
}

impl Case {
    fn new(property: &str) -> Case {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        // The daemon's socket is `<home>/v2.sock`, and a Unix socket's path is at most 104 bytes
        // on macOS: a `TMPDIR` such as nix-shell's is already most of that.
        let root = Path::new("/tmp").join(format!(
            "blaze-repro-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&root);
        let dir = root.join(property);
        let home = root.join("home");
        let cache = root.join("cache");
        copy_dir(&Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/repro").join(property), &dir);
        fs::create_dir_all(&home).unwrap();
        fs::create_dir_all(&cache).unwrap();
        Case { root, dir, home, cache, owns_root: true }
    }

    /// The same crate and machine cache under a `BLAZE_HOME` of its own: a second daemon on the
    /// machine.
    fn second_home(&self) -> Case {
        let home = self.root.join("home2");
        fs::create_dir_all(&home).unwrap();
        // clone: the case's paths, shared by the second home
        Case {
            root: self.root.clone(),
            dir: self.dir.clone(),
            home,
            cache: self.cache.clone(),
            owns_root: false,
        }
    }

    fn write(&self, path: &str, text: &str) {
        let path = self.dir.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    /// `blaze args` in `sub`, a directory of the case's crate (`""` for the crate itself). A
    /// case's crate is a throwaway copy, most with no lock file of their own, so every command
    /// may write one under CI too (`BLAZE_LOCK=write`, docs/cargo.md, "Lock files").
    fn blaze_in(&self, sub: &str, args: &[&str]) -> Out {
        let dir = self.dir.join(sub);
        let mut child = Command::new(env!("CARGO_BIN_EXE_blaze"))
            .env("BLAZE_LOCK", "write")
            .args(args)
            .current_dir(&dir)
            .env("BLAZE_HOME", &self.home)
            .env("XDG_CACHE_HOME", &self.cache)
            .env_remove("CARGO_TARGET_DIR")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let stdout = drain(child.stdout.take().unwrap());
        let stderr = drain(child.stderr.take().unwrap());
        let began = Instant::now();
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            if began.elapsed() > DEADLINE {
                child.kill().unwrap();
                child.wait().unwrap();
                self.kill_daemon();
                panic!(
                    "`blaze {}` in {} ran {} s without finishing: a hang\n{}",
                    args.join(" "),
                    dir.display(),
                    DEADLINE.as_secs(),
                    stderr.join().unwrap()
                );
            }
            #[allow(
                clippy::disallowed_methods,
                reason = "std has no timed wait on a child, and a daemon it starts may hold its pipes open past its exit"
            )]
            thread::sleep(Duration::from_millis(50));
        };
        Out { ok: status.success(), stdout: stdout.join().unwrap(), stderr: stderr.join().unwrap() }
    }

    /// `blaze args`, which must succeed; its stdout.
    fn ok(&self, args: &[&str]) -> String {
        self.ok_in("", args).stdout
    }

    /// `blaze args` in `sub`, which must succeed.
    fn ok_in(&self, sub: &str, args: &[&str]) -> Out {
        let out = self.blaze_in(sub, args);
        assert!(
            out.ok,
            "`blaze {}` in {} failed:\n{}\n(the daemon's log: {})",
            args.join(" "),
            self.dir.join(sub).display(),
            out.stderr,
            self.home.join("v2/serve.log").display()
        );
        out
    }

    fn stop(&self) {
        self.ok(&["stop"]);
    }

    /// The daemon `home`'s log last says is listening, killed: one a test left hung or broken.
    fn kill_daemon(&self) {
        let Ok(log) = fs::read_to_string(self.home.join("v2/serve.log")) else { return };
        let Some(pid) = log.rsplit("(pid ").next().and_then(|it| it.split(')').next()) else {
            return;
        };
        if pid.bytes().all(|b| b.is_ascii_digit()) && !pid.is_empty() {
            let _ = Command::new("kill").args(["-KILL", pid]).status();
        }
    }
}

impl Drop for Case {
    fn drop(&mut self) {
        let stopped = Command::new(env!("CARGO_BIN_EXE_blaze"))
            .arg("stop")
            .current_dir(&self.dir)
            .env("BLAZE_HOME", &self.home)
            .env("XDG_CACHE_HOME", &self.cache)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|it| it.success());
        if !stopped {
            self.kill_daemon();
        }
        if self.owns_root && !thread::panicking() {
            let _ = fs::remove_dir_all(&self.root);
        }
    }
}

fn drain(mut pipe: impl Read + Send + 'static) -> thread::JoinHandle<String> {
    thread::spawn(move || {
        let mut bytes = Vec::new();
        pipe.read_to_end(&mut bytes).unwrap();
        String::from_utf8_lossy(&bytes).into_owned()
    })
}

fn copy_dir(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for entry in fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let to = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_dir(&entry.path(), &to);
        } else {
            fs::copy(entry.path(), to).unwrap();
        }
    }
}

const AFTER: &str = "fn main() {\n    println!(\"after\");\n}\n";

/// The second build of a crate whose `main.rs` changed, on the daemon that built the first, made
/// its plan panic at `blaze-query/src/segment.rs:367` (`at + bytes.len() <= segment.cap`).
#[test]
fn an_edited_crate_rebuilds_on_one_daemon() {
    let case = Case::new("an_edited_crate_rebuilds_on_one_daemon");
    assert_eq!(case.ok(&["run"]), "before\n");
    case.write("src/main.rs", AFTER);
    assert_eq!(case.ok(&["run"]), "after\n");
}

/// The same crate after `blaze stop`, on the daemon started from the image the stop wrote.
#[test]
fn a_crate_rebuilds_after_its_daemon_restarts() {
    let case = Case::new("a_crate_rebuilds_after_its_daemon_restarts");
    assert_eq!(case.ok(&["run"]), "before\n");
    case.stop();
    case.write("src/main.rs", AFTER);
    assert_eq!(case.ok(&["run"]), "after\n");
}

/// The standard library is built once per daemon, not per workspace: a second workspace on the
/// daemon that built the first compiles its own package and nothing else (it has no
/// dependencies), so no `core`, `alloc`, `std`, `compiler_builtins`, `panic_*` or std's vendored
/// crates. It used to build std again for each workspace, a cold std's minutes each time.
#[test]
fn a_second_workspace_on_one_daemon_compiles_no_std_crate() {
    let case = Case::new("a_second_workspace_on_one_daemon_compiles_no_std_crate");
    assert_eq!(case.ok_in("one", &["run"]).stdout, "one\n");
    let second = case.ok_in("two", &["run"]);
    assert_eq!(second.stdout, "two\n");
    compiled_only(&second, "repro-two", "the second workspace");
    holds_no_std(&case, "one");
    holds_no_std(&case, "two");
}

/// Std is built once per machine: a second `BLAZE_HOME`'s daemon takes it from the machine's
/// shared store (`<XDG_CACHE_HOME>/blaze`) the first daemon put it in, and compiles only its
/// workspace's package.
#[test]
fn a_second_home_on_one_machine_compiles_no_std_crate() {
    let case = Case::new("a_second_home_on_one_machine_compiles_no_std_crate");
    assert_eq!(case.ok_in("one", &["run"]).stdout, "one\n");
    let other = case.second_home();
    let second = other.ok_in("two", &["run"]);
    assert_eq!(second.stdout, "two\n");
    compiled_only(&second, "repro-two", "the second home's daemon");
    holds_no_std(&case, "one");
    holds_no_std(&case, "two");
}

/// `out`'s `Compiling` lines are `package`'s alone: no std crate, vendored ones included.
fn compiled_only(out: &Out, package: &str, who: &str) {
    let compiled: Vec<&str> = out
        .stderr
        .lines()
        .filter_map(|line| line.trim_start().strip_prefix("Compiling "))
        .collect();
    assert!(
        compiled.len() == 1 && compiled[0].starts_with(&format!("{package} ")),
        "{who} compiled more than {package}:\n{}",
        compiled.join("\n")
    );
}

/// The workspace `sub`'s target directory holds none of std's outputs, which are the daemon's.
fn holds_no_std(case: &Case, sub: &str) {
    let deps = case.dir.join(sub).join("target/debug/deps");
    let core: Vec<String> = fs::read_dir(&deps)
        .into_iter()
        .flatten()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with("libcore-"))
        .collect();
    assert!(
        core.is_empty(),
        "std's outputs are the daemon's, yet {} holds {core:?}",
        deps.display()
    );
}

/// A registry crate's build script (zmij 1.0.21's) was run before its link wrote it: `could not
/// run .../build/zmij/<hash>/out/build_script_build: No such file or directory`, the link of
/// that path reported after. A package's own script and a path dependency's ran.
#[test]
fn a_registry_dependencys_build_script_runs() {
    let case = Case::new("a_registry_dependencys_build_script_runs");
    assert_eq!(case.ok(&["run"]), "1.5\n");
}

/// The crates.io copy of a crate std vendors (object 0.39.1), as a dependency: once a
/// `CrateKeyClash` of the two build scripts, now the registry script run of
/// [`a_registry_dependencys_build_script_runs`].
#[test]
fn a_dependency_std_also_uses_builds() {
    let case = Case::new("a_dependency_std_also_uses_builds");
    assert_eq!(case.ok(&["run"]), "Little\n");
}

/// A bin over a path dependency with a `thread_local!` whose value has a `Drop` (blaze's own
/// `blaze-reclaim`): its release imports are decided from the build's summaries, and one each
/// of 16 functions imports is called by none of the bodies it builds in ("imports <id>, which
/// nothing it builds calls"), the self-build's wall.
#[test]
fn a_bin_using_a_path_dependencys_thread_local_builds() {
    let case = Case::new("a_bin_using_a_path_dependencys_thread_local_builds");
    assert_eq!(case.ok(&["run"]), "true\n");
}

/// `simd_lt` and `simd_ge` are intrinsics of one signature, and a function's interface left
/// an intrinsic's name out: `vcltz_s8` and `vcgez_s8` (hashbrown's `match_empty_or_deleted`
/// and `match_full` on aarch64) had one code hash and shared one compiled body, so a
/// `HashSet`'s first insert probed past the end of its table.
#[test]
fn two_simd_compares_of_one_signature_keep_their_own_code() {
    let case = Case::new("two_simd_compares_of_one_signature_keep_their_own_code");
    assert_eq!(case.ok(&["run"]), "00010101 11101010 3\n");
}

/// Two `and_then`s over closures of one signature in a body with no generics: the share key
/// took a closure callee by its parent's arguments alone, so both instances were one and the
/// second ran the first closure (rustc_apfloat's build script: "expected `src/lib.rs` to have
/// more than 3 lines"), and std's `DebugList::finish` ran `DebugTuple::finish`'s closure (libm's
/// build script faulted printing a `Vec<String>`).
#[test]
fn two_closures_of_one_signature_keep_their_own_bodies() {
    let case = Case::new("two_closures_of_one_signature_keep_their_own_bodies");
    assert_eq!(case.ok(&["run"]), "Ok(\"a\")\n[\"x\"] (\"y\",)\n");
}

/// cc 1.4.3's in-process jobserver, which a build script without cargo's jobserver takes a token
/// of before it runs the C compiler: `fetch_update(AcqRel, Acquire, |t| t.checked_sub(1))` on
/// an `AtomicU32` of `NUM_JOBS`, retried after a `YieldOnce` on a no-op waker, and given back
/// when `retain_mut` drops a finished job's token. ring's and zstd-sys's build scripts, built by
/// blaze, compiled `NUM_JOBS` objects and then slept in cc's `block_on` forever (queue 01523):
/// the MIR inliner inlined `drop_in_place`'s `drop_glue`, a placeholder, so `retain_mut`
/// dropped nothing it removed. Each step is checked on its own, in the build script and in the
/// program.
#[test]
fn a_build_script_takes_a_token_of_cc_s_in_process_jobserver() {
    let case = Case::new("a_build_script_takes_a_token_of_cc_s_in_process_jobserver");
    let said = "parse Ok(true) sub Some(17) zero None update Ok(18) left 17 exchange Ok(17) left 16 \
                yield (Some(7), 2) acquire (Some(18), 1) none left (None, 100) released 3";
    assert_eq!(case.ok(&["run"]), format!("script: {said}\nmain: {said}\n"));
}

/// A crate indexing a `[u8]` by a `Range`, and foldhash 0.2.0 at `opt-level = 3` under a
/// `Hasher` of its own, as blaze-hash is. The walk listed what core's `#[inline(always)]`
/// callees build in at the walk's settings; at the unit's, `<[u8] as Index<Range>>::index`
/// called `<Range<usize> as SliceIndex<[u8]>>::index` and foldhash's `hash_bytes_long` called
/// `check_language_ub::runtime` instead, which no reference listed, so blaze-hash and
/// blake3-servil did not build. A callee built in at any depth is now listed too.
#[test]
fn a_crate_indexing_bytes_by_a_range_builds() {
    let case = Case::new("a_crate_indexing_bytes_by_a_range_builds");
    assert_eq!(case.ok(&["run"]), "[1, 2]\nequal true distinct true\n");
}

/// A build script that prints a `Vec<String>` with `{:?}`, as libm 0.2.16's `configure.rs`
/// does: std's `DebugList::finish` and `DebugTuple::finish` each give `Result::and_then` a
/// closure of one signature, the share key took them as one callee, and the list was finished
/// by the tuple's closure, which read its `Formatter` as a `DebugTuple` and faulted (exit 139).
#[test]
fn a_build_script_prints_a_debug_list() {
    let case = Case::new("a_build_script_prints_a_debug_list");
    assert_eq!(case.ok(&["run"]), "[\"default\"] (\"neon\",)\n");
}

/// A build script that declares the cfgs it sets (`cargo::rustc-check-cfg=cfg(from_build)`, as
/// every crate whose build script probes the compiler does): the check was given only the
/// plan's `--check-cfg`s, never the run's, so each `#[cfg(from_build)]` warned
/// "unexpected `cfg` condition name: `from_build`" (harness/editmacros.py, 2026-11-02).
#[test]
fn a_cfg_its_build_script_declares_is_expected() {
    let case = Case::new("a_cfg_its_build_script_declares_is_expected");
    let out = case.ok_in("", &["run"]);
    assert_eq!(out.stdout, "declared\n");
    assert!(
        !out.stderr.contains("unexpected `cfg`"),
        "a cfg the build script declares warned:\n{}",
        out.stderr
    );
}

/// `str::split_ascii_whitespace` (cc's build-time flag parsing, so every build script using cc):
/// core's `impl_fn_for_zst!` writes `impl Fn<(&u8,)> for IsAsciiWhitespace` with an elided
/// lifetime in its header, and `SplitAsciiWhitespace::next`, `size_hint` and
/// `split_ascii_whitespace` failed to lower (`UnresolvedMethod("next" | "size_hint" | "map")`).
#[test]
fn a_program_splitting_on_ascii_whitespace_builds() {
    let case = Case::new("a_program_splitting_on_ascii_whitespace_builds");
    assert_eq!(case.ok(&["run"]), "[\"-O2\", \"-g\", \"-Wall\"]\n");
}

/// A promoted temporary with its place in the source inside it, as expect-test's
/// `expect![..].assert_eq(..)` and `&expect![..]` make one: `&Here { file: file!(), line: line!() }`.
/// Its line is read where the code runs, so the function writes it into writable data of its
/// own each call (`promoted_slot`), which v2 refused: "data a function writes to, which is no
/// constant (`promoted_slot`)" for parser's and blaze-graph's expect tests (B6). The one `here`
/// returns outlives its call, and is read after `main` has called on.
#[test]
fn a_promoted_temporary_with_its_place_in_it_runs() {
    let case = Case::new("a_promoted_temporary_with_its_place_in_it_runs");
    assert_eq!(case.ok(&["run"]), "main.rs:16\nmain.rs:11\n");
}

/// sqlx's `#[sqlx::test(migrations = "..")]` passes `&Migrator { migrations: Cow::Borrowed(const
/// { &[..] }), .., ..Migrator::DEFAULT }` where a `&'static Migrator` is wanted: a record literal
/// with a constant base, and a `const { .. }` block, are promoted, as rustc promotes them. blaze
/// promoted neither, so its borrow checker refused every such test (E0716). Each borrow here
/// outlives its function and is read after a call has written over that frame.
#[test]
fn a_record_literal_with_a_constant_base_is_promoted() {
    let case = Case::new("a_record_literal_with_a_constant_base_is_promoted");
    assert_eq!(
        case.ok(&["run"]),
        "Migrator { migrations: [1, 2, 3], locking: true, table: \"_migrations\" }\n\
         Migrator { migrations: [], locking: true, table: \"_migrations\" }\n\
         State { state: Empty, n: 7 }\n\
         None\n\
         0\n"
    );
}

/// object's `ImageSymbol::name`, a provided trait method generic over its reader, reached
/// through `CoffFile` (psm's build script, through ar_archive_writer): its references held a
/// call of a trait method `distance` resolved to no impl, so the function was refused.
#[test]
fn a_program_reading_coff_symbol_names_builds() {
    let case = Case::new("a_program_reading_coff_symbol_names_builds");
    assert_eq!(case.ok(&["run"]), "Invalid COFF file header size or alignment\n");
}

/// cc's `is_flag_supported` caches into a `HashMap<CompilerFlag, bool>`: hashbrown's
/// `reserve_rehash` closure for it went in the dependency image, and the `<*mut u8>::cast` it
/// calls was taken for the workspace's, so the build refused ("the image would change with
/// workspace code"; libmimalloc-sys' and blake3's build scripts).
#[test]
fn a_build_script_asking_cc_whether_a_flag_is_supported_builds() {
    let case = Case::new("a_build_script_asking_cc_whether_a_flag_is_supported_builds");
    assert_eq!(case.ok(&["run"]), "asked\n");
}

/// A crate both a build script and the program depend on, lowered alike (indexmap's
/// `equivalent`, a build-dependency too): blaze-lower makes one front-end crate of the two
/// units, and the code generator took the crate for build-time code in the program's build, so
/// indexmap's `get_index_of`, in the dependency image, referred to `<str as
/// Equivalent<String>>::equivalent` as the workspace's and the build refused ("the image would
/// change with workspace code"; toml, parking_lot_core, getrandom in blaze's self-build).
///
/// Known red until the template-key blocker is fixed (G3 in the self-build survey, lane
/// sb-tplkey; the refusal of #1612): it builds past the dependency image and stops at "built in
/// the code .. which its template's key .. does not hold" (hashbrown's `reserve_rehash`). A
/// failure there is that blocker, not a regression of this one. A `blaze run` of a program not
/// over blaze's runtime links no dependency image, so no code here is split; the ownership a
/// live build splits by is pinned in hir-ty-codegen (`a_registry_crate_over_a_path_crate_is_the_workspaces`).
#[test]
fn a_crate_a_build_script_and_the_program_share_is_the_programs() {
    let case = Case::new("a_crate_a_build_script_and_the_program_share_is_the_programs");
    assert_eq!(case.ok(&["run"]), "Some(1) None\n");
}

/// A registry crate over a `[patch.crates-io]`d path crate (indexmap over a path `equivalent`;
/// zstd-safe over blaze's vendored zstd-sys): the registry crate's code was the dependency
/// image's while the path crate's, which it calls, was the workspace's, so the build refused
/// ("the image would change with workspace code"). Code over a crate an edit reaches is the
/// workspace's too.
///
/// Known red until the template-key blocker is fixed (G3 in the self-build survey, lane
/// sb-tplkey; the refusal of #1612): it builds past the dependency image and stops at "built in
/// the code .. which its template's key .. does not hold" (hashbrown's `reserve_rehash`). A
/// failure there is that blocker, not a regression of this one. A `blaze run` of a program not
/// over blaze's runtime links no dependency image, so no code here is split; the ownership a
/// live build splits by is pinned in hir-ty-codegen (`a_registry_crate_over_a_path_crate_is_the_workspaces`).
#[test]
fn a_registry_crate_over_a_patched_path_crate_builds() {
    let case = Case::new("a_registry_crate_over_a_patched_path_crate_builds");
    assert_eq!(case.ok(&["run"]), "Some(1) None\n");
}

/// An `opt-level = 3` caller that builds in `#[inline(always)] dep::resize::<K>`, which calls
/// through the `&dyn Fn` it is given (hashbrown's `resize_inner` in `reserve_rehash_inner`):
/// the callee's own unit is built against the program's dyn facts, so its code hashes what it
/// read of them, while the copy built into `main` is built against none. The compile named it
/// by its plain code, the key held its own, and the build refused `main` ("built in the code
/// .. which its template's key .. does not hold"; blaze-query-macros in blaze's self-build).
#[test]
fn an_opt3_caller_building_in_a_dyn_fn_callee_builds() {
    let case = Case::new("an_opt3_caller_building_in_a_dyn_fn_callee_builds");
    assert_eq!(case.ok(&["run"]), "43\n");
}

/// arrow-buffer's `BitIndexIterator`: a library's `#[inline(always)]` `next` with a `?` on an
/// `Option`, built by the code generator into core's `fold`, `try_fold` and `Map::next` of an
/// `opt-level = 0` bin. The copy is built at its crate's level, where the `?`'s
/// `FromResidual::from_residual` stays a call; the compile named what it built in by walking
/// `next` again as a unit that MIR-inlines walks it, which inlines `from_residual`, and the build
/// refused all three ("`fold` built in the code .. (`from_residual`), which its template's key
/// .. does not hold"; arrow in ix-prof). The compile now names the bodies it built in itself.
/// `bits` comes from a directory source: only a library's `#[inline(always)]` bodies are built in.
#[test]
fn an_always_inline_next_with_a_question_mark_built_into_fold_builds() {
    let case = Case::new("an_always_inline_next_with_a_question_mark_built_into_fold_builds");
    assert_eq!(case.ok(&["run"]), "195 4 None\n");
}

/// The same callee built into a caller at `opt-level = 0`, where the code generator builds it
/// in itself (`built_in_body`) rather than the MIR inliner, while another package at level 3
/// makes the build walk every generic for inlining (blaze's own workspace's shape).
#[test]
fn an_opt0_caller_building_in_a_dyn_fn_callee_builds() {
    let case = Case::new("an_opt0_caller_building_in_a_dyn_fn_callee_builds");
    assert_eq!(case.ok(&["run"]), "43\n");
}

/// The dyn callee reaches the caller through another callee: `main` inlines
/// `dep::outer::<K>`, which builds in `dep::resize::<K>`, as hashbrown's `reserve_rehash`
/// reaches `resize_inner` through `reserve_rehash_inner`.
#[test]
fn a_dyn_fn_callee_built_in_through_another_builds() {
    let case = Case::new("a_dyn_fn_callee_built_in_through_another_builds");
    assert_eq!(case.ok(&["run"]), "43\n");
}

/// A path dependency whose enum has a variant, and a macro's match an arm, under a `cfg` that is
/// off (object's `#[cfg(feature = "elf")]` variants in `read/any.rs`): the names check of a
/// local crate resolved the disabled variant's path and reported E0433 for its module.
#[test]
fn a_crate_with_cfg_disabled_variants_builds() {
    let case = Case::new("a_crate_with_cfg_disabled_variants_builds");
    assert_eq!(case.ok(&["run"]), "1\n");
}

/// A bin over crates.io's libc (0.2.191, which `#[link(name = "iconv")]`s on Apple targets): on
/// the macOS 27 SDK `libiconv.tbd` re-exports `/usr/lib/libcharset.1.dylib`, and the link
/// refused it as a library "the link line does not have", so no program over libc linked.
#[test]
fn a_program_calling_libc_links() {
    let case = Case::new("a_program_calling_libc_links");
    assert_eq!(case.ok(&["run"]), "true\n");
}

/// A program reading an `extern_weak` static, with a call through a `dyn`: the program's dyn
/// facts asked the static's value of `compiled_static`, which has none for a static of an
/// `extern` block ("extern static without a rustc symbol"), so no executable that reached one
/// built. psm's and llvm-sys's build scripts reach std's (`__cxa_thread_atexit_impl`, a
/// thread-local's destructor on Linux), which blocked 75 packages of blaze's self-build. A
/// weak static holds a foreign symbol's address, which makes no vtable.
#[test]
fn a_program_reading_an_extern_weak_static_builds() {
    let case = Case::new("a_program_reading_an_extern_weak_static_builds");
    assert_eq!(case.ok(&["run"]), "true\n");
}

/// serde_derive's `Deserialize`, run on blaze's evaluator: every derive of the 00:49 self-build
/// stopped building the MIR of the bridge's `run_client` closure (`MissingFunctionDefinition`),
/// 26 of them across blaze-protocol, blaze-core, rustc_abi and paths. The struct it derives
/// reads its fields in any order and defaults the one it is not given.
#[test]
fn a_struct_deriving_deserialize_reads_its_fields() {
    let case = Case::new("a_struct_deriving_deserialize_reads_its_fields");
    assert_eq!(case.ok(&["run"]), "3 -4 None\n");
}

/// blaze-serde-derive, the derive blaze's crates use in serde_derive's place, run on blaze's
/// evaluator: its impls build and read a struct; a `#[serde]` attribute it does not read stops
/// the build with an error naming the attribute, the item and the place, never with an impl
/// that means something else.
#[test]
fn a_serde_attribute_blaze_serde_derive_lacks_is_an_error_naming_it() {
    let case = Case::new("a_serde_attribute_blaze_serde_derive_lacks_is_an_error_naming_it");
    let facade = Path::new(env!("CARGO_MANIFEST_DIR")).join("../blaze-serde");
    case.write(
        "Cargo.toml",
        &format!(
            "[package]\nname = \"repro\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[dependencies]\n\
             blaze-serde = {{ path = {:?} }}\n\n[workspace]\n",
            facade.display().to_string()
        ),
    );
    assert_eq!(case.ok(&["run"]), "3 -4 None\n");
    case.write(
        "src/main.rs",
        "#[derive(blaze_serde::Serialize)]\n#[serde(rename = \"other\")]\nstruct Renamed {\n    a: u8,\n}\n\nfn main() {}\n",
    );
    let out = case.blaze_in("", &["build"]);
    assert!(!out.ok, "a derive with an attribute it does not read built:\n{}", out.stderr);
    let said = "`#[serde(rename = \"other\")]` on the container `Renamed` (";
    assert!(
        out.stderr.contains(said),
        "the error does not name the attribute and item:\n{}",
        out.stderr
    );
    assert!(out.stderr.contains("main.rs:2:"), "the error does not say where:\n{}", out.stderr);
}

/// `#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]` on a tuple struct with a const
/// parameter (blaze-mem's `swiss::Ctrl`): the lint pass warned "unreachable pattern ... no value
/// can reach this" at the derive, on code blaze itself wrote, and on llvm-sys's `include!`.
#[test]
fn a_derived_impl_warns_of_no_unreachable_pattern() {
    let case = Case::new("a_derived_impl_warns_of_no_unreachable_pattern");
    let out = case.ok_in("", &["run"]);
    assert_eq!(out.stdout, "Ctrl(3) false\n");
    assert!(!out.stderr.contains("unreachable pattern"), "a derive warned:\n{}", out.stderr);
}

/// A workspace setting blaze's own lint (`[workspace.metadata.blaze.lints] hot_alloc`, as
/// blaze's root manifest does): every member was told `unknown lint: blaze::hot_alloc` (E0602).
#[test]
fn blazes_own_lint_in_a_workspaces_lints_is_known() {
    let case = Case::new("blazes_own_lint_in_a_workspaces_lints_is_known");
    let out = case.ok_in("", &["run"]);
    assert_eq!(out.stdout, "linted\n");
    assert!(!out.stderr.contains("unknown lint"), "blaze's own lint was unknown:\n{}", out.stderr);
}

/// A proc-macro package's lib, with a `#[test]` of its own, and an integration test that
/// expands its macro (01463): the lib and its lib test were one root text, so the lib test's
/// spans named bytes the lib's text lacks and the daemon panicked.
#[test]
fn a_proc_macro_package_tests_build() {
    let case = Case::new("a_proc_macro_package_tests_build");
    let out = case.ok_in("", &["test"]);
    let said = format!("{}{}", out.stdout, out.stderr);
    for test in ["the_lib_test_runs", "the_macro_expands"] {
        assert!(said.contains(&format!("test {test} ... ok")), "`{test}` did not pass:\n{said}");
    }
}

/// `#![no_std]` proc-macro crates of every edition, an attribute macro among them (as
/// multiversion_no_op's), and one that declares its own `extern crate std`, each built as a
/// wasm module and expanded: the glue blaze appends to a macro crate's root names std only
/// through its own `extern crate std as __blaze_std`, so a crate whose extern prelude has no
/// `std`, or that already declares one, builds.
#[test]
fn a_no_std_proc_macro_expands() {
    let case = Case::new("a_no_std_proc_macro_expands");
    let out = case.ok_in("", &["test"]);
    let said = format!("{}{}", out.stdout, out.stderr);
    assert!(
        said.contains("test every_no_std_macro_expands ... ok"),
        "`every_no_std_macro_expands` did not pass:\n{said}"
    );
}

/// `#[blaze::contract_test]` (docs/v2/plan/lanes/contracts.md, part 2): a function whose
/// `ensures` is wrong for one input gets a generated Hegel test that fails, naming the function
/// and the clause, with the shrunk case as a test to paste; a contract that holds passes, and
/// `skip` generates nothing. The crate's manifest names `vendor/hegeltest` by its absolute path,
/// and blaze's own lock pins hegeltest's registry dependencies to the versions blaze builds.
#[test]
fn generated_test_finds_planted_bug() {
    let case = Case::new("generated_test_finds_planted_bug");
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    case.write(
        "Cargo.toml",
        &format!(
            "[package]\nname = \"repro\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[dev-dependencies]\n\
             hegeltest = {{ path = {:?}, default-features = false, features = [\"static-engine\"] }}\n\n\
             [profile.test]\ncontract-checks = false\n\n[workspace]\n",
            root.join("vendor/hegeltest").display().to_string()
        ),
    );
    case.write("Cargo.lock", &fs::read_to_string(root.join("Cargo.lock")).unwrap());
    let out = case.blaze_in("", &["test"]);
    let said = format!("{}{}", out.stdout, out.stderr);
    assert!(!out.ok, "the planted bug was not found:\n{said}");
    assert!(
        said.contains("test double_contract ... FAILED"),
        "`double`'s test did not fail:\n{said}"
    );
    assert!(said.contains("test count_contract ... ok"), "`count`'s test did not pass:\n{said}");
    assert!(!said.contains("halve_contract"), "`skip` generated a test:\n{said}");
    let named = "`#[blaze::contract_test]` on `double`: `ensures r > x` does not hold";
    assert!(said.contains(named), "the failure does not name the function and clause:\n{said}");
    for line in [
        "fn double_contract_case() {",
        "    let x: u32 = 0;",
        "    let r = double(x.clone());",
        "    assert!(r > x);",
    ] {
        assert!(said.contains(line), "the case to paste lacks `{line}`:\n{said}");
    }
}
