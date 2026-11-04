//! One app at a time: the running app listens on `~/.harness/native.sock`; a second launch hands it one line (`focus` or
//! `select <session id>`) and exits.
use crate::link::home;
use std::io::{ErrorKind, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::time::Duration;

pub fn path() -> PathBuf {
    home().join("native.sock")
}

/// What a launch asks of the running app.
#[derive(Clone, Copy, Debug)]
pub enum Ask {
    Focus,
    Select(u64),
}

impl Ask {
    fn line(self) -> String {
        match self {
            Ask::Focus => "focus\n".into(),
            Ask::Select(id) => format!("select {id}\n"),
        }
    }

    fn parse(s: &str) -> Option<Self> {
        let s = s.trim();
        if s == "focus" {
            return Some(Ask::Focus);
        }
        s.strip_prefix("select ").and_then(|id| id.trim().parse().ok()).map(Ask::Select)
    }
}

/// `HARNESS_NATIVE_SELECT` (`s12` or `12`): the session to show.
pub fn select_env() -> Option<u64> {
    let v = std::env::var("HARNESS_NATIVE_SELECT").ok()?;
    let v = v.trim();
    v.strip_prefix('s').unwrap_or(v).parse().ok()
}

/// Become the one app: the listener, or `None` when another app took `ask`.
pub fn claim(ask: Ask) -> Result<Option<UnixListener>, String> {
    let p = path();
    std::fs::create_dir_all(home()).map_err(|e| e.to_string())?;
    match UnixListener::bind(&p) {
        Ok(l) => return Ok(Some(l)),
        Err(e) if e.kind() == ErrorKind::AddrInUse => {}
        Err(e) => return Err(format!("{}: {e}", p.display())),
    }
    if let Ok(mut s) = UnixStream::connect(&p) {
        let _ = s.set_write_timeout(Some(Duration::from_secs(2)));
        if s.write_all(ask.line().as_bytes()).is_ok() {
            return Ok(None);
        }
    }
    // Nobody listens: a socket left by an app that died. Take its place.
    std::fs::remove_file(&p).map_err(|e| format!("{}: {e}", p.display()))?;
    UnixListener::bind(&p).map(Some).map_err(|e| format!("{}: {e}", p.display()))
}

/// Hand each launch's ask to `on` (called on the listener's thread).
pub fn serve(l: UnixListener, on: impl Fn(Ask) + Send + 'static) {
    std::thread::Builder::new()
        .name("harness-single".into())
        .spawn(move || {
            for s in l.incoming().flatten() {
                let mut s = s;
                let _ = s.set_read_timeout(Some(Duration::from_secs(2)));
                let mut buf = String::new();
                let _ = (&mut s).take(256).read_to_string(&mut buf);
                if let Some(a) = Ask::parse(&buf) {
                    on(a);
                }
            }
        })
        .expect("single-instance thread");
}

/// The socket goes with the app.
pub fn release() {
    let _ = std::fs::remove_file(path());
}
