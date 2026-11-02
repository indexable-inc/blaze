//! Each thread of this process and its time on a CPU so far: for seeing
//! which threads a measurement kept busy (a pipeline's stages, a pool's
//! workers), beside its wall time; never for timing a measurement itself
//! (wall time, [`crate::measure`]).

/// One thread: its id, its name, and its time on a CPU in nanoseconds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Thread {
    pub id: u64,
    pub name: String,
    pub cpu_ns: u64,
}

/// Every thread of this process now, in id order; `None` where the
/// platform gives no per-thread CPU time in nanoseconds (all but Linux,
/// whose /proc/self/task/ID/schedstat starts with it).
pub fn snapshot() -> Option<Vec<Thread>> {
    #[cfg(target_os = "linux")]
    {
        let mut threads: Vec<Thread> = std::fs::read_dir("/proc/self/task")
            .ok()?
            .filter_map(|entry| {
                let dir = entry.ok()?.path();
                let id = dir.file_name()?.to_str()?.parse().ok()?;
                // A thread that ended meanwhile has no files: it is left out.
                let name = std::fs::read_to_string(dir.join("comm")).ok()?.trim_end().to_string();
                let schedstat = std::fs::read_to_string(dir.join("schedstat")).ok()?;
                let cpu_ns = schedstat.split(' ').next()?.parse().expect("schedstat starts with an integer");
                Some(Thread { id, name, cpu_ns })
            })
            .collect();
        threads.sort_by_key(|t| t.id);
        Some(threads)
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

/// Each thread of `after` with its CPU time since `before` (a thread that
/// started between them counts from zero).
pub fn since(before: &[Thread], after: &[Thread]) -> Vec<Thread> {
    after
        .iter()
        .map(|t| {
            let earlier = before.iter().find(|b| b.id == t.id).map_or(0, |b| b.cpu_ns);
            Thread { id: t.id, name: t.name.clone(), cpu_ns: t.cpu_ns - earlier }
        })
        .collect()
}

/// `threads`' CPU time as a share of `wall_ns`, one line: each thread
/// that ran (name/id share%), busiest first.
pub fn describe(threads: &[Thread], wall_ns: u64) -> String {
    assert!(wall_ns > 0, "a share of some wall time");
    let mut busy: Vec<&Thread> = threads.iter().filter(|t| t.cpu_ns > 0).collect();
    busy.sort_by_key(|t| std::cmp::Reverse(t.cpu_ns));
    let parts: Vec<String> = busy
        .iter()
        .map(|t| {
            let permille = (u128::from(t.cpu_ns) * 1000 + u128::from(wall_ns) / 2) / u128::from(wall_ns);
            format!("{}/{} {}.{}%", t.name, t.id, permille / 10, permille % 10)
        })
        .collect();
    parts.join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_busy_thread_shows_its_time() {
        let Some(before) = snapshot() else { return };
        let t = crate::now();
        let worker = std::thread::Builder::new().name("clocks-busy".into()).spawn(|| {
            crate::busy_work(20_000_000);
            snapshot().unwrap()
        });
        let during = worker.unwrap().join().unwrap();
        let wall = crate::since_ns(t);
        let busy = since(&before, &during);
        let mine = busy.iter().find(|x| x.name == "clocks-busy").expect("the worker saw itself");
        assert!(mine.cpu_ns >= 15_000_000 && mine.cpu_ns <= wall, "busy 20 ms: {} ns on a CPU in {wall} ns", mine.cpu_ns);
        assert!(describe(&busy, wall).contains("clocks-busy/"));
    }

    #[test]
    fn since_counts_a_new_thread_from_zero() {
        let before = [Thread { id: 1, name: "a".into(), cpu_ns: 5 }];
        let after = [Thread { id: 1, name: "a".into(), cpu_ns: 9 }, Thread { id: 2, name: "b".into(), cpu_ns: 4 }];
        assert_eq!(since(&before, &after).iter().map(|t| t.cpu_ns).collect::<Vec<_>>(), [4, 4]);
        assert_eq!(describe(&since(&before, &after), 8), "a/1 50.0%, b/2 50.0%");
    }
}
