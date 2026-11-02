//! The thread's counts on Linux x86-64 through `perf_event_open`. Hybrid
//! CPUs use one counter group (cycles, instructions) on each core kind's
//! PMU, `cpu_core` and `cpu_atom`. Homogeneous CPUs use the generic hardware
//! PMU, recorded in `p`, with `e` zero. Cycles and instructions are user
//! space's; time is the group's running time, including kernel time, so
//! MHz reads low across system calls. The generic path requires both
//! hybrid PMU names to be absent. A sole `cpu_core` or `cpu_atom` gives no
//! counts, even on a homogeneous CPU: one missing PMU alone does not
//! establish the CPU's topology. A kernel that refuses perf access also
//! gives no counts.
use super::{Counts, Level};
use std::ffi::{c_int, c_long, c_void};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::sync::OnceLock;

/// perf_event_attr, version 0 (64 bytes): the fields used, zero after.
#[repr(C)]
struct Attr {
    kind: u32,
    size: u32,
    config: u64,
    sample_period: u64,
    sample_type: u64,
    read_format: u64,
    flags: u64,
    reserved: [u64; 2],
}

unsafe extern "C" {
    fn syscall(number: c_long, ...) -> c_long;
    #[link_name = "read"]
    fn read_fd(fd: c_int, buffer: *mut c_void, size: usize) -> isize;
}

const SYS_PERF_EVENT_OPEN: c_long = 298;
const PERF_FLAG_FD_CLOEXEC: c_long = 8;

fn attr(pmu: u32, event: u64, leader: bool) -> Attr {
    Attr {
        // PERF_TYPE_HARDWARE, with the PMU in config's upper word.
        kind: 0,
        size: std::mem::size_of::<Attr>() as u32,
        config: (u64::from(pmu) << 32) | event,
        sample_period: 0,
        sample_type: 0,
        // PERF_FORMAT_TOTAL_TIME_ENABLED | _RUNNING | _GROUP.
        read_format: 1 | 2 | 8,
        // pinned (the leader: never multiplexed), exclude_kernel, exclude_hv.
        flags: (u64::from(leader) << 2) | (1 << 5) | (1 << 6),
        reserved: [0; 2],
    }
}

/// A counter of `event` on `pmu` for the calling thread, in `group` (or a
/// new group's leader, with -1).
fn open(pmu: u32, event: u64, group: c_int) -> Option<OwnedFd> {
    let a = attr(pmu, event, group == -1);
    // Sound: the x86-64 Linux system call, `a` an initialized attr of the
    // size it states; pid 0 and cpu -1 count the calling thread anywhere.
    let fd = unsafe { syscall(SYS_PERF_EVENT_OPEN, &a as *const Attr, 0 as c_long, -1 as c_long, c_long::from(group), PERF_FLAG_FD_CLOEXEC) };
    // Sound: a fresh descriptor, ours alone.
    (fd >= 0).then(|| unsafe { OwnedFd::from_raw_fd(c_int::try_from(fd).unwrap()) })
}

/// One core kind's group: cycles leading, instructions.
struct Group {
    leader: OwnedFd,
    _instructions: OwnedFd,
}

impl Group {
    fn new(pmu: u32) -> Option<Group> {
        let leader = open(pmu, 0, -1)?; // PERF_COUNT_HW_CPU_CYCLES
        let instructions = open(pmu, 1, leader.as_raw_fd())?; // PERF_COUNT_HW_INSTRUCTIONS
        let group = Group { leader, _instructions: instructions };
        group.read()?;
        Some(group)
    }

    /// The group's counts so far; `None` once a pinned group could not be
    /// scheduled (its reads then return nothing).
    fn read(&self) -> Option<Level> {
        // nr, time enabled, time running, cycles, instructions.
        let mut words = [0u64; 5];
        // Sound: `words` is writable for the size passed.
        let n = unsafe { read_fd(self.leader.as_raw_fd(), words.as_mut_ptr().cast(), std::mem::size_of_val(&words)) };
        if n == 0 {
            return None;
        }
        assert_eq!(n, std::mem::size_of_val(&words) as isize, "a perf group read gives its whole record");
        assert_eq!(words[0], 2, "the group holds cycles and instructions");
        Some(Level { cycles: words[3], instructions: words[4], time_ns: words[2] })
    }
}

/// Both hybrid PMU types, or generic hardware events when neither name
/// is present. A single named PMU deliberately remains unsupported.
/// Zero in config's upper word asks perf for its generic hardware event.
fn pmus() -> Option<(u32, Option<u32>)> {
    static PMUS: OnceLock<Option<(u32, Option<u32>)>> = OnceLock::new();
    *PMUS.get_or_init(|| pmus_at(std::path::Path::new("/sys/bus/event_source/devices")))
}

fn pmus_at(root: &std::path::Path) -> Option<(u32, Option<u32>)> {
    // Missing/unreadable sysfs is unavailable evidence about CPU kinds.
    if !root.try_exists().ok()? {
        return None;
    }
    let p = root.join("cpu_core").try_exists().ok()?;
    let e = root.join("cpu_atom").try_exists().ok()?;
    if !p && !e {
        return Some((0, None));
    }
    let kind = |name: &str| std::fs::read_to_string(root.join(name).join("type")).ok()?.trim().parse::<u32>().ok();
    Some((kind("cpu_core")?, Some(kind("cpu_atom")?)))
}

pub fn read() -> Option<Counts> {
    // Opened on the thread's first read; they count that thread alone and
    // close when it exits.
    std::thread_local! {
        static GROUPS: Option<(Group, Option<Group>)> = pmus().and_then(|(p, e)| {
            Some((Group::new(p)?, match e { Some(e) => Some(Group::new(e)?), None => None }))
        });
    }
    GROUPS.with(|groups| {
        let (p, e) = groups.as_ref()?;
        Some(Counts {
            p: p.read()?,
            e: match e {
                Some(e) => e.read()?,
                None => Level::default(),
            },
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_attr_is_perf_event_attr_v0_counting_one_thread_unscaled() {
        assert_eq!(std::mem::size_of::<Attr>(), 64);
        let a = attr(8, 1, false);
        assert_eq!(a.config, (8u64 << 32) | 1);
        assert_eq!(attr(0, 0, true).config, 0);
        assert_eq!(attr(0, 1, false).config, 1);
        assert_eq!(a.read_format, 11);
        assert_eq!(a.flags, (1 << 5) | (1 << 6));
        assert_eq!(attr(8, 0, true).flags & (1 << 2), 1 << 2);
    }

    #[test]
    fn pmu_selection_requires_a_complete_readable_hybrid_pair() {
        let base = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target");
        std::fs::create_dir_all(&base).unwrap();
        let root = base.join(format!("pmu-test-{}", std::process::id()));
        assert_eq!(pmus_at(&root), None, "missing sysfs");
        std::fs::create_dir(&root).unwrap();
        assert_eq!(pmus_at(&root), Some((0, None)), "homogeneous PMU");
        for (first, second) in [("cpu_core", "cpu_atom"), ("cpu_atom", "cpu_core")] {
            std::fs::create_dir(root.join(first)).unwrap();
            std::fs::write(root.join(first).join("type"), "8\n").unwrap();
            assert_eq!(pmus_at(&root), None, "one hybrid PMU is insufficient");
            std::fs::create_dir(root.join(second)).unwrap();
            let path = root.join(second).join("type");
            assert_eq!(pmus_at(&root), None, "missing type");
            std::fs::write(&path, "invalid\n").unwrap();
            assert_eq!(pmus_at(&root), None, "malformed type");
            std::fs::write(&path, "9\n").unwrap();
            let expected = if first == "cpu_core" { (8, Some(9)) } else { (9, Some(8)) };
            assert_eq!(pmus_at(&root), Some(expected));
            std::fs::remove_dir_all(root.join(first)).unwrap();
            std::fs::remove_dir_all(root.join(second)).unwrap();
        }
        std::fs::remove_dir(&root).unwrap();
    }

    #[test]
    #[ignore = "needs a homogeneous x86 CPU with perf access"]
    fn homogeneous_counts_cover_work() {
        assert_eq!(pmus(), Some((0, None)));
        for b in crate::measure(4, 2_000_000, || crate::busy_work(10_000)) {
            let c = b.counts.expect("generic hardware PMU readable here");
            assert!(c.p.cycles > 0 && c.p.instructions > 0 && c.p.time_ns > 0);
            assert_eq!(c.e, Level::default());
            println!("{}; {c:?}", b.show());
        }
    }

    /// On a hybrid CPU with perf allowed: run with the thread pinned to one
    /// kind, CLOCKS_EXPECT_KIND=P (taskset -c 0) or E (taskset -c 16).
    #[test]
    #[ignore = "needs Intel hybrid PMUs, perf allowed, and a declared pinning"]
    fn counts_fall_on_the_kind_that_ran() {
        let expected = std::env::var("CLOCKS_EXPECT_KIND").expect("CLOCKS_EXPECT_KIND=P or E");
        for b in crate::measure(4, 2_000_000, || crate::busy_work(10_000)) {
            let c = b.counts.expect("hybrid PMUs readable here");
            let (ran, idle) = if expected == "P" { (c.p, c.e) } else { (c.e, c.p) };
            assert!(ran.cycles > 0 && ran.instructions > 0 && ran.time_ns > 0);
            assert_eq!(idle, Level::default(), "the other kind counted nothing");
            println!("{}", b.show());
        }
    }
}
