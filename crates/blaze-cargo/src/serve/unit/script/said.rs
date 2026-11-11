//! What a sandboxed run of a build script read and who read it, said as events of the build's
//! profile, so "what did this script's last run do" is one command (`blaze explain script
//! UNIT`, crates/blaze-cargo/src/cli/observe/script.rs) and nothing of it lives only in the
//! run's scratch directory, which is gone when the run is.
//!
//! Two kinds of event, each of the target `blaze` with the run's unit as the profile names it
//! everywhere else (`spans.unit`, `processes.unit`: `libc v0.2.155 (build script run)`):
//!
//! - [`calls::EVENT_RUN`], once a run that ran: how many files, directories and variables it read, what
//!   it read that can't be traced, and the first [`NAMED`] of each by name.
//! - [`calls::EVENT_CALL`], once a call its sandbox described ([`blaze_usercode::calls`]): a read that
//!   can't be traced (`what` is `random`, `clock`, `network`) or a program started (`exec`),
//!   with the process, its program and `argv[0]`, the system call, where its code is and how
//!   far the process had come. [`calls::EVENT_CUT`] counts those past the sandbox's bound.
//!
//! The run's exit, time and CPU are its row of `processes`, and its whole time its `build
//! script run` span: the events say only what no other row does.

use std::fmt::Write as _;

use blaze_build_script as bs;
use blaze_core::Traced;
use blaze_usercode::{
    calls::{self, Call},
    trace,
};

/// The events' messages, which `tracing` takes as literals where they are said: the reader
/// asks for them by [`calls::EVENT_RUN`], [`calls::EVENT_CALL`] and [`calls::EVENT_CUT`], and
/// a test holds the two to each other.
#[cfg(test)]
const MESSAGES: [&str; 3] = [calls::EVENT_RUN, calls::EVENT_CALL, calls::EVENT_CUT];

/// The names of each kind a run's event lists, at most, and the bytes they take, at most: a
/// script that reads a compiler's whole include tree is counted, and its first files named.
const NAMED: usize = 64;
const NAMED_BYTES: usize = 8 << 10;

/// The first of `names` a line each, up to [`NAMED`] of them and [`NAMED_BYTES`]; a list cut
/// short ends in a line saying how many more there are.
fn named<'a>(names: impl ExactSizeIterator<Item = std::borrow::Cow<'a, str>>) -> String {
    let all = names.len();
    // alloc: once a run that ran, a list of its event
    let mut out = String::with_capacity(NAMED_BYTES.min(all * 48));
    let mut listed = 0;
    for name in names.take(NAMED) {
        if out.len() + name.len() + 1 > NAMED_BYTES {
            break;
        }
        if listed > 0 {
            out.push('\n');
        }
        out.push_str(&name);
        listed += 1;
    }
    if listed < all {
        // Writing to a `String` does not fail.
        _ = write!(out, "{}({} more)", if listed > 0 { "\n" } else { "" }, all - listed);
    }
    out
}

/// Says the events of `unit`'s sandboxed run that just ended having read `traced`, whole (asked
/// before the run's key takes anything out of it), with the calls its sandbox described
/// ([`calls::of`] its program). Inside the run's span, so each event names it.
pub(super) fn run(unit: &bs::Unit, traced: &Traced) {
    // alloc: once a run that ran, the name its events carry
    let name = format!("{} ({})", unit.package.descr(), super::RUN);
    let name = name.as_str();
    let described = calls::of(&unit.script);
    let made = || {
        described.iter().filter_map(|it| match it {
            Call::Made(it) => Some(it),
            Call::More(_) => None,
        })
    };
    let (mut execs, mut reads) = (0u64, 0u64);
    for it in made() {
        match it.what == calls::EXEC {
            true => execs += 1,
            false => reads += 1,
        }
    }
    // alloc: once a run that ran, a field of its event
    let untraceable: Vec<&str> =
        traced.untraceable.iter().map(|it| trace::untraceable_name(*it)).collect();
    let untraceable = untraceable.join(", "); // alloc: as above
    let files = named(traced.files.iter().map(|it| it.0.to_string_lossy()));
    let dirs = named(traced.dirs.iter().map(|it| it.0.to_string_lossy()));
    let env = named(traced.env.iter().map(|it| match it.0 == trace::ALL_ENV_NAME {
        true => std::borrow::Cow::Borrowed("(every variable: it read them all)"),
        false => it.0.to_string_lossy(),
    }));
    tracing::info!(
        target: "blaze",
        unit = name,
        program = %unit.script.display(),
        files = traced.files.len() as u64,
        dirs = traced.dirs.len() as u64,
        env = traced.env.len() as u64,
        untraceable = untraceable.as_str(),
        calls = reads,
        execs,
        files_named = files.as_str(),
        dirs_named = dirs.as_str(),
        env_named = env.as_str(),
        "script run traced"
    );
    for it in made() {
        // alloc: once a call described, its address as text (a u64 past i64 is no number in SQL)
        let ip = format!("{:#x}", it.ip);
        tracing::info!(
            target: "blaze",
            unit = name,
            what = it.what.as_str(),
            pid = it.pid,
            inside_pid = it.inside_pid.map_or(-1, i64::from),
            program = it.program.as_str(),
            argv0 = it.argv0.as_str(),
            syscall = it.syscall.as_str(),
            number = it.number,
            phase = it.phase.name(),
            before_main = match it.phase.before_main() {
                Some(true) => "yes (a heuristic)",
                Some(false) => "no (a heuristic)",
                None => "not known",
            },
            ip = ip.as_str(),
            module = it.module.as_str(),
            module_kind = it.kind.name(),
            "script call"
        );
    }
    let more: u64 = described
        .iter()
        .map(|it| match it {
            Call::More(n) => *n,
            Call::Made(_) => 0,
        })
        .sum();
    if more > 0 {
        tracing::info!(target: "blaze", unit = name, more, "script calls not described");
    }
}

#[cfg(test)]
mod tests {
    use std::borrow::Cow;

    use super::*;

    /// The messages the events are written with are the ones `blaze explain script` reads by.
    /// The unit's name is the one the run's span and its `processes` row carry.
    #[test]
    fn the_events_messages_are_the_ones_the_reader_asks_for() {
        let source = include_str!("said.rs");
        for message in MESSAGES {
            // Once where it is said; this test names it only through the constant.
            assert_eq!(source.matches(&format!("\"{message}\"")).count(), 1, "{message}");
        }
    }

    #[test]
    fn a_list_names_its_first_and_counts_the_rest() {
        let names = |n: usize| (0..n).map(|it| Cow::Owned(format!("/src/f{it}.h")));
        assert_eq!(named(names(0)), "");
        assert_eq!(named(names(2)), "/src/f0.h\n/src/f1.h");
        let cut = named(names(NAMED + 5));
        assert_eq!(cut.lines().count(), NAMED + 1, "{cut}");
        assert!(cut.ends_with("\n(5 more)"), "{cut}");
        // Names past the bytes a list takes are counted, not written.
        let long = "x".repeat(NAMED_BYTES);
        let over = named([Cow::Borrowed("/a"), Cow::Borrowed(long.as_str())].into_iter());
        assert_eq!(over, "/a\n(1 more)");
        let only = named([Cow::Borrowed(long.as_str())].into_iter());
        assert_eq!(only, "(1 more)");
    }
}
