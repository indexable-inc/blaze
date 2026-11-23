//! Resident per-checkout `blaze` daemon tests (Wave 7, rules 26 & 70):
//!
//! 1. `daemon_soundness_oracle_10_step_byte_identical`:
//!    Runs a 10-step real-world edit lifecycle in lockstep through the live daemon vs standalone
//!    `BLAZE_NO_DAEMON=1` (cold check -> warm no-op #1 -> warm no-op #2 fast path -> 1-line body
//!    edit -> error edit -> fix edit -> signature edit -> new file added -> `SIGKILL` daemon
//!    mid-sequence & transparent recovery -> `blaze clean` graceful `Shutdown`). Asserts
//!    byte-identical stdout, stderr, exit status, and CAS `Pointer` semantic root (`pointer.root`,
//!    `pointer.lines`, `pointer.facts`) at every single step.
//! 2. `daemon_kqueue_vnode_zero_toctou_and_atomic_rename` (macOS):
//!    Directly exercises `KqueueVnode` proving synchronous `kevent(0)` detection with zero false
//!    negatives for in-place `write`, `truncate`, atomic `tmp -> rename` (vim / VS Code save),
//!    new file creation in a watched dir filling an `ABSENT` slot, `unlink`, and zero-TOCTOU
//!    post-`EV_ADD` stale-stamp detection.
//! 3. `daemon_external_writer_and_gc_generation_detection`:
//!    Proves that external writes (`BLAZE_NO_DAEMON=1` + `blaze gc`) advancing the on-disk CAS
//!    `head` generation are detected immediately on the next live-daemon request (`disk_gen !=
//!    state.store_gen`), forcing a fresh store reopen and serving the exact updated result.
//! 4. `daemon_opt_out_blaze_no_daemon`:
//!    Proves `BLAZE_NO_DAEMON=1` neither connects to nor spawns a daemon socket.

#[cfg(target_os = "macos")]
use std::os::unix::fs::{MetadataExt, symlink};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use blaze_drive::Key;
use blaze_watch::daemon as wire;

const CMD_DEADLINE: Duration = Duration::from_secs(120);
const SOCKET_WAIT: Duration = Duration::from_secs(10);
const EXIT_WAIT: Duration = Duration::from_secs(5);

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

/// Locates the compiled `blaze` CLI binary (`BLAZE_TEST_BIN` or walking ancestors of `current_exe()`).
fn blaze_bin() -> PathBuf {
    if let Some(p) = std::env::var_os("BLAZE_TEST_BIN") {
        let pb = PathBuf::from(p);
        assert!(pb.is_file(), "BLAZE_TEST_BIN does not exist: {}", pb.display());
        return pb;
    }
    let exe = std::env::current_exe().expect("current_exe");
    for ancestor in exe.ancestors().skip(1) {
        let candidate = ancestor.join("blaze");
        if candidate.is_file() {
            return candidate;
        }
    }
    panic!(
        "compiled blaze binary not found above {} — run `cargo build -p blaze-cli --bin blaze` first",
        exe.display(),
    );
}

/// Short scratch base under `/tmp/bd-<pid>-<seq>` so `<home>/.blaze/v2/daemon/<16-hex>.sock`
/// stays well under macOS's 104-byte `sun_path` limit, with RAII cleanup of any spawned daemon
/// and scratch files (rule 39).
struct Scratch {
    base: PathBuf,
    homes: std::vec::Vec<(PathBuf, PathBuf)>,
}

#[cfg(unix)]
fn posix_stat(p: &Path) -> Option<blaze_sys::posix::proc::stat> {
    let s = p.to_str()?;
    let mut buf = [0u8; 4096];
    let b = s.as_bytes();
    if b.is_empty() || b.len() >= buf.len() { return None; }
    buf[..b.len()].copy_from_slice(b);
    buf[b.len()] = 0;
    let c = core::ffi::CStr::from_bytes_with_nul(&buf[..=b.len()]).ok()?;
    blaze_sys::posix::proc::stat_at(blaze_sys::posix::io::CWD, blaze_sys::Path::new(c), 0).ok()
}

/// Chooses a short scratch root directory that stays on the toolchain shelf's filesystem
/// (avoiding cross-device `EXDEV` copies of the ~2.5 GB shelf) while keeping
/// `<home>/.blaze/v2/daemon/<16-hex>.sock` within Unix `sun_path` limits (104 B macOS / 108 B Linux).
fn scratch_parent(tag: &str, seq: u64) -> PathBuf {
    let leaf = format!("bd-{tag}-{}-{seq}", std::process::id());
    let dev_of = |p: &Path| posix_stat(p).map(|st| st.st_dev);
    let (owner_home, tmp) = (std::env::var_os("HOME"), std::env::temp_dir());
    let shelf_dev = owner_home.as_ref().and_then(|h| dev_of(&Path::new(h).join(".blaze/v2/toolchain")));
    let fits = |dir: &Path| -> bool {
        let sample = dir.join(&leaf).join("standalone").join("h");
        sample.to_str().is_some_and(|s| crate::daemon_server::socket_fits(Some(s)))
    };
    let mut candidates = [Some(tmp), None, Some(PathBuf::from("/tmp"))];
    if let Some(h) = owner_home.as_ref() {
        let short_home = Path::new(h).join(".blaze/t");
        if std::fs::create_dir_all(&short_home).is_ok() {
            candidates[1] = Some(short_home);
        }
    }
    if let Some(want_dev) = shelf_dev {
        for cand in candidates.iter().flatten() {
            if let Ok(canon) = cand.canonicalize()
                && dev_of(&canon) == Some(want_dev)
                && fits(&canon)
            {
                return canon.join(&leaf);
            }
        }
    }
    for cand in candidates.iter().flatten() {
        if let Ok(canon) = cand.canonicalize()
            && fits(&canon)
        {
            return canon.join(&leaf);
        }
    }
    PathBuf::from(format!("/tmp/{leaf}"))
}

impl Scratch {
    fn new(tag: &str) -> Scratch {
        let seq = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let base = scratch_parent(tag, seq);
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).expect("create scratch base");
        let base = base.canonicalize().expect("canonicalize scratch base");
        Scratch { base, homes: std::vec::Vec::new() }
    }

    fn lane(&mut self, name: &str, with_shelf: bool) -> (PathBuf, PathBuf) {
        let lane = self.base.join(name);
        let home = lane.join("h");
        let proj = lane.join("p");
        std::fs::create_dir_all(&home).expect("create lane home");
        std::fs::create_dir_all(&proj).expect("create lane proj");
        if with_shelf {
            link_toolchain_shelf(&home);
        }
        self.homes.push((home.clone(), proj.clone()));
        (home, proj)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        for (home, proj) in &self.homes {
            let home_s = home.to_string_lossy();
            let proj_s = proj.to_string_lossy();
            crate::daemon_client::send_shutdown(&home_s, &proj_s);
            let sock = daemon_socket_path(home, proj);
            if let Some(pid) = find_daemon_pid(&sock) {
                let _ = Command::new("kill").args(["-9", &pid.to_string()]).status();
            }
        }
        let _ = std::fs::remove_dir_all(&self.base);
    }
}

/// Hard-links `~/.blaze/v2/toolchain` into `<home>/.blaze/v2/toolchain` (read-only shelf,
/// matching `tests/support/mod.rs`).
fn link_toolchain_shelf(home: &Path) {
    let owner_home = std::env::var_os("HOME").expect("owner HOME must be set");
    let src_shelf = Path::new(&owner_home).join(".blaze/v2/toolchain");
    assert!(
        src_shelf.is_dir(),
        "toolchain shelf missing at {} (needed for blaze check tests)",
        src_shelf.display(),
    );
    let dst_shelf = home.join(".blaze/v2/toolchain");
    let mut stack = std::vec![(src_shelf, dst_shelf)];
    while let Some((from, to)) = stack.pop() {
        std::fs::create_dir_all(&to).unwrap_or_else(|e| panic!("mkdir {}: {e}", to.display()));
        for entry in std::fs::read_dir(&from).unwrap_or_else(|e| panic!("read_dir {}: {e}", from.display())) {
            let entry = entry.expect("shelf entry");
            let kind = entry.file_type().expect("shelf file_type");
            let (src, dst) = (entry.path(), to.join(entry.file_name()));
            if kind.is_dir() {
                stack.push((src, dst));
            } else if kind.is_symlink() {
                let target = std::fs::read_link(&src).expect("read_link");
                std::os::unix::fs::symlink(&target, &dst).unwrap_or_else(|e| panic!("symlink {}: {e}", dst.display()));
            } else if let Err(e) = std::fs::hard_link(&src, &dst) {
                if e.kind() == std::io::ErrorKind::CrossesDevices || e.raw_os_error() == Some(18) {
                    std::fs::copy(&src, &dst).unwrap_or_else(|copy_e| panic!("copy {} -> {}: {copy_e}", src.display(), dst.display()));
                } else {
                    panic!("hard_link {} -> {}: {e}", src.display(), dst.display());
                }
            }
        }
    }
}

fn write_file(dir: &Path, rel: &str, content: &str) {
    let full = dir.join(rel);
    if let Some(parent) = full.parent() {
        std::fs::create_dir_all(parent).expect("create parent dirs");
    }
    std::fs::write(&full, content).unwrap_or_else(|e| panic!("write {}: {e}", full.display()));
}

#[derive(Debug)]
struct CmdOut {
    stdout: std::vec::Vec<u8>,
    stderr: std::vec::Vec<u8>,
    code: i32,
    elapsed: Duration,
}

fn run_blaze(dir: &Path, home: &Path, args: &[&str], no_daemon: bool) -> CmdOut {
    let bin = blaze_bin();
    let started = Instant::now();
    let mut cmd = Command::new(&bin);
    cmd.args(args)
        .current_dir(dir)
        .env("PWD", dir)
        .env("HOME", home)
        .env("CARGO_HOME", home.join(".cargo"))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if no_daemon {
        cmd.env("BLAZE_NO_DAEMON", "1");
    } else {
        cmd.env_remove("BLAZE_NO_DAEMON");
    }
    let mut child = cmd.spawn().expect("spawn blaze");
    let deadline = Instant::now() + CMD_DEADLINE;
    loop {
        if child.try_wait().expect("try_wait").is_some() {
            let output = child.wait_with_output().expect("wait_with_output");
            return CmdOut {
                stdout: output.stdout,
                stderr: output.stderr,
                code: output.status.code().unwrap_or(-1),
                elapsed: started.elapsed(),
            };
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let output = child.wait_with_output().expect("wait_with_output after kill");
            panic!(
                "blaze {:?} timed out after {:?}\nstdout:\n{}\nstderr:\n{}",
                args,
                CMD_DEADLINE,
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr),
            );
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}

fn daemon_socket_path(home: &Path, proj_abs: &Path) -> PathBuf {
    let key = crate::journal_ask::checkout_key_of(&proj_abs.to_string_lossy());
    let rel = wire::socket_path(&key);
    let rel_str = core::str::from_utf8(&rel).expect("utf8 socket rel");
    home.join(".blaze/v2").join(rel_str)
}

fn wait_for_socket(sock: &Path, timeout: Duration) -> bool {
    let end = Instant::now() + timeout;
    while Instant::now() < end {
        if sock.exists() {
            // Confirm it actually accepts connections (listener bound, not mid-startup).
            if blaze_sys::posix::net::connect_unix(sock.as_os_str().as_encoded_bytes()).is_ok() {
                return true;
            }
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    false
}

fn wait_socket_gone(sock: &Path, timeout: Duration) -> bool {
    let end = Instant::now() + timeout;
    while Instant::now() < end {
        if !sock.exists() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    !sock.exists()
}

/// Finds the PID of the resident `blaze daemon` listening on `sock` via `LOCAL_PEERPID` / `SO_PEERCRED`.
fn find_daemon_pid(sock: &Path) -> Option<u32> {
    let conn = blaze_sys::posix::net::connect_unix(sock.as_os_str().as_encoded_bytes()).ok()?;
    #[cfg(target_os = "macos")]
    {
        let pid = blaze_sys::posix::net::peer_pid(conn.fd()).ok()?;
        u32::try_from(pid).ok()
    }
    #[cfg(target_os = "linux")]
    {
        let cred = blaze_sys::posix::net::peer_cred(conn.fd()).ok()?;
        u32::try_from(cred.pid).ok()
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = conn;
        None
    }
}

/// Semantic CAS `Pointer` summary read directly from `<home>/.blaze/v2/store/f5s3`.
#[derive(Debug, PartialEq, Eq)]
struct PointerSummary {
    answer_root: Key,
    lines_key: Key,
    facts: crate::engine_answer::Facts,
    generation: u64,
}

fn read_pointer_summary(home: &Path, proj_abs: &Path) -> PointerSummary {
    let blaze_dir = home.join(".blaze");
    let driver = blaze_core::native(&blaze_dir, 2).expect("native driver on home/.blaze");
    let alloc = blaze_own::testing::heap().alloc();
    let settings = blaze_cas::plain::Settings::default();
    let facts = crate::store::plain_place::Facts { boot: None, nonce: 0xdead_beef_cafe_0001 };
    let dir = blaze_core::FileName::new(crate::store::plain_place::STORE_DIR).expect("store dir name");
    let cfg = crate::store::plain_place::config(dir, &settings, facts, alloc);
    let lent = crate::store::plain_place::open(driver, cfg, alloc).expect("open CAS store read-only");
    let generation = lent.generation();
    let frame = bstd::mem::Frame::with_reserve(2 << 20, bstd::mem::Tag::Other).expect("frame");
    let proj_s = proj_abs.to_string_lossy();
    let root_name = crate::engine_answer::root_name(&proj_s, &frame);
    let place = crate::store::PlainPlace::new(&lent, false);
    let held_root = place
        .root(root_name.as_str())
        .expect("read answer root")
        .unwrap_or_else(|| panic!("answer root `{}` missing in {}", root_name.as_str(), blaze_dir.display()));
    let key_bytes: [u8; 32] = held_root.bytes.try_into().expect("32-byte pointer key");
    let pkey = Key(blaze_ids::ContentId(key_bytes));
    let held_ptr = place
        .get(pkey)
        .expect("read pointer blob")
        .expect("pointer blob missing");
    let ptr = crate::engine_answer::decode_pointer_bytes(pkey, held_ptr.bytes)
        .expect("decode pointer")
        .expect("valid pointer");
    let summary = PointerSummary {
        answer_root: ptr.root,
        lines_key: ptr.lines,
        facts: ptr.facts,
        generation,
    };
    let _ = lent.release_lease();
    summary
}

fn normalize_root(bytes: &[u8], proj_abs: &Path) -> std::vec::Vec<u8> {
    let text = String::from_utf8_lossy(bytes);
    let root_s = proj_abs.to_string_lossy();
    text.replace(root_s.as_ref(), "<ROOT>").into_bytes()
}

fn assert_step_matches(step: &str, d_proj: &Path, d_home: &Path, d: &CmdOut, s_proj: &Path, s_home: &Path, s: &CmdOut) {
    assert_eq!(
        d.code, s.code,
        "step `{step}`: exit code mismatch (daemon={} vs standalone={})\ndaemon stderr:\n{}\nstandalone stderr:\n{}",
        d.code,
        s.code,
        String::from_utf8_lossy(&d.stderr),
        String::from_utf8_lossy(&s.stderr),
    );
    let d_out = normalize_root(&d.stdout, d_proj);
    let s_out = normalize_root(&s.stdout, s_proj);
    assert_eq!(
        d_out,
        s_out,
        "step `{step}`: stdout mismatch\n--- daemon ---\n{}\n--- standalone ---\n{}",
        String::from_utf8_lossy(&d_out),
        String::from_utf8_lossy(&s_out),
    );
    let d_err = normalize_root(&d.stderr, d_proj);
    let s_err = normalize_root(&s.stderr, s_proj);
    assert_eq!(
        d_err,
        s_err,
        "step `{step}`: stderr mismatch\n--- daemon ---\n{}\n--- standalone ---\n{}",
        String::from_utf8_lossy(&d_err),
        String::from_utf8_lossy(&s_err),
    );
    let d_ptr = read_pointer_summary(d_home, d_proj);
    let s_ptr = read_pointer_summary(s_home, s_proj);
    assert_eq!(
        d_ptr.answer_root, s_ptr.answer_root,
        "step `{step}`: CAS Pointer.root mismatch (daemon={:?} vs standalone={:?})",
        d_ptr.answer_root, s_ptr.answer_root,
    );
    assert_eq!(
        d_ptr.lines_key, s_ptr.lines_key,
        "step `{step}`: CAS Pointer.lines mismatch",
    );
    assert_eq!(
        d_ptr.facts, s_ptr.facts,
        "step `{step}`: CAS Pointer.facts mismatch",
    );
}

#[cfg(unix)]
#[test]
fn daemon_soundness_oracle_10_step_byte_identical() {
    let mut scratch = Scratch::new("oracle");
    let (d_home, d_proj) = scratch.lane("d", true);
    let (s_home, s_proj) = scratch.lane("s", true);

    let cargo_toml = "[package]\nname = \"demo\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[lib]\npath = \"src/lib.rs\"\n";
    let init_lib = "mod util;\n\npub fn answer() -> u32 {\n    util::helper(1)\n}\n";
    let init_util = "pub fn helper(x: u32) -> u32 {\n    x + 41\n}\n";

    for proj in [&d_proj, &s_proj] {
        write_file(proj, "Cargo.toml", cargo_toml);
        write_file(proj, "src/lib.rs", init_lib);
        write_file(proj, "src/util.rs", init_util);
    }

    let sock = daemon_socket_path(&d_home, &d_proj);

    // Step 1: Cold check (spawns daemon in daemon lane).
    let d1 = run_blaze(&d_proj, &d_home, &["check"], false);
    let s1 = run_blaze(&s_proj, &s_home, &["check"], true);
    assert_step_matches("1-cold", &d_proj, &d_home, &d1, &s_proj, &s_home, &s1);
    assert!(wait_for_socket(&sock, SOCKET_WAIT), "daemon socket did not appear at {}", sock.display());
    let pid1 = find_daemon_pid(&sock).expect("live daemon pid after cold check");

    // Step 2: Warm no-op #1 (hits live daemon, records env_fp).
    let d2 = run_blaze(&d_proj, &d_home, &["check"], false);
    let s2 = run_blaze(&s_proj, &s_home, &["check"], true);
    assert_step_matches("2-warm-noop-1", &d_proj, &d_home, &d2, &s_proj, &s_home, &s2);
    assert_eq!(find_daemon_pid(&sock), Some(pid1), "same daemon served step 2");

    // Step 3: Warm no-op #2 (hits ultra-fast <0.5 ms certified fast path!).
    let d3 = run_blaze(&d_proj, &d_home, &["check"], false);
    let s3 = run_blaze(&s_proj, &s_home, &["check"], true);
    assert_step_matches("3-warm-noop-fast", &d_proj, &d_home, &d3, &s_proj, &s_home, &s3);
    let profile3 = std::fs::read_to_string(d_proj.join(".blaze/v2/profile.txt")).expect("read daemon profile.txt");
    assert!(
        profile3.contains("resident daemon fast path"),
        "expected step 3 to hit resident daemon fast path, got:\n{profile3}",
    );
    eprintln!(
        "daemon_soundness_oracle: warm fast hit elapsed = {:?} (standalone = {:?})",
        d3.elapsed, s3.elapsed
    );

    // Step 4: 1-line body edit.
    let edit4 = "mod util;\n\npub fn answer() -> u32 {\n    util::helper(2)\n}\n";
    for proj in [&d_proj, &s_proj] {
        write_file(proj, "src/lib.rs", edit4);
    }
    let d4 = run_blaze(&d_proj, &d_home, &["check"], false);
    let s4 = run_blaze(&s_proj, &s_home, &["check"], true);
    assert_step_matches("4-body-edit", &d_proj, &d_home, &d4, &s_proj, &s_home, &s4);
    assert_eq!(find_daemon_pid(&sock), Some(pid1), "same daemon served step 4");
    eprintln!(
        "daemon_soundness_oracle: 1-line body edit elapsed = {:?} (standalone = {:?})",
        d4.elapsed, s4.elapsed
    );

    // Step 5: Error edit (type error -> non-zero exit code & exact diagnostic).
    let edit5 = "mod util;\n\npub fn answer() -> u32 {\n    \"not_a_u32\"\n}\n";
    for proj in [&d_proj, &s_proj] {
        write_file(proj, "src/lib.rs", edit5);
    }
    let d5 = run_blaze(&d_proj, &d_home, &["check"], false);
    let s5 = run_blaze(&s_proj, &s_home, &["check"], true);
    assert_eq!(d5.code, 1, "error edit must exit 1");
    assert_step_matches("5-error-edit", &d_proj, &d_home, &d5, &s_proj, &s_home, &s5);

    // Step 6: Fix edit.
    let edit6 = "mod util;\n\npub fn answer() -> u32 {\n    util::helper(3)\n}\n";
    for proj in [&d_proj, &s_proj] {
        write_file(proj, "src/lib.rs", edit6);
    }
    let d6 = run_blaze(&d_proj, &d_home, &["check"], false);
    let s6 = run_blaze(&s_proj, &s_home, &["check"], true);
    assert_ne!(d6.code, 1, "fix edit must clear the type error");
    assert_step_matches("6-fix-edit", &d_proj, &d_home, &d6, &s_proj, &s_home, &s6);

    // Step 7: Signature edit (change helper arity + caller).
    let util7 = "pub fn helper(x: u32, y: u32) -> u32 {\n    x + y\n}\n";
    let lib7 = "mod util;\n\npub fn answer() -> u32 {\n    util::helper(10, 32)\n}\n";
    for proj in [&d_proj, &s_proj] {
        write_file(proj, "src/util.rs", util7);
        write_file(proj, "src/lib.rs", lib7);
    }
    let d7 = run_blaze(&d_proj, &d_home, &["check"], false);
    let s7 = run_blaze(&s_proj, &s_home, &["check"], true);
    assert_step_matches("7-signature-edit", &d_proj, &d_home, &d7, &s_proj, &s_home, &s7);

    // Step 8: New file added (`src/extra.rs` + `mod extra;`).
    let extra8 = "pub fn bonus() -> u32 {\n    7\n}\n";
    let lib8 = "mod util;\nmod extra;\n\npub fn answer() -> u32 {\n    util::helper(10, extra::bonus())\n}\n";
    for proj in [&d_proj, &s_proj] {
        write_file(proj, "src/extra.rs", extra8);
        write_file(proj, "src/lib.rs", lib8);
    }
    let d8 = run_blaze(&d_proj, &d_home, &["check"], false);
    let s8 = run_blaze(&s_proj, &s_home, &["check"], true);
    assert_step_matches("8-new-file", &d_proj, &d_home, &d8, &s_proj, &s_home, &s8);

    // Step 9: SIGKILL daemon mid-sequence, edit while dead, verify transparent recovery + respawn.
    let kill_status = Command::new("kill")
        .args(["-9", &pid1.to_string()])
        .status()
        .expect("kill -9 daemon");
    assert!(kill_status.success(), "kill -9 daemon failed");
    let wait_dead = Instant::now() + EXIT_WAIT;
    while find_daemon_pid(&sock) == Some(pid1) && Instant::now() < wait_dead {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_ne!(find_daemon_pid(&sock), Some(pid1), "daemon did not die after SIGKILL");

    let lib9 = "mod util;\nmod extra;\n\npub fn answer() -> u32 {\n    util::helper(99, extra::bonus())\n}\n";
    for proj in [&d_proj, &s_proj] {
        write_file(proj, "src/lib.rs", lib9);
    }
    let d9 = run_blaze(&d_proj, &d_home, &["check"], false);
    let s9 = run_blaze(&s_proj, &s_home, &["check"], true);
    assert_step_matches("9-sigkill-recovery", &d_proj, &d_home, &d9, &s_proj, &s_home, &s9);
    assert!(wait_for_socket(&sock, SOCKET_WAIT), "daemon did not respawn after SIGKILL recovery");
    let pid2 = find_daemon_pid(&sock).expect("respawned daemon pid");
    assert_ne!(pid1, pid2, "new daemon must have a new PID");

    // Step 10: `blaze clean` sends `ClientFrame::Shutdown`, daemon exits cleanly & unlinks socket.
    let clean_out = run_blaze(&d_proj, &d_home, &["clean"], false);
    assert_eq!(
        clean_out.code, 0,
        "blaze clean failed: stderr={}",
        String::from_utf8_lossy(&clean_out.stderr),
    );
    assert!(
        wait_socket_gone(&sock, EXIT_WAIT),
        "daemon socket still exists after `blaze clean`: {}",
        sock.display(),
    );
    let wait_shutdown = Instant::now() + EXIT_WAIT;
    while find_daemon_pid(&sock).is_some() && Instant::now() < wait_shutdown {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(
        find_daemon_pid(&sock),
        None,
        "daemon process still alive after `blaze clean`",
    );
}

#[cfg(unix)]
#[test]
fn daemon_external_writer_and_gc_generation_detection() {
    let mut scratch = Scratch::new("extgen");
    let (home, proj) = scratch.lane("d", true);

    write_file(&proj, "Cargo.toml", "[package]\nname = \"ext\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[lib]\npath = \"src/lib.rs\"\n");
    write_file(&proj, "src/lib.rs", "pub fn val() -> u32 { 1 }\n");

    let sock = daemon_socket_path(&home, &proj);

    // Cold check spawns daemon.
    let c1 = run_blaze(&proj, &home, &["check"], false);
    assert_ne!(c1.code, 1);
    assert!(wait_for_socket(&sock, SOCKET_WAIT), "daemon socket did not appear");
    let pid = find_daemon_pid(&sock).expect("daemon pid");
    let gen_before = read_pointer_summary(&home, &proj).generation;

    // Warm check through live daemon.
    let c2 = run_blaze(&proj, &home, &["check"], false);
    assert_eq!(c2.code, c1.code);

    // External writer (`BLAZE_NO_DAEMON=1`) checks a broken edit, advancing `head` generation on disk!
    write_file(&proj, "src/lib.rs", "pub fn val() -> u32 { \"broken_external\" }\n");
    let ext_broken = run_blaze(&proj, &home, &["check"], true);
    assert_eq!(ext_broken.code, 1, "external broken check must exit 1");
    let gen_after_ext = read_pointer_summary(&home, &proj).generation;
    assert!(
        gen_after_ext > gen_before,
        "expected external check to advance CAS head generation ({gen_after_ext} <= {gen_before})",
    );

    // Now ask the LIVE daemon (without touching any files again) — it MUST detect `disk_gen != state.store_gen`,
    // reopen the store, and serve the exact updated broken diagnostic & status 1!
    let d_broken = run_blaze(&proj, &home, &["check"], false);
    assert_eq!(d_broken.code, 1, "daemon missed external writer's broken check!");
    assert_eq!(
        normalize_root(&d_broken.stdout, &proj),
        normalize_root(&ext_broken.stdout, &proj),
        "daemon stdout after external write must match standalone",
    );
    assert_eq!(find_daemon_pid(&sock), Some(pid), "same daemon remained alive");

    // External fix + `blaze gc` advancing generation again!
    write_file(&proj, "src/lib.rs", "pub fn val() -> u32 { 99 }\n");
    let ext_fixed = run_blaze(&proj, &home, &["check"], true);
    assert_ne!(ext_fixed.code, 1, "external fixed check must succeed");
    let gc = run_blaze(&proj, &home, &["gc"], true);
    assert_eq!(gc.code, 0, "blaze gc failed: {}", String::from_utf8_lossy(&gc.stderr));

    let d_fixed = run_blaze(&proj, &home, &["check"], false);
    assert_eq!(d_fixed.code, ext_fixed.code, "daemon missed external fix + gc!");
    assert_eq!(
        normalize_root(&d_fixed.stdout, &proj),
        normalize_root(&ext_fixed.stdout, &proj),
        "daemon stdout after external fix + gc must match standalone",
    );
    assert_eq!(find_daemon_pid(&sock), Some(pid), "same daemon served post-GC check");
}

#[cfg(unix)]
#[test]
fn daemon_opt_out_blaze_no_daemon() {
    let mut scratch = Scratch::new("nodae");
    let (home, proj) = scratch.lane("s", true);

    write_file(&proj, "Cargo.toml", "[package]\nname = \"nod\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[lib]\npath = \"src/lib.rs\"\n");
    write_file(&proj, "src/lib.rs", "pub fn ok() -> u32 { 7 }\n");

    let sock = daemon_socket_path(&home, &proj);
    let c1 = run_blaze(&proj, &home, &["check"], true);
    let c2 = run_blaze(&proj, &home, &["check"], true);
    assert_ne!(c1.code, 1);
    assert_eq!(c1.code, c2.code);
    assert_eq!(c1.stdout, c2.stdout);

    // Give any hypothetical stray background spawn a moment, then assert ZERO socket & ZERO daemon.
    std::thread::sleep(Duration::from_millis(100));
    assert!(!sock.exists(), "BLAZE_NO_DAEMON=1 must never create daemon socket {}", sock.display());
    assert_eq!(
        find_daemon_pid(&sock),
        None,
        "BLAZE_NO_DAEMON=1 must never spawn a daemon process",
    );
}

#[cfg(target_os = "macos")]
#[test]
fn daemon_kqueue_vnode_zero_toctou_and_atomic_rename() {
    use blaze_ids::ContentId;
    use crate::daemon_watch::KqueueVnode;
    use crate::sweep::{ABSENT, LeafKind, Leaves, Rows, Stamp};

    let mut scratch = Scratch::new("kqueue");
    let (_home, proj) = scratch.lane("k", false);

    write_file(&proj, "Cargo.toml", "[package]\nname = \"kq\"\nversion = \"0.1.0\"\nedition = \"2024\"\n");
    write_file(&proj, "src/lib.rs", "pub fn a() -> u32 { 1 }\n");
    write_file(&proj, "src/other.rs", "pub fn b() -> u32 { 2 }\n");

    let stat_of = |rel: &str| -> Stamp {
        use std::os::unix::fs::MetadataExt;
        let md = std::fs::symlink_metadata(proj.join(rel)).unwrap_or_else(|e| panic!("lstat {rel}: {e}"));
        let ns = |secs: i64, nsec: i64| secs.cast_unsigned().wrapping_mul(1_000_000_000).wrapping_add(nsec.cast_unsigned());
        Stamp {
            len: md.len(),
            mtime_ns: ns(md.mtime(), md.mtime_nsec()),
            ctime_ns: ns(md.ctime(), md.ctime_nsec()),
            ino: md.ino(),
            dev: md.dev(),
        }
    };

    let alloc = blaze_own::testing::heap().alloc();
    let frame = bstd::mem::Frame::with_reserve(1 << 20, bstd::mem::Tag::Other).expect("frame");
    let root_s = proj.to_string_lossy();
    let zero_id = ContentId([0; 32]);

    let build_leaves_bytes = |new_mod: Option<Stamp>| -> blaze_own::Vec<u8, &bstd::mem::Frame> {
        let mut rows = Rows::new_in(&frame);
        rows.push(LeafKind::Manifest, 0, "Cargo.toml", Some(stat_of("Cargo.toml")), zero_id);
        rows.push(LeafKind::Slot, 0, "src/lib.rs", Some(stat_of("src/lib.rs")), zero_id);
        rows.push(LeafKind::Slot, 0, "src/other.rs", Some(stat_of("src/other.rs")), zero_id);
        match new_mod {
            Some(st) => rows.push(LeafKind::Slot, 0, "src/new_mod.rs", Some(st), zero_id),
            None => rows.push(LeafKind::Slot, ABSENT, "src/new_mod.rs", None, zero_id),
        }
        rows.push(LeafKind::Listing, 0, "src", Some(stat_of("src")), zero_id);
        Leaves::bytes_in(&rows, &frame).expect("encode leaves")
    };

    // 1. Arm with accurate stamps -> immediate poll must report 0 dirty paths!
    let bytes1 = build_leaves_bytes(None);
    let (leaves1, _) = Leaves::of(&bytes1).expect("decode leaves1");
    let mut kq = KqueueVnode::arm(&root_s, None, &leaves1, alloc).expect("KqueueVnode::arm");
    assert!(kq.watched_count() >= 5, "expected dirs + existing leaves watched, got {}", kq.watched_count());
    let clean = kq.poll_dirty(&frame);
    assert!(clean.paths.is_empty() && !clean.full_rescan, "freshly armed kqueue must be clean: {:?}", clean.paths.as_slice());

    // 2. In-place write -> synchronously detected on kevent(0).
    write_file(&proj, "src/lib.rs", "pub fn a() -> u32 { 10 }\n");
    let after_write = kq.poll_dirty(&frame);
    assert!(
        after_write.paths.iter().any(|&p| p == "src/lib.rs"),
        "in-place write not detected: {:?}",
        after_write.paths.as_slice(),
    );

    // 3. Truncate -> synchronously detected on kevent(0).
    {
        let f = std::fs::OpenOptions::new().write(true).open(proj.join("src/other.rs")).expect("open other.rs");
        f.set_len(0).expect("truncate other.rs");
    }
    let after_trunc = kq.poll_dirty(&frame);
    assert!(
        after_trunc.paths.iter().any(|&p| p == "src/other.rs"),
        "truncate not detected: {:?}",
        after_trunc.paths.as_slice(),
    );

    // 4. Atomic tmp -> rename (vim / VS Code save) -> synchronously detected + marks needs_refresh!
    write_file(&proj, "src/.lib.rs.tmp", "pub fn a() -> u32 { 999 }\n");
    std::fs::rename(proj.join("src/.lib.rs.tmp"), proj.join("src/lib.rs")).expect("atomic rename");
    let after_rename = kq.poll_dirty(&frame);
    assert!(
        after_rename.paths.iter().any(|&p| p == "src/lib.rs"),
        "atomic tmp -> rename not detected: {:?}",
        after_rename.paths.as_slice(),
    );
    assert!(after_rename.needs_refresh, "atomic rename must mark watch refresh needed");

    // Re-arm after atomic rename so new inode is watched.
    let bytes2 = build_leaves_bytes(None);
    let (leaves2, _) = Leaves::of(&bytes2).expect("decode leaves2");
    let mut kq = KqueueVnode::arm(&root_s, None, &leaves2, alloc).expect("re-arm after rename");
    let clean2 = kq.poll_dirty(&frame);
    assert!(clean2.paths.is_empty(), "re-armed kqueue must be clean: {:?}", clean2.paths.as_slice());

    // 5. New file created in watched dir (filling an ABSENT slot) -> synchronously detected!
    write_file(&proj, "src/new_mod.rs", "pub fn fresh() -> u32 { 5 }\n");
    let after_create = kq.poll_dirty(&frame);
    assert!(
        after_create.paths.iter().any(|&p| p == "src/new_mod.rs"),
        "new file in watched dir (ABSENT slot) not detected: {:?}",
        after_create.paths.as_slice(),
    );

    // Re-arm now that `src/new_mod.rs` exists.
    let bytes3 = build_leaves_bytes(Some(stat_of("src/new_mod.rs")));
    let (leaves3, _) = Leaves::of(&bytes3).expect("decode leaves3");
    let mut kq = KqueueVnode::arm(&root_s, None, &leaves3, alloc).expect("re-arm with new_mod.rs");
    let clean3 = kq.poll_dirty(&frame);
    assert!(clean3.paths.is_empty(), "re-armed kqueue with new_mod.rs must be clean");

    // 6. Unlink -> synchronously detected + marks needs_refresh!
    std::fs::remove_file(proj.join("src/new_mod.rs")).expect("unlink new_mod.rs");
    let after_unlink = kq.poll_dirty(&frame);
    assert!(
        after_unlink.paths.iter().any(|&p| p == "src/new_mod.rs"),
        "unlink not detected: {:?}",
        after_unlink.paths.as_slice(),
    );
    assert!(after_unlink.needs_refresh, "unlink must mark watch refresh needed");

    // 7. Zero-TOCTOU post-EV_ADD stale-stamp test: pass a stale stamp for `src/lib.rs` ->
    //    FIRST poll_dirty MUST report `src/lib.rs` dirty even with ZERO subsequent writes!
    let mut stale_rows = Rows::new_in(&frame);
    let mut stale_stamp = stat_of("src/lib.rs");
    stale_stamp.len = stale_stamp.len.wrapping_add(17);
    stale_rows.push(LeafKind::Manifest, 0, "Cargo.toml", Some(stat_of("Cargo.toml")), zero_id);
    stale_rows.push(LeafKind::Slot, 0, "src/lib.rs", Some(stale_stamp), zero_id);
    let stale_bytes = Leaves::bytes_in(&stale_rows, &frame).expect("encode stale leaves");
    let (stale_leaves, _) = Leaves::of(&stale_bytes).expect("decode stale leaves");
    let mut kq_stale = KqueueVnode::arm(&root_s, None, &stale_leaves, alloc).expect("arm with stale stamp");
    let first_poll = kq_stale.poll_dirty(&frame);
    assert!(
        first_poll.paths.iter().any(|&p| p == "src/lib.rs"),
        "zero-TOCTOU post-EV_ADD stamp verification failed to catch stale stamp: {:?}",
        first_poll.paths.as_slice(),
    );
}

#[cfg(unix)]
#[test]
fn daemon_catches_rapid_back_to_back_equal_length_and_type_error_edits() {
    let mut scratch = Scratch::new("rapid");
    let (d_home, d_proj) = scratch.lane("d", true);
    let (s_home, s_proj) = scratch.lane("s", true);

    let cargo_toml = "[package]\nname = \"rapid\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[lib]\npath = \"src/lib.rs\"\n";
    let init_lib = "pub fn answer() -> u32 {\n    1000\n}\n";

    for proj in [&d_proj, &s_proj] {
        write_file(proj, "Cargo.toml", cargo_toml);
        write_file(proj, "src/lib.rs", init_lib);
    }

    let sock = daemon_socket_path(&d_home, &d_proj);

    // Cold check spawns daemon.
    let d0 = run_blaze(&d_proj, &d_home, &["check"], false);
    let s0 = run_blaze(&s_proj, &s_home, &["check"], true);
    assert_step_matches("rapid-0-cold", &d_proj, &d_home, &d0, &s_proj, &s_home, &s0);
    assert!(wait_for_socket(&sock, SOCKET_WAIT), "daemon socket did not appear");
    let pid = find_daemon_pid(&sock).expect("live daemon pid");

    // Warm #1 records env_fp; Warm #2 hits fast path.
    let d_w1 = run_blaze(&d_proj, &d_home, &["check"], false);
    let s_w1 = run_blaze(&s_proj, &s_home, &["check"], true);
    assert_step_matches("rapid-w1", &d_proj, &d_home, &d_w1, &s_proj, &s_home, &s_w1);
    let d_w2 = run_blaze(&d_proj, &d_home, &["check"], false);
    let s_w2 = run_blaze(&s_proj, &s_home, &["check"], true);
    assert_step_matches("rapid-w2-fast", &d_proj, &d_home, &d_w2, &s_proj, &s_home, &s_w2);

    // Back-to-back edits with 0 ms sleep!
    // Edit #1 (same byte length = 37):
    let edit1 = "pub fn answer() -> u32 {\n    1001\n}\n";
    assert_eq!(edit1.len(), init_lib.len());
    for proj in [&d_proj, &s_proj] {
        write_file(proj, "src/lib.rs", edit1);
    }
    let d1 = run_blaze(&d_proj, &d_home, &["check"], false);
    let prof1 = std::fs::read_to_string(d_proj.join(".blaze/v2/profile.txt")).expect("read prof1");
    assert!(
        prof1.contains("driver: ready") && !prof1.contains("certificate stood"),
        "edit #1 must run driver: ready, got:\n{prof1}",
    );
    // IMMEDIATELY (0 ms sleep!) write Edit #2 (EXACT SAME BYTE LENGTH!) before even running standalone for #1!
    let edit2 = "pub fn answer() -> u32 {\n    1002\n}\n";
    assert_eq!(edit2.len(), edit1.len());
    write_file(&d_proj, "src/lib.rs", edit2);
    let d2 = run_blaze(&d_proj, &d_home, &["check"], false);
    let prof2 = std::fs::read_to_string(d_proj.join(".blaze/v2/profile.txt")).expect("read prof2");
    assert!(
        prof2.contains("driver: ready") && !prof2.contains("certificate stood"),
        "edit #2 (0ms after edit #1, equal length!) was MISSED! profile:\n{prof2}",
    );
    // IMMEDIATELY (0 ms sleep!) write Edit #3 (type error!):
    let edit3 = "pub fn answer() -> u32 {\n    \"oops\"\n}\n";
    write_file(&d_proj, "src/lib.rs", edit3);
    let d3 = run_blaze(&d_proj, &d_home, &["check"], false);
    let prof3 = std::fs::read_to_string(d_proj.join(".blaze/v2/profile.txt")).expect("read prof3");
    assert_eq!(d3.code, 1, "edit #3 (type error 0ms after edit #2) must fail with exit 1! profile:\n{prof3}");
    assert!(
        prof3.contains("driver: ready") && !prof3.contains("certificate stood"),
        "edit #3 must run driver: ready, got:\n{prof3}",
    );
    // IMMEDIATELY (0 ms sleep!) write Edit #4 (fix edit!):
    let edit4 = "pub fn answer() -> u32 {\n    1004\n}\n";
    write_file(&d_proj, "src/lib.rs", edit4);
    let d4 = run_blaze(&d_proj, &d_home, &["check"], false);
    let prof4 = std::fs::read_to_string(d_proj.join(".blaze/v2/profile.txt")).expect("read prof4");
    assert_ne!(d4.code, 1, "edit #4 (fix 0ms after error edit) must clear error! profile:\n{prof4}");
    assert!(
        prof4.contains("driver: ready") && !prof4.contains("certificate stood"),
        "edit #4 must run driver: ready, got:\n{prof4}",
    );
    // IMMEDIATELY (0 ms sleep!) atomic rename edit #5 (forces kqueue re-arm before Done!):
    let edit5 = "pub fn answer() -> u32 {\n    1005\n}\n";
    write_file(&d_proj, "src/.lib.rs.tmp", edit5);
    std::fs::rename(d_proj.join("src/.lib.rs.tmp"), d_proj.join("src/lib.rs")).expect("atomic rename #5");
    let d5 = run_blaze(&d_proj, &d_home, &["check"], false);
    let prof5 = std::fs::read_to_string(d_proj.join(".blaze/v2/profile.txt")).expect("read prof5");
    assert!(
        prof5.contains("driver: ready") && !prof5.contains("certificate stood"),
        "edit #5 (atomic rename) must run driver: ready, got:\n{prof5}",
    );
    // IMMEDIATELY (0 ms sleep!) write Edit #6 (type error right after atomic-rename re-arm!):
    let edit6 = "pub fn answer() -> u32 {\n    \"boom\"\n}\n";
    write_file(&d_proj, "src/lib.rs", edit6);
    let d6 = run_blaze(&d_proj, &d_home, &["check"], false);
    let prof6 = std::fs::read_to_string(d_proj.join(".blaze/v2/profile.txt")).expect("read prof6");
    assert_eq!(d6.code, 1, "edit #6 (type error 0ms after atomic rename re-arm) was MISSED! profile:\n{prof6}");

    // Now run standalone through the exact same sequence and verify every output matches byte-for-byte!
    let s1 = run_blaze(&s_proj, &s_home, &["check"], true);
    assert_eq!(normalize_root(&d1.stdout, &d_proj), normalize_root(&s1.stdout, &s_proj), "edit #1 stdout mismatch");
    write_file(&s_proj, "src/lib.rs", edit2);
    let s2 = run_blaze(&s_proj, &s_home, &["check"], true);
    assert_eq!(normalize_root(&d2.stdout, &d_proj), normalize_root(&s2.stdout, &s_proj), "edit #2 stdout mismatch");
    write_file(&s_proj, "src/lib.rs", edit3);
    let s3 = run_blaze(&s_proj, &s_home, &["check"], true);
    assert_eq!(d3.code, s3.code);
    assert_eq!(normalize_root(&d3.stdout, &d_proj), normalize_root(&s3.stdout, &s_proj), "edit #3 stdout mismatch");
    write_file(&s_proj, "src/lib.rs", edit4);
    let s4 = run_blaze(&s_proj, &s_home, &["check"], true);
    assert_eq!(d4.code, s4.code);
    assert_eq!(normalize_root(&d4.stdout, &d_proj), normalize_root(&s4.stdout, &s_proj), "edit #4 stdout mismatch");
    write_file(&s_proj, "src/lib.rs", edit5);
    let s5 = run_blaze(&s_proj, &s_home, &["check"], true);
    assert_eq!(normalize_root(&d5.stdout, &d_proj), normalize_root(&s5.stdout, &s_proj), "edit #5 stdout mismatch");
    write_file(&s_proj, "src/lib.rs", edit6);
    let s6 = run_blaze(&s_proj, &s_home, &["check"], true);
    assert_step_matches("rapid-6-final-error", &d_proj, &d_home, &d6, &s_proj, &s_home, &s6);

    // Fix once more, then verify the warm no-op immediately hits the fast path on the same daemon PID!
    let edit7 = "pub fn answer() -> u32 {\n    1007\n}\n";
    for proj in [&d_proj, &s_proj] {
        write_file(proj, "src/lib.rs", edit7);
    }
    let d7 = run_blaze(&d_proj, &d_home, &["check"], false);
    let s7 = run_blaze(&s_proj, &s_home, &["check"], true);
    assert_step_matches("rapid-7-fix", &d_proj, &d_home, &d7, &s_proj, &s_home, &s7);

    let d8 = run_blaze(&d_proj, &d_home, &["check"], false);
    let s8 = run_blaze(&s_proj, &s_home, &["check"], true);
    assert_step_matches("rapid-8-warm-fast", &d_proj, &d_home, &d8, &s_proj, &s_home, &s8);
    let prof8 = std::fs::read_to_string(d_proj.join(".blaze/v2/profile.txt")).expect("read prof8");
    assert!(
        prof8.contains("resident daemon fast path"),
        "expected warm no-op right after in-place fix edit to hit fast path, got:\n{prof8}",
    );
    assert_eq!(find_daemon_pid(&sock), Some(pid), "same daemon served all rapid edits");
}

#[cfg(target_os = "macos")]
#[test]
fn daemon_kqueue_unsettled_none_stamp_marks_dirty() {
    use blaze_ids::ContentId;
    use crate::daemon_watch::KqueueVnode;
    use crate::sweep::{DIRECT_PATH, LeafKind, Leaves, Rows, Stamp};

    let mut scratch = Scratch::new("kqnone");
    let (_home, proj) = scratch.lane("k", false);

    write_file(&proj, "Cargo.toml", "[package]\nname = \"kqnone\"\nversion = \"0.1.0\"\nedition = \"2024\"\n");
    write_file(&proj, "src/lib.rs", "pub fn a() -> u32 { 1 }\n");
    write_file(&proj, "src/dup.rs", "pub fn b() -> u32 { 2 }\n");

    let stat_of = |rel: &str| -> Stamp {
        use std::os::unix::fs::MetadataExt;
        let md = std::fs::symlink_metadata(proj.join(rel)).unwrap_or_else(|e| panic!("lstat {rel}: {e}"));
        let ns = |secs: i64, nsec: i64| secs.cast_unsigned().wrapping_mul(1_000_000_000).wrapping_add(nsec.cast_unsigned());
        Stamp {
            len: md.len(),
            mtime_ns: ns(md.mtime(), md.mtime_nsec()),
            ctime_ns: ns(md.ctime(), md.ctime_nsec()),
            ino: md.ino(),
            dev: md.dev(),
        }
    };

    let alloc = blaze_own::testing::heap().alloc();
    let frame = bstd::mem::Frame::with_reserve(1 << 20, bstd::mem::Tag::Other).expect("frame");
    let root_s = proj.to_string_lossy();
    let zero_id = ContentId([0; 32]);

    // Case 1: Unsettled file (`row.stamp = None`) + unsettled Listing dir (`row.stamp = None`)
    // + duplicate path (`Slot` with `Some(stamp)` AND `ScriptFile` with `None`).
    let mut rows = Rows::new_in(&frame);
    rows.push(LeafKind::Manifest, 0, "Cargo.toml", Some(stat_of("Cargo.toml")), zero_id);
    rows.push(LeafKind::Slot, 0, "src/lib.rs", None, zero_id);
    rows.push(LeafKind::Slot, 0, "src/dup.rs", Some(stat_of("src/dup.rs")), zero_id);
    rows.push(LeafKind::ScriptFile, DIRECT_PATH, "src/dup.rs", None, zero_id);
    rows.push(LeafKind::Listing, 0, "src", None, zero_id);
    let bytes = Leaves::bytes_in(&rows, &frame).expect("encode leaves");
    let (leaves, _) = Leaves::of(&bytes).expect("decode leaves");

    let mut kq = KqueueVnode::arm(&root_s, None, &leaves, alloc).expect("KqueueVnode::arm");
    let first = kq.poll_dirty(&frame);
    assert!(
        first.paths.iter().any(|&p| p == "src/lib.rs"),
        "unsettled file (`stamp = None`) MUST be marked dirty on first poll: {:?}",
        first.paths.as_slice(),
    );
    assert!(
        first.paths.iter().any(|&p| p == "src/dup.rs"),
        "duplicate path with one `Some(stamp)` and one `None` MUST be marked dirty: {:?}",
        first.paths.as_slice(),
    );
    assert!(
        first.paths.iter().any(|&p| p == "src"),
        "unsettled Listing dir (`stamp = None`) MUST be marked dirty: {:?}",
        first.paths.as_slice(),
    );
    assert!(
        !first.needs_refresh,
        "stamp-only `None` mismatches on live watched inodes must NOT force a full kqueue rebuild",
    );

    // Second poll (after initial_dirty drained, with no disk writes) must be clean!
    let second = kq.poll_dirty(&frame);
    assert!(
        second.paths.is_empty() && !second.needs_refresh && !second.full_rescan,
        "second poll after draining initial_dirty with no edits must be clean: {:?}",
        second.paths.as_slice(),
    );

    // Subsequent in-place write to the previously-unsettled file must still be caught synchronously!
    write_file(&proj, "src/lib.rs", "pub fn a() -> u32 { 2 }\n");
    let third = kq.poll_dirty(&frame);
    assert!(
        third.paths.iter().any(|&p| p == "src/lib.rs"),
        "in-place write after draining initial_dirty must be caught: {:?}",
        third.paths.as_slice(),
    );
}

#[cfg(target_os = "macos")]
#[test]
fn daemon_fast_certified_fires_with_direct_script_and_git_leaves() {
    use blaze_ids::ContentId;
    use crate::daemon_watch::KqueueVnode;
    use crate::sweep::{DIRECT_PATH, EXECUTOR_FILE, LeafKind, Leaves, PINS_FILE, Rows, SHIM_FILE, Stamp};

    let mut scratch = Scratch::new("fastsg");
    let (_home, proj) = scratch.lane("g", false);
    let fake_exe_dir = scratch.base.join("bin");
    std::fs::create_dir_all(&fake_exe_dir).expect("mkdir fake_exe_dir");

    write_file(&proj, "Cargo.toml", "[package]\nname = \"sg\"\nversion = \"0.1.0\"\nedition = \"2024\"\n");
    write_file(&proj, "src/lib.rs", "pub fn ok() -> u32 { 1 }\n");
    write_file(&proj, "data.txt", "hello\n");

    // Initialize a real git repository with one commit.
    let run_git = |args: &[&str]| {
        let out = Command::new("git")
            .args(args)
            .current_dir(&proj)
            .env("GIT_AUTHOR_NAME", "test")
            .env("GIT_AUTHOR_EMAIL", "test@example.com")
            .env("GIT_COMMITTER_NAME", "test")
            .env("GIT_COMMITTER_EMAIL", "test@example.com")
            .output()
            .expect("spawn git");
        assert!(
            out.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&out.stderr),
        );
    };
    run_git(&["init", "-b", "main"]);
    run_git(&["add", "."]);
    run_git(&["commit", "-m", "initial"]);

    let stat_of = |rel: &str| -> Stamp {
        use std::os::unix::fs::MetadataExt;
        let md = std::fs::symlink_metadata(proj.join(rel)).unwrap_or_else(|e| panic!("lstat {rel}: {e}"));
        let ns = |secs: i64, nsec: i64| secs.cast_unsigned().wrapping_mul(1_000_000_000).wrapping_add(nsec.cast_unsigned());
        Stamp {
            len: md.len(),
            mtime_ns: ns(md.mtime(), md.mtime_nsec()),
            ctime_ns: ns(md.ctime(), md.ctime_nsec()),
            ino: md.ino(),
            dev: md.dev(),
        }
    };

    let alloc = blaze_own::testing::heap().alloc();
    let frame = bstd::mem::Frame::with_reserve(1 << 20, bstd::mem::Tag::Other).expect("frame");
    let root_s = proj.to_string_lossy();
    let exe_s = fake_exe_dir.to_string_lossy();
    let zero_id = ContentId([0; 32]);

    // Compute real FACT_MOUNT digests matching `fake_exe_dir` (where shim is absent -> NONE)
    // and this host's pinned Apple userland so `verify_fact_leaves` passes!
    let erased = blaze_own::Erased::new(&frame);
    let setting = blaze_proc_macro::fact_pins::command_spawn_setting(crate::graph::TRIPLE, erased.handle())
        .expect("command_spawn_setting");
    let pins_id = blaze_proc_macro::native_facts::pins_digest(setting.pins.as_ref(), erased.handle());
    let exec_id = setting.executor_id(erased.handle()).unwrap_or(crate::engine_script_reads::NONE);
    let shim_id = crate::engine_script_reads::NONE;

    let mut rows = Rows::new_in(&frame);
    rows.push(LeafKind::Manifest, 0, "Cargo.toml", Some(stat_of("Cargo.toml")), zero_id);
    rows.push(LeafKind::Slot, 0, "src/lib.rs", Some(stat_of("src/lib.rs")), zero_id);
    rows.push(LeafKind::ScriptFile, DIRECT_PATH, "data.txt", None, zero_id);
    rows.push(LeafKind::ScriptFile, 0, PINS_FILE, None, pins_id);
    rows.push(LeafKind::ScriptFile, 0, EXECUTOR_FILE, None, exec_id);
    rows.push(LeafKind::ScriptFile, 0, SHIM_FILE, None, shim_id);
    rows.push(LeafKind::ScriptGit, 0, "\nrev-parse HEAD", None, zero_id);
    let bytes = Leaves::bytes_in(&rows, &frame).expect("encode leaves");
    let (leaves, _) = Leaves::of(&bytes).expect("decode leaves");

    let mut kq = KqueueVnode::arm_with_env(&root_s, None, Some(&exe_s), &leaves, true, &frame, alloc)
        .expect("arm_with_env with direct script + FACT_MOUNT + ScriptGit");

    // Poll #1 drains initial verification for `data.txt` (unstamped ScriptFile) and `git_dirty`
    // (initial post-EV_ADD git check), WITHOUT requiring a full kqueue rebuild!
    let p1 = kq.poll_dirty(&frame);
    assert!(p1.paths.iter().any(|&p| p == "data.txt"), "unstamped ScriptFile must verify once on poll #1");
    assert!(p1.git_dirty, "non-status ScriptGit must verify once on poll #1");
    assert!(!p1.facts_dirty, "matching FACT_MOUNT leaves must be immediately clean");
    assert!(!p1.needs_refresh, "initial verification must keep kqueue alive");

    // Poll #2 (nothing changed) -> 100% CLEAN (fast certified path eligible)!
    let p2 = kq.poll_dirty(&frame);
    assert!(
        p2.paths.is_empty() && !p2.git_dirty && !p2.facts_dirty && !p2.home_dirty && !p2.needs_refresh && !p2.full_rescan,
        "expected completely clean poll #2 with direct script + FACT_MOUNT + ScriptGit leaves",
    );

    // Mutate direct ScriptFile `data.txt` -> immediately caught!
    write_file(&proj, "data.txt", "changed\n");
    let p3 = kq.poll_dirty(&frame);
    assert!(
        p3.paths.iter().any(|&p| p == "data.txt"),
        "direct ScriptFile edit not detected: {:?}",
        p3.paths.as_slice(),
    );

    // Next poll clean again without rebuilding kqueue!
    let p4 = kq.poll_dirty(&frame);
    assert!(p4.paths.is_empty() && !p4.git_dirty && !p4.facts_dirty && !p4.needs_refresh);

    // New git commit on `main` -> immediately invalidates via `.git/refs/heads/main`!
    run_git(&["commit", "--allow-empty", "-m", "second"]);
    let p5 = kq.poll_dirty(&frame);
    assert!(p5.git_dirty && p5.needs_refresh, "git commit MUST immediately set git_dirty + needs_refresh!");

    // Creating `blaze-fact-shim` in `fake_exe_dir` (was absent at arm time) -> immediately invalidates facts!
    std::fs::write(fake_exe_dir.join(blaze_build_script::fact_shim::SHIM_NAME), b"fake-shim").expect("write fake shim");
    let p6 = kq.poll_dirty(&frame);
    assert!(p6.facts_dirty && p6.needs_refresh, "creating blaze-fact-shim MUST immediately set facts_dirty + needs_refresh!");
}

#[cfg(unix)]
#[test]
fn daemon_end_to_end_fast_path_with_direct_script_file_leaf() {
    let mut scratch = Scratch::new("e2esg");
    let (d_home, d_proj) = scratch.lane("d", true);
    let (s_home, s_proj) = scratch.lane("s", true);

    let cargo_toml = "[package]\nname = \"sg_e2e\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[lib]\npath = \"src/lib.rs\"\n";
    // Computed `include_str!(concat!(...))` produces a real `LeafKind::ScriptFile` with `DIRECT_PATH`.
    let lib_rs = "pub fn data() -> &'static str {\n    include_str!(concat!(env!(\"CARGO_MANIFEST_DIR\"), \"/data.txt\"))\n}\n";

    for proj in [&d_proj, &s_proj] {
        write_file(proj, "Cargo.toml", cargo_toml);
        write_file(proj, "src/lib.rs", lib_rs);
        write_file(proj, "data.txt", "good\n");
    }

    let sock = daemon_socket_path(&d_home, &d_proj);

    // Cold check spawns daemon.
    let d1 = run_blaze(&d_proj, &d_home, &["check"], false);
    let s1 = run_blaze(&s_proj, &s_home, &["check"], true);
    assert_step_matches("e2esg-1-cold", &d_proj, &d_home, &d1, &s_proj, &s_home, &s1);
    assert!(wait_for_socket(&sock, SOCKET_WAIT), "daemon socket did not appear");

    // Warm #1 drains initial verification of `data.txt` (unstamped ScriptFile) + records env_fp.
    let d2 = run_blaze(&d_proj, &d_home, &["check"], false);
    let s2 = run_blaze(&s_proj, &s_home, &["check"], true);
    assert_step_matches("e2esg-2-warm1", &d_proj, &d_home, &d2, &s_proj, &s_home, &s2);

    // Warm #2 MUST hit `resident daemon fast path` even with a direct `ScriptFile` leaf!
    let d3 = run_blaze(&d_proj, &d_home, &["check"], false);
    let s3 = run_blaze(&s_proj, &s_home, &["check"], true);
    assert_step_matches("e2esg-3-warm-fast", &d_proj, &d_home, &d3, &s_proj, &s_home, &s3);
    let prof3 = std::fs::read_to_string(d_proj.join(".blaze/v2/profile.txt")).expect("read prof3");
    assert!(
        prof3.contains("resident daemon fast path"),
        "expected warm no-op with direct ScriptFile leaf to hit fast path, got:\n{prof3}",
    );

    // Editing `data.txt` MUST immediately invalidate the certificate and match standalone!
    for proj in [&d_proj, &s_proj] {
        write_file(proj, "data.txt", "updated-data\n");
    }
    let d4 = run_blaze(&d_proj, &d_home, &["check"], false);
    let s4 = run_blaze(&s_proj, &s_home, &["check"], true);
    let prof4 = std::fs::read_to_string(d_proj.join(".blaze/v2/profile.txt")).expect("read prof4");
    assert!(
        !prof4.contains("certificate stood"),
        "editing direct ScriptFile `data.txt` MUST invalidate cached answer! profile:\n{prof4}",
    );
    assert_step_matches("e2esg-4-script-file-edit", &d_proj, &d_home, &d4, &s_proj, &s_home, &s4);

    // Next warm no-op immediately hits fast path again!
    let d5 = run_blaze(&d_proj, &d_home, &["check"], false);
    let s5 = run_blaze(&s_proj, &s_home, &["check"], true);
    assert_step_matches("e2esg-5-warm-fast-again", &d_proj, &d_home, &d5, &s_proj, &s_home, &s5);
    let prof5 = std::fs::read_to_string(d_proj.join(".blaze/v2/profile.txt")).expect("read prof5");
    assert!(
        prof5.contains("resident daemon fast path"),
        "expected warm no-op after ScriptFile edit to hit fast path again, got:\n{prof5}",
    );
}

#[cfg(target_os = "macos")]
#[test]
fn kqueue_vnode_does_not_double_count_nonexistent_module_stems() {
    use crate::daemon_watch::{KQUEUE_CAP, KqueueVnode};
    use crate::sweep::{ABSENT, LeafKind, Leaves, MODULE, Rows, Stamp};
    use blaze_ids::ContentId;
    use std::os::unix::fs::MetadataExt;

    let mut scratch = Scratch::new("kqstem");
    let (_home, proj) = scratch.lane("p", true);
    write_file(&proj, "Cargo.toml", "[package]\nname = \"kqstem\"\nversion = \"0.1.0\"\nedition = \"2024\"\n");
    write_file(&proj, "src/lib.rs", "pub mod m0;\n");
    write_file(&proj, "src/m0.rs", "pub fn x() -> u8 { 1 }\n");
    // Create ONE existing stem directory `src/m0/` so we also prove Phase 1 still registers
    // existing `MODULE` stem directories before Phase 2.
    std::fs::create_dir_all(proj.join("src/m0")).expect("mkdir src/m0");

    let stat_of = |rel: &str| -> Stamp {
        let md = std::fs::symlink_metadata(proj.join(rel)).expect("stat");
        Stamp {
            len: md.len(),
            mtime_ns: md.mtime().cast_unsigned().wrapping_mul(1_000_000_000).wrapping_add(md.mtime_nsec().cast_unsigned()),
            ctime_ns: md.ctime().cast_unsigned().wrapping_mul(1_000_000_000).wrapping_add(md.ctime_nsec().cast_unsigned()),
            ino: md.ino(),
            dev: md.dev(),
        }
    };

    let alloc = blaze_own::testing::heap().alloc();
    let frame = bstd::mem::Frame::with_reserve(16 << 20, bstd::mem::Tag::Other).expect("frame");
    let root_s = proj.to_string_lossy();
    let zero_id = ContentId([0; 32]);

    // Add `KQUEUE_CAP + 256` absent `MODULE` slots whose `src/ghost_N/` stem directories do NOT
    // exist on disk. Before Wave 8, inserting every `MODULE` stem into `dir_set` inflated
    // `estimated > KQUEUE_CAP` and caused `KqueueVnode::arm_with_env` to bail out before opening
    // a single FD; with stem dirs tracked separately, only real existing dirs + files count.
    let mut rows = Rows::new_in(&frame);
    rows.push(LeafKind::Manifest, 0, "Cargo.toml", Some(stat_of("Cargo.toml")), zero_id);
    rows.push(LeafKind::Slot, 0, "src/lib.rs", Some(stat_of("src/lib.rs")), zero_id);
    rows.push(LeafKind::Slot, MODULE, "src/m0.rs", Some(stat_of("src/m0.rs")), zero_id);
    for i in 0..(KQUEUE_CAP + 256) {
        let rel = std::format!("src/ghost_{i}.rs");
        rows.push(LeafKind::Slot, MODULE | ABSENT, &rel, None, zero_id);
    }
    let bytes = Leaves::bytes_in(&rows, &frame).expect("encode leaves");
    let (leaves, _) = Leaves::of(&bytes).expect("decode leaves");

    let mut kq = KqueueVnode::arm_with_env(&root_s, None, None, &leaves, true, &frame, alloc)
        .expect("nonexistent MODULE stem dirs must NOT blow KQUEUE_CAP");
    // Only `""`, `"src"`, `"src/m0"`, `"Cargo.toml"`, `"src/lib.rs"`, `"src/m0.rs"` = 6 FDs!
    assert_eq!(kq.watched_count(), 6, "expected exactly 6 open FDs (3 real dirs + 3 real files)");

    let clean = kq.poll_dirty(&frame);
    assert!(clean.paths.is_empty() && !clean.needs_refresh && !clean.full_rescan, "initial poll must be clean");

    // Existing stem dir `src/m0/` MUST still be watched so creating `src/m0/mod.rs` fires immediately.
    write_file(&proj, "src/m0/mod.rs", "pub fn y() -> u8 { 2 }\n");
    let dirty = kq.poll_dirty(&frame);
    assert!(
        dirty.paths.iter().any(|&p| p == "src/m0.rs" || p == "src/m0"),
        "creating src/m0/mod.rs inside existing stem dir must be caught: {:?}",
        dirty.paths.as_slice(),
    );
}

#[cfg(target_os = "macos")]
#[test]
fn kqueue_vnode_script_stat_stamped_and_symlinks_do_not_loop_refresh() {
    use crate::daemon_watch::KqueueVnode;
    use crate::engine_script_reads::{NONE, NOT_A_FILE, NOT_A_FOLDER, ReadKind};
    use crate::sweep::{DIRECT_PATH, LeafKind, Leaves, Plant, Rows, Stamp};

    let mut scratch = Scratch::new("kqsym");
    let (_home, proj) = scratch.lane("p", true);
    write_file(&proj, "Cargo.toml", "[package]\nname = \"kqsym\"\nversion = \"0.1.0\"\nedition = \"2024\"\n");
    write_file(&proj, "src/lib.rs", "pub fn ok() -> u8 { 1 }\n");
    write_file(&proj, "real_dir/target.txt", "hello-symlink\n");
    symlink(proj.join("real_dir/target.txt"), proj.join("src/link.txt")).expect("file symlink");
    symlink(proj.join("real_dir"), proj.join("link_dir")).expect("dir symlink");

    // 1. Verify `engine_script_reads::now` preserves settled stamps for `ReadKind::Stat`,
    //    `(ReadKind::File, dir -> NOT_A_FILE)`, and `(ReadKind::Listing, file -> NOT_A_FOLDER)`.
    struct FixedStamps;
    impl crate::sweep::Stamps for FixedStamps {
        fn stamp(&self, _path: &str, meta: &bstd::fs::Meta) -> Option<Stamp> {
            Some(Stamp { len: meta.len, mtime_ns: 1, ctime_ns: 1, ino: 42, dev: 7 })
        }
        fn swept_at_ns(&self) -> u64 {
            10_000_000_000
        }
        fn granularity_ns(&self) -> u64 {
            1_000_000_000
        }
    }
    let sim = bstd::io::Io::simulated(bstd::io::SimWorld::new(77, bstd::io::Limits::USUAL));
    let cx = sim.cx(bstd::test::BUDGET, bstd::mem::TASK_FRAME).expect("cx");
    let sim_root = sim.need_fs().expect("fs").open_dir(&cx, &bstd::path::Path::new("ws").expect("path")).expect("open sim");
    sim_root
        .publish(&cx, &bstd::path::Path::new("real_dir/target.txt").expect("path"), bstd::fs::Content::bytes(b"hello-symlink\n"), bstd::fs::Durable::No)
        .expect("write sim");
    let frame = bstd::mem::Frame::with_reserve(16 << 20, bstd::mem::Tag::Other).expect("frame");
    let stamps = FixedStamps;

    let (_, stat_file_stamp, stat_file_id) = crate::engine_script_reads::now(&cx, &sim_root, &stamps, Plant::None, ReadKind::Stat, "real_dir/target.txt", None, &frame).expect("stat file");
    assert!(stat_file_stamp.is_some(), "ReadKind::Stat of a file must keep its settled stamp");
    let (_, stat_dir_stamp, stat_dir_id) = crate::engine_script_reads::now(&cx, &sim_root, &stamps, Plant::None, ReadKind::Stat, "real_dir", None, &frame).expect("stat dir");
    assert!(stat_dir_stamp.is_some(), "ReadKind::Stat of a dir must keep its settled stamp");
    let (_, not_file_stamp, not_file_id) = crate::engine_script_reads::now(&cx, &sim_root, &stamps, Plant::None, ReadKind::File, "real_dir", None, &frame).expect("file on dir");
    assert_eq!(not_file_id, NOT_A_FILE);
    assert!(not_file_stamp.is_some(), "ReadKind::File on dir (NOT_A_FILE) must keep its settled stamp");
    let (_, not_dir_stamp, not_dir_id) = crate::engine_script_reads::now(&cx, &sim_root, &stamps, Plant::None, ReadKind::Listing, "real_dir/target.txt", None, &frame).expect("listing on file");
    assert_eq!(not_dir_id, NOT_A_FOLDER);
    assert!(not_dir_stamp.is_some(), "ReadKind::Listing on file (NOT_A_FOLDER) must keep its settled stamp");

    // 2. Verify `KqueueVnode` with stamped `ScriptStat`, symlinked file, symlinked dir, and absent
    //    `ScriptStat` arms cleanly with ZERO `initial_dirty` and `!needs_refresh` (no infinite re-arm!).
    let target_stat_of = |rel: &str| -> Stamp {
        let md = std::fs::metadata(proj.join(rel)).expect("stat following symlink");
        Stamp {
            len: md.len(),
            mtime_ns: md.mtime().cast_unsigned().wrapping_mul(1_000_000_000).wrapping_add(md.mtime_nsec().cast_unsigned()),
            ctime_ns: md.ctime().cast_unsigned().wrapping_mul(1_000_000_000).wrapping_add(md.ctime_nsec().cast_unsigned()),
            ino: md.ino(),
            dev: md.dev(),
        }
    };

    let alloc = blaze_own::testing::heap().alloc();
    let root_s = proj.to_string_lossy();
    let mut rows = Rows::new_in(&frame);
    rows.push(LeafKind::ScriptStat, DIRECT_PATH, "real_dir/target.txt", Some(target_stat_of("real_dir/target.txt")), stat_file_id);
    rows.push(LeafKind::ScriptStat, DIRECT_PATH, "real_dir", Some(target_stat_of("real_dir")), stat_dir_id);
    rows.push(LeafKind::ScriptFile, DIRECT_PATH, "real_dir", Some(target_stat_of("real_dir")), not_file_id);
    rows.push(LeafKind::ScriptListing, DIRECT_PATH, "real_dir/target.txt", Some(target_stat_of("real_dir/target.txt")), not_dir_id);
    rows.push(LeafKind::ScriptStat, DIRECT_PATH, "real_dir/absent.txt", None, NONE);
    // Symlinked file & symlinked directory (previously looped `initial_needs_refresh = true` forever!).
    rows.push(LeafKind::ScriptStat, 0, "src/link.txt", Some(target_stat_of("src/link.txt")), stat_file_id);
    rows.push(LeafKind::Listing, 0, "link_dir", Some(target_stat_of("link_dir")), blaze_ids::ContentId([1; 32]));
    let bytes = Leaves::bytes_in(&rows, &frame).expect("encode leaves");
    let (leaves, _) = Leaves::of(&bytes).expect("decode leaves");

    let mut kq = KqueueVnode::arm_with_env(&root_s, None, None, &leaves, true, &frame, alloc)
        .expect("KqueueVnode::arm_with_env with stamped ScriptStat + symlinks");
    let p1 = kq.poll_dirty(&frame);
    assert!(
        p1.paths.is_empty() && !p1.needs_refresh && !p1.full_rescan,
        "stamped ScriptStat + symlinks must be immediately clean without refresh loop: paths={:?} needs_refresh={} full_rescan={}",
        p1.paths.as_slice(),
        p1.needs_refresh,
        p1.full_rescan,
    );

    // 3. Mutations to symlink target and creating the absent ScriptStat path are still caught immediately!
    write_file(&proj, "real_dir/target.txt", "hello-edited!!\n");
    let p2 = kq.poll_dirty(&frame);
    assert!(
        p2.paths.iter().any(|&p| p == "src/link.txt") && p2.paths.iter().any(|&p| p == "real_dir/target.txt"),
        "editing symlink target must dirty both symlink and target leaves: {:?}",
        p2.paths.as_slice(),
    );

    write_file(&proj, "real_dir/absent.txt", "appeared\n");
    let p3 = kq.poll_dirty(&frame);
    assert!(
        p3.paths.iter().any(|&p| p == "real_dir/absent.txt") && p3.needs_refresh,
        "creating absent ScriptStat file must dirty it and mark needs_refresh: paths={:?} needs_refresh={}",
        p3.paths.as_slice(),
        p3.needs_refresh,
    );
}

#[test]
fn env_fingerprint_tracks_path_developer_dir_and_toolchains() {
    let heap = blaze_own::testing::heap();
    let shared = heap.shared();
    let alloc = heap.alloc();
    let make_env = |pairs: &[(&str, &str)]| bstd::env::Env::from_vars(bstd::env::Vars::from_pairs_in(pairs, shared));

    let base = super::EnvFingerprint::from_env(&make_env(&[("PATH", "/usr/bin"), ("DEVELOPER_DIR", "/dev1"), ("TOOLCHAINS", "t1")]), alloc);
    let diff_path = super::EnvFingerprint::from_env(&make_env(&[("PATH", "/opt/bin"), ("DEVELOPER_DIR", "/dev1"), ("TOOLCHAINS", "t1")]), alloc);
    let diff_dev = super::EnvFingerprint::from_env(&make_env(&[("PATH", "/usr/bin"), ("DEVELOPER_DIR", "/dev2"), ("TOOLCHAINS", "t1")]), alloc);
    let diff_tc = super::EnvFingerprint::from_env(&make_env(&[("PATH", "/usr/bin"), ("DEVELOPER_DIR", "/dev1"), ("TOOLCHAINS", "t2")]), alloc);
    assert!(base != diff_path, "PATH change must invalidate EnvFingerprint");
    assert!(base != diff_dev, "DEVELOPER_DIR change must invalidate EnvFingerprint");
    assert!(base != diff_tc, "TOOLCHAINS change must invalidate EnvFingerprint");
}

#[test]
fn rebase_relative_origin_from_client_cwd() {
    let frame = bstd::mem::Frame::with_reserve(1 << 20, bstd::mem::Tag::Other).expect("frame");
    assert_eq!(super::rebase_relative_path("/ws/member", "../records", "/ws", &frame), Some("records"));
    assert_eq!(super::rebase_relative_path("/ws/member", "sub/records", "/ws", &frame), Some("member/sub/records"));
    assert_eq!(super::rebase_relative_path("/ws/member", "../../outside", "/ws", &frame), None);
    assert_eq!(super::rebase_relative_path("/ws/member", "http://127.0.0.1:8080", "/ws", &frame), None);
    assert_eq!(super::daemon_fs_root_abs("/parent/ws", "ws"), "/parent");
}

#[cfg(unix)]
#[test]
fn locate_workspace_root_respects_exclude_and_package_workspace() {
    let mut scratch = Scratch::new("wsexcl");
    let (_home, proj) = scratch.lane("p", false);
    write_file(&proj, "Cargo.toml", "[workspace]\nmembers = [\"a\"]\nexclude = [\"excl\"]\n");
    write_file(&proj, "a/Cargo.toml", "[package]\nname = \"a\"\nversion = \"0.1.0\"\n");
    write_file(&proj, "excl/Cargo.toml", "[package]\nname = \"excl\"\nversion = \"0.1.0\"\n");
    write_file(&proj, "explicit/Cargo.toml", "[package]\nname = \"explicit\"\nversion = \"0.1.0\"\nworkspace = \"..\"\n");

    let frame = bstd::mem::Frame::with_reserve(1 << 20, bstd::mem::Tag::Other).expect("frame");
    let root_s = proj.to_string_lossy();
    let a_s = proj.join("a").to_string_lossy().into_owned();
    let excl_s = proj.join("excl").to_string_lossy().into_owned();
    let explicit_s = proj.join("explicit").to_string_lossy().into_owned();

    let mut out = [0u8; 4096];
    assert_eq!(crate::daemon_client::locate_workspace_root(&a_s, &root_s, &frame, &mut out), Some(root_s.as_ref()));
    assert_eq!(crate::daemon_client::locate_workspace_root(&excl_s, &root_s, &frame, &mut out), Some(excl_s.as_str()));
    assert_eq!(crate::daemon_client::locate_workspace_root(&explicit_s, &root_s, &frame, &mut out), None);
}

#[cfg(target_os = "macos")]
#[test]
fn kqueue_vnode_root_symlink_retarget_invalidates_descendants_and_nested_git_watched() {
    use crate::daemon_watch::KqueueVnode;
    use crate::sweep::{LeafKind, Leaves, Rows, Stamp};
    use blaze_sys::posix::io::CWD;
    use blaze_sys::posix::proc::rename_at;

    fn cpath<'a>(p: &Path, buf: &'a mut [u8; 4096]) -> blaze_sys::Path<'a> {
        let b = p.to_str().expect("utf8").as_bytes();
        buf[..b.len()].copy_from_slice(b);
        buf[b.len()] = 0;
        blaze_sys::Path::new(core::ffi::CStr::from_bytes_with_nul(&buf[..=b.len()]).expect("cstr"))
    }
    let make_link = |target: &Path, link: &Path| {
        symlink(target, link).expect("symlink");
    };

    let mut scratch = Scratch::new("kqretarget");
    let (_home, proj) = scratch.lane("p", false);
    write_file(&proj, "target_a/Cargo.toml", "[package]\nname = \"m\"\nversion = \"0.1.0\"\n");
    write_file(&proj, "target_a/src/lib.rs", "pub fn a() -> u32 { 1 }\n");
    write_file(&proj, "target_b/Cargo.toml", "[package]\nname = \"m\"\nversion = \"0.1.0\"\n");
    write_file(&proj, "target_b/src/lib.rs", "pub fn a() -> u32 { 2 }\n");
    make_link(&proj.join("target_a"), &proj.join("member"));

    // Nested git repository inside `sub/.git`.
    write_file(&proj, "sub/.git/HEAD", "ref: refs/heads/main\n");
    write_file(&proj, "sub/.git/refs/heads/main", "1111111111111111111111111111111111111111\n");

    let target_stat_of = |rel: &str| -> Stamp {
        let st = posix_stat(&proj.join(rel)).expect("stat");
        Stamp {
            len: st.st_size.cast_unsigned(),
            mtime_ns: st.st_mtime.cast_unsigned().wrapping_mul(1_000_000_000).wrapping_add(st.st_mtime_nsec.cast_unsigned()),
            ctime_ns: st.st_ctime.cast_unsigned().wrapping_mul(1_000_000_000).wrapping_add(st.st_ctime_nsec.cast_unsigned()),
            ino: st.st_ino,
            dev: i64::from(st.st_dev).cast_unsigned(),
        }
    };

    let frame = bstd::mem::Frame::with_reserve(4 << 20, bstd::mem::Tag::Other).expect("frame");
    let alloc = blaze_own::testing::heap().alloc();
    let root_s = proj.to_string_lossy();
    let mut rows = Rows::new_in(&frame);
    rows.push(LeafKind::Manifest, 0, "member/Cargo.toml", Some(target_stat_of("member/Cargo.toml")), blaze_ids::ContentId([1; 32]));
    rows.push(LeafKind::Slot, 0, "member/src/lib.rs", Some(target_stat_of("member/src/lib.rs")), blaze_ids::ContentId([2; 32]));
    rows.push(LeafKind::ScriptGit, 0, "sub\nrev-parse HEAD", None, blaze_ids::ContentId([3; 32]));
    let bytes = Leaves::bytes_in(&rows, &frame).expect("encode leaves");
    let (leaves, _) = Leaves::of(&bytes).expect("decode leaves");

    let mut kq = KqueueVnode::arm_with_env(&root_s, None, None, &leaves, true, &frame, alloc).expect("arm");
    // Drain initial git verification.
    let init = kq.poll_dirty(&frame);
    assert!(init.git_dirty, "initial post-EV_ADD git drain expected");

    // 1. Atomic retarget of root symlink `member`: must dirty `member/Cargo.toml` and `member/src/lib.rs`!
    make_link(&proj.join("target_b"), &proj.join("member.tmp"));
    let (mut b1, mut b2) = ([0u8; 4096], [0u8; 4096]);
    rename_at(CWD, cpath(&proj.join("member.tmp"), &mut b1), CWD, cpath(&proj.join("member"), &mut b2)).expect("atomic rename");
    let p_sym = kq.poll_dirty(&frame);
    assert!(
        p_sym.paths.iter().any(|&p| p == "member/Cargo.toml")
            && p_sym.paths.iter().any(|&p| p == "member/src/lib.rs"),
        "retargeting root symlink dir must dirty descendant leaves: {:?}",
        p_sym.paths.as_slice(),
    );

    // 2. Updating the nested repo's `sub/.git/refs/heads/main` must fire `git_dirty`!
    write_file(&proj, "sub/.git/refs/heads/main", "2222222222222222222222222222222222222222\n");
    let p_git = kq.poll_dirty(&frame);
    assert!(p_git.git_dirty, "nested ScriptGit repo ref update must fire git_dirty");
}
