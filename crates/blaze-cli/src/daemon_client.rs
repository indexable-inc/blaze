//! Ultra-fast pre-`bstd::rt` Unix-socket client for the resident per-checkout `blaze` daemon
//! (Wave 7, rule 70).
//!
//! Invoked at the very top of `main()` BEFORE `bstd::entry::run_with_heap_at(...)`, avoiding the
//! entire `bstd::rt` runtime, worker pool, and driver startup/teardown cost on daemon-served
//! `blaze check` requests.
//!
//! On any miss (no socket, stale socket, version mismatch, daemon shutting down, or `Busy`),
//! falls through transparently to standalone execution with zero delay, and after a successful
//! standalone `check`, spawns a detached `blaze daemon` in the background so the next check hits
//! the hot resident daemon.

#[cfg(unix)]
use blaze_own::Vec;
#[cfg(unix)]
use blaze_watch::daemon::{self, CannotServe, CheckReqRef, FORM, Hello, ServerFrame};
#[cfg(unix)]
use blaze_watch::helper::Version;
use bstd::entry::Status;

/// Outcome of attempting to serve the current CLI invocation via the resident daemon.
pub(crate) enum ClientOutcome {
    /// The daemon served the request to completion; `main` exits immediately with this status.
    Exit(Status),
    /// Run standalone in-process; if `spawn_daemon` is true, spawn a detached `blaze daemon`
    /// after a successful standalone `check`.
    Fallthrough { spawn_daemon: bool },
}

/// Attempts to forward `blaze check` / `blaze engine-check` directly to the checkout's resident
/// daemon over its Unix domain socket BEFORE initializing `bstd::rt`.
#[cfg(unix)]
pub(crate) fn try_run() -> ClientOutcome {
    let Some(block) = bstd::entry::start_block() else {
        return ClientOutcome::Fallthrough { spawn_daemon: false };
    };
    let Some(&cmd) = block.args.get(1) else {
        return ClientOutcome::Fallthrough { spawn_daemon: false };
    };
    if cmd != "check" && cmd != "engine-check" {
        return ClientOutcome::Fallthrough { spawn_daemon: false };
    }

    let mut home: Option<&str> = None;
    let mut no_daemon = false;
    for entry in block.env.iter() {
        let bytes = entry.to_bytes();
        if let Some(val) = bytes.strip_prefix(b"HOME=") {
            home = core::str::from_utf8(val).ok();
        } else if let Some(val) = bytes.strip_prefix(b"BLAZE_NO_DAEMON=") {
            no_daemon = val == b"1";
        }
    }
    if no_daemon {
        return ClientOutcome::Fallthrough { spawn_daemon: false };
    }
    let Some(home) = home else {
        return ClientOutcome::Fallthrough { spawn_daemon: false };
    };
    if !crate::daemon_server::socket_fits(Some(home)) {
        return ClientOutcome::Fallthrough { spawn_daemon: false };
    }

    let Some(dir_arg) = parse_check_dir(block.args) else {
        return ClientOutcome::Fallthrough { spawn_daemon: false };
    };
    if let Some(raw) = dir_arg {
        if raw.starts_with('/') || raw.split('/').any(|part| part == "..") {
            return ClientOutcome::Fallthrough { spawn_daemon: false };
        }
        if raw.ends_with(".rs") && is_regular_file(raw) {
            return ClientOutcome::Fallthrough { spawn_daemon: false };
        }
    }

    let Some(build_id) = bstd::host::build_id_self() else {
        return ClientOutcome::Fallthrough { spawn_daemon: false };
    };
    let Some(version) = Version::of(build_id.as_bytes()) else {
        return ClientOutcome::Fallthrough { spawn_daemon: false };
    };

    let mut cwd_buf = [0u8; 4096];
    let Some(cwd_abs) = realpath_str(".", &mut cwd_buf) else {
        return ClientOutcome::Fallthrough { spawn_daemon: false };
    };
    let mut target_buf = [0u8; 4096];
    let target_abs = match dir_arg {
        None | Some(".") => cwd_abs,
        Some(raw) => match realpath_str(raw, &mut target_buf) {
            Some(path) => path,
            None => return ClientOutcome::Fallthrough { spawn_daemon: false },
        },
    };

    let Ok(frame) = bstd::mem::Frame::with_reserve(8 << 20, bstd::mem::Tag::Other) else {
        return ClientOutcome::Fallthrough { spawn_daemon: false };
    };

    let mut ws_buf = [0u8; 4096];
    let Some(ws_root) = locate_workspace_root(target_abs, cwd_abs, &frame, &mut ws_buf) else {
        return ClientOutcome::Fallthrough { spawn_daemon: false };
    };

    let checkout_key = crate::journal_ask::checkout_key_of(ws_root);
    let mut sock_buf = [0u8; 256];
    let Some(sock_path) = format_socket_path(home, &checkout_key, &mut sock_buf) else {
        return ClientOutcome::Fallthrough { spawn_daemon: false };
    };

    let Ok(conn) = blaze_sys::posix::net::connect_unix(sock_path) else {
        return ClientOutcome::Fallthrough { spawn_daemon: true };
    };
    // Raise Unix socket buffers above macOS's 8 KiB default so large stdout payloads (e.g. 519 KB)
    // stream in 1-2 syscalls instead of ~65 round-trips.
    {
        use blaze_sys::posix::net::{SO_RCVBUF, SO_SNDBUF, SOL_SOCKET, setsockopt_int};
        if setsockopt_int(conn.fd(), SOL_SOCKET, SO_RCVBUF, 512 << 10).is_err() {
            let _ = setsockopt_int(conn.fd(), SOL_SOCKET, SO_RCVBUF, 256 << 10);
        }
        let _ = setsockopt_int(conn.fd(), SOL_SOCKET, SO_SNDBUF, 64 << 10);
    }

    // 1. Read Hello frame.
    let mut recv_buf: Vec<u8, &bstd::mem::Frame> = Vec::new_in(&frame);
    let Ok(hello_body) = read_frame_fd(conn.fd(), &mut recv_buf) else {
        return ClientOutcome::Fallthrough { spawn_daemon: true };
    };
    let Ok(hello) = Hello::decode(hello_body) else {
        return ClientOutcome::Fallthrough { spawn_daemon: true };
    };
    if hello.form != FORM {
        return ClientOutcome::Fallthrough { spawn_daemon: true };
    }

    // 2. Gather client TTYs, argv, env, monotonic start timestamp and send CheckReq.
    let (stdout_tty, cols) = probe_tty(1);
    let (stderr_tty, _) = probe_tty(2);
    let mut argv: Vec<&[u8], &bstd::mem::Frame> = Vec::with_capacity_in(block.args.len(), &frame);
    for &arg in block.args.iter() {
        argv.push(arg.as_bytes());
    }
    let mut env_pairs: Vec<(&[u8], &[u8]), &bstd::mem::Frame> = Vec::with_capacity_in(block.env.len(), &frame);
    for entry in block.env.iter() {
        let bytes = entry.to_bytes();
        if let Some(eq) = bytes.iter().position(|&b| b == b'=')
            && let (Some(k), Some(v)) = (bytes.get(..eq), bytes.get(eq + 1..))
        {
            env_pairs.push((k, v));
        }
    }
    let req = CheckReqRef {
        version,
        checkout: checkout_key,
        cwd: cwd_abs.as_bytes(),
        argv: argv.as_slice(),
        stdout_tty,
        stderr_tty,
        cols,
        env: env_pairs.as_slice(),
        client_start_mono_ns: 0,
    };
    let mut send_buf: Vec<u8, &bstd::mem::Frame> = Vec::new_in(&frame);
    req.encode(&mut send_buf);
    if write_all_fd(conn.fd(), send_buf.as_slice()).is_err() {
        return ClientOutcome::Fallthrough { spawn_daemon: true };
    }

    // 3. Stream ServerFrames directly to stdout (fd 1) / stderr (fd 2) until Done or Refused.
    // Once `CheckReq` has been submitted, hold stop/status signals so Ctrl-C/SIGTERM/SIGINFO
    // forward to the active daemon request, and NEVER fall through to standalone on disconnect
    // or decode error (the daemon may already be executing side effects such as `--fix`).
    let asked = bstd::procs::Asked::hold().ok();
    let requests = asked.as_ref().map(bstd::procs::Asked::requests);
    let mut sent_stop = false;
    let mut wrote_any = false;
    loop {
        let body = match read_frame_fd_with_signals(conn.fd(), &mut recv_buf, requests, &mut sent_stop, &frame) {
            Ok(b) => b,
            Err(()) => {
                if !sent_stop {
                    let _ = write_all_fd(
                        blaze_sys::Fd::new(2),
                        b"error: blaze daemon connection closed before completing request\n",
                    );
                }
                return ClientOutcome::Exit(Status::FAILED);
            }
        };
        let server_frame = match ServerFrame::decode(body) {
            Ok(f) => f,
            Err(_) => {
                let _ = write_all_fd(
                    blaze_sys::Fd::new(2),
                    b"error: blaze daemon returned malformed response frame\n",
                );
                return ClientOutcome::Exit(Status::FAILED);
            }
        };
        match server_frame {
            ServerFrame::Out(bytes) => {
                if !bytes.is_empty() {
                    wrote_any = true;
                    let _ = write_all_fd(blaze_sys::Fd::new(1), bytes);
                }
            }
            ServerFrame::Err(bytes) => {
                if !bytes.is_empty() {
                    wrote_any = true;
                    let _ = write_all_fd(blaze_sys::Fd::new(2), bytes);
                }
            }
            ServerFrame::Done { status } => {
                return ClientOutcome::Exit(Status(status));
            }
            ServerFrame::Refused { why } => {
                if !wrote_any {
                    let spawn = matches!(why, CannotServe::OtherVersion | CannotServe::ShuttingDown);
                    return ClientOutcome::Fallthrough { spawn_daemon: spawn };
                }
                return ClientOutcome::Exit(Status::FAILED);
            }
        }
    }
}

#[cfg(not(unix))]
pub(crate) fn try_run() -> ClientOutcome {
    ClientOutcome::Fallthrough { spawn_daemon: false }
}

/// Sends a best-effort `ClientFrame::Shutdown` to the resident daemon serving `checkout_root_abs`
/// under `home` (called by `blaze clean`). Silently succeeds if no daemon is running.
#[cfg(unix)]
pub(crate) fn send_shutdown(home: &str, checkout_root_abs: &str) {
    if !crate::daemon_server::socket_fits(Some(home)) {
        return;
    }
    let key = crate::journal_ask::checkout_key_of(checkout_root_abs);
    let mut sock_buf = [0u8; 256];
    let Some(sock_path) = format_socket_path(home, &key, &mut sock_buf) else {
        return;
    };
    send_shutdown_socket(sock_path);
}

/// Sends a best-effort `ClientFrame::Shutdown` to the resident daemon listening at `sock_path`.
#[cfg(unix)]
pub(crate) fn send_shutdown_socket(sock_path: &[u8]) {
    let Ok(conn) = blaze_sys::posix::net::connect_unix(sock_path) else {
        return;
    };
    let Ok(frame) = bstd::mem::Frame::new(bstd::mem::Tag::Other) else {
        return;
    };
    let mut buf: Vec<u8, &bstd::mem::Frame> = Vec::new_in(&frame);
    if read_frame_fd(conn.fd(), &mut buf).is_err() {
        return;
    }
    buf.clear();
    daemon::encode_shutdown(&mut buf);
    if write_all_fd(conn.fd(), buf.as_slice()).is_err() {
        return;
    }
    // Wait briefly for daemon's Done acknowledgement so the daemon finishes unlinking its socket.
    let _ = read_frame_fd(conn.fd(), &mut buf);
}

/// Spawns a detached `blaze daemon` for `checkout_root_abs` after a standalone `blaze check`
/// finishes, so the very next `blaze check` hits the hot resident daemon.
#[cfg(unix)]
pub(crate) fn spawn_detached_daemon(
    cx: &bstd::Cx,
    host: &bstd::host::Host,
    env: Option<&bstd::env::Env>,
    checkout_root_abs: &str,
    frame: &bstd::mem::Frame,
) {
    if cx.is_simulated() {
        return;
    }
    let var = |name: &str| -> Option<&str> { env?.get(&bstd::env::VarName::new(name).ok()?).ok().flatten() };
    if var("BLAZE_NO_DAEMON") == Some("1") {
        return;
    }
    let Some(home) = var("HOME") else {
        return;
    };
    if !crate::daemon_server::socket_fits(Some(home)) {
        return;
    }
    let Ok(spawner) = host.spawner() else {
        return;
    };
    let Ok(exe) = host.exe() else {
        return;
    };
    const PASS_VARS: [&str; 8] = [
        "HOME",
        "PATH",
        "SDKROOT",
        "DEVELOPER_DIR",
        "CARGO_HOME",
        "TOOLCHAINS",
        "BLAZE_STORE_MAX_BYTES",
        "BLAZE_STORE_MAX_GB",
    ];
    let mut pairs: Vec<(&str, &str), &bstd::mem::Frame> = Vec::with_capacity_in(PASS_VARS.len(), frame);
    for name in PASS_VARS {
        if let Some(val) = var(name) {
            pairs.push((name, val));
        }
    }
    const DAEMON_ARGS: [&str; 1] = ["daemon"];
    let spec = bstd::procs::DetachedSpec {
        program: exe.as_ref(),
        args: &DAEMON_ARGS,
        env: pairs.as_slice(),
        dir: checkout_root_abs,
    };
    let _ = spawner.start_detached(cx, &spec);
}

#[cfg(not(unix))]
pub(crate) fn spawn_detached_daemon(
    _cx: &bstd::Cx,
    _host: &bstd::host::Host,
    _env: Option<&bstd::env::Env>,
    _checkout_root_abs: &str,
    _frame: &bstd::mem::Frame,
) {
}

// ---------------------------------------------------------------------------------------------
// Low-level pre-runtime Unix helpers (zero `unsafe`, zero `std::*`, zero `Global`)
// ---------------------------------------------------------------------------------------------

#[cfg(unix)]
fn parse_check_dir<'a>(args: &'a [&'a str]) -> Option<Option<&'a str>> {
    let mut dir: Option<&'a str> = None;
    let mut at = 2usize;
    while let Some(&arg) = args.get(at) {
        at += 1;
        if arg == "--" {
            // Words after `--` are rejected by `check_command`; let standalone print Usage.
            return None;
        }
        match arg {
            "-p" | "--package" | "--view" | "--grant" | "--bin" | "--workers" | "--status-every" | "--color" | "--origin" | "--origin-key" => {
                // Skip its value; if missing, let standalone print Usage.
                if args.get(at).is_none() {
                    return None;
                }
                at += 1;
            }
            "--fix" => {
                while args.get(at).is_some_and(|w| !w.starts_with("--")) {
                    at += 1;
                }
            }
            "--stats" | "--each" | "--names-oracle" => {}
            _ if arg.starts_with("--") => {}
            _ => {
                if dir.is_some() {
                    return None;
                }
                dir = Some(arg);
            }
        }
    }
    Some(dir)
}

#[cfg(unix)]
fn cstr_in<'a>(text: &str, buf: &'a mut [u8; 4096]) -> Option<blaze_sys::Path<'a>> {
    let bytes = text.as_bytes();
    if bytes.is_empty() || bytes.len() >= buf.len() || bytes.contains(&0) {
        return None;
    }
    buf.get_mut(..bytes.len())?.copy_from_slice(bytes);
    *buf.get_mut(bytes.len())? = 0;
    let cstr = core::ffi::CStr::from_bytes_with_nul(buf.get(..=bytes.len())?).ok()?;
    Some(blaze_sys::Path::new(cstr))
}

#[cfg(unix)]
fn is_regular_file(path: &str) -> bool {
    let mut buf = [0u8; 4096];
    let Some(cpath) = cstr_in(path, &mut buf) else {
        return false;
    };
    match blaze_sys::posix::proc::stat_at(blaze_sys::posix::io::CWD, cpath, blaze_sys::posix::proc::AT_SYMLINK_NOFOLLOW) {
        Ok(st) => (st.st_mode & blaze_sys::posix::proc::S_IFMT) == blaze_sys::posix::proc::S_IFREG,
        Err(_) => false,
    }
}

#[cfg(unix)]
fn realpath_str<'a>(path: &str, out: &'a mut [u8; 4096]) -> Option<&'a str> {
    let mut cbuf = [0u8; 4096];
    let cpath = cstr_in(path, &mut cbuf)?;
    let resolved = blaze_sys::posix::proc::realpath_into(cpath, out).ok()?;
    core::str::from_utf8(resolved).ok()
}

/// Resolves the workspace root for `target_abs` (beneath `cwd_abs`), matching `run_package::locate`:
/// walks up from `target_abs` to `cwd_abs` for `[workspace]`, respecting `package.workspace` and
/// `workspace.exclude`, and refusing if an ancestor above `cwd_abs` holds a non-excluding
/// `[workspace]` (returning `None` so standalone prints `RootAboveStart`).
#[cfg(unix)]
pub(crate) fn locate_workspace_root<'a>(
    target_abs: &str,
    cwd_abs: &str,
    frame: &bstd::mem::Frame,
    out: &'a mut [u8; 4096],
) -> Option<&'a str> {
    let mut nearest_dir: Option<&str> = None;
    let mut cur = target_abs;
    let cwd_trimmed = cwd_abs.trim_end_matches('/');

    // Walk up from `target_abs` up to and including `cwd_abs`.
    for _ in 0..64 {
        match inspect_cargo_toml(cur, nearest_dir, frame) {
            CargoTomlState::Absent => {}
            CargoTomlState::Ambiguous => return None,
            CargoTomlState::PackageWithWorkspaceKey => return None,
            CargoTomlState::PackageOnly => {
                if nearest_dir.is_none() {
                    nearest_dir = Some(cur);
                }
            }
            CargoTomlState::Workspace { excludes_package } => {
                if nearest_dir.is_none() {
                    return copy_str_into(cur, out);
                }
                if !excludes_package {
                    return copy_str_into(cur, out);
                }
            }
        }
        let cur_trimmed = cur.trim_end_matches('/');
        if cur_trimmed == cwd_trimmed || cur_trimmed.is_empty() {
            break;
        }
        match cur_trimmed.rsplit_once('/') {
            Some((parent, _)) if !parent.is_empty() => cur = parent,
            _ => break,
        }
    }

    let pkg_dir = nearest_dir?;

    // Check ancestors strictly above `cwd_abs`: if one has `[workspace]` that does not exclude
    // `pkg_dir`, return `None` so standalone `run_package::locate` emits `RootAboveStart`.
    let mut above = cwd_trimmed;
    for _ in 0..64 {
        let Some((parent, _)) = above.rsplit_once('/') else {
            break;
        };
        if parent.is_empty() {
            break;
        }
        match inspect_cargo_toml(parent, Some(pkg_dir), frame) {
            CargoTomlState::Absent | CargoTomlState::PackageOnly | CargoTomlState::PackageWithWorkspaceKey => {}
            CargoTomlState::Workspace { excludes_package: true } => {}
            CargoTomlState::Workspace { excludes_package: false } | CargoTomlState::Ambiguous => return None,
        }
        above = parent;
    }

    copy_str_into(pkg_dir, out)
}

#[cfg(unix)]
fn copy_str_into<'a>(src: &str, out: &'a mut [u8; 4096]) -> Option<&'a str> {
    let bytes = src.as_bytes();
    out.get_mut(..bytes.len())?.copy_from_slice(bytes);
    core::str::from_utf8(out.get(..bytes.len())?).ok()
}

#[cfg(unix)]
enum CargoTomlState {
    Absent,
    Ambiguous,
    PackageOnly,
    PackageWithWorkspaceKey,
    Workspace { excludes_package: bool },
}

#[cfg(unix)]
fn path_beneath(path: &str, entry: &str) -> bool {
    let mut parts = path.split('/').filter(|part| !part.is_empty() && *part != ".");
    for want in entry.split('/').filter(|part| !part.is_empty() && *part != ".") {
        if want == ".." || parts.next() != Some(want) {
            return false;
        }
    }
    true
}

#[cfg(unix)]
fn has_glob_meta(entry: &str) -> bool {
    entry.as_bytes().iter().any(|&b| matches!(b, b'*' | b'?' | b'['))
}

/// Inspects `<dir>/Cargo.toml` with `blaze_toml` to classify workspace membership and exclusion
/// identically to `run_package::locate` (Cargo's `find_root`).
#[cfg(unix)]
fn inspect_cargo_toml(dir: &str, pkg_dir: Option<&str>, frame: &bstd::mem::Frame) -> CargoTomlState {
    use blaze_sys::posix::io::{CWD, OpenFlags, openat, read};
    let mut path_buf = [0u8; 4096];
    let dir_b = dir.trim_end_matches('/').as_bytes();
    let suffix = b"/Cargo.toml";
    let Some(total) = dir_b.len().checked_add(suffix.len()) else {
        return CargoTomlState::Ambiguous;
    };
    if total >= path_buf.len() {
        return CargoTomlState::Ambiguous;
    }
    let Some(prefix_slot) = path_buf.get_mut(..dir_b.len()) else {
        return CargoTomlState::Ambiguous;
    };
    prefix_slot.copy_from_slice(dir_b);
    let Some(suffix_slot) = path_buf.get_mut(dir_b.len()..total) else {
        return CargoTomlState::Ambiguous;
    };
    suffix_slot.copy_from_slice(suffix);
    let Some(nul_slot) = path_buf.get_mut(total) else {
        return CargoTomlState::Ambiguous;
    };
    *nul_slot = 0;
    let Ok(cstr) = core::ffi::CStr::from_bytes_with_nul(path_buf.get(..=total).unwrap_or(&[])) else {
        return CargoTomlState::Ambiguous;
    };
    let fd = match openat(CWD, blaze_sys::Path::new(cstr), OpenFlags::READ.with(OpenFlags::CLOSE_ON_EXEC), 0) {
        Ok(fd) => fd,
        Err(blaze_sys::Errno::NoEnt | blaze_sys::Errno::NotDir) => return CargoTomlState::Absent,
        Err(_) => return CargoTomlState::Ambiguous,
    };

    let mut buf = [0u8; 64 << 10];
    let mut filled = 0usize;
    while filled < buf.len() {
        let Some(slot) = buf.get_mut(filled..) else {
            break;
        };
        match read(fd.fd(), slot) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(blaze_sys::Errno::Intr) => continue,
            Err(_) => return CargoTomlState::Ambiguous,
        }
    }
    if filled == buf.len() {
        // Manifest exceeds fast-path 64 KiB cap -> let standalone parse it cleanly.
        return CargoTomlState::Ambiguous;
    }
    let Ok(text) = core::str::from_utf8(buf.get(..filled).unwrap_or(&[])) else {
        return CargoTomlState::Ambiguous;
    };
    let Ok(doc) = blaze_toml::Document::parse_in(text, blaze_toml::Limits::CARGO, frame) else {
        return CargoTomlState::Ambiguous;
    };
    if let Some(ws) = doc.root().get("workspace") {
        let Some(pkg) = pkg_dir else {
            return CargoTomlState::Workspace { excludes_package: false };
        };
        let dir_trimmed = dir.trim_end_matches('/');
        let Some(rel) = pkg.trim_end_matches('/').strip_prefix(dir_trimmed) else {
            return CargoTomlState::Ambiguous;
        };
        let rel = rel.trim_start_matches('/');
        if rel.is_empty() {
            return CargoTomlState::Workspace { excludes_package: false };
        }
        // If `exclude` or `members` uses glob patterns, fall through to standalone `Layout`
        // rather than guessing glob expansion in the thin pre-runtime client.
        for key in ["exclude", "members"] {
            if let Some(list) = ws.get(key)
                && list.children().any(|entry| entry.as_str().is_some_and(has_glob_meta))
            {
                return CargoTomlState::Ambiguous;
            }
        }
        let under = |key: &str| {
            ws.get(key)
                .is_some_and(|list| list.children().any(|entry| entry.as_str().is_some_and(|e| path_beneath(rel, e))))
        };
        let excludes_package = under("exclude") && !under("members");
        return CargoTomlState::Workspace { excludes_package };
    }
    if pkg_dir.is_none()
        && doc.root().get("package").and_then(|p| p.get("workspace")).is_some()
    {
        return CargoTomlState::PackageWithWorkspaceKey;
    }
    CargoTomlState::PackageOnly
}

#[cfg(unix)]
fn format_socket_path<'a>(home: &str, key: &blaze_watch::helper::CheckoutKey, out: &'a mut [u8; 256]) -> Option<&'a [u8]> {
    let rel = daemon::socket_path(key);
    let home_b = home.trim_end_matches('/').as_bytes();
    let mid = b"/.blaze/v2/";
    let total = home_b.len().checked_add(mid.len())?.checked_add(rel.len())?;
    if total > out.len() {
        return None;
    }
    let mut pos = 0usize;
    out.get_mut(..home_b.len())?.copy_from_slice(home_b);
    pos += home_b.len();
    out.get_mut(pos..pos + mid.len())?.copy_from_slice(mid);
    pos += mid.len();
    out.get_mut(pos..pos + rel.len())?.copy_from_slice(&rel);
    pos += rel.len();
    out.get(..pos)
}

#[cfg(unix)]
fn probe_tty(fd_num: i32) -> (bool, Option<u16>) {
    let fd = blaze_sys::Fd::new(fd_num);
    #[cfg(target_os = "macos")]
    {
        if !blaze_sys::macos::is_terminal(fd) {
            return (false, None);
        }
        let cols = blaze_sys::macos::window_size(fd).ok().filter(|w| w.cols > 0).map(|w| w.cols);
        (true, cols)
    }
    #[cfg(target_os = "linux")]
    {
        if !blaze_sys::linux::is_terminal(fd) {
            return (false, None);
        }
        let cols = blaze_sys::linux::window_size(fd).ok().filter(|w| w.cols > 0).map(|w| w.cols);
        (true, cols)
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = fd;
        (false, None)
    }
}

#[cfg(unix)]
fn write_all_fd(fd: blaze_sys::Fd, mut bytes: &[u8]) -> Result<(), ()> {
    while !bytes.is_empty() {
        match blaze_sys::posix::io::write(fd, bytes) {
            Ok(0) => return Err(()),
            Ok(n) => {
                bytes = bytes.get(n..).unwrap_or(&[]);
            }
            Err(blaze_sys::Errno::Intr) => continue,
            Err(_) => return Err(()),
        }
    }
    Ok(())
}

#[cfg(unix)]
fn read_exact_fd(fd: blaze_sys::Fd, mut dst: &mut [u8]) -> Result<(), ()> {
    while !dst.is_empty() {
        match blaze_sys::posix::io::read(fd, dst) {
            Ok(0) => return Err(()),
            Ok(n) => {
                let Some(rest) = dst.get_mut(n..) else {
                    return Err(());
                };
                dst = rest;
            }
            Err(blaze_sys::Errno::Intr) => continue,
            Err(_) => return Err(()),
        }
    }
    Ok(())
}

#[cfg(unix)]
fn forward_pending_signals(
    fd: blaze_sys::Fd,
    requests: Option<&bstd::procs::Requests>,
    sent_stop: &mut bool,
    frame: &bstd::mem::Frame,
) {
    let Some(requests) = requests else {
        return;
    };
    if !*sent_stop
        && let Some(by) = requests.stop()
    {
        let mut sig_buf: Vec<u8, &bstd::mem::Frame> = Vec::new_in(frame);
        daemon::encode_signal(by.number(), &mut sig_buf);
        let _ = write_all_fd(fd, sig_buf.as_slice());
        *sent_stop = true;
    }
    if let Some(by) = requests.take_status() {
        let mut sig_buf: Vec<u8, &bstd::mem::Frame> = Vec::new_in(frame);
        daemon::encode_signal(by.number(), &mut sig_buf);
        let _ = write_all_fd(fd, sig_buf.as_slice());
    }
}

#[cfg(unix)]
fn read_exact_fd_with_signals(
    fd: blaze_sys::Fd,
    mut dst: &mut [u8],
    requests: Option<&bstd::procs::Requests>,
    sent_stop: &mut bool,
    frame: &bstd::mem::Frame,
) -> Result<(), ()> {
    use blaze_sys::Timespec;
    use blaze_sys::posix::io::{PollEvents, PollFd, poll};
    let slice_timeout = Some(Timespec { sec: 0, nsec: 50_000_000 });
    while !dst.is_empty() {
        forward_pending_signals(fd, requests, sent_stop, frame);
        let mut pfd = [PollFd::new(fd, PollEvents::IN)];
        match poll(&mut pfd, slice_timeout) {
            Ok(0) | Err(blaze_sys::Errno::Intr) => continue,
            Ok(_) => {}
            Err(_) => return Err(()),
        }
        forward_pending_signals(fd, requests, sent_stop, frame);
        match blaze_sys::posix::io::read(fd, dst) {
            Ok(0) => return Err(()),
            Ok(n) => {
                let Some(rest) = dst.get_mut(n..) else {
                    return Err(());
                };
                dst = rest;
            }
            Err(blaze_sys::Errno::Intr) => continue,
            Err(_) => return Err(()),
        }
    }
    Ok(())
}

#[cfg(unix)]
fn read_frame_fd_with_signals<'b>(
    fd: blaze_sys::Fd,
    buf: &'b mut Vec<u8, &bstd::mem::Frame>,
    requests: Option<&bstd::procs::Requests>,
    sent_stop: &mut bool,
    frame: &bstd::mem::Frame,
) -> Result<&'b [u8], ()> {
    let mut head = [0u8; 4];
    read_exact_fd_with_signals(fd, &mut head, requests, sent_stop, frame)?;
    let len = daemon::frame_len(&head).ok_or(())?.map_err(|_| ())?;
    buf.clear();
    buf.resize(len, 0);
    read_exact_fd_with_signals(fd, buf.as_mut_slice(), requests, sent_stop, frame)?;
    Ok(buf.as_slice())
}

#[cfg(unix)]
fn read_frame_fd<'b>(fd: blaze_sys::Fd, buf: &'b mut Vec<u8, &bstd::mem::Frame>) -> Result<&'b [u8], ()> {
    let mut head = [0u8; 4];
    read_exact_fd(fd, &mut head)?;
    let len = daemon::frame_len(&head).ok_or(())?.map_err(|_| ())?;
    buf.clear();
    buf.resize(len, 0);
    read_exact_fd(fd, buf.as_mut_slice())?;
    Ok(buf.as_slice())
}
