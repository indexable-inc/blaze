//! Each failure blaze hit building a real crate, reduced to a minimal crate under
//! `tests/repro/<property>/` (by `tools/v2/repro/repro.py`) and a test named for the property it
//! broke. A test copies its crate out of the tree, runs this package's `blaze` on it, and checks
//! what makes the build right: it succeeds and the program prints what the program says it
//! prints.
//!
//! A case that only builds its crate and checks what that prints ([`Case::new`]) runs on the
//! one daemon of this test binary's run: one `BLAZE_HOME` and one `XDG_CACHE_HOME` that every
//! test process of the binary finds by itself ([`shared_root`]), so std is built once a run, not
//! once a case, and the machine runs one daemon for them, not one each (44 at once took an
//! 18-core Mac to a load of 200 on 2026-11-13). A case that tests the daemon's own life (it
//! stops or kills it, starts a second home, reads its log, or asserts on what the daemon built
//! before it) has a home of its own ([`Case::own`]). Every daemon here is started with
//! `BLAZE_DAEMON_EXIT_UNHELD` ([`UNHELD`]), so it exits by itself once no test holds a
//! connection to it, whether or not the tests that used it ended well: nextest kills a test
//! at its time cap before its `Drop` runs.
//!
//! Every case builds std, which is compiled at opt-level 3, so only a blaze with LLVM can build
//! one: in the seed profile (no `native`) this target is empty.
#![cfg(all(unix, feature = "native"))]
// `#[expect(blaze::..)]` names blaze's own lints; inert under rustc.
#![feature(register_tool)]
#![register_tool(blaze)]

use std::{
    fs,
    io::Read,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::atomic::{AtomicU32, Ordering},
    thread,
    time::{Duration, Instant, UNIX_EPOCH},
};

/// Longer than any of these builds takes with the standard library built cold; a build still
/// running then is a hang, and fails the test instead of holding the run.
const DEADLINE: Duration = Duration::from_secs(900);

/// What every daemon these tests start is started with: it exits once no connection has been
/// open at its socket for its grace (`crates/blaze-cargo/src/serve/unheld.rs`), so a run that
/// ended, or was killed, leaves none.
const UNHELD: (&str, &str) = ("BLAZE_DAEMON_EXIT_UNHELD", "on");

/// The longest a Unix socket's path may be on macOS (`sun_path`), its terminating zero
/// included.
const SOCKET_MOST: usize = 104;

/// The directory every case on the shared daemon is under, the same in every process of this
/// test binary and of no other build of it: `/tmp/blaze-repro-<hash>`, the hash of the
/// binary's path, modification time and length. nextest runs each test in a process of its
/// own, and these agree on it without talking to each other. Under `/tmp`, not `TMPDIR`: the
/// daemon's socket, `<home>/v2.sock`, must stay under [`SOCKET_MOST`] bytes, and a `TMPDIR` such
/// as nix-shell's is most of that already.
#[expect(
    blaze::tmp_path,
    reason = "the shared root is one fixed path every test process of the run finds by itself and outlives each of them (swept by `sweep_past_runs`), so no process's TempDir can own it; under `/tmp` for the daemon socket's length"
)]
fn shared_root() -> PathBuf {
    let exe = std::env::current_exe().unwrap();
    let meta = fs::metadata(&exe).unwrap();
    let modified = meta.modified().unwrap().duration_since(UNIX_EPOCH).unwrap().as_nanos();
    // FNV-1a: no dependency, and stable for a binary as long as the binary is.
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    let (modified, len) = (modified.to_le_bytes(), meta.len().to_le_bytes());
    let bytes = exe.as_os_str().as_encoded_bytes().iter().chain(&modified).chain(&len);
    for byte in bytes {
        hash = (hash ^ u64::from(*byte)).wrapping_mul(0x0100_0000_01b3);
    }
    Path::new("/tmp").join(format!("blaze-repro-{hash:016x}"))
}

/// Where the run's builds are served: `[serve] host` of the `config.toml` of the blaze home this
/// test process runs under (`blaze_thin::host`, the one setting a command reads), so the same
/// cases run against a daemon or in each command's own process by that one setting and no
/// other. Each shared case's home is given the same (`served_as_this_run_is`); a case with a
/// home of its own ([`Case::own`]) tests a daemon's own life and keeps the daemon.
fn serve_host() -> blaze_thin::host::Host {
    static HOST: std::sync::OnceLock<blaze_thin::host::Host> = std::sync::OnceLock::new();
    *HOST.get_or_init(|| {
        let Ok(home) = blaze_manifest::user_config::home() else { return blaze_thin::host::Host::Daemon };
        let file = blaze_manifest::user_config::file(&home);
        match fs::read_to_string(&file) {
            Ok(text) => blaze_thin::host::Host::from_toml(&text).unwrap_or_else(|why| panic!("{}: {why}", file.display())),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => blaze_thin::host::Host::Daemon,
            Err(error) => panic!("{} can't be read: {error}", file.display()),
        }
    })
}

/// `home`'s `config.toml` says this run's host ([`serve_host`]); nothing is written for the
/// daemon, the default.
fn served_as_this_run_is(home: &Path) {
    let host = serve_host();
    if host != blaze_thin::host::Host::Daemon {
        let text = format!("[{}]\n{} = \"{}\"\n", blaze_thin::host::SECTION, blaze_thin::host::KEY, host.word());
        fs::write(blaze_manifest::user_config::file(home), text).unwrap();
    }
}

/// How long a past run's shared root ([`shared_root`]) is kept with no case made or removed in
/// it and no daemon at its socket: then it is another build's of this binary, done.
const PAST_RUN_KEPT: Duration = Duration::from_secs(60 * 60);

/// Every past run's shared root but `current` removed: one whose daemon has exited (nothing
/// listens at its socket) and whose `cases` has not changed for [`PAST_RUN_KEPT`]. Each build of
/// the test binary has a root of its own, which holds a built std, so without this every build
/// would leave one. Once a process.
#[expect(
    blaze::tmp_path,
    reason = "lists the real `/tmp`, where `shared_root` puts every run's shared root, to remove the past runs' ones"
)]
fn sweep_past_runs(current: &Path) {
    static SWEPT: std::sync::Once = std::sync::Once::new();
    SWEPT.call_once(|| {
        let Ok(entries) = fs::read_dir("/tmp") else { return };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name();
            let Some(hash) = name.to_str().and_then(|it| it.strip_prefix("blaze-repro-")) else { continue };
            if path == current || hash.len() != 16 || !hash.bytes().all(|b| b.is_ascii_hexdigit()) {
                continue;
            }
            if std::os::unix::net::UnixStream::connect(path.join("home/v2.sock")).is_ok() {
                continue;
            }
            let quiet = fs::metadata(path.join("cases"))
                .or_else(|_| fs::metadata(&path))
                .and_then(|it| it.modified())
                .is_ok_and(|at| at.elapsed().is_ok_and(|it| it > PAST_RUN_KEPT));
            if quiet {
                let _ = fs::remove_dir_all(&path);
            }
        }
    });
}

struct Case {
    root: PathBuf,
    dir: PathBuf,
    home: PathBuf,
    /// `XDG_CACHE_HOME` of every blaze it runs: the machine's shared store (`<cache>/blaze`) is
    /// the run's ([`Case::new`]) or the case's own ([`Case::own`]), never the real one.
    cache: PathBuf,
    /// Whether dropping it removes `root`: a [`Case::second_home`] shares its case's.
    owns_root: bool,
    /// Whether its daemon is its own, which dropping it stops: not the shared one, which serves
    /// the run's other cases and exits once none holds it.
    owns_daemon: bool,
}

struct Out {
    ok: bool,
    stdout: String,
    stderr: String,
}

/// A number no other case of this process has, for its directory's name.
fn next() -> u32 {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

impl Case {
    /// A case on the run's one daemon ([`shared_root`]): its crate copied into a directory of
    /// its own, which is all it removes as it drops.
    fn new(property: &str) -> Case {
        let shared = shared_root();
        sweep_past_runs(&shared);
        let (home, cache) = (shared.join("home"), shared.join("cache"));
        let socket = home.join("v2.sock");
        assert!(
            socket.as_os_str().len() < SOCKET_MOST,
            "the shared daemon's socket {} is {} bytes, past macOS's {SOCKET_MOST}",
            socket.display(),
            socket.as_os_str().len()
        );
        let root = shared.join("cases").join(format!("{}-{}", std::process::id(), next()));
        let _ = fs::remove_dir_all(&root);
        let dir = root.join(property);
        copy_dir(&Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/repro").join(property), &dir);
        fs::create_dir_all(&home).unwrap();
        served_as_this_run_is(&home);
        fs::create_dir_all(&cache).unwrap();
        Case { root, dir, home, cache, owns_root: true, owns_daemon: false }
    }

    /// A case with a `BLAZE_HOME` and machine cache of its own, so a daemon of its own: for a
    /// test of the daemon's own life, or of what one daemon built before.
    #[expect(
        blaze::tmp_path,
        reason = "a case's root is kept when its test panics, for the failure to be read, which a TempDir (removed on every drop) cannot do; under `/tmp` for the daemon socket's length"
    )]
    fn own(property: &str) -> Case {
        // The daemon's socket is `<home>/v2.sock`, and a Unix socket's path is at most 104 bytes
        // on macOS: a `TMPDIR` such as nix-shell's is already most of that.
        let root = Path::new("/tmp").join(format!("blaze-repro-{}-{}", std::process::id(), next()));
        let _ = fs::remove_dir_all(&root);
        let dir = root.join(property);
        let home = root.join("home");
        let cache = root.join("cache");
        copy_dir(&Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/repro").join(property), &dir);
        fs::create_dir_all(&home).unwrap();
        fs::create_dir_all(&cache).unwrap();
        Case { root, dir, home, cache, owns_root: true, owns_daemon: true }
    }

    /// The same crate and machine cache under a `BLAZE_HOME` of its own: a second daemon on the
    /// machine. Of a case with a home of its own.
    fn second_home(&self) -> Case {
        assert!(self.owns_daemon, "a second home is of a case with a home of its own (`Case::own`)");
        let home = self.root.join("home2");
        fs::create_dir_all(&home).unwrap();
        // clone: the case's paths, shared by the second home
        Case {
            root: self.root.clone(),
            dir: self.dir.clone(),
            home,
            cache: self.cache.clone(),
            owns_root: false,
            owns_daemon: true,
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
            .env(UNHELD.0, UNHELD.1)
            .env_remove("CARGO_TARGET_DIR")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map(stdx::JodChild)
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
                // The shared daemon serves the run's other cases: it is left to them, and exits
                // once none holds it.
                if self.owns_daemon {
                    self.kill_daemon();
                }
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
        assert!(self.owns_daemon, "only a case with a home of its own (`Case::own`) stops its daemon");
        self.ok(&["stop"]);
    }

    /// The daemon `home`'s log last says is listening, killed: one a test left hung or broken.
    /// Of a case with a home of its own.
    fn kill_daemon(&self) {
        assert!(self.owns_daemon, "only a case with a home of its own (`Case::own`) kills its daemon");
        let Ok(log) = fs::read_to_string(self.home.join("v2/serve.log")) else { return };
        let Some(pid) = log.rsplit("(pid ").next().and_then(|it| it.split(')').next()) else {
            return;
        };
        if pid.bytes().all(|b| b.is_ascii_digit()) && !pid.is_empty() {
            let _ = Command::new("kill").args(["-KILL", pid]).status();
        }
    }
}

/// A case on the shared daemon removes its own directory alone and never stops the daemon,
/// which serves the run's other cases and exits once none holds it. One with a home of its own
/// stops its daemon, killing it if the stop fails, then removes its root.
impl Drop for Case {
    fn drop(&mut self) {
        if self.owns_daemon {
            let stopped = Command::new(env!("CARGO_BIN_EXE_blaze"))
                .arg("stop")
                .current_dir(&self.dir)
                .env("BLAZE_HOME", &self.home)
                .env("XDG_CACHE_HOME", &self.cache)
                .env(UNHELD.0, UNHELD.1)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .is_ok_and(|it| it.success());
            if !stopped {
                self.kill_daemon();
            }
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
    // A home of its own: it stops its daemon.
    let case = Case::own("a_crate_rebuilds_after_its_daemon_restarts");
    assert_eq!(case.ok(&["run"]), "before\n");
    case.stop();
    case.write("src/main.rs", AFTER);
    assert_eq!(case.ok(&["run"]), "after\n");
}

/// The pid of the daemon `case`'s log last says is listening.
fn daemon_pid(case: &Case) -> Option<String> {
    let log = fs::read_to_string(case.home.join("v2/serve.log")).ok()?;
    let pid = log.rsplit("listening on ").next()?.split("(pid ").nth(1)?.split(')').next()?;
    (!pid.is_empty() && pid.bytes().all(|b| b.is_ascii_digit())).then(|| pid.to_owned())
}

/// The longest the first request after a restart that sees a changed file may take here: the
/// 45 s an agent's loop gave it on 2026-11-11 was the bound it missed; a release blaze answers
/// in about 8 s, an unoptimised one is slower.
const RESTART_EDIT_BOUND: Duration = Duration::from_secs(120);

/// The first request after a restart that sees a changed source file answers within its
/// bound, prints what a fresh home's build of the same source prints, and the daemon caught
/// no panic on the way: after a `blaze stop`, and after a `kill -9`. On 5c2f8b2ec8 it did not
/// answer in 45 s, three times of three: the restarted daemon's first build of the edit
/// writes the input fields of every library crate again (4,722 of them, which 551,000 rows
/// read), the changed-set walk verified each row it reached on the pool, and each row keyed
/// by a module the def maps no longer had panicked as it ran (40,903 caught panics, each a
/// stack written to the log). The walk now marks a wide level and contains at most 8 unwinds
/// (`blaze_query`'s `WIDE` and `UNWOUND_MOST`).
#[test]
fn an_edit_after_a_restart_answers_within_its_bound_as_a_fresh_build_does_with_no_panic_caught() {
    const THIRD: &str = "fn main() {\n    println!(\"a third\");\n}\n";
    // A home of its own: it stops and kills its daemon, starts a second home, and reads its
    // daemon's log for panics, which another case's build would add to.
    let case = Case::own("a_crate_rebuilds_after_its_daemon_restarts");
    assert_eq!(case.ok(&["run"]), "before\n");
    // Once more, warm: the image the next daemon restores is written with nothing to build.
    assert_eq!(case.ok(&["run"]), "before\n");
    for (crash, text, prints) in [(false, AFTER, "after\n"), (true, THIRD, "a third\n")] {
        match crash {
            false => case.stop(),
            true => {
                let pid = daemon_pid(&case).expect("the daemon's log names its pid");
                case.kill_daemon();
                // `kill` returns before the process is gone: a run started meanwhile would
                // reach the dying daemon, which then hangs up on it.
                let began = Instant::now();
                while Command::new("kill").args(["-0", &pid]).stderr(Stdio::null()).status().unwrap().success() {
                    assert!(began.elapsed() < Duration::from_secs(30), "pid {pid} outlived its kill -9 by 30 s");
                    #[allow(clippy::disallowed_methods, reason = "std has no wait on a process that is not a child")]
                    thread::sleep(Duration::from_millis(20));
                }
            }
        }
        case.write("src/main.rs", text);
        let began = Instant::now();
        let got = case.ok(&["run"]);
        let took = began.elapsed();
        assert_eq!(got, prints, "after a restart (kill -9: {crash})");
        assert!(
            took < RESTART_EDIT_BOUND,
            "the first run after a restart (kill -9: {crash}) that saw src/main.rs changed took {took:?}, past its bound of {RESTART_EDIT_BOUND:?}"
        );
    }
    // What a fresh build of the source as it is now prints: a home, and so a daemon, of its own.
    let fresh = case.second_home();
    assert_eq!(fresh.ok(&["run"]), "a third\n", "a fresh home's build of the same source");
    let log = fs::read_to_string(case.home.join("v2/serve.log")).unwrap();
    let panics: Vec<&str> = log.lines().filter(|line| line.contains("panicked at")).collect();
    assert!(
        panics.is_empty(),
        "the daemon caught {} panics across two restarts and their edits, the first: {}",
        panics.len(),
        panics[0]
    );
}

/// The standard library is built once per daemon, not per workspace: a second workspace on the
/// daemon that built the first compiles its own package and nothing else (it has no
/// dependencies), so no `core`, `alloc`, `std`, `compiler_builtins`, `panic_*` or std's vendored
/// crates. It used to build std again for each workspace, a cold std's minutes each time.
#[test]
fn a_second_workspace_on_one_daemon_compiles_no_std_crate() {
    // A home of its own: what it asserts is what this daemon built before the second
    // workspace, which on a shared daemon another case's build would be.
    let case = Case::own("a_second_workspace_on_one_daemon_compiles_no_std_crate");
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
    // A home and machine cache of its own: it starts a second home, and asserts that std
    // came from the store its first home's daemon put it in.
    let case = Case::own("a_second_home_on_one_machine_compiles_no_std_crate");
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
/// blaze, compiled `NUM_JOBS` objects and then slept in cc's `block_on` forever:
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

/// An `async` body with an `.await` in a branch a constant condition rules out (`if F::ON { ..
/// .await }` of an associated constant, or of a `const`) builds and runs: the branch's blocks are
/// not compiled, so its yield has no arm in the switch that resumes or drops the future. Each
/// `.await` that runs suspends once, and the last future is dropped suspended.
#[test]
fn an_await_a_constant_condition_rules_out_builds() {
    let case = Case::new("an_await_a_constant_condition_rules_out_builds");
    assert_eq!(
        case.ok(&["run"]),
        "dropped early\ndropped early\n(7, 2) (8, 2)\n(2, 1) (7, 2)\n(8, 2)\ndropped early\n"
    );
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

/// `#[blaze::contract_test]` (docs/v2/plan/design/contracts.md, part 2): a function whose
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

/// `#[blaze::contract_test]` (docs/v2/plan/design/contracts.md, part 2): a function whose
/// `ensures` is wrong for one input gets a generated blaze-sim test that fails, naming the
/// function and the clause, with the least case as a test to paste; a contract that holds
/// passes, and `skip` generates nothing. The crate's manifest names `crates/blaze-sim` by its
/// absolute path, and blaze's own lock pins blaze-sim's dependencies to the versions blaze builds.
#[test]
fn generated_test_finds_planted_bug() {
    let case = Case::new("generated_test_finds_planted_bug");
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    case.write(
        "Cargo.toml",
        &format!(
            "[package]\nname = \"repro\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[dev-dependencies]\n\
             blaze-sim = {{ path = {:?} }}\n\n\
             [profile.test]\ncontract-checks = false\n\n[workspace]\n",
            root.join("crates/blaze-sim").display().to_string()
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

/// "The wrong call is unwritable" (docs/v2/plan/design/std.md, "The second rule"), tested and
/// not only said: each file under `wrong/` is one call bstd's types refuse, and `blaze check`
/// of it fails naming the call. The control is the crate as it stands (`src/lib.rs`): the
/// right form of each of those calls, which checks, so a failure below is the wrong call's
/// and not the setup's.
#[test]
fn bstd_the_wrong_call_does_not_compile() {
    let case = Case::new("bstd_the_wrong_call_does_not_compile");
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    case.write(
        "Cargo.toml",
        &format!(
            "[package]\nname = \"repro\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[dependencies]\n\
             bstd = {{ package = \"blaze-std\", path = {:?} }}\n\n[workspace]\n",
            root.join("crates/blaze-std").display().to_string()
        ),
    );
    case.write("Cargo.lock", &fs::read_to_string(root.join("Cargo.lock")).unwrap());
    case.ok(&["check"]);
    // Each wrong call, and what its error must name: the call, and the type that lacks it.
    let wrong: [(&str, &[&str]); 4] = [
        ("a_clone_from_written", &["clone_to", "Written"]),
        ("a_buffered_writer_dropped", &["`writer`", "Buffered"]),
        ("a_write_through_a_narrowed_handle", &["append_unsynced", "ReadOnly"]),
        ("a_removed_name", &["`write`", "Dir"]),
    ];
    for (fixture, names) in wrong {
        let source = fs::read_to_string(case.dir.join("wrong").join(format!("{fixture}.rs"))).unwrap();
        case.write("src/lib.rs", &source);
        let out = case.blaze_in("", &["check"]);
        assert!(!out.ok, "{fixture}: the wrong call checked:\n{}", out.stderr);
        for name in names {
            assert!(out.stderr.contains(name), "{fixture}: the error does not name {name}:\n{}", out.stderr);
        }
        assert!(out.stderr.contains("src/lib.rs"), "{fixture}: the error does not say where:\n{}", out.stderr);
    }
}

/// The manifest of a package named `name` on `edition`, with `dependencies` (lines of its
/// table) and, for a blaze edition, bstd from this tree: a package of the edition names
/// bstd by path until blaze ships it (docs/v2/plan/design/std.md, row S5d).
fn edition_manifest(name: &str, edition: &str, dependencies: &str, workspace: bool) -> String {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let bstd = match edition.starts_with("blaze-") {
        true => format!(
            "bstd = {{ package = \"blaze-std\", path = {:?} }}\n",
            root.join("crates/blaze-std").display().to_string()
        ),
        false => String::new(),
    };
    let workspace = if workspace { "\n[workspace]\n" } else { "" };
    format!(
        "[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"{edition}\"\n\n[lib]\ndoctest = \
         false\npath = \"src/lib.rs\"\n\n[dependencies]\n{bstd}{dependencies}{workspace}"
    )
}

/// The same, for a package that is a program alone.
fn edition_program(edition: &str, dependencies: &str) -> String {
    edition_manifest("repro", edition, dependencies, true).replace("[lib]\ndoctest = false\npath = \"src/lib.rs\"\n\n", "")
}

/// The tree's lockfile, which has bstd's dependencies at the versions the tree builds.
fn tree_lock() -> String {
    fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../Cargo.lock")).unwrap()
}

const HELLO_2024: &str = "fn main() {\n    println!(\"Hello, world!\");\n}\n";

/// S5 (docs/v2/plan/design/std.md, "The blaze edition: opting in"), in a real process, which
/// the exit status and standard error need: a hello world of the blaze edition, whose `main`
/// takes `&Cx` and `Io` and writes through `Out`, builds, prints its line and exits 0; one
/// that returns `Err` prints `error: <what>` on standard error, nothing on standard output,
/// and exits non-zero.
#[test]
fn the_blaze_edition_hello_world_runs_and_an_err_fails_the_process() {
    let case = Case::new("the_blaze_edition");
    case.write("Cargo.toml", &edition_program("blaze-2026.11", ""));
    case.write("Cargo.lock", &tree_lock());
    assert_eq!(case.ok(&["run"]), "Hello, world!\n");
    case.write(
        "src/main.rs",
        "fn main(cx: &Cx, io: Io) -> Result<(), Error> {\n    io.need_out()?.line(cx, \"before\")?;\n    \
         Err(Error::new(\"no such luck\"))\n}\n",
    );
    let out = case.blaze_in("", &["run"]);
    assert!(!out.ok, "a `main` that returned `Err` exited 0:\n{}", out.stderr);
    assert_eq!(out.stdout, "before\n");
    assert!(out.stderr.contains("error: no such luck"), "the error is not printed:\n{}", out.stderr);
    // Each shorter entry the rule allows runs too: `io` alone, with a context of its own.
    case.write(
        "src/main.rs",
        "fn main(io: Io) -> Result<(), Error> {\n    let cx = io.cx(std::entry::MAIN_BUDGET, 1 << 20)?;\n    \
         io.need_out()?.line(&cx, \"io alone\")?;\n    Ok(())\n}\n",
    );
    assert_eq!(case.ok(&["run"]), "io alone\n");
}

/// Old code is untouched: the same package on Rust 2024, with `fn main()` and `println!`,
/// builds and prints as before, in the same home that built the blaze edition's (so nothing
/// made for one is served to the other); and nothing of Rust std's ambient authority is
/// reachable through `std` in the edition: the same `println!` there does not resolve.
#[test]
fn the_blaze_edition_leaves_a_rust_edition_package_as_it_was() {
    let case = Case::new("the_blaze_edition");
    case.write("Cargo.toml", &edition_program("blaze-2026.11", ""));
    case.write("Cargo.lock", &tree_lock());
    assert_eq!(case.ok(&["run"]), "Hello, world!\n");
    // The same package, opted out: Rust's `main`, Rust's `std`.
    case.write("Cargo.toml", &edition_program("2024", ""));
    case.write("src/main.rs", HELLO_2024);
    assert_eq!(case.ok(&["run"]), "Hello, world!\n");
    // And back, with the Rust source left in place: `println!` and `fn main()` with Rust's
    // std are not the edition's.
    case.write("Cargo.toml", &edition_program("blaze-2026.11", ""));
    case.write("src/main.rs", "fn main() {\n    let _ = std::fs::read(\"/etc/hosts\");\n}\n");
    let out = case.blaze_in("", &["build"]);
    assert!(!out.ok, "`std::fs::read` resolved in the blaze edition:\n{}", out.stderr);
    assert!(out.stderr.contains("read"), "the error does not name the path:\n{}", out.stderr);
}

/// Both directions of the boundary link and run: a program of the blaze edition calls a
/// library of Rust 2024 (which uses Rust std's `Vec`, `String` and `format!` inside), and a
/// program of Rust 2024 makes the roots itself (`bstd::entry::run`) and hands a capability
/// to a library of the blaze edition.
#[test]
fn the_blaze_edition_and_rust_editions_depend_on_each_other() {
    let case = Case::new("the_blaze_edition");
    case.write("shelf/Cargo.toml", &edition_manifest("shelf", "blaze-2026.11", "", false));
    case.write("Cargo.lock", &tree_lock());
    // New on old.
    case.write("Cargo.toml", &edition_program("blaze-2026.11", "old = { path = \"old\" }\n"));
    case.write(
        "src/main.rs",
        "fn main(cx: &Cx, io: Io) -> Result<(), Error> {\n    let out = io.need_out()?;\n    \
         match old::answer() {\n        42 => out.line(cx, \"forty-two\")?,\n        _ => \
         out.line(cx, \"wrong\")?,\n    }\n    Ok(())\n}\n",
    );
    assert_eq!(case.ok(&["run"]), "forty-two\n");
    // Old on new: the Rust program names bstd as `bstd`, as any crate.
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    case.write(
        "Cargo.toml",
        &edition_program(
            "2024",
            &format!(
                "shelf = {{ path = \"shelf\" }}\nbstd = {{ package = \"blaze-std\", path = {:?} }}\n",
                root.join("crates/blaze-std").display().to_string()
            ),
        ),
    );
    case.write(
        "src/main.rs",
        "fn main() -> bstd::entry::Exit {\n    println!(\"from Rust's std\");\n    bstd::entry::run(|cx, io| \
         -> Result<(), bstd::entry::Error> {\n        shelf::greet(cx, &io.need_out()?)?;\n        \
         Ok(())\n    })\n}\n",
    );
    assert_eq!(case.ok(&["run"]), "from Rust's std\nHello from the shelf!\n");
}

/// A `main` the entry rule refuses is an error at `main` that names the rule
/// (`entry-takes-roots`) and the parameter: in the blaze edition a parameter that is no
/// root, and roots out of order; in a Rust edition any parameter, saying the edition it
/// needs and the manifest line. The control is each test above: a `main` the rule accepts
/// builds.
#[test]
fn the_blaze_edition_refuses_a_main_that_is_no_entry_by_name() {
    let case = Case::new("the_blaze_edition");
    case.write("Cargo.lock", &tree_lock());
    let wrong: [(&str, &str, &[&str]); 3] = [
        (
            "blaze-2026.11",
            "fn main(io: Io, cx: &Cx) {\n    let _ = (io, cx);\n}\n",
            &["entry-takes-roots", "parameter 2", "`&Cx` after `Io`"],
        ),
        (
            "blaze-2026.11",
            "fn main(cx: &Cx, count: u32) {\n    let _ = (cx, count);\n}\n",
            &["entry-takes-roots", "parameter 2", "not something the runtime makes"],
        ),
        (
            "2024",
            "fn main(count: u32) {\n    let _ = count;\n}\n",
            &["entry-takes-roots", "needs the blaze edition", "edition = \"blaze-2026.11\""],
        ),
    ];
    for (edition, source, names) in wrong {
        case.write("Cargo.toml", &edition_program(edition, ""));
        case.write("src/main.rs", source);
        let out = case.blaze_in("", &["build"]);
        assert!(!out.ok, "{edition}: `{source}` built:\n{}", out.stderr);
        for name in names {
            assert!(out.stderr.contains(name), "{edition}: the error does not say {name}:\n{}", out.stderr);
        }
        assert!(out.stderr.contains("src/main.rs"), "{edition}: the error does not say where:\n{}", out.stderr);
    }
    // An edition blaze does not know is refused by name, never read as some Rust edition.
    case.write("Cargo.toml", &edition_program("blaze-2026.12", ""));
    let out = case.blaze_in("", &["build"]);
    assert!(!out.ok && out.stderr.contains("blaze-2026.12") && out.stderr.contains("blaze-2026.11"), "{}", out.stderr);
}

/// `blaze new` then `blaze run`, for both kinds: where bstd is named the package is of the
/// blaze edition and its hello world runs on bstd; `--rust` is the plain package, which runs
/// as it always did.
#[test]
fn blaze_new_then_run_works_for_the_blaze_edition_and_for_rust() {
    let case = Case::new("blaze_new_then_run");
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let bstd = root.join("crates/blaze-std").display().to_string();
    case.ok_in("", &["new", "--vcs", "none", "--bstd", &bstd, "hello"]);
    let manifest = fs::read_to_string(case.dir.join("hello/Cargo.toml")).unwrap();
    assert!(manifest.contains("edition = \"blaze-2026.11\""), "{manifest}");
    let source = fs::read_to_string(case.dir.join("hello/src/main.rs")).unwrap();
    assert!(source.contains("fn main(cx: &Cx, io: Io) -> Result<(), Error>"), "{source}");
    case.write("hello/Cargo.lock", &tree_lock());
    assert_eq!(case.ok_in("hello", &["run"]).stdout, "Hello, world!\n");
    case.ok_in("", &["new", "--vcs", "none", "--rust", "plain"]);
    let manifest = fs::read_to_string(case.dir.join("plain/Cargo.toml")).unwrap();
    assert!(manifest.contains("edition = \"2024\"") && !manifest.contains("bstd"), "{manifest}");
    assert_eq!(fs::read_to_string(case.dir.join("plain/src/main.rs")).unwrap(), HELLO_2024);
    assert_eq!(case.ok_in("plain", &["run"]).stdout, "Hello, world!\n");
}

/// `blaze::not_latest_edition` (docs/lints.md#not_latest_edition), through `blaze check`: a
/// workspace package on a Rust edition is told exactly once; its path dependency that is no
/// member of the workspace is not told; allowing the lint silences it; denying it fails the check
/// naming the lint; a package of the blaze edition is told nothing. The workspace asks for
/// the lint by name, since its default waits for `ready::EDITION_SHIPS`.
#[test]
fn a_package_not_on_the_latest_blaze_edition_is_told_once() {
    const TOLD: &str = "not the latest blaze edition";
    let case = Case::new("the_blaze_edition");
    case.write("Cargo.lock", &tree_lock());
    case.write("src/main.rs", "fn main() {\n    println!(\"{}\", old::answer());\n}\n");
    let with = |level: &str| {
        let lints = match level {
            "" => String::new(),
            level => format!("\n[workspace.metadata.blaze.lints]\nnot_latest_edition = \"{level}\"\n"),
        };
        // `old` is beneath the workspace's root, which would make it a member, told as one:
        // excluded, it is a dependency by path that is not the workspace's own.
        format!("{}exclude = [\"old\"]\n{lints}", edition_program("2024", "old = { path = \"old\" }\n"))
    };
    case.write("Cargo.toml", &with("warn"));
    let out = case.ok_in("", &["check"]);
    assert_eq!(out.stderr.matches(TOLD).count(), 1, "told once, and not for `old`:\n{}", out.stderr);
    assert!(out.stderr.contains("package `repro`"), "{}", out.stderr);
    assert!(out.stderr.contains("edition = \"blaze-2026.11\""), "{}", out.stderr);
    case.write("Cargo.toml", &with("allow"));
    let out = case.ok_in("", &["check"]);
    assert_eq!(out.stderr.matches(TOLD).count(), 0, "{}", out.stderr);
    case.write("Cargo.toml", &with("deny"));
    let out = case.blaze_in("", &["check"]);
    assert!(!out.ok, "the denied lint did not fail the check:\n{}", out.stderr);
    // The level's note names the lint as the command line spells it, with dashes.
    assert!(out.stderr.contains("-D blaze::not-latest-edition") && out.stderr.contains(TOLD), "{}", out.stderr);
    // The same workspace on the blaze edition, the lint still denied: nothing to tell.
    let denied = "\n[workspace.metadata.blaze.lints]\nnot_latest_edition = \"deny\"\n";
    case.write("Cargo.toml", &format!("{}{denied}", edition_program("blaze-2026.11", "")));
    case.write("src/main.rs", "fn main(cx: &Cx, io: Io) -> Result<(), Error> {\n    io.need_out()?.line(cx, \"x\")?;\n    Ok(())\n}\n");
    let out = case.ok_in("", &["check"]);
    assert_eq!(out.stderr.matches(TOLD).count(), 0, "{}", out.stderr);
}
