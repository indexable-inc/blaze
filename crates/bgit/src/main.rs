//! `bgit` — fast, Git-wire-compatible CLI and native macOS driver over `blaze-git`.
//!
//! Commands:
//! - `bgit init [dir]`
//! - `bgit infect [dir] [--raw]`              Convert an existing `.git` repo into native `.bgit`
//! - `bgit disinfect <dir> <out-git-dir>`     Export `.bgit` back into standard `.git` pack+idx
//! - `bgit log [dir] [rev] [-n N]`            Fast commit history walk from `.bgit` / `CGPH`
//! - `bgit merge-base <dir> <revA> <revB>`    Instant LCA via `CGPH` commit-graph
//! - `bgit merge <dir> <ours> <theirs> [--write-commit]`  ORT-style O(changed) 3-way merge
//! - `bgit serve --http <addr> <repo>`        Git Smart HTTP server (Protocol v2 + push v1)
//! - `bgit serve --stdio <upload-pack|receive-pack> <repo>`  Git stdio/SSH server
//! - `bgit ls-remote <http-url>`              Git Protocol v2 client `ls-refs`
//! - `bgit clone <http-url> <dest-dir>`       Git Protocol v2 client clone into `.bgit` + worktree
//! - `bgit push <http-url> <dir> <ref>`       Git `receive-pack` push client
//! - `bgit bench <repo>`                      Head-to-head `.bgit` vs `git` macOS benchmark

#![forbid(unsafe_code)]

mod disk;
mod net_driver;
mod status_driver;

use std::{
    alloc::Global,
    env,
    io::{self, IsTerminal as _},
    path::{Path, PathBuf},
    process::{Command, ExitCode},
    time::{Instant, SystemTime, UNIX_EPOCH},
};

use blaze_git::{
    Detail, PromptStyle, StatusOpts, UntrackedMode, format_long_status, format_porcelain_v1,
    format_short_status, format_status,
    merge::{MergeLimits, MergeOpts, MergeStatus, find_merge_bases, merge_commits},
    slab::SlabCompression,
    storage::{ObjectStore, RefStore, RefTarget, RefUpdate, Signature, encode_commit},
};
use blaze_git_fetch::{Kind as ObjectKind, store::Commit};
use blaze_own::{Alloc, Erased, String as OwnString, Vec as OwnVec};

use crate::{
    disk::{BgitDiskStore, disinfect_repo_on_disk, infect_repo_on_disk, open_or_infect},
    net_driver::{http_clone, http_ls_remote, http_push, serve_http, serve_stdio},
    status_driver::run_status,
};

fn main() -> ExitCode {
    let erased = Erased::new(Global);
    let alloc = erased.handle();
    let args: Vec<String> = env::args().skip(1).collect();
    if args.is_empty() || args[0] == "-h" || args[0] == "--help" {
        print_usage();
        return ExitCode::SUCCESS;
    }
    match run(&args, alloc) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("bgit error: {e}");
            ExitCode::FAILURE
        }
    }
}

fn print_usage() {
    println!(
        "bgit — sans-IO, Git-wire-compatible VCS engine (macOS / APFS native .bgit store)\n\n\
         USAGE:\n\
         \tbgit status [dir] [-s|--short] [--porcelain] [-b|--branch] [-uno|-unormal|-uall] [--prompt] [--timing]\n\
         \tbgit init [dir]\n\
         \tbgit infect [dir] [--raw]\n\
         \tbgit disinfect <dir> <out-git-dir>\n\
         \tbgit verify [dir]\n\
         \tbgit log [dir] [rev] [-n N]\n\
         \tbgit merge-base <dir> <revA> <revB>\n\
         \tbgit merge <dir> <ours> <theirs> [--write-commit]\n\
         \tbgit serve --http <addr> <repo>\n\
         \tbgit serve --stdio <upload-pack|receive-pack> <repo>\n\
         \tbgit ls-remote <http-url>\n\
         \tbgit clone <http-url> <dest-dir>\n\
         \tbgit push <http-url> <dir> <ref>\n\
         \tbgit bench <repo>"
    );
}

fn run<'a>(args: &[String], alloc: Alloc<'a>) -> io::Result<()> {
    match args[0].as_str() {
        "status" | "st" => cmd_status(&args[1..], alloc),
        "init" => {
            let dir = args.get(1).map_or_else(|| PathBuf::from("."), PathBuf::from);
            let store = BgitDiskStore::init(&dir, alloc)?;
            println!("Initialized empty .bgit repository in {}", store.bgit_dir.display());
            Ok(())
        }
        "infect" => {
            let mut dir = PathBuf::from(".");
            let mut compression = SlabCompression::AutoZstd;
            for a in &args[1..] {
                if a == "--raw" {
                    compression = SlabCompression::RawOnly;
                } else {
                    dir = PathBuf::from(a);
                }
            }
            let t0 = Instant::now();
            let stats = infect_repo_on_disk(&dir, compression, alloc)?;
            let elapsed = t0.elapsed();
            let ratio = if stats.stored_slab_bytes > 0 {
                stats.raw_object_bytes as f64 / stats.stored_slab_bytes as f64
            } else {
                1.0
            };
            println!(
                "Infected {} -> {}/.bgit in {:.2} ms\n  \
                 objects: {} unique (from {} loose + {} in {} packfiles)\n  \
                 commits in CGPH graph: {}\n  \
                 refs in BGRF table: {}\n  \
                 bytes: {} raw -> {} stored ({:.2}x)",
                dir.display(),
                dir.display(),
                elapsed.as_secs_f64() * 1000.0,
                stats.unique_objects_out,
                stats.loose_objects_in,
                stats.packed_objects_in,
                stats.packfiles_in,
                stats.commits_in_graph,
                stats.refs_out,
                stats.raw_object_bytes,
                stats.stored_slab_bytes,
                ratio,
            );
            Ok(())
        }
        "disinfect" => {
            if args.len() < 3 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "usage: bgit disinfect <dir> <out-git-dir>",
                ));
            }
            let dir = Path::new(&args[1]);
            let out = Path::new(&args[2]);
            let t0 = Instant::now();
            disinfect_repo_on_disk(dir, out, alloc)?;
            println!(
                "Disinfected {} -> {} in {:.2} ms",
                dir.display(),
                out.display(),
                t0.elapsed().as_secs_f64() * 1000.0
            );
            Ok(())
        }
        "verify" => {
            let dir = args.get(1).map_or_else(|| PathBuf::from("."), PathBuf::from);
            let t0 = Instant::now();
            let store = BgitDiskStore::open(&dir, alloc)?;
            let checked = store.verify_full()?;
            println!(
                "Verified {} objects (BGIX CRC-32 + per-record CRC-32 + SHA-1) in {:.2} ms",
                checked,
                t0.elapsed().as_secs_f64() * 1000.0
            );
            Ok(())
        }
        "log" => cmd_log(&args[1..], alloc),
        "merge-base" => {
            if args.len() < 4 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "usage: bgit merge-base <dir> <revA> <revB>",
                ));
            }
            let store = open_or_infect(Path::new(&args[1]), alloc)?;
            let a = store
                .resolve_rev(&args[2])
                .map_err(|e| io::Error::other(format!("{e}")))?
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("rev not found: {}", args[2])))?;
            let b = store
                .resolve_rev(&args[3])
                .map_err(|e| io::Error::other(format!("{e}")))?
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("rev not found: {}", args[3])))?;
            let cg = store.commit_graph();
            let bases = find_merge_bases(&store, cg.as_ref(), a, b, MergeLimits::SHIPPED, alloc)
                .map_err(|e| io::Error::other(format!("{e}")))?;
            for base in bases {
                println!("{base}");
            }
            Ok(())
        }
        "merge" => cmd_merge(&args[1..], alloc),
        "serve" => {
            if args.len() < 4 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "usage: bgit serve --http <addr> <repo> | bgit serve --stdio <upload-pack|receive-pack> <repo>",
                ));
            }
            match args[1].as_str() {
                "--http" => serve_http(&args[2], Path::new(&args[3]), alloc),
                "--stdio" => serve_stdio(&args[2], Path::new(&args[3]), alloc),
                other => Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("unknown serve mode `{other}` (use --http or --stdio)"),
                )),
            }
        }
        "ls-remote" => {
            if args.len() < 2 {
                return Err(io::Error::new(io::ErrorKind::InvalidInput, "usage: bgit ls-remote <http-url>"));
            }
            http_ls_remote(&args[1], alloc)
        }
        "clone" => {
            if args.len() < 3 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "usage: bgit clone <http-url> <dest-dir>",
                ));
            }
            http_clone(&args[1], Path::new(&args[2]), alloc)
        }
        "push" => {
            if args.len() < 4 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "usage: bgit push <http-url> <dir> <ref>",
                ));
            }
            http_push(&args[1], Path::new(&args[2]), &args[3], alloc)
        }
        "bench" => {
            let dir = args.get(1).map_or_else(|| PathBuf::from("."), PathBuf::from);
            cmd_bench(&dir, alloc)
        }
        other => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("unknown command `{other}` (run `bgit --help`)"),
        )),
    }
}

fn cmd_log<'a>(args: &[String], alloc: Alloc<'a>) -> io::Result<()> {
    let mut dir = PathBuf::from(".");
    let mut rev = String::from("HEAD");
    let mut limit = 20usize;
    let mut i = 0;
    let mut positional = 0;
    while i < args.len() {
        if args[i] == "-n" && i + 1 < args.len() {
            limit = args[i + 1].parse().unwrap_or(20);
            i += 2;
        } else {
            if positional == 0 {
                dir = PathBuf::from(&args[i]);
            } else if positional == 1 {
                rev = args[i].clone();
            }
            positional += 1;
            i += 1;
        }
    }

    let store = open_or_infect(&dir, alloc)?;
    let mut cur = store
        .resolve_rev(&rev)
        .map_err(|e| io::Error::other(format!("{e}")))?;
    let mut buf = OwnVec::new_in(alloc);
    let mut shown = 0usize;
    while let Some(oid) = cur {
        if shown >= limit {
            break;
        }
        let kind = store
            .read_object(&oid, &mut buf)
            .map_err(|e| io::Error::other(format!("{e}")))?;
        if kind != Some(ObjectKind::Commit) {
            break;
        }
        let parsed = Commit::parse(&buf).map_err(|(_, why)| io::Error::other(format!("{why:?}")))?;
        let text = core::str::from_utf8(&buf).unwrap_or("");
        let subject = text
            .split_once("\n\n")
            .and_then(|(_, msg)| msg.lines().next())
            .unwrap_or("");
        println!("{oid}  {subject}");
        cur = parsed.parents().next();
        shown += 1;
    }
    Ok(())
}

fn cmd_merge<'a>(args: &[String], alloc: Alloc<'a>) -> io::Result<()> {
    if args.len() < 3 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "usage: bgit merge <dir> <ours> <theirs> [--write-commit]",
        ));
    }
    let dir = Path::new(&args[0]);
    let ours_rev = &args[1];
    let theirs_rev = &args[2];
    let write_commit = args.iter().any(|a| a == "--write-commit");

    let mut store = open_or_infect(dir, alloc)?;
    let ours = store
        .resolve_rev(ours_rev)
        .map_err(|e| io::Error::other(format!("{e}")))?
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("rev not found: {ours_rev}")))?;
    let theirs = store
        .resolve_rev(theirs_rev)
        .map_err(|e| io::Error::other(format!("{e}")))?
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("rev not found: {theirs_rev}")))?;

    let t0 = Instant::now();
    // Run merge using an O(1) cloned handle to the read-only mmap'd commit-graph (zero file read!).
    let result = {
        let cg_map = store.graph_mapping();
        let cg = cg_map
            .as_ref()
            .filter(|m| !m.bytes().is_empty())
            .and_then(|m| blaze_git::commit_graph::CommitGraph::from_layers(&[m.bytes()], alloc).ok());
        merge_commits(&mut store, cg.as_ref(), ours, theirs, MergeOpts::DEFAULT, alloc)
            .map_err(|e| io::Error::other(format!("{e}")))?
    };
    let elapsed = t0.elapsed();

    println!(
        "status={:?} tree={} subtrees_skipped={} subtrees_merged={} blobs_merged={} renames={} conflicts={} ({:.3} ms)",
        result.status,
        result.tree,
        result.stats.subtrees_skipped,
        result.stats.subtrees_merged,
        result.stats.blobs_merged,
        result.stats.renames_detected,
        result.conflicts.len(),
        elapsed.as_secs_f64() * 1000.0,
    );
    for c in &result.conflicts {
        println!("  CONFLICT ({:?}): {}", c.kind, c.path.as_str());
    }

    if write_commit && result.status == MergeStatus::MergedClean {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs() as i64);
        let sig = Signature { name: "bgit", email: "bgit@ix.dev", timestamp: now, tz: "+0000" };
        let mut commit_buf = OwnVec::new_in(alloc);
        let msg = format!("Merge {theirs_rev} into {ours_rev}\n");
        encode_commit(result.tree, &[ours, theirs], sig, sig, &msg, &mut commit_buf);
        let commit_oid = store
            .write_object(ObjectKind::Commit, &commit_buf)
            .map_err(|e| io::Error::other(format!("{e}")))?;

        // Update current branch if HEAD is symbolic.
        if let Some(RefTarget::Symbolic(sym)) = store
            .read_head(alloc)
            .map_err(|e| io::Error::other(format!("{e}")))?
        {
            let _ = store
                .apply_ref_updates(
                    &[RefUpdate {
                        name: OwnString::from_str_in(sym.as_str(), alloc),
                        old: None,
                        new: commit_oid,
                    }],
                    false,
                    alloc,
                )
                .map_err(|e| io::Error::other(format!("{e}")))?;
        }
        store.flush()?;
        println!("commit={commit_oid}");
    } else if result.status == MergeStatus::MergedClean || result.status == MergeStatus::Conflicted {
        store.flush()?;
    }
    Ok(())
}

fn cmd_status<'a>(args: &[String], alloc: Alloc<'a>) -> io::Result<()> {
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Mode {
        Long,
        Short,
        Porcelain,
        Prompt,
    }

    let mut dir = PathBuf::from(".");
    let mut mode = Mode::Long;
    let mut show_branch = false;
    let mut untracked = UntrackedMode::Normal;
    let mut color_override: Option<bool> = None;
    let mut show_timing = false;

    for arg in args {
        match arg.as_str() {
            "-s" | "--short" => mode = Mode::Short,
            "--porcelain" | "--porcelain=v1" | "--porcelain=1" => mode = Mode::Porcelain,
            "--prompt" => mode = Mode::Prompt,
            "-b" | "--branch" => show_branch = true,
            "-sb" | "-bs" => {
                mode = Mode::Short;
                show_branch = true;
            }
            "-uno" | "-u=no" | "--untracked-files=no" => untracked = UntrackedMode::Off,
            "-unormal" | "-u=normal" | "--untracked-files=normal" => {
                untracked = UntrackedMode::Normal;
            }
            "-u" | "-uall" | "-u=all" | "--untracked-files=all" => {
                untracked = UntrackedMode::All;
            }
            "--color" | "--color=always" => color_override = Some(true),
            "--no-color" | "--color=never" => color_override = Some(false),
            "--color=auto" => color_override = None,
            "--timing" => show_timing = true,
            "-h" | "--help" => {
                println!(
                    "USAGE:\n\
                     \tbgit status [dir] [-s|--short] [--porcelain] [-b|--branch] \
                     [-uno|-unormal|-uall] [--prompt] [--color|--no-color] [--timing]"
                );
                return Ok(());
            }
            other if other.starts_with('-') => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("unknown status flag: {other}"),
                ));
            }
            other => dir = PathBuf::from(other),
        }
    }

    let detail = if mode == Mode::Prompt { Detail::Counts } else { Detail::Full };
    let opts = StatusOpts { untracked, detail, ..StatusOpts::FULL };
    let (status, timing) = run_status(&dir, opts, alloc)?;

    let use_color = match mode {
        Mode::Porcelain | Mode::Prompt => false,
        Mode::Long | Mode::Short => color_override.unwrap_or_else(|| io::stdout().is_terminal()),
    };

    let mut out = String::new();
    match mode {
        Mode::Long => {
            let _ = format_long_status(&status, use_color, &mut out);
        }
        Mode::Short => {
            let _ = format_short_status(&status, show_branch, use_color, &mut out);
        }
        Mode::Porcelain => {
            let _ = format_porcelain_v1(&status, show_branch, &mut out);
        }
        Mode::Prompt => {
            let _ = format_status(&status, &PromptStyle::STARSHIP, &mut out);
            out.push('\n');
        }
    }
    print!("{out}");

    if show_timing {
        eprintln!(
            "timing: total={:.2}ms machine={:.2}ms list_dirs={:.2}ms(n={}) stat_paths={:.2}ms(n={}) read_files={:.2}ms(n={}) read_with_stat={:.2}ms links={:.2}ms ranges={:.2}ms rounds={}",
            timing.total_us as f64 / 1000.0,
            timing.machine_us as f64 / 1000.0,
            timing.list_dirs_us as f64 / 1000.0,
            timing.list_dirs_count,
            timing.stat_paths_us as f64 / 1000.0,
            timing.stat_paths_count,
            timing.read_files_us as f64 / 1000.0,
            timing.read_files_count,
            timing.read_with_stat_us as f64 / 1000.0,
            timing.read_links_us as f64 / 1000.0,
            timing.read_ranges_us as f64 / 1000.0,
            timing.rounds,
        );
    }

    Ok(())
}

fn cmd_bench<'a>(repo_dir: &Path, alloc: Alloc<'a>) -> io::Result<()> {
    let store = open_or_infect(repo_dir, alloc)?;
    let mut oids = OwnVec::new_in(alloc);
    store
        .list_oids(&mut oids)
        .map_err(|e| io::Error::other(format!("{e}")))?;
    let mut refs = OwnVec::new_in(alloc);
    store
        .list_refs("", alloc, &mut refs)
        .map_err(|e| io::Error::other(format!("{e}")))?;

    println!(
        "=== bgit macOS floor benchmark on {} (objects={}, refs={}) ===",
        repo_dir.display(),
        oids.len(),
        refs.len()
    );

    // 1. ls-refs / show-ref (5 rounds)
    let mut bgit_refs_us = [0u64; 5];
    for slot in &mut bgit_refs_us {
        let t0 = Instant::now();
        let mut r = OwnVec::new_in(alloc);
        store.list_refs("", alloc, &mut r).map_err(|e| io::Error::other(format!("{e}")))?;
        *slot = t0.elapsed().as_micros() as u64;
    }
    bgit_refs_us.sort_unstable();

    let mut git_refs_us = [0u64; 5];
    for slot in &mut git_refs_us {
        let t0 = Instant::now();
        let _ = Command::new("git").arg("-C").arg(repo_dir).arg("show-ref").output()?;
        *slot = t0.elapsed().as_micros() as u64;
    }
    git_refs_us.sort_unstable();
    println!(
        "ls-refs / show-ref:     bgit = {:>6} us  vs  git = {:>6} us  ({:.1}x faster)",
        bgit_refs_us[2],
        git_refs_us[2],
        git_refs_us[2] as f64 / bgit_refs_us[2].max(1) as f64
    );

    // 2. Read 100 objects by OID (or all if fewer)
    let sample_n = oids.len().min(100);
    if sample_n > 0 {
        let mut buf = OwnVec::new_in(alloc);
        let t0 = Instant::now();
        for oid in &oids[..sample_n] {
            let _ = store.read_object(oid, &mut buf).map_err(|e| io::Error::other(format!("{e}")))?;
        }
        let bgit_obj_us = t0.elapsed().as_micros() as u64;

        let mut batch_in = String::new();
        for oid in &oids[..sample_n] {
            batch_in.push_str(&format!("{oid}\n"));
        }
        let t1 = Instant::now();
        let mut child = Command::new("git")
            .arg("-C")
            .arg(repo_dir)
            .args(["cat-file", "--batch"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .spawn()?;
        if let Some(mut stdin) = child.stdin.take() {
            use std::io::Write as _;
            let _ = stdin.write_all(batch_in.as_bytes());
        }
        let _ = child.wait()?;
        let git_obj_us = t1.elapsed().as_micros() as u64;
        println!(
            "read {sample_n} objects:       bgit = {:>6} us  vs  git = {:>6} us  ({:.1}x faster)",
            bgit_obj_us,
            git_obj_us,
            git_obj_us as f64 / bgit_obj_us.max(1) as f64
        );
    }

    // 3. Parallel PACK v2 encoding of ALL objects vs `git pack-objects --stdout`
    if !oids.is_empty() {
        let mut pack_out = OwnVec::new_in(alloc);
        let t0 = Instant::now();
        let _ = store
            .write_pack_parallel(&oids, blaze_git::pack_write::PackWriteOpts::FAST, &mut pack_out)
            .map_err(|e| io::Error::other(format!("{e}")))?;
        let bgit_pack_us = t0.elapsed().as_micros() as u64;

        let t1 = Instant::now();
        let _ = Command::new("git")
            .arg("-C")
            .arg(repo_dir)
            .args(["pack-objects", "--all", "--stdout", "-q"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .output()?;
        let git_pack_us = t1.elapsed().as_micros() as u64;
        println!(
            "pack all ({:>4} objs):   bgit = {:>6} us  vs  git = {:>6} us  ({:.1}x faster, {} B)",
            oids.len(),
            bgit_pack_us,
            git_pack_us,
            git_pack_us as f64 / bgit_pack_us.max(1) as f64,
            pack_out.len(),
        );
    }

    // 4. merge-base + 3-way tree merge on HEAD vs HEAD~1 (if >= 2 commits)
    if let Some(head_oid) = store.resolve_ref("HEAD").map_err(|e| io::Error::other(format!("{e}")))? {
        let mut buf = OwnVec::new_in(alloc);
        if store.read_object(&head_oid, &mut buf).ok().flatten() == Some(ObjectKind::Commit)
            && let Ok(c) = Commit::parse(&buf)
            && let Some(parent_oid) = c.parents().next()
        {
            let cg = store.commit_graph();
            let t0 = Instant::now();
            let _ = find_merge_bases(&store, cg.as_ref(), head_oid, parent_oid, MergeLimits::SHIPPED, alloc)
                .map_err(|e| io::Error::other(format!("{e}")))?;
            let bgit_mb_us = t0.elapsed().as_micros() as u64;

            let t1 = Instant::now();
            let _ = Command::new("git")
                .arg("-C")
                .arg(repo_dir)
                .args(["merge-base", &format!("{head_oid}"), &format!("{parent_oid}")])
                .output()?;
            let git_mb_us = t1.elapsed().as_micros() as u64;
            println!(
                "merge-base (CGPH):      bgit = {:>6} us  vs  git = {:>6} us  ({:.1}x faster)",
                bgit_mb_us,
                git_mb_us,
                git_mb_us as f64 / bgit_mb_us.max(1) as f64
            );
        }
    }
    Ok(())
}
