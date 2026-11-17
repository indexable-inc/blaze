//! The thin `blaze`'s own check of a request with nothing changed: the request slot the
//! daemon wrote for it (`<state>/slots/<key hex>`, blaze-certify's `slot`) read whole, the
//! stamps that vouch for each read looked at with `lstat`, and the request machine driven
//! over them by blaze-certify's `solo::Solo`, which adds no rule of its own. A hit is said
//! here as the daemon would say it, and a `run` becomes its program, with no daemon asked,
//! none started, no store opened and no file hashed (/tmp/plan/10-no-daemon-run.md, section
//! 1.3; end-state/daemon.md 4.3).
//!
//! Anything the check does not vouch for is [`Ended::Daemon`], naming why: the request goes
//! to the daemon as it always has (`main.rs`), which decides by its own first state. That is
//! the design's "on `Failed` the client sends `Build`", not a second route to an answer.
//!
//! The driver of a sans-IO core: [`Solo`]'s looks are answered here, each one batch, and
//! the decision is the machine's.

use std::{
    io::Read as _,
    os::unix::fs::MetadataExt as _,
    path::Path,
    time::Duration,
};

use blaze_api::{Request, Response, ViewRef};
use blaze_certify::{
    slot,
    solo::{Look, MOST_ROUNDS, Seen, Solo, Step, Verdict},
};
use blaze_core::Stamp;

use crate::{
    answer::{Exec, Plain},
    route::Command,
    talk::{self, Spoken},
};

/// What the client's own check came to.
#[derive(Debug, PartialEq, Eq)]
pub enum Ended {
    /// Certified: `said` goes to stderr, and a `run` then becomes `exec`. `slot` is the
    /// request slot's key as its file is named, and `certified` the build whose answer it
    /// holds, where that answer named one: what the answer's kept rows say it was
    /// (`blaze_certify::kept::Kept`).
    Said { said: String, exec: Option<Exec>, slot: String, certified: Option<u64> },
    /// The daemon answers the request, for this reason.
    Daemon(String),
}

/// The most steps of a check this driver takes: the machine's own bound
/// ([`MOST_ROUNDS`]) and the first look, with room for a look each round.
const MOST_STEPS: u32 = 2 * MOST_ROUNDS + 2;

/// The check of `spoken`'s request over its slot in the daemon's state directory `state`, by
/// the engine at `engine` (resolved: the file itself). `took` is the time since the command
/// began, read as the answer is said.
pub fn solo(spoken: &Spoken<'_>, state: &Path, engine: &Path, took: impl FnOnce() -> Duration) -> Ended {
    match check(spoken, state, engine, took) {
        Ok(Checked { said, exec, slot, certified }) => Ended::Said { said, exec, slot, certified },
        Err(why) => {
            tracing::debug!(target: "blaze", why = %why, "thin: the client's own check leaves the request to the daemon");
            Ended::Daemon(why)
        }
    }
}

/// A check that certified its answer: [`Ended::Said`]'s fields.
struct Checked {
    said: String,
    exec: Option<Exec>,
    slot: String,
    certified: Option<u64>,
}

/// [`solo`]'s check: what is said and run, or why the daemon answers.
fn check(
    spoken: &Spoken<'_>,
    state: &Path,
    engine: &Path,
    took: impl FnOnce() -> Duration,
) -> Result<Checked, String> {
    let Request::Build { view: ViewRef::Head(root), targets } = spoken.request else {
        return Err("the request is not a build of a checkout's head".to_owned());
    };
    let Some(key) = slot::key_of("build", targets, root) else {
        return Err("the request has no cargo command line, so no slot".to_owned());
    };
    let env = targets.cargo.as_ref().map_or(&[][..], |it| it.env.as_slice());
    let engine = engine
        .to_str()
        .ok_or_else(|| format!("the engine's path {} is not UTF-8", engine.display()))?;
    let slot = format!("{key:x}"); // alloc: the slot's name, once a command
    // alloc: the slot's path, once a command
    let path = state.join("slots").join(&slot);
    let Some(bytes) = read_slot(&path)? else {
        return Err(format!("no request slot at {}", path.display()));
    };
    let daemon = holds_lock(&state.join("serve.lock"))?;
    let mut solo = Solo::open(&bytes, &key, env, engine, daemon).map_err(|why| why.to_string())?;
    drop(bytes);
    let mut seen = None;
    // Bounded: the machine ends in `Done` within its own rounds; past this many steps the
    // check is the daemon's, by name.
    let answer = 'checked: {
        for _ in 0..MOST_STEPS {
            match solo.step(seen.take()) {
                Step::Look(look) => seen = Some(look_at(look)?),
                Step::Done(Verdict::Hit(answer)) => break 'checked answer,
                Step::Done(Verdict::Daemon(why)) => return Err(why.to_string()),
            }
        }
        return Err(format!("the client's check did not end within {MOST_STEPS} steps"));
    };
    // What the build said, as a build with nothing to do says it: the daemon's encoding of
    // its responses (blaze-cargo's `certify_hit`), then the `Done` the daemon ends it with.
    let responses = blaze_protocol::cbor::from_slice::<Vec<Response>>(&answer.said)
        .map_err(|error| format!("the request slot's answer does not decode as what a build says: {error}"))?;
    let asked = spoken.asked;
    let mut plain = Plain::of(asked.command == Command::Build && !asked.quiet);
    let mut certified = None;
    for response in responses {
        if let Response::Work(worked) = &response {
            certified = worked.build_id;
        }
        match plain.take(response) {
            crate::answer::Step::More => {}
            crate::answer::Step::Done => return Err("the request slot's answer ends before its last response".to_owned()),
            crate::answer::Step::HandOver(why) => return Err(format!("the request slot's answer is the engine's to print: {why}")),
        }
    }
    match plain.take(Response::Done { ok: true }) {
        crate::answer::Step::Done => {}
        crate::answer::Step::More | crate::answer::Step::HandOver(_) => {
            return Err("the request slot's answer does not end as a build that went well".to_owned());
        }
    }
    let (said, exec) =
        talk::said(spoken, plain, took()).map_err(|()| "the build has no one program to run".to_owned())?;
    Ok(Checked { said, exec, slot, certified })
}

/// The slot's bytes, read whole (open, fstat, read, close), or `None` where there is none.
/// One longer than [`slot::MOST_BYTES`] is not read.
fn read_slot(path: &Path) -> Result<Option<Vec<u8>>, String> {
    let what = |error: std::io::Error| format!("reading the request slot {}: {error}", path.display());
    let mut file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(what(error)),
    };
    let len = file.metadata().map_err(what)?.len();
    if len > slot::MOST_BYTES as u64 {
        return Err(format!(
            "the request slot {} is {len} bytes, more than the {} a client reads",
            path.display(),
            slot::MOST_BYTES
        ));
    }
    let mut bytes = Vec::with_capacity(len as usize); // alloc: the slot, once a command
    file.read_to_end(&mut bytes).map_err(what)?;
    Ok(Some(bytes))
}

/// Whether a daemon holds `serve.lock` (`lock`): asked as blaze-cargo's `serve::holder` asks
/// it, by taking it shared without waiting, and letting go at once. No file: no daemon ever
/// ran on this state.
fn holds_lock(lock: &Path) -> Result<bool, String> {
    let file = match std::fs::File::open(lock) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(format!("opening the daemon's lock {}: {error}", lock.display())),
    };
    match stdx::file_lock::Locked::try_lock_shared(file) {
        // Taken, so no daemon holds it; let go as it drops, here.
        Ok(Ok(_free)) => Ok(false),
        Ok(Err(_held)) => Ok(true),
        Err(error) => Err(format!("asking who holds the daemon's lock {}: {error}", lock.display())),
    }
}

/// What the check asked to see.
fn look_at(look: Look) -> Result<Seen, String> {
    match look {
        Look::Stamps(paths) => Ok(Seen::Stamps(paths.iter().map(|it| stamp(Path::new(it))).collect())), // alloc: a stamp a path, once a look
        Look::Head { path, len } => head(Path::new(&path), len).map(Seen::Head),
    }
}

/// The stamp of the name at `path`, not followed through a link, as the daemon's sweep of a
/// column takes it (blaze-core's `fs::stamp_of` of an `lstat`): `Stamp::GONE` where nothing
/// is there, `Stamp::UNKNOWN` where the system would not say.
fn stamp(path: &Path) -> Stamp {
    match std::fs::symlink_metadata(path) {
        Ok(meta) => {
            let ns = |s: i64, n: i64| (s as u64).wrapping_mul(1_000_000_000).wrapping_add(n as u64);
            Stamp {
                ino: meta.ino(),
                size: meta.size(),
                mtime_ns: ns(meta.mtime(), meta.mtime_nsec()),
                ctime_ns: ns(meta.ctime(), meta.ctime_nsec()),
                mode: meta.mode(),
            }
        }
        Err(error) => match error.kind() {
            std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory => Stamp::GONE,
            _ => Stamp::UNKNOWN,
        },
    }
}

/// The first `len` bytes of the file at `path` (open, read, close): fewer where it is
/// shorter, `None` where no file is there.
fn head(path: &Path, len: usize) -> Result<Option<Vec<u8>>, String> {
    let what = |error: std::io::Error| format!("reading the head of {}: {error}", path.display());
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(what(error)),
    };
    let mut bytes = Vec::with_capacity(len); // alloc: a start file's head, once a check
    file.take(len as u64).read_to_end(&mut bytes).map_err(what)?;
    Ok(Some(bytes))
}
