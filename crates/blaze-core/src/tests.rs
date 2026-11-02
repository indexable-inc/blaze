//! One script of effects on every driver: they must end the same.

use std::{
    collections::BTreeMap,
    path::PathBuf,
    time::{Duration, Instant},
};

use crate::{
    Completion, Done, Driver, Effect, FileName, Tag, run_until_idle,
    sim::{Faults, Sim},
    threads::Threads,
};

fn name(it: &str) -> FileName {
    FileName::new(it).unwrap()
}

fn dir(what: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("blaze-core-{what}-{}", std::process::id()));
    _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// As [`dir`], under `/tmp`: a Unix socket's whole path must fit in its address (about 104
/// bytes), which a long temporary directory (a Nix shell's) leaves no room for.
#[cfg(unix)]
fn socket_dir(what: &str) -> PathBuf {
    let dir = PathBuf::from(format!("/tmp/blaze-core-{what}-{}", std::process::id()));
    _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// What became of each step, by its tag. A clone is either done or refused for the file
/// system, which the drivers on disk may differ in; it reads back the same either way.
fn script(driver: &mut impl Driver) -> BTreeMap<u64, String> {
    let mut seen = BTreeMap::new();
    let mut effects = vec![Effect::Write {
        file: name("store/a/value"),
        bytes: b"compiled".to_vec(),
        tag: Tag(1),
    }];
    run_until_idle(driver, &mut effects, |done, effects| {
        let shown = match &done.done {
            Done::Failed(why) if done.tag == Tag(3) && why.0.contains("does not copy") => {
                "refused".to_owned()
            }
            Done::Cloned if done.tag == Tag(3) => "refused".to_owned(),
            Done::Stat(Some(meta)) if meta.dir => "stat a directory".to_owned(),
            Done::Stat(Some(meta)) => format!("stat {} bytes", meta.len),
            // A directory's own length is the file system's.
            Done::Listed(found) => found
                .iter()
                .map(|(file, meta)| match meta.dir {
                    true => format!("{file}/"),
                    false => format!("{file} {}", meta.len),
                })
                .collect::<Vec<_>>()
                .join(", "),
            Done::Failed(_) => "failed".to_owned(),
            other => format!("{other:?}"),
        };
        seen.insert(done.tag.0, shown);
        let next = match done.tag.0 {
            1 => Effect::Write { file: name("store/a/value"), bytes: b"again".to_vec(), tag: Tag(2) },
            2 => Effect::Clone { from: name("store/a/value"), to: name("out/c"), tag: Tag(3) },
            3 => Effect::Link { from: name("store/a/value"), to: name("out/l"), tag: Tag(4) },
            4 => Effect::ReadAll { file: name("out/l"), tag: Tag(5) },
            5 => Effect::Stat { file: name("store/a/value"), tag: Tag(6) },
            6 => Effect::Stat { file: name("store/a"), tag: Tag(7) },
            7 => Effect::Stat { file: name("missing"), tag: Tag(8) },
            8 => Effect::Remove { file: name("out/l"), tag: Tag(9) },
            9 => Effect::Remove { file: name("out/l"), tag: Tag(10) },
            10 => Effect::ReadAll { file: name("out/l"), tag: Tag(11) },
            11 => Effect::Append { file: name("store/a/value"), bytes: b"+".to_vec(), tag: Tag(12) },
            12 => Effect::ReadAll { file: name("store/a/value"), tag: Tag(13) },
            13 => Effect::Link { from: name("missing"), to: name("out/m"), tag: Tag(14) },
            14 => Effect::Unwatch { tag: Tag(99) },
            99 => Effect::WriteAt {
                file: name("store/a/value"),
                at: 4,
                bytes: b"XYZ".to_vec(),
                tag: Tag(16),
            },
            16 => Effect::ReadAll { file: name("store/a/value"), tag: Tag(17) },
            17 => Effect::Mode { file: name("store/a/value"), mode: 0o755, tag: Tag(18) },
            18 => Effect::Rename {
                from: name("store/a/value"),
                to: name("store/a/moved"),
                tag: Tag(19),
            },
            19 => Effect::Stat { file: name("store/a/value"), tag: Tag(20) },
            20 => Effect::List { dir: name("store"), tag: Tag(21) },
            21 => Effect::WriteAt { file: name("missing"), at: 0, bytes: vec![1], tag: Tag(22) },
            22 => Effect::Rename { from: name("missing"), to: name("out/m"), tag: Tag(23) },
            23 => Effect::List { dir: name("missing"), tag: Tag(24) },
            _ => return,
        };
        effects.push(next);
    });
    seen
}

#[test]
fn every_driver_ends_the_script_the_same() {
    let mut sim = Sim::new(1, Faults::default());
    let simulated = script(&mut sim);
    let root = dir("threads");
    let threads = script(&mut Threads::new(&root, 2));
    assert_eq!(simulated, threads);
    #[cfg(target_os = "macos")]
    {
        let root = dir("kqueue");
        let kqueue = script(&mut crate::kqueue::Kqueue::new(&root, 2).unwrap());
        assert_eq!(simulated, kqueue);
    }
    #[cfg(windows)]
    {
        let root = dir("iocp");
        let iocp = script(&mut crate::iocp::Iocp::new(&root, 2).unwrap());
        assert_eq!(simulated, iocp);
    }
    assert_eq!(simulated[&5], format!("{:?}", Done::Read(b"again".to_vec())));
    assert_eq!(simulated[&13], format!("{:?}", Done::Read(b"again+".to_vec())));
    assert_eq!(simulated[&8], format!("{:?}", Done::Stat(None)));
    assert_eq!(simulated[&7], "stat a directory");
    assert_eq!(simulated[&14], "failed");
    assert_eq!(simulated[&17], format!("{:?}", Done::Read(b"agaiXYZ".to_vec())));
    assert_eq!(simulated[&20], format!("{:?}", Done::Stat(None)));
    assert_eq!(simulated[&21], "store/a/, store/a/moved 7");
    assert_eq!((&simulated[&22][..], &simulated[&23][..], &simulated[&24][..]), ("failed", "failed", "failed"));
    #[cfg(target_os = "linux")]
    {
        let root = dir("uring");
        let ring = script(&mut crate::uring::Uring::new(&root).unwrap());
        assert_eq!(simulated, ring);
    }
}

/// This target's [`crate::Native`] driver, as the build path makes it, ends the script as the
/// threads driver does.
#[cfg(any(target_os = "linux", target_os = "macos", windows))]
#[test]
fn the_native_driver_runs_the_shared_script_as_threads_does() {
    let root = dir("native-script");
    let threads = script(&mut Threads::new(&root, 2));
    let root = dir("native-script-native");
    assert_eq!(script(&mut crate::native(&root, 2).unwrap()), threads);
}

/// A time with nanoseconds below the second, and a whole number of NTFS's 100 ns, so every
/// file system the tests run on keeps it exactly.
const TIME: u64 = 1_700_000_000_123_456_700;

/// A file written and its times set to `TIME`, then a directory's set to an earlier time, read
/// back by a stat and a listing; then a name never written and one renamed away are each set,
/// and fail naming it. What came of each step, by its tag.
fn times(driver: &mut impl Driver) -> BTreeMap<u64, String> {
    let earlier = TIME - 86_400 * 1_000_000_000;
    let mut seen = BTreeMap::new();
    let mut effects =
        vec![Effect::Write { file: name("target/build/output"), bytes: b"cargo::rerun-if-changed=build.rs\n".to_vec(), tag: Tag(1) }];
    run_until_idle(driver, &mut effects, |done, effects| {
        let shown = match &done.done {
            // A directory's own length is the file system's.
            Done::Stat(Some(meta)) if meta.dir => format!("stat a directory {}", meta.modified_ns),
            Done::Stat(Some(meta)) => format!("stat {} bytes {}", meta.len, meta.modified_ns),
            Done::Listed(found) => found
                .iter()
                .filter(|(_, meta)| !meta.dir)
                .map(|(file, meta)| format!("{file} {}", meta.modified_ns))
                .collect::<Vec<_>>()
                .join(", "),
            Done::Failed(why) if why.0.contains("never-written") || why.0.contains("output") => "failed, named".to_owned(),
            other => format!("{other:?}"),
        };
        seen.insert(done.tag.0, shown);
        let next = match done.tag.0 {
            1 => Effect::Times { file: name("target/build/output"), modified_ns: TIME, tag: Tag(2) },
            2 => Effect::Stat { file: name("target/build/output"), tag: Tag(3) },
            3 => Effect::Times { file: name("target/build"), modified_ns: earlier, tag: Tag(4) },
            4 => Effect::Stat { file: name("target/build"), tag: Tag(5) },
            5 => Effect::List { dir: name("target"), tag: Tag(6) },
            6 => Effect::Times { file: name("target/never-written"), modified_ns: TIME, tag: Tag(7) },
            7 => Effect::Rename { from: name("target/build/output"), to: name("target/moved"), tag: Tag(8) },
            8 => Effect::Times { file: name("target/build/output"), modified_ns: TIME, tag: Tag(9) },
            9 => Effect::Stat { file: name("target/moved"), tag: Tag(10) },
            _ => return,
        };
        effects.push(next);
    });
    seen
}

/// A file's times read back as they were set, on every driver that runs here; a name not
/// there, never written or renamed away, fails and is named.
#[test]
fn every_driver_sets_a_files_times_as_asked() {
    let earlier = TIME - 86_400 * 1_000_000_000;
    let simulated = times(&mut Sim::new(5, Faults::default()));
    assert_eq!(simulated[&2], format!("{:?}", Done::Timed));
    assert_eq!(simulated[&3], format!("stat 33 bytes {TIME}"));
    assert_eq!(simulated[&4], format!("{:?}", Done::Timed));
    assert_eq!(simulated[&5], format!("stat a directory {earlier}"));
    assert_eq!(simulated[&6], format!("target/build/output {TIME}"));
    assert_eq!((&simulated[&7][..], &simulated[&9][..]), ("failed, named", "failed, named"));
    // A rename keeps the inode, and with it the time.
    assert_eq!(simulated[&10], format!("stat 33 bytes {TIME}"));
    let root = dir("times-threads");
    assert_eq!(times(&mut Threads::new(&root, 2)), simulated, "threads");
    #[cfg(target_os = "linux")]
    assert_eq!(times(&mut crate::uring::Uring::new(dir("times-uring")).unwrap()), simulated, "io_uring");
    #[cfg(target_os = "macos")]
    assert_eq!(times(&mut crate::kqueue::Kqueue::new(dir("times-kqueue"), 2).unwrap()), simulated, "kqueue");
    #[cfg(windows)]
    assert_eq!(times(&mut crate::iocp::Iocp::new(&dir("times-iocp"), 2).unwrap()), simulated, "IOCP");
}

/// `rounds` of the file `f`'s times set: what they allocated on the polling thread.
fn times_rounds(driver: &mut impl Driver, rounds: u64) -> u64 {
    let file = name("f");
    let (mut effects, mut out) = (Vec::with_capacity(1), Vec::with_capacity(4));
    let mut used = 0;
    for round in 0..rounds {
        // The caller's: made before the cycle.
        effects.push(Effect::Times { file: file.clone(), modified_ns: TIME + 100 * round, tag: Tag(round) });
        let ((), cycle) = blaze_alloc_count::measure(|| {
            driver.submit(&mut effects);
            poll_for(driver, &mut out, 1);
        });
        used += cycle.allocations;
        assert_eq!(out.as_slice(), [Completion { tag: Tag(round), done: Done::Timed }]);
        out.clear();
    }
    assert_eq!(driver.in_flight(), 0);
    used
}

/// A file's times set, once the driver has grown to it: nothing allocated by the drivers on
/// disk, on the polling thread or a file thread. The simulation's disk keeps a journal of
/// every change, which a set of times is: one entry each (its name), and the journal's growth.
#[test]
fn a_warm_times_set_allocates_nothing() {
    fn write(driver: &mut impl Driver) {
        let mut effects = vec![Effect::Write { file: name("f"), bytes: b"bytes".to_vec(), tag: Tag(1) }];
        run_until_idle(driver, &mut effects, |it, _| assert_eq!(it.done, Done::Written));
    }
    let mut sim = Sim::new(15, Faults::default());
    write(&mut sim);
    times_rounds(&mut sim, WARM);
    let used = times_rounds(&mut sim, ROUNDS);
    let journal = ROUNDS + u64::from(ROUNDS.ilog2());
    assert!(
        used <= journal,
        "the simulation: {used} allocations over {ROUNDS} warm sets of times; its journal's entries and growth are {journal}"
    );
    let root = dir("budget-times");
    let mut threads = Threads::new(&root, 1);
    write(&mut threads);
    times_rounds(&mut threads, WARM);
    threads.off_thread();
    assert_eq!(times_rounds(&mut threads, ROUNDS), 0, "threads: {ROUNDS} warm sets of times, on the polling thread");
    // On Windows the file thread makes its path with `sys::path`, which allocates; `Iocp`, the
    // native driver there, sets times on its polling thread and is held to zero below.
    #[cfg(unix)]
    assert_eq!(threads.off_thread(), (0, 0), "threads: the file thread's utimensat");
    drop(threads);
    std::fs::remove_dir_all(root).unwrap();
    #[cfg(target_os = "linux")]
    {
        let root = dir("budget-times-uring");
        let mut ring = crate::uring::Uring::new(&root).unwrap();
        write(&mut ring);
        times_rounds(&mut ring, WARM);
        assert_eq!(times_rounds(&mut ring, ROUNDS), 0, "io_uring: {ROUNDS} warm sets of times, its utimensat inline");
        drop(ring);
        std::fs::remove_dir_all(root).unwrap();
    }
    #[cfg(windows)]
    {
        let root = dir("budget-times-iocp");
        let mut iocp = crate::iocp::Iocp::new(&root, 1).unwrap();
        write(&mut iocp);
        times_rounds(&mut iocp, WARM);
        assert_eq!(times_rounds(&mut iocp, ROUNDS), 0, "IOCP: {ROUNDS} warm sets of times, SetFileTime inline");
        drop(iocp);
        std::fs::remove_dir_all(root).unwrap();
    }
}

/// Timers asked for out of order, some due at once and one never in the test's time, fire in
/// the order they are due on every driver, each counted out of what is in flight.
#[test]
fn every_driver_fires_timers_in_their_order() {
    fn run(driver: &mut impl Driver) -> Vec<u64> {
        let mut effects: Vec<Effect> = [9u64, 0, 4, 0, 13, 2, 7]
            .into_iter()
            .enumerate()
            .map(|(n, it)| Effect::Timer { after: Duration::from_millis(3 * it), tag: Tag(n as u64) })
            .collect();
        effects.push(Effect::Timer { after: Duration::from_secs(1 << 40), tag: Tag(99) });
        driver.submit(&mut effects);
        let mut fired = Vec::new();
        let mut out = Vec::new();
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while driver.in_flight() > 1 && std::time::Instant::now() < deadline {
            driver.poll(&mut out, Duration::from_millis(100));
            for it in out.drain(..) {
                assert_eq!(it.done, Done::Fired);
                fired.push(it.tag.0);
            }
        }
        assert_eq!(driver.in_flight(), 1, "the far timer is still in flight");
        // The two due at once may fire together, in either order on a real clock.
        fired[..2].sort_unstable();
        fired
    }
    let want = vec![1, 3, 5, 2, 6, 0, 4];
    assert_eq!(run(&mut Sim::new(3, Faults::default())), want);
    assert_eq!(run(&mut Threads::new(&dir("timers-threads"), 1)), want);
    #[cfg(target_os = "macos")]
    assert_eq!(run(&mut crate::kqueue::Kqueue::new(dir("timers-kqueue"), 1).unwrap()), want);
    #[cfg(windows)]
    assert_eq!(run(&mut crate::iocp::Iocp::new(&dir("timers-iocp"), 1).unwrap()), want);
    #[cfg(target_os = "linux")]
    assert_eq!(run(&mut crate::uring::Uring::new(dir("timers-uring")).unwrap()), want);
}

/// A file written under a watched directory is heard of, on the drivers that watch.
#[cfg(target_os = "linux")]
#[test]
fn watches_hear_a_file_written_under_them() {
    fn run(driver: &mut impl Driver, root: Option<&std::path::Path>) -> Vec<FileName> {
        let mut effects = vec![
            Effect::Write { file: name("src/keep"), bytes: Vec::new(), tag: Tag(1) },
        ];
        run_until_idle(driver, &mut effects, |_, _| {});
        driver.submit(&mut vec![Effect::Watch { dir: name("src"), tag: Tag(7) }]);
        let mut effects = vec![
            Effect::Write { file: name("src/deep/lib.rs"), bytes: b"fn f() {}".to_vec(), tag: Tag(2) },
        ];
        let mut heard = Vec::new();
        let mut asked_end = false;
        let mut ended = false;
        // A write by someone else, as an editor's.
        if let Some(root) = root {
            std::fs::write(root.join("src/keep"), b"edited").unwrap();
        }
        let mut done = Vec::new();
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while !ended && std::time::Instant::now() < deadline {
            driver.submit(&mut effects);
            driver.poll(&mut done, Duration::from_millis(50));
            for it in done.drain(..) {
                match it.done {
                    Done::Changed { files, .. } => heard.extend(files),
                    Done::Unwatched => ended = true,
                    Done::Failed(why) => panic!("{why}"),
                    _ => {}
                }
            }
            if !asked_end && heard.contains(&name("src/deep/lib.rs")) {
                asked_end = true;
                effects.push(Effect::Unwatch { tag: Tag(7) });
            }
        }
        assert!(ended, "the watch never ended; heard {heard:?}");
        assert_eq!(driver.in_flight(), 0);
        heard
    }
    let root = dir("watch-threads");
    let heard = run(&mut Threads::new(&root, 2), Some(&root));
    assert!(heard.contains(&name("src/keep")), "{heard:?}");
    let root = dir("watch-uring");
    let heard = run(&mut crate::uring::Uring::new(&root).unwrap(), Some(&root));
    assert!(heard.contains(&name("src/keep")), "{heard:?}");
}

/// A file written by someone else before a flush is heard of before the flush completes, every
/// round; a flush of a watch not in flight fails; nothing is left in flight once the watch ends.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn flushes_follow_the_changes_before_them(driver: &mut impl Driver, root: &std::path::Path) {
    let mut effects = vec![Effect::Write { file: name("src/keep"), bytes: Vec::new(), tag: Tag(1) }];
    run_until_idle(driver, &mut effects, |_, _| {});
    driver.submit(&mut vec![Effect::Watch { dir: name("src"), tag: Tag(7) }]);
    let mut done = Vec::new();
    for round in 0..50u64 {
        let file = format!("src/deep/edit-{round}");
        std::fs::create_dir_all(root.join("src/deep")).unwrap();
        std::fs::write(root.join(&file), b"edited").unwrap();
        // Two flushes at once: each comes after the change.
        let (one, two) = (Tag(1000 + 2 * round), Tag(1001 + 2 * round));
        driver.submit(&mut vec![
            Effect::Flush { watch: Tag(7), tag: one },
            Effect::Flush { watch: Tag(7), tag: two },
        ]);
        let mut heard = Vec::new();
        let mut flushed = 0;
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while flushed < 2 && std::time::Instant::now() < deadline {
            driver.poll(&mut done, Duration::from_millis(50));
            for it in done.drain(..) {
                match it.done {
                    Done::Changed { files, .. } => heard.extend(files),
                    Done::Flushed if it.tag == one || it.tag == two => {
                        assert!(heard.contains(&name(&file)), "round {round}: flushed first; {heard:?}");
                        flushed += 1;
                    }
                    other => panic!("round {round}: {other:?}"),
                }
            }
        }
        assert_eq!(flushed, 2, "round {round}: the flushes never came");
    }
    let mut effects = vec![Effect::Flush { watch: Tag(8), tag: Tag(9) }, Effect::Unwatch { tag: Tag(7) }];
    let (mut refused, mut ended) = (false, false);
    run_until_idle(driver, &mut effects, |it, _| match (it.tag.0, it.done) {
        (9, Done::Failed(_)) => refused = true,
        (7, Done::Changed { .. }) => {}
        (7, Done::Unwatched) => ended = true,
        other => panic!("{other:?}"),
    });
    assert!(refused && ended);
    assert_eq!(driver.in_flight(), 0);
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn every_watching_driver_tells_a_flush_after_the_changes_before_it() {
    let root = dir("flush-threads");
    flushes_follow_the_changes_before_them(&mut Threads::new(&root, 2), &root);
    #[cfg(target_os = "linux")]
    {
        let root = dir("flush-uring");
        flushes_follow_the_changes_before_them(&mut crate::uring::Uring::new(&root).unwrap(), &root);
    }
}

/// A file mapped, a process run with input, and a connection made, used and hung up: what
/// came of each, by step.
#[cfg(unix)]
fn processes_and_sockets(driver: &mut impl Driver) -> Vec<String> {
    use crate::{Address, Command, Sandbox};
    let at = Address::Unix(name("run/daemon.sock"));
    let sh = Box::new(Command {
        program: "/bin/sh".into(),
        args: vec!["-c".into(), "cat; echo err >&2; exit 3".into()],
        env: vec![("PATH".into(), "/usr/bin:/bin".into())],
        dir: "/".into(),
        stdin: b"in".to_vec(),
        sandbox: Sandbox::Off,
        memory: None,
    });
    let mut seen = Vec::new();
    let mut effects = vec![
        Effect::Write { file: name("store/pack"), bytes: b"packed".to_vec(), tag: Tag(1) },
        Effect::Spawn { command: sh, tag: Tag(3) },
        Effect::Listen { at: at.clone(), tag: Tag(4) },
    ];
    let (mut client, mut server, mut got) = (None, None, Vec::new());
    let mut ended_after_hangup = false;
    let mut next = 100;
    run_until_idle(driver, &mut effects, |done, effects| {
        next += 1;
        match (done.tag.0, done.done) {
            (1, Done::Written) => {
                seen.push("written".to_owned());
                effects.push(Effect::Map { file: name("store/pack"), tag: Tag(2) });
                effects.push(Effect::Connect { at: at.clone(), tag: Tag(5) });
            }
            (2, Done::Mapped(it)) => seen.push(format!("mapped {:?}", it.bytes())),
            (3, Done::Exited(exit)) => seen.push(format!(
                "exited {:?} {:?} {:?} {:?}",
                exit.code,
                exit.signal,
                String::from_utf8_lossy(&exit.stdout),
                String::from_utf8_lossy(&exit.stderr)
            )),
            (4, Done::Accepted { conn }) => {
                server = Some(conn);
                effects.push(Effect::Recv { conn, most: 16, into: Vec::new(), tag: Tag(next) });
            }
            (5, Done::Connected { conn }) => {
                client = Some(conn);
                effects.push(Effect::Send { conn, bytes: b"ping".to_vec(), tag: Tag(6) });
            }
            (6, Done::Sent(bytes)) => seen.push(format!("sent, {} bytes back", bytes.len())),
            (7, Done::HungUp) => seen.push("hung up".to_owned()),
            (4, Done::Unlistened) => seen.push("unlistened".to_owned()),
            (_, Done::Read(bytes)) if bytes.is_empty() => {
                ended_after_hangup = got.len() == 4;
                seen.push(format!("received {:?}, then the end", String::from_utf8_lossy(&got)));
                effects.push(Effect::Unlisten { tag: Tag(4) });
            }
            (_, Done::Read(mut bytes)) => {
                got.extend_from_slice(&bytes);
                bytes.clear();
                let conn = server.expect("accepted");
                if got.len() == 4 {
                    effects.push(Effect::Hangup { conn: client.expect("connected"), tag: Tag(7) });
                }
                effects.push(Effect::Recv { conn, most: 16, into: bytes, tag: Tag(next) });
            }
            (tag, other) => seen.push(format!("{tag}: {other:?}")),
        }
    });
    assert!(ended_after_hangup, "{seen:?}");
    assert_eq!(driver.in_flight(), 0);
    seen.sort();
    seen
}

#[cfg(unix)]
#[test]
fn every_driver_runs_processes_and_sockets_the_same() {
    let mut sim = Sim::new(3, Faults::default());
    sim.program("/bin/sh", |command: &crate::Command, _: &mut crate::sim::Rng| crate::sim::Ran {
        exit: crate::Exit {
            code: Some(3),
            signal: None,
            stdout: command.stdin.clone(),
            stderr: b"err\n".to_vec(),
            traced: None,
            peak_memory: 0,
        },
        takes: Duration::from_millis(1),
    });
    let simulated = processes_and_sockets(&mut sim);
    assert_eq!(
        simulated,
        [
            "exited Some(3) None \"in\" \"err\\n\"",
            "hung up",
            "mapped [112, 97, 99, 107, 101, 100]",
            "received \"ping\", then the end",
            "sent, 0 bytes back",
            "unlistened",
            "written",
        ]
    );
    let root = socket_dir("threads-processes");
    assert_eq!(simulated, processes_and_sockets(&mut Threads::new(&root, 2)));
    #[cfg(target_os = "linux")]
    {
        let root = socket_dir("uring-processes");
        let ring = processes_and_sockets(&mut crate::uring::Uring::new(&root).unwrap());
        assert_eq!(simulated, ring);
    }
    #[cfg(target_os = "macos")]
    {
        let root = socket_dir("kqueue-processes");
        let kqueue = processes_and_sockets(&mut crate::kqueue::Kqueue::new(&root, 2).unwrap());
        assert_eq!(simulated, kqueue);
    }
}

/// A sandbox asked for of a driver given nothing to make one is refused, and one given a
/// `Confine` runs what it says and hands back its record.
#[cfg(unix)]
#[test]
fn a_sandbox_is_made_by_the_confine_or_refused() {
    use std::sync::Arc;

    use crate::{Command, Confine, Confined, Sandbox, Traced};

    struct Shell;
    impl Confine for Shell {
        fn confine(&self, command: &Command, scratch: &std::path::Path) -> Result<Confined, String> {
            assert!(scratch.is_dir());
            let mut args = vec!["-c".into(), "printf confined; \"$0\" \"$@\"".into()];
            args.push(command.program.clone().into_os_string());
            args.extend(command.args.iter().cloned());
            Ok(Confined {
                program: "/bin/sh".into(),
                args,
                env: command.env.clone(),
                dir: command.dir.clone(),
            })
        }
        fn traced(&self, _: &Command, scratch: &std::path::Path) -> Result<Traced, String> {
            assert!(scratch.is_dir());
            let env = vec![("HOME".into(), None)];
            Ok(Traced { env, ..Traced::default() })
        }
    }
    fn command() -> Box<Command> {
        Box::new(Command {
            program: "/bin/echo".into(),
            args: vec![" ok".into()],
            env: Vec::new(),
            dir: "/".into(),
            stdin: Vec::new(),
            sandbox: Sandbox::On { read: Vec::new(), write: Vec::new(), network: false, trace: true, services: Vec::new() },
            memory: None,
        })
    }
    fn run(driver: &mut impl Driver) -> Done {
        let mut effects = vec![Effect::Spawn { command: command(), tag: Tag(1) }];
        let mut done = None;
        run_until_idle(driver, &mut effects, |it, _| done = Some(it.done));
        done.expect("the spawn ended")
    }
    let check = |root: &std::path::Path, refused: Done, confined: Done| {
        assert!(matches!(&refused, Done::Failed(why) if why.0.contains("Confine")), "{refused:?}");
        let Done::Exited(exit) = confined else { panic!("{confined:?}") };
        assert_eq!((exit.code, exit.stdout.as_slice()), (Some(0), &b"confined ok\n"[..]));
        assert_eq!(
            exit.traced.expect("traced").env,
            vec![(std::ffi::OsString::from("HOME"), None)],
        );
        assert!(!root.join(".blaze-spawn").read_dir().is_ok_and(|mut it| it.next().is_some()));
    };
    let root = dir("unconfined");
    check(
        &root,
        run(&mut Threads::new(&root, 1)),
        run(&mut Threads::new(&root, 1).confined(Arc::new(Shell))),
    );
    #[cfg(target_os = "linux")]
    {
        let root = dir("uring-unconfined");
        check(
            &root,
            run(&mut crate::uring::Uring::new(&root).unwrap()),
            run(&mut crate::uring::Uring::new(&root).unwrap().confined(Arc::new(Shell))),
        );
    }
    #[cfg(target_os = "macos")]
    {
        let root = dir("kqueue-unconfined");
        check(
            &root,
            run(&mut crate::kqueue::Kqueue::new(&root, 1).unwrap()),
            run(&mut crate::kqueue::Kqueue::new(&root, 1).unwrap().confined(Arc::new(Shell))),
        );
    }
}

#[test]
fn an_answer_is_its_bytes_and_a_confine_hands_nothing_unless_it_says() {
    use crate::{Answer, Command, Confine, Confined, Handed, Sandbox, Traced};
    let answers = [Answer { run: 0, code: 0 }, Answer { run: u64::MAX, code: -1 }, Answer { run: 7, code: 125 }];
    for answer in answers {
        assert_eq!(Answer::from_bytes(answer.to_bytes()), answer);
    }
    assert_eq!(Answer { run: 1, code: 2 }.to_bytes(), [1, 0, 0, 0, 0, 0, 0, 0, 2, 0, 0, 0]);
    struct Cold;
    impl Confine for Cold {
        fn confine(&self, _: &Command, _: &std::path::Path) -> Result<Confined, String> {
            Err("no".into())
        }
        fn traced(&self, _: &Command, _: &std::path::Path) -> Result<Traced, String> {
            Ok(Traced::default())
        }
    }
    let command = Command {
        program: "x".into(),
        args: Vec::new(),
        env: Vec::new(),
        dir: ".".into(),
        stdin: Vec::new(),
        sandbox: Sandbox::On { read: Vec::new(), write: Vec::new(), network: false, trace: false, services: Vec::new() },
        memory: None,
    };
    assert!(Cold.warm().is_none());
    let handed = Handed { run: 1, stdin: 0, stdout: 0, stderr: 0, job: 0 };
    assert!(Cold.hand(&command, std::path::Path::new("."), &handed).is_err());
}

#[test]
fn a_mapping_is_the_file() {
    let mut sim = Sim::new(7, Faults::default());
    sim.change(&name("store/pack"), b"packed");
    let mut effects = vec![Effect::Map { file: name("store/pack"), tag: Tag(1) }];
    let mut mapped = None;
    run_until_idle(&mut sim, &mut effects, |done, _| match done.done {
        Done::Mapped(it) => mapped = Some(it),
        other => panic!("{other:?}"),
    });
    assert_eq!(mapped.expect("mapped").bytes(), b"packed");
}

/// Rounds that grow a driver's slabs, queues and buffers (and its threads') to a cycle's
/// working set before the measured ones.
const WARM: u64 = 8;
/// Measured rounds of a warm cycle.
const ROUNDS: u64 = 50;

/// Polls until `out` holds `n` completions.
fn poll_for(driver: &mut impl Driver, out: &mut Vec<Completion>, n: usize) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while out.len() < n {
        assert!(Instant::now() < deadline, "{} of {n} completions: {out:?}", out.len());
        driver.poll(out, Duration::from_millis(50));
    }
}

/// `rounds` of a Sync, a Stat and a Read of the file `f`: what they allocated on the polling
/// thread.
fn file_rounds(driver: &mut impl Driver, rounds: u64) -> u64 {
    let file = name("f");
    let (mut effects, mut out) = (Vec::with_capacity(3), Vec::with_capacity(16));
    let mut used = 0;
    for round in 0..rounds {
        // The caller's: made before the cycle.
        effects.extend([
            Effect::Sync { file: file.clone(), tag: Tag(10) },
            Effect::Stat { file: file.clone(), tag: Tag(11) },
            Effect::Read { file: file.clone(), at: 1, len: 3, tag: Tag(12) },
        ]);
        let ((), cycle) = blaze_alloc_count::measure(|| {
            driver.submit(&mut effects);
            poll_for(driver, &mut out, 3);
        });
        used += cycle.allocations;
        out.sort_by_key(|it| it.tag);
        assert!(
            matches!(
                out.as_slice(),
                [
                    Completion { done: Done::Synced, .. },
                    Completion { done: Done::Stat(Some(_)), .. },
                    Completion { done: Done::Read(bytes), .. },
                ] if bytes == b"yte"
            ),
            "round {round}: {out:?}"
        );
        out.clear();
    }
    assert_eq!(driver.in_flight(), 0);
    used
}

/// A Sync, a Stat and a Read, once the driver has grown to them: on `Sim` only the Read's
/// `Vec<u8>`; on `Threads` nothing on the polling thread, and on its file thread only the
/// Read's `Vec<u8>`.
#[test]
fn a_warm_file_cycle_allocates_only_the_reads_bytes() {
    fn write(driver: &mut impl Driver) {
        let mut effects = vec![Effect::Write { file: name("f"), bytes: b"bytes".to_vec(), tag: Tag(1) }];
        run_until_idle(driver, &mut effects, |it, _| assert_eq!(it.done, Done::Written));
    }
    let mut sim = Sim::new(11, Faults::default());
    write(&mut sim);
    file_rounds(&mut sim, WARM);
    let used = file_rounds(&mut sim, ROUNDS);
    assert!(
        used <= ROUNDS,
        "the simulation: {used} allocations over {ROUNDS} warm cycles; each may make the Read's Vec<u8> only"
    );
    let root = dir("budget-files");
    let mut threads = Threads::new(&root, 1);
    write(&mut threads);
    file_rounds(&mut threads, WARM);
    threads.off_thread();
    let used = file_rounds(&mut threads, ROUNDS);
    assert_eq!(used, 0, "threads: {ROUNDS} warm Sync, Stat and Read cycles, on the polling thread");
    let (off, payload) = threads.off_thread();
    assert!(
        off - payload <= ROUNDS,
        "threads: the file thread made {} allocations over {ROUNDS} warm cycles; each may make the Read's Vec<u8> only",
        off - payload
    );
    drop(threads);
    std::fs::remove_dir_all(root).unwrap();
}

/// `rounds` of a timer armed and fired: what they allocated on the polling thread.
fn timer_rounds(driver: &mut impl Driver, rounds: u64) -> u64 {
    let (mut effects, mut out) = (Vec::with_capacity(1), Vec::with_capacity(4));
    let mut used = 0;
    for round in 0..rounds {
        let ((), cycle) = blaze_alloc_count::measure(|| {
            effects.push(Effect::Timer { after: Duration::ZERO, tag: Tag(round) });
            driver.submit(&mut effects);
            poll_for(driver, &mut out, 1);
        });
        used += cycle.allocations;
        assert_eq!(out.as_slice(), [Completion { tag: Tag(round), done: Done::Fired }]);
        out.clear();
    }
    used
}

#[test]
fn a_warm_timer_arm_and_fire_allocates_nothing() {
    let mut sim = Sim::new(12, Faults::default());
    timer_rounds(&mut sim, WARM);
    assert_eq!(timer_rounds(&mut sim, 10 * ROUNDS), 0, "the simulation: warm timers armed and fired");
    let root = dir("budget-timers");
    let mut threads = Threads::new(&root, 1);
    timer_rounds(&mut threads, WARM);
    threads.off_thread();
    assert_eq!(timer_rounds(&mut threads, 10 * ROUNDS), 0, "threads: warm timers armed and fired");
    assert_eq!(threads.off_thread(), (0, 0), "threads: a timer is the polling thread's alone");
    drop(threads);
    std::fs::remove_dir_all(root).unwrap();
}

/// `rounds` of a watch of `src` added, a change under it (`edit`, the world's, not counted)
/// heard, and the watch removed: what they allocated on the polling thread.
fn watch_rounds<D: Driver>(driver: &mut D, rounds: u64, mut edit: impl FnMut(&mut D)) -> u64 {
    let (src, edited) = (name("src"), name("src/edit"));
    let (mut effects, mut out) = (Vec::with_capacity(1), Vec::with_capacity(64));
    let mut used = 0;
    for round in 0..rounds {
        // The caller's: made before the cycle.
        effects.push(Effect::Watch { dir: src.clone(), tag: Tag(7) });
        let ((), added) = blaze_alloc_count::measure(|| driver.submit(&mut effects));
        used += added.allocations;
        let (mut heard, deadline) = (false, Instant::now() + Duration::from_secs(10));
        while !heard {
            assert!(Instant::now() < deadline, "round {round}: the change was never heard");
            edit(driver);
            let (_, polled) = blaze_alloc_count::measure(|| driver.poll(&mut out, Duration::from_millis(50)));
            used += polled.allocations;
            for it in out.drain(..) {
                match it.done {
                    Done::Changed { files, .. } => heard |= files.contains(&edited),
                    other => panic!("round {round}: {other:?}"),
                }
            }
        }
        effects.push(Effect::Unwatch { tag: Tag(7) });
        let ((), removed) = blaze_alloc_count::measure(|| {
            driver.submit(&mut effects);
            let deadline = Instant::now() + Duration::from_secs(10);
            while !matches!(out.last(), Some(Completion { done: Done::Unwatched, .. })) {
                assert!(Instant::now() < deadline, "round {round}: the watch never ended");
                driver.poll(&mut out, Duration::from_millis(50));
            }
        });
        used += removed.allocations;
        for it in out.drain(..) {
            assert!(matches!(it.done, Done::Changed { .. } | Done::Unwatched), "round {round}: {it:?}");
        }
        assert_eq!(driver.in_flight(), 0, "round {round}");
    }
    used
}

/// A watch added, a change heard and the watch removed, once the driver keeps a stopped watch
/// for the next: nothing allocated by the driver beyond the names `Done::Changed` carries.
#[test]
fn a_warm_watch_add_change_and_remove_allocates_only_the_changes() {
    let mut sim = Sim::new(13, Faults::default());
    // The simulated world changed, which makes the change's payload.
    let edit = |sim: &mut Sim| sim.change(&name("src/edit"), b"edited");
    watch_rounds(&mut sim, WARM, edit);
    assert_eq!(watch_rounds(&mut sim, ROUNDS, edit), 0, "the simulation: warm watch cycles");
    assert_eq!(sim.stale(), 0, "no change raced an end");
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        let root = dir("budget-watch");
        std::fs::create_dir_all(root.join("src")).unwrap();
        let path = root.join("src/edit");
        let edit = |_: &mut Threads| std::fs::write(&path, b"edited").unwrap();
        let mut threads = Threads::new(&root, 1);
        watch_rounds(&mut threads, 2, edit);
        threads.off_thread();
        assert_eq!(watch_rounds(&mut threads, 4, edit), 0, "threads: warm watch cycles, on the polling thread");
        let (off, payload) = threads.off_thread();
        assert_eq!(
            off - payload,
            0,
            "threads: the watch's errand or callbacks made {off} allocations, {payload} of them the changes' names"
        );
        drop(threads);
        std::fs::remove_dir_all(root).unwrap();
    }
}

/// A listen at `at` as `Tag(1)` and a connect to it as `connect`: the accepted end and the
/// connecting one.
#[cfg(unix)]
fn connected(driver: &mut impl Driver, at: &crate::Address, connect: Tag, listen: bool) -> (crate::Conn, crate::Conn) {
    let mut effects = Vec::with_capacity(2);
    if listen {
        effects.push(Effect::Listen { at: at.clone(), tag: Tag(1) });
    }
    effects.push(Effect::Connect { at: at.clone(), tag: connect });
    driver.submit(&mut effects);
    let mut out = Vec::with_capacity(4);
    poll_for(driver, &mut out, 2);
    let (mut server, mut client) = (None, None);
    for it in out {
        match it.done {
            Done::Accepted { conn } if it.tag == Tag(1) => server = Some(conn),
            Done::Connected { conn } if it.tag == connect => client = Some(conn),
            other => panic!("{other:?}"),
        }
    }
    (server.expect("accepted"), client.expect("connected"))
}

/// `rounds` of "ping" sent on `client` and received on `server`, the send's buffer and the
/// receive's (`buffers`) handed back by each round and given to the next: what they allocated
/// on the polling thread.
#[cfg(unix)]
fn send_rounds(
    driver: &mut impl Driver,
    (server, client): (crate::Conn, crate::Conn),
    rounds: u64,
    buffers: &mut [Vec<u8>; 2],
) -> u64 {
    let (mut effects, mut out) = (Vec::with_capacity(2), Vec::with_capacity(16));
    let mut used = 0;
    for round in 0..rounds {
        let [sending, receiving] = buffers;
        sending.extend_from_slice(b"ping");
        receiving.clear();
        let ((), cycle) = blaze_alloc_count::measure(|| {
            effects.push(Effect::Send { conn: client, bytes: std::mem::take(sending), tag: Tag(10) });
            effects.push(Effect::Recv { conn: server, most: 64, into: std::mem::take(receiving), tag: Tag(11) });
            driver.submit(&mut effects);
            poll_for(driver, &mut out, 2);
        });
        used += cycle.allocations;
        out.sort_by_key(|it| it.tag);
        match &mut out[..] {
            [Completion { done: Done::Sent(sent), .. }, Completion { done: Done::Read(read), .. }]
                if sent.is_empty() && read == b"ping" =>
            {
                *buffers = [std::mem::take(sent), std::mem::take(read)];
            }
            other => panic!("round {round}: {other:?}"),
        }
        out.clear();
    }
    used
}

/// Both ends of a connection hung up, and a new connection made in their slots: the old
/// `Conn`s name nothing, a receive and a send on them fail with their buffers back, and the
/// new one carries bytes.
#[cfg(unix)]
fn hung_up_conns_name_nothing(driver: &mut impl Driver, at: &crate::Address, (server, client): (crate::Conn, crate::Conn)) {
    let mut effects = vec![Effect::Hangup { conn: client, tag: Tag(20) }, Effect::Hangup { conn: server, tag: Tag(21) }];
    let mut out = Vec::new();
    driver.submit(&mut effects);
    poll_for(driver, &mut out, 2);
    assert!(out.iter().all(|it| it.done == Done::HungUp), "{out:?}");
    out.clear();
    let again = connected(driver, at, Tag(3), false);
    assert!(again.0 != server && again.0 != client && again.1 != server && again.1 != client);
    effects.extend([
        Effect::Recv { conn: client, most: 8, into: b"kept".to_vec(), tag: Tag(30) },
        Effect::Send { conn: server, bytes: b"x".to_vec(), tag: Tag(31) },
        Effect::Hangup { conn: client, tag: Tag(32) },
    ]);
    driver.submit(&mut effects);
    poll_for(driver, &mut out, 3);
    out.sort_by_key(|it| it.tag);
    assert!(
        matches!(
            &out[..],
            [
                Completion { done: Done::Broken { buffer: kept, .. }, .. },
                Completion { done: Done::Broken { buffer: sent, .. }, .. },
                Completion { done: Done::Failed(_), .. },
            ] if kept == b"kept" && sent.is_empty() && sent.capacity() >= 1
        ),
        "stale: {out:?}"
    );
    out.clear();
    send_rounds(driver, again, 1, &mut Default::default());
    effects.extend([
        Effect::Hangup { conn: again.0, tag: Tag(40) },
        Effect::Hangup { conn: again.1, tag: Tag(41) },
        Effect::Unlisten { tag: Tag(1) },
    ]);
    let mut done = BTreeMap::new();
    run_until_idle(driver, &mut effects, |it, _| _ = done.insert(it.tag.0, it.done));
    assert_eq!(done, BTreeMap::from([(1, Done::Unlistened), (40, Done::HungUp), (41, Done::HungUp)]));
}

/// A send on one end and a receive on the other of a warm connection, each buffer handed back
/// and given to the next: nothing allocated, on `Sim` by the poll and on `Threads` by the
/// errands or on its polling thread. Then the connection's `Conn`s are stale once hung up.
#[cfg(unix)]
#[test]
fn a_warm_send_and_receive_allocates_nothing() {
    let at = crate::Address::Unix(name("s"));
    let mut sim = Sim::new(14, Faults::default());
    let conns = connected(&mut sim, &at, Tag(2), true);
    let mut buffers = Default::default();
    send_rounds(&mut sim, conns, WARM, &mut buffers);
    let used = send_rounds(&mut sim, conns, ROUNDS, &mut buffers);
    assert_eq!(used, 0, "the simulation: {ROUNDS} warm sends and receives, their buffers reused");
    hung_up_conns_name_nothing(&mut sim, &at, conns);
    let root = dir("budget-conn");
    let mut threads = Threads::new(&root, 1);
    let conns = connected(&mut threads, &at, Tag(2), true);
    let mut buffers = Default::default();
    send_rounds(&mut threads, conns, WARM, &mut buffers);
    threads.off_thread();
    let used = send_rounds(&mut threads, conns, ROUNDS, &mut buffers);
    assert_eq!(used, 0, "threads: {ROUNDS} warm sends and receives, on the polling thread");
    let (off, payload) = threads.off_thread();
    assert_eq!((off, payload), (0, 0), "threads: the errands' allocations, and of them the receives' buffers");
    hung_up_conns_name_nothing(&mut threads, &at, conns);
    drop(threads);
    std::fs::remove_dir_all(root).unwrap();
}

/// A mapping copied reads as the file mapped with the same bytes, on a sixteen-byte boundary
/// like the file's page, at every length around a block's; equal bytes are equal mappings, and
/// only a clone is the same one.
#[test]
fn a_copied_mapping_reads_as_the_file_mapped() {
    let root = dir("mapping");
    for len in [0usize, 1, 15, 16, 17, 31, 4097] {
        let bytes: Vec<u8> = (0..len).map(|at| (at * 7 + 3) as u8).collect();
        let path = root.join(len.to_string());
        std::fs::write(&path, &bytes).unwrap();
        let file = crate::fs::map(&path).unwrap();
        let copied = crate::Mapping::copy(&bytes);
        assert_eq!((file.bytes(), copied.bytes()), (&bytes[..], &bytes[..]), "{len} bytes");
        assert!((copied.bytes().as_ptr() as usize).is_multiple_of(16), "{len} bytes");
        assert_eq!(file, copied, "{len} bytes");
        assert!(!file.same(&copied) && file.same(&file.clone()) && copied.same(&copied.clone()));
        assert!(!copied.same(&crate::Mapping::copy(&bytes)));
    }
    std::fs::remove_dir_all(root).unwrap();
}

/// An asker names a file in each effect it makes of it (a journal's appends and syncs, the
/// store's reads): a name's clone is a count of the one made, so those effects allocate nothing
/// for their names.
#[test]
fn a_file_name_cloned_into_effects_allocates_nothing() {
    assert!(blaze_alloc_count::installed());
    let file = name("state/journal");
    let mut effects = Vec::with_capacity(64);
    blaze_alloc_count::assert_at_most(0, "a name in each of 64 effects", || {
        for round in 0..32 {
            effects.push(Effect::Sync { file: file.clone(), tag: Tag(2 * round) });
            effects.push(Effect::ReadAll { file: file.clone(), tag: Tag(2 * round + 1) });
        }
        effects.clear();
    });
    let copy = file.clone();
    assert_eq!(copy, file);
    assert_eq!(copy.as_str().as_ptr(), file.as_str().as_ptr(), "one name, shared");
}
