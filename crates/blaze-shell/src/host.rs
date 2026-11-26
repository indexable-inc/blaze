//! Thin host IO driver for `blaze_sh::machine::Machine`: answers every sans-IO [`Request`]
//! (`ReadDir`, `Stat`, `ReadFile`, `WriteFile`, `FsOp`, `ChangeDir`, `Which`, `Now`, `SysInfo`,
//! `ProcessList`, `SpawnExternal`, `EvalRust`, `ForkSession`) with bounded timeouts and zero
//! panics on user input.

use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use blaze_own::{AString, AVec, Alloc};
use blaze_sh::machine::Machine;
use blaze_sh::{
    Answer, Bytes, DateTime, DirEntry, EngineState, ErrorKind, EvalOutcome, ExternalMode,
    ExternalResult, FileKind, FsOp, ProcEntry, Request, ShellError, Span, Step, StreamChunk,
    SysSnapshot,
};

use crate::rust_cell;

/// Hard read cap per `open` / `ReadFile` call so a giant file cannot exhaust memory (rule 18).
const MAX_READ_BYTES: u64 = 64 << 20;
/// Bounded timeout for captured external commands (rule 18 & 69).
const EXTERNAL_CAPTURE_TIMEOUT: Duration = Duration::from_secs(30);
/// Bounded timeout for `ps` snapshot queries.
const PS_TIMEOUT: Duration = Duration::from_secs(5);
/// Maximum number of `Machine::step` iterations per command line (guard against runaway loops).
const MAX_MACHINE_STEPS: usize = 250_000;

/// Pending interactive external process to be attached to a PTY + `blaze-vt` terminal block.
#[derive(Clone, Debug)]
pub struct InteractiveSpawn {
    pub program: String,
    pub args: Vec<String>,
    pub cwd: String,
    pub env: Vec<(String, String)>,
}

/// Metadata captured when a line executed a `rust { ... }` cell.
#[derive(Clone, Debug)]
pub struct RustCellMeta {
    pub code: String,
    pub compile_ms: u64,
    pub run_ms: u64,
    pub stdout: String,
}

/// Complete result of driving `blaze_sh::machine::Machine` for one submitted command line.
pub struct ExecResult {
    pub outcome: EvalOutcome<'static>,
    pub streamed_lines: Vec<String>,
    pub rust_meta: Option<RustCellMeta>,
    pub interactive_spawn: Option<InteractiveSpawn>,
    pub forked_label: Option<String>,
}

/// Drive `blaze_sh::machine::Machine` to completion for `src` starting from `state`.
///
/// When `allow_interactive_pty` is `true` (the GUI app), an interactive external command
/// (`ExternalMode::Interactive`) records an [`InteractiveSpawn`] so the caller attaches a live
/// PTY + `blaze-vt` terminal block; when `false` (headless tests / CLI), it runs captured.
pub fn run_command_line(
    src: &str,
    state: EngineState<'static>,
    allow_interactive_pty: bool,
) -> ExecResult {
    run_command_line_with_pid(src, state, allow_interactive_pty, None)
}

/// Drive `blaze_sh::machine::Machine` to completion, optionally publishing the spawned OS child
/// PID into `pid_out` as soon as an external process starts.
pub fn run_command_line_with_pid(
    src: &str,
    state: EngineState<'static>,
    allow_interactive_pty: bool,
    pid_out: Option<&Arc<AtomicU32>>,
) -> ExecResult {
    let alloc = Alloc::global();
    let initial_cwd = state.cwd.as_ref().to_owned();
    let home = state.get_env("HOME").unwrap_or("").to_owned();
    let oldpwd = state.get_env("OLDPWD").unwrap_or(&initial_cwd).to_owned();
    let path_env = state.get_env("PATH").map(str::to_owned);

    let mut cur_cwd = initial_cwd;
    let mut machine = Machine::new(src, state);
    let mut streamed_lines: Vec<String> = Vec::new();
    let mut current_line = String::new();
    let mut rust_meta: Option<RustCellMeta> = None;
    let mut interactive_spawn: Option<InteractiveSpawn> = None;
    let mut forked_label: Option<String> = None;

    for _ in 0..MAX_MACHINE_STEPS {
        match machine.step() {
            Step::Done(outcome) => {
                if !current_line.is_empty() {
                    streamed_lines.push(current_line);
                }
                return ExecResult {
                    outcome,
                    streamed_lines,
                    rust_meta,
                    interactive_spawn,
                    forked_label,
                };
            }
            Step::Stream(chunk) => match chunk {
                StreamChunk::Text { text, newline } => {
                    current_line.push_str(text.as_str());
                    if newline {
                        streamed_lines.push(core::mem::take(&mut current_line));
                    }
                }
                StreamChunk::Value(v) => {
                    let mut s = String::new();
                    let _ = v.format_cell(&mut s);
                    streamed_lines.push(s);
                }
            },
            Step::Need(req) => {
                let ans = handle_request(
                    req,
                    &mut cur_cwd,
                    &home,
                    &oldpwd,
                    path_env.as_deref(),
                    allow_interactive_pty,
                    pid_out,
                    &mut rust_meta,
                    &mut interactive_spawn,
                    &mut forked_label,
                    alloc,
                );
                machine.answer(ans);
            }
        }
    }

    // Should never be reached under normal evaluation; return a bounded-budget error.
    let err = ShellError::new(
        ErrorKind::Budget,
        Span::ZERO,
        "command exceeded host step budget",
        alloc,
    );
    let fallback_state = EngineState::new(&cur_cwd, &[], alloc);
    ExecResult {
        outcome: EvalOutcome {
            value: blaze_sh::Value::error(err, alloc),
            state: fallback_state,
            exit_code: 1,
            terminal_passthrough: false,
        },
        streamed_lines,
        rust_meta,
        interactive_spawn,
        forked_label,
    }
}

#[allow(clippy::too_many_arguments)]
fn handle_request(
    req: Request<'static>,
    cur_cwd: &mut String,
    home: &str,
    oldpwd: &str,
    path_env: Option<&str>,
    allow_interactive_pty: bool,
    pid_out: Option<&Arc<AtomicU32>>,
    rust_meta: &mut Option<RustCellMeta>,
    interactive_spawn: &mut Option<InteractiveSpawn>,
    forked_label: &mut Option<String>,
    alloc: Alloc<'static>,
) -> Answer<'static> {
    match req {
        Request::ReadDir { path, show_hidden } => {
            let resolved = resolve_path(path.as_str(), cur_cwd, home);
            match read_dir_entries(&resolved, show_hidden, alloc) {
                Ok(entries) => Answer::DirEntries(entries),
                Err(msg) => Answer::Err(ShellError::new(ErrorKind::Io, Span::ZERO, &msg, alloc)),
            }
        }
        Request::Stat { path } => {
            let resolved = resolve_path(path.as_str(), cur_cwd, home);
            match stat_entry(&resolved, alloc) {
                Ok(entry) => Answer::Stat(entry),
                Err(msg) => Answer::Err(ShellError::new(ErrorKind::Io, Span::ZERO, &msg, alloc)),
            }
        }
        Request::ReadFile { path } => {
            let resolved = resolve_path(path.as_str(), cur_cwd, home);
            match read_file_bounded(&resolved, alloc) {
                Ok(bytes) => Answer::FileBytes(bytes),
                Err(msg) => Answer::Err(ShellError::new(ErrorKind::Io, Span::ZERO, &msg, alloc)),
            }
        }
        Request::WriteFile { path, bytes, append } => {
            let resolved = resolve_path(path.as_str(), cur_cwd, home);
            match write_file_bytes(&resolved, bytes.as_slice(), append) {
                Ok(()) => Answer::Ok,
                Err(msg) => Answer::Err(ShellError::new(ErrorKind::Io, Span::ZERO, &msg, alloc)),
            }
        }
        Request::FsOp(op) => match run_fs_op(&op, cur_cwd, home) {
            Ok(()) => Answer::Ok,
            Err(msg) => Answer::Err(ShellError::new(ErrorKind::Io, Span::ZERO, &msg, alloc)),
        },
        Request::ChangeDir { path } => {
            let raw = path.as_str();
            let target = if raw.is_empty() || raw == "~" {
                if home.is_empty() { PathBuf::from(cur_cwd.as_str()) } else { PathBuf::from(home) }
            } else if raw == "-" {
                PathBuf::from(oldpwd)
            } else {
                resolve_path(raw, cur_cwd, home)
            };
            match fs::canonicalize(&target) {
                Ok(canon) if canon.is_dir() => {
                    let s = canon.to_string_lossy().into_owned();
                    *cur_cwd = s.clone();
                    Answer::ChangedDir(AString::from_str_in(&s, alloc))
                }
                Ok(canon) => {
                    let msg = format!("cd: `{}` is not a directory", canon.display());
                    Answer::Err(ShellError::new(ErrorKind::Io, Span::ZERO, &msg, alloc))
                }
                Err(e) => {
                    let msg = format!("cd: `{}`: {e}", target.display());
                    Answer::Err(ShellError::new(ErrorKind::Io, Span::ZERO, &msg, alloc))
                }
            }
        }
        Request::Which { name } => {
            let found = which_lookup(name.as_str(), path_env).map(|p| {
                AString::from_str_in(&p.to_string_lossy(), alloc)
            });
            Answer::FoundPath(found)
        }
        Request::Now => Answer::Now(now_datetime()),
        Request::SysInfo => Answer::Sys(collect_sys_snapshot(alloc)),
        Request::ProcessList => match collect_process_list(alloc) {
            Ok(procs) => Answer::Processes(procs),
            Err(msg) => Answer::Err(ShellError::new(ErrorKind::Io, Span::ZERO, &msg, alloc)),
        },
        Request::SpawnExternal { program, args, cwd, env, stdin, mode } => {
            if mode == ExternalMode::Interactive && allow_interactive_pty {
                let spawn = InteractiveSpawn {
                    program: program.as_str().to_owned(),
                    args: args.iter().map(|a| a.as_str().to_owned()).collect(),
                    cwd: cwd.as_str().to_owned(),
                    env: env
                        .iter()
                        .map(|(k, v)| (k.as_ref().to_owned(), v.as_ref().to_owned()))
                        .collect(),
                };
                *interactive_spawn = Some(spawn);
                Answer::External(ExternalResult {
                    exit_code: 0,
                    stdout: AVec::new_in(alloc),
                    stderr: AVec::new_in(alloc),
                    interactive: true,
                })
            } else {
                match run_external_captured(
                    program.as_str(),
                    &args,
                    cwd.as_str(),
                    &env,
                    stdin.as_ref().map(|b| b.as_slice()),
                    pid_out,
                    alloc,
                ) {
                    Ok(res) => Answer::External(res),
                    Err(msg) => {
                        Answer::Err(ShellError::new(ErrorKind::External, Span::ZERO, &msg, alloc))
                    }
                }
            }
        }
        Request::EvalRust { prior_cells, code, input_json, cwd } => {
            let prior_refs: Vec<&str> = prior_cells.iter().map(|c| c.as_ref()).collect();
            let res = rust_cell::eval_rust_cell(
                &prior_refs,
                code.as_str(),
                input_json.as_str(),
                cwd.as_str(),
                path_env,
                alloc,
            );
            *rust_meta = Some(RustCellMeta {
                code: code.as_str().to_owned(),
                compile_ms: res.compile_ms,
                run_ms: res.run_ms,
                stdout: res.stdout.as_str().to_owned(),
            });
            Answer::Rust(res)
        }
        Request::ForkSession { label } => {
            let name = label
                .as_ref()
                .map(|s| s.as_str().to_owned())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "fork".to_owned());
            *forked_label = Some(name.clone());
            Answer::Forked(AString::from_str_in(&name, alloc))
        }
    }
}

/// Resolve a potentially `~`-prefixed or relative path against `cwd`.
pub fn resolve_path(raw: &str, cwd: &str, home: &str) -> PathBuf {
    if raw.is_empty() || raw == "." {
        return PathBuf::from(cwd);
    }
    if raw == "~" && !home.is_empty() {
        return PathBuf::from(home);
    }
    if let Some(rest) = raw.strip_prefix("~/")
        && !home.is_empty()
    {
        return Path::new(home).join(rest);
    }
    let p = Path::new(raw);
    if p.is_absolute() { p.to_path_buf() } else { Path::new(cwd).join(p) }
}

fn file_kind_from_meta(meta: &fs::Metadata) -> FileKind {
    let ft = meta.file_type();
    if ft.is_symlink() {
        FileKind::Symlink
    } else if ft.is_dir() {
        FileKind::Dir
    } else if ft.is_file() {
        FileKind::File
    } else {
        FileKind::Other
    }
}

fn system_time_to_datetime(t: SystemTime, offset_secs: i32) -> DateTime {
    let ns = match t.duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_nanos().min(i64::MAX as u128) as i64,
        Err(e) => -(e.duration().as_nanos().min(i64::MAX as u128) as i64),
    };
    DateTime { utc_ns: ns, offset_secs }
}

fn sniff_entry_label(entry_path: &Path, name: &str, kind: FileKind) -> String {
    let res = match kind {
        FileKind::Dir => {
            let markers = blaze_sniff::DirMarkers {
                has_cargo_toml: entry_path.join("Cargo.toml").exists(),
                has_package_json: entry_path.join("package.json").exists(),
                has_pyproject: entry_path.join("pyproject.toml").exists(),
                has_go_mod: entry_path.join("go.mod").exists(),
                has_flake_nix: entry_path.join("flake.nix").exists(),
                has_git: entry_path.join(".git").exists(),
            };
            blaze_sniff::sniff_dir(name, markers)
        }
        FileKind::Symlink => blaze_sniff::sniff_symlink(),
        FileKind::File => {
            let mut head = [0u8; blaze_sniff::SNIFF_HEAD_BYTES];
            let n = fs::File::open(entry_path)
                .and_then(|mut f| f.read(&mut head))
                .unwrap_or(0);
            blaze_sniff::sniff_file(name, &head[..n])
        }
        FileKind::Other => blaze_sniff::sniff_file(name, &[]),
    };
    let mut s = String::new();
    let _ = res.format_label(&mut s);
    s
}

fn read_dir_entries(
    path: &Path,
    show_hidden: bool,
    alloc: Alloc<'static>,
) -> Result<AVec<'static, DirEntry<'static>>, String> {
    let rd = fs::read_dir(path).map_err(|e| format!("ls `{}`: {e}", path.display()))?;
    let tz = local_tz_offset_secs();
    let mut rows: Vec<(String, FileKind, String, Bytes, DateTime, bool)> = Vec::new();
    for item in rd {
        let Ok(ent) = item else { continue };
        let name = ent.file_name().to_string_lossy().into_owned();
        if !show_hidden && name.starts_with('.') {
            continue;
        }
        let ent_path = ent.path();
        let meta = match fs::symlink_metadata(&ent_path) {
            Ok(m) => m,
            Err(_) => continue,
        };
        let kind = file_kind_from_meta(&meta);
        let analysis = sniff_entry_label(&ent_path, &name, kind);
        let size = Bytes(meta.len().min(i64::MAX as u64) as i64);
        let modified = meta
            .modified()
            .map(|t| system_time_to_datetime(t, tz))
            .unwrap_or_default();
        let readonly = meta.permissions().readonly();
        rows.push((name, kind, analysis, size, modified, readonly));
    }
    // Deterministic ordering by name (rule 25).
    rows.sort_by(|a, b| a.0.cmp(&b.0));

    let mut out = AVec::with_capacity_in(rows.len(), alloc);
    for (name, kind, analysis, size, modified, readonly) in rows {
        out.push(DirEntry {
            name: AString::from_str_in(&name, alloc),
            kind,
            analysis: Some(AString::from_str_in(&analysis, alloc)),
            size,
            modified,
            readonly,
        });
    }
    Ok(out)
}

fn stat_entry(path: &Path, alloc: Alloc<'static>) -> Result<DirEntry<'static>, String> {
    let meta = fs::symlink_metadata(path).map_err(|e| format!("stat `{}`: {e}", path.display()))?;
    let name = path
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string_lossy().into_owned());
    let tz = local_tz_offset_secs();
    let kind = file_kind_from_meta(&meta);
    let analysis = sniff_entry_label(path, &name, kind);
    Ok(DirEntry {
        name: AString::from_str_in(&name, alloc),
        kind,
        analysis: Some(AString::from_str_in(&analysis, alloc)),
        size: Bytes(meta.len().min(i64::MAX as u64) as i64),
        modified: meta
            .modified()
            .map(|t| system_time_to_datetime(t, tz))
            .unwrap_or_default(),
        readonly: meta.permissions().readonly(),
    })
}

fn read_file_bounded(path: &Path, alloc: Alloc<'static>) -> Result<AVec<'static, u8>, String> {
    let meta = fs::metadata(path).map_err(|e| format!("open `{}`: {e}", path.display()))?;
    if meta.len() > MAX_READ_BYTES {
        return Err(format!(
            "open `{}`: file size {} bytes exceeds 64 MiB shell read cap",
            path.display(),
            meta.len()
        ));
    }
    let data = fs::read(path).map_err(|e| format!("open `{}`: {e}", path.display()))?;
    let mut out = AVec::with_capacity_in(data.len(), alloc);
    out.extend_from_slice(&data);
    Ok(out)
}

fn write_file_bytes(path: &Path, bytes: &[u8], append: bool) -> Result<(), String> {
    let mut f = OpenOptions::new()
        .create(true)
        .write(true)
        .append(append)
        .truncate(!append)
        .open(path)
        .map_err(|e| format!("save `{}`: {e}", path.display()))?;
    f.write_all(bytes)
        .map_err(|e| format!("save `{}`: {e}", path.display()))
}

fn run_fs_op(op: &FsOp<'_>, cwd: &str, home: &str) -> Result<(), String> {
    match op {
        FsOp::Cp { src, dst, recursive } => {
            let s = resolve_path(src.as_str(), cwd, home);
            let d = resolve_path(dst.as_str(), cwd, home);
            if s.is_dir() {
                if !*recursive {
                    return Err(format!("cp: `{}` is a directory (pass -r)", s.display()));
                }
                copy_dir_recursive(&s, &d)
            } else {
                let target = if d.is_dir() {
                    if let Some(name) = s.file_name() { d.join(name) } else { d }
                } else {
                    d
                };
                fs::copy(&s, &target)
                    .map(|_| ())
                    .map_err(|e| format!("cp `{}` -> `{}`: {e}", s.display(), target.display()))
            }
        }
        FsOp::Mv { src, dst } => {
            let s = resolve_path(src.as_str(), cwd, home);
            let d = resolve_path(dst.as_str(), cwd, home);
            let target = if d.is_dir() {
                if let Some(name) = s.file_name() { d.join(name) } else { d }
            } else {
                d
            };
            fs::rename(&s, &target)
                .map_err(|e| format!("mv `{}` -> `{}`: {e}", s.display(), target.display()))
        }
        FsOp::Rm { path, recursive, force } => {
            let p = resolve_path(path.as_str(), cwd, home);
            if !p.exists() {
                return if *force {
                    Ok(())
                } else {
                    Err(format!("rm: `{}`: not found", p.display()))
                };
            }
            if p.is_dir() {
                if *recursive {
                    fs::remove_dir_all(&p).map_err(|e| format!("rm `{}`: {e}", p.display()))
                } else {
                    fs::remove_dir(&p).map_err(|e| format!("rm `{}`: {e}", p.display()))
                }
            } else {
                fs::remove_file(&p).map_err(|e| format!("rm `{}`: {e}", p.display()))
            }
        }
        FsOp::Mkdir { path, parents } => {
            let p = resolve_path(path.as_str(), cwd, home);
            if *parents {
                fs::create_dir_all(&p).map_err(|e| format!("mkdir `{}`: {e}", p.display()))
            } else {
                fs::create_dir(&p).map_err(|e| format!("mkdir `{}`: {e}", p.display()))
            }
        }
        FsOp::Touch { path } => {
            let p = resolve_path(path.as_str(), cwd, home);
            OpenOptions::new()
                .create(true)
                .append(true)
                .open(&p)
                .map(|_| ())
                .map_err(|e| format!("touch `{}`: {e}", p.display()))
        }
    }
}

fn copy_dir_recursive(src: &Path, dst: &Path) -> Result<(), String> {
    fs::create_dir_all(dst).map_err(|e| format!("cp mkdir `{}`: {e}", dst.display()))?;
    for entry in fs::read_dir(src).map_err(|e| format!("cp read `{}`: {e}", src.display()))? {
        let entry = entry.map_err(|e| format!("cp entry: {e}"))?;
        let ty = entry.file_type().map_err(|e| format!("cp type: {e}"))?;
        let dest_path = dst.join(entry.file_name());
        if ty.is_dir() {
            copy_dir_recursive(&entry.path(), &dest_path)?;
        } else {
            fs::copy(entry.path(), &dest_path)
                .map_err(|e| format!("cp `{}`: {e}", entry.path().display()))?;
        }
    }
    Ok(())
}

fn which_lookup(name: &str, path_env: Option<&str>) -> Option<PathBuf> {
    if name.contains('/') {
        let p = PathBuf::from(name);
        return if is_exec(&p) { Some(p) } else { None };
    }
    let path_str = path_env
        .map(str::to_owned)
        .or_else(|| std::env::var("PATH").ok())
        .unwrap_or_default();
    for dir in std::env::split_paths(&path_str) {
        let candidate = dir.join(name);
        if is_exec(&candidate) {
            return Some(candidate);
        }
    }
    None
}

fn is_exec(p: &Path) -> bool {
    let Ok(meta) = fs::metadata(p) else { return false };
    if !meta.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

pub fn now_datetime() -> DateTime {
    system_time_to_datetime(SystemTime::now(), local_tz_offset_secs())
}

fn local_tz_offset_secs() -> i32 {
    #[cfg(unix)]
    {
        #[repr(C)]
        struct Tm {
            tm_sec: i32,
            tm_min: i32,
            tm_hour: i32,
            tm_mday: i32,
            tm_mon: i32,
            tm_year: i32,
            tm_wday: i32,
            tm_yday: i32,
            tm_isdst: i32,
            tm_gmtoff: isize,
            tm_zone: *const std::ffi::c_char,
        }
        unsafe extern "C" {
            fn time(t: *mut isize) -> isize;
            fn localtime_r(t: *const isize, out: *mut Tm) -> *mut Tm;
        }
        let mut t: isize = 0;
        let mut tm = Tm {
            tm_sec: 0,
            tm_min: 0,
            tm_hour: 0,
            tm_mday: 0,
            tm_mon: 0,
            tm_year: 0,
            tm_wday: 0,
            tm_yday: 0,
            tm_isdst: 0,
            tm_gmtoff: 0,
            tm_zone: core::ptr::null(),
        };
        // SAFETY: `t` and `tm` are valid stack locals; `localtime_r` is thread-safe.
        unsafe {
            time(&mut t);
            if !localtime_r(&t, &mut tm).is_null() {
                return tm.tm_gmtoff as i32;
            }
        }
        0
    }
    #[cfg(not(unix))]
    {
        0
    }
}

fn collect_sys_snapshot(alloc: Alloc<'static>) -> SysSnapshot<'static> {
    let host = read_hostname();
    let cpu_count = thread::available_parallelism()
        .map(|n| n.get() as u32)
        .unwrap_or(1);
    let (mem_total, mem_used, uptime_ns) = read_os_mem_uptime();
    SysSnapshot {
        host: AString::from_str_in(&host, alloc),
        os: AString::from_str_in(std::env::consts::OS, alloc),
        arch: AString::from_str_in(std::env::consts::ARCH, alloc),
        cpu_count,
        mem_total: Bytes(mem_total),
        mem_used: Bytes(mem_used),
        uptime_ns,
    }
}

fn read_hostname() -> String {
    #[cfg(unix)]
    {
        unsafe extern "C" {
            fn gethostname(name: *mut u8, len: usize) -> std::ffi::c_int;
        }
        let mut buf = [0u8; 256];
        // SAFETY: `buf` is writable for 256 bytes; we search for NUL within the buffer.
        let rc = unsafe { gethostname(buf.as_mut_ptr(), buf.len()) };
        if rc == 0 {
            let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
            if let Ok(s) = core::str::from_utf8(&buf[..end])
                && !s.is_empty()
            {
                return s.to_owned();
            }
        }
    }
    std::env::var("HOSTNAME").unwrap_or_else(|_| "localhost".to_owned())
}

fn read_os_mem_uptime() -> (i64, i64, i64) {
    #[cfg(target_os = "macos")]
    {
        use std::ffi::{CStr, c_char, c_int, c_void};
        #[repr(C)]
        struct Timeval {
            tv_sec: i64,
            tv_usec: i32,
            _pad: i32,
        }
        #[repr(C)]
        struct VmStatistics64 {
            free_count: u32,
            active_count: u32,
            inactive_count: u32,
            wire_count: u32,
            zero_fill_count: u64,
            reactivations: u64,
            pageins: u64,
            pageouts: u64,
            faults: u64,
            cow_faults: u64,
            lookups: u64,
            hits: u64,
            purges: u64,
            purgeable_count: u32,
            speculative_count: u32,
            decompressions: u64,
            compressions: u64,
            swapins: u64,
            swapouts: u64,
            compressor_page_count: u32,
            throttled_count: u32,
            external_page_count: u32,
            internal_page_count: u32,
            total_uncompressed_pages_in_compressor: u64,
        }
        unsafe extern "C" {
            fn sysctlbyname(
                name: *const c_char,
                oldp: *mut c_void,
                oldlenp: *mut usize,
                newp: *const c_void,
                newlen: usize,
            ) -> c_int;
            fn mach_host_self() -> u32;
            fn host_statistics64(
                host: u32,
                flavor: c_int,
                host_info_out: *mut c_void,
                host_info_out_cnt: *mut u32,
            ) -> c_int;
        }
        let mut mem_total: u64 = 0;
        let mut len = core::mem::size_of::<u64>();
        let key_mem = CStr::from_bytes_with_nul(b"hw.memsize\0").expect("nul");
        // SAFETY: `mem_total` and `len` point to valid stack locals sized for `u64`.
        unsafe {
            sysctlbyname(
                key_mem.as_ptr(),
                (&raw mut mem_total).cast(),
                &mut len,
                core::ptr::null(),
                0,
            );
        }

        let mut page_size: u64 = 16_384;
        let mut plen = core::mem::size_of::<u64>();
        let key_ps = CStr::from_bytes_with_nul(b"hw.pagesize\0").expect("nul");
        // SAFETY: `page_size` and `plen` point to valid stack locals sized for `u64`.
        unsafe {
            sysctlbyname(
                key_ps.as_ptr(),
                (&raw mut page_size).cast(),
                &mut plen,
                core::ptr::null(),
                0,
            );
        }
        // SAFETY: `VmStatistics64` is a plain C integer struct (`vm_statistics64_data_t`).
        let mut vm: VmStatistics64 = unsafe { core::mem::zeroed() };
        let mut count = (core::mem::size_of::<VmStatistics64>() / core::mem::size_of::<u32>()) as u32;
        const HOST_VM_INFO64: c_int = 4;
        // SAFETY: `vm` matches `vm_statistics64_data_t` and `count` is its `natural_t` count.
        let vm_rc = unsafe {
            host_statistics64(
                mach_host_self(),
                HOST_VM_INFO64,
                (&raw mut vm).cast(),
                &mut count,
            )
        };
        let mem_used = if vm_rc == 0 {
            let pages = u64::from(vm.active_count)
                .saturating_add(u64::from(vm.wire_count))
                .saturating_add(u64::from(vm.compressor_page_count));
            pages.saturating_mul(page_size).min(i64::MAX as u64) as i64
        } else {
            0
        };

        let mut boot = Timeval { tv_sec: 0, tv_usec: 0, _pad: 0 };
        let mut blen = core::mem::size_of::<Timeval>();
        let key_boot = CStr::from_bytes_with_nul(b"kern.boottime\0").expect("nul");
        // SAFETY: `boot` and `blen` point to valid stack locals sized for `Timeval`.
        let rc = unsafe {
            sysctlbyname(
                key_boot.as_ptr(),
                (&raw mut boot).cast(),
                &mut blen,
                core::ptr::null(),
                0,
            )
        };
        let uptime_ns = if rc == 0 && boot.tv_sec > 0 {
            let now_secs = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            now_secs.saturating_sub(boot.tv_sec).saturating_mul(1_000_000_000)
        } else {
            0
        };
        (mem_total.min(i64::MAX as u64) as i64, mem_used, uptime_ns)
    }
    #[cfg(not(target_os = "macos"))]
    {
        (0, 0, 0)
    }
}

fn collect_process_list(alloc: Alloc<'static>) -> Result<AVec<'static, ProcEntry<'static>>, String> {
    let mut child = Command::new("ps")
        .args(["-axo", "pid=,ppid=,pcpu=,rss=,state=,comm="])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("ps: {e}"))?;

    let reader = child.stdout.take().map(|mut p| {
        thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = p.read_to_end(&mut buf);
            buf
        })
    });
    let deadline = Instant::now() + PS_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err("ps timed out after 5s".to_owned());
                }
                thread::sleep(Duration::from_millis(5));
            }
            Err(e) => return Err(format!("ps wait: {e}")),
        }
    }
    let bytes = reader.and_then(|h| h.join().ok()).unwrap_or_default();
    let text = String::from_utf8_lossy(&bytes);
    let mut out = AVec::new_in(alloc);
    for line in text.lines() {
        let mut parts = line.split_whitespace();
        let (Some(pid_s), Some(ppid_s), Some(cpu_s), Some(rss_s), Some(state_s)) =
            (parts.next(), parts.next(), parts.next(), parts.next(), parts.next())
        else {
            continue;
        };
        let rest: Vec<&str> = parts.collect();
        let name = rest.join(" ");
        let Ok(pid) = pid_s.parse::<u32>() else { continue };
        let ppid = ppid_s.parse::<u32>().unwrap_or(0);
        let cpu_pct = cpu_s.parse::<f64>().unwrap_or(0.0);
        let rss_kb = rss_s.parse::<i64>().unwrap_or(0);
        out.push(ProcEntry {
            pid,
            ppid,
            name: AString::from_str_in(&name, alloc),
            status: AString::from_str_in(state_s, alloc),
            cpu_pct,
            mem: Bytes(rss_kb.saturating_mul(1024)),
        });
    }
    Ok(out)
}

fn run_external_captured(
    program: &str,
    args: &AVec<'_, AString<'_>>,
    cwd: &str,
    env: &AVec<'_, (blaze_own::ARc<'_, str>, blaze_own::ARc<'_, str>)>,
    stdin_bytes: Option<&[u8]>,
    pid_out: Option<&Arc<AtomicU32>>,
    alloc: Alloc<'static>,
) -> Result<ExternalResult<'static>, String> {
    let mut cmd = Command::new(program);
    for a in args.iter() {
        cmd.arg(a.as_str());
    }
    cmd.current_dir(cwd);
    if !env.is_empty() {
        cmd.env_clear();
        let mut has_term = false;
        let mut has_colorterm = false;
        let mut has_clicolor = false;
        for (k, v) in env.iter() {
            match k.as_ref() {
                "TERM" => has_term = true,
                "COLORTERM" => has_colorterm = true,
                "CLICOLOR" => has_clicolor = true,
                _ => {}
            }
            cmd.env(k.as_ref(), v.as_ref());
        }
        if !has_term {
            cmd.env("TERM", "xterm-256color");
        }
        if !has_colorterm {
            cmd.env("COLORTERM", "truecolor");
        }
        if !has_clicolor {
            cmd.env("CLICOLOR", "1");
        }
    } else {
        cmd.env("TERM", "xterm-256color");
        cmd.env("COLORTERM", "truecolor");
        cmd.env("CLICOLOR", "1");
    }
    cmd.stdin(if stdin_bytes.is_some() { Stdio::piped() } else { Stdio::null() });
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());

    let mut child = cmd
        .spawn()
        .map_err(|e| format!("failed to run `{program}`: {e}"))?;
    if let Some(slot) = pid_out {
        slot.store(child.id(), Ordering::Relaxed);
    }

    if let Some(input) = stdin_bytes
        && let Some(mut pipe) = child.stdin.take()
    {
        let data = input.to_vec();
        thread::spawn(move || {
            let _ = pipe.write_all(&data);
        });
    }

    let out_reader = child.stdout.take().map(|p| {
        thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = p.take(16 << 20).read_to_end(&mut buf);
            buf
        })
    });
    let err_reader = child.stderr.take().map(|p| {
        thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = p.take(4 << 20).read_to_end(&mut buf);
            buf
        })
    });

    let deadline = Instant::now() + EXTERNAL_CAPTURE_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(st)) => break st,
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(format!("`{program}` timed out after 30s"));
                }
                thread::sleep(Duration::from_millis(5));
            }
            Err(e) => return Err(format!("waiting on `{program}`: {e}")),
        }
    };

    let raw_out = out_reader.and_then(|h| h.join().ok()).unwrap_or_default();
    let raw_err = err_reader.and_then(|h| h.join().ok()).unwrap_or_default();

    let mut stdout = AVec::with_capacity_in(raw_out.len(), alloc);
    stdout.extend_from_slice(&raw_out);
    let mut stderr = AVec::with_capacity_in(raw_err.len(), alloc);
    stderr.extend_from_slice(&raw_err);

    Ok(ExternalResult {
        exit_code: status.code().unwrap_or(1),
        stdout,
        stderr,
        interactive: false,
    })
}
