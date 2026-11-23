//! `blaze check [dir]`: the one checker is the execution engine (engine_check.rs over
//! crates/blaze-engine and front2). Rule 30, one concept, one home: before this, `check` ran the
//! old checker (main.rs `request`, whole-crate tasks, body_types.rs) while the engine was a
//! second command, `engine-check`, and every fix was weighed against two paths (a bug lived in
//! the seam between them on 2026-11-17). Now `check` and `engine-check` (its old name, an alias)
//! both enter here, and the old checker is gone (2026-11-19, with `check-old` and main.rs
//! `request`). Never a fallback (rule 3).
//!
//! The arguments are `check`'s (main.rs `flags`, one parser). Each flag of `check`:
//!
//! | flag | on the engine |
//! |---|---|
//! | `[dir]` | taken (engine_check.rs `serve`: the checkout) |
//! | `--workers N` | taken (`Opts::workers`) |
//! | `--color`, `--view` | taken (the printer's abilities and view, print.rs `Shown`) |
//! | `--each` | taken (`Shown::each`: each consequence and borrow note drawn) |
//! | `--status-every S` | taken (engine_status.rs; the engine's alone) |
//! | `--fix <selector>..` | taken (engine_check.rs `serve`: a quiet check, fix.rs `apply`, a second check, through main.rs `fix_then_check`; the listed findings are in the request's frame, which outlives both checks) |
//! | `--stats` | taken (engine_check.rs `stats_line`: one line on stderr; the profile holds the rest) |
//! | `--origin`, `--origin-key` | taken (`Opts::origin`: the store opened with the named origin) |
//! | `--grant`, `-p`/`--bin`, `--open`, `-- args` | REFUSED: `run`'s, never `check`'s |
//!
//! A flag of `check` the engine does not take would be listed in [`NOT_TAKEN`] and refused as
//! [`crate::CliError::NotOnEngine`], naming the flag and why, before anything is read or
//! written; none is now.

use bstd::entry::Status;
use bstd::out::Out;
use bstd::Cx;

use crate::CliError;
use crate::phases::Phases;

/// The flags of `check` the engine does not honour yet, each with why: refused by name. Each
/// is a `(flag, why, given)`: whether `flags` gave it. None now: `--fix`, `--stats`,
/// `--origin` and `--origin-key` are taken (the table above).
pub(crate) const NOT_TAKEN: [(&str, &str, fn(&crate::Flags<'_>) -> bool); 0] = [];

/// The command: `check`'s arguments refused where the engine does not honour them, then the
/// engine's check (engine_check.rs `run`), which prints its findings, notes and closing line
/// through print.rs and returns the status main.rs `STATUSES` explains.
#[expect(clippy::too_many_arguments, reason = "the command's edge: each argument one specific capability")]
pub(crate) fn run(
    cx: &Cx,
    args: &bstd::args::Args,
    fs: &bstd::fs::Fs,
    env: Option<&bstd::env::Env>,
    net: Option<&bstd::net::Net>,
    host: &bstd::host::Host,
    clock: &bstd::clock::Clock,
    rng: &mut bstd::rng::Rng,
    out: &Out,
    err: &Out,
    phases: &mut Phases,
    heap: &'static blaze_own::Heap,
    spawn_daemon: bool,
) -> Result<Status, CliError> {
    let flags = crate::flags(args)?;
    refuse_not_taken(&flags)?;
    let status = crate::engine_check::run(cx, args, fs, env, net, host, clock, rng, out, err, phases, heap)?;
    if spawn_daemon && !cx.is_simulated() && !flags.dir.is_some_and(crate::single_file::is_script_arg) {
        let frame = cx.frame();
        if let Ok(checkout) = crate::checkout::checkout(flags.dir, frame)
            && let Ok(located) = crate::run_package::locate(cx, fs, &checkout, frame)
            && let Ok(served) = crate::engine_script_reads::Served::at_edge(false, located.at, frame)
            && let Some(root_abs) = served.root_text()
        {
            crate::daemon_client::spawn_detached_daemon(cx, host, env, root_abs, frame);
        }
    }
    Ok(status)
}

/// The first flag given that the engine's check does not take, refused by name (rule 3: never
/// quietly ignored).
pub(crate) fn refuse_not_taken(flags: &crate::Flags<'_>) -> Result<(), CliError> {
    if let Some((flag, why, _)) = NOT_TAKEN.into_iter().find(|(_, _, given)| given(flags)) {
        return Err(CliError::NotOnEngine { flag, why });
    }
    if !flags.grants.is_empty() || !flags.program.is_empty() {
        return Err(CliError::Usage("--grant and the words after -- are `blaze run`'s program's"));
    }
    if flags.select != crate::run_package::Select::NONE {
        return Err(CliError::Usage("-p and --bin choose `blaze run`'s or `blaze export`'s program"));
    }
    if flags.output.is_some() {
        return Err(CliError::Usage("-o / --output is `blaze export`'s"));
    }
    if flags.open == blaze_run::viewer::OpenExplore::Open {
        return Err(CliError::Usage("--open is `blaze run`'s"));
    }
    Ok(())
}
