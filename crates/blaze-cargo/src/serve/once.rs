//! The engine served in a command's own process: the daemon's code, driven here for one
//! request, with this command its only client, then the process exits.
//!
//! Two blazes run it: the seed (docs/future/bootstrap.md), built without the `native` feature,
//! which has no `blaze serve` to connect to; and a native blaze whose `config.toml` says
//! `[serve] host = "process"` (`blaze_thin::host`, `cli::daemon::connect`), the first step of
//! the architecture audit of 2026-11-13 (verdict (b): no long-lived daemon as the home of
//! state). Either way it is the same engine ([`super::engine`], [`super::start`]: the
//! daemon's, never a second build path), spoken to through one in-process connection with the
//! socket's frames ([`blaze_api::client::Local`]): no socket, no fork, no second process.
//!
//! The life of the process, each stretch a span (rules 15 and 16), so the profile says where
//! an in-process run spends its time:
//!
//! 1. `in-process: the process's start to the engine's main` (native): from the kernel's start
//!    of this process, kept across the thin `blaze`'s `exec` into the engine, so the thin's
//!    own certificate check (`blaze_thin::solo`) and the exec are in it; a named span of the
//!    command's client rows (`cli::client_rows::handed`), as no tracing span can begin before
//!    the program does.
//! 2. `in-process: start serving`: the state's lock, the history, the toolchain, the store,
//!    the image restored, the pools and the driver made ([`super::engine`], [`super::start`]).
//! 3. `in-process: turn the engine until it replies`: the request, served by the loop on this
//!    thread, once a read of the answer. The engine's own first state checks the request's
//!    certificate (the no-op lane) as a daemon's does, under the daemon's spans.
//! 4. `in-process: stop and flush`: the daemon's own stop ([`super::Daemon::stop`]): what the
//!    answered build left settles, the workspace members' pages and the image are written,
//!    and the store's, the shared store's and the VFS journal's flushes complete.
//! 5. `in-process: wait for the profile history's last flush` ([`super::Running::exit`]).
//!    The history's thread writes this process's rows as its last act, so this span's start
//!    is the last thing it holds; the exit itself is the script's wall time past it.
//!
//! The profile: this process claims the history's `owner.lock` for its life as `blaze serve`
//! does (`observe::keep_history`), writes its rows from its own `blaze-history` thread and lets
//! go as that thread's last flush ends; a later command, in its own process or a daemon,
//! claims it next. Two commands of one home at once: the second waits on the state's lock
//! (`serve.lock`, [`super::hold`]) until the first exits, as a daemon waits for the one before.
//!
//! The exit pays for no teardown: the database, the interners, the kept front ends and the
//! pools are never dropped ([`super::Running::exit`]); the kernel frees the process at once.
//! Everything durable is complete first: the stop ends only once no effect is in flight. A
//! crash before that loses only this run's work; the simulation's proof is
//! `serve::tests::in_process`.

use std::{io, time::Duration};

use blaze_api::client::{Client, Local};
use blaze_core::Conn;

use super::{Running, Serving, engine, signals, start};

/// How long the stop of an engine served in this process may take before it is said to have
/// not ended: one step of it writes the image, which grows with the engine (679 MiB took 11 s
/// in the self-build survey), so as long as a daemon's patience with its stop
/// (`cli::daemon::PATIENCE`), with room for what answered builds left to settle first.
const STOP_WITHIN: Duration = Duration::from_secs(600);

/// A client of the engine started in this process for this command alone.
pub(crate) fn connect() -> anyhow::Result<Client> {
    #[cfg(feature = "native")]
    {
        crate::cli::client_rows::served_here();
        // What the profile cannot say with a span of its own: the time before `main`.
        match before_main() {
            Ok((started_ns, main_ns)) => crate::cli::client_rows::handed(
                vec![blaze_api::Timed {
                    name: BEFORE_MAIN.to_owned(), // alloc: once a command
                    start_ns: started_ns,
                    end_ns: main_ns,
                }],
                started_ns,
            ),
            Err(why) => tracing::warn!(target: "blaze", "{BEFORE_MAIN}: not recorded: {why}"),
        }
    }
    let serving = Serving {
        socket: None,
        http: None,
        #[cfg(unix)]
        activated: None,
        stall: None,
    };
    let starting = tracing::info_span!(target: "blaze", "in-process: start serving").entered();
    let engine = engine(serving, None)?;
    let mut running = start(engine, "blaze", None)?;
    let Some(conn) = running.daemon.connect_local() else {
        let failed = running.daemon.failed().map(|why| format!(": {why}")).unwrap_or_default();
        running.end()?;
        anyhow::bail!(
            "the build engine started in this process took no connection of its own{failed}"
        );
    };
    drop(starting);
    // alloc: once a command, the client's link
    Ok(Client::local(Box::new(Once { running: Some(running), conn, told: false })))
}

/// The name of the span from the kernel's start of this process to the engine's `main`.
#[cfg(feature = "native")]
const BEFORE_MAIN: &str = "in-process: the process's start to the engine's main";

/// When this process started and when the engine's `main` began
/// ([`blaze_profile::began`]), both on the profile's clock (`blaze_profile::clock::mono_ns`):
/// the kernel's record of the start, read as its age and taken from now, as the thin `blaze`
/// reads its own (`blaze_thin`'s `kernel::started_ns`). macOS: `proc_pidinfo`, microseconds on
/// the wall clock; Linux: `/proc/self/stat`'s field 22, clock ticks since boot, against
/// `CLOCK_BOOTTIME`.
#[cfg(all(feature = "native", not(any(target_os = "linux", target_os = "macos"))))]
fn before_main() -> Result<(i64, i64), String> {
    Err(format!("blaze reads its own start on Linux and macOS, not {}", std::env::consts::OS))
}

/// [`before_main`] where the kernel says when this process started.
#[cfg(all(feature = "native", any(target_os = "linux", target_os = "macos")))]
fn before_main() -> Result<(i64, i64), String> {
    let main_ns = blaze_profile::clock::mono_of(blaze_profile::began());
    let pid = std::process::id();
    let started = crate::raw::process::started(pid)
        .map_err(|error| format!("when this process (pid {pid}) started: {error}"))?
        .ok_or_else(|| format!("the system names no start of this process (pid {pid})"))?;
    let now_ns = blaze_profile::clock::mono_ns();
    #[cfg(target_os = "macos")]
    let age_ns = {
        let wall_ns = i128::from(blaze_gc::record::now_ns());
        wall_ns - i128::from(started) * 1_000
    };
    #[cfg(target_os = "linux")]
    let age_ns = {
        let hz = crate::raw::process::clock_ticks().ok_or("sysconf(_SC_CLK_TCK) names no tick")?;
        let boot_ns = blaze_gc::record::boot_ns().ok_or("CLOCK_BOOTTIME can't be read")?;
        i128::from(boot_ns) - i128::from(started) * 1_000_000_000 / i128::from(hz)
    };
    let age_ns = i64::try_from(age_ns).map_err(|_| format!("this process's age of {age_ns} ns"))?;
    if age_ns < 0 {
        return Err(format!("the kernel says this process starts {} ns from now", -age_ns));
    }
    Ok((now_ns - age_ns, main_ns))
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
        let _span =
            tracing::info_span!(target: "blaze", "in-process: turn the engine until it replies").entered();
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

    /// The engine's stop, then the process's end with nothing dropped
    /// ([`Running::exit`]): what is durable is complete once the stop's last turn has no
    /// effect in flight. The stop is bounded by [`STOP_WITHIN`]; past it this is an error
    /// naming the bound, and nothing is dropped either way.
    fn close(&mut self) -> io::Result<()> {
        let Some(mut running) = self.running.take() else {
            return Ok(());
        };
        let stopped = {
            let _span = tracing::info_span!(target: "blaze", "in-process: stop and flush").entered();
            let since = std::time::Instant::now();
            running.daemon.stop();
            // Bounded: each turn by the engine's own deadlines, and the turns by `STOP_WITHIN`.
            loop {
                if !running.daemon.turn() {
                    break Ok(());
                }
                if since.elapsed() > STOP_WITHIN {
                    break Err(io::Error::other(format!(
                        "the build engine in this process did not end its stop (settle, image, flush) within {} s",
                        STOP_WITHIN.as_secs()
                    )));
                }
            }
        };
        let failed = running
            .daemon
            .failed()
            .map(|why| format!("the build engine in this process failed as it stopped: {why}"));
        let marked = match (&stopped, &failed) {
            (Ok(()), None) => running.stopped().map_err(|why| io::Error::other(format!("{why:#}"))),
            _ => Ok(()),
        };
        let ended = match stopped {
            // Over: nothing is in flight, so the history's last flush is all that is left.
            Ok(()) => running.exit(),
            // A stop that did not end is not dropped either: the process ends with this error,
            // and the next start finds the state's lock not marked stopped.
            Err(_) => {
                let _kept = std::mem::ManuallyDrop::new(running);
                Ok(())
            }
        }
        .map_err(|why| io::Error::other(format!("ending the build engine in this process: {why:#}")));
        stopped?;
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
