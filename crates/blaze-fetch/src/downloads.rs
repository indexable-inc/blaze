//! The downloads in flight in this process, each with its [`Meter`]: what the client's status
//! line names while a build waits on one, what `blaze status` lists, and what the build's
//! stall bound counts as progress (a byte arriving is progress, so a live download is never
//! stopped as stuck). The driver of [`crate::progress`]: it reads the clock and holds the
//! meters; every download blaze makes registers here ([`Downloads::begin`]) and feeds its
//! bytes in as they come.

use std::{
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use blaze_core::lock::Lock;

use crate::progress::{LOG_EVERY, Line, Meter, STALL};

/// The downloads in flight.
pub struct Downloads {
    origin: Instant,
    next: AtomicU64,
    // alloc: the process's one map of downloads in flight, a handful at a time.
    open: papaya::HashMap<u64, Arc<Entry>>,
    /// When any download last got a byte, in nanoseconds since `origin` plus one (0: never).
    last_byte: AtomicU64,
}

struct Entry {
    what: String,
    url: String,
    meter: Lock<Meter>,
}

/// One download in flight, from [`Downloads::begin`] until it drops.
pub struct Download<'d> {
    downloads: &'d Downloads,
    id: u64,
    entry: Arc<Entry>,
}

/// What feeds one download's bytes in from another task: a clone of its [`Download`]'s meter.
#[derive(Clone)]
pub struct Counter<'d> {
    downloads: &'d Downloads,
    entry: Arc<Entry>,
}

/// A download as it is now: what it is, where from (redacted), and its line.
#[derive(Clone, Debug)]
pub struct Shown {
    /// Its id in this process, never reused while the process lives ([`Downloads::begin`]
    /// counts them from 0): the explorer's task id of it.
    pub id: u64,
    pub what: String,
    pub url: String,
    pub line: Line,
}

impl Downloads {
    pub fn new() -> Downloads {
        Downloads {
            origin: Instant::now(),
            next: AtomicU64::new(0),
            open: papaya::HashMap::new(),
            last_byte: AtomicU64::new(0),
        }
    }

    /// This process's.
    pub fn process() -> &'static Downloads {
        #[expect(
            clippy::disallowed_types,
            reason = "the process's one registry of downloads in flight, read by the status line, `blaze status` and the stall bound"
        )]
        static PROCESS: std::sync::OnceLock<Downloads> = std::sync::OnceLock::new();
        PROCESS.get_or_init(Downloads::new)
    }

    fn now(&self) -> Duration {
        self.origin.elapsed()
    }

    /// A download of `what` (`LLVM 23.1.4 for aarch64-apple-darwin`, a file's name) from `url`,
    /// said redacted, `total` bytes when known.
    pub fn begin(&self, what: &str, url: &str, total: Option<u64>) -> Download<'_> {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let entry = Arc::new(Entry {
            what: what.to_owned(),
            url: crate::redact(url),
            meter: Lock::new("fetch.download.meter", Meter::new(self.now(), total, LOG_EVERY, STALL)),
        });
        self.open.pin().insert(id, Arc::clone(&entry));
        Download { downloads: self, id, entry }
    }

    /// Every download in flight, oldest first.
    pub fn shown(&self) -> Vec<Shown> {
        let now = self.now();
        let open = self.open.pin();
        let mut all: Vec<(u64, Shown)> = open
            .iter()
            .map(|(id, entry)| {
                let mut meter = entry.meter.lock();
                meter.advance_clock(now);
                let line = meter.line();
                (*id, Shown { id: *id, what: entry.what.clone(), url: entry.url.clone(), line })
            })
            .collect(); // alloc: a status's list, a handful
        all.sort_by_key(|(id, _)| *id);
        all.into_iter().map(|(_, it)| it).collect() // alloc: as above
    }

    /// How long since any download got a byte, while one is in flight: what the build's stall
    /// bound counts as progress. `None` when none is in flight.
    pub fn quiet_for(&self) -> Option<Duration> {
        if self.open.pin().is_empty() {
            return None;
        }
        let last = self.last_byte.load(Ordering::Relaxed);
        let now = self.now();
        Some(match last {
            0 => now,
            at => now.saturating_sub(Duration::from_nanos(at - 1)),
        })
    }
}

impl Default for Downloads {
    fn default() -> Downloads {
        Downloads::new()
    }
}

impl<'d> Download<'d> {
    /// The whole length, once a response says it.
    pub fn set_total(&self, total: Option<u64>) {
        self.entry.meter.lock().set_total(total);
    }

    /// `bytes` were there already, in a partial file this download resumes.
    pub fn resumed(&self, bytes: u64) {
        self.entry.meter.lock().resumed(bytes);
    }

    /// What another task feeds bytes in through.
    pub fn counter(&self) -> Counter<'d> {
        Counter { downloads: self.downloads, entry: Arc::clone(&self.entry) }
    }

    /// `bytes` more came now.
    pub fn got(&self, bytes: u64) {
        self.counter().got(bytes);
    }

    /// The line as of now.
    pub fn line(&self) -> Line {
        self.counter().line()
    }
}

impl Counter<'_> {
    /// `bytes` more came now.
    pub fn got(&self, bytes: u64) {
        let now = self.downloads.now();
        {
            let mut meter = self.entry.meter.lock();
            meter.advance_clock(now);
            meter.got(bytes);
        }
        let at = u64::try_from(now.as_nanos()).unwrap_or(u64::MAX - 1) + 1;
        self.downloads.last_byte.fetch_max(at, Ordering::Relaxed);
    }

    /// The line as of now.
    pub fn line(&self) -> Line {
        let mut meter = self.entry.meter.lock();
        meter.advance_clock(self.downloads.now());
        meter.line()
    }
}

impl Drop for Download<'_> {
    fn drop(&mut self) {
        self.downloads.open.pin().remove(&self.id);
    }
}
