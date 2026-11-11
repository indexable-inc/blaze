//! The one way the daemon writes a line to its log (its standard error, `serve.log`): as a line
//! there and as an event of the profile, so nothing the daemon says is wrong lives in a log
//! file alone (docs/v2/plan/design/debugging.md, "Problems").
//!
//! An event at WARN or ERROR is a row of `events` whatever its target
//! (`blaze_profile::logs::keeps_event`), tied to the build under way, and a row of the
//! `problems` view, which `blaze explain problems`, `blaze slow` and `blaze why` list first. On
//! 2026-11-10 the daemon wrote "the results of `build` were not kept after its answer was sent"
//! to `serve.log` after every build for hours; where a line had no event, or its event no
//! reader, no command said it.
//!
//! [`logged!`] takes the event's level, then what `eprintln!` takes. A line that already has an
//! event of its own beside it (one with fields) keeps its `eprintln!` and says so with
//! `// logged: <which event>` on the line before; the guard below refuses any other
//! `eprintln!` in the daemon's code.

/// Writes one line to the daemon's log and records it as an event of level `$level` (`ERROR`,
/// `WARN` or `INFO`) of the target `blaze`: `logged!(WARN, "blaze serve: {what}")`.
macro_rules! logged {
    ($level:ident, $($arg:tt)*) => {{
        // alloc: a line of the daemon's log, once a line
        let line = format!($($arg)*);
        eprintln!("{line}");
        tracing::event!(target: "blaze", tracing::Level::$level, "{line}");
    }};
}
pub(crate) use logged;

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    /// The files under `dir`, sorted, so the guard names the same line first on every machine.
    fn sources(dir: &Path, out: &mut Vec<PathBuf>) {
        let mut entries: Vec<PathBuf> = std::fs::read_dir(dir)
            .unwrap_or_else(|error| panic!("{}: {error}", dir.display()))
            .map(|it| it.unwrap().path())
            .collect();
        entries.sort();
        for path in entries {
            if path.is_dir() {
                sources(&path, out);
            } else if path.extension().is_some_and(|it| it == "rs") {
                out.push(path);
            }
        }
    }

    /// The lines of `text` that write to standard error with no event: an `eprintln!` or
    /// `eprint!` whose line, or the line before, does not say `// logged:`.
    fn unlogged(text: &str) -> Vec<usize> {
        let lines: Vec<&str> = text.lines().collect();
        let mut out = Vec::new();
        for (at, line) in lines.iter().enumerate() {
            let code = line.split("//").next().unwrap_or("");
            if !(code.contains("eprintln!(") || code.contains("eprint!(")) {
                continue;
            }
            let marked = line.contains("// logged:")
                || at.checked_sub(1).is_some_and(|it| lines[it].contains("// logged:"));
            if !marked {
                out.push(at + 1);
            }
        }
        out
    }

    /// The invariant: every line the daemon writes to its log is an event of the profile too.
    /// Held by reading the daemon's sources: tests, this file and `once.rs` (the seed's engine
    /// in the client's process, whose standard error is the terminal) aside.
    #[test]
    fn the_daemon_writes_no_line_to_its_log_that_is_not_an_event() {
        let serve = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/serve");
        let mut files = Vec::new();
        sources(&serve, &mut files);
        assert!(files.len() > 20, "the daemon's sources are under {}", serve.display());
        let mut found = Vec::new();
        for file in files {
            let name = file.strip_prefix(&serve).unwrap().to_string_lossy().into_owned();
            if name == "said.rs" || name == "once.rs" || name.ends_with("tests.rs") || name.contains("/tests/") {
                continue;
            }
            let text = std::fs::read_to_string(&file).unwrap();
            // A file's own `mod tests` is past its code: what is after it is a test's.
            let code = text.split("#[cfg(test)]\nmod tests {").next().unwrap_or(&text);
            for line in unlogged(code) {
                found.push(format!("crates/blaze-cargo/src/serve/{name}:{line}"));
            }
        }
        assert!(
            found.is_empty(),
            "these lines write to the daemon's log with no event, so no command says them \
             (`blaze explain problems`): write them with `said::logged!(LEVEL, ...)`, or mark \
             the event that says the same with `// logged:` on the line before:\n{}",
            found.join("\n")
        );
    }

    #[test]
    fn the_oracle_catches_a_line_written_to_the_log_alone() {
        let bad = "fn f() {\n    eprintln!(\"blaze serve: lost\");\n}\n";
        assert_eq!(unlogged(bad), [2]);
        let marked = "fn f() {\n    tracing::warn!(\"lost\");\n    // logged: the warn above\n    eprintln!(\"blaze serve: lost\");\n}\n";
        assert_eq!(unlogged(marked), Vec::<usize>::new());
        let comment = "// eprintln!(\"x\") is banned here\n";
        assert_eq!(unlogged(comment), Vec::<usize>::new());
    }
}
