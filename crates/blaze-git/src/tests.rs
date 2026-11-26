//! Unit tests, reference model oracle, `blaze_sim` property/DST tests, and planted-bug controls
//! for `blaze-git`.

use std::collections::BTreeMap;
use std::format;
use std::string::String as StdString;
use std::vec::Vec as StdVec;

use blaze_git_fetch::{Kind as ObjectKind, Oid};
use blaze_own::{
    Alloc, Vec,
    planted::{NoPlant, Planted},
};
use blaze_sim::{Config, Tape};

fn test_alloc() -> Alloc<'static> {
    blaze_own::testing::heap().alloc()
}

use crate::{
    Divergence, FileKind, FileStat, GitStatusPlant, Head, Limits, PromptStatus, PromptStyle,
    RepoState, StatusCounts, StatusMachine, StatusOpts, Step, UntrackedMode, WorktreeKind,
    commit_graph::CommitGraphLayer,
    format::format_status,
    ignore::IgnoreStack,
    index::parse_index,
    machine::{DirListing, FileData, Got, PathScope, RangeData, StatEntry, Want},
    refs::{RepoConfig, count_stashes, parse_config, parse_gitdir_file, parse_head},
};

/// Minimal in-memory repository fixture driving [`StatusMachine`] with zero OS IO.
struct MemRepo {
    worktree: BTreeMap<StdString, (FileStat, StdVec<u8>)>,
    links: BTreeMap<StdString, (FileStat, StdVec<u8>)>,
    gitdir: BTreeMap<StdString, (FileStat, StdVec<u8>)>,
    commondir: BTreeMap<StdString, (FileStat, StdVec<u8>)>,
    worktree_kind: WorktreeKind,
    /// Counters for verifying hardware-floor IO efficiency.
    dir_lists: u32,
    stat_calls: u32,
    file_reads: u32,
}

impl MemRepo {
    fn new() -> MemRepo {
        MemRepo {
            worktree: BTreeMap::new(),
            links: BTreeMap::new(),
            gitdir: BTreeMap::new(),
            commondir: BTreeMap::new(),
            worktree_kind: WorktreeKind::Normal,
            dir_lists: 0,
            stat_calls: 0,
            file_reads: 0,
        }
    }

    fn put_git(&mut self, path: &str, bytes: &[u8], mtime_sec: i64) {
        let st = file_stat(bytes.len() as u64, mtime_sec, 0, 1000, false);
        self.gitdir.insert(StdString::from(path), (st, bytes.to_vec()));
        if self.worktree_kind != WorktreeKind::LinkedWorktree {
            self.commondir.insert(StdString::from(path), (st, bytes.to_vec()));
        }
    }

    fn put_common(&mut self, path: &str, bytes: &[u8]) {
        let st = file_stat(bytes.len() as u64, 100, 0, 2000, false);
        self.commondir.insert(StdString::from(path), (st, bytes.to_vec()));
    }

    fn put_work(&mut self, path: &str, bytes: &[u8], mtime_sec: i64, ino: u64, exec: bool) {
        let st = file_stat(bytes.len() as u64, mtime_sec, 0, ino, exec);
        self.worktree.insert(StdString::from(path), (st, bytes.to_vec()));
    }

    fn put_link(&mut self, path: &str, target: &[u8], mtime_sec: i64, ino: u64) {
        let st = FileStat {
            kind: FileKind::Symlink,
            size: target.len() as u64,
            mtime_sec,
            mtime_nsec: 0,
            ctime_sec: mtime_sec,
            ctime_nsec: 0,
            ino,
            dev: 1,
            mode: 0o120000,
        };
        self.links.insert(StdString::from(path), (st, target.to_vec()));
    }

    fn put_loose_object(&mut self, kind: ObjectKind, body: &[u8]) -> Oid {
        let oid = Oid::of(kind, body);
        let mut raw = StdVec::new();
        raw.extend_from_slice(kind.name().as_bytes());
        raw.push(b' ');
        raw.extend_from_slice(format!("{}", body.len()).as_bytes());
        raw.push(0);
        raw.extend_from_slice(body);
        let compressed = zlib_compress(&raw);
        let hex = oid.hex();
        let hex_s = core::str::from_utf8(&hex).unwrap();
        let path = format!("objects/{}/{}", &hex_s[..2], &hex_s[2..]);
        self.put_common(&path, &compressed);
        oid
    }

    fn lookup_file(&self, scope: PathScope, path: &str) -> Option<&(FileStat, StdVec<u8>)> {
        match scope {
            PathScope::Worktree => self.worktree.get(path),
            PathScope::GitDir => self.gitdir.get(path),
            PathScope::CommonDir => self.commondir.get(path),
        }
    }

    fn stat_path(&self, scope: PathScope, path: &str) -> FileStat {
        if scope == PathScope::Worktree {
            if let Some((st, _)) = self.links.get(path) {
                return *st;
            }
            if let Some((st, _)) = self.worktree.get(path) {
                return *st;
            }
            let prefix = format!("{path}/");
            if self.worktree.keys().any(|k| k.starts_with(&prefix))
                || self.links.keys().any(|k| k.starts_with(&prefix))
            {
                return dir_stat();
            }
            return FileStat::GONE;
        }
        self.lookup_file(scope, path).map_or(FileStat::GONE, |(st, _)| *st)
    }

    fn list_dir(&self, scope: PathScope, dir: &str) -> Option<StdVec<(StdString, FileStat)>> {
        let mut entries: BTreeMap<StdString, FileStat> = BTreeMap::new();
        let prefix = if dir.is_empty() { StdString::new() } else { format!("{dir}/") };
        let scan = |map: &BTreeMap<StdString, (FileStat, StdVec<u8>)>, out: &mut BTreeMap<StdString, FileStat>| {
            for (k, (st, _)) in map {
                if let Some(rest) = k.strip_prefix(&prefix) {
                    if let Some((sub, _)) = rest.split_once('/') {
                        out.entry(StdString::from(sub)).or_insert_with(dir_stat);
                    } else if !rest.is_empty() {
                        out.insert(StdString::from(rest), *st);
                    }
                }
            }
        };
        match scope {
            PathScope::Worktree => {
                scan(&self.worktree, &mut entries);
                scan(&self.links, &mut entries);
            }
            PathScope::GitDir => scan(&self.gitdir, &mut entries),
            PathScope::CommonDir => scan(&self.commondir, &mut entries),
        }
        if entries.is_empty() && !dir.is_empty() {
            return None;
        }
        Some(entries.into_iter().collect())
    }

    fn drive<'a, P: Planted<GitStatusPlant>>(
        &mut self,
        opts: StatusOpts,
        alloc: Alloc<'a>,
        plant: P,
    ) -> Result<PromptStatus<'a>, crate::GitError> {
        let mut m = StatusMachine::new(opts, self.worktree_kind, alloc, plant);
        let mut steps = 0u32;
        loop {
            steps += 1;
            assert!(steps < 256, "state machine exceeded step bound");
            match m.poll()? {
                Step::Done(status) => return Ok(status),
                Step::Need(want) => {
                    let got = match want {
                        Want::ReadFiles(paths) => {
                            self.file_reads += paths.len() as u32;
                            let mut out = Vec::with_capacity_in(paths.len(), alloc);
                            for sp in paths.iter() {
                                if let Some((_, data)) = self.lookup_file(sp.scope, sp.path.as_str()) {
                                    let mut v = Vec::with_capacity_in(data.len(), alloc);
                                    v.extend_from_slice(data);
                                    out.push(Some(v));
                                } else {
                                    out.push(None);
                                }
                            }
                            Got::Files(out)
                        }
                        Want::ReadWithStat(paths) => {
                            self.file_reads += paths.len() as u32;
                            let mut out = Vec::with_capacity_in(paths.len(), alloc);
                            for sp in paths.iter() {
                                if let Some((st, data)) = self.lookup_file(sp.scope, sp.path.as_str()) {
                                    let mut v = Vec::with_capacity_in(data.len(), alloc);
                                    v.extend_from_slice(data);
                                    out.push(FileData { bytes: Some(v), stat: Some(*st) });
                                } else {
                                    out.push(FileData { bytes: None, stat: None });
                                }
                            }
                            Got::FilesWithStat(out)
                        }
                        Want::ListDirs(dirs) => {
                            self.dir_lists += dirs.len() as u32;
                            let mut out = Vec::with_capacity_in(dirs.len(), alloc);
                            for sp in dirs.iter() {
                                if let Some(list) = self.list_dir(sp.scope, sp.path.as_str()) {
                                    let mut dl = DirListing::with_capacity(list.len(), list.len() * 16, alloc);
                                    for (name, stat) in list {
                                        dl.push(&name, stat);
                                    }
                                    out.push(Some(dl));
                                } else {
                                    out.push(None);
                                }
                            }
                            Got::Dirs(out)
                        }
                        Want::StatPaths(paths) => {
                            self.stat_calls += paths.len() as u32;
                            let mut out = Vec::with_capacity_in(paths.len(), alloc);
                            for sp in paths.iter() {
                                out.push(StatEntry { stat: self.stat_path(sp.scope, sp.path.as_str()) });
                            }
                            Got::Stats(out)
                        }
                        Want::ReadLinks(paths) => {
                            let mut out = Vec::with_capacity_in(paths.len(), alloc);
                            for sp in paths.iter() {
                                if let Some((_, data)) = self.links.get(sp.path.as_str()) {
                                    let mut v = Vec::with_capacity_in(data.len(), alloc);
                                    v.extend_from_slice(data);
                                    out.push(Some(v));
                                } else {
                                    out.push(None);
                                }
                            }
                            Got::Links(out)
                        }
                        Want::ReadRanges(reads) => {
                            let mut out = Vec::with_capacity_in(reads.len(), alloc);
                            for rr in reads.iter() {
                                if let Some((_, data)) = self.lookup_file(rr.scope, rr.path.as_str()) {
                                    let s = rr.offset as usize;
                                    let e = (s + rr.len as usize).min(data.len());
                                    if s <= e {
                                        let mut v = Vec::with_capacity_in(e - s, alloc);
                                        v.extend_from_slice(&data[s..e]);
                                        out.push(RangeData { bytes: Some(v) });
                                        continue;
                                    }
                                }
                                out.push(RangeData { bytes: None });
                            }
                            Got::Ranges(out)
                        }
                    };
                    m.feed(got)?;
                }
            }
        }
    }
}

fn file_stat(size: u64, mtime_sec: i64, mtime_nsec: u32, ino: u64, exec: bool) -> FileStat {
    FileStat {
        kind: FileKind::File,
        size,
        mtime_sec,
        mtime_nsec,
        ctime_sec: mtime_sec,
        ctime_nsec: mtime_nsec,
        ino,
        dev: 1,
        mode: if exec { 0o100755 } else { 0o100644 },
    }
}

fn dir_stat() -> FileStat {
    FileStat {
        kind: FileKind::Dir,
        size: 0,
        mtime_sec: 100,
        mtime_nsec: 0,
        ctime_sec: 100,
        ctime_nsec: 0,
        ino: 1,
        dev: 1,
        mode: 0o040755,
    }
}

/// Minimal stored-block (uncompressed) valid RFC 1950 zlib stream for tests (no external crate).
fn zlib_compress(data: &[u8]) -> StdVec<u8> {
    let mut out = StdVec::new();
    // Zlib header: CMF=0x78 (deflate, 32K window), FLG=0x01 (check bits valid).
    out.push(0x78);
    out.push(0x01);
    let mut at = 0;
    loop {
        let chunk_len = (data.len() - at).min(65535);
        let is_last = at + chunk_len == data.len();
        out.push(if is_last { 0x01 } else { 0x00 });
        let len16 = chunk_len as u16;
        let nlen16 = !len16;
        out.extend_from_slice(&len16.to_le_bytes());
        out.extend_from_slice(&nlen16.to_le_bytes());
        out.extend_from_slice(&data[at..at + chunk_len]);
        at += chunk_len;
        if is_last {
            break;
        }
    }
    // Adler-32 checksum.
    let mut a: u32 = 1;
    let mut b: u32 = 0;
    for &byte in data {
        a = (a + u32::from(byte)) % 65521;
        b = (b + a) % 65521;
    }
    let adler = (b << 16) | a;
    out.extend_from_slice(&adler.to_be_bytes());
    out
}

/// Helper to build a valid Git commit object body.
fn make_commit_body(tree: Oid, parents: &[Oid], timestamp: i64) -> StdVec<u8> {
    let mut s = format!("tree {}\n", tree);
    for p in parents {
        s.push_str(&format!("parent {}\n", p));
    }
    s.push_str(&format!("author Test <t@example.com> {timestamp} +0000\n"));
    s.push_str(&format!("committer Test <t@example.com> {timestamp} +0000\n\nmsg\n"));
    s.into_bytes()
}

/// Helper to build a valid Git tree object body from `(mode, name, oid)` entries.
fn make_tree_body(entries: &[(u32, &str, Oid)]) -> StdVec<u8> {
    let mut sorted: StdVec<(u32, &str, Oid)> = entries.to_vec();
    sorted.sort_by(|a, b| {
        let ka = if a.0 == 0o040000 { format!("{}/", a.1) } else { StdString::from(a.1) };
        let kb = if b.0 == 0o040000 { format!("{}/", b.1) } else { StdString::from(b.1) };
        ka.as_bytes().cmp(kb.as_bytes())
    });
    let mut out = StdVec::new();
    for (mode, name, oid) in sorted {
        out.extend_from_slice(format!("{mode:o} {name}").as_bytes());
        out.push(0);
        out.extend_from_slice(&oid.0);
    }
    out
}

/// One entry descriptor for [`build_index`].
#[derive(Clone)]
struct RawIdxEntry<'a> {
    path: &'a str,
    oid: Oid,
    size: u32,
    mtime_sec: u32,
    ino: u32,
    mode: u32,
    stage: u8,
}

/// Build a valid `.git/index` (version 2 or 4) with optional `TREE` cache-tree extension.
fn build_index(version: u32, entries: &[RawIdxEntry<'_>], root_cache_tree: Option<Oid>) -> StdVec<u8> {
    let mut sorted = entries.to_vec();
    sorted.sort_by(|a, b| a.path.as_bytes().cmp(b.path.as_bytes()).then_with(|| a.stage.cmp(&b.stage)));

    let mut out = StdVec::new();
    out.extend_from_slice(b"DIRC");
    out.extend_from_slice(&version.to_be_bytes());
    out.extend_from_slice(&(sorted.len() as u32).to_be_bytes());

    let mut prev_path = StdVec::<u8>::new();
    for e in &sorted {
        let start = out.len();
        out.extend_from_slice(&e.mtime_sec.to_be_bytes()); // ctime_sec
        out.extend_from_slice(&0u32.to_be_bytes()); // ctime_nsec
        out.extend_from_slice(&e.mtime_sec.to_be_bytes()); // mtime_sec
        out.extend_from_slice(&0u32.to_be_bytes()); // mtime_nsec
        out.extend_from_slice(&1u32.to_be_bytes()); // dev
        out.extend_from_slice(&e.ino.to_be_bytes()); // ino
        out.extend_from_slice(&e.mode.to_be_bytes()); // mode
        out.extend_from_slice(&0u32.to_be_bytes()); // uid
        out.extend_from_slice(&0u32.to_be_bytes()); // gid
        out.extend_from_slice(&e.size.to_be_bytes()); // size
        out.extend_from_slice(&e.oid.0); // oid
        let name_len = (e.path.len().min(0x0fff)) as u16;
        let flags = (u16::from(e.stage) << 12) | name_len;
        out.extend_from_slice(&flags.to_be_bytes());

        if version == 4 {
            let cur = e.path.as_bytes();
            let common = prev_path.iter().zip(cur.iter()).take_while(|(a, b)| a == b).count();
            let drop = prev_path.len() - common;
            // Encode drop as Git varint.
            let mut varint = StdVec::new();
            let mut val = drop as u64;
            varint.push((val & 0x7f) as u8);
            val >>= 7;
            while val > 0 {
                val -= 1;
                varint.push(((val & 0x7f) as u8) | 0x80);
                val >>= 7;
            }
            varint.reverse();
            out.extend_from_slice(&varint);
            out.extend_from_slice(&cur[common..]);
            out.push(0);
            prev_path.clear();
            prev_path.extend_from_slice(cur);
        } else {
            out.extend_from_slice(e.path.as_bytes());
            out.push(0);
            while (out.len() - start) % 8 != 0 {
                out.push(0);
            }
        }
    }

    if let Some(root_oid) = root_cache_tree {
        let mut ext = StdVec::new();
        ext.push(0); // empty root path
        ext.extend_from_slice(format!("{} 0\n", sorted.len()).as_bytes());
        ext.extend_from_slice(&root_oid.0);
        out.extend_from_slice(b"TREE");
        out.extend_from_slice(&(ext.len() as u32).to_be_bytes());
        out.extend_from_slice(&ext);
    }

    // 20-byte SHA-1 trailer.
    let hash = blaze_git_fetch::sha1::sha1(&out);
    out.extend_from_slice(&hash);
    out
}

/// Build a valid binary `CGPH` v1 commit-graph file from `(oid, tree, parents, gen, time)`.
fn build_commit_graph(commits: &[(Oid, Oid, StdVec<Oid>, u32, u64)]) -> StdVec<u8> {
    let mut sorted = commits.to_vec();
    sorted.sort_by_key(|c| c.0);

    let find_pos = |target: Oid| -> u32 {
        sorted.binary_search_by_key(&target, |c| c.0).expect("parent in graph") as u32
    };

    let n = sorted.len();
    let mut oidf = [0u32; 256];
    for c in &sorted {
        for b in usize::from(c.0.0[0])..256 {
            oidf[b] += 1;
        }
    }

    let mut oidl = StdVec::with_capacity(n * 20);
    let mut cdat = StdVec::with_capacity(n * 36);
    let mut edge = StdVec::<u8>::new();

    for (_, tree, parents, generation, time) in &sorted {
        cdat.extend_from_slice(&tree.0);
        let p1 = parents.first().map_or(0x7000_0000, |&p| find_pos(p));
        let p2 = match parents.len() {
            0 | 1 => 0x7000_0000,
            2 => find_pos(parents[1]),
            _ => {
                let edge_idx = (edge.len() / 4) as u32;
                for (i, &p) in parents[1..].iter().enumerate() {
                    let mut val = find_pos(p);
                    if i + 1 == parents.len() - 1 {
                        val |= 0x8000_0000;
                    }
                    edge.extend_from_slice(&val.to_be_bytes());
                }
                edge_idx | 0x8000_0000
            }
        };
        cdat.extend_from_slice(&p1.to_be_bytes());
        cdat.extend_from_slice(&p2.to_be_bytes());
        let gen_date = (u64::from(*generation) << 34) | (*time & 0x0000_0003_ffff_ffff);
        cdat.extend_from_slice(&gen_date.to_be_bytes());
    }
    for (oid, ..) in &sorted {
        oidl.extend_from_slice(&oid.0);
    }

    let has_edge = !edge.is_empty();
    let num_chunks: u8 = if has_edge { 4 } else { 3 };
    let header_len = 8 + (usize::from(num_chunks) + 1) * 12;
    let oidf_off = header_len as u64;
    let oidl_off = oidf_off + 1024;
    let cdat_off = oidl_off + (oidl.len() as u64);
    let edge_off = cdat_off + (cdat.len() as u64);
    let end_off = edge_off + (edge.len() as u64);

    let mut out = StdVec::new();
    out.extend_from_slice(b"CGPH");
    out.push(1); // version
    out.push(1); // SHA-1
    out.push(num_chunks);
    out.push(0); // base graphs

    out.extend_from_slice(b"OIDF");
    out.extend_from_slice(&oidf_off.to_be_bytes());
    out.extend_from_slice(b"OIDL");
    out.extend_from_slice(&oidl_off.to_be_bytes());
    out.extend_from_slice(b"CDAT");
    out.extend_from_slice(&cdat_off.to_be_bytes());
    if has_edge {
        out.extend_from_slice(b"EDGE");
        out.extend_from_slice(&edge_off.to_be_bytes());
    }
    out.extend_from_slice(&[0u8; 4]);
    out.extend_from_slice(&end_off.to_be_bytes());

    for c in oidf {
        out.extend_from_slice(&c.to_be_bytes());
    }
    out.extend_from_slice(&oidl);
    out.extend_from_slice(&cdat);
    out.extend_from_slice(&edge);
    let trailer = blaze_git_fetch::sha1::sha1(&out);
    out.extend_from_slice(&trailer);
    out
}

// ---------------------------------------------------------------------------
// Core unit tests
// ---------------------------------------------------------------------------

#[test]
fn clean_repo_with_commit_graph_and_cache_tree_does_zero_lstats_and_zero_object_reads() {
    let alloc = test_alloc();

    let mut repo = MemRepo::new();
    let blob_a = Oid::of(ObjectKind::Blob, b"hello\n");
    let blob_b = Oid::of(ObjectKind::Blob, b"world\n");
    let tree_oid = Oid::of(ObjectKind::Tree, &make_tree_body(&[(0o100644, "a.txt", blob_a), (0o100644, "b.txt", blob_b)]));
    let commit_oid = Oid::of(ObjectKind::Commit, &make_commit_body(tree_oid, &[], 1_700_000_000));

    repo.put_git("HEAD", b"ref: refs/heads/main\n", 100);
    repo.put_common("refs/heads/main", format!("{commit_oid}\n").as_bytes());
    repo.put_common("refs/remotes/origin/main", format!("{commit_oid}\n").as_bytes());
    repo.put_common(
        "config",
        b"[core]\n\tfilemode = true\n[branch \"main\"]\n\tremote = origin\n\tmerge = refs/heads/main\n",
    );
    let cg = build_commit_graph(&[(commit_oid, tree_oid, StdVec::new(), 1, 1_700_000_000)]);
    repo.put_common("objects/info/commit-graph", &cg);

    let idx = build_index(
        2,
        &[
            RawIdxEntry { path: "a.txt", oid: blob_a, size: 6, mtime_sec: 50, ino: 10, mode: 0o100644, stage: 0 },
            RawIdxEntry { path: "b.txt", oid: blob_b, size: 6, mtime_sec: 50, ino: 11, mode: 0o100644, stage: 0 },
        ],
        Some(tree_oid),
    );
    repo.put_git("index", &idx, 100);
    repo.put_work("a.txt", b"hello\n", 50, 10, false);
    repo.put_work("b.txt", b"world\n", 50, 11, false);

    let status = repo.drive(StatusOpts::STARSHIP, alloc, NoPlant).expect("status ok");
    assert_eq!(status.head.branch_name(), Some("main"));
    assert_eq!(status.head.oid(), Some(commit_oid));
    assert_eq!(status.state, RepoState::Clean);
    assert_eq!(status.counts, StatusCounts::default());
    assert_eq!(status.upstream.as_ref().unwrap().divergence, Divergence::Equal);
    // Hardware-floor proof: 0 individual lstats (all verified via bulk directory listing)!
    assert_eq!(repo.stat_calls, 0, "clean worktree should need 0 separate lstats");

    let mut prompt = StdString::new();
    format_status(&status, &PromptStyle::STARSHIP, &mut prompt).unwrap();
    assert_eq!(prompt, "main");
}

#[test]
fn index_v4_conflicts_staged_modified_deleted_untracked_stashed_and_format() {
    let alloc = test_alloc();

    let mut repo = MemRepo::new();
    let blob_keep = Oid::of(ObjectKind::Blob, b"keep");
    let blob_mod_old = Oid::of(ObjectKind::Blob, b"old");
    let blob_mod_new = Oid::of(ObjectKind::Blob, b"new_staged");
    let blob_del_head = Oid::of(ObjectKind::Blob, b"gone_in_index");
    let blob_add_idx = Oid::of(ObjectKind::Blob, b"added_in_index");
    let blob_wt_mod = Oid::of(ObjectKind::Blob, b"before_wt_edit");
    let blob_wt_del = Oid::of(ObjectKind::Blob, b"deleted_on_disk");

    let head_tree_body = make_tree_body(&[
        (0o100644, "dir/keep.txt", blob_keep),
        (0o100644, "dir/staged_mod.txt", blob_mod_old),
        (0o100644, "dir/staged_del.txt", blob_del_head),
        (0o100644, "wt_mod.txt", blob_wt_mod),
        (0o100644, "wt_del.txt", blob_wt_del),
    ]);
    // Build proper two-level tree: `dir` subtree + root tree.
    let dir_tree_oid = repo.put_loose_object(
        ObjectKind::Tree,
        &make_tree_body(&[
            (0o100644, "keep.txt", blob_keep),
            (0o100644, "staged_del.txt", blob_del_head),
            (0o100644, "staged_mod.txt", blob_mod_old),
        ]),
    );
    let _ = head_tree_body;
    let root_tree_oid = repo.put_loose_object(
        ObjectKind::Tree,
        &make_tree_body(&[
            (0o040000, "dir", dir_tree_oid),
            (0o100644, "wt_del.txt", blob_wt_del),
            (0o100644, "wt_mod.txt", blob_wt_mod),
        ]),
    );
    let commit_oid = repo.put_loose_object(
        ObjectKind::Commit,
        &make_commit_body(root_tree_oid, &[], 1_700_000_000),
    );

    repo.put_git("HEAD", b"ref: refs/heads/feat/starship\n", 100);
    repo.put_common("refs/heads/feat/starship", format!("{commit_oid}\n").as_bytes());
    repo.put_common("logs/refs/stash", b"0000 1111 a\n1111 2222 b\n");
    repo.put_git("MERGE_HEAD", format!("{commit_oid}\n").as_bytes(), 100);

    // Index v4 (prefix compressed) with 1 conflict (stages 1, 2, 3), staged add/mod/del, wt mod/del.
    let idx = build_index(
        4,
        &[
            RawIdxEntry { path: "conflict.rs", oid: blob_keep, size: 4, mtime_sec: 50, ino: 1, mode: 0o100644, stage: 1 },
            RawIdxEntry { path: "conflict.rs", oid: blob_mod_old, size: 3, mtime_sec: 50, ino: 1, mode: 0o100644, stage: 2 },
            RawIdxEntry { path: "conflict.rs", oid: blob_mod_new, size: 10, mtime_sec: 50, ino: 1, mode: 0o100644, stage: 3 },
            RawIdxEntry { path: "dir/keep.txt", oid: blob_keep, size: 4, mtime_sec: 50, ino: 2, mode: 0o100644, stage: 0 },
            RawIdxEntry { path: "dir/staged_add.txt", oid: blob_add_idx, size: 14, mtime_sec: 50, ino: 3, mode: 0o100644, stage: 0 },
            RawIdxEntry { path: "dir/staged_mod.txt", oid: blob_mod_new, size: 10, mtime_sec: 50, ino: 4, mode: 0o100644, stage: 0 },
            RawIdxEntry { path: "wt_del.txt", oid: blob_wt_del, size: 15, mtime_sec: 50, ino: 5, mode: 0o100644, stage: 0 },
            RawIdxEntry { path: "wt_mod.txt", oid: blob_wt_mod, size: 14, mtime_sec: 50, ino: 6, mode: 0o100644, stage: 0 },
        ],
        None,
    );
    repo.put_git("index", &idx, 100);
    repo.put_work(".gitignore", b"*.log\nignored_dir/\n!keep.log\n", 50, 99, false);
    repo.put_work("dir/keep.txt", b"keep", 50, 2, false);
    repo.put_work("dir/staged_add.txt", b"added_in_index", 50, 3, false);
    repo.put_work("dir/staged_mod.txt", b"new_staged", 50, 4, false);
    repo.put_work("wt_mod.txt", b"EDITED_ON_DISK", 60, 6, false);
    // Untracked files & ignore rules:
    repo.put_work("debug.log", b"ignored", 50, 20, false);
    repo.put_work("keep.log", b"negated_so_untracked", 50, 21, false);
    repo.put_work("ignored_dir/foo.txt", b"ignored", 50, 22, false);
    repo.put_work("new_dir/a.txt", b"collapsed_1", 50, 23, false);
    repo.put_work("new_dir/b.txt", b"collapsed_2", 50, 24, false);

    let status = repo.drive(StatusOpts::STARSHIP, alloc, NoPlant).expect("status ok");
    assert_eq!(status.head.branch_name(), Some("feat/starship"));
    assert_eq!(status.state, RepoState::Merge);
    assert_eq!(status.counts.conflicted, 1);
    assert_eq!(status.counts.staged_added, 1);
    assert_eq!(status.counts.staged_modified, 1);
    assert_eq!(status.counts.staged_deleted, 1);
    assert_eq!(status.counts.staged(), 3);
    assert_eq!(status.counts.modified, 1);
    assert_eq!(status.counts.deleted, 1);
    // `.gitignore` + `keep.log` + `new_dir/` (collapsed) = 3 untracked!
    assert_eq!(status.counts.untracked, 3);
    assert_eq!(status.counts.stashed, 2);

    let mut prompt = StdString::new();
    format_status(&status, &PromptStyle::STARSHIP, &mut prompt).unwrap();
    assert_eq!(prompt, "feat/starship (MERGING) [=$2✘!+3?3]");

    // Verify `Detail::Full` collects every exact per-path porcelain XY entry (including UU,
    // staged add/mod/del, unstaged mod/del, collapsed untracked dir, and simultaneous MM!).
    repo.put_work("dir/staged_mod.txt", b"staged_then_edited_again", 70, 4, false);
    repo.put_work("conflict.rs", b"<<<<<<< ours\n=======\n>>>>>>> theirs\n", 50, 1, false);
    let full = repo.drive(StatusOpts::FULL, alloc, NoPlant).expect("full status ok");
    let mut porcelain = StdString::new();
    crate::format_porcelain_v1(&full, true, &mut porcelain).unwrap();
    assert_eq!(
        porcelain,
        "## feat/starship\n\
         UU conflict.rs\n\
         A  dir/staged_add.txt\n\
         D  dir/staged_del.txt\n\
         MM dir/staged_mod.txt\n\
         \x20D wt_del.txt\n\
         \x20M wt_mod.txt\n\
         ?? .gitignore\n\
         ?? keep.log\n\
         ?? new_dir/\n"
    );
    let mut long_out = StdString::new();
    crate::format_long_status(&full, false, &mut long_out).unwrap();
    assert!(long_out.contains("On branch feat/starship"));
    assert!(long_out.contains("Changes to be committed:"));
    assert!(long_out.contains("Unmerged paths:"));
    assert!(long_out.contains("both modified:   conflict.rs"));
    assert!(long_out.contains("Changes not staged for commit:"));
    assert!(long_out.contains("Untracked files:"));
}

#[test]
fn ahead_behind_via_commit_graph_and_detached_peeled_tag() {
    let alloc = test_alloc();

    let mut repo = MemRepo::new();
    let t = Oid::of(ObjectKind::Tree, &[]);
    // DAG:
    //   c0 <- c1 <- c2 (upstream)
    //          \<- c3 <- c4 <- c5 (head)
    // Expect: ahead = 3 (c3, c4, c5), behind = 1 (c2).
    let c0 = Oid::of(ObjectKind::Commit, &make_commit_body(t, &[], 100));
    let c1 = Oid::of(ObjectKind::Commit, &make_commit_body(t, &[c0], 200));
    let c2 = Oid::of(ObjectKind::Commit, &make_commit_body(t, &[c1], 300));
    let c3 = Oid::of(ObjectKind::Commit, &make_commit_body(t, &[c1], 350));
    let c4 = Oid::of(ObjectKind::Commit, &make_commit_body(t, &[c3], 400));
    let c5 = Oid::of(ObjectKind::Commit, &make_commit_body(t, &[c4], 500));

    let cg = build_commit_graph(&[
        (c0, t, StdVec::new(), 1, 100),
        (c1, t, std::vec![c0], 2, 200),
        (c2, t, std::vec![c1], 3, 300),
        (c3, t, std::vec![c1], 3, 350),
        (c4, t, std::vec![c3], 4, 400),
        (c5, t, std::vec![c4], 5, 500),
    ]);
    repo.put_common("objects/info/commit-graph", &cg);
    repo.put_git("HEAD", b"ref: refs/heads/dev\n", 100);
    repo.put_common("refs/heads/dev", format!("{c5}\n").as_bytes());
    repo.put_common("refs/remotes/origin/dev", format!("{c2}\n").as_bytes());
    repo.put_common(
        "config",
        b"[branch \"dev\"]\n\tremote = origin\n\tmerge = refs/heads/dev\n",
    );
    repo.put_git("index", &build_index(2, &[], Some(t)), 100);

    let status = repo.drive(StatusOpts::STARSHIP, alloc, NoPlant).expect("status ok");
    assert_eq!(
        status.upstream.as_ref().unwrap().divergence,
        Divergence::Counted { ahead: 3, behind: 1 }
    );
    let mut prompt = StdString::new();
    format_status(&status, &PromptStyle::STARSHIP, &mut prompt).unwrap();
    assert_eq!(prompt, "dev [⇕⇡3⇣]");

    // Now test detached HEAD with peeled annotated tag in packed-refs!
    let mut repo2 = MemRepo::new();
    let tag_obj = Oid::of(ObjectKind::Tag, b"tag body");
    repo2.put_git("HEAD", format!("{c5}\n").as_bytes(), 100);
    repo2.put_common("packed-refs", format!("# pack-refs with: peeled\n{tag_obj} refs/tags/v2.0.0\n^{c5}\n").as_bytes());
    repo2.put_common("objects/info/commit-graph", &cg);
    repo2.put_git("index", &build_index(2, &[], Some(t)), 100);

    let st2 = repo2.drive(StatusOpts::STARSHIP, alloc, NoPlant).expect("detached ok");
    assert!(matches!(st2.head, Head::Detached { oid, .. } if oid == c5));
    assert_eq!(st2.tag.as_deref(), Some("v2.0.0"));
}

#[test]
fn rebase_progress_and_symlinks_and_bare_repo() {
    let alloc = test_alloc();

    let mut repo = MemRepo::new();
    let link_oid = Oid::of(ObjectKind::Blob, b"../target");
    let tree_oid = Oid::of(ObjectKind::Tree, &make_tree_body(&[(0o120000, "sym", link_oid)]));
    let c0 = Oid::of(ObjectKind::Commit, &make_commit_body(tree_oid, &[], 100));
    repo.put_common("objects/info/commit-graph", &build_commit_graph(&[(c0, tree_oid, StdVec::new(), 1, 100)]));
    repo.put_git("HEAD", format!("{c0}\n").as_bytes(), 100);
    repo.put_git("rebase-merge/msgnum", b"3\n", 100);
    repo.put_git("rebase-merge/end", b"7\n", 100);
    repo.put_git("rebase-merge/interactive", b"", 100);
    repo.put_git(
        "index",
        &build_index(
            2,
            &[RawIdxEntry { path: "sym", oid: link_oid, size: 9, mtime_sec: 50, ino: 42, mode: 0o120000, stage: 0 }],
            Some(tree_oid),
        ),
        100,
    );
    // Symlink target modified!
    repo.put_link("sym", b"../moved", 60, 42);

    let st = repo.drive(StatusOpts::STARSHIP, alloc, NoPlant).expect("rebase ok");
    assert_eq!(st.state, RepoState::Rebase { interactive: true, step: Some(3), total: Some(7) });
    assert_eq!(st.counts.modified, 1);

    // Bare repository skips worktree & index completely.
    let mut bare = MemRepo::new();
    bare.worktree_kind = WorktreeKind::Bare;
    bare.put_git("HEAD", b"ref: refs/heads/main\n", 100);
    bare.put_common("refs/heads/main", format!("{c0}\n").as_bytes());
    bare.put_common("config", b"[core]\n\tbare = true\n");
    let st_bare = bare.drive(StatusOpts::STARSHIP, alloc, NoPlant).expect("bare ok");
    assert_eq!(st_bare.worktree, WorktreeKind::Bare);
    assert_eq!(st_bare.head.branch_name(), Some("main"));
    assert_eq!(st_bare.counts, StatusCounts::default());
}

// ---------------------------------------------------------------------------
// Reference model + `blaze_sim` property tests + planted-bug controls
// ---------------------------------------------------------------------------

/// The reference model: pure sequential symmetric difference on an in-memory DAG.
fn ref_ahead_behind(
    parents_map: &BTreeMap<Oid, StdVec<Oid>>,
    head: Oid,
    upstream: Oid,
) -> (u32, u32) {
    if head == upstream {
        return (0, 0);
    }
    let reachable = |start: Oid| -> std::collections::BTreeSet<Oid> {
        let mut seen = std::collections::BTreeSet::new();
        let mut stack = std::vec![start];
        while let Some(cur) = stack.pop() {
            if seen.insert(cur)
                && let Some(ps) = parents_map.get(&cur)
            {
                for &p in ps {
                    stack.push(p);
                }
            }
        }
        seen
    };
    let h = reachable(head);
    let u = reachable(upstream);
    let ahead = h.difference(&u).count() as u32;
    let behind = u.difference(&h).count() as u32;
    (ahead, behind)
}

fn check_dag_refinement(tape: &Tape, plant: impl Planted<GitStatusPlant>) {
    let alloc = test_alloc();

    let n = tape.int_in(2..=24, "commits") as usize;
    let use_graph = tape.coin("use commit-graph");
    let t = Oid::of(ObjectKind::Tree, &[]);

    let mut repo = MemRepo::new();
    let mut oids = StdVec::with_capacity(n);
    let mut gens = StdVec::with_capacity(n);
    let mut parents_map: BTreeMap<Oid, StdVec<Oid>> = BTreeMap::new();
    let mut cg_rows = StdVec::with_capacity(n);

    for i in 0..n {
        let mut ps = StdVec::new();
        if i > 0 {
            let p1_idx = tape.below(i as u64, "p1") as usize;
            ps.push(oids[p1_idx]);
            if i > 1 && tape.coin("merge") {
                let p2_idx = tape.below(i as u64, "p2") as usize;
                if p2_idx != p1_idx {
                    ps.push(oids[p2_idx]);
                    tape.reached("merge commit");
                }
            }
        }
        let generation = ps.iter().map(|p| {
            let idx = oids.iter().position(|x| x == p).unwrap();
            gens[idx]
        }).max().unwrap_or(0) + 1;
        let ts = 1_700_000_000 + (i as i64) * 10;
        let body = make_commit_body(t, &ps, ts);
        let oid = if use_graph {
            Oid::of(ObjectKind::Commit, &body)
        } else {
            repo.put_loose_object(ObjectKind::Commit, &body)
        };
        oids.push(oid);
        gens.push(generation);
        parents_map.insert(oid, ps.clone());
        cg_rows.push((oid, t, ps, generation, ts as u64));
    }

    let head_idx = tape.below(n as u64, "head") as usize;
    let up_idx = tape.below(n as u64, "upstream") as usize;
    let head_oid = oids[head_idx];
    let up_oid = oids[up_idx];

    if use_graph {
        let cg = build_commit_graph(&cg_rows);
        // Also sanity-check layer parsing.
        assert!(CommitGraphLayer::parse(&cg, 0).is_ok());
        repo.put_common("objects/info/commit-graph", &cg);
    }
    repo.put_git("HEAD", b"ref: refs/heads/main\n", 100);
    repo.put_common("refs/heads/main", format!("{head_oid}\n").as_bytes());
    repo.put_common("refs/remotes/origin/main", format!("{up_oid}\n").as_bytes());
    repo.put_common("config", b"[branch \"main\"]\n\tremote = origin\n\tmerge = refs/heads/main\n");
    repo.put_git("index", &build_index(2, &[], Some(t)), 100);

    let (want_a, want_b) = ref_ahead_behind(&parents_map, head_oid, up_oid);
    let status = repo.drive(StatusOpts::STARSHIP, alloc, plant).expect("dag status ok");
    let div = status.upstream.as_ref().unwrap().divergence;
    assert_eq!((div.ahead(), div.behind()), (want_a, want_b), "head={head_idx} up={up_idx} use_graph={use_graph}");
}

/// The refinement: ahead/behind computed by [`StatusMachine`] (both via `CGPH` commit-graph and
/// via loose objects) equals [`ref_ahead_behind`] on every random DAG including merges.
#[test]
fn ahead_behind_matches_reference_model() {
    blaze_sim::check(Config::cases(120).must_reach(&["merge commit"]), |tape| {
        check_dag_refinement(tape, NoPlant);
    });
}

/// Control: planting `SkipMergeSecondParent` is caught by [`ahead_behind_matches_reference_model`].
#[test]
#[should_panic]
fn the_oracle_catches_skip_merge_second_parent() {
    blaze_sim::check(Config::cases(120), |tape| {
        check_dag_refinement(tape, GitStatusPlant::SkipMergeSecondParent);
    });
}

fn check_racy_and_status_oracle(tape: &Tape, plant: impl Planted<GitStatusPlant>) {
    let alloc = test_alloc();

    let n_files = tape.int_in(1..=8, "files") as usize;
    let mut repo = MemRepo::new();
    let mut idx_entries = StdVec::new();
    let mut tree_entries = StdVec::new();
    let mut expect_modified = 0u32;
    let mut expect_deleted = 0u32;
    let mut expect_staged = 0u32;
    let mut expect_conflicted = 0u32;
    let mut names = StdVec::new();

    for i in 0..n_files {
        let name = format!("f{i}.txt");
        names.push(name);
    }

    for (i, name) in names.iter().enumerate() {
        let base_bytes = format!("base_{i}_len");
        let base_oid = Oid::of(ObjectKind::Blob, base_bytes.as_bytes());
        tree_entries.push((0o100644, name.as_str(), base_oid));

        let kind = tape.below(5, "file state");
        match kind {
            0 => {
                // Clean.
                idx_entries.push(RawIdxEntry {
                    path: name,
                    oid: base_oid,
                    size: base_bytes.len() as u32,
                    mtime_sec: 50,
                    ino: 10 + i as u32,
                    mode: 0o100644,
                    stage: 0,
                });
                repo.put_work(name, base_bytes.as_bytes(), 50, 10 + i as u64, false);
            }
            1 => {
                // Racy same-size edit in same second as index (mtime == index_mtime == 100)!
                tape.reached("racy edit");
                let edited = format!("EDIT_{i}_len");
                assert_eq!(edited.len(), base_bytes.len());
                idx_entries.push(RawIdxEntry {
                    path: name,
                    oid: base_oid,
                    size: base_bytes.len() as u32,
                    mtime_sec: 100,
                    ino: 10 + i as u32,
                    mode: 0o100644,
                    stage: 0,
                });
                repo.put_work(name, edited.as_bytes(), 100, 10 + i as u64, false);
                expect_modified += 1;
            }
            2 => {
                // Deleted on disk.
                idx_entries.push(RawIdxEntry {
                    path: name,
                    oid: base_oid,
                    size: base_bytes.len() as u32,
                    mtime_sec: 50,
                    ino: 10 + i as u32,
                    mode: 0o100644,
                    stage: 0,
                });
                expect_deleted += 1;
            }
            3 => {
                // Staged modification (index differs from HEAD, worktree matches index).
                tape.reached("staged edit");
                let staged_bytes = format!("staged_content_{i}");
                let staged_oid = Oid::of(ObjectKind::Blob, staged_bytes.as_bytes());
                idx_entries.push(RawIdxEntry {
                    path: name,
                    oid: staged_oid,
                    size: staged_bytes.len() as u32,
                    mtime_sec: 50,
                    ino: 10 + i as u32,
                    mode: 0o100644,
                    stage: 0,
                });
                repo.put_work(name, staged_bytes.as_bytes(), 50, 10 + i as u64, false);
                expect_staged += 1;
            }
            _ => {
                // Unmerged conflict (stages 2 & 3).
                tape.reached("conflict");
                let other_oid = Oid::of(ObjectKind::Blob, b"theirs");
                idx_entries.push(RawIdxEntry {
                    path: name,
                    oid: base_oid,
                    size: base_bytes.len() as u32,
                    mtime_sec: 50,
                    ino: 10 + i as u32,
                    mode: 0o100644,
                    stage: 2,
                });
                idx_entries.push(RawIdxEntry {
                    path: name,
                    oid: other_oid,
                    size: 6,
                    mtime_sec: 50,
                    ino: 10 + i as u32,
                    mode: 0o100644,
                    stage: 3,
                });
                expect_conflicted += 1;
            }
        }
    }

    let root_tree = repo.put_loose_object(ObjectKind::Tree, &make_tree_body(&tree_entries));
    let commit = repo.put_loose_object(ObjectKind::Commit, &make_commit_body(root_tree, &[], 50));
    repo.put_git("HEAD", b"ref: refs/heads/main\n", 100);
    repo.put_common("refs/heads/main", format!("{commit}\n").as_bytes());
    let root_ct = if expect_staged == 0 && expect_conflicted == 0 { Some(root_tree) } else { None };
    repo.put_git("index", &build_index(2, &idx_entries, root_ct), 100);

    let opts = StatusOpts { untracked: UntrackedMode::Off, ..StatusOpts::STARSHIP };
    let status = repo.drive(opts, alloc, plant).expect("status ok");
    assert_eq!(status.counts.modified, expect_modified, "modified mismatch");
    assert_eq!(status.counts.deleted, expect_deleted, "deleted mismatch");
    assert_eq!(status.counts.conflicted, expect_conflicted, "conflicted mismatch");
    if expect_conflicted == 0 {
        assert_eq!(status.counts.staged(), expect_staged, "staged mismatch");
    }
}

/// The invariant: racy edits, deletions, staged changes, and conflicts are counted identically to
/// the reference construction across random working-tree states.
#[test]
fn worktree_and_index_counts_match_oracle() {
    blaze_sim::check(
        Config::cases(100).must_reach(&["racy edit", "staged edit", "conflict"]),
        |tape| check_racy_and_status_oracle(tape, NoPlant),
    );
}

#[test]
#[should_panic]
fn the_oracle_catches_skip_racy_hash_check() {
    blaze_sim::check(Config::cases(100), |tape| {
        check_racy_and_status_oracle(tape, GitStatusPlant::SkipRacyHashCheck);
    });
}

#[test]
#[should_panic]
fn the_oracle_catches_skip_conflict_count() {
    blaze_sim::check(Config::cases(100), |tape| {
        check_racy_and_status_oracle(tape, GitStatusPlant::SkipConflictCount);
    });
}

#[test]
#[should_panic]
fn the_oracle_catches_skip_staged_diff() {
    blaze_sim::check(Config::cases(100), |tape| {
        check_racy_and_status_oracle(tape, GitStatusPlant::SkipStagedDiff);
    });
}

fn check_ignore_negation(plant: impl Planted<GitStatusPlant>) {
    let alloc = test_alloc();
    let mut stack = IgnoreStack::new(Limits::SHIPPED, alloc);
    stack.add_folder_gitignore("", b"*.tmp\n!keep.tmp\ndir/**/*.o\n", plant);
    assert!(stack.is_ignored("a.tmp", false));
    assert!(!stack.is_ignored("keep.tmp", false), "negated rule must un-ignore keep.tmp");
    assert!(stack.is_ignored("dir/sub/x.o", false));
    assert!(stack.is_ignored("dir/x.o", false));
}

#[test]
fn ignore_rules_glob_and_negation() {
    check_ignore_negation(NoPlant);
}

#[test]
#[should_panic]
fn the_oracle_catches_ignore_negation() {
    check_ignore_negation(GitStatusPlant::IgnoreNegation);
}

#[test]
fn hostile_truncated_inputs_refuse_without_panic() {
    let alloc = test_alloc();

    blaze_sim::check(Config::cases(100), |tape| {
        let len = tape.int_in(0..=128, "len") as usize;
        let mut buf = StdVec::with_capacity(len);
        for _ in 0..len {
            buf.push(tape.int_in(0..=255, "b") as u8);
        }
        let _ = parse_index(&buf, Limits::SHIPPED, alloc, NoPlant);
        let _ = CommitGraphLayer::parse(&buf, 0);
        let _ = parse_head(&buf);
        let _ = parse_gitdir_file(&buf);
        let _ = parse_config(&buf, Some("main"), alloc);
        let _ = count_stashes(&buf);
    });
}

#[test]
fn config_parser_handles_branch_upstream_and_filemode() {
    let alloc = test_alloc();

    let cfg: RepoConfig<'_> = parse_config(
        b"[core]\n  filemode = false # comment\n  autocrlf = input\n[branch \"main\"]\n  remote = upstream\n  merge = refs/heads/develop\n",
        Some("main"),
        alloc,
    );
    assert!(!cfg.filemode);
    assert_eq!(cfg.autocrlf, crate::refs::AutoCrlf::Input);
    let up = cfg.upstream_ref(alloc).expect("upstream");
    assert_eq!(up.remote.as_str(), "upstream");
    assert_eq!(up.branch.as_str(), "develop");
    assert_eq!(up.ref_path.as_str(), "refs/remotes/upstream/develop");
}
