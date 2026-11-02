//! Other programs' load on the machine while this process measures,
//! recorded by every measurement in this crate, with nothing to call.
//!
//! **Where.** Linux alone ([`MEASURED`]). On macOS the machine's busy
//! ticks less this process's own CPU time read as other programs' load
//! exactly where this process's own threads burst or wake (every busy
//! window of Mac jobs 1672-1676, BLAKE3 #142), so a busy verdict there
//! said nothing about other programs, and the macOS reading is gone:
//! its windows are none, and its runs say "not measured" beside their
//! results. Keep a Mac quiet by hand while it measures.
//!
//! **What it counts.** The OS counts the CPU time every CPU spent busy;
//! this process counts its own (every thread). Their difference over a
//! stretch of wall time is how many CPUs other programs kept busy, in
//! milli-CPUs (1000: one CPU busy throughout). On Linux the OS also
//! counts steal time, which a hypervisor took from a VM's CPUs for other
//! work on the host, load a guest sees no other way.
//!
//! **When it reads.** A reading costs system calls, which take far longer
//! than a short timed call, so it never happens inside a timed interval
//! or per call. Each measurement in this crate calls [`tick`] outside its
//! timed intervals, between batches and before each gap; a tick reads
//! the machine only once a window of [`WINDOW_NS`] has passed since the
//! last reading, and otherwise costs one wall-clock read and one atomic
//! load. Measured in the Linux VM (`reading_cost` below, September 30,
//! 2026): a reading 8.6 us, a tick between readings 18 ns. A program
//! that times with [`crate::now`] directly calls [`tick`] itself, between
//! samples.
//!
//! **Placing samples in windows.** Every [`crate::Batch`] records when it
//! started, as [`now_ns`] (nanoseconds since this process's first
//! measurement), on the same scale as each [`Window`]'s bounds.
//!
//! **Resolution.** Linux counts machine CPU time in ticks of 10 ms per
//! CPU. A window's reading therefore carries an error of up to a tick per
//! CPU: at most 0.16 CPUs over a 1 s window on 16 CPUs, well under
//! [`BUSY_MILLI_CPUS`].
//!
//! A window whose other load or steal reaches [`BUSY_MILLI_CPUS`] is
//! busy: when one closes, this module says so on stderr, so every probe
//! and benchmark shows it with no code of its own; [`windows`] gives
//! every window for reports and samples files.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

/// Whether this platform's load is measured: Linux alone (the module's
/// doc, "Where"). Elsewhere [`windows`] is always empty.
pub const MEASURED: bool = cfg!(target_os = "linux");

/// A window's least length: a reading ends it once this much wall time
/// has passed since the one before.
pub const WINDOW_NS: u64 = 1_000_000_000;
/// Other programs (or a hypervisor) keeping a whole CPU busy make a
/// window busy: a core taken from the shared scenario and from the
/// multithreaded contenders, which use every CPU. An M4 Max running a
/// Linux VM beside the benchmark keeps 0.40-0.56 CPUs busy, with results
/// level with a quieter run's.
pub const BUSY_MILLI_CPUS: u64 = 1000;

/// One stretch of wall time and the load other programs put on the
/// machine in it. Bounds are [`now_ns`] values.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Window {
    pub start_ns: u64,
    pub end_ns: u64,
    /// CPUs other programs kept busy, in milli-CPUs.
    pub other_milli_cpus: u64,
    /// CPUs a hypervisor withheld, in milli-CPUs (0 where none is counted).
    pub steal_milli_cpus: u64,
}

impl Window {
    /// Whether other programs or a hypervisor took a CPU or more.
    pub fn busy(&self) -> bool {
        self.other_milli_cpus.max(self.steal_milli_cpus) >= BUSY_MILLI_CPUS
    }

    /// Whether the window holds the instant `at_ns` (a [`now_ns`] value).
    pub fn holds(&self, at_ns: u64) -> bool {
        self.start_ns <= at_ns && at_ns < self.end_ns
    }
}

/// Nanoseconds of wall time since this process's first measurement (its
/// first call of this function, [`tick`], or a measurement in this crate).
pub fn now_ns() -> u64 {
    static ORIGIN: OnceLock<Instant> = OnceLock::new();
    crate::since_ns(*ORIGIN.get_or_init(Instant::now))
}

/// A machine reading: when, the machine's CPU times, this process's.
#[derive(Clone, Copy)]
struct Reading {
    at_ns: u64,
    machine: imp::CpuTimes,
    own_ns: u64,
}

impl Reading {
    fn take() -> Option<Reading> {
        let at_ns = now_ns();
        let machine = imp::read()?;
        Some(Reading { at_ns, machine, own_ns: crate::process_cpu_ns() })
    }
}

/// The window between two readings.
fn between(from: Reading, to: Reading) -> Window {
    assert!(to.at_ns > from.at_ns, "a window lasts some time");
    let (busy_ns, steal_ns) = imp::busy_and_steal_ns(from.machine, to.machine);
    let milli = |ns: u64| u64::try_from(u128::from(ns) * 1000 / u128::from(to.at_ns - from.at_ns)).expect("load fits in u64");
    // Tick rounding can put the machine's busy time a little under this
    // process's own; that reads as none.
    Window {
        start_ns: from.at_ns,
        end_ns: to.at_ns,
        other_milli_cpus: milli(busy_ns.saturating_sub(to.own_ns - from.own_ns)),
        steal_milli_cpus: milli(steal_ns),
    }
}

struct State {
    last: Option<Reading>,
    /// The reading the last closed window started at, until a tail joins it.
    last_start: Option<Reading>,
    windows: Vec<Window>,
}

static STATE: Mutex<State> = Mutex::new(State { last: None, last_start: None, windows: Vec::new() });
/// The [`now_ns`] at which the next tick reads the machine; u64::MAX where
/// the platform gives no CPU times.
static DUE_NS: AtomicU64 = AtomicU64::new(0);

/// Close the running window if `least_ns` have passed since the last
/// reading (take the first reading when there is none).
fn close(state: &mut State, least_ns: u64) {
    let Some(reading) = Reading::take() else {
        DUE_NS.store(u64::MAX, Ordering::Relaxed);
        return;
    };
    match state.last {
        Some(last) if reading.at_ns - last.at_ns < least_ns => return,
        Some(last) => {
            push(state, between(last, reading));
            state.last_start = Some(last);
        }
        None => {}
    }
    state.last = Some(reading);
    DUE_NS.store(reading.at_ns + WINDOW_NS, Ordering::Relaxed);
}

/// Add a window, reporting it on stderr when busy.
fn push(state: &mut State, window: Window) {
    if window.busy() {
        eprintln!("clocks: {}; measurements in that time may read slower than this machine runs", describe_window(&window));
    }
    state.windows.push(window);
}

/// Join the final tail using the last window's original counter baseline.
fn join_tail(state: &mut State, start: Reading, reading: Reading) {
    if state.windows.last().expect("a tail joins a closed window").busy() {
        // Keep this observation: a quiet tail could average it below the
        // busy threshold. The run already has no speed verdict.
        return;
    }
    state.windows.pop();
    push(state, between(start, reading));
    state.last = Some(reading);
}

/// How long until the next tick reads the machine, in ns (0: now; u64::MAX
/// where the platform gives no CPU times).
pub fn until_due_ns() -> u64 {
    DUE_NS.load(Ordering::Relaxed).saturating_sub(now_ns())
}

/// Read the machine once a window has passed since the last reading;
/// otherwise nothing. Call it outside timed intervals: every measurement
/// in this crate does. Another thread's tick in progress makes this one
/// return at once.
pub fn tick() {
    if now_ns() < DUE_NS.load(Ordering::Relaxed) {
        return;
    }
    if let Ok(mut state) = STATE.try_lock() {
        close(&mut state, WINDOW_NS);
    }
}

/// Every window so far, in order, the running one closed first when it
/// has lasted half a window or more; a shorter one joins the last closed
/// quiet window, read again from that window's start, so the run's last samples
/// lie in a window too (once: a tail joins a window that no tail has
/// joined, so repeated calls never dilute a burst into a longer average).
/// An observed busy window stays intact; the run already has no speed verdict.
/// Empty where the platform gives no CPU times, or before a window has
/// closed (a run under half a second).
pub fn windows() -> Vec<Window> {
    let mut state = STATE.lock().expect("load state lock");
    if DUE_NS.load(Ordering::Relaxed) != u64::MAX {
        let closed = state.windows.len();
        close(&mut state, WINDOW_NS / 2);
        if state.windows.len() == closed {
            if let (Some(start), Some(reading)) = (state.last_start.take(), Reading::take()) {
                join_tail(&mut state, start, reading);
            }
        }
    }
    state.windows.clone()
}

/// Other programs' load over the next `ns` of wall time, in milli-CPUs,
/// from two readings of its own (the windows' rule, apart from their
/// state): the tests' measure of the load beside them. 0 where the
/// platform gives no CPU times.
#[cfg(all(test, unix))]
pub(crate) fn other_milli_cpus_over(ns: u64) -> u64 {
    let Some(before) = Reading::take() else { return 0 };
    std::thread::sleep(std::time::Duration::from_nanos(ns));
    let after = Reading::take().expect("CPU times read a moment ago");
    between(before, after).other_milli_cpus
}

/// The tests that use this process's windows (they tick, read, or run a
/// child, whose watcher ticks) take turns: the tests run on many threads,
/// and one's windows are another's.
#[cfg(all(test, unix))]
pub(crate) fn exclusive() -> std::sync::MutexGuard<'static, ()> {
    static TURN: Mutex<()> = Mutex::new(());
    TURN.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Milli-CPUs as "1.23".
pub fn cpus(milli: u64) -> String {
    let hundredths = (milli + 5) / 10;
    format!("{}.{:02}", hundredths / 100, hundredths % 100)
}

/// Nanoseconds as seconds with one decimal: "12.3".
fn seconds(ns: u64) -> String {
    let tenths = (ns + 50_000_000) / 100_000_000;
    format!("{}.{}", tenths / 10, tenths % 10)
}

/// "other programs kept 1.40 CPUs busy from 12.0 s to 13.0 s" (with the
/// hypervisor's share where it took any).
pub fn describe_window(window: &Window) -> String {
    let mut line =
        format!("other programs kept {} CPUs busy from {} s to {} s", cpus(window.other_milli_cpus), seconds(window.start_ns), seconds(window.end_ns));
    if window.steal_milli_cpus > 0 {
        line += &format!(", the hypervisor withheld {} CPUs", cpus(window.steal_milli_cpus));
    }
    line
}

/// One line for readers about `windows`: "quiet: other programs kept 0.02
/// CPUs busy on average, 0.10 in the busiest second", or "busy: ..., busy
/// in 2 of 40 windows (12.0-13.0 s, 20.1-21.1 s)"; for none, "not
/// measured on this platform", or where the OS counts CPU time "not
/// measured: the run lasted under half a second" (a window closes after
/// half a second at the least).
pub fn describe(windows: &[Window]) -> String {
    let Some(first) = windows.first() else {
        return if MEASURED {
            "not measured: the run lasted under half a second"
        } else if cfg!(target_vendor = "apple") {
            "not measured on macOS (its accounting books this process's own threads as other programs')"
        } else {
            "not measured on this platform"
        }
        .to_owned();
    };
    let span = windows.last().unwrap().end_ns - first.start_ns;
    let weighted = |pick: fn(&Window) -> u64| {
        let sum: u128 = windows.iter().map(|w| u128::from(pick(w)) * u128::from(w.end_ns - w.start_ns)).sum();
        u64::try_from((sum + u128::from(span) / 2) / u128::from(span)).expect("load fits in u64")
    };
    let busy: Vec<&Window> = windows.iter().filter(|w| w.busy()).collect();
    let mut line = format!(
        "{}: other programs kept {} CPUs busy on average, {} in the busiest window",
        if busy.is_empty() { "quiet" } else { "busy" },
        cpus(weighted(|w| w.other_milli_cpus)),
        cpus(windows.iter().map(|w| w.other_milli_cpus).max().unwrap())
    );
    let steal = windows.iter().map(|w| w.steal_milli_cpus).max().unwrap();
    if steal > 0 {
        line += &format!("; the hypervisor withheld {} CPUs on average, {} at most", cpus(weighted(|w| w.steal_milli_cpus)), cpus(steal));
    }
    if !busy.is_empty() {
        let list: Vec<String> = busy.iter().map(|w| format!("{}-{} s", seconds(w.start_ns), seconds(w.end_ns))).collect();
        line += &format!("; busy in {} of {} windows ({})", busy.len(), windows.len(), list.join(", "));
    }
    line
}

#[cfg(target_os = "linux")]
mod imp {
    /// Machine CPU times since boot in ticks, all CPUs summed: busy (not
    /// idle, not waiting on I/O) and stolen by a hypervisor.
    #[derive(Clone, Copy)]
    pub struct CpuTimes {
        pub(super) busy: u64,
        pub(super) steal: u64,
    }

    fn ticks_per_second() -> u64 {
        unsafe extern "C" {
            fn sysconf(name: i32) -> i64;
        }
        const SC_CLK_TCK: i32 = 2;
        // Sound: a plain query.
        u64::try_from(unsafe { sysconf(SC_CLK_TCK) }).expect("sysconf(_SC_CLK_TCK) is positive")
    }

    pub fn read() -> Option<CpuTimes> {
        let stat = std::fs::read_to_string("/proc/stat").ok()?;
        let fields: Vec<u64> =
            stat.lines().next()?.strip_prefix("cpu ")?.split_whitespace().map(|field| field.parse().expect("/proc/stat counts are integers")).collect();
        // user nice system idle iowait irq softirq steal (guest time is inside user).
        assert!(fields.len() >= 8, "/proc/stat's cpu line has at least eight fields");
        Some(CpuTimes { busy: fields[0] + fields[1] + fields[2] + fields[5] + fields[6], steal: fields[7] })
    }

    pub fn busy_and_steal_ns(from: CpuTimes, to: CpuTimes) -> (u64, u64) {
        let ns = |ticks: u64| ticks * 1_000_000_000 / ticks_per_second();
        (ns(to.busy - from.busy), ns(to.steal - from.steal))
    }
}

#[cfg(not(target_os = "linux"))]
mod imp {
    #[derive(Clone, Copy)]
    pub struct CpuTimes;

    pub fn read() -> Option<CpuTimes> {
        None
    }

    pub fn busy_and_steal_ns(_: CpuTimes, _: CpuTimes) -> (u64, u64) {
        (0, 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn window(start_ns: u64, end_ns: u64, other_milli_cpus: u64, steal_milli_cpus: u64) -> Window {
        Window { start_ns, end_ns, other_milli_cpus, steal_milli_cpus }
    }

    #[test]
    fn busy_starts_at_one_cpu_of_other_load_or_steal() {
        assert!(!window(0, 1, 999, 999).busy());
        assert!(window(0, 1, 1000, 0).busy());
        assert!(window(0, 1, 0, 1000).busy());
    }

    #[test]
    fn a_window_holds_its_start_and_not_its_end() {
        let w = window(10, 20, 0, 0);
        assert!(w.holds(10) && w.holds(19) && !w.holds(20) && !w.holds(9));
    }

    #[test]
    fn descriptions_weigh_windows_by_length_and_name_busy_ones() {
        let none = if MEASURED {
            "not measured: the run lasted under half a second"
        } else if cfg!(target_vendor = "apple") {
            "not measured on macOS (its accounting books this process's own threads as other programs')"
        } else {
            "not measured on this platform"
        };
        assert_eq!(describe(&[]), none);
        let quiet = [window(0, 1_000_000_000, 100, 0), window(1_000_000_000, 4_000_000_000, 500, 0)];
        assert_eq!(describe(&quiet), "quiet: other programs kept 0.40 CPUs busy on average, 0.50 in the busiest window");
        let busy = [window(0, 1_000_000_000, 1400, 0), window(1_000_000_000, 2_000_000_000, 200, 300)];
        assert_eq!(
            describe(&busy),
            "busy: other programs kept 0.80 CPUs busy on average, 1.40 in the busiest window; \
            the hypervisor withheld 0.15 CPUs on average, 0.30 at most; busy in 1 of 2 windows (0.0-1.0 s)"
        );
    }

    #[cfg(target_os = "linux")]
    fn tail_fixture(busy: u64, steal: u64, tail_busy: u64, tail_steal: u64) -> State {
        let start = Reading { at_ns: 0, machine: imp::CpuTimes { busy: 0, steal: 0 }, own_ns: 0 };
        let last = Reading { at_ns: WINDOW_NS, machine: imp::CpuTimes { busy, steal }, own_ns: 0 };
        let mut state = State { last: Some(last), last_start: None, windows: vec![between(start, last)] };
        let tail = Reading { at_ns: WINDOW_NS + 400_000_000, machine: imp::CpuTimes { busy: tail_busy, steal: tail_steal }, own_ns: 0 };
        join_tail(&mut state, start, tail);
        state
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn quiet_tail_preserves_observed_other_load() {
        let state = tail_fixture(110, 0, 110, 0); // 1.10 CPUs, then no other work.
        assert!(state.windows.iter().any(Window::busy), "a quiet tail preserves the prior busy finding");
        assert!(describe(&state.windows).starts_with("busy:"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn quiet_tail_preserves_observed_steal() {
        let state = tail_fixture(0, 110, 0, 110);
        assert!(state.windows.iter().any(Window::busy), "a quiet tail preserves the prior steal finding");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn quiet_tail_still_extends_a_quiet_window() {
        let state = tail_fixture(20, 0, 28, 0);
        assert_eq!(state.windows, vec![window(0, 1_400_000_000, 200, 0)]);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn newly_busy_tail_still_marks_the_run_busy() {
        let state = tail_fixture(20, 0, 160, 0);
        assert!(state.windows[0].busy());
        assert_eq!(state.windows[0].end_ns, 1_400_000_000);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn ticks_close_a_window_after_a_window_of_time() {
        let _turn = exclusive();
        tick();
        let started = now_ns();
        while now_ns() - started < WINDOW_NS + WINDOW_NS / 10 {
            crate::busy_work(10_000_000);
            tick();
        }
        let ended = now_ns();
        let windows = windows();
        assert!(!windows.is_empty(), "a window closed");
        for pair in windows.windows(2) {
            assert_eq!(pair[0].end_ns, pair[1].start_ns, "windows follow one another");
        }
        assert!(windows.iter().all(|w| w.end_ns - w.start_ns >= WINDOW_NS / 2), "each window lasts half a window or more");
        // The tail under half a window joined the last window: it reaches the call.
        let last = *windows.last().unwrap();
        assert!(last.busy() || last.end_ns >= ended, "the tail is covered or the last busy finding is preserved");
        // Once: a later call joins nothing more to it.
        let tail = now_ns();
        while now_ns() - tail < WINDOW_NS / 10 {
            crate::busy_work(10_000_000);
        }
        assert_eq!(super::windows().last().copied(), Some(last), "a second call joins nothing more");
    }
}

/// What a machine reading costs (`cargo test --release -- --ignored
/// --nocapture reading_cost`): measured through this crate, printed.
#[cfg(test)]
#[test]
#[ignore]
fn reading_cost() {
    for batch in crate::measure(9, 20_000_000, || {
        std::hint::black_box(Reading::take());
    }) {
        println!("a machine reading: {}", batch.show());
    }
    for batch in crate::measure(9, 20_000_000, tick) {
        println!("a tick between readings: {}", batch.show());
    }
}
