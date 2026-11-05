//! The seed's one-shot build (docs/future/bootstrap.md): with no `native` feature there is no
//! `blaze serve` to connect to, so a command starts the same engine in its own process
//! ([`super::engine`], [`super::start`]: the daemon's, never a second build path), speaks to it
//! through one in-process connection with the socket's frames ([`blaze_api::client::Local`]),
//! and stops it as the command ends, its image written as a daemon's is when it stops.

use std::io;

use blaze_api::client::{Client, Local};
use blaze_core::Conn;

use super::{Running, Serving, engine, signals, start};

/// A client of the engine started in this process for this command alone.
pub(crate) fn connect() -> anyhow::Result<Client> {
    let serving = Serving {
        socket: None,
        http: None,
        #[cfg(unix)]
        activated: None,
        stall: None,
        memory: None,
    };
    let engine = engine(serving, None)?;
    let mut running = start(engine, "blaze", None)?;
    let Some(conn) = running.daemon.connect_local() else {
        let failed = running.daemon.failed().map(|why| format!(": {why}")).unwrap_or_default();
        running.end()?;
        anyhow::bail!(
            "the build engine started in this process took no connection of its own{failed}"
        );
    };
    // alloc: once a command, the client's link
    Ok(Client::local(Box::new(Once { running: Some(running), conn, told: false })))
}

/// The engine and this command's connection to it; `running` is taken as it stops.
struct Once {
    running: Option<Running>,
    conn: Conn,
    /// Whether a stop signal was passed on to the engine already.
    told: bool,
}

impl Once {
    fn running(&mut self) -> io::Result<&mut Running> {
        self.running
            .as_mut()
            .ok_or_else(|| io::Error::other("the build engine in this process was already stopped"))
    }
}

impl Local for Once {
    fn send(&mut self, frame: &[u8]) -> io::Result<()> {
        let conn = self.conn;
        self.running()?.daemon.send_local(conn, frame);
        Ok(())
    }

    /// Turns the engine until it has replied: each turn is bounded by the engine's own
    /// deadlines, and the engine stopping (failed, or by a signal) ends the wait, named.
    fn receive(&mut self, into: &mut Vec<u8>) -> io::Result<()> {
        let conn = self.conn;
        let mut told = self.told;
        let running = self.running()?;
        let before = into.len();
        let ended = loop {
            if !running.daemon.take_local(conn, into) || into.len() > before {
                break Ok(());
            }
            if signals::signalled() && !told {
                told = true;
                let by = signals::stopped_by().unwrap_or_default();
                eprintln!("blaze: asked to stop by {by}: the build engine writes its image first");
                running.daemon.stop();
            }
            if signals::dump_asked() {
                tracing::warn!("SIGUSR1: {}", blaze_profile::watchdog::running());
            }
            if !running.daemon.turn() {
                break Err(match running.daemon.failed() {
                    Some(why) => {
                        io::Error::other(format!("the build engine in this process failed: {why}"))
                    }
                    None => io::Error::other(
                        "the build engine in this process stopped before it replied",
                    ),
                });
            }
        };
        self.told = told;
        ended
    }

    fn close(&mut self) -> io::Result<()> {
        let Some(mut running) = self.running.take() else {
            return Ok(());
        };
        running.daemon.stop();
        while running.daemon.turn() {}
        let failed = running
            .daemon
            .failed()
            .map(|why| format!("the build engine in this process failed as it stopped: {why}"));
        let marked = match failed {
            None => running.stopped().map_err(|why| io::Error::other(format!("{why:#}"))),
            Some(_) => Ok(()),
        };
        let ended = running.end().map_err(|why| {
            io::Error::other(format!("ending the build engine in this process: {why:#}"))
        });
        match failed {
            Some(why) => Err(io::Error::other(why)),
            None => ended.and(marked),
        }
    }
}

/// A client dropped without [`Client::close`] (an error path) still stops the engine, writing
/// its image; what goes wrong then is said, as nothing is left to return it to.
impl Drop for Once {
    fn drop(&mut self) {
        if let Err(why) = self.close() {
            eprintln!("blaze: {why}");
        }
    }
}
