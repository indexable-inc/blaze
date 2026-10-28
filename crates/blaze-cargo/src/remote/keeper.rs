//! A daemon that stays keeps its store and the remote in step while it runs.
//!
//! One task does it, a step at a time, on a thread of its own; no build waits for it. A build that lacked a key says so, and the task reads the manifest: when it is
//! not what it was, it pulls, and the next build finds what arrived. What the daemon
//! compiled is sealed and pushed every `BLAZE_REMOTE_PUSH_EVERY` (five minutes; `off` for
//! never) and once more when the daemon is told to end, after which it ends as it was told.

use std::{
    sync::{
        Arc, Mutex, OnceLock, PoisonError, RwLock,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use anyhow::Context;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

use blaze_store::remote_index::Hash;

use super::{Digest, Shared, Tiered, collect::parse_age, push};

/// How often what the daemon compiled is sent when nothing says otherwise.
const PUSH_EVERY: Duration = Duration::from_secs(5 * 60);
/// The manifest is read no more often than this, however many builds lack something.
const LOOK_EVERY: Duration = Duration::from_secs(5);
/// How long the push of a daemon that was told to end may take before it ends anyway.
const LAST_PUSH: Duration = Duration::from_secs(60);

/// `BLAZE_REMOTE_PUSH_EVERY`: a count and `s`, `m`, `h` or `d`, or `off`.
pub fn push_every_from_env() -> anyhow::Result<Option<Duration>> {
    match std::env::var("BLAZE_REMOTE_PUSH_EVERY") {
        Err(_) => Ok(Some(PUSH_EVERY)),
        Ok(text) if text == "off" => Ok(None),
        Ok(text) => {
            let every = parse_age(&text).filter(|it| !it.is_zero());
            Ok(Some(every.with_context(|| {
                format!("BLAZE_REMOTE_PUSH_EVERY=`{text}` is neither a time nor `off`")
            })?))
        }
    }
}

/// What the task is to do next.
enum Step {
    /// A build lacked a key.
    Look,
    Push,
    /// The daemon was told to end, by this signal.
    End(i32),
}

/// What the store asks when it lacks a key, and what is known of the remote to answer it.
#[derive(Default)]
pub struct Asked {
    /// The remote as a tier of the store, with `BLAZE_REMOTE_LAZY`: what fetches the pack
    /// of a key the store lacks, and waits for it.
    tier: RwLock<Option<Arc<Tiered>>>,
    /// What the manifest hashed to when it was last pulled from.
    seen: Mutex<Option<Digest>>,
    /// Whether a build lacked a key since the manifest was last looked at.
    lacked: AtomicBool,
    steps: OnceLock<UnboundedSender<Step>>,
}

impl Asked {
    pub fn read_through(&self, tier: Arc<Tiered>) {
        *self.tier.write().unwrap_or_else(PoisonError::into_inner) = Some(tier);
    }

    pub fn tiered(&self) -> Option<Arc<Tiered>> {
        self.tier.read().unwrap_or_else(PoisonError::into_inner).clone()
    }

    /// Whether the manifest is what it was when it was last pulled from.
    fn saw(&self, manifest: Option<Digest>) -> bool {
        self.seen.lock().is_ok_and(|seen| *seen == manifest)
    }

    /// Notes what the manifest is, and says whether that is not what it was.
    pub fn see(&self, manifest: Option<Digest>) -> bool {
        let Ok(mut seen) = self.seen.lock() else { return false };
        std::mem::replace(&mut *seen, manifest) != manifest
    }

    /// The store's question of a key it lacks (`blaze_store::FetchMissing`): through the
    /// tier, which waits for the pack or fails, with `BLAZE_REMOTE_LAZY`; otherwise the pull
    /// brings what there is. A key that stays lacking has the task look at the manifest,
    /// once for all that are lacking by then.
    pub fn fetch(&self, key: &[u8; 32], tried: &[Hash]) -> Result<Option<Hash>, String> {
        if let Some(tier) = self.tiered()
            && let Some(pack) = tier.fetch(key, tried)?
        {
            return Ok(Some(pack));
        }
        if !self.lacked.swap(true, Ordering::Relaxed)
            && let Some(steps) = self.steps.get()
        {
            _ = steps.send(Step::Look);
        }
        Ok(None)
    }
}

impl Shared {
    /// Starts the task: see the module. The first push is at once, for what the build
    /// before this added.
    pub fn keep(self) {
        let (steps, coming) = unbounded_channel();
        // What the first build lacked, it lacked after the pull before it.
        self.asked.lacked.store(false, Ordering::Relaxed);
        _ = self.asked.steps.set(steps.clone());
        let runtime = self.runtime.clone();
        runtime.spawn(told_to_end(steps.clone()));
        if let Some(every) = self.push_every.filter(|_| self.remote.may_write().is_ok()) {
            runtime.spawn(due(steps.clone(), every));
        }
        _ = steps.send(Step::Push);
        // A thread of its own waits on the runtime for it, as the threads that compile do.
        let keeping = move || runtime.block_on(self.kept(steps, coming));
        if let Err(e) =
            std::thread::Builder::new().name("blaze-remote-keeper".to_owned()).spawn(keeping)
        {
            eprintln!("remote: nothing is pulled or pushed from here on: {e}");
        }
    }

    async fn kept(self, steps: UnboundedSender<Step>, mut coming: UnboundedReceiver<Step>) {
        let mut looked: Option<Instant> = None;
        while let Some(step) = coming.recv().await {
            match step {
                Step::Look => {
                    // No build says it lacks a key again before this look is done.
                    let wait = looked.and_then(|it| LOOK_EVERY.checked_sub(it.elapsed()));
                    if let Some(wait) = wait.filter(|it| !it.is_zero()) {
                        let steps = steps.clone();
                        tokio::spawn(async move {
                            tokio::time::sleep(wait).await;
                            _ = steps.send(Step::Look);
                        });
                        continue;
                    }
                    looked = Some(Instant::now());
                    self.asked.lacked.store(false, Ordering::Relaxed);
                    self.look().await;
                }
                Step::Push => self.send().await,
                Step::End(signal) => {
                    if tokio::time::timeout(LAST_PUSH, self.send()).await.is_err() {
                        eprintln!("remote: the last push took over {LAST_PUSH:?} and was left");
                    }
                    end_by(signal);
                }
            }
        }
    }

    /// Pulls if the manifest is not what it was when it was last pulled from.
    async fn look(&self) {
        self.remote.traffic.try_again();
        let start = Instant::now();
        let now = self.manifest_now().await;
        if now.is_some() && self.asked.see(now) {
            eprintln!("remote: the manifest of `{}` has changed", self.project);
            let done = self.pull_as_set().await;
            _ = self.report(done, start);
        }
    }

    /// Seals what the daemon compiled and pushes it.
    async fn send(&self) {
        if self.remote.may_write().is_err() {
            return;
        }
        self.remote.traffic.try_again();
        let start = Instant::now();
        if tokio::task::spawn_blocking(blaze_store::seal_open).await.is_err() {
            eprintln!("remote: sealing the open pack panicked");
        }
        let ours = self.asked.saw(self.manifest_now().await);
        let done = push(&self.store, &self.remote, &self.project).await;
        let sent = done.as_ref().is_ok_and(|said| !said.starts_with("0 packs"));
        if sent {
            // Its own lines are no reason to pull; those another machine added before
            // them still are.
            if ours {
                self.asked.see(self.manifest_now().await);
            }
            _ = self.report(done, start);
        } else if let Err(e) = done {
            _ = self.failed(e);
        }
    }
}

async fn due(steps: UnboundedSender<Step>, every: Duration) {
    let mut due = tokio::time::interval_at(tokio::time::Instant::now() + every, every);
    loop {
        due.tick().await;
        if steps.send(Step::Push).is_err() {
            return;
        }
    }
}

/// Waits for the daemon to be told to end and has the task push first.
#[cfg(unix)]
async fn told_to_end(steps: UnboundedSender<Step>) {
    use std::pin::pin;

    use futures_util::future::{Either, select};
    use tokio::signal::unix::{SignalKind, signal};

    let (Ok(mut ended), Ok(mut interrupted)) =
        (signal(SignalKind::terminate()), signal(SignalKind::interrupt()))
    else {
        eprintln!("remote: no signal can be waited for: nothing is pushed when the daemon ends");
        return;
    };
    let by = {
        let (ended, interrupted) = (pin!(ended.recv()), pin!(interrupted.recv()));
        match select(ended, interrupted).await {
            Either::Left(_) => libc::SIGTERM,
            Either::Right(_) => libc::SIGINT,
        }
    };
    _ = steps.send(Step::End(by));
}

/// On Windows a process is told to end by an event of its console: Ctrl-C, Ctrl-Break, which
/// is the one that can be sent to a single process group, or the console closing.
#[cfg(windows)]
async fn told_to_end(steps: UnboundedSender<Step>) {
    use std::pin::pin;

    use futures_util::future::select;
    use tokio::signal::windows::{ctrl_break, ctrl_c, ctrl_close};

    let (Ok(mut interrupted), Ok(mut broken), Ok(mut closed)) =
        (ctrl_c(), ctrl_break(), ctrl_close())
    else {
        eprintln!("remote: no event can be waited for: nothing is pushed when the daemon ends");
        return;
    };
    let (interrupted, broken, closed) =
        (pin!(interrupted.recv()), pin!(broken.recv()), pin!(closed.recv()));
    select(select(interrupted, broken), closed).await;
    _ = steps.send(Step::End(CONTROL_C_EXIT));
}

/// How Windows ends a process that waits for none of those events.
#[cfg(windows)]
const CONTROL_C_EXIT: i32 = 0xC000_013A_u32 as i32;

/// Ends the process the way the signal would have, had nothing waited for it.
#[cfg(unix)]
fn end_by(signal: i32) -> ! {
    // SAFETY: setting a signal to what it does by default and raising it are both safe to
    // do from any thread.
    unsafe {
        libc::signal(signal, libc::SIG_DFL);
        libc::raise(signal);
    }
    std::process::exit(128 + signal)
}

#[cfg(windows)]
fn end_by(status: i32) -> ! {
    std::process::exit(status)
}
