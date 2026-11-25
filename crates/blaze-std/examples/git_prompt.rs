//! Starship-class Git prompt and floor benchmark written 100% in `bstd` (`bstd::git`).
//!
//! Run from any Git repository root:
//! `cargo +$(cat pins/toolchain) run --release -p blaze-std --features git --example git_prompt`

#![feature(macro_attr)]

use core::fmt::Write as _;

use bstd::{
    Cx,
    clock::Clock,
    entry::Error,
    fs::Fs,
    git::{PromptStyle, Repo, StatusOpts, UntrackedMode, format_status},
    mem::{Erased, Mem, String},
    out::Out,
};

#[bstd::main]
fn main(cx: &Cx, fs: Fs, clock: Clock, mem: Mem, out: Out) -> Result<(), Error> {
    let root = fs.root().read_only();
    let repo = match Repo::open(cx, &root) {
        Ok(r) => r,
        Err(e) => {
            let mut msg = String::new_in(cx.frame());
            let _ = write!(msg, "Repo::open failed: {e}");
            let _ = out.line(cx, msg.as_str());
            return Err(Error::new("Repo::open failed"));
        }
    };

    let mut line = String::new_in(cx.frame());
    let mut prompt = String::new_in(cx.frame());
    let mut summary = String::new_in(cx.frame());

    // Warm-up run + capture output & per-phase profile in its own bounded arena.
    let mut prof_line = String::new_in(cx.frame());
    {
        let arena = mem.arena("git-warmup", 128 << 20).map_err(|_| Error::new("arena"))?;
        let erased = Erased::new(arena.alloc());
        let (status, t) = match repo.status_profiled(cx, &clock, StatusOpts::STARSHIP, erased.handle()) {
            Ok(s) => s,
            Err(e) => {
                let mut msg = String::new_in(cx.frame());
                let _ = write!(msg, "Repo::status failed: {e}");
                let _ = out.line(cx, msg.as_str());
                return Err(Error::new("Repo::status failed"));
            }
        };
        let _ = write!(
            prof_line,
            "profile(starship): rounds={} machine={}us read_files={}us(n={}) read_stat={}us(n={}) list_dirs={}us(n={}) stat_paths={}us(n={}) links={}us ranges={}us",
            t.rounds,
            t.machine_us,
            t.read_files_us,
            t.read_files_count,
            t.read_with_stat_us,
            t.read_with_stat_count,
            t.list_dirs_us,
            t.list_dirs_count,
            t.stat_paths_us,
            t.stat_paths_count,
            t.read_links_us,
            t.read_ranges_us,
        );
        let _ = format_status(&status, &PromptStyle::STARSHIP, &mut prompt);
        let _ = write!(
            summary,
            "branch={:?} state={:?} upstream={:?} conflicted={} staged={}(+{} ~{} -{} T{}) mod={} del={} tc={} untracked={} stashed={}",
            status.head.branch_name(),
            status.state,
            status.upstream.as_ref().map(|u| (u.remote.as_str(), u.branch.as_str(), u.divergence)),
            status.counts.conflicted,
            status.counts.staged(),
            status.counts.staged_added,
            status.counts.staged_modified,
            status.counts.staged_deleted,
            status.counts.staged_typechanged,
            status.counts.modified,
            status.counts.deleted,
            status.counts.typechanged,
            status.counts.untracked,
            status.counts.stashed,
        );
    }

    // 1. Full Starship status (`UntrackedMode::Normal`, exact counts, ahead/behind).
    let rounds = 5usize;
    let mut full_us = [0u64; 5];
    let mut warm_starship_t = bstd::git::StatusTiming::default();
    for slot in &mut full_us {
        let arena = mem.arena("git-starship", 128 << 20).map_err(|_| Error::new("arena"))?;
        let start = clock.now();
        {
            let erased = Erased::new(arena.alloc());
            match repo.status_profiled(cx, &clock, StatusOpts::STARSHIP, erased.handle()) {
                Ok((_, t)) => warm_starship_t = t,
                Err(e) => {
                    let mut msg = String::new_in(cx.frame());
                    let _ = write!(msg, "starship round failed: {e}");
                    let _ = out.line(cx, msg.as_str());
                    return Err(Error::new("starship round failed"));
                }
            }
        }
        let end = clock.now();
        *slot = end.since(start).unwrap_or_default().as_micros() as u64;
    }
    full_us.sort_unstable();

    // 2. Fast `-uno` mode (`UntrackedMode::Off`).
    let mut uno_us = [0u64; 5];
    let mut warm_uno_t = bstd::git::StatusTiming::default();
    for slot in &mut uno_us {
        let arena = mem.arena("git-uno", 128 << 20).map_err(|_| Error::new("arena"))?;
        let start = clock.now();
        {
            let erased = Erased::new(arena.alloc());
            let opts = StatusOpts { untracked: UntrackedMode::Off, ..StatusOpts::STARSHIP };
            match repo.status_profiled(cx, &clock, opts, erased.handle()) {
                Ok((_, t)) => warm_uno_t = t,
                Err(e) => {
                    let mut msg = String::new_in(cx.frame());
                    let _ = write!(msg, "uno round failed: {e}");
                    let _ = out.line(cx, msg.as_str());
                    return Err(Error::new("uno round failed"));
                }
            }
        }
        let end = clock.now();
        *slot = end.since(start).unwrap_or_default().as_micros() as u64;
    }
    uno_us.sort_unstable();

    // 3. Fast presence-only mode (`StatusOpts::FAST_PRESENCE`).
    let mut pres_us = [0u64; 5];
    for slot in &mut pres_us {
        let arena = mem.arena("git-presence", 128 << 20).map_err(|_| Error::new("arena"))?;
        let start = clock.now();
        {
            let erased = Erased::new(arena.alloc());
            if let Err(e) = repo.status(cx, StatusOpts::FAST_PRESENCE, erased.handle()) {
                let mut msg = String::new_in(cx.frame());
                let _ = write!(msg, "presence round failed: {e}");
                let _ = out.line(cx, msg.as_str());
                return Err(Error::new("presence round failed"));
            }
        }
        let end = clock.now();
        *slot = end.since(start).unwrap_or_default().as_micros() as u64;
    }
    pres_us.sort_unstable();

    let _ = write!(line, "prompt: {prompt}");
    out.line(cx, line.as_str())?;
    out.line(cx, summary.as_str())?;
    prof_line.clear();
    let _ = write!(
        prof_line,
        "warm(starship): rounds={} machine={}us read_files={}us(n={}) read_stat={}us(n={}) list_dirs={}us(n={}) stat_paths={}us(n={})",
        warm_starship_t.rounds,
        warm_starship_t.machine_us,
        warm_starship_t.read_files_us,
        warm_starship_t.read_files_count,
        warm_starship_t.read_with_stat_us,
        warm_starship_t.read_with_stat_count,
        warm_starship_t.list_dirs_us,
        warm_starship_t.list_dirs_count,
        warm_starship_t.stat_paths_us,
        warm_starship_t.stat_paths_count,
    );
    out.line(cx, prof_line.as_str())?;
    prof_line.clear();
    let _ = write!(
        prof_line,
        "warm(uno):      rounds={} machine={}us read_files={}us(n={}) read_stat={}us(n={}) list_dirs={}us(n={}) stat_paths={}us(n={})",
        warm_uno_t.rounds,
        warm_uno_t.machine_us,
        warm_uno_t.read_files_us,
        warm_uno_t.read_files_count,
        warm_uno_t.read_with_stat_us,
        warm_uno_t.read_with_stat_count,
        warm_uno_t.list_dirs_us,
        warm_uno_t.list_dirs_count,
        warm_uno_t.stat_paths_us,
        warm_uno_t.stat_paths_count,
    );
    out.line(cx, prof_line.as_str())?;

    line.clear();
    let _ = write!(
        line,
        "median ({rounds} rounds): starship={}us ({:.2}ms) | uno={}us ({:.2}ms) | presence={}us ({:.2}ms)",
        full_us[rounds / 2],
        full_us[rounds / 2] as f64 / 1000.0,
        uno_us[rounds / 2],
        uno_us[rounds / 2] as f64 / 1000.0,
        pres_us[rounds / 2],
        pres_us[rounds / 2] as f64 / 1000.0,
    );
    out.line(cx, line.as_str())?;
    Ok(())
}
