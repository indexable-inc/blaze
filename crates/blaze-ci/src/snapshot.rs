//! The VM snapshots itself (docs/v2/plan/lanes/ci.md section 10, "no single point of failure"): after a
//! green run of the default branch or a release, once the newest snapshot is older than
//! [`SnapshotConfig::every_secs`], the daemon makes its state durable
//! ([`blaze_api::Request::Snapshot`]) and the configured command snapshots the VM. Beyond
//! [`SnapshotConfig::keep`], the oldest are removed. A standby ([`crate::standby`]) restores
//! the newest.
//!
//! A command that fails is an error naming it and its stderr's last line; nothing is kept, and
//! the next green run tries again.

use std::{fmt, process::Command};

use serde::{Deserialize, Serialize};

use crate::config::SnapshotConfig;

/// A snapshot taken, as the state file keeps it.
#[derive(Serialize, Deserialize, PartialEq, Eq, Debug)]
#[serde(deny_unknown_fields)]
pub struct Kept {
    /// What the command printed.
    pub id: String,
    pub name: String,
    /// Seconds since the epoch.
    pub at: u64,
}

#[derive(PartialEq, Eq, Debug)]
pub struct SnapshotError {
    pub what: String,
}

impl fmt::Display for SnapshotError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.what)
    }
}

impl std::error::Error for SnapshotError {}

/// Whether a snapshot is due at `now`.
pub fn due(kept: &[Kept], now: u64, every_secs: u64) -> bool {
    match kept.iter().map(|it| it.at).max() {
        Some(newest) => now.saturating_sub(newest) >= every_secs,
        None => true,
    }
}

/// The name of a snapshot of `commit` taken at `now`: the commit's first 12 characters (only
/// letters and digits) and the time.
pub fn name(commit: &str, now: u64) -> String {
    // alloc: once per snapshot, its name
    let short: String = commit.chars().filter(char::is_ascii_alphanumeric).take(12).collect();
    format!("blaze-ci-{short}-{now}") // alloc: once per snapshot, its name
}

/// The ids to remove so that `keep` remain, oldest first.
pub fn beyond(kept: &[Kept], keep: usize) -> Vec<String> {
    // alloc: once per snapshot, the kept ones by age
    let mut by_age: Vec<&Kept> = kept.iter().collect();
    by_age.sort_by(|a, b| a.at.cmp(&b.at).then_with(|| a.id.cmp(&b.id)));
    let over = by_age.len().saturating_sub(keep);
    // alloc: once per snapshot, the ids removed
    by_age.into_iter().take(over).map(|it| it.id.clone()).collect()
}

/// `argv` with every `{key}` in each argument replaced by `value`.
pub fn substitute(argv: &[String], key: &str, value: &str) -> Vec<String> {
    let pattern = format!("{{{key}}}"); // alloc: once per snapshot, its argv
    // alloc: once per snapshot, its argv
    argv.iter().map(|arg| arg.replace(&pattern, value)).collect()
}

/// The last line of `bytes` with something besides whitespace in it, trimmed.
pub fn last_line(bytes: &[u8]) -> Option<String> {
    String::from_utf8_lossy(bytes) // alloc: once per snapshot, the id it printed
        .lines()
        .rev()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(str::to_owned)
}

/// Runs `argv`, returning its stdout; a failure names the command and its stderr's last line.
pub fn run(argv: &[String]) -> Result<Vec<u8>, SnapshotError> {
    let shown = argv.join(" ");
    let Some((program, args)) = argv.split_first() else {
        return Err(SnapshotError { what: "an empty command".to_owned() }); // alloc: error path
    };
    let output = Command::new(program)
        .args(args)
        .stdin(std::process::Stdio::null())
        .output()
        // alloc: error path
        .map_err(|error| SnapshotError { what: format!("`{shown}` did not start: {error}") })?;
    if !output.status.success() {
        // alloc: error path
        let said = last_line(&output.stderr).unwrap_or_else(|| "nothing on stderr".to_owned());
        // alloc: error path
        return Err(SnapshotError { what: format!("`{shown}` failed ({}): {said}", output.status) });
    }
    Ok(output.stdout)
}

/// Runs `argv` and reads its stdout's last nonblank line as an id.
pub fn run_for_id(argv: &[String]) -> Result<String, SnapshotError> {
    let stdout = run(argv)?;
    last_line(&stdout).ok_or_else(|| SnapshotError {
        what: format!("`{}` succeeded and printed no id", argv.join(" ")), // alloc: error path
    })
}

/// Takes the VM's snapshot `name` with the configured command, once the daemon's state is
/// durable.
pub fn take(config: &SnapshotConfig, name: &str, now: u64) -> Result<Kept, SnapshotError> {
    let id = run_for_id(&substitute(&config.command, "name", name))?;
    Ok(Kept { id, name: name.to_owned(), at: now }) // alloc: once per snapshot, the one kept
}

/// Removes snapshot `id` with the configured command.
pub fn remove(config: &SnapshotConfig, id: &str) -> Result<(), SnapshotError> {
    run(&substitute(&config.remove, "id", id)).map(drop)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kept(id: &str, at: u64) -> Kept {
        Kept { id: id.to_owned(), name: format!("n-{id}"), at }
    }

    #[test]
    fn the_first_snapshot_is_due_and_then_every_interval() {
        assert!(due(&[], 0, 3600));
        let list = [kept("a", 100), kept("b", 1000)];
        assert!(!due(&list, 1000 + 3599, 3600));
        assert!(due(&list, 1000 + 3600, 3600));
    }

    #[test]
    fn the_oldest_are_removed_beyond_keep() {
        let list = [kept("c", 300), kept("a", 100), kept("b", 200), kept("d", 400)];
        assert_eq!(beyond(&list, 2), ["a", "b"]);
        assert_eq!(beyond(&list, 4), Vec::<String>::new());
        assert_eq!(beyond(&list, 9), Vec::<String>::new());
    }

    #[test]
    fn names_and_substitutions() {
        assert_eq!(name("0123456789abcdef", 77), "blaze-ci-0123456789ab-77");
        assert_eq!(name("../x;y", 1), "blaze-ci-xy-1");
        let argv = ["ix".to_owned(), "snap".to_owned(), "--name={name}".to_owned()];
        assert_eq!(substitute(&argv, "name", "s1"), ["ix", "snap", "--name=s1"]);
    }

    #[test]
    fn the_last_nonblank_line_is_the_id() {
        assert_eq!(last_line(b"progress\nsnap-42\n\n  \n").as_deref(), Some("snap-42"));
        assert_eq!(last_line(b" \n\n"), None);
    }

    #[test]
    fn needs_sh_a_failing_command_names_itself_and_its_stderr() {
        let argv: Vec<String> =
            ["sh", "-c", "echo first >&2; echo the last line >&2; exit 3"].map(str::to_owned).into();
        let error = run(&argv).unwrap_err();
        assert!(error.what.contains("sh -c"), "{error}");
        assert!(error.what.contains("the last line"), "{error}");
        let argv: Vec<String> = ["sh", "-c", "echo noise; echo snap-7"].map(str::to_owned).into();
        assert_eq!(run_for_id(&argv).unwrap(), "snap-7");
        let argv: Vec<String> = ["sh", "-c", "true"].map(str::to_owned).into();
        assert!(run_for_id(&argv).unwrap_err().what.contains("no id"));
    }
}
