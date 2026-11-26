//! Capability-based Starship-class Git prompt and status driver over [`crate::fs::Dir`].
//!
//! All Git semantics, `.git/index` parsing, `.gitignore` evaluation, binary `commit-graph`
//! traversal, cache-tree subtree pruning, and racy-git verification live in the sans-IO core
//! crate (`blaze-git`). This module is a thin driver that executes each [`Want`] batch over
//! [`crate::fs::Dir`] capabilities (`looks`, `read_all_many`, `map_read`, `entries` via
//! `getattrlistbulk` on macOS / `statx` on Linux / `SimWorld` in DST, `read_link`, `read_at`).

use core::fmt;

use blaze_own::{AVec, Alloc, Vec as OwnVec, planted::NoPlant};

pub use blaze_git::{
    AheadBehindMode, Detail, Divergence, ErrorKind, FileKind, FileStat, GitError, GitFile, Head,
    IncrementalPlan, Limits, ObjectKind, Oid, PromptStatus, PromptStyle, RepoState, ShortOid,
    Snapshot, StatusCounts, StatusMachine, StatusOpts, Step, TagMode, Truncated, UntrackedMode,
    Upstream, WorktreeKind, format_status,
    machine::{
        DirEntry, DirListing, FileData, FullReason, Got, PathScope, RangeData, RangeRead,
        ScopedPath, StatEntry, Want,
    },
};

use crate::{
    cx::Cx,
    fs::{Dir, EntryKind, FsError, FsKind, Look, ReadOnly},
    mem::{Bytes, Frame, Tag},
    path::Path,
};

/// Errors produced by [`Repo::open`] or [`Repo::status`].
#[derive(Debug)]
pub enum RepoError {
    /// No `.git` directory/file or bare repository found at the given capability directory.
    NotARepository,
    /// A linked worktree's `.git` file points outside the granted directory capability.
    GitDirOutsideGrant,
    /// Scratch frame reservation failed.
    OutOfMemory,
    /// An underlying filesystem capability error.
    Fs(FsError),
    /// A Git format or protocol refusal from the sans-IO core.
    Git(GitError),
}

impl fmt::Display for RepoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RepoError::NotARepository => f.write_str("not a git repository"),
            RepoError::GitDirOutsideGrant => {
                f.write_str("gitdir pointer references a directory outside the granted capability")
            }
            RepoError::OutOfMemory => f.write_str("git scratch frame reservation failed"),
            RepoError::Fs(e) => write!(f, "git fs error: {e}"),
            RepoError::Git(e) => write!(f, "{e}"),
        }
    }
}

impl core::error::Error for RepoError {}

impl From<FsError> for RepoError {
    fn from(e: FsError) -> RepoError {
        RepoError::Fs(e)
    }
}

impl From<GitError> for RepoError {
    fn from(e: GitError) -> RepoError {
        RepoError::Git(e)
    }
}

/// Capability handle to an opened Git repository (normal, linked worktree, or bare).
pub struct Repo {
    worktree: Option<Dir<ReadOnly>>,
    git_dir: Dir<ReadOnly>,
    common_dir: Dir<ReadOnly>,
    kind: WorktreeKind,
}

/// Per-phase execution breakdown returned by [`Repo::status_profiled`].
#[derive(Clone, Copy, Default, Debug)]
pub struct StatusTiming {
    pub read_files_us: u64,
    pub read_files_count: u32,
    pub read_with_stat_us: u64,
    pub read_with_stat_count: u32,
    pub list_dirs_us: u64,
    pub list_dirs_count: u32,
    pub stat_paths_us: u64,
    pub stat_paths_count: u32,
    pub read_links_us: u64,
    pub read_ranges_us: u64,
    pub machine_us: u64,
    pub rounds: u32,
}

impl Repo {
    /// Open a Git repository directly at `dir` (normal checkout with `.git/`, linked worktree
    /// whose `.git` file points to a relative path beneath `dir`, or bare repo containing `HEAD`
    /// and `objects/`).
    pub fn open(cx: &Cx, dir: &Dir<ReadOnly>) -> Result<Repo, RepoError> {
        let dot_git = Path::new(".git").expect("valid path");
        let head = Path::new("HEAD").expect("valid path");
        let objects = Path::new("objects").expect("valid path");
        let looks = dir.looks(cx, [&dot_git, &head, &objects])?;

        match looks.get(0).map(|l| l.kind) {
            Some(EntryKind::Dir) => {
                let gd = dir.subdir(&dot_git);
                return Ok(Repo {
                    worktree: Some(dir.read_only()),
                    git_dir: gd.read_only(),
                    common_dir: gd,
                    kind: WorktreeKind::Normal,
                });
            }
            Some(EntryKind::File) => {
                let mut raw = Bytes::new_in(cx.frame(), "bstd::git");
                dir.read_all(cx, &dot_git, &mut raw)?;
                let target = blaze_git::refs::parse_gitdir_file(raw.as_slice())?;
                let rel_git = Path::new(target).map_err(|_| RepoError::GitDirOutsideGrant)?;
                let gd = dir.subdir(&rel_git);
                // Check optional `commondir` file inside `gd`.
                let common_path = Path::new("commondir").expect("valid path");
                let common = if gd.stat(cx, &common_path)?.is_some() {
                    let mut cb = Bytes::new_in(cx.frame(), "bstd::git");
                    gd.read_all(cx, &common_path, &mut cb)?;
                    let cstr = core::str::from_utf8(cb.as_slice())
                        .map_err(|_| GitError {
                            file: GitFile::CommonDir,
                            at: 0,
                            kind: ErrorKind::Malformed,
                        })?
                        .trim();
                    let rel_c = Path::new(cstr).map_err(|_| RepoError::GitDirOutsideGrant)?;
                    gd.subdir(&rel_c)
                } else {
                    gd.read_only()
                };
                return Ok(Repo {
                    worktree: Some(dir.read_only()),
                    git_dir: gd,
                    common_dir: common,
                    kind: WorktreeKind::LinkedWorktree,
                });
            }
            _ => {}
        }

        // Check for bare repository (`HEAD` file + `objects` dir).
        if looks.get(1).map(|l| l.kind) == Some(EntryKind::File)
            && looks.get(2).map(|l| l.kind) == Some(EntryKind::Dir)
        {
            return Ok(Repo {
                worktree: None,
                git_dir: dir.read_only(),
                common_dir: dir.read_only(),
                kind: WorktreeKind::Bare,
            });
        }

        Err(RepoError::NotARepository)
    }

    /// Construct a [`Repo`] from explicitly granted capability directories (for linked worktrees
    /// whose `.git/worktrees/<name>` lives outside the worktree directory).
    pub fn from_parts(
        worktree: Option<Dir<ReadOnly>>,
        git_dir: Dir<ReadOnly>,
        common_dir: Dir<ReadOnly>,
        kind: WorktreeKind,
    ) -> Repo {
        Repo { worktree, git_dir, common_dir, kind }
    }

    pub fn kind(&self) -> WorktreeKind {
        self.kind
    }

    /// Compute [`PromptStatus`] while recording per-phase IO and sans-IO machine timings.
    pub fn status_profiled<'a>(
        &self,
        cx: &Cx,
        clock: &crate::clock::Clock,
        opts: StatusOpts,
        alloc: Alloc<'a>,
    ) -> Result<(PromptStatus<'a>, StatusTiming), RepoError> {
        let mut scratch = Frame::with_reserve(128 << 20, Tag::Other)
            .map_err(|_| RepoError::OutOfMemory)?;
        let mut machine = StatusMachine::new(opts, self.kind, alloc, NoPlant);
        let mut timing = StatusTiming::default();
        loop {
            let t0 = clock.now();
            let step = machine.poll()?;
            let t1 = clock.now();
            timing.machine_us += t1.since(t0).unwrap_or_default().as_micros() as u64;
            match step {
                Step::Done(status) => return Ok((status, timing)),
                Step::Need(want) => {
                    timing.rounds += 1;
                    let kind_tag = match &want {
                        Want::ReadFiles(v) => (0u8, v.len() as u32),
                        Want::ReadWithStat(v) => (1, v.len() as u32),
                        Want::ListDirs(v) => (2, v.len() as u32),
                        Want::StatPaths(v) => (3, v.len() as u32),
                        Want::ReadLinks(v) => (4, v.len() as u32),
                        Want::ReadRanges(v) => (5, v.len() as u32),
                    };
                    let io0 = clock.now();
                    let got = self.execute(cx, &scratch, want, alloc)?;
                    scratch.reset();
                    let io1 = clock.now();
                    let us = io1.since(io0).unwrap_or_default().as_micros() as u64;
                    match kind_tag.0 {
                        0 => {
                            timing.read_files_us += us;
                            timing.read_files_count += kind_tag.1;
                        }
                        1 => {
                            timing.read_with_stat_us += us;
                            timing.read_with_stat_count += kind_tag.1;
                        }
                        2 => {
                            timing.list_dirs_us += us;
                            timing.list_dirs_count += kind_tag.1;
                        }
                        3 => {
                            timing.stat_paths_us += us;
                            timing.stat_paths_count += kind_tag.1;
                        }
                        4 => timing.read_links_us += us,
                        _ => timing.read_ranges_us += us,
                    }
                    let f0 = clock.now();
                    machine.feed(got)?;
                    let f1 = clock.now();
                    timing.machine_us += f1.since(f0).unwrap_or_default().as_micros() as u64;
                }
            }
        }
    }

    /// Compute the Starship-class [`PromptStatus`] by driving [`StatusMachine`] over `bstd::fs`.
    ///
    /// All returned strings and scratch tables are allocated in `alloc`. Temporary read buffers
    /// live in a bounded per-call frame reset after each IO batch, so `cx.frame()` is never
    /// polluted across repeated calls.
    pub fn status<'a>(
        &self,
        cx: &Cx,
        opts: StatusOpts,
        alloc: Alloc<'a>,
    ) -> Result<PromptStatus<'a>, RepoError> {
        let mut scratch = Frame::with_reserve(128 << 20, Tag::Other)
            .map_err(|_| RepoError::OutOfMemory)?;
        let mut machine = StatusMachine::new(opts, self.kind, alloc, NoPlant);
        loop {
            match machine.poll()? {
                Step::Done(status) => return Ok(status),
                Step::Need(want) => {
                    let got = self.execute(cx, &scratch, want, alloc)?;
                    scratch.reset();
                    machine.feed(got)?;
                }
            }
        }
    }

    fn dir_for(&self, scope: PathScope) -> Option<&Dir<ReadOnly>> {
        match scope {
            PathScope::Worktree => self.worktree.as_ref(),
            PathScope::GitDir => Some(&self.git_dir),
            PathScope::CommonDir => Some(&self.common_dir),
        }
    }

    fn execute<'a>(
        &self,
        cx: &Cx,
        scratch: &Frame,
        want: Want<'a>,
        alloc: Alloc<'a>,
    ) -> Result<Got<'a>, RepoError> {
        match want {
            Want::ReadFiles(paths) => self.exec_read_files(cx, scratch, &paths, alloc),
            Want::ReadWithStat(paths) => self.exec_read_with_stat(cx, &paths, alloc),
            Want::ListDirs(dirs) => self.exec_list_dirs(cx, scratch, &dirs, alloc),
            Want::StatPaths(paths) => self.exec_stat_paths(cx, scratch, &paths, alloc),
            Want::ReadLinks(paths) => self.exec_read_links(cx, scratch, &paths, alloc),
            Want::ReadRanges(reads) => self.exec_read_ranges(cx, scratch, &reads, alloc),
        }
    }

    fn exec_read_files<'a>(
        &self,
        cx: &Cx,
        scratch: &Frame,
        paths: &[ScopedPath<'a>],
        alloc: Alloc<'a>,
    ) -> Result<Got<'a>, RepoError> {
        let mut out: AVec<'a, Option<AVec<'a, u8>>> = OwnVec::with_capacity_in(paths.len(), alloc);
        for _ in 0..paths.len() {
            out.push(None);
        }
        // Group by scope so each scope runs ONE batched `looks` + ONE concurrent `read_all_many`!
        for scope in [PathScope::GitDir, PathScope::CommonDir, PathScope::Worktree] {
            let Some(dir) = self.dir_for(scope) else {
                continue;
            };
            let mut slot_indices = OwnVec::new_in(alloc);
            let mut parsed_paths = OwnVec::new_in(alloc);
            for (i, sp) in paths.iter().enumerate() {
                if sp.scope == scope
                    && let Ok(p) = Path::new(sp.path.as_str())
                {
                    slot_indices.push(i);
                    parsed_paths.push(p);
                }
            }
            if parsed_paths.is_empty() {
                continue;
            }
            // Batch stat first so missing optional files (MERGE_HEAD, rebase-*, etc.) are skipped cleanly.
            let looks = dir.looks(cx, parsed_paths.iter())?;
            let mut present_slots = OwnVec::new_in(alloc);
            let mut present_paths = OwnVec::new_in(alloc);
            for (k, p) in parsed_paths.iter().enumerate() {
                if looks.get(k).map(|l| l.kind) == Some(EntryKind::File) {
                    present_slots.push(slot_indices[k]);
                    present_paths.push(p);
                }
            }
            if present_paths.is_empty() {
                continue;
            }
            let mut bufs: AVec<'a, Bytes<'_>> = OwnVec::with_capacity_in(present_paths.len(), alloc);
            for _ in 0..present_paths.len() {
                bufs.push(Bytes::new_in(scratch, "bstd::git"));
            }
            dir.read_all_many(cx, &present_paths, &mut bufs)?;
            for (k, &slot) in present_slots.iter().enumerate() {
                let slice = bufs[k].as_slice();
                let mut v = OwnVec::with_capacity_in(slice.len(), alloc);
                v.extend_from_slice(slice);
                out[slot] = Some(v);
            }
        }
        Ok(Got::Files(out))
    }

    fn exec_read_with_stat<'a>(
        &self,
        cx: &Cx,
        paths: &[ScopedPath<'a>],
        alloc: Alloc<'a>,
    ) -> Result<Got<'a>, RepoError> {
        let mut out: AVec<'a, FileData<'a>> = OwnVec::with_capacity_in(paths.len(), alloc);
        for _ in 0..paths.len() {
            out.push(FileData { bytes: None, stat: None });
        }
        for scope in [PathScope::GitDir, PathScope::CommonDir, PathScope::Worktree] {
            let Some(dir) = self.dir_for(scope) else {
                continue;
            };
            let mut slot_indices = OwnVec::new_in(alloc);
            let mut parsed_paths = OwnVec::new_in(alloc);
            for (i, sp) in paths.iter().enumerate() {
                if sp.scope == scope
                    && let Ok(p) = Path::new(sp.path.as_str())
                {
                    slot_indices.push(i);
                    parsed_paths.push(p);
                }
            }
            if parsed_paths.is_empty() {
                continue;
            }
            let looks = dir.looks(cx, parsed_paths.iter())?;
            for (k, p) in parsed_paths.iter().enumerate() {
                let Some(look) = looks.get(k) else {
                    continue;
                };
                if look.kind != EntryKind::File {
                    continue;
                }
                let stat = look_to_file_stat(look, looks.mode(k));
                // Map large binary files read-only (zero intermediate heap buffer).
                let mapped = dir.map_read(cx, p)?;
                let slice = mapped.bytes();
                let mut v = OwnVec::with_capacity_in(slice.len(), alloc);
                v.extend_from_slice(slice);
                out[slot_indices[k]] = FileData { bytes: Some(v), stat: Some(stat) };
            }
        }
        Ok(Got::FilesWithStat(out))
    }

    fn exec_list_dirs<'a>(
        &self,
        cx: &Cx,
        scratch: &Frame,
        dirs: &[ScopedPath<'a>],
        alloc: Alloc<'a>,
    ) -> Result<Got<'a>, RepoError> {
        let mut out: AVec<'a, Option<DirListing<'a>>> = OwnVec::with_capacity_in(dirs.len(), alloc);
        for _ in 0..dirs.len() {
            out.push(None);
        }
        // Group by scope and submit all `Effect::Entries` concurrently via `Dir::entries_many`!
        for scope in [PathScope::Worktree, PathScope::GitDir, PathScope::CommonDir] {
            let Some(base) = self.dir_for(scope) else {
                continue;
            };
            let mut slots = OwnVec::new_in(alloc);
            let mut owned_paths: AVec<'a, Option<Path>> = OwnVec::new_in(alloc);
            for (i, sp) in dirs.iter().enumerate() {
                if sp.scope != scope {
                    continue;
                }
                if sp.path.is_empty() {
                    slots.push(i);
                    owned_paths.push(None);
                } else if let Ok(rel) = Path::new(sp.path.as_str()) {
                    slots.push(i);
                    owned_paths.push(Some(rel));
                }
            }
            if slots.is_empty() {
                continue;
            }
            let mut refs: AVec<'a, Option<&Path>> = OwnVec::with_capacity_in(owned_paths.len(), alloc);
            for opt in owned_paths.iter() {
                refs.push(opt.as_ref());
            }
            let listings = base.entries_many(cx, &refs, scratch)?;
            for (k, &slot) in slots.iter().enumerate() {
                let Some(entries) = &listings[k] else {
                    continue;
                };
                // Flat packed DirListing: zero per-entry String heap allocations!
                let mut dl = DirListing::with_capacity(entries.len(), entries.len() * 16, alloc);
                for j in 0..entries.len() {
                    let look = entries.look(j);
                    let mode = entries.mode(j);
                    dl.push(entries.name(j), look_to_file_stat(look, mode));
                }
                out[slot] = Some(dl);
            }
        }
        Ok(Got::Dirs(out))
    }

    fn exec_stat_paths<'a>(
        &self,
        cx: &Cx,
        scratch: &Frame,
        paths: &[ScopedPath<'a>],
        alloc: Alloc<'a>,
    ) -> Result<Got<'a>, RepoError> {
        let mut out: AVec<'a, StatEntry> = OwnVec::with_capacity_in(paths.len(), alloc);
        for _ in 0..paths.len() {
            out.push(StatEntry { stat: FileStat::GONE });
        }
        for scope in [PathScope::Worktree, PathScope::GitDir, PathScope::CommonDir] {
            let Some(dir) = self.dir_for(scope) else {
                continue;
            };
            let mut slot_indices = OwnVec::new_in(alloc);
            let mut parsed_paths = OwnVec::new_in(alloc);
            for (i, sp) in paths.iter().enumerate() {
                if sp.scope == scope
                    && let Ok(p) = Path::new(sp.path.as_str())
                {
                    slot_indices.push(i);
                    parsed_paths.push(p);
                }
            }
            if parsed_paths.is_empty() {
                continue;
            }
            // `.git/index` entries are already strictly sorted by bytes: use `Dir::facts_of` so
            // dense folders run parallel `getattrlistbulk` instead of N individual `fstatat` calls!
            let strictly_sorted = parsed_paths
                .windows(2)
                .all(|w| w[0].as_str().as_bytes() < w[1].as_str().as_bytes());
            if strictly_sorted && parsed_paths.len() >= 16 {
                let facts = dir.facts_of(cx, &parsed_paths, scratch).map_err(|e| match e {
                    crate::fs::FactsError::Fs(fs) => RepoError::Fs(fs),
                    _ => RepoError::OutOfMemory,
                })?;
                for (k, &slot) in slot_indices.iter().enumerate() {
                    if let Some(look) = facts.get(k) {
                        out[slot] = StatEntry { stat: look_to_file_stat(look, facts.mode(k)) };
                    }
                }
            } else {
                let looks = dir.looks(cx, parsed_paths.iter())?;
                for (k, &slot) in slot_indices.iter().enumerate() {
                    if let Some(look) = looks.get(k) {
                        out[slot] = StatEntry { stat: look_to_file_stat(look, looks.mode(k)) };
                    }
                }
            }
        }
        Ok(Got::Stats(out))
    }

    fn exec_read_links<'a>(
        &self,
        cx: &Cx,
        scratch: &Frame,
        paths: &[ScopedPath<'a>],
        alloc: Alloc<'a>,
    ) -> Result<Got<'a>, RepoError> {
        let mut out: AVec<'a, Option<AVec<'a, u8>>> = OwnVec::with_capacity_in(paths.len(), alloc);
        for sp in paths.iter() {
            let Some(dir) = self.dir_for(sp.scope) else {
                out.push(None);
                continue;
            };
            let Ok(p) = Path::new(sp.path.as_str()) else {
                out.push(None);
                continue;
            };
            let mut buf = Bytes::new_in(scratch, "bstd::git");
            match dir.read_link(cx, &p, &mut buf) {
                Ok(_) => {
                    let slice = buf.as_slice();
                    let mut v = OwnVec::with_capacity_in(slice.len(), alloc);
                    v.extend_from_slice(slice);
                    out.push(Some(v));
                }
                Err(e) if matches!(e.kind, FsKind::NotFound) => out.push(None),
                Err(_) => out.push(None),
            }
        }
        Ok(Got::Links(out))
    }

    fn exec_read_ranges<'a>(
        &self,
        cx: &Cx,
        scratch: &Frame,
        reads: &[RangeRead<'a>],
        alloc: Alloc<'a>,
    ) -> Result<Got<'a>, RepoError> {
        let mut out: AVec<'a, RangeData<'a>> = OwnVec::with_capacity_in(reads.len(), alloc);
        for rr in reads.iter() {
            let Some(dir) = self.dir_for(rr.scope) else {
                out.push(RangeData { bytes: None });
                continue;
            };
            let Ok(p) = Path::new(rr.path.as_str()) else {
                out.push(RangeData { bytes: None });
                continue;
            };
            match dir.open_read(cx, &p) {
                Ok(file) => {
                    let len = usize::try_from(rr.len).unwrap_or(usize::MAX);
                    let mut buf = Bytes::new_in(scratch, "bstd::git");
                    file.read_at(cx, rr.offset, len, &mut buf)?;
                    let slice = buf.as_slice();
                    let mut v = OwnVec::with_capacity_in(slice.len(), alloc);
                    v.extend_from_slice(slice);
                    out.push(RangeData { bytes: Some(v) });
                }
                Err(e) if matches!(e.kind, FsKind::NotFound) => {
                    out.push(RangeData { bytes: None });
                }
                Err(e) => return Err(RepoError::Fs(e)),
            }
        }
        Ok(Got::Ranges(out))
    }
}

fn look_to_file_stat(look: Look, mode: u32) -> FileStat {
    let kind = match look.kind {
        EntryKind::File => FileKind::File,
        EntryKind::Dir => FileKind::Dir,
        EntryKind::Link => FileKind::Symlink,
        EntryKind::Other | EntryKind::Unknown => FileKind::Other,
        EntryKind::Gone => FileKind::Gone,
    };
    FileStat::from_ns(kind, look.len, look.modified_ns, look.changed_ns, look.ino, look.dev, mode)
}

#[cfg(test)]
mod tests {
    use alloc::{format, string::String, vec::Vec};

    use blaze_git::ObjectKind;

    use super::*;
    use crate::{
        fs::{Content, Durable},
        io::{Io, Limits as IoLimits, SimWorld},
        mem::TASK_FRAME,
        test::BUDGET,
    };

    fn make_v2_index(entries: &[(&str, Oid, u32, u32, u32)], root_tree: Option<Oid>) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(b"DIRC");
        out.extend_from_slice(&2u32.to_be_bytes());
        out.extend_from_slice(&(entries.len() as u32).to_be_bytes());
        for (path, oid, size, mtime_sec, ino) in entries {
            let start = out.len();
            out.extend_from_slice(&mtime_sec.to_be_bytes());
            out.extend_from_slice(&0u32.to_be_bytes());
            out.extend_from_slice(&mtime_sec.to_be_bytes());
            out.extend_from_slice(&0u32.to_be_bytes());
            out.extend_from_slice(&0u32.to_be_bytes()); // dev
            out.extend_from_slice(&ino.to_be_bytes());
            out.extend_from_slice(&0o100644u32.to_be_bytes());
            out.extend_from_slice(&0u32.to_be_bytes());
            out.extend_from_slice(&0u32.to_be_bytes());
            out.extend_from_slice(&size.to_be_bytes());
            out.extend_from_slice(&oid.0);
            let flags = (path.len().min(0x0fff)) as u16;
            out.extend_from_slice(&flags.to_be_bytes());
            out.extend_from_slice(path.as_bytes());
            out.push(0);
            while (out.len() - start) % 8 != 0 {
                out.push(0);
            }
        }
        if let Some(rt) = root_tree {
            let mut ext = Vec::new();
            ext.push(0);
            ext.extend_from_slice(format!("{} 0\n", entries.len()).as_bytes());
            ext.extend_from_slice(&rt.0);
            out.extend_from_slice(b"TREE");
            out.extend_from_slice(&(ext.len() as u32).to_be_bytes());
            out.extend_from_slice(&ext);
        }
        out.extend_from_slice(&[0u8; 20]);
        out
    }

    fn make_single_commit_graph(commit: Oid, tree: Oid) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(b"CGPH");
        out.push(1);
        out.push(1);
        out.push(3);
        out.push(0);
        let oidf_off = 8u64 + 4 * 12;
        let oidl_off = oidf_off + 1024;
        let cdat_off = oidl_off + 20;
        let end_off = cdat_off + 36;
        out.extend_from_slice(b"OIDF");
        out.extend_from_slice(&oidf_off.to_be_bytes());
        out.extend_from_slice(b"OIDL");
        out.extend_from_slice(&oidl_off.to_be_bytes());
        out.extend_from_slice(b"CDAT");
        out.extend_from_slice(&cdat_off.to_be_bytes());
        out.extend_from_slice(&[0u8; 4]);
        out.extend_from_slice(&end_off.to_be_bytes());
        for b in 0..256usize {
            let c: u32 = if b >= usize::from(commit.0[0]) { 1 } else { 0 };
            out.extend_from_slice(&c.to_be_bytes());
        }
        out.extend_from_slice(&commit.0);
        out.extend_from_slice(&tree.0);
        out.extend_from_slice(&0x7000_0000u32.to_be_bytes());
        out.extend_from_slice(&0x7000_0000u32.to_be_bytes());
        out.extend_from_slice(&((1u64 << 34) | 1000).to_be_bytes());
        out.extend_from_slice(&[0u8; 20]);
        out
    }

    /// End-to-end deterministic simulation of `bstd::git::Repo` over `SimWorld`: verifies branch,
    /// upstream equality, clean/modified/deleted/untracked/stashed counts, and Starship formatting
    /// with zero real filesystem calls.
    #[test]
    fn simulated_world_starship_status_end_to_end() {
        let io = Io::simulated(SimWorld::new(7, IoLimits::USUAL));
        let cx = io.cx(BUDGET, TASK_FRAME).expect("root cx");
        let fs = io.need_fs().expect("fs");
        let root = fs.root();

        let put = |rel: &str, bytes: &[u8]| {
            let p = Path::new(rel).expect("path");
            root.publish(&cx, &p, Content::bytes(bytes), Durable::No).expect("publish");
        };

        let a_oid = Oid::of(ObjectKind::Blob, b"alpha\n");
        let b_oid = Oid::of(ObjectKind::Blob, b"beta\n");
        let tree_oid = Oid([0x11; 20]);
        let commit_oid = Oid([0x22; 20]);

        put(".git/HEAD", b"ref: refs/heads/main\n");
        put(".git/refs/heads/main", format!("{commit_oid}\n").as_bytes());
        put(".git/refs/remotes/origin/main", format!("{commit_oid}\n").as_bytes());
        put(
            ".git/config",
            b"[core]\n  filemode = false\n[branch \"main\"]\n  remote = origin\n  merge = refs/heads/main\n",
        );
        put(".git/logs/refs/stash", b"0000 1111 stash@{0}\n");
        put(".git/objects/info/commit-graph", &make_single_commit_graph(commit_oid, tree_oid));
        put(".gitignore", b"*.ignored\n");
        put("a.txt", b"alpha\n");
        put("b.txt", b"EDITED\n");
        put("new.rs", b"fn main() {}\n");
        put("skip.ignored", b"bye\n");

        let idx = make_v2_index(
            &[
                (".gitignore", Oid::of(ObjectKind::Blob, b"*.ignored\n"), 10, 0, 0),
                ("a.txt", a_oid, 6, 0, 0),
                ("b.txt", b_oid, 5, 0, 0),
                ("gone.txt", a_oid, 6, 0, 0),
            ],
            Some(tree_oid),
        );
        put(".git/index", &idx);

        let repo = Repo::open(&cx, &root.read_only()).expect("open repo");
        let alloc = blaze_own::testing::heap().alloc();
        let status = repo.status(&cx, StatusOpts::STARSHIP, alloc).expect("status");

        assert_eq!(status.head.branch_name(), Some("main"));
        assert_eq!(status.head.oid(), Some(commit_oid));
        assert_eq!(status.upstream.as_ref().unwrap().divergence, Divergence::Equal);
        assert_eq!(status.counts.modified, 1, "b.txt edited");
        assert_eq!(status.counts.deleted, 1, "gone.txt missing");
        assert_eq!(status.counts.untracked, 1, "new.rs untracked, skip.ignored ignored");
        assert_eq!(status.counts.stashed, 1);
        assert_eq!(status.counts.staged(), 0);

        let mut rendered = String::new();
        format_status(&status, &PromptStyle::STARSHIP, &mut rendered).unwrap();
        assert_eq!(rendered, "main [$✘!?]");
    }

    /// Multi-seed DST over `SimWorld`: random file edits, deletions, untracked files, ignore rules,
    /// and simulator seeds/latencies must always produce exact Starship counts matching the oracle.
    #[test]
    fn simulated_world_dst_multi_seed_matches_oracle() {
        blaze_sim::check(blaze_sim::Config::cases(32), |tape| {
            let sim_seed = tape.next_u64("sim seed");
            let io = Io::simulated(SimWorld::new(sim_seed, IoLimits::USUAL));
            let cx = io.cx(BUDGET, TASK_FRAME).expect("root cx");
            let fs = io.need_fs().expect("fs");
            let root = fs.root();

            let put = |rel: &str, bytes: &[u8]| {
                let p = Path::new(rel).expect("path");
                root.publish(&cx, &p, Content::bytes(bytes), Durable::No).expect("publish");
            };

            let tree_oid = Oid([0x33; 20]);
            let commit_oid = Oid([0x44; 20]);
            put(".git/HEAD", b"ref: refs/heads/main\n");
            put(".git/refs/heads/main", format!("{commit_oid}\n").as_bytes());
            put(".git/config", b"[core]\n  filemode = false\n");
            put(".git/objects/info/commit-graph", &make_single_commit_graph(commit_oid, tree_oid));
            put(".gitignore", b"*.bak\n");

            let n_tracked = tape.int_in(1..=6, "tracked") as usize;
            let n_untracked = tape.int_in(0..=4, "untracked") as usize;
            let mut idx_rows: Vec<(String, Oid, u32)> = Vec::new();
            idx_rows.push((String::from(".gitignore"), Oid::of(ObjectKind::Blob, b"*.bak\n"), 6));

            let mut expect_mod = 0u32;
            let mut expect_del = 0u32;
            for i in 0..n_tracked {
                let name = format!("f{i}.txt");
                let orig = format!("orig_{i}");
                let oid = Oid::of(ObjectKind::Blob, orig.as_bytes());
                idx_rows.push((name.clone(), oid, orig.len() as u32));
                match tape.below(3, "tracked op") {
                    0 => put(&name, orig.as_bytes()),
                    1 => {
                        put(&name, format!("MOD_{i}_changed").as_bytes());
                        expect_mod += 1;
                    }
                    _ => {
                        expect_del += 1;
                    }
                }
            }

            let mut expect_untracked = 0u32;
            for i in 0..n_untracked {
                if tape.coin("ignored") {
                    put(&format!("tmp{i}.bak"), b"ignored");
                } else {
                    put(&format!("u{i}.rs"), b"new");
                    expect_untracked += 1;
                }
            }

            idx_rows.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
            let borrowed: Vec<(&str, Oid, u32, u32, u32)> =
                idx_rows.iter().map(|(p, o, sz)| (p.as_str(), *o, *sz, 0, 0)).collect();
            put(".git/index", &make_v2_index(&borrowed, Some(tree_oid)));

            let repo = Repo::open(&cx, &root.read_only()).expect("open");
            let alloc = blaze_own::testing::heap().alloc();
            let status = repo.status(&cx, StatusOpts::STARSHIP, alloc).expect("status");

            assert_eq!(status.counts.modified, expect_mod, "modified mismatch");
            assert_eq!(status.counts.deleted, expect_del, "deleted mismatch");
            assert_eq!(status.counts.untracked, expect_untracked, "untracked mismatch");
            assert_eq!(status.counts.staged(), 0);
        });
    }
}
