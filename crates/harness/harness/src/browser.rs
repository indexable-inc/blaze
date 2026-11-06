//! The harness browser's host (crates/harness/harness-browser/README.md): the embedded Chromium this daemon owns. The
//! first `browser` request starts it, every request answers where it listens ([`Reply::Browser`]), a request after it
//! died starts it again, and the daemon kills it on shutdown. There is no stand-in: when the host cannot start, the
//! request's error says why (the bundle missing, CEF failing), naming the log and the host's own words.
//!
//! One start at a time: a request that comes while another starts the host waits behind it and shares its outcome (the
//! host it started, or the error it got), instead of starting a second one.
use crate::paths;
use anyhow::{Context, Result, anyhow, bail};
use harness_proto::msg::Reply;
use parking_lot::Mutex;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::LazyLock;
use std::sync::mpsc::{RecvTimeoutError, channel};
use std::time::{Duration, Instant};

/// How long the host has to print its ready line after it is started.
const READY_WAIT: Duration = Duration::from_secs(20);
/// The bundle's name, and its executable inside it.
const APP: &str = "Harness Browser.app";
const EXE: &str = "Contents/MacOS/Harness Browser";
/// How the bundle is made (crates/harness/scripts/cef.py).
const BUNDLE: &str = r#"uv run --frozen crates/harness/scripts/cef.py '{"op": "bundle", "profile": "release"}'"#;
/// What the host prints on stdout once it serves its socket and Mach service, or when it cannot start.
const READY: &str = "harness-browser ready pid=";
const FAILED: &str = "harness-browser failed: ";
/// How much of the host's stderr an error quotes.
const TAIL_BYTES: u64 = 4096;

/// The running host.
struct Host {
    child: Child,
    /// The pid its ready line said.
    pid: u32,
}

#[derive(Default)]
struct State {
    host: Option<Host>,
    /// Starts begun so far, and the last one's error while no host runs: a request that waited behind a start that
    /// failed answers that error.
    starts: u64,
    failed: Option<(u64, String)>,
}

static STATE: LazyLock<Mutex<State>> = LazyLock::new(|| Mutex::new(State::default()));
/// Held for one start (up to [`READY_WAIT`]); [`STATE`] is held only for its field updates.
static STARTING: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

/// `browser`: this daemon's host, started if it is not running, and where it listens.
///
/// # Errors
/// Why the host is not running: its bundle is missing, it failed or printed no ready line in time (with its words, its
/// stderr's tail and where its log is), or a start this request waited behind failed so.
pub fn request() -> Result<Reply> {
    let seen = STATE.lock().starts;
    let _one = STARTING.lock();
    let start = {
        let mut st = STATE.lock();
        if let Some(h) = st.host.as_mut() {
            match h.child.try_wait() {
                Ok(None) => return Ok(reply(h.pid)),
                Ok(Some(status)) => {
                    eprintln!("harness browser: the host (pid {}) {status} since it last answered; starting it again", h.pid);
                    st.host = None;
                }
                Err(e) => bail!("harness browser: checking whether the host (pid {}) still runs: {e}", h.pid),
            }
        }
        if let Some((n, error)) = &st.failed
            && *n > seen
        {
            bail!("{error}");
        }
        st.starts += 1;
        st.starts
    };
    match start_host() {
        Ok(host) => {
            let pid = host.pid;
            let mut st = STATE.lock();
            st.host = Some(host);
            st.failed = None;
            Ok(reply(pid))
        }
        Err(e) => {
            STATE.lock().failed = Some((start, format!("{e:#}")));
            Err(e)
        }
    }
}

/// Kill the host, if one runs (the daemon is shutting down). The host also exits by itself when this daemon's pid does.
pub fn stop() {
    let Some(mut h) = STATE.lock().host.take() else { return };
    if let Err(e) = h.child.kill() {
        eprintln!("harness browser: killing the host (pid {}): {e}", h.pid);
    }
    let _ = h.child.wait();
}

fn reply(pid: u32) -> Reply {
    let home = paths::home();
    Reply::Browser {
        pid,
        sock: harness_proto::browser::dir(&home).join(harness_proto::browser::SOCK).display().to_string(),
        service: harness_proto::browser::service_name(&home),
    }
}

/// The bundle: `$HARNESS_BROWSER_APP`, else `Harness Browser.app` beside this executable (`target/<profile>/`, where
/// cef.py's bundle puts it; one level up for a binary in `target/<profile>/deps`).
fn find_app() -> Result<PathBuf> {
    if let Some(p) = std::env::var_os("HARNESS_BROWSER_APP").map(PathBuf::from) {
        if p.join(EXE).is_file() {
            return Ok(p);
        }
        bail!(
            "harness browser (embedded Chromium): HARNESS_BROWSER_APP is {}, which has no {EXE}; point it at a bundle {BUNDLE} made, or unset it to use the one beside the daemon",
            p.display()
        );
    }
    let exe = std::env::current_exe()
        .and_then(|e| e.canonicalize())
        .context("harness browser (embedded Chromium): the daemon's own path, to find Harness Browser.app beside it")?;
    let dir = exe.parent().ok_or_else(|| anyhow!("harness browser: the daemon's path {} has no directory", exe.display()))?;
    let mut looked = vec![dir.join(APP)];
    if matches!(dir.file_name().and_then(|n| n.to_str()), Some("deps" | "examples"))
        && let Some(up) = dir.parent()
    {
        looked.push(up.join(APP));
    }
    if let Some(found) = looked.iter().find(|p| p.join(EXE).is_file()) {
        return Ok(found.clone());
    }
    let said: Vec<String> = looked.iter().map(|p| p.display().to_string()).collect();
    bail!(
        "harness browser (embedded Chromium) is not built: HARNESS_BROWSER_APP is unset and there is no {APP} at {}; build harness-browser \
         (cd crates/harness && cargo build --release -p harness-browser) and bundle it with {BUNDLE}",
        said.join(" or ")
    )
}

/// The browser's state directory, 0700 (README: Processes).
fn state_dir(home: &Path) -> Result<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let dir = harness_proto::browser::dir(home);
    std::fs::create_dir_all(&dir).with_context(|| format!("harness browser: creating {}", dir.display()))?;
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))
        .with_context(|| format!("harness browser: making {} private (0700)", dir.display()))?;
    Ok(dir)
}

/// What the host's stdout said, line by line, until its ready line (then it is the daemon's log's).
enum Out {
    Line(String),
    End,
}

/// Start the host and wait (at most [`READY_WAIT`]) for its ready line.
fn start_host() -> Result<Host> {
    let home = paths::home();
    let app = find_app()?;
    let exe = app.join(EXE);
    let dir = state_dir(&home)?;
    let log = dir.join("cef.log");
    let err_path = dir.join("host.stderr");
    let err_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&err_path)
        .with_context(|| format!("harness browser: opening {}", err_path.display()))?;
    let err_from = err_file.metadata().map(|m| m.len()).unwrap_or(0);
    let mut child = Command::new(&exe)
        .env("HARNESS_HOME", &home)
        .env("HARNESS_DAEMON_PID", std::process::id().to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(err_file)
        .spawn()
        .with_context(|| format!("harness browser: starting {}", exe.display()))?;
    let stdout = child.stdout.take().ok_or_else(|| anyhow!("harness browser: {} has no stdout pipe", exe.display()))?;
    let (tx, rx) = channel();
    let child_pid = child.id();
    std::thread::Builder::new().name("browser-stdout".into()).spawn(move || {
        let mut lines = BufReader::new(stdout).lines();
        while let Some(Ok(line)) = lines.next() {
            // Once the start is over (nobody receives), the host's words go to the daemon's log.
            if let Err(e) = tx.send(Out::Line(line)) {
                let Out::Line(line) = e.0 else { unreachable!() };
                eprintln!("harness-browser[{child_pid}]: {line}");
            }
        }
        let _ = tx.send(Out::End);
    })?;
    let why = |what: String| -> anyhow::Error {
        anyhow!(
            "harness browser (embedded Chromium) {} {what}; its log is {}{}",
            exe.display(),
            log.display(),
            tail(&err_path, err_from).map(|t| format!(", and its stderr ended:\n{t}")).unwrap_or_default()
        )
    };
    let deadline = Instant::now() + READY_WAIT;
    loop {
        match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(Out::Line(line)) => {
                if let Some(pid) = line.strip_prefix(READY) {
                    let pid = pid.trim().parse::<u32>().map_err(|e| why(format!("printed a ready line whose pid is not a number ({line:?}: {e})")))?;
                    eprintln!("harness browser: host pid {pid} ready ({})", app.display());
                    return Ok(Host { child, pid });
                }
                if let Some(failed) = line.strip_prefix(FAILED) {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(why(format!("failed to start: {failed}")));
                }
                eprintln!("harness-browser[{child_pid}]: {line}");
            }
            Ok(Out::End) | Err(RecvTimeoutError::Disconnected) => {
                let status = child.wait().map_or_else(|e| format!("could not be waited for ({e})"), |s| s.to_string());
                return Err(why(format!("exited ({status}) before printing `{READY}<n>`")));
            }
            Err(RecvTimeoutError::Timeout) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(why(format!("printed no `{READY}<n>` line within {} s, so it was killed", READY_WAIT.as_secs())));
            }
        }
    }
}

/// The last [`TAIL_BYTES`] the host wrote to its stderr file after byte `from` (this start's), or None when it wrote none.
fn tail(path: &Path, from: u64) -> Option<String> {
    let mut f = std::fs::File::open(path).ok()?;
    let len = f.metadata().ok()?.len();
    if len <= from {
        return None;
    }
    let start = from.max(len.saturating_sub(TAIL_BYTES));
    f.seek(SeekFrom::Start(start)).ok()?;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf).ok()?;
    let text = String::from_utf8_lossy(&buf).trim().to_owned();
    (!text.is_empty()).then_some(text)
}
