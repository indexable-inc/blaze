//! How many cores blaze's work runs on, and at what priority: the one home of both
//! (docs/cargo.md, "Cores and priority").
//!
//! A build sized to every core, at the priority of the terminal that started it, takes the
//! machine from whoever sits at it: on 2026-11-03 a `blaze build --release` of blaze-engine ran
//! 22 minutes at ~1,500% CPU on an 18-core Mac, load 154, and the GUI froze. So by default
//! blaze leaves [`RESERVED`] cores free ([`Cores::budget`]) and lowers its own priority
//! ([`lower`]); `BLAZE_CORES` ([`VAR`]) says otherwise:
//!
//! - unset (or empty): [`default_budget`], at background priority;
//! - `all`: every core, at normal priority (CI and ix VMs, where no one sits at the machine);
//! - a whole number `n` of 1 or more: exactly `n`, at background priority.
//!
//! Every site that sizes job-level parallelism reads [`get`]: the daemon's plane, the process's
//! worker pool (`blaze_graph::exec::shared`), test runs, the `NUM_JOBS` a build script is told.
//! Where a child starts parallel work of its own (a build script's `cc`, a test binary's
//! libtest threads) it is given a share of the budget ([`Cores::split`]), so the product stays
//! near the budget rather than its square.

use std::{fmt, num::NonZero};

/// The variable that sets the budget and the priority.
pub const VAR: &str = "BLAZE_CORES";

/// The cores the default budget leaves to the rest of the machine (the GUI, the terminal, the
/// editor): two, enough for a window server and one app to stay responsive under a full build.
pub const RESERVED: usize = 2;

/// How many ways the budget is split where each child runs parallel work of its own and
/// blaze does not know how many such children run at once: a build script's `cc` (`NUM_JOBS`,
/// with no shared jobserver), the test binaries `blaze test`'s live view runs at once. Four:
/// the -sys build scripts of a workspace mostly run in a burst of a handful, and four children
/// at a quarter each fill the budget; a lone one on the critical path still gets a quarter.
pub const NESTED: usize = 4;

/// What `BLAZE_CORES` says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Setting {
    /// Unset or empty: [`default_budget`], background priority.
    Default,
    /// `all`: every core, normal priority.
    All,
    /// `n`: exactly that many, background priority.
    Exactly(NonZero<usize>),
}

/// The process's budget: how many cores its work runs on, and whether it runs at background
/// priority.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cores {
    pub setting: Setting,
    /// The cores this machine lets the process use (`std::thread::available_parallelism`).
    pub machine: NonZero<usize>,
    /// How many cores job-level parallelism is sized to.
    pub budget: NonZero<usize>,
}

impl Cores {
    /// The budget `setting` gives on a machine of `machine` cores.
    pub fn resolve(setting: Setting, machine: NonZero<usize>) -> Cores {
        let budget = match setting {
            Setting::Default => default_budget(machine),
            Setting::All => machine,
            Setting::Exactly(n) => n,
        };
        Cores { setting, machine, budget }
    }

    /// Whether the process runs at background priority: every setting but `all`.
    pub fn lowered(&self) -> bool {
        !matches!(self.setting, Setting::All)
    }

    /// One of `parts` equal shares of the budget, at least 1: what each of `parts` children
    /// that run at once gets for parallel work of its own.
    pub fn split(&self, parts: usize) -> NonZero<usize> {
        split(self.budget, parts)
    }

    /// Whether a process with `other` runs work the same way: the same budget at the same
    /// priority (`BLAZE_CORES` unset and `BLAZE_CORES=<the default budget>` agree).
    pub fn agrees(&self, other: &Cores) -> bool {
        self.budget == other.budget && self.lowered() == other.lowered()
    }
}

impl fmt::Display for Cores {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.setting {
            Setting::Default => {
                write!(f, "{VAR} unset: {} of {} cores", self.budget, self.machine)?
            }
            Setting::All => write!(f, "{VAR}=all: all {} cores", self.machine)?,
            Setting::Exactly(n) => {
                write!(f, "{VAR}={n}: {} of {} cores", self.budget, self.machine)?
            }
        }
        match self.lowered() {
            true => f.write_str(" at background priority"),
            false => f.write_str(" at normal priority"),
        }
    }
}

/// The default budget on a machine of `machine` cores: [`RESERVED`] fewer, at least 1.
pub fn default_budget(machine: NonZero<usize>) -> NonZero<usize> {
    NonZero::new(machine.get().saturating_sub(RESERVED)).unwrap_or(NonZero::<usize>::MIN)
}

/// One of `parts` equal shares of `budget`, rounded down, at least 1 (`parts` of 0 is 1).
pub fn split(budget: NonZero<usize>, parts: usize) -> NonZero<usize> {
    NonZero::new(budget.get() / parts.max(1)).unwrap_or(NonZero::<usize>::MIN)
}

/// What `BLAZE_CORES` says, given its value (`None` when unset).
///
/// # Errors
///
/// A value that is not UTF-8, `all`, or a whole number of 1 or more, naming the variable, the
/// value and what it takes.
pub fn parse(value: Option<&std::ffi::OsStr>) -> Result<Setting, CoresError> {
    let Some(value) = value else { return Ok(Setting::Default) };
    let Some(text) = value.to_str() else {
        // alloc: error path, once
        return Err(CoresError::Invalid { value: value.to_string_lossy().into_owned() });
    };
    match text {
        "" => Ok(Setting::Default),
        "all" => Ok(Setting::All),
        _ => match text.parse::<usize>().ok().and_then(NonZero::new) {
            Some(n) if text.bytes().all(|it| it.is_ascii_digit()) => Ok(Setting::Exactly(n)),
            // alloc: error path, once
            _ => Err(CoresError::Invalid { value: text.to_owned() }),
        },
    }
}

/// Why the budget can't be made. A command whose budget disagrees with the running daemon's
/// is refused by the CLI with every other setting it differs in (blaze-cargo's
/// `serve::settings`, by [`Cores::agrees`]), not here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CoresError {
    /// `BLAZE_CORES` holds `value`, which is not what it takes.
    Invalid { value: String },
    /// The machine's core count can't be read.
    Machine { why: String },
}

impl fmt::Display for CoresError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CoresError::Invalid { value } => write!(
                f,
                "{VAR}={value:?} is not a setting of blaze's cores: it takes `all` (every core, normal \
                 priority), a whole number of 1 or more (that many cores, background priority), or \
                 nothing (unset: {RESERVED} fewer than the machine's, at least 1, background priority)"
            ),
            CoresError::Machine { why } => write!(
                f,
                "the number of cores this machine lets blaze use (std::thread::available_parallelism), \
                 which {VAR}'s budget is made from, can't be read: {why}"
            ),
        }
    }
}

impl std::error::Error for CoresError {}

/// The budget `value` (`BLAZE_CORES`'s, `None` unset) gives on a machine of `machine` cores.
///
/// # Errors
///
/// As [`parse`].
pub fn of(value: Option<&std::ffi::OsStr>, machine: NonZero<usize>) -> Result<Cores, CoresError> {
    parse(value).map(|setting| Cores::resolve(setting, machine))
}

/// The cores this machine lets the process use.
///
/// # Errors
///
/// When the OS can't say, naming the call.
pub fn machine() -> Result<NonZero<usize>, CoresError> {
    // A target without threads (the wasm64 seed) runs on its one thread, as its worker pool
    // does (`blaze_graph::topology`): std has no count to give there.
    if cfg!(all(target_family = "wasm", not(target_feature = "atomics"))) {
        return Ok(NonZero::<usize>::MIN);
    }
    // alloc: error path, once
    std::thread::available_parallelism().map_err(|why| CoresError::Machine { why: why.to_string() })
}

/// This process's budget, from its own `BLAZE_CORES` and the machine, read once: every site
/// that sizes parallelism asks this, so one process never runs two budgets.
///
/// # Errors
///
/// As [`of`] and [`machine`]; the same error on every call.
pub fn get() -> Result<Cores, CoresError> {
    #[expect(
        clippy::disallowed_types,
        reason = "the process's one budget: its priority is the process's, and the process-wide worker pool is \
                  sized by it before anything could pass it there"
    )]
    static CORES: std::sync::OnceLock<Result<Cores, CoresError>> = std::sync::OnceLock::new();
    CORES
        .get_or_init(|| machine().and_then(|machine| of(std::env::var_os(VAR).as_deref(), machine)))
        .clone() // clone: an error's text at most, once per site
}

/// Why the process's priority was not lowered: the call, and what the OS said.
#[derive(Debug)]
pub struct PriorityError {
    pub call: &'static str,
    pub why: std::io::Error,
}

impl fmt::Display for PriorityError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "blaze could not lower its priority to background ({}): {}; it runs at the priority it was started \
             with (set {VAR}=all to say that is meant)",
            self.call, self.why
        )
    }
}

impl std::error::Error for PriorityError {}

/// The nice value a lowered process runs at.
pub const NICE: i32 = 10;

/// Lowers this process to background priority when `cores` says so ([`Cores::lowered`]), so
/// every thread it starts and every process it spawns (linkers, build scripts, tests, the
/// daemon a CLI starts) inherits it. Call it on the main thread before any other thread
/// starts: on Linux a nice value is a thread's, inherited by the threads and processes it
/// starts.
///
/// The mechanism, on Linux and macOS alike, is `setpriority(PRIO_PROCESS, 0, 10)`: nice 10,
/// which a child inherits across fork and exec. On macOS `setpriority(PRIO_DARWIN_PROCESS, 0,
/// PRIO_DARWIN_BG)` is not used: getpriority(2) says it sets the scheduling priority "to the
/// lowest value", throttles disk IO as `setiopolicy_np` does and throttles network IO, and on
/// Apple silicon background threads run on the efficiency cores alone. A build reads and
/// writes its store all the time and fetches packages, so that would make it several times
/// slower, not just polite. `QOS_CLASS_UTILITY` is not used either: blaze's worker pool
/// measured utility workers starved to ~2% of a core each on a loaded host (2026-11-02,
/// `blaze_graph::pool`), so its workers instead drop from `user-initiated` to `default` when
/// the process is lowered, below every GUI's interactive work.
///
/// Idempotent: a process already at nice 10 or lower priority (a daemon started by a lowered
/// CLI inherits it) is left as it is; it is never raised, which an unprivileged process can't.
///
/// # Errors
///
/// The call that failed and the errno. The caller logs it as a warning and goes on: this is the
/// one refusal that would be worse than continuing. A build at normal priority is what blaze
/// did before; refusing to build because politeness failed (a sandbox that forbids
/// `setpriority`, a platform without it) would stop work no one asked to stop, and the warning
/// names what happened and how to say it is meant.
pub fn lower(cores: &Cores) -> Result<(), PriorityError> {
    if !cores.lowered() {
        return Ok(());
    }
    nice_at_least(NICE)
}

#[cfg(unix)]
fn nice_at_least(nice: i32) -> Result<(), PriorityError> {
    // getpriority returns -1 both for nice -1 and on error; either way the value is below
    // `nice`, so the set below decides, and its own error is the one reported.
    // SAFETY: one system call on the calling process, no memory passed.
    let now = unsafe { libc::getpriority(libc::PRIO_PROCESS, 0) };
    if now >= nice {
        return Ok(());
    }
    // SAFETY: one system call on the calling process, no memory passed.
    match unsafe { libc::setpriority(libc::PRIO_PROCESS, 0, nice) } {
        0 => Ok(()),
        _ => Err(PriorityError {
            call: "setpriority(PRIO_PROCESS, 0, 10)",
            why: std::io::Error::last_os_error(),
        }),
    }
}

#[cfg(not(unix))]
fn nice_at_least(_nice: i32) -> Result<(), PriorityError> {
    Err(PriorityError {
        call: "lowering the process's priority",
        // alloc: error path, once
        why: std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            format!("not done on {}", std::env::consts::OS),
        ),
    })
}

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;

    use super::*;

    fn n(it: usize) -> NonZero<usize> {
        NonZero::new(it).unwrap()
    }

    #[test]
    fn parses_what_blaze_cores_takes() {
        assert_eq!(parse(None), Ok(Setting::Default));
        assert_eq!(parse(Some(OsStr::new(""))), Ok(Setting::Default));
        assert_eq!(parse(Some(OsStr::new("all"))), Ok(Setting::All));
        assert_eq!(parse(Some(OsStr::new("1"))), Ok(Setting::Exactly(n(1))));
        assert_eq!(parse(Some(OsStr::new("64"))), Ok(Setting::Exactly(n(64))));
        for bad in
            ["0", "-1", "+4", " 4", "4 ", "ALL", "All", "half", "1.5", "99999999999999999999999"]
        {
            assert_eq!(
                parse(Some(OsStr::new(bad))),
                Err(CoresError::Invalid { value: bad.to_owned() }),
                "{bad:?}"
            );
        }
        let said = parse(Some(OsStr::new("lots"))).unwrap_err().to_string();
        assert!(
            said.contains("BLAZE_CORES=\"lots\"")
                && said.contains("`all`")
                && said.contains("whole number"),
            "{said}"
        );
    }

    #[test]
    fn budget_arithmetic() {
        assert_eq!(default_budget(n(18)), n(16));
        assert_eq!(default_budget(n(3)), n(1));
        assert_eq!(default_budget(n(2)), n(1));
        assert_eq!(default_budget(n(1)), n(1));
        assert_eq!(Cores::resolve(Setting::All, n(18)).budget, n(18));
        assert_eq!(Cores::resolve(Setting::Exactly(n(40)), n(18)).budget, n(40));
        assert!(!Cores::resolve(Setting::All, n(18)).lowered());
        assert!(Cores::resolve(Setting::Default, n(18)).lowered());
        assert!(Cores::resolve(Setting::Exactly(n(18)), n(18)).lowered());
        assert_eq!(split(n(16), NESTED), n(4));
        assert_eq!(split(n(16), 3), n(5));
        assert_eq!(split(n(3), NESTED), n(1));
        assert_eq!(split(n(5), 0), n(5));
    }

    #[test]
    fn budgets_agree_by_budget_and_priority() {
        let daemon = Cores::resolve(Setting::Default, n(18));
        assert!(daemon.agrees(&of(None, n(18)).unwrap()));
        assert!(daemon.agrees(&of(Some("16".as_ref()), n(18)).unwrap()));
        assert!(!daemon.agrees(&of(Some("all".as_ref()), n(18)).unwrap()));
        assert_eq!(
            of(Some("x".as_ref()), n(18)),
            Err(CoresError::Invalid { value: "x".to_owned() })
        );
        // `all` on a machine the default would also give every core still differs: priority.
        let all = Cores::resolve(Setting::All, n(18));
        assert!(!all.agrees(&of(Some("18".as_ref()), n(18)).unwrap()));
    }
}
