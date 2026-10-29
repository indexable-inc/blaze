//! What a process of the daemon says of the build it does, as it does it: the records of
//! [`blaze_protocol::events`], in the file beside the process's socket that the daemon follows
//! ([`blaze_protocol::follow`]). A load, which comes before the process listens, is in it from
//! its first step. The engine says what only it knows (crates/blaze-engine/src/events.rs).

use std::{
    fs::{File, OpenOptions},
    path::Path,
    sync::{Mutex, atomic::Ordering},
    time::Duration,
};

use blaze_protocol::events::{Event, Record, Step};

/// The file this process says what it does in, once it has a socket.
static OUT: Mutex<Option<File>> = Mutex::new(None);

/// Has this process say what it does beside `socket` from now on.
pub fn said_beside(socket: &Path) {
    let path = blaze_protocol::follow::beside(socket, std::process::id());
    let mut options = OpenOptions::new();
    options.create(true).write(true).truncate(true);
    if let Ok(file) = crate::socket::private_file(&mut options, &path) {
        *crate::lock(&OUT) = Some(file);
    }
}

/// Whether this process says anything: what it would say need not be worked out otherwise.
pub fn heard() -> bool {
    crate::lock(&OUT).is_some()
}

/// Says `event`. A process with no socket says nothing.
pub fn say(event: Event) {
    let mut out = crate::lock(&OUT);
    let Some(file) = out.as_mut() else { return };
    let record = Record { at_ns: blaze_protocol::follow::now_ns(), event };
    // One write of the whole frame: the daemon reads the file as it grows.
    if let Ok(item) = blaze_protocol::cbor::to_vec(&record) {
        let _ = blaze_protocol::write_frame(file, &item);
    }
}

/// The step each thread of the process is at, which its next one ends. Of each thread: a
/// worker builds programs side by side, each on a thread of its own, and one program's step
/// does not end another's (a program's link went on until the next program began a step).
static AT: Mutex<Vec<(std::thread::ThreadId, Step, Option<String>)>> = Mutex::new(Vec::new());

/// Takes the step this thread is at, and has it at `next` from now on.
fn replace(next: Option<(Step, Option<String>)>) -> Option<(Step, Option<String>)> {
    let thread = std::thread::current().id();
    let mut at = crate::lock(&AT);
    let was = at.iter().position(|it| it.0 == thread).map(|it| at.swap_remove(it));
    if let Some((step, of)) = next {
        at.push((thread, step, of));
    }
    was.map(|(_, step, of)| (step, of))
}

/// The step this thread is at.
fn current() -> Option<(Step, Option<String>)> {
    let thread = std::thread::current().id();
    let at = crate::lock(&AT);
    at.iter().find(|it| it.0 == thread).map(|(_, step, of)| (*step, of.clone()))
}

/// Says this thread of the process is at `step`, of `of` when it is of one crate or program,
/// and done with the one it was at before.
pub fn at(step: Step, of: Option<&str>) {
    if crate::trace::on() {
        crate::trace::step(Some(match of {
            Some(of) => format!("{step:?} {of}"),
            None => format!("{step:?}"),
        }));
    }
    let of = of.map(str::to_owned);
    let waited = crate::lock(&AT).is_empty();
    match replace(Some((step, of.clone()))) {
        Some((step, of)) => say(Event::Ended { step, of }),
        // At a step from waiting: how busy the process is is said from now on.
        None if waited && heard() => ticker().unpark(),
        None => {}
    }
    say(Event::Started { step, of });
}

/// Does `work` as `step` of `of`, and is at the step it was at before again after.
pub fn within<T>(step: Step, of: Option<&str>, work: impl FnOnce() -> T) -> T {
    let before = current();
    at(step, of);
    let done = work();
    match before {
        Some((step, of)) => at(step, of.as_deref()),
        None => idle(),
    }
    done
}

/// Says this thread of the process is done with its step: once every thread is, the process
/// waits to be asked.
pub fn idle() {
    crate::trace::step(None);
    if let Some((step, of)) = replace(None) {
        say(Event::Ended { step, of });
    }
}

/// How often a compile says how many functions it has compiled.
pub const COUNTED: Duration = Duration::from_millis(100);

/// The first of `count` ids of nodes no other node of the process has: whatever says nodes
/// takes them here or from [`blaze_graph::exec::ids`], which this is, so the nodes of one
/// graph and another's are told apart.
pub fn node_ids(count: usize) -> u32 {
    blaze_graph::exec::ids(count)
}

/// A count of what a step does, said every [`COUNTED`] while it goes on, with what is taking
/// longest among what is under way: the files a check reads, the crates names are resolved
/// of. Each worker thread has a slot it says what it works on in.
pub struct Counting {
    shared: Option<std::sync::Arc<Counted>>,
}

struct Counted {
    step: Step,
    total: u64,
    done: std::sync::atomic::AtomicU64,
    /// What each thread works on, and since when.
    slots: Vec<Mutex<Option<(String, std::time::Instant)>>>,
    /// The count and what took longest, as said last.
    said: Mutex<Option<(u64, Option<String>)>>,
}

impl Counted {
    /// Says how far the count is if that is not what was said last; with `over`, that
    /// nothing is under way any more.
    fn tick(&self, over: bool) {
        let done = self.done.load(Ordering::Relaxed);
        let longest = match over {
            true => None,
            false => {
                let slots = self.slots.iter().filter_map(|it| crate::lock(it).clone());
                slots.min_by_key(|it| it.1).map(|it| it.0)
            }
        };
        let now = Some((done, longest));
        let mut said = crate::lock(&self.said);
        if *said != now {
            let longest = now.as_ref().and_then(|it| it.1.clone());
            say(Event::Progress { step: self.step, done, total: self.total, longest });
            *said = now;
        }
    }
}

/// How often the process says how busy it is while it is at a step ([`Event::Cores`]).
const CORES_EVERY: Duration = Duration::from_millis(250);

/// When the process's CPU time was read last, that time, and the hundredths of a core said
/// last; none while it waits, so that what it says is of the step alone.
static CPU: Mutex<Option<(std::time::Instant, Duration, Option<u32>)>> = Mutex::new(None);

/// The CPU time the process has had, with its children's: those it waited for, and on Linux
/// those still running and theirs (a build script, the standard library of build scripts),
/// so that a child exiting moves its time from one to the other.
fn cpu_time() -> Option<Duration> {
    #[cfg(unix)]
    {
        let used = |who| {
            let mut usage = std::mem::MaybeUninit::<libc::rusage>::zeroed();
            // SAFETY: getrusage fills in the struct it is handed, and says if it did not.
            if unsafe { libc::getrusage(who, usage.as_mut_ptr()) } != 0 {
                return None;
            }
            // SAFETY: it was filled in, and was all zeros before.
            let usage = unsafe { usage.assume_init() };
            let time = |it: libc::timeval| {
                let micros = Duration::from_micros(u64::try_from(it.tv_usec).unwrap_or(0));
                Duration::from_secs(u64::try_from(it.tv_sec).unwrap_or(0)) + micros
            };
            Some(time(usage.ru_utime) + time(usage.ru_stime))
        };
        let running = running_children_time().unwrap_or_default();
        Some(used(libc::RUSAGE_SELF)? + used(libc::RUSAGE_CHILDREN)? + running)
    }
    #[cfg(not(unix))]
    None
}

/// The CPU time of the process's children still running, and of every process under them,
/// from `/proc`. None where the system says nothing of it.
#[cfg(target_os = "linux")]
fn running_children_time() -> Option<Duration> {
    // SAFETY: sysconf reads a constant of the system.
    let ticks =
        u64::try_from(unsafe { libc::sysconf(libc::_SC_CLK_TCK) }).ok().filter(|it| *it > 0)?;
    let children_of = |pid: &str| -> Vec<String> {
        let Ok(tasks) = std::fs::read_dir(format!("/proc/{pid}/task")) else { return Vec::new() };
        let tasks = tasks
            .flatten()
            .filter_map(|task| std::fs::read_to_string(task.path().join("children")).ok());
        tasks.flat_map(|it| it.split_whitespace().map(str::to_owned).collect::<Vec<_>>()).collect()
    };
    let mut spent = 0;
    let mut under = children_of("self");
    while let Some(pid) = under.pop() {
        if let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) {
            // utime, stime, cutime and cstime: the 14th to the 17th fields, the 12th to the
            // 15th after the name in parentheses (proc_pid_stat(5)).
            let fields = stat.rsplit_once(')').map_or("", |it| it.1);
            let times = fields.split_whitespace().skip(11).take(4);
            spent += times.filter_map(|it| it.parse::<u64>().ok()).sum::<u64>();
        }
        under.extend(children_of(&pid));
    }
    Some(
        Duration::from_secs(spent / ticks)
            + Duration::from_nanos(spent % ticks * 1_000_000_000 / ticks),
    )
}

#[cfg(all(unix, not(target_os = "linux")))]
fn running_children_time() -> Option<Duration> {
    None
}

/// Says how busy the process was since it last read its CPU time, once [`CORES_EVERY`] went
/// by and when that moved by a tenth of a core: the first read only starts the count.
fn cores() {
    static OF: std::sync::OnceLock<u32> = std::sync::OnceLock::new();
    let Some(cpu) = cpu_time() else { return };
    let now = std::time::Instant::now();
    let mut last = crate::lock(&CPU);
    let said = match *last {
        Some((at, _, _)) if now - at < CORES_EVERY => return,
        Some((at, before, said)) => {
            let busy = cpu.saturating_sub(before).as_secs_f64() / (now - at).as_secs_f64();
            let busy = (busy * 100.0).round() as u32;
            if said.is_none_or(|it| it / 10 != busy / 10) {
                let of = *OF.get_or_init(|| {
                    std::thread::available_parallelism().map_or(1, |it| it.get() as u32)
                });
                say(Event::Cores { busy, of });
                Some(busy)
            } else {
                said
            }
        }
        None => None,
    };
    *last = Some((now, cpu, said));
}

/// The counts going on, which one thread of the process says every [`COUNTED`]: a count
/// that ends in less starts no thread of its own. The same thread says how busy the process
/// is while it is at a step ([`cores`]).
static COUNTS: Mutex<Vec<std::sync::Weak<Counted>>> = Mutex::new(Vec::new());

/// The thread that says the [`COUNTS`] and how busy the process is, which waits while there
/// are none and the process is at no step.
fn ticker() -> &'static std::thread::Thread {
    static TICKER: std::sync::OnceLock<std::thread::Thread> = std::sync::OnceLock::new();
    TICKER.get_or_init(|| {
        let spawned = std::thread::Builder::new().name("counts".into()).spawn(|| {
            loop {
                let any = {
                    let mut counts = crate::lock(&COUNTS);
                    counts.retain(|it| it.strong_count() > 0);
                    // Said under the lock: a count dropped meanwhile says its last after.
                    counts
                        .iter()
                        .filter_map(std::sync::Weak::upgrade)
                        .for_each(|it| it.tick(false));
                    !counts.is_empty()
                };
                let stepping = !crate::lock(&AT).is_empty();
                if stepping {
                    cores();
                }
                match any || stepping {
                    true => std::thread::park_timeout(COUNTED),
                    false => {
                        *crate::lock(&CPU) = None;
                        std::thread::park();
                    }
                }
            }
        });
        match spawned {
            Ok(handle) => handle.thread().clone(),
            Err(_) => std::thread::current(),
        }
    })
}

impl Counting {
    /// Counts nothing.
    pub fn none() -> Counting {
        Counting { shared: None }
    }

    /// `slots` threads count toward `total` of `step` from now until this is dropped.
    pub fn new(step: Step, total: usize, slots: usize) -> Counting {
        if !heard() {
            return Counting::none();
        }
        let shared = std::sync::Arc::new(Counted {
            step,
            total: total as u64,
            done: Default::default(),
            slots: (0..slots).map(|_| Mutex::new(None)).collect(),
            said: Mutex::new(None),
        });
        let first = {
            let mut counts = crate::lock(&COUNTS);
            counts.push(std::sync::Arc::downgrade(&shared));
            counts.len() == 1
        };
        let ticker = ticker();
        if first {
            ticker.unpark();
        }
        Counting { shared: Some(shared) }
    }

    /// Thread `slot` works on `what` now; `what` is made only when anyone hears.
    pub fn on(&self, slot: usize, what: impl FnOnce() -> String) {
        if let Some(it) = self.shared.as_ref().and_then(|it| it.slots.get(slot)) {
            *crate::lock(it) = Some((what(), std::time::Instant::now()));
        }
    }

    /// Thread `slot` finished one.
    pub fn done(&self, slot: usize) {
        if let Some(shared) = &self.shared {
            shared.done.fetch_add(1, Ordering::Relaxed);
            if let Some(it) = shared.slots.get(slot) {
                *crate::lock(it) = None;
            }
        }
    }
}

impl Drop for Counting {
    fn drop(&mut self) {
        if let Some(shared) = self.shared.take() {
            let this = std::sync::Arc::as_ptr(&shared);
            crate::lock(&COUNTS).retain(|it| it.as_ptr() != this);
            shared.tick(true);
        }
    }
}
