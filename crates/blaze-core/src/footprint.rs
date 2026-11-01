//! The memory backstop of a spawned process on macOS (docs/v2/plan/lanes/core.md section 4, "Hard
//! backstop"): one thread for every watched process that reads its physical footprint, the
//! number Activity Monitor and the kernel's memory pressure go by, and kills it once it passes
//! the limit. No resource limit is set: macOS refuses RLIMIT_AS and RLIMIT_DATA below the
//! address space every process reserves, and setting one would need a `fork` where the
//! process is started with `posix_spawn`.
//!
//! The watched processes sit in a [`Slab`] tagged [`MemTag::Io`], each [`Watch`] naming its
//! entry by handle: starting and stopping a watch allocates nothing once the slab has grown to
//! the most watched at once, and a handle to an entry taken out names nothing, even once its
//! slot is another process's.

use std::{
    io,
    sync::Once,
    time::Duration,
};

use blaze_mem::{Slab, Tag as MemTag};
use parking_lot::{Condvar, Mutex, MutexGuard};

/// How often the footprint is read: a process faulting pages in as fast as it can gets tens of
/// megabytes past its limit at most before it is killed.
const INTERVAL: Duration = Duration::from_millis(2);
/// The most processes watched at once: the slab's reservation, committed only as used.
const MOST: u32 = 1 << 16;

/// One watched process.
struct Entry {
    pid: libc::pid_t,
    limit: u64,
    /// The most footprint read so far.
    seen: u64,
    /// Killed or gone: read no more.
    done: bool,
}

/// The one thread that reads every watched process's footprint, and what it reads. A kill is
/// made with the lock held and the entry in the list, so once [`Watch::stop`] has taken the entry
/// out, nothing signals its pid.
struct Sampler {
    /// Made with the first watch.
    entries: Mutex<Option<Slab<Entry>>>,
    wake: Condvar,
}

static SAMPLER: Sampler = Sampler { entries: Mutex::new(None), wake: Condvar::new() };
static STARTED: Once = Once::new();

fn entries() -> MutexGuard<'static, Option<Slab<Entry>>> {
    SAMPLER.entries.lock()
}

pub(crate) struct Watch {
    id: blaze_mem::Handle<Entry>,
    pid: libc::pid_t,
}

impl Watch {
    /// Watches `pid`'s physical footprint (proc_pid_rusage RUSAGE_INFO_V4 ri_phys_footprint)
    /// on the one footprint thread, every few milliseconds, and SIGKILLs it once it passes
    /// `limit`.
    /// Stops when the slab's room cannot be reserved, or the most processes are watched,
    /// naming the subsystem.
    #[track_caller]
    pub(crate) fn start(pid: libc::pid_t, limit: u64) -> Watch {
        STARTED.call_once(|| {
            std::thread::Builder::new()
                .name("blaze-footprint".into()) // alloc: setup, once per process.
                .spawn(sample)
                .expect("starting the footprint watch");
        });
        let mut entries = entries();
        let slab = entries.get_or_insert_with(|| match Slab::new(MOST, MemTag::Io) {
            Ok(slab) => slab,
            Err(error) => panic!("{error}"),
        });
        let id = slab.insert(Entry { pid, limit, seen: 0, done: false });
        drop(entries);
        SAMPLER.wake.notify_one();
        Watch { id, pid }
    }

    /// Called once the process exited and before it is reaped: stops the thread and says
    /// the most it saw (ri_lifetime_max_phys_footprint where it could read it).
    pub(crate) fn stop(self) -> u64 {
        let seen = self.remove();
        match footprint(self.pid) {
            Ok(usage) => seen.max(usage.now).max(usage.most),
            Err(_) => seen,
        }
    }

    /// Out of the sampler's slab: nothing signals the pid once this returns. The most seen; 0
    /// once taken out already (its handle stale, the slot maybe another's since).
    fn remove(&self) -> u64 {
        entries().as_mut().and_then(|slab| slab.remove(self.id)).map_or(0, |it| it.seen)
    }
}

impl Drop for Watch {
    fn drop(&mut self) {
        self.remove();
    }
}

fn sample() {
    let mut entries = entries();
    loop {
        let Some(slab) = entries.as_mut().filter(|slab| slab.iter().any(|(_, it)| !it.done)) else {
            SAMPLER.wake.wait(&mut entries);
            continue;
        };
        for (_, entry) in slab.iter_mut().filter(|(_, it)| !it.done) {
            check(entry);
        }
        SAMPLER.wake.wait_for(&mut entries, INTERVAL);
    }
}

fn check(entry: &mut Entry) {
    match footprint(entry.pid) {
        Ok(usage) => {
            entry.seen = entry.seen.max(usage.now).max(usage.most);
            if usage.now > entry.limit {
                // The pid is in the list, so not yet reaped.
                crate::raw::kill(entry.pid);
                entry.done = true;
            }
        }
        // Gone: there is nothing left to hold.
        Err(error) if error.raw_os_error() == Some(libc::ESRCH) => entry.done = true,
        // Its footprint cannot be read, so its limit cannot be held: it does not run on.
        Err(_) => {
            crate::raw::kill(entry.pid);
            entry.done = true;
        }
    }
}

struct Usage {
    now: u64,
    most: u64,
}

fn footprint(pid: libc::pid_t) -> io::Result<Usage> {
    let (now, most) = crate::raw::footprint(pid)?;
    Ok(Usage { now, most })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spawn(script: &str) -> std::process::Child {
        let mut command = std::process::Command::new("/bin/sh");
        command.args(["-c", script]).env_clear().stdout(std::process::Stdio::piped());
        command.spawn().unwrap()
    }

    /// Waits for it to exit, stops the watch, then reaps it, as a driver does.
    fn end(child: std::process::Child, watch: Watch) -> (libc::c_int, u64) {
        let pid = child.id() as libc::pid_t;
        drop(child);
        crate::process::wait_exited(pid).unwrap();
        let peak = watch.stop();
        let (status, _) = crate::process::reap(pid).unwrap();
        (status, peak)
    }

    #[test]
    fn a_process_past_its_limit_is_killed() {
        let limit = 32 << 20;
        // Doubles a string until it is a gigabyte, unless it is stopped first.
        let child =
            spawn(r#"x=a; i=0; while [ $i -lt 30 ]; do x="$x$x"; i=$((i+1)); done; exit 0"#);
        let watch = Watch::start(child.id() as libc::pid_t, limit);
        let (status, peak) = end(child, watch);
        assert!(libc::WIFSIGNALED(status), "it ran to its end: {status:#x}");
        assert_eq!(libc::WTERMSIG(status), libc::SIGKILL);
        assert!(peak > limit, "{peak}");
    }

    #[test]
    fn a_process_within_its_limit_runs_to_its_end() {
        let child = spawn("exit 3");
        let watch = Watch::start(child.id() as libc::pid_t, 1 << 30);
        let (status, peak) = end(child, watch);
        assert!(!libc::WIFSIGNALED(status));
        assert_eq!(libc::WEXITSTATUS(status), 3);
        assert!(peak > 0 && peak < 1 << 30, "{peak}");
    }

    /// A watch's handle once its entry is taken out (a late drop of a copy) takes nothing out:
    /// not the watch of a process started since, which may hold its slot.
    #[test]
    fn a_watch_taken_out_names_nothing_once_its_slot_is_another_s() {
        let first = spawn("sleep 0.2; exit 0");
        let watch = Watch::start(first.id() as libc::pid_t, 1 << 30);
        let late = Watch { id: watch.id, pid: watch.pid };
        let (status, _) = end(first, watch);
        assert!(!libc::WIFSIGNALED(status));
        let second = spawn("sleep 0.2; exit 0");
        let live = Watch::start(second.id() as libc::pid_t, 1 << 30);
        assert_eq!(late.remove(), 0, "a stale handle");
        drop(late);
        assert!(entries().as_ref().unwrap().contains(live.id), "the live watch is left as it is");
        let (status, peak) = end(second, live);
        assert!(!libc::WIFSIGNALED(status) && peak > 0);
    }

    /// The one thread holds each process to its own limit.
    #[test]
    fn watches_at_once_keep_their_own_limits() {
        let grows =
            spawn(r#"x=a; i=0; while [ $i -lt 30 ]; do x="$x$x"; i=$((i+1)); done; exit 0"#);
        let small = spawn("sleep 0.3; exit 0");
        let small_watch = Watch::start(small.id() as libc::pid_t, 1 << 30);
        let grows_watch = Watch::start(grows.id() as libc::pid_t, 32 << 20);
        let (grew, _) = end(grows, grows_watch);
        let (stayed, _) = end(small, small_watch);
        assert!(libc::WIFSIGNALED(grew) && libc::WTERMSIG(grew) == libc::SIGKILL, "{grew:#x}");
        assert!(!libc::WIFSIGNALED(stayed) && libc::WEXITSTATUS(stayed) == 0, "{stayed:#x}");
    }
}
