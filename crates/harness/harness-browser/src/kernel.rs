//! The kernel's socket, `$HARNESS_HOME/browser/host.sock` (0600): harness frames carrying deterministic-CBOR
//! [`KernelUp`] / [`KernelDown`]. One accept thread; per connection a reader thread (blocking reads: the hello, then
//! every message, posted to the UI thread) and a writer thread (draining a crossbeam channel the UI thread fills, so
//! nothing on the UI thread ever blocks on a socket). The UI thread owns every view and answers there (host.rs).
use crate::host::{self, Cmd};
use harness_proto::browser::{KernelDown, KernelUp, SOCK};
use harness_proto::{SCHEMA, cbor, frame};
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;

/// One message as the frame the writer thread sends.
#[must_use]
pub fn frame(msg: &KernelDown) -> Vec<u8> {
    frame::frame(frame::T_CBOR, &cbor::encode(msg))
}

/// Bind `dir/host.sock`. A file already there is removed only when nothing answers a connect to it (a host that died);
/// a live one is an error.
///
/// # Errors
/// Another host answers there, or the socket cannot be made or set to 0600, naming the path.
pub fn bind(dir: &Path) -> Result<UnixListener, String> {
    let path = dir.join(SOCK);
    if std::fs::symlink_metadata(&path).is_ok() {
        match UnixStream::connect(&path) {
            Ok(_) => return Err(format!("another harness browser host answers at {} (one host per HARNESS_HOME)", path.display())),
            Err(_) => std::fs::remove_file(&path).map_err(|e| format!("removing the stale socket {}: {e}", path.display()))?,
        }
    }
    let l = UnixListener::bind(&path).map_err(|e| format!("binding the kernel socket {}: {e}", path.display()))?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).map_err(|e| format!("chmod 0600 {}: {e}", path.display()))?;
    Ok(l)
}

/// Start the accept thread.
///
/// # Errors
/// The thread cannot be spawned.
pub fn serve(listener: UnixListener) -> Result<(), String> {
    std::thread::Builder::new()
        .name("harness-browser.accept".into())
        .spawn(move || accept_loop(&listener))
        .map(drop)
        .map_err(|e| format!("spawning the kernel socket's accept thread: {e}"))
}

fn accept_loop(listener: &UnixListener) {
    const MAX_ERRORS_IN_A_ROW: u32 = 100;
    let mut next: u64 = 1;
    let mut errors = 0;
    // Waits for: the next connection on host.sock (accept). Ends after MAX_ERRORS_IN_A_ROW failed accepts in a row,
    // which quits the host: no socket, no host.
    loop {
        match listener.accept() {
            Ok((stream, _)) => {
                errors = 0;
                let conn = next;
                next += 1;
                if let Err(e) = start(conn, stream) {
                    eprintln!("harness-browser: connection {conn}: {e}");
                }
            }
            Err(e) => {
                errors += 1;
                eprintln!("harness-browser: accept on host.sock failed ({errors} in a row): {e}");
                if errors >= MAX_ERRORS_IN_A_ROW {
                    host::post(Cmd::Quit { why: format!("accept on host.sock failed {MAX_ERRORS_IN_A_ROW} times in a row, last: {e}") });
                    return;
                }
            }
        }
    }
}

fn start(conn: u64, stream: UnixStream) -> Result<(), String> {
    let (tx, rx) = crossbeam_channel::unbounded::<Vec<u8>>();
    let mut w = stream.try_clone().map_err(|e| format!("cloning the socket for its writer: {e}"))?;
    std::thread::Builder::new()
        .name(format!("harness-browser.conn-{conn}.write"))
        .spawn(move || {
            // Waits for: the next frame the UI thread queued for connection `conn`. Ends when every sender is gone
            // (the connection dropped) or a write fails (the peer went away), and then shuts the socket so the reader
            // ends too.
            for f in rx {
                if w.write_all(&f).and_then(|()| w.flush()).is_err() {
                    break;
                }
            }
            let _ = w.shutdown(std::net::Shutdown::Both);
        })
        .map_err(|e| format!("spawning its writer thread: {e}"))?;
    std::thread::Builder::new()
        .name(format!("harness-browser.conn-{conn}.read"))
        .spawn(move || read_loop(conn, stream, tx))
        .map(drop)
        .map_err(|e| format!("spawning its reader thread: {e}"))
}

fn read_one(conn: u64, r: &mut UnixStream) -> Result<Option<KernelUp>, String> {
    let Some((t, payload)) = frame::read(r).map_err(|e| format!("reading connection {conn}: {e}"))? else { return Ok(None) };
    if t != frame::T_CBOR {
        return Err(format!("connection {conn} sent a frame of type {t}; the browser socket takes only CBOR frames (type {})", frame::T_CBOR));
    }
    cbor::decode::<KernelUp>(&payload).map(Some).map_err(|e| format!("connection {conn} sent a message that is no KernelUp: {e}"))
}

fn read_loop(conn: u64, mut stream: UnixStream, tx: crossbeam_channel::Sender<Vec<u8>>) {
    let refuse = |error: String| {
        let _ = tx.send(frame(&KernelDown::Error { req: None, error }));
    };
    // Waits for: connection `conn`'s hello.
    match read_one(conn, &mut stream) {
        Ok(Some(KernelUp::Hello { schema })) if schema == SCHEMA => {
            let _ = tx.send(frame(&KernelDown::Hello { schema: SCHEMA, pid: std::process::id() }));
        }
        Ok(Some(KernelUp::Hello { schema })) => {
            return refuse(format!("connection {conn} speaks harness schema {schema}; this browser host speaks schema {SCHEMA}: update the older side"));
        }
        Ok(Some(other)) => return refuse(format!("connection {conn}'s first message must be hello (schema {SCHEMA}), not {other:?}")),
        Ok(None) => return,
        Err(e) => return refuse(e),
    }
    if !host::post(Cmd::Connected { conn, tx: tx.clone() }) {
        return;
    }
    // Waits for: connection `conn`'s next message. Ends when it closes or sends what is not a frame.
    let why = loop {
        match read_one(conn, &mut stream) {
            Ok(Some(msg)) => {
                if !host::post(Cmd::Kernel { conn, msg }) {
                    break "the host is stopping".to_owned();
                }
            }
            Ok(None) => break "it closed".to_owned(),
            Err(e) => {
                refuse(e.clone());
                break e;
            }
        }
    };
    host::post(Cmd::Dropped { conn, why });
}
