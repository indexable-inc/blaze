//! Where the daemon's memory is.
//!
//! With `BLAZE_MEMORY` naming a directory, the daemon reports what the process holds after
//! each step of warming up, and once warm and after each request it answers writes there what
//! every ingredient of the database holds, and how much of the heap is live. A build with the
//! `heap-profile` feature run with `_RJEM_MALLOC_CONF=prof:true` also writes a profile of the
//! heap next to the ingredients, for `jeprof` to read.

use std::{
    fmt::Write as _,
    path::{Path, PathBuf},
};

use ide_db::{RootDatabase, base_db::salsa};

/// The directory `BLAZE_MEMORY` names, when it is set.
pub fn report_dir() -> Option<PathBuf> {
    std::env::var_os("BLAZE_MEMORY").map(PathBuf::from)
}

/// Megabytes of memory the process holds, as [`held_by`] counts it.
pub fn resident() -> Option<u64> {
    Some(held_by(std::process::id())? >> 20)
}

/// Bytes of memory a process holds: its resident set, where `/proc` says it.
#[cfg(not(target_os = "macos"))]
pub fn held_by(pid: u32) -> Option<u64> {
    kilobytes(&std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?, "VmRSS:")
}

/// Bytes of memory a process of this user owns: its physical footprint, which is what
/// Activity Monitor shows, and which still counts pages macOS has compressed.
#[cfg(target_os = "macos")]
pub fn held_by(pid: u32) -> Option<u64> {
    let mut info = std::mem::MaybeUninit::<libc::rusage_info_v4>::zeroed();
    let pid = libc::c_int::try_from(pid).ok()?;
    // SAFETY: `info` has room for the `RUSAGE_INFO_V4` record the call writes.
    let ok = unsafe { libc::proc_pid_rusage(pid, libc::RUSAGE_INFO_V4, info.as_mut_ptr().cast()) };
    // SAFETY: the call wrote the record when it returned 0, and zeroes are a record too.
    (ok == 0).then(|| unsafe { info.assume_init() }.ri_phys_footprint)
}

/// Bytes of memory the machine has, where the system says, or the fewer the control group of
/// the process allows: a container's, a CI job's or a systemd unit's, which the kernel holds
/// the process to however much the machine has.
#[cfg(not(target_os = "macos"))]
pub fn of_the_machine() -> Option<u64> {
    Some(limit()?.bytes)
}

/// The most memory the processes of this one may hold, and what says so.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Limit {
    pub bytes: u64,
    /// The file under `/sys/fs/cgroup` that allows the fewest, or None for the machine's memory.
    pub group: Option<String>,
}

/// [`of_the_machine`], with the control group's file that sets it.
#[cfg(not(target_os = "macos"))]
pub fn limit() -> Option<Limit> {
    let total = kilobytes(&std::fs::read_to_string("/proc/meminfo").ok()?, "MemTotal:")?;
    let groups = std::fs::read_to_string("/proc/self/cgroup").unwrap_or_default();
    let allowed = allowed_by(&groups, |file| {
        std::fs::read_to_string(Path::new("/sys/fs/cgroup").join(file)).ok()
    });
    Some(match allowed {
        Some((bytes, file)) if bytes < total => Limit { bytes, group: Some(file) },
        _ => Limit { bytes: total, group: None },
    })
}

/// [`of_the_machine`]: macOS has no control groups.
#[cfg(target_os = "macos")]
pub fn limit() -> Option<Limit> {
    Some(Limit { bytes: of_the_machine()?, group: None })
}

/// The fewest bytes the control groups of `/proc/self/cgroup` (`groups`) and the groups they
/// are in allow, and the file that says so, `read` reading a file under `/sys/fs/cgroup`:
/// `memory.max` and `memory.high` in version 2, where `max` allows any (past `memory.high`
/// the kernel throttles and reclaims, which is as good as out of memory for a build), and
/// `memory/…/memory.limit_in_bytes` in version 1.
#[cfg_attr(target_os = "macos", allow(dead_code))]
fn allowed_by(groups: &str, read: impl Fn(&str) -> Option<String>) -> Option<(u64, String)> {
    groups
        .lines()
        .filter_map(|line| {
            let (_, line) = line.split_once(':')?;
            let (controllers, path) = line.split_once(':')?;
            let (under, files): (_, &[&str]) = match controllers {
                "" => ("", &["memory.max", "memory.high"]),
                _ if controllers.split(',').any(|it| it == "memory") => {
                    ("memory/", &["memory.limit_in_bytes"])
                }
                _ => return None,
            };
            let mut at = path.trim_matches('/');
            let mut fewest: Option<(u64, String)> = None;
            loop {
                let dir = if at.is_empty() { String::new() } else { format!("{at}/") };
                for file in files {
                    let file = format!("{under}{dir}{file}");
                    if let Some(bytes) = read(&file).and_then(|it| it.trim().parse::<u64>().ok())
                        && fewest.as_ref().is_none_or(|it| bytes < it.0)
                    {
                        fewest = Some((bytes, file));
                    }
                }
                if at.is_empty() {
                    break fewest;
                }
                at = at.rsplit_once('/').map_or("", |(up, _)| up);
            }
        })
        .min()
}

/// What blaze may hold on this machine, all of its processes together (docs/memory-budget.md):
/// the [`limit`], or what `BLAZE_MEMORY_BUDGET` says, less what other processes hold and a
/// reserve for the kernel and the page cache. Swap is not counted: a build that needs it does
/// not fit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Budget {
    pub bytes: u64,
    pub limit: u64,
    /// Where `limit` comes from, for a person to read.
    pub from: String,
    pub others: u64,
    pub reserve: u64,
}

/// The setting of [`Budget`]'s limit: a size, or `off` for none.
pub const BUDGET: &str = "BLAZE_MEMORY_BUDGET";

/// The share of the limit kept back, one part in this many, and at least [`RESERVE_LEAST`].
const RESERVE_SHARE: u64 = 20;
const RESERVE_LEAST: u64 = 512 << 20;

impl Budget {
    /// The budget now, with `own` the bytes blaze's processes hold (left out of what others
    /// hold); None with `BLAZE_MEMORY_BUDGET=off` or where the system says nothing of memory.
    pub fn now(own: u64) -> Result<Option<Budget>, String> {
        let setting = std::env::var(BUDGET).ok();
        let (limit, from) = match setting.as_deref() {
            Some("off") => return Ok(None),
            Some(text) => match blaze_store::parse_size(text) {
                Some(bytes) => (bytes, format!("{BUDGET}={text}")),
                None => return Err(format!("{BUDGET}=`{text}` is neither a size nor `off`")),
            },
            None => match limit() {
                Some(Limit { bytes, group: Some(file) }) => {
                    (bytes, format!("/sys/fs/cgroup/{file}"))
                }
                Some(Limit { bytes, group: None }) => (bytes, "the machine's memory".to_owned()),
                None => return Ok(None),
            },
        };
        let others = in_use().map_or(0, |it| it.saturating_sub(own));
        Ok(Some(Budget::of(limit, from, others)))
    }

    fn of(limit: u64, from: String, others: u64) -> Budget {
        let reserve = (limit / RESERVE_SHARE).max(RESERVE_LEAST);
        let bytes = limit.saturating_sub(others).saturating_sub(reserve);
        Budget { bytes, limit, from, others, reserve }
    }
}

impl std::fmt::Display for Budget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let gb = |it: u64| it as f64 / f64::from(1 << 30);
        write!(
            f,
            "{:.1} GB: {:.1} GB ({}), less {:.1} GB other processes hold and {:.1} GB kept back",
            gb(self.bytes),
            gb(self.limit),
            self.from,
            gb(self.others),
            gb(self.reserve)
        )
    }
}

/// Bytes of memory in use on the machine: all but what the kernel could hand out without
/// swapping (`MemAvailable`).
#[cfg(not(target_os = "macos"))]
fn in_use() -> Option<u64> {
    let info = std::fs::read_to_string("/proc/meminfo").ok()?;
    Some(kilobytes(&info, "MemTotal:")?.saturating_sub(kilobytes(&info, "MemAvailable:")?))
}

/// Not read on macOS yet: what other processes hold counts as nothing there.
#[cfg(target_os = "macos")]
fn in_use() -> Option<u64> {
    None
}

/// Bytes of memory the machine has.
#[cfg(target_os = "macos")]
pub fn of_the_machine() -> Option<u64> {
    let mut bytes: u64 = 0;
    let mut size = size_of::<u64>();
    // SAFETY: `hw.memsize` is a 64-bit integer, and `bytes` and `size` say where it goes.
    let ok = unsafe {
        libc::sysctlbyname(
            c"hw.memsize".as_ptr(),
            (&raw mut bytes).cast(),
            &raw mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    (ok == 0 && bytes != 0).then_some(bytes)
}

/// The bytes a line of `/proc` such as `VmRSS:   1234 kB` says.
#[cfg_attr(target_os = "macos", allow(dead_code))]
fn kilobytes(text: &str, name: &str) -> Option<u64> {
    let line = text.lines().find_map(|it| it.strip_prefix(name))?;
    line.trim().trim_end_matches("kB").trim().parse::<u64>().ok()?.checked_mul(1024)
}

/// Bytes allocated and not freed, when the allocator counts them.
pub fn live() -> Option<u64> {
    #[cfg(unix)]
    {
        tikv_jemalloc_ctl::epoch::advance().ok()?;
        tikv_jemalloc_ctl::stats::allocated::read().ok().map(|it| it as u64)
    }
    #[cfg(windows)]
    None
}

/// Gives the system back the pages of what was freed, which jemalloc keeps for ten seconds
/// to reuse otherwise.
pub fn give_back() {
    #[cfg(unix)]
    // SAFETY: `arena.<i>.purge` reads and writes nothing, and 4096 (`MALLCTL_ARENAS_ALL`)
    // is every arena.
    unsafe {
        use std::ptr::null_mut;
        tikv_jemalloc_sys::mallctl(
            c"arena.4096.purge".as_ptr(),
            null_mut(),
            null_mut(),
            null_mut(),
            0,
        );
    }
}

/// Reports what the process holds now that `step` is done.
pub fn checkpoint(step: &str) {
    if report_dir().is_none() {
        return;
    }
    let live = live().map(|it| format!(", {} MB live", it >> 20)).unwrap_or_default();
    eprintln!("memory after {step}: {} MB resident{live}", resident().unwrap_or(0));
}

/// Writes what the daemon holds now that `step` is done to the directory `BLAZE_MEMORY` names.
pub fn report(db: &mut RootDatabase, step: &str) -> anyhow::Result<()> {
    checkpoint(step);
    let Some(dir) = report_dir() else { return Ok(()) };
    std::fs::create_dir_all(&dir)?;
    write_heap_profile(&dir.join(format!("{step}.prof")))?;
    std::fs::write(dir.join(format!("{step}.tsv")), ingredients(db))?;
    Ok(())
}

/// [`report`] once the daemon has answered a request, as `request-N`, N counting from 1.
pub fn served(db: &mut RootDatabase) -> anyhow::Result<()> {
    static SERVED: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    if report_dir().is_none() {
        return Ok(());
    }
    let served = SERVED.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
    report(db, &format!("request-{served}"))
}

/// A line for every struct and query of the database: how many there are and the bytes of
/// their fields, of the database's own records about them and, where the ingredient can
/// tell, of what the fields own on the heap.
fn ingredients(db: &mut RootDatabase) -> String {
    let db: &mut dyn salsa::Database = db;
    let usage = db.memory_usage();
    let structs = usage.structs.iter().map(|it| ("struct", it.debug_name(), it));
    let queries = usage.queries.iter().map(|(&name, it)| ("query", name, it));
    let mut rows: Vec<_> = structs.chain(queries).collect();
    rows.sort_by_key(|(_, _, it)| std::cmp::Reverse(it.size_of_fields() + it.size_of_metadata()));
    let mut table = String::from("kind\tcount\tfields\tmetadata\theap\tname\n");
    for (kind, name, it) in rows {
        let heap = it.heap_size_of_fields().map(|it| it.to_string()).unwrap_or_default();
        _ = writeln!(
            table,
            "{kind}\t{}\t{}\t{}\t{heap}\t{name}",
            it.count(),
            it.size_of_fields(),
            it.size_of_metadata(),
        );
    }
    table
}

#[cfg(feature = "heap-profile")]
fn write_heap_profile(path: &Path) -> anyhow::Result<()> {
    use std::os::unix::ffi::OsStrExt as _;

    // SAFETY: `opt.prof` is a `bool`.
    if !unsafe { tikv_jemalloc_ctl::raw::read::<bool>(b"opt.prof\0") }.unwrap_or(false) {
        return Ok(());
    }
    // A profile has the addresses of the code that allocated, and on macOS nothing of where
    // the program was loaded, which `harness/heapfold.py` needs to name them.
    #[cfg(target_os = "macos")]
    {
        // SAFETY: image 0 is the program itself, which is loaded for as long as it runs.
        let header = unsafe { libc::_dyld_get_image_header(0) } as usize;
        std::fs::write(path.with_extension("load"), format!("{header:#x}\n"))?;
    }
    let path = std::ffi::CString::new(path.as_os_str().as_bytes())?;
    // SAFETY: `prof.dump` takes a C string, which `path` is for the length of the call.
    unsafe { tikv_jemalloc_ctl::raw::write(b"prof.dump\0", path.as_ptr()) }
        .map_err(|e| anyhow::anyhow!("writing the heap profile: {e}"))
}

#[cfg(not(feature = "heap-profile"))]
fn write_heap_profile(_: &Path) -> anyhow::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_line_of_proc_is_read_as_bytes() {
        let status = "Name:\tblaze\nVmRSS:\t    2048 kB\nThreads:\t4\n";
        assert_eq!(kilobytes(status, "VmRSS:"), Some(2048 << 10));
        assert_eq!(kilobytes(status, "VmSwap:"), None);
        assert_eq!(kilobytes("VmRSS: many kB\n", "VmRSS:"), None);
    }

    #[test]
    fn a_control_group_allows_what_it_and_those_above_it_allow() {
        let files = |files: &'static [(&'static str, &'static str)]| {
            move |file: &str| files.iter().find(|it| it.0 == file).map(|it| it.1.to_owned())
        };
        let unit = files(&[
            ("user.slice/memory.max", "8589934592\n"),
            ("user.slice/run.scope/memory.max", "2147483648\n"),
        ]);
        let bytes = |it: Option<(u64, String)>| it.map(|it| it.0);
        assert_eq!(
            allowed_by("0::/user.slice/run.scope\n", &unit),
            Some((2 << 30, "user.slice/run.scope/memory.max".to_owned()))
        );
        let above = files(&[("a/memory.max", "1073741824\n"), ("a/b/memory.max", "max\n")]);
        assert_eq!(bytes(allowed_by("0::/a/b\n", above)), Some(1 << 30));
        let container = files(&[("memory.max", "4294967296\n")]);
        assert_eq!(bytes(allowed_by("0::/\n", container)), Some(4 << 30));
        let v1 = files(&[("memory/docker/c1/memory.limit_in_bytes", "536870912\n")]);
        assert_eq!(
            bytes(allowed_by("5:cpu,cpuacct:/docker/c1\n4:memory:/docker/c1\n", v1)),
            Some(512 << 20)
        );
        assert_eq!(
            allowed_by("0::/user.slice\n", files(&[("user.slice/memory.max", "max\n")])),
            None
        );
        assert_eq!(allowed_by("", files(&[])), None);
    }

    #[test]
    fn a_groups_high_mark_limits_as_its_max_does() {
        let files = |file: &str| match file {
            "ci.slice/memory.max" => Some("8589934592\n".to_owned()),
            "ci.slice/memory.high" => Some("6442450944\n".to_owned()),
            _ => None,
        };
        assert_eq!(
            allowed_by("0::/ci.slice\n", files),
            Some((6 << 30, "ci.slice/memory.high".to_owned()))
        );
    }

    #[test]
    fn the_budget_is_the_limit_less_others_and_a_reserve() {
        let budget = Budget::of(16 << 30, "the machine's memory".to_owned(), 1 << 30);
        // 5% of 16 GB is 0.8 GB, over the 512 MB least.
        assert_eq!(budget.reserve, (16 << 30) / 20);
        assert_eq!(budget.bytes, (16 << 30) - (1 << 30) - (16 << 30) / 20);
        let small = Budget::of(2 << 30, "BLAZE_MEMORY_BUDGET=2G".to_owned(), 0);
        assert_eq!(small.reserve, 512 << 20);
        assert_eq!(Budget::of(256 << 20, String::new(), 0).bytes, 0);
    }

    #[test]
    fn this_process_and_the_machine_hold_some() {
        let held = held_by(std::process::id());
        let machine = of_the_machine();
        if cfg!(any(target_os = "linux", target_os = "macos")) {
            assert!(held.is_some_and(|it| it > 0) && machine.is_some_and(|it| it > held.unwrap()));
        }
    }
}
