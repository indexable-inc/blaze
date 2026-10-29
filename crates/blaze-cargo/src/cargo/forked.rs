//! A process for each test, as cargo nextest runs them, from a fork server (#1443).
//!
//! Each test executable blaze built starts once, in its sandbox when it has one, with
//! [`VAR`] naming its end of a socket: the harness blaze writes (`test_harness.rs` in
//! blaze-engine) then lists its tests, as libtest's own filters pick them from its arguments,
//! and runs none itself. For each test, blaze sends the server a file for its output and the
//! server forks a copy-on-write child that runs that one test with libtest and exits; the
//! server says how each child ended. A scheduler here runs the tests of every executable of
//! the command on all the cores at once, one queue for all of them, with each test's output
//! kept to itself, a time limit for each test when one is set, and the command's
//! `--no-fail-fast`.
//!
//! The server never starts a thread, so its fork is sound on Linux and macOS alike; one that
//! has threads before its first test (a static constructor's) says so and the command fails.
//! A package that needs its tests to share one process (a static one test sets and another
//! reads, a logger set up once) takes them back to libtest's threads in [`TABLE`] of its
//! manifest (`threads = true`), and `BLAZE_TEST_PROCESSES=one` does it for every test of a
//! command. Elsewhere than Linux and macOS, and for harness flags it does not take, the
//! command stops and says so: there is no quiet way back to threads.

use std::{path::Path, time::Duration};

use anyhow::Context as _;

#[cfg(unix)]
mod server;
#[cfg(unix)]
pub use server::*;

/// The variable that tells a program of tests it is a fork server, with the number of its
/// descriptor of the socket to blaze. The server takes it out of its environment before any
/// test runs.
pub const VAR: &str = "__BLAZE_TEST_SERVER";

/// The variable that runs every test of a command as libtest's threads in one process of its
/// executable, as `cargo test` does: `one`. `per-test`, the default, runs a process for each.
pub const PROCESSES: &str = "BLAZE_TEST_PROCESSES";

/// The variable that gives every test of a command a time limit, in seconds, past which it is
/// killed and fails.
pub const TIMEOUT: &str = "BLAZE_TEST_TIMEOUT";

/// The table of a package's manifest that says how its tests run.
pub const TABLE: &str = "[package.metadata.blaze.tests]";

/// What a package's manifest says of how its tests run, in [`TABLE`].
#[derive(Default, Clone)]
pub struct Options {
    /// `threads = true`: its tests run as libtest's threads in one process.
    pub threads: bool,
    /// `timeout`: the most each test may take.
    pub timeout: Option<Duration>,
}

/// What the manifest of the package in `package_dir` says in [`TABLE`]. A key it does not
/// know, or a value of another type, is an error.
pub fn options(package_dir: &Path) -> anyhow::Result<Options> {
    let manifest = package_dir.join("Cargo.toml");
    let text = std::fs::read_to_string(&manifest)
        .with_context(|| format!("reading {}", manifest.display()))?;
    let value: toml::Table =
        toml::from_str(&text).with_context(|| format!("reading {}", manifest.display()))?;
    let table = ["package", "metadata", "blaze", "tests"]
        .iter()
        .try_fold(&value, |table, key| table.get(*key)?.as_table());
    let mut out = Options::default();
    let Some(table) = table else { return Ok(out) };
    let wrong = |what: &str| anyhow::anyhow!("{TABLE} in {}: {what}", manifest.display());
    for (key, value) in table {
        match key.as_str() {
            "threads" => {
                out.threads = value.as_bool().ok_or_else(|| wrong("`threads` is true or false"))?;
            }
            "timeout" => {
                let seconds = match value {
                    toml::Value::Integer(it) => *it as f64,
                    toml::Value::Float(it) => *it,
                    _ => return Err(wrong("`timeout` is a number of seconds")),
                };
                out.timeout = Some(
                    seconds_of(seconds)
                        .ok_or_else(|| wrong("`timeout` is a number of seconds more than 0"))?,
                );
            }
            other => {
                return Err(wrong(&format!(
                    "`{other}` is not known (`threads` and `timeout` are)"
                )));
            }
        }
    }
    Ok(out)
}

fn seconds_of(seconds: f64) -> Option<Duration> {
    (seconds.is_finite() && seconds > 0.0).then(|| Duration::from_secs_f64(seconds))
}

/// The time limit [`TIMEOUT`] gives every test of the command, if any.
pub fn command_timeout() -> anyhow::Result<Option<Duration>> {
    let Ok(text) = std::env::var(TIMEOUT) else { return Ok(None) };
    let seconds: f64 = text
        .trim()
        .parse()
        .map_err(|_| anyhow::anyhow!("{TIMEOUT}={text}: a number of seconds"))?;
    seconds_of(seconds)
        .map(Some)
        .ok_or_else(|| anyhow::anyhow!("{TIMEOUT}={text}: a number of seconds more than 0"))
}

/// Whether [`PROCESSES`] has every test of the command run as libtest's threads; any value
/// but `one` and `per-test` is an error.
pub fn one_process() -> anyhow::Result<bool> {
    match std::env::var(PROCESSES).as_deref() {
        Err(_) | Ok("per-test") => Ok(false),
        Ok("one") => Ok(true),
        Ok(other) => anyhow::bail!("{PROCESSES}={other}: `per-test` (the default) or `one`"),
    }
}

/// Why the harness flags `args` can't be given to a fork server, `None` when they can: it
/// takes libtest's filters (names, `--exact`, `--skip`, `--ignored`, `--include-ignored`,
/// `--exclude-should-panic`) and what only shows the run, which blaze shows itself.
pub fn refused_flag(args: &[String]) -> Option<String> {
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        let flag = arg.split_once('=').map_or(arg.as_str(), |it| it.0);
        match flag {
            "--exact"
            | "--ignored"
            | "--include-ignored"
            | "--exclude-should-panic"
            | "-q"
            | "--quiet"
            | "--nocapture"
            | "--no-capture"
            | "--show-output" => {}
            "--skip" | "--test-threads" | "--color" if flag != arg => {}
            "--skip" | "--test-threads" | "--color" => {
                args.next()?;
            }
            "--format" if matches!(arg.as_str(), "--format=pretty" | "--format=terse") => {}
            "--format" if flag == arg => match args.next().map(String::as_str) {
                Some("pretty" | "terse") => {}
                _ => return Some("--format".to_owned()),
            },
            _ if !arg.starts_with('-') => {}
            _ => return Some(flag.to_owned()),
        }
    }
    None
}

/// How many tests run at once: the harness's `--test-threads`, else `RUST_TEST_THREADS`, else
/// as many as the machine has cores.
pub fn jobs(args: &[String]) -> usize {
    let mut given = None;
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        if let Some(n) = arg.strip_prefix("--test-threads=") {
            given = n.parse().ok();
        } else if arg == "--test-threads" {
            given = it.next().and_then(|n| n.parse().ok());
        }
    }
    given
        .or_else(|| std::env::var("RUST_TEST_THREADS").ok()?.parse().ok())
        .filter(|it| *it > 0)
        .unwrap_or_else(|| std::thread::available_parallelism().map_or(1, |it| it.get()))
}

#[cfg(all(test, unix))]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    fn args(of: &[&str]) -> Vec<String> {
        of.iter().map(|it| (*it).to_owned()).collect()
    }

    #[test]
    fn a_fork_server_takes_libtests_filters_and_what_shows_the_run() {
        assert_eq!(refused_flag(&args(&["a::b", "--exact", "--skip", "c", "--ignored"])), None);
        assert_eq!(refused_flag(&args(&["--test-threads", "4", "--color=never", "-q"])), None);
        assert_eq!(refused_flag(&args(&["--format", "terse"])), None);
        assert_eq!(refused_flag(&args(&["--format", "json"])), Some("--format".to_owned()));
        assert_eq!(refused_flag(&args(&["--bench"])), Some("--bench".to_owned()));
        assert_eq!(refused_flag(&args(&["--logfile=x"])), Some("--logfile".to_owned()));
        assert_eq!(jobs(&args(&["--test-threads=3"])), 3);
        assert_eq!(jobs(&args(&["--test-threads", "5"])), 5);
    }

    #[test]
    fn a_package_says_how_its_tests_run() {
        let dir = std::env::temp_dir().join(format!("blaze-forked-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let write = |text: &str| std::fs::write(dir.join("Cargo.toml"), text).unwrap();
        write("[package]\nname = \"a\"\n");
        let it = options(&dir).unwrap();
        assert!(!it.threads && it.timeout.is_none());
        write(
            "[package]\nname = \"a\"\n[package.metadata.blaze.tests]\nthreads = true\ntimeout = 2.5\n",
        );
        let it = options(&dir).unwrap();
        assert!(it.threads && it.timeout == Some(Duration::from_millis(2500)));
        write("[package]\nname = \"a\"\n[package.metadata.blaze.tests]\nthread = true\n");
        assert!(options(&dir).is_err());
        write("[package]\nname = \"a\"\n[package.metadata.blaze.tests]\ntimeout = 0\n");
        assert!(options(&dir).is_err());
        _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_report_reads_as_libtests() {
        let ran = vec![
            Ran {
                name: "b".into(),
                end: End::Failed(None),
                output: b"boom\n".to_vec(),
                took: Duration::ZERO,
            },
            Ran { name: "a".into(), end: End::Passed, output: Vec::new(), took: Duration::ZERO },
        ];
        let shown = Shown { terse: false, colored: false, passed_output: false };
        let text = report(
            &ran,
            &[("c".into(), String::new())],
            2,
            &BTreeMap::new(),
            Duration::ZERO,
            &shown,
        );
        assert!(
            text.contains(
                "running 3 tests\ntest a ... ok\ntest b ... FAILED\ntest c ... ignored\n"
            )
        );
        assert!(text.contains("\n---- b stdout ----\nboom\n"));
        assert!(text.contains("\nfailures:\n    b\n"));
        assert!(text.contains(
            "test result: FAILED. 1 passed; 1 failed; 1 ignored; 0 measured; 2 filtered out;"
        ));
    }
}
