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

use blaze_own::{AString, AVec, Alloc, Rc};
use blaze_sh::machine::Machine;
use blaze_sh::{
    Answer, Bytes, DateTime, DirEntry, EngineState, ErrorKind, EvalOutcome, ExternalMode,
    ExternalResult, FileKind, FsOp, Nanos, OutputChunk, OutputStream, ProcEntry, Request,
    ShellError, Span, Step, StreamChunk, SysSnapshot,
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
        Request::Sleep { duration } => {
            let total_ns = duration.0.max(0) as u64;
            let max_ns = 3_600_000_000_000u64; // 1hr hard upper bound per sleep call
            let capped = total_ns.min(max_ns);
            let deadline = Instant::now() + Duration::from_nanos(capped);
            while Instant::now() < deadline {
                let rem = deadline.saturating_duration_since(Instant::now());
                thread::sleep(rem.min(Duration::from_millis(25)));
            }
            Answer::Ok
        }
        Request::SysInfo => Answer::Sys(collect_sys_snapshot(alloc)),
        Request::ProcessList => match collect_process_list(alloc) {
            Ok(procs) => Answer::Processes(procs),
            Err(msg) => Answer::Err(ShellError::new(ErrorKind::Io, Span::ZERO, &msg, alloc)),
        },
        Request::SpawnExternal { program, args, cwd, env, stdin, mode, forced_external } => {
            // Fast in-process `libbgit` execution for `bgit` and natively-backed `git` commands
            // (zero fork/exec; bypassed on explicit `^git` / `^bgit` escape or piped stdin).
            if !forced_external
                && stdin.is_none()
                && let Some(res) = try_run_in_process_bgit(
                    program.as_str(),
                    &args,
                    cwd.as_str(),
                    mode == ExternalMode::Interactive,
                    alloc,
                )
            {
                return Answer::External(res);
            }
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
                    duration_ns: 0,
                    stdout: AVec::new_in(alloc),
                    stderr: AVec::new_in(alloc),
                    chunks: AVec::new_in(alloc),
                    interactive: true,
                })
            } else {
                match run_external_captured(
                    program.as_str(),
                    &args,
                    cwd.as_str(),
                    &env,
                    stdin.as_ref().map(|b| b.as_slice()),
                    mode == ExternalMode::Interactive,
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
        FsOp::Rm { path, recursive, force, permanent } => {
            let p = resolve_path(path.as_str(), cwd, home);
            let meta = match fs::symlink_metadata(&p) {
                Ok(m) => m,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    return if *force {
                        Ok(())
                    } else {
                        Err(format!("rm: `{}`: not found", p.display()))
                    };
                }
                Err(e) => return Err(format!("rm `{}`: {e}", p.display())),
            };
            let is_dir = meta.file_type().is_dir();
            if is_dir && !*recursive {
                return Err(format!("rm: `{}`: is a directory; pass -r", p.display()));
            }
            #[cfg(target_os = "macos")]
            if !*permanent {
                return move_to_macos_trash(&p, is_dir).map(|_| ());
            }
            #[cfg(not(target_os = "macos"))]
            let _ = permanent;
            if is_dir {
                fs::remove_dir_all(&p).map_err(|e| format!("rm `{}`: {e}", p.display()))
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

#[cfg(target_os = "macos")]
fn move_to_macos_trash(path: &Path, is_dir: bool) -> Result<Option<PathBuf>, String> {
    use objc2::rc::{Retained, autoreleasepool};
    use objc2_foundation::{NSFileManager, NSString, NSURL};

    let Some(path_str) = path.to_str() else {
        return Err(format!(
            "rm: `{}`: non-UTF-8 path cannot be moved to macOS Trash (pass -P/--permanent to delete)",
            path.display()
        ));
    };
    autoreleasepool(|_| {
        let ns_path = NSString::from_str(path_str);
        let url = NSURL::fileURLWithPath_isDirectory(&ns_path, is_dir);
        let fm = NSFileManager::defaultManager();
        let mut resulting: Option<Retained<NSURL>> = None;
        fm.trashItemAtURL_resultingItemURL_error(&url, Some(&mut resulting))
            .map_err(|err| {
                format!(
                    "rm (trash) `{}`: {err} (pass -P/--permanent to delete permanently)",
                    path.display()
                )
            })?;
        let trashed = resulting
            .and_then(|u| u.path())
            .map(|p| PathBuf::from(p.to_string()));
        Ok(trashed)
    })
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
    #[cfg(target_os = "macos")]
    {
        let now_secs = match SystemTime::now().duration_since(UNIX_EPOCH) {
            Ok(d) => d.as_secs().min(i64::MAX as u64) as i64,
            Err(e) => -(e.duration().as_secs().min(i64::MAX as u64) as i64),
        };
        blaze_sys::posix::io::localtime(now_secs).map_or(0, |tm| tm.gmtoff as i32)
    }
    #[cfg(not(target_os = "macos"))]
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
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    if let Ok(uts) = blaze_sys::posix::thread::system_name() {
        #[cfg(target_os = "macos")]
        let bytes: Vec<u8> = uts
            .nodename
            .iter()
            .take_while(|&&c| c != 0)
            .map(|&c| c as u8)
            .collect();
        #[cfg(target_os = "linux")]
        let bytes: Vec<u8> = uts
            .nodename
            .iter()
            .take_while(|&&b| b != 0)
            .copied()
            .collect();
        if let Ok(s) = String::from_utf8(bytes)
            && !s.is_empty()
        {
            return s;
        }
    }
    std::env::var("HOSTNAME").unwrap_or_else(|_| "localhost".to_owned())
}

fn read_os_mem_uptime() -> (i64, i64, i64) {
    #[cfg(target_os = "macos")]
    {
        let (mem_total, mem_used) = match blaze_sys::macos_proc::vm_mem_info() {
            Ok(vm) => {
                let used = vm
                    .active_bytes
                    .saturating_add(vm.wired_bytes)
                    .saturating_add(vm.compressed_bytes);
                (
                    vm.total_bytes.min(i64::MAX as u64) as i64,
                    used.min(i64::MAX as u64) as i64,
                )
            }
            Err(_) => (0, 0),
        };
        let uptime_ns = blaze_sys::macos_proc::boot_time_secs()
            .ok()
            .and_then(|boot_secs| {
                let now_secs = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs();
                Some(now_secs.saturating_sub(boot_secs).saturating_mul(1_000_000_000))
            })
            .map_or(0, |ns| ns.min(i64::MAX as u64) as i64);
        (mem_total, mem_used, uptime_ns)
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
    rich_color: bool,
    pid_out: Option<&Arc<AtomicU32>>,
    alloc: Alloc<'static>,
) -> Result<ExternalResult<'static>, String> {
    let mut cmd = Command::new(program);
    for a in args.iter() {
        cmd.arg(a.as_str());
    }
    cmd.current_dir(cwd);
    let mut has_no_color = false;
    let mut has_term = false;
    let mut has_colorterm = false;
    let mut has_clicolor = false;
    let mut has_clicolor_force = false;
    let mut has_force_color = false;
    let mut has_cargo_color = false;
    let mut has_gh_tty = false;
    let mut has_columns = false;
    let mut has_git_pager = false;
    let mut has_git_cfg = false;
    if !env.is_empty() {
        cmd.env_clear();
        for (k, v) in env.iter() {
            match k.as_ref() {
                "NO_COLOR" if !v.as_ref().is_empty() => has_no_color = true,
                "TERM" => has_term = true,
                "COLORTERM" => has_colorterm = true,
                "CLICOLOR" => has_clicolor = true,
                "CLICOLOR_FORCE" => has_clicolor_force = true,
                "FORCE_COLOR" => has_force_color = true,
                "CARGO_TERM_COLOR" => has_cargo_color = true,
                "GH_FORCE_TTY" => has_gh_tty = true,
                "COLUMNS" => has_columns = true,
                "GIT_PAGER" => has_git_pager = true,
                "GIT_CONFIG_COUNT" => has_git_cfg = true,
                _ => {}
            }
            cmd.env(k.as_ref(), v.as_ref());
        }
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
    cmd.env("TERM_PROGRAM", "blaze-shell");
    // When this command's output is rendered directly to the user (not piped into another
    // blaze-sh stage), tell modern CLIs (`rg`, `cargo`, `git`, `gh`, `bun`, `pytest`, `bat`,
    // `eza`, `npm`) to emit full ANSI 24-bit color & pretty tables while we still capture
    // clean, separate, time-interleaved `stdout`, `stderr`, and `exit_code`!
    if rich_color && !has_no_color {
        if !has_clicolor_force {
            cmd.env("CLICOLOR_FORCE", "1");
        }
        if !has_force_color {
            cmd.env("FORCE_COLOR", "1");
        }
        if !has_cargo_color {
            cmd.env("CARGO_TERM_COLOR", "always");
        }
        if !has_gh_tty {
            cmd.env("GH_FORCE_TTY", "120");
        }
        if !has_columns {
            cmd.env("COLUMNS", "120");
        }
        cmd.env("PY_COLORS", "1");
        let prog_base = program.rsplit('/').next().unwrap_or(program);
        if prog_base == "git" {
            if !has_git_pager {
                cmd.env("GIT_PAGER", "cat");
            }
            if !has_git_cfg {
                cmd.env("GIT_CONFIG_COUNT", "1");
                cmd.env("GIT_CONFIG_KEY_0", "color.ui");
                cmd.env("GIT_CONFIG_VALUE_0", "always");
            }
        }
    }
    cmd.stdin(if stdin_bytes.is_some() { Stdio::piped() } else { Stdio::null() });
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());

    let started = Instant::now();
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

    // Stream stdout & stderr concurrently into one timestamped event channel so exact arrival
    // interleaving is preserved for both `Value::Output` display and `.events` pipelines!
    let (ev_tx, ev_rx) = std::sync::mpsc::channel::<(i64, OutputStream, Vec<u8>)>();
    let out_reader = child.stdout.take().map(|mut p| {
        let tx = ev_tx.clone();
        thread::spawn(move || {
            let mut total = 0usize;
            let mut buf = [0u8; 8192];
            while total < (16 << 20) {
                match p.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        total += n;
                        let at_ns = started.elapsed().as_nanos().min(i64::MAX as u128) as i64;
                        let _ = tx.send((at_ns, OutputStream::Stdout, buf[..n].to_vec()));
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(_) => break,
                }
            }
        })
    });
    let err_reader = child.stderr.take().map(|mut p| {
        let tx = ev_tx.clone();
        thread::spawn(move || {
            let mut total = 0usize;
            let mut buf = [0u8; 8192];
            while total < (4 << 20) {
                match p.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        total += n;
                        let at_ns = started.elapsed().as_nanos().min(i64::MAX as u128) as i64;
                        let _ = tx.send((at_ns, OutputStream::Stderr, buf[..n].to_vec()));
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(_) => break,
                }
            }
        })
    });
    drop(ev_tx);

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

    if let Some(h) = out_reader {
        let _ = h.join();
    }
    if let Some(h) = err_reader {
        let _ = h.join();
    }
    let duration_ns = started.elapsed().as_nanos().min(i64::MAX as u128) as i64;

    let mut events: Vec<(i64, OutputStream, Vec<u8>)> = ev_rx.into_iter().collect();
    events.sort_by_key(|(at, _, _)| *at);

    let mut stdout = AVec::new_in(alloc);
    let mut stderr = AVec::new_in(alloc);
    let mut chunks = AVec::with_capacity_in(events.len(), alloc);
    for (at_ns, stream, data) in events {
        match stream {
            OutputStream::Stdout => stdout.extend_from_slice(&data),
            OutputStream::Stderr => stderr.extend_from_slice(&data),
        }
        chunks.push(OutputChunk {
            at: Nanos(at_ns),
            stream,
            bytes: Rc::from_slice_in(&data, alloc),
        });
    }

    Ok(ExternalResult {
        exit_code: status.code().unwrap_or(1),
        duration_ns,
        stdout,
        stderr,
        chunks,
        interactive: false,
    })
}

/// Runs `bgit` and natively-backed `git` commands directly in-process via `libbgit` (zero
/// `fork`/`exec`, microsecond latency, thread-safe captured stdout/stderr).
fn try_run_in_process_bgit(
    program: &str,
    args: &[AString<'static>],
    cwd: &str,
    rich_color: bool,
    alloc: Alloc<'static>,
) -> Option<ExternalResult<'static>> {
    let prog_base = program.rsplit('/').next().unwrap_or(program);
    if prog_base != "bgit" && prog_base != "git" {
        return None;
    }
    let arg_refs: Vec<&str> = args.iter().map(AString::as_str).collect();
    let cwd_path = Path::new(cwd);
    let _ = bgit::ensure_fsmonitor_watching(cwd_path);
    let started = Instant::now();

    let exec = if prog_base == "bgit" {
        if bgit::is_streaming_bgit_command(&arg_refs) {
            return None;
        }
        bgit::run_bgit_in_process(&arg_refs, cwd_path, rich_color, alloc)
    } else {
        bgit::try_run_git_via_bgit(&arg_refs, cwd_path, rich_color, alloc)?
    };

    let duration_ns = started.elapsed().as_nanos().min(i64::MAX as u128) as i64;
    let mut stdout = AVec::with_capacity_in(exec.stdout.len(), alloc);
    stdout.extend_from_slice(&exec.stdout);
    let mut stderr = AVec::with_capacity_in(exec.stderr.len(), alloc);
    stderr.extend_from_slice(&exec.stderr);
    let mut chunks = AVec::new_in(alloc);
    if !exec.stdout.is_empty() {
        chunks.push(OutputChunk {
            at: Nanos(duration_ns),
            stream: OutputStream::Stdout,
            bytes: Rc::from_slice_in(&exec.stdout, alloc),
        });
    }
    if !exec.stderr.is_empty() {
        chunks.push(OutputChunk {
            at: Nanos(duration_ns),
            stream: OutputStream::Stderr,
            bytes: Rc::from_slice_in(&exec.stderr, alloc),
        });
    }

    Some(ExternalResult {
        exit_code: exec.exit_code,
        duration_ns,
        stdout,
        stderr,
        chunks,
        interactive: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unique_temp_dir(tag: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!(
            "blaze_shell_rm_{tag}_{}_{}",
            std::process::id(),
            nanos
        ));
        fs::create_dir_all(&dir).expect("create temp test dir");
        dir
    }

    #[test]
    fn rm_missing_respects_force_and_dir_requires_recursive() {
        let dir = unique_temp_dir("flags");
        let sub = dir.join("subdir");
        fs::create_dir(&sub).expect("create subdir");
        let cwd = dir.to_str().expect("utf8 cwd");
        let alloc = Alloc::global();

        // Missing file without -f fails.
        let op_missing = FsOp::Rm {
            path: AString::from_str_in("nope.txt", alloc),
            recursive: false,
            force: false,
            permanent: true,
        };
        let err = run_fs_op(&op_missing, cwd, "").expect_err("missing must fail");
        assert!(err.contains("not found"), "unexpected err: {err}");

        // Missing file with -f succeeds silently (both Trash and permanent).
        let op_force = FsOp::Rm {
            path: AString::from_str_in("nope.txt", alloc),
            recursive: false,
            force: true,
            permanent: false,
        };
        run_fs_op(&op_force, cwd, "").expect("-f on missing must succeed");

        // Directory without -r fails with `is a directory; pass -r`.
        let op_dir = FsOp::Rm {
            path: AString::from_str_in("subdir", alloc),
            recursive: false,
            force: false,
            permanent: false,
        };
        let dir_err = run_fs_op(&op_dir, cwd, "").expect_err("dir without -r must fail");
        assert!(
            dir_err.contains("is a directory; pass -r"),
            "unexpected err: {dir_err}"
        );
        assert!(sub.exists(), "subdir must not have been deleted");

        // Directory with -r and --permanent deletes permanently.
        let op_dir_perm = FsOp::Rm {
            path: AString::from_str_in("subdir", alloc),
            recursive: true,
            force: false,
            permanent: true,
        };
        run_fs_op(&op_dir_perm, cwd, "").expect("-r -P must remove dir");
        assert!(!sub.exists());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn rm_permanent_deletes_file() {
        let dir = unique_temp_dir("perm");
        let file = dir.join("gone.txt");
        fs::write(&file, b"bye").expect("write test file");
        let cwd = dir.to_str().expect("utf8 cwd");
        let alloc = Alloc::global();

        let op = FsOp::Rm {
            path: AString::from_str_in("gone.txt", alloc),
            recursive: false,
            force: false,
            permanent: true,
        };
        run_fs_op(&op, cwd, "").expect("permanent rm must succeed");
        assert!(!file.exists());

        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn rm_default_moves_to_macos_trash_and_cleans_up() {
        let dir = unique_temp_dir("trash");
        let file = dir.join("trash_me.txt");
        fs::write(&file, b"recoverable").expect("write test file");

        let trashed = move_to_macos_trash(&file, false)
            .expect("NSFileManager::trashItemAtURL should succeed");
        assert!(!file.exists(), "source file should be moved out of original dir");
        if let Some(ref trashed_path) = trashed {
            assert!(
                trashed_path.exists(),
                "trashed item should exist at {}",
                trashed_path.display()
            );
            // Clean up test file from Trash so unit tests leave no clutter.
            let _ = fs::remove_file(trashed_path);
        }

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn blaze_shell_routes_git_and_bgit_in_process_via_libbgit_and_respects_caret_escape() {
        let dir = unique_temp_dir("libbgit");
        let run_real_git = |args: &[&str]| {
            let st = Command::new("git")
                .current_dir(&dir)
                .env("GIT_AUTHOR_NAME", "Test")
                .env("GIT_AUTHOR_EMAIL", "test@ix.dev")
                .env("GIT_COMMITTER_NAME", "Test")
                .env("GIT_COMMITTER_EMAIL", "test@ix.dev")
                .args(args)
                .output()
                .expect("git");
            assert!(st.status.success(), "git {args:?} failed: {}", String::from_utf8_lossy(&st.stderr));
        };
        run_real_git(&["init", "-b", "main"]);
        fs::write(dir.join("hello.txt"), "hi\n").unwrap();
        run_real_git(&["add", "."]);
        run_real_git(&["commit", "-m", "initial commit"]);
        fs::write(dir.join("dirty.txt"), "untracked\n").unwrap();

        let alloc = Alloc::global();
        let cwd = dir.to_str().unwrap();
        let path_val = std::env::var("PATH").unwrap_or_default();
        let env_pairs = [("PATH", path_val.as_str())];

        let run = |cmd: &str| {
            let state = EngineState::new(cwd, &env_pairs, alloc);
            run_command_line(cmd, state, false)
        };

        let value_stdout = |v: &blaze_sh::Value<'static>| -> String {
            match v {
                blaze_sh::Value::Output(o) => {
                    String::from_utf8_lossy(o.stdout_bytes(alloc).as_slice()).into_owned()
                }
                blaze_sh::Value::String(s) => s.as_ref().to_owned(),
                other => format!("{other:?}"),
            }
        };

        // 1. `bgit infect` runs directly in-process via `libbgit` through `blaze-sh` host dispatch.
        let res_infect = run("bgit infect");
        assert_eq!(res_infect.outcome.exit_code, 0);
        assert!(dir.join(".bgit/VERSION").is_file());

        // 2. `git status --porcelain | lines` runs in-process via `libbgit` and feeds structured pipeline!
        let res_pipe = run("git status --porcelain | lines");
        assert_eq!(res_pipe.outcome.exit_code, 0);
        if let blaze_sh::Value::List(items) = &res_pipe.outcome.value {
            assert_eq!(items.len(), 1);
            let first = items.iter().next().expect("first line");
            assert_eq!(value_stdout(first), "?? dirty.txt");
        } else {
            panic!("expected List from `git status --porcelain | lines`, got {:?}", res_pipe.outcome.value);
        }

        // 3. `git rev-parse --abbrev-ref HEAD` and `git log --oneline -n 1` run in-process via `libbgit`.
        let res_br = run("git rev-parse --abbrev-ref HEAD");
        assert_eq!(res_br.outcome.exit_code, 0);
        assert_eq!(value_stdout(&res_br.outcome.value).trim(), "main");

        let res_log = run("git log --oneline -n 1");
        assert_eq!(res_log.outcome.exit_code, 0);
        let log_str = value_stdout(&res_log.outcome.value);
        assert!(log_str.contains("initial commit"), "unexpected log: {log_str}");

        // 4. Explicit `^git --version` bypasses `libbgit` and invokes external `git`.
        let res_ext = run("^git --version");
        assert_eq!(res_ext.outcome.exit_code, 0);
        let ver_str = value_stdout(&res_ext.outcome.value);
        assert!(ver_str.contains("git version"), "expected external git version, got: {ver_str}");

        // 5. `try_native_git_command` returns a rich structured Table for `git log` / `bgit log`!
        let log_tbl = crate::prompt::try_native_git_command("git log -n 1", cwd)
            .expect("git log should return rich table");
        assert!(matches!(log_tbl, blaze_sh::Value::Table(t) if t.nrows == 1));

        let _ = fs::remove_dir_all(&dir);
    }
}
