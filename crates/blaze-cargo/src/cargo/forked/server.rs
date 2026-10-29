//! The fork servers and the scheduler of [`super`], on Unix.

use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    fs::File,
    io::{BufRead as _, BufReader},
    os::{
        fd::{AsRawFd as _, RawFd},
        unix::{net::UnixStream, process::CommandExt as _},
    },
    path::{Path, PathBuf},
    process::{Child, Command},
    sync::mpsc,
    time::{Duration, Instant},
};

use anyhow::Context as _;

/// A test as a server lists it.
pub struct Listed {
    pub name: String,
    /// `#[ignore]`d and not asked for: not run, with its message.
    pub ignored: Option<String>,
}

/// A program of tests running as a fork server.
pub struct Server {
    child: Child,
    socket: UnixStream,
    /// The tests it lists, by their index there.
    pub tests: Vec<Listed>,
    /// How many tests the program has, those its filters left out too.
    pub total: usize,
    /// What it said when it ended before its tests did.
    lines: Option<BufReader<UnixStream>>,
}

impl Server {
    /// Starts the command `make` gives for the descriptor the server is to talk to blaze on
    /// (which it puts in [`VAR`], in its environment or its sandbox's), running `program`,
    /// and reads its list of tests.
    pub fn start(
        make: impl FnOnce(RawFd) -> anyhow::Result<Command>,
        program: &Path,
        state: &super::super::state::State,
    ) -> anyhow::Result<Server> {
        let (ours, theirs) = UnixStream::pair().context("making a socket for a fork server")?;
        let fd = theirs.as_raw_fd();
        let mut command = make(fd)?;
        // The descriptor is the child's alone: made close-on-exec, it is opened up only in
        // the child, between its fork and its exec, so no other program started meanwhile
        // holds it.
        // SAFETY: `fcntl` is async-signal-safe.
        unsafe {
            command.pre_exec(move || match libc::fcntl(fd, libc::F_SETFD, 0) {
                0 => Ok(()),
                _ => Err(std::io::Error::last_os_error()),
            })
        };
        command.stdin(std::process::Stdio::null());
        let child = {
            let _held = state.hold();
            super::super::run::started_running(&mut command, program)
        }
        .with_context(|| format!("could not execute {}", program.display()))?;
        drop(theirs);
        let mut lines = BufReader::new(ours.try_clone()?);
        let (mut tests, mut total) = (Vec::new(), None);
        let mut line = String::new();
        while total.is_none() {
            line.clear();
            if lines.read_line(&mut line)? == 0 {
                anyhow::bail!(
                    "{} ended before it listed its tests, which blaze's harness does first",
                    program.display()
                );
            }
            let line = line.trim_end_matches('\n');
            match line.split_once(' ') {
                Some(("t", name)) => tests.push(Listed { name: name.to_owned(), ignored: None }),
                Some(("i", rest)) => {
                    let (name, message) = rest.split_once('\t').unwrap_or((rest, ""));
                    tests.push(Listed { name: name.to_owned(), ignored: Some(message.to_owned()) });
                }
                Some(("e", count)) => total = count.parse().ok(),
                Some(("f", why)) => {
                    anyhow::bail!(
                        "the tests of {} can't run, each in a process: {why}",
                        program.display()
                    )
                }
                _ => anyhow::bail!("{} said `{line}` as its tests' fork server", program.display()),
            }
        }
        Ok(Server { child, socket: ours, tests, total: total.unwrap_or(0), lines: Some(lines) })
    }

    /// Sends the server `op` for the run `id` of the test at `index`, with `fd`.
    fn send(&self, op: u32, id: u32, index: u32, fd: Option<RawFd>) -> std::io::Result<()> {
        let mut data = [0u8; 12];
        data[..4].copy_from_slice(&op.to_ne_bytes());
        data[4..8].copy_from_slice(&id.to_ne_bytes());
        data[8..].copy_from_slice(&index.to_ne_bytes());
        let mut iov = libc::iovec { iov_base: data.as_mut_ptr().cast(), iov_len: data.len() };
        let mut control = [0u64; 8];
        // SAFETY: every field of `msghdr` is an integer or a pointer.
        let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        if let Some(fd) = fd {
            msg.msg_control = control.as_mut_ptr().cast();
            // SAFETY: CMSG_SPACE is arithmetic.
            msg.msg_controllen = unsafe { libc::CMSG_SPACE(4) } as _;
            // SAFETY: `control` has room for one header and one descriptor, aligned.
            unsafe {
                let cmsg = libc::CMSG_FIRSTHDR(&msg);
                (*cmsg).cmsg_level = libc::SOL_SOCKET;
                (*cmsg).cmsg_type = libc::SCM_RIGHTS;
                (*cmsg).cmsg_len = libc::CMSG_LEN(4) as _;
                std::ptr::write_unaligned(libc::CMSG_DATA(cmsg).cast::<libc::c_int>(), fd);
            }
        }
        loop {
            // SAFETY: `msg` points at what it says.
            let sent = unsafe { libc::sendmsg(self.socket.as_raw_fd(), &msg, 0) };
            match sent {
                12 => return Ok(()),
                n if n < 0 => {
                    let error = std::io::Error::last_os_error();
                    if error.kind() != std::io::ErrorKind::Interrupted {
                        return Err(error);
                    }
                }
                _ => return Err(std::io::Error::other("a fork server took part of a request")),
            }
        }
    }

    /// Tells the server no more tests come, and waits for it to end once those running have.
    fn end(&mut self) -> std::io::Result<std::process::ExitStatus> {
        _ = self.socket.shutdown(std::net::Shutdown::Write);
        self.child.wait()
    }
}

/// How a test's process ended.
pub enum End {
    Passed,
    /// It failed: libtest said so (101), or the process ended otherwise, as said.
    Failed(Option<String>),
    TimedOut(Duration),
}

/// A test the scheduler ran, of the job it was queued with.
pub struct Ran {
    pub name: String,
    pub end: End,
    pub output: Vec<u8>,
    pub took: Duration,
}

/// A test to run: of `job`, on its server `server`, at `index` of that server's list.
pub struct Queued {
    pub job: usize,
    pub server: usize,
    pub index: u32,
}

/// What a job (a test executable) holds while its tests run: its servers, by their index in
/// the scheduler's, and how many of its tests are still queued or running.
pub struct Job {
    pub servers: Vec<usize>,
    pub left: usize,
    pub timeout: Option<Duration>,
    pub ran: Vec<Ran>,
}

/// What reaches the scheduler from the servers' readers.
enum Event {
    Ended { server: usize, id: u32, status: i32 },
    Gone { server: usize, said: Option<String> },
}

/// The most of a test's output kept to show: its end.
const OUTPUT_KEPT: u64 = 4 << 20;

/// A test running: its job, its name, its output file, when it started and whether it was
/// killed for its time.
struct Running {
    job: usize,
    server: usize,
    name: String,
    output: File,
    started: Instant,
    killed: Option<Duration>,
}

/// Runs the tests of `queue`, `jobs` at once, on `servers`: in the order queued, and after a
/// test fails no more are started unless `no_fail_fast`. As the last test of a job ends, its
/// servers are told to end and are waited for, and `done` is given the job's index, what ran
/// and whether its servers ended well; once every test that started has ended, so is every
/// job that has not been. `scratch` is where the output files are made.
pub fn schedule(
    mut servers: Vec<Server>,
    mut jobs_of: Vec<Job>,
    queue: Vec<Queued>,
    jobs: usize,
    no_fail_fast: bool,
    scratch: &Path,
    mut done: impl FnMut(usize, Job, bool),
) -> anyhow::Result<()> {
    std::fs::create_dir_all(scratch).with_context(|| format!("making {}", scratch.display()))?;
    let (tx, rx) = mpsc::channel();
    for (at, server) in servers.iter_mut().enumerate() {
        let Some(mut lines) = server.lines.take() else { continue };
        let tx = tx.clone();
        std::thread::spawn(move || {
            let mut line = String::new();
            let mut said = None;
            loop {
                line.clear();
                match lines.read_line(&mut line) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {}
                }
                let mut words = line.trim_end().splitn(3, ' ');
                match (words.next(), words.next(), words.next()) {
                    (Some("x"), Some(id), Some(status)) => {
                        let (Ok(id), Ok(status)) = (id.parse(), status.parse()) else { continue };
                        if tx.send(Event::Ended { server: at, id, status }).is_err() {
                            return;
                        }
                    }
                    (Some("f"), ..) => said = Some(line.trim_end()[2..].to_owned()),
                    _ => {}
                }
            }
            _ = tx.send(Event::Gone { server: at, said });
        });
    }
    drop(tx);
    let mut queue: VecDeque<Queued> = queue.into();
    let mut running: BTreeMap<(usize, u32), Running> = BTreeMap::new();
    let mut next_id: u32 = 0;
    let mut stopped = false;
    let mut gone: BTreeSet<usize> = BTreeSet::new();
    let mut ended: BTreeSet<usize> = BTreeSet::new();
    let jobs = jobs.max(1);
    let finish_job = |job: usize,
                      servers: &mut Vec<Server>,
                      jobs_of: &mut Vec<Job>,
                      ended: &mut BTreeSet<usize>,
                      done: &mut dyn FnMut(usize, Job, bool)| {
        if !ended.insert(job) {
            return;
        }
        let mut well = true;
        for &server in &jobs_of[job].servers {
            well &= servers[server].end().is_ok_and(|it| it.success());
        }
        let taken = std::mem::replace(
            &mut jobs_of[job],
            Job { servers: Vec::new(), left: 0, timeout: None, ran: Vec::new() },
        );
        done(job, taken, well);
    };
    for job in 0..jobs_of.len() {
        if jobs_of[job].left == 0 {
            finish_job(job, &mut servers, &mut jobs_of, &mut ended, &mut done);
        }
    }
    loop {
        while running.len() < jobs && !stopped {
            let Some(next) = queue.pop_front() else { break };
            if gone.contains(&next.server) {
                let name = servers[next.server].tests[next.index as usize].name.clone();
                let ran = Ran {
                    name,
                    end: End::Failed(Some("its fork server had ended".to_owned())),
                    output: Vec::new(),
                    took: Duration::ZERO,
                };
                jobs_of[next.job].ran.push(ran);
                jobs_of[next.job].left -= 1;
                if jobs_of[next.job].left == 0 {
                    finish_job(next.job, &mut servers, &mut jobs_of, &mut ended, &mut done);
                }
                continue;
            }
            let id = next_id;
            next_id += 1;
            let path = scratch.join(format!("out-{id}"));
            let output = File::options()
                .read(true)
                .write(true)
                .create_new(true)
                .open(&path)
                .with_context(|| format!("making {}", path.display()))?;
            _ = std::fs::remove_file(&path);
            let server = &servers[next.server];
            let name = server.tests[next.index as usize].name.clone();
            server
                .send(1, id, next.index, Some(output.as_raw_fd()))
                .with_context(|| format!("asking a fork server to run {name}"))?;
            running.insert(
                (next.server, id),
                Running {
                    job: next.job,
                    server: next.server,
                    name,
                    output,
                    started: Instant::now(),
                    killed: None,
                },
            );
        }
        if running.is_empty() {
            break;
        }
        let now = Instant::now();
        let deadline = running
            .values()
            .filter(|it| it.killed.is_none())
            .filter_map(|it| Some(it.started + jobs_of[it.job].timeout?))
            .min();
        let event = match deadline {
            Some(at) => match rx.recv_timeout(at.saturating_duration_since(now)) {
                Ok(event) => Some(event),
                Err(mpsc::RecvTimeoutError::Timeout) => None,
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    anyhow::bail!("every fork server ended with tests running")
                }
            },
            None => Some(rx.recv().context("every fork server ended with tests running")?),
        };
        let Some(event) = event else {
            let now = Instant::now();
            for (&(server, id), it) in running.iter_mut() {
                let Some(limit) = jobs_of[it.job].timeout else { continue };
                if it.killed.is_none() && now >= it.started + limit {
                    it.killed = Some(limit);
                    _ = servers[server].send(2, id, 0, None);
                }
            }
            continue;
        };
        let ended_now: Vec<(Running, Option<i32>)> = match event {
            Event::Ended { server, id, status } => {
                running.remove(&(server, id)).map(|it| (it, Some(status))).into_iter().collect()
            }
            Event::Gone { server, said } => {
                gone.insert(server);
                if let Some(said) = said {
                    super::super::say::error(&format!("a fork server ended: {said}"));
                }
                let keys: Vec<(usize, u32)> =
                    running.keys().filter(|it| it.0 == server).copied().collect();
                keys.into_iter()
                    .filter_map(|key| running.remove(&key))
                    .map(|it| (it, None))
                    .collect()
            }
        };
        for (it, status) in ended_now {
            let took = it.started.elapsed();
            let output = own_output(&read_output(&it.output));
            let end = match (it.killed, status) {
                (Some(limit), _) => End::TimedOut(limit),
                (None, None) => End::Failed(Some("its fork server ended".to_owned())),
                (None, Some(status)) => ended_with(status),
            };
            if !matches!(end, End::Passed) && !no_fail_fast {
                stopped = true;
            }
            let _ = it.server;
            let job = it.job;
            jobs_of[job].ran.push(Ran { name: it.name, end, output, took });
            jobs_of[job].left -= 1;
            if jobs_of[job].left == 0 {
                finish_job(job, &mut servers, &mut jobs_of, &mut ended, &mut done);
            }
        }
    }
    for job in 0..jobs_of.len() {
        finish_job(job, &mut servers, &mut jobs_of, &mut ended, &mut done);
    }
    Ok(())
}

/// What a test's process wrote, at most the last [`OUTPUT_KEPT`] bytes of it.
fn read_output(file: &File) -> Vec<u8> {
    use std::os::unix::fs::FileExt as _;
    let len = file.metadata().map_or(0, |it| it.len());
    let start = len.saturating_sub(OUTPUT_KEPT);
    let mut out = vec![0; (len - start) as usize];
    let mut at = 0;
    while at < out.len() {
        match file.read_at(&mut out[at..], start + at as u64) {
            Ok(0) | Err(_) => break,
            Ok(n) => at += n,
        }
    }
    out.truncate(at);
    out
}

/// What a test wrote of what its process wrote: all but libtest's own report of the one test,
/// which the child gives as JSON lines (`--format json`) so it is told apart from the test's
/// output whatever that is. libtest's line follows a test's last write, which may not end
/// its line.
fn own_output(written: &[u8]) -> Vec<u8> {
    const LIBTESTS: &[u8] = b"{ \"type\": \"";
    let mut out = Vec::with_capacity(written.len());
    for line in written.split_inclusive(|&it| it == b'\n') {
        let text = line.strip_suffix(b"\n").unwrap_or(line);
        let libtests = (0..text.len()).rev().find(|&at| {
            text[at..].starts_with(LIBTESTS)
                && serde_json::from_slice::<serde_json::Value>(&text[at..]).is_ok_and(|it| {
                    matches!(it.get("type").and_then(|it| it.as_str()), Some("suite" | "test"))
                })
        });
        match libtests {
            Some(at) => out.extend_from_slice(&text[..at]),
            None => out.extend_from_slice(line),
        }
    }
    out
}

/// How a process that ended with the wait status `status` did: libtest exits 0 when its test
/// passed and 101 when it failed.
fn ended_with(status: i32) -> End {
    if status == -1 {
        return End::Failed(Some("its fork server could not start it".to_owned()));
    }
    if libc::WIFEXITED(status) {
        return match libc::WEXITSTATUS(status) {
            0 => End::Passed,
            101 => End::Failed(None),
            code => End::Failed(Some(format!("its process exited with {code}"))),
        };
    }
    if libc::WIFSIGNALED(status) {
        let signal = libc::WTERMSIG(status);
        return End::Failed(Some(format!(
            "its process was killed by signal {signal}{}",
            match signal {
                libc::SIGSEGV => " (SIGSEGV)",
                libc::SIGABRT => " (SIGABRT)",
                libc::SIGBUS => " (SIGBUS)",
                libc::SIGKILL => " (SIGKILL)",
                _ => "",
            }
        )));
    }
    End::Failed(Some(format!("its process ended with the status {status}")))
}

/// How to show a run of one executable's tests: as libtest's pretty output, or terse (`-q`),
/// with or without colors, and with the output of the tests that passed (`--show-output`,
/// `--nocapture`).
pub struct Shown {
    pub terse: bool,
    pub colored: bool,
    pub passed_output: bool,
}

impl Shown {
    pub fn of(args: &[String], quiet: bool) -> Shown {
        use std::io::IsTerminal as _;
        let has = |names: &[&str]| args.iter().any(|it| names.contains(&it.as_str()));
        let color =
            args.iter().enumerate().find_map(|(at, it)| match it.strip_prefix("--color=") {
                Some(value) => Some(value.to_owned()),
                None if it == "--color" => args.get(at + 1).cloned(),
                None => None,
            });
        let colored = match color.as_deref() {
            Some("always") => true,
            Some("never") => false,
            _ => std::io::stdout().is_terminal(),
        };
        Shown {
            terse: quiet || has(&["-q", "--quiet", "--format=terse"]),
            colored,
            passed_output: has(&["--show-output", "--nocapture", "--no-capture"]),
        }
    }
}

/// libtest's report of a run, from what ran (`ran`), what was ignored, and how many tests
/// were left out (`filtered`): each test's line, the output of each that failed, the names of
/// those that failed, and `test result:`. `failed_too` are tests that failed for something
/// else than how their process ended (what the sandbox denied them), with why.
pub fn report(
    ran: &[Ran],
    ignored: &[(String, String)],
    filtered: usize,
    failed_too: &BTreeMap<String, String>,
    took: Duration,
    shown: &Shown,
) -> String {
    use std::fmt::Write as _;
    let paint = |code: &str, text: &str| match shown.colored {
        true => format!("\x1b[{code}m{text}\x1b[0m"),
        false => text.to_owned(),
    };
    let (ok, failed, ignored_word) =
        (paint("32", "ok"), paint("31", "FAILED"), paint("33", "ignored"));
    let mut lines: Vec<(&str, String)> = Vec::new();
    let mut failures: Vec<&Ran> = Vec::new();
    let mut passed = 0;
    for it in ran {
        let failed_here = !matches!(it.end, End::Passed) || failed_too.contains_key(&it.name);
        let word = match (&it.end, failed_here) {
            (End::TimedOut(_), _) => paint("31", "TIMEOUT"),
            (_, true) => failed.clone(),
            (_, false) => ok.clone(),
        };
        match failed_here {
            true => failures.push(it),
            false => passed += 1,
        }
        lines.push((&it.name, word));
    }
    for (name, message) in ignored {
        let word = match message.is_empty() {
            true => ignored_word.clone(),
            false => format!("{ignored_word}, {message}"),
        };
        lines.push((name, word));
    }
    lines.sort_by(|a, b| a.0.cmp(b.0));
    let total = ran.len() + ignored.len();
    let mut out = String::new();
    _ = writeln!(out, "\nrunning {total} test{}", if total == 1 { "" } else { "s" });
    match shown.terse {
        true => {
            for (_, word) in &lines {
                let plain = without_colors(word);
                out.push(match plain.as_str() {
                    "ok" => '.',
                    _ if plain.starts_with("ignored") => 'i',
                    _ => 'F',
                });
            }
            out.push('\n');
        }
        false => {
            for (name, word) in &lines {
                _ = writeln!(out, "test {name} ... {word}");
            }
        }
    }
    let shown_output = |out: &mut String, it: &Ran, why: Option<String>| {
        _ = writeln!(out, "\n---- {} stdout ----", it.name);
        let text = String::from_utf8_lossy(&it.output);
        out.push_str(&text);
        if !text.is_empty() && !text.ends_with('\n') {
            out.push('\n');
        }
        if let Some(why) = why {
            _ = writeln!(out, "{why}");
        }
    };
    if shown.passed_output {
        let passes: Vec<&Ran> =
            ran.iter().filter(|it| !failures.iter().any(|f| f.name == it.name)).collect();
        if !passes.is_empty() {
            out.push_str("\nsuccesses:\n");
            for it in &passes {
                shown_output(&mut out, it, None);
            }
            out.push_str("\nsuccesses:\n");
            for it in &passes {
                _ = writeln!(out, "    {}", it.name);
            }
        }
    }
    failures.sort_by(|a, b| a.name.cmp(&b.name));
    if !failures.is_empty() {
        out.push_str("\nfailures:\n");
        for it in &failures {
            let why = match &it.end {
                End::TimedOut(limit) => {
                    Some(format!("blaze: it took more than its {:.1} s", limit.as_secs_f64()))
                }
                End::Failed(Some(why)) => Some(format!("blaze: {why}")),
                _ => failed_too.get(&it.name).map(|why| format!("blaze: {why}")),
            };
            shown_output(&mut out, it, why);
        }
        out.push_str("\nfailures:\n");
        for it in &failures {
            _ = writeln!(out, "    {}", it.name);
        }
    }
    let result = match failures.is_empty() {
        true => ok,
        false => failed,
    };
    _ = writeln!(
        out,
        "\ntest result: {result}. {passed} passed; {} failed; {} ignored; 0 measured; {filtered} filtered out; finished in {:.2}s\n",
        failures.len(),
        ignored.len(),
        took.as_secs_f64()
    );
    out
}

fn without_colors(text: &str) -> String {
    let mut out = String::new();
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            for c in chars.by_ref() {
                if c.is_ascii_alphabetic() {
                    break;
                }
            }
            continue;
        }
        out.push(c);
    }
    out
}

/// Where a command keeps its tests' output files: removed as each is made, so nothing stays.
pub fn scratch() -> PathBuf {
    std::env::temp_dir().join(format!("blaze-test-output-{}", std::process::id()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tests_output_is_what_it_wrote_without_libtests_report() {
        let written = concat!(
            "{ \"type\": \"suite\", \"event\": \"started\", \"test_count\": 1 }\n",
            "{ \"type\": \"test\", \"event\": \"started\", \"name\": \"tests::fails\" }\n",
            "some output\n",
            "{ \"type\": \"not libtest's\" }\n",
            "thread 'tests::fails' panicked at src/lib.rs:7:51:\n",
            "no newline",
            "{ \"type\": \"test\", \"name\": \"tests::fails\", \"event\": \"failed\" }\n",
            "{ \"type\": \"suite\", \"event\": \"failed\", \"passed\": 0, \"failed\": 1 }\n",
        );
        assert_eq!(
            String::from_utf8(own_output(written.as_bytes())).unwrap(),
            "some output\n{ \"type\": \"not libtest's\" }\nthread 'tests::fails' panicked at src/lib.rs:7:51:\nno newline"
        );
    }
}
