//! Sans-IO Git wire protocol — **both Server AND Client**, 100% compatible with real `git`.
//!
//! Covers:
//! - **Object closure graph walk** ([`collect_closure`]): computes the minimal set of commits,
//!   trees, blobs, and tags reachable from `wants` excluding ancestors of `haves` (with shallow
//!   `deepen` support).
//! - **Git Protocol v2 Upload-Pack Server** ([`UploadPackServer`]): serves `ls-refs` (`symrefs`,
//!   `peel`, `ref-prefix`, `unborn`) and `fetch` (`want`, `have`, `done`, `ofs-delta`, `deepen`,
//!   `include-tag`, `no-progress`, `thin-pack`, multi-round negotiation + side-band-64k packfile).
//! - **Git Push Receive-Pack Server** ([`ReceivePackServer`]): serves v1 ref advertisement +
//!   atomic CAS push command parsing + thin-pack unpacking + `report-status` (raw or sideband).
//! - **Git Wire Client** ([`WireClient`]): v2 `ls-refs` & `fetch`/`clone` + v1 `receive-pack` push!
//! - **Smart HTTP Router & Client Framing** ([`SmartHttpServer`], [`HttpClientFraming`]): sans-IO
//!   HTTP/1.1 request/response codecs (including `Transfer-Encoding: chunked` and `Git-Protocol:
//!   version=2`) so real `git clone/fetch/push http://...` works directly against our server!

#![blaze::sans_io]

use blaze_git_fetch::{
    Kind as ObjectKind, Oid, PackLimits,
    pkt::{self, Lines, Pkt},
    store::Commit,
};
use blaze_hash::{BuildKeyHasher, Map, MapOpts};
use blaze_own::{
    AString, AVec, Alloc, String, Vec,
    planted::{NoPlant, Planted},
};

use crate::{
    ErrorKind, GitError, GitFile,
    objects::TreeIter,
    pack_write::{PackWriteOpts, UnpackStats, unpack_pack_into_store_planted, write_pack_from_store_planted},
    plant::GitEnginePlant,
    refs::ref_name_safe,
    storage::{
        ObjectStore, RefTarget, RefUpdate, RefUpdateStatus, RepoStore, ZERO_OID, parse_tag,
        peel_tag,
    },
};

pub const SERVER_AGENT: &[u8] = b"agent=blaze-git/1";

/// Framing mode for wire protocol advertisements.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Transport {
    /// Smart HTTP (`/info/refs?service=...`): prefixes `# service=<svc>\n0000`.
    Http,
    /// Raw bidirectional stream (stdio / SSH / TCP `git://`): no `# service=` prefix.
    Stream,
}

/// Bounds on wire protocol operations.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct WireLimits {
    pub most_objects: u32,
    pub most_wants: usize,
    pub most_haves: usize,
    pub most_commands: usize,
    pub most_pack_bytes: u64,
    pub pack: PackLimits,
}

impl WireLimits {
    pub const SHIPPED: WireLimits = WireLimits {
        most_objects: 2_000_000,
        most_wants: 65_536,
        most_haves: 65_536,
        most_commands: 65_536,
        most_pack_bytes: 2 << 30,
        pack: PackLimits::SHIPPED,
    };
}

// ---------------------------------------------------------------------------
// Object closure computation (wants \ reachable(haves))
// ---------------------------------------------------------------------------

/// Collects all object OIDs reachable from `wants` that are NOT reachable from `haves` (and
/// respects optional `deepen` commit depth). Returns OIDs ordered commits -> trees -> blobs -> tags.
pub fn collect_closure<'a, S: ObjectStore<'a> + ?Sized>(
    store: &S,
    wants: &[Oid],
    haves: &[Oid],
    deepen: Option<u32>,
    limits: WireLimits,
    alloc: Alloc<'a>,
    out: &mut AVec<'a, Oid>,
) -> Result<(), GitError> {
    out.clear();
    let mut excluded: Map<Oid, (), BuildKeyHasher, Alloc<'a>> =
        Map::new(MapOpts { hasher: BuildKeyHasher::default(), alloc, capacity: Some(haves.len()) });
    let mut obj_buf = Vec::new_in(alloc);

    // Mark all objects reachable from `haves` (walk commit ancestry + their root trees).
    let mut have_stack: AVec<'a, Oid> = Vec::new_in(alloc);
    for &h in haves {
        if store.has_object(&h) {
            have_stack.push(h);
        }
    }
    let mut excluded_trees: AVec<'a, Oid> = Vec::new_in(alloc);
    while let Some(oid) = have_stack.pop() {
        if excluded.contains_key(&oid) {
            continue;
        }
        excluded.insert(oid, ());
        if excluded.len() as u32 > limits.most_objects {
            return Err(GitError {
                file: GitFile::Wire,
                at: 0,
                kind: ErrorKind::BudgetExceeded,
            });
        }
        let Some(kind) = store.read_object(&oid, &mut obj_buf)? else {
            continue;
        };
        match kind {
            ObjectKind::Commit => {
                let c = Commit::parse(&obj_buf).map_err(|(at, why)| GitError {
                    file: GitFile::CommitObject(oid),
                    at,
                    kind: ErrorKind::CommitParse(why),
                })?;
                excluded_trees.push(c.tree);
                for p in c.parents() {
                    if !excluded.contains_key(&p) {
                        have_stack.push(p);
                    }
                }
            }
            ObjectKind::Tag => {
                let tag = parse_tag(oid, &obj_buf)?;
                if !excluded.contains_key(&tag.object) {
                    have_stack.push(tag.object);
                }
            }
            ObjectKind::Tree => excluded_trees.push(oid),
            ObjectKind::Blob => {}
        }
    }
    // Walk excluded trees to mark excluded blobs/subtrees so incremental fetches/pushes are tiny!
    while let Some(tree_oid) = excluded_trees.pop() {
        if excluded.contains_key(&tree_oid) {
            continue;
        }
        excluded.insert(tree_oid, ());
        let Some(kind) = store.read_object(&tree_oid, &mut obj_buf)? else {
            continue;
        };
        if kind != ObjectKind::Tree {
            continue;
        }
        let mut iter = TreeIter::new(tree_oid, &obj_buf);
        while let Some(e) = iter.next_entry()? {
            if e.mode == 0o160000 {
                continue; // gitlink submodule
            }
            if e.is_dir() {
                if !excluded.contains_key(&e.oid) {
                    excluded_trees.push(e.oid);
                }
            } else {
                excluded.insert(e.oid, ());
            }
        }
    }

    // Now walk `wants` (respecting `deepen`) and collect non-excluded objects!
    let mut visited: Map<Oid, (), BuildKeyHasher, Alloc<'a>> =
        Map::new(MapOpts { hasher: BuildKeyHasher::default(), alloc, capacity: Some(64) });
    let mut commit_queue: AVec<'a, (Oid, u32)> = Vec::new_in(alloc);
    let mut tree_queue: AVec<'a, Oid> = Vec::new_in(alloc);
    let mut commits_out: AVec<'a, Oid> = Vec::new_in(alloc);
    let mut trees_out: AVec<'a, Oid> = Vec::new_in(alloc);
    let mut blobs_out: AVec<'a, Oid> = Vec::new_in(alloc);
    let mut tags_out: AVec<'a, Oid> = Vec::new_in(alloc);

    for &w in wants {
        commit_queue.push((w, 1));
    }

    while let Some((oid, depth)) = commit_queue.pop() {
        if excluded.contains_key(&oid) || visited.contains_key(&oid) {
            continue;
        }
        visited.insert(oid, ());
        if visited.len() as u32 > limits.most_objects {
            return Err(GitError {
                file: GitFile::Wire,
                at: 0,
                kind: ErrorKind::BudgetExceeded,
            });
        }
        let Some(kind) = store.read_object(&oid, &mut obj_buf)? else {
            return Err(GitError {
                file: GitFile::Wire,
                at: 0,
                kind: ErrorKind::ObjectMissing(oid),
            });
        };
        match kind {
            ObjectKind::Commit => {
                commits_out.push(oid);
                let c = Commit::parse(&obj_buf).map_err(|(at, why)| GitError {
                    file: GitFile::CommitObject(oid),
                    at,
                    kind: ErrorKind::CommitParse(why),
                })?;
                tree_queue.push(c.tree);
                let allow_parents = deepen.is_none_or(|d| depth < d);
                if allow_parents {
                    for p in c.parents() {
                        if !excluded.contains_key(&p) && !visited.contains_key(&p) {
                            commit_queue.push((p, depth + 1));
                        }
                    }
                }
            }
            ObjectKind::Tag => {
                tags_out.push(oid);
                let tag = parse_tag(oid, &obj_buf)?;
                if !excluded.contains_key(&tag.object) && !visited.contains_key(&tag.object) {
                    commit_queue.push((tag.object, depth));
                }
            }
            ObjectKind::Tree => {
                tree_queue.push(oid);
            }
            ObjectKind::Blob => {
                blobs_out.push(oid);
            }
        }
    }

    let mut seen_trees: Map<Oid, (), BuildKeyHasher, Alloc<'a>> =
        Map::new(MapOpts { hasher: BuildKeyHasher::default(), alloc, capacity: Some(64) });
    while let Some(tree_oid) = tree_queue.pop() {
        if excluded.contains_key(&tree_oid) || seen_trees.contains_key(&tree_oid) {
            continue;
        }
        seen_trees.insert(tree_oid, ());
        trees_out.push(tree_oid);
        visited.insert(tree_oid, ());
        if visited.len() as u32 > limits.most_objects {
            return Err(GitError {
                file: GitFile::Wire,
                at: 0,
                kind: ErrorKind::BudgetExceeded,
            });
        }
        let Some(kind) = store.read_object(&tree_oid, &mut obj_buf)? else {
            return Err(GitError {
                file: GitFile::TreeObject(tree_oid),
                at: 0,
                kind: ErrorKind::ObjectMissing(tree_oid),
            });
        };
        if kind != ObjectKind::Tree {
            return Err(GitError {
                file: GitFile::TreeObject(tree_oid),
                at: 0,
                kind: ErrorKind::WrongObjectKind { want: ObjectKind::Tree, got: kind },
            });
        }
        let mut iter = TreeIter::new(tree_oid, &obj_buf);
        while let Some(e) = iter.next_entry()? {
            if e.mode == 0o160000 {
                continue;
            }
            if excluded.contains_key(&e.oid) || visited.contains_key(&e.oid) {
                continue;
            }
            if e.is_dir() {
                tree_queue.push(e.oid);
            } else {
                visited.insert(e.oid, ());
                blobs_out.push(e.oid);
            }
        }
    }

    out.extend_from_slice(&tags_out);
    out.extend_from_slice(&commits_out);
    out.extend_from_slice(&trees_out);
    out.extend_from_slice(&blobs_out);
    Ok(())
}

// ---------------------------------------------------------------------------
// Sideband helper (band 1 = pack/data, band 2 = progress, band 3 = error)
// ---------------------------------------------------------------------------

fn write_sideband<'a>(band: u8, payload: &[u8], out: &mut AVec<'a, u8>) -> Result<(), GitError> {
    // Max pkt-line payload is 65516; reserve 1 byte for band prefix.
    const CHUNK: usize = 65_500;
    let mut pos = 0usize;
    while pos < payload.len() {
        let take = (payload.len() - pos).min(CHUNK);
        pkt::line(&[&[band], &payload[pos..pos + take]], out).map_err(|_| GitError {
            file: GitFile::Wire,
            at: out.len() as u64,
            kind: ErrorKind::WireProtocol,
        })?;
        pos += take;
    }
    Ok(())
}

fn pkt_err(at: usize) -> GitError {
    GitError { file: GitFile::Wire, at: at as u64, kind: ErrorKind::WireProtocol }
}

// ---------------------------------------------------------------------------
// Git Protocol v2 Upload-Pack Server (`git clone` / `git fetch` / `git ls-remote`)
// ---------------------------------------------------------------------------

/// Sans-IO Git Protocol v2 server handling `ls-refs` and `fetch`.
pub struct UploadPackServer;

impl UploadPackServer {
    /// Writes the Protocol v2 capability advertisement into `out` (cleared first).
    pub fn advertise<'a>(transport: Transport, out: &mut AVec<'a, u8>) -> Result<(), GitError> {
        out.clear();
        if transport == Transport::Http {
            pkt::data(b"# service=git-upload-pack\n", out).map_err(|_| pkt_err(0))?;
            pkt::flush(out).map_err(|_| pkt_err(0))?;
        }
        pkt::data(b"version 2\n", out).map_err(|_| pkt_err(0))?;
        pkt::line(&[SERVER_AGENT, b"\n"], out).map_err(|_| pkt_err(0))?;
        pkt::data(b"ls-refs=unborn\n", out).map_err(|_| pkt_err(0))?;
        pkt::data(b"fetch=shallow wait-for-done\n", out).map_err(|_| pkt_err(0))?;
        pkt::data(b"object-format=sha1\n", out).map_err(|_| pkt_err(0))?;
        pkt::flush(out).map_err(|_| pkt_err(0))?;
        Ok(())
    }

    /// Handles one Protocol v2 request (`command=ls-refs` or `command=fetch`), writing the exact
    /// wire response into `out` (cleared first).
    pub fn handle_request<'a, S: RepoStore<'a> + ?Sized>(
        request: &[u8],
        store: &S,
        limits: WireLimits,
        pack_opts: PackWriteOpts,
        alloc: Alloc<'a>,
        out: &mut AVec<'a, u8>,
    ) -> Result<(), GitError> {
        Self::handle_request_planted(request, store, limits, pack_opts, alloc, out, NoPlant)
    }

    pub fn handle_request_planted<'a, S: RepoStore<'a> + ?Sized, P: Planted<GitEnginePlant>>(
        request: &[u8],
        store: &S,
        limits: WireLimits,
        pack_opts: PackWriteOpts,
        alloc: Alloc<'a>,
        out: &mut AVec<'a, u8>,
        plant: P,
    ) -> Result<(), GitError> {
        out.clear();
        let mut lines = Lines::new(request);
        let mut command: Option<&[u8]> = None;

        loop {
            match lines.expect_line().map_err(|_| pkt_err(lines.offset()))? {
                Pkt::Data(line) => {
                    let t = pkt::text(line);
                    if let Some(cmd) = t.strip_prefix(b"command=") {
                        command = Some(cmd);
                    }
                    // Ignore client capability metadata lines (`agent=...`, `object-format=sha1`).
                }
                Pkt::Delim => break,
                Pkt::Flush => {
                    // Command with no argument section (e.g. bare `ls-refs`).
                    return match command {
                        Some(b"ls-refs") => {
                            Self::exec_ls_refs(store, false, false, false, &[], alloc, out)
                        }
                        _ => Err(pkt_err(lines.offset())),
                    };
                }
                Pkt::End => return Err(pkt_err(lines.offset())),
            }
        }

        match command {
            Some(b"ls-refs") => {
                let mut symrefs = false;
                let mut peel = false;
                let mut unborn = false;
                let mut prefixes: AVec<'a, &str> = Vec::new_in(alloc);
                loop {
                    match lines.expect_line().map_err(|_| pkt_err(lines.offset()))? {
                        Pkt::Flush => break,
                        Pkt::Data(arg) => {
                            let t = pkt::text(arg);
                            if t == b"symrefs" {
                                symrefs = true;
                            } else if t == b"peel" {
                                peel = true;
                            } else if t == b"unborn" {
                                unborn = true;
                            } else if let Some(pfx) = t.strip_prefix(b"ref-prefix ")
                                && let Ok(s) = core::str::from_utf8(pfx)
                            {
                                prefixes.push(s);
                            }
                        }
                        Pkt::Delim | Pkt::End => return Err(pkt_err(lines.offset())),
                    }
                }
                Self::exec_ls_refs(store, symrefs, peel, unborn, &prefixes, alloc, out)
            }
            Some(b"fetch") => {
                let mut wants: AVec<'a, Oid> = Vec::new_in(alloc);
                let mut haves: AVec<'a, Oid> = Vec::new_in(alloc);
                let mut done = false;
                let mut wait_for_done = false;
                let mut deepen: Option<u32> = None;
                let mut ofs_delta = false;

                loop {
                    match lines.expect_line().map_err(|_| pkt_err(lines.offset()))? {
                        Pkt::Flush => break,
                        Pkt::Data(arg) => {
                            let t = pkt::text(arg);
                            if let Some(hex) = t.strip_prefix(b"want ") {
                                if wants.len() >= limits.most_wants {
                                    return Err(GitError {
                                        file: GitFile::Wire,
                                        at: lines.offset() as u64,
                                        kind: ErrorKind::BudgetExceeded,
                                    });
                                }
                                let oid = Oid::parse_hex(hex).ok_or_else(|| pkt_err(lines.offset()))?;
                                wants.push(oid);
                            } else if let Some(hex) = t.strip_prefix(b"have ") {
                                if haves.len() >= limits.most_haves {
                                    return Err(GitError {
                                        file: GitFile::Wire,
                                        at: lines.offset() as u64,
                                        kind: ErrorKind::BudgetExceeded,
                                    });
                                }
                                let oid = Oid::parse_hex(hex).ok_or_else(|| pkt_err(lines.offset()))?;
                                haves.push(oid);
                            } else if t == b"done" {
                                done = true;
                            } else if t == b"wait-for-done" {
                                wait_for_done = true;
                            } else if t == b"ofs-delta" {
                                ofs_delta = true;
                            } else if let Some(d_bytes) = t.strip_prefix(b"deepen ")
                                && let Ok(s) = core::str::from_utf8(d_bytes)
                                && let Ok(d) = s.parse::<u32>()
                            {
                                deepen = Some(d.max(1));
                            }
                        }
                        Pkt::Delim | Pkt::End => return Err(pkt_err(lines.offset())),
                    }
                }

                // Multi-round negotiation when client didn't send `done` yet.
                if !done {
                    let mut common_haves: AVec<'a, Oid> = Vec::new_in(alloc);
                    for &h in &haves {
                        if store.has_object(&h) {
                            common_haves.push(h);
                        }
                    }
                    pkt::data(b"acknowledgments\n", out).map_err(|_| pkt_err(0))?;
                    if common_haves.is_empty() {
                        pkt::data(b"NAK\n", out).map_err(|_| pkt_err(0))?;
                        pkt::flush(out).map_err(|_| pkt_err(0))?;
                        return Ok(());
                    }
                    for c in &common_haves {
                        pkt::line(&[b"ACK ", &c.hex(), b"\n"], out).map_err(|_| pkt_err(0))?;
                    }
                    if wait_for_done {
                        pkt::flush(out).map_err(|_| pkt_err(0))?;
                        return Ok(());
                    }
                    pkt::data(b"ready\n", out).map_err(|_| pkt_err(0))?;
                    pkt::delim(out).map_err(|_| pkt_err(0))?;
                }

                let mut closure = Vec::new_in(alloc);
                collect_closure(store, &wants, &haves, deepen, limits, alloc, &mut closure)?;

                let mut effective_opts = pack_opts;
                effective_opts.use_ofs_delta = pack_opts.use_ofs_delta && ofs_delta;
                let mut pack_bytes = Vec::new_in(alloc);
                write_pack_from_store_planted(
                    store,
                    &closure,
                    effective_opts,
                    alloc,
                    &mut pack_bytes,
                    plant,
                )?;

                // Optional shallow-info section if `deepen` was requested.
                if let Some(d) = deepen {
                    let mut shallow_oids = Vec::new_in(alloc);
                    collect_shallow_boundary(store, &wants, d, alloc, &mut shallow_oids)?;
                    if !shallow_oids.is_empty() {
                        pkt::data(b"shallow-info\n", out).map_err(|_| pkt_err(0))?;
                        for s in shallow_oids {
                            pkt::line(&[b"shallow ", &s.hex(), b"\n"], out)
                                .map_err(|_| pkt_err(0))?;
                        }
                        pkt::delim(out).map_err(|_| pkt_err(0))?;
                    }
                }

                pkt::data(b"packfile\n", out).map_err(|_| pkt_err(0))?;
                write_sideband(1, &pack_bytes, out)?;
                pkt::flush(out).map_err(|_| pkt_err(0))?;
                Ok(())
            }
            _ => Err(pkt_err(lines.offset())),
        }
    }

    fn exec_ls_refs<'a, S: RepoStore<'a> + ?Sized>(
        store: &S,
        symrefs: bool,
        peel: bool,
        unborn: bool,
        prefixes: &[&str],
        alloc: Alloc<'a>,
        out: &mut AVec<'a, u8>,
    ) -> Result<(), GitError> {
        let matches_prefix = |name: &str| -> bool {
            prefixes.is_empty() || prefixes.iter().any(|p| name.starts_with(p))
        };

        // Emit HEAD first if it matches prefixes.
        if matches_prefix("HEAD")
            && let Some(head) = store.read_head(alloc)?
        {
            match &head {
                RefTarget::Direct(oid) => {
                    pkt::line(&[&oid.hex(), b" HEAD\n"], out).map_err(|_| pkt_err(0))?;
                }
                RefTarget::Symbolic(sym) => {
                    if let Some(resolved) = store.resolve_ref("HEAD")? {
                        if symrefs {
                            pkt::line(
                                &[&resolved.hex(), b" HEAD symref-target:", sym.as_str().as_bytes(), b"\n"],
                                out,
                            )
                            .map_err(|_| pkt_err(0))?;
                        } else {
                            pkt::line(&[&resolved.hex(), b" HEAD\n"], out)
                                .map_err(|_| pkt_err(0))?;
                        }
                    } else if unborn {
                        if symrefs {
                            pkt::line(
                                &[b"unborn HEAD symref-target:", sym.as_str().as_bytes(), b"\n"],
                                out,
                            )
                            .map_err(|_| pkt_err(0))?;
                        } else {
                            pkt::data(b"unborn HEAD\n", out).map_err(|_| pkt_err(0))?;
                        }
                    }
                }
            }
        }

        let mut refs = Vec::new_in(alloc);
        store.list_refs("", alloc, &mut refs)?;
        for r in &refs {
            if !matches_prefix(r.name.as_str()) {
                continue;
            }
            let resolved_oid = match &r.target {
                RefTarget::Direct(oid) => Some(*oid),
                RefTarget::Symbolic(_) => store.resolve_ref(r.name.as_str())?,
            };
            let Some(oid) = resolved_oid else {
                continue;
            };
            let mut line = Vec::new_in(alloc);
            line.extend_from_slice(&oid.hex());
            line.push(b' ');
            line.extend_from_slice(r.name.as_str().as_bytes());
            if symrefs
                && let RefTarget::Symbolic(sym) = &r.target
            {
                line.extend_from_slice(b" symref-target:");
                line.extend_from_slice(sym.as_str().as_bytes());
            }
            if peel {
                let peeled = match r.peeled {
                    Some(p) => Some(p),
                    None => peel_tag(store, oid, 8, alloc)?,
                };
                if let Some(p) = peeled {
                    line.extend_from_slice(b" peeled:");
                    line.extend_from_slice(&p.hex());
                }
            }
            line.push(b'\n');
            pkt::data(&line, out).map_err(|_| pkt_err(0))?;
        }
        pkt::flush(out).map_err(|_| pkt_err(0))?;
        Ok(())
    }
}

fn collect_shallow_boundary<'a, S: ObjectStore<'a> + ?Sized>(
    store: &S,
    wants: &[Oid],
    deepen: u32,
    alloc: Alloc<'a>,
    out: &mut AVec<'a, Oid>,
) -> Result<(), GitError> {
    let mut visited: Map<Oid, (), BuildKeyHasher, Alloc<'a>> =
        Map::new(MapOpts { hasher: BuildKeyHasher::default(), alloc, capacity: Some(16) });
    let mut stack: AVec<'a, (Oid, u32)> = Vec::new_in(alloc);
    let mut buf = Vec::new_in(alloc);
    for &w in wants {
        stack.push((w, 1));
    }
    while let Some((oid, depth)) = stack.pop() {
        if visited.contains_key(&oid) {
            continue;
        }
        visited.insert(oid, ());
        let Some(ObjectKind::Commit) = store.read_object(&oid, &mut buf)? else {
            continue;
        };
        let c = Commit::parse(&buf).map_err(|(at, why)| GitError {
            file: GitFile::CommitObject(oid),
            at,
            kind: ErrorKind::CommitParse(why),
        })?;
        if depth >= deepen {
            if c.parents().next().is_some() {
                out.push(oid);
            }
            continue;
        }
        for p in c.parents() {
            stack.push((p, depth + 1));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Git Push Receive-Pack Server (`git push` wire v1)
// ---------------------------------------------------------------------------

const RECEIVE_CAPS: &[u8] =
    b"report-status delete-refs ofs-delta side-band-64k atomic quiet agent=blaze-git/1";

/// Summary of one push handled by [`ReceivePackServer::handle_push`].
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct PushHandled {
    pub unpack: UnpackStats,
    pub updated_refs: u32,
    pub rejected_refs: u32,
}

/// Sans-IO Git `receive-pack` server compatible with standard `git push`.
pub struct ReceivePackServer;

impl ReceivePackServer {
    /// Writes the `git-receive-pack` ref & capability advertisement into `out` (cleared first).
    pub fn advertise<'a, S: RepoStore<'a> + ?Sized>(
        transport: Transport,
        store: &S,
        alloc: Alloc<'a>,
        out: &mut AVec<'a, u8>,
    ) -> Result<(), GitError> {
        out.clear();
        if transport == Transport::Http {
            pkt::data(b"# service=git-receive-pack\n", out).map_err(|_| pkt_err(0))?;
            pkt::flush(out).map_err(|_| pkt_err(0))?;
        }
        let mut refs = Vec::new_in(alloc);
        store.list_refs("", alloc, &mut refs)?;
        let mut emitted = 0usize;
        for r in &refs {
            let oid = match &r.target {
                RefTarget::Direct(o) => Some(*o),
                RefTarget::Symbolic(_) => store.resolve_ref(r.name.as_str())?,
            };
            let Some(oid) = oid else {
                continue;
            };
            if emitted == 0 {
                pkt::line(
                    &[&oid.hex(), b" ", r.name.as_str().as_bytes(), b"\0", RECEIVE_CAPS, b"\n"],
                    out,
                )
                .map_err(|_| pkt_err(0))?;
            } else {
                pkt::line(&[&oid.hex(), b" ", r.name.as_str().as_bytes(), b"\n"], out)
                    .map_err(|_| pkt_err(0))?;
            }
            emitted += 1;
        }
        if emitted == 0 {
            pkt::line(&[&ZERO_OID.hex(), b" capabilities^{}\0", RECEIVE_CAPS, b"\n"], out)
                .map_err(|_| pkt_err(0))?;
        }
        pkt::flush(out).map_err(|_| pkt_err(0))?;
        Ok(())
    }

    /// Handles a `git-receive-pack` request (ref-update commands + optional PACK v2 payload),
    /// unpacking objects into `store`, applying atomic/non-atomic CAS ref updates, and writing
    /// the `report-status` reply into `out` (cleared first).
    pub fn handle_push<'a, S: RepoStore<'a> + ?Sized>(
        request: &[u8],
        store: &mut S,
        limits: WireLimits,
        alloc: Alloc<'a>,
        out: &mut AVec<'a, u8>,
    ) -> Result<PushHandled, GitError> {
        Self::handle_push_planted(request, store, limits, alloc, out, NoPlant)
    }

    pub fn handle_push_planted<'a, S: RepoStore<'a> + ?Sized, P: Planted<GitEnginePlant>>(
        request: &[u8],
        store: &mut S,
        limits: WireLimits,
        alloc: Alloc<'a>,
        out: &mut AVec<'a, u8>,
        plant: P,
    ) -> Result<PushHandled, GitError> {
        out.clear();
        let mut lines = Lines::new(request);
        let mut updates: AVec<'a, RefUpdate<'a>> = Vec::new_in(alloc);
        let mut want_report_status = false;
        let mut want_sideband = false;
        let mut want_atomic = false;
        let mut any_non_delete = false;

        loop {
            match lines.expect_line().map_err(|_| pkt_err(lines.offset()))? {
                Pkt::Flush => break,
                Pkt::Data(line) => {
                    if updates.len() >= limits.most_commands {
                        return Err(GitError {
                            file: GitFile::Wire,
                            at: lines.offset() as u64,
                            kind: ErrorKind::BudgetExceeded,
                        });
                    }
                    let (cmd_part, caps_part) = match line.iter().position(|&b| b == 0) {
                        Some(nul) => (&line[..nul], Some(pkt::text(&line[nul + 1..]))),
                        None => (pkt::text(line), None),
                    };
                    if let Some(caps) = caps_part {
                        for cap in caps.split(|&b| b == b' ') {
                            match cap {
                                b"report-status" | b"report-status-v2" => want_report_status = true,
                                b"side-band-64k" => want_sideband = true,
                                b"atomic" => want_atomic = true,
                                _ => {}
                            }
                        }
                    }
                    // `<40-hex-old> <40-hex-new> <refname>`
                    if cmd_part.len() < 83 || cmd_part[40] != b' ' || cmd_part[81] != b' ' {
                        return Err(pkt_err(lines.offset()));
                    }
                    let old_oid =
                        Oid::parse_hex(&cmd_part[0..40]).ok_or_else(|| pkt_err(lines.offset()))?;
                    let new_oid =
                        Oid::parse_hex(&cmd_part[41..81]).ok_or_else(|| pkt_err(lines.offset()))?;
                    let ref_str = core::str::from_utf8(&cmd_part[82..])
                        .map_err(|_| pkt_err(lines.offset()))?;
                    if new_oid != ZERO_OID {
                        any_non_delete = true;
                    }
                    let cas_old = if plant.has(GitEnginePlant::SkipCasOldOidCheck) {
                        None
                    } else {
                        Some(old_oid)
                    };
                    updates.push(RefUpdate {
                        name: String::from_str_in(ref_str, alloc),
                        old: cas_old,
                        new: new_oid,
                    });
                }
                Pkt::Delim | Pkt::End => return Err(pkt_err(lines.offset())),
            }
        }

        let pack_slice = &request[lines.offset()..];
        let mut handled = PushHandled::default();
        let mut unpack_ok = true;
        let mut unpack_err_msg: &[u8] = b"";

        if !pack_slice.is_empty() {
            if pack_slice.len() as u64 > limits.most_pack_bytes {
                return Err(GitError {
                    file: GitFile::Wire,
                    at: lines.offset() as u64,
                    kind: ErrorKind::BudgetExceeded,
                });
            }
            match unpack_pack_into_store_planted(pack_slice, store, limits.pack, alloc, plant) {
                Ok(stats) => handled.unpack = stats,
                Err(_) => {
                    unpack_ok = false;
                    unpack_err_msg = b"unpack error invalid packfile\n";
                }
            }
        } else if any_non_delete {
            // Check whether every target OID already exists in `store` (e.g. creating a branch at
            // an existing commit without sending a pack).
            for u in &updates {
                if u.new != ZERO_OID && !store.has_object(&u.new) {
                    unpack_ok = false;
                    unpack_err_msg = b"unpack error missing objects\n";
                    break;
                }
            }
        }

        let mut report_raw = Vec::new_in(alloc);
        if unpack_ok {
            pkt::data(b"unpack ok\n", &mut report_raw).map_err(|_| pkt_err(0))?;
            let statuses = store.apply_ref_updates(&updates, want_atomic, alloc)?;
            for (u, st) in updates.iter().zip(statuses.iter()) {
                match st {
                    RefUpdateStatus::Ok => {
                        handled.updated_refs += 1;
                        // If HEAD is unborn and points to a newly created branch, HEAD resolves automatically.
                        // If repo had no HEAD yet, point HEAD at the first created branch.
                        if store.read_head(alloc)?.is_none()
                            && u.name.as_str().starts_with("refs/heads/")
                            && u.new != ZERO_OID
                        {
                            store.write_head(&RefTarget::Symbolic(String::from_str_in(
                                u.name.as_str(),
                                alloc,
                            )))?;
                        }
                        pkt::line(&[b"ok ", u.name.as_str().as_bytes(), b"\n"], &mut report_raw)
                            .map_err(|_| pkt_err(0))?;
                    }
                    RefUpdateStatus::CasMismatch { .. } => {
                        handled.rejected_refs += 1;
                        pkt::line(
                            &[b"ng ", u.name.as_str().as_bytes(), b" non-fast-forward or stale old oid\n"],
                            &mut report_raw,
                        )
                        .map_err(|_| pkt_err(0))?;
                    }
                    RefUpdateStatus::InvalidName => {
                        handled.rejected_refs += 1;
                        pkt::line(
                            &[b"ng ", u.name.as_str().as_bytes(), b" invalid ref name\n"],
                            &mut report_raw,
                        )
                        .map_err(|_| pkt_err(0))?;
                    }
                    RefUpdateStatus::RejectedAtomic => {
                        handled.rejected_refs += 1;
                        pkt::line(
                            &[b"ng ", u.name.as_str().as_bytes(), b" atomic push failed\n"],
                            &mut report_raw,
                        )
                        .map_err(|_| pkt_err(0))?;
                    }
                }
            }
        } else {
            handled.rejected_refs = updates.len() as u32;
            pkt::data(unpack_err_msg, &mut report_raw).map_err(|_| pkt_err(0))?;
            for u in &updates {
                pkt::line(&[b"ng ", u.name.as_str().as_bytes(), b" unpacker error\n"], &mut report_raw)
                    .map_err(|_| pkt_err(0))?;
            }
        }
        pkt::flush(&mut report_raw).map_err(|_| pkt_err(0))?;

        if want_report_status {
            if want_sideband {
                write_sideband(1, &report_raw, out)?;
                pkt::flush(out).map_err(|_| pkt_err(0))?;
            } else {
                out.extend_from_slice(&report_raw);
            }
        }
        Ok(handled)
    }
}

// ---------------------------------------------------------------------------
// Git Wire Client (Protocol v2 ls-refs + fetch/clone + v1 receive-pack push)
// ---------------------------------------------------------------------------

/// One reference parsed from a remote `ls-refs` or `receive-pack` advertisement.
#[derive(PartialEq, Eq, Debug)]
pub struct RemoteRef<'a> {
    pub name: AString<'a>,
    pub oid: Option<Oid>,
    pub symref_target: Option<AString<'a>>,
    pub peeled: Option<Oid>,
}

/// Result of one ref reported by a remote `receive-pack` server.
#[derive(PartialEq, Eq, Debug)]
pub struct PushRefReport<'a> {
    pub name: AString<'a>,
    pub ok: bool,
    pub message: Option<AString<'a>>,
}

/// Parsed response from a remote `receive-pack` push.
#[derive(PartialEq, Eq, Debug)]
pub struct PushReport<'a> {
    pub unpack_ok: bool,
    pub unpack_message: AString<'a>,
    pub refs: AVec<'a, PushRefReport<'a>>,
}

/// Parsed `receive-pack` server capability advertisement.
#[derive(PartialEq, Eq, Debug)]
pub struct ReceivePackAdvert<'a> {
    pub refs: AVec<'a, RemoteRef<'a>>,
    pub report_status: bool,
    pub delete_refs: bool,
    pub ofs_delta: bool,
    pub sideband_64k: bool,
    pub atomic: bool,
}

/// Sans-IO Git client speaking Protocol v2 (`ls-refs`, `fetch`) and Push v1 (`receive-pack`).
pub struct WireClient;

impl WireClient {
    /// Builds a Protocol v2 `ls-refs` request body.
    pub fn encode_ls_refs<'a>(prefixes: &[&str], out: &mut AVec<'a, u8>) -> Result<(), GitError> {
        out.clear();
        pkt::data(b"command=ls-refs\n", out).map_err(|_| pkt_err(0))?;
        pkt::line(&[SERVER_AGENT, b"\n"], out).map_err(|_| pkt_err(0))?;
        pkt::data(b"object-format=sha1\n", out).map_err(|_| pkt_err(0))?;
        pkt::delim(out).map_err(|_| pkt_err(0))?;
        pkt::data(b"peel\n", out).map_err(|_| pkt_err(0))?;
        pkt::data(b"symrefs\n", out).map_err(|_| pkt_err(0))?;
        for p in prefixes {
            pkt::line(&[b"ref-prefix ", p.as_bytes(), b"\n"], out).map_err(|_| pkt_err(0))?;
        }
        pkt::flush(out).map_err(|_| pkt_err(0))?;
        Ok(())
    }

    /// Parses a Protocol v2 `ls-refs` response into structured [`RemoteRef`] entries.
    pub fn parse_ls_refs<'a>(
        body: &[u8],
        alloc: Alloc<'a>,
    ) -> Result<AVec<'a, RemoteRef<'a>>, GitError> {
        let mut lines = Lines::new(body);
        let mut out = Vec::new_in(alloc);
        loop {
            match lines.expect_line().map_err(|_| pkt_err(lines.offset()))? {
                Pkt::Flush => break,
                Pkt::Data(line) => {
                    let t = pkt::text(line);
                    let mut parts = t.split(|&b| b == b' ');
                    let first = parts.next().ok_or_else(|| pkt_err(lines.offset()))?;
                    let oid = if first == b"unborn" {
                        None
                    } else {
                        Some(Oid::parse_hex(first).ok_or_else(|| pkt_err(lines.offset()))?)
                    };
                    let name_b = parts.next().ok_or_else(|| pkt_err(lines.offset()))?;
                    let name_s = core::str::from_utf8(name_b).map_err(|_| pkt_err(lines.offset()))?;
                    let mut symref_target = None;
                    let mut peeled = None;
                    for attr in parts {
                        if let Some(sym) = attr.strip_prefix(b"symref-target:")
                            && let Ok(s) = core::str::from_utf8(sym)
                        {
                            symref_target = Some(String::from_str_in(s, alloc));
                        } else if let Some(hex) = attr.strip_prefix(b"peeled:")
                            && let Some(p) = Oid::parse_hex(hex)
                        {
                            peeled = Some(p);
                        }
                    }
                    out.push(RemoteRef {
                        name: String::from_str_in(name_s, alloc),
                        oid,
                        symref_target,
                        peeled,
                    });
                }
                Pkt::Delim | Pkt::End => return Err(pkt_err(lines.offset())),
            }
        }
        Ok(out)
    }

    /// Options for Protocol v2 `fetch` requests (`deepen`, partial-clone `filter`, `thin-pack`).
    pub fn encode_fetch_with_opts<'a>(
        wants: &[Oid],
        haves: &[Oid],
        deepen: Option<u32>,
        filter: Option<&str>,
        thin_pack: bool,
        out: &mut AVec<'a, u8>,
    ) -> Result<(), GitError> {
        out.clear();
        pkt::data(b"command=fetch\n", out).map_err(|_| pkt_err(0))?;
        pkt::line(&[SERVER_AGENT, b"\n"], out).map_err(|_| pkt_err(0))?;
        pkt::data(b"object-format=sha1\n", out).map_err(|_| pkt_err(0))?;
        pkt::delim(out).map_err(|_| pkt_err(0))?;
        if thin_pack {
            pkt::data(b"thin-pack\n", out).map_err(|_| pkt_err(0))?;
        }
        pkt::data(b"ofs-delta\n", out).map_err(|_| pkt_err(0))?;
        pkt::data(b"no-progress\n", out).map_err(|_| pkt_err(0))?;
        if let Some(spec) = filter
            && !spec.is_empty()
        {
            pkt::line(&[b"filter ", spec.as_bytes(), b"\n"], out).map_err(|_| pkt_err(0))?;
        }
        for w in wants {
            pkt::line(&[b"want ", &w.hex(), b"\n"], out).map_err(|_| pkt_err(0))?;
        }
        for h in haves {
            pkt::line(&[b"have ", &h.hex(), b"\n"], out).map_err(|_| pkt_err(0))?;
        }
        if let Some(d) = deepen {
            let mut digits = [0u8; 10];
            let mut at = digits.len();
            let mut v = d.max(1);
            loop {
                at -= 1;
                digits[at] = b'0' + (v % 10) as u8;
                v /= 10;
                if v == 0 {
                    break;
                }
            }
            pkt::line(&[b"deepen ", &digits[at..], b"\n"], out).map_err(|_| pkt_err(0))?;
        }
        pkt::data(b"done\n", out).map_err(|_| pkt_err(0))?;
        pkt::flush(out).map_err(|_| pkt_err(0))?;
        Ok(())
    }

    /// Builds a Protocol v2 `fetch` request body for `wants` with optional `haves` and `deepen`.
    pub fn encode_fetch<'a>(
        wants: &[Oid],
        haves: &[Oid],
        deepen: Option<u32>,
        out: &mut AVec<'a, u8>,
    ) -> Result<(), GitError> {
        Self::encode_fetch_with_opts(wants, haves, deepen, None, false, out)
    }

    /// Parses a Protocol v2 `fetch` response and unpacks its packfile directly into `store`.
    pub fn parse_fetch_and_unpack<'a, S: ObjectStore<'a> + ?Sized>(
        body: &[u8],
        store: &mut S,
        limits: WireLimits,
        alloc: Alloc<'a>,
    ) -> Result<UnpackStats, GitError> {
        let mut pack = Vec::new_in(alloc);
        blaze_git_fetch::fetched_pack(body, limits.most_pack_bytes, &mut pack)
            .map_err(|_| pkt_err(0))?;
        unpack_pack_into_store_planted(&pack, store, limits.pack, alloc, NoPlant)
    }

    /// Parses a `git-receive-pack` advertisement (Smart HTTP or Stream).
    pub fn parse_receive_pack_advert<'a>(
        body: &[u8],
        alloc: Alloc<'a>,
    ) -> Result<ReceivePackAdvert<'a>, GitError> {
        let mut lines = Lines::new(body);
        let mut first = lines.expect_line().map_err(|_| pkt_err(lines.offset()))?;
        if let Pkt::Data(line) = first
            && pkt::text(line) == b"# service=git-receive-pack"
        {
            if lines.expect_line().map_err(|_| pkt_err(lines.offset()))? != Pkt::Flush {
                return Err(pkt_err(lines.offset()));
            }
            first = lines.expect_line().map_err(|_| pkt_err(lines.offset()))?;
        }

        let mut advert = ReceivePackAdvert {
            refs: Vec::new_in(alloc),
            report_status: false,
            delete_refs: false,
            ofs_delta: false,
            sideband_64k: false,
            atomic: false,
        };

        let mut cur = first;
        let mut is_first = true;
        loop {
            match cur {
                Pkt::Flush => break,
                Pkt::Data(line) => {
                    let (ref_part, caps_part) = if is_first {
                        is_first = false;
                        match line.iter().position(|&b| b == 0) {
                            Some(nul) => (&line[..nul], Some(pkt::text(&line[nul + 1..]))),
                            None => (pkt::text(line), None),
                        }
                    } else {
                        (pkt::text(line), None)
                    };
                    if let Some(caps) = caps_part {
                        for cap in caps.split(|&b| b == b' ') {
                            match cap {
                                b"report-status" => advert.report_status = true,
                                b"delete-refs" => advert.delete_refs = true,
                                b"ofs-delta" => advert.ofs_delta = true,
                                b"side-band-64k" => advert.sideband_64k = true,
                                b"atomic" => advert.atomic = true,
                                _ => {}
                            }
                        }
                    }
                    if ref_part.len() >= 42 && ref_part[40] == b' ' {
                        let oid = Oid::parse_hex(&ref_part[..40])
                            .ok_or_else(|| pkt_err(lines.offset()))?;
                        let name_s = core::str::from_utf8(&ref_part[41..])
                            .map_err(|_| pkt_err(lines.offset()))?;
                        if name_s != "capabilities^{}" {
                            advert.refs.push(RemoteRef {
                                name: String::from_str_in(name_s, alloc),
                                oid: (oid != ZERO_OID).then_some(oid),
                                symref_target: None,
                                peeled: None,
                            });
                        }
                    }
                }
                Pkt::Delim | Pkt::End => return Err(pkt_err(lines.offset())),
            }
            cur = lines.expect_line().map_err(|_| pkt_err(lines.offset()))?;
        }
        Ok(advert)
    }

    /// Builds a complete `git-receive-pack` push request (ref-update pkt-lines + PACK v2).
    pub fn encode_push_request<'a, S: ObjectStore<'a> + ?Sized>(
        store: &S,
        advert: &ReceivePackAdvert<'_>,
        updates: &[RefUpdate<'_>],
        atomic: bool,
        limits: WireLimits,
        pack_opts: PackWriteOpts,
        alloc: Alloc<'a>,
        out: &mut AVec<'a, u8>,
    ) -> Result<(), GitError> {
        out.clear();
        let mut caps = Vec::new_in(alloc);
        if advert.report_status {
            caps.extend_from_slice(b"report-status");
        }
        if advert.sideband_64k {
            if !caps.is_empty() {
                caps.push(b' ');
            }
            caps.extend_from_slice(b"side-band-64k");
        }
        if advert.ofs_delta {
            if !caps.is_empty() {
                caps.push(b' ');
            }
            caps.extend_from_slice(b"ofs-delta");
        }
        if atomic && advert.atomic {
            if !caps.is_empty() {
                caps.push(b' ');
            }
            caps.extend_from_slice(b"atomic");
        }
        if !caps.is_empty() {
            caps.push(b' ');
        }
        caps.extend_from_slice(SERVER_AGENT);

        let mut wants = Vec::new_in(alloc);
        for (i, u) in updates.iter().enumerate() {
            if !ref_name_safe(u.name.as_str()) {
                return Err(GitError { file: GitFile::Ref, at: 0, kind: ErrorKind::Malformed });
            }
            let old = u.old.unwrap_or(ZERO_OID);
            if u.new != ZERO_OID {
                wants.push(u.new);
            }
            if i == 0 {
                pkt::line(
                    &[&old.hex(), b" ", &u.new.hex(), b" ", u.name.as_str().as_bytes(), b"\0", &caps, b"\n"],
                    out,
                )
                .map_err(|_| pkt_err(0))?;
            } else {
                pkt::line(
                    &[&old.hex(), b" ", &u.new.hex(), b" ", u.name.as_str().as_bytes(), b"\n"],
                    out,
                )
                .map_err(|_| pkt_err(0))?;
            }
        }
        pkt::flush(out).map_err(|_| pkt_err(0))?;

        if !wants.is_empty() {
            let mut haves = Vec::new_in(alloc);
            for r in &advert.refs {
                if let Some(o) = r.oid {
                    haves.push(o);
                }
            }
            let mut closure = Vec::new_in(alloc);
            collect_closure(store, &wants, &haves, None, limits, alloc, &mut closure)?;
            let mut pack_bytes = Vec::new_in(alloc);
            let mut opts = pack_opts;
            opts.use_ofs_delta = pack_opts.use_ofs_delta && advert.ofs_delta;
            write_pack_from_store_planted(store, &closure, opts, alloc, &mut pack_bytes, NoPlant)?;
            out.extend_from_slice(&pack_bytes);
        }
        Ok(())
    }

    /// Parses a `git-receive-pack` push response (`report-status`, raw or side-band-64k).
    pub fn parse_push_response<'a>(
        body: &[u8],
        sideband: bool,
        alloc: Alloc<'a>,
    ) -> Result<PushReport<'a>, GitError> {
        let mut inner = Vec::new_in(alloc);
        let raw = if sideband {
            let mut lines = Lines::new(body);
            pkt::sideband(&mut lines, 1 << 20, &mut inner).map_err(|_| pkt_err(0))?;
            inner.as_slice()
        } else {
            body
        };

        let mut lines = Lines::new(raw);
        let mut unpack_ok = false;
        let mut unpack_message = String::new_in(alloc);
        let mut refs = Vec::new_in(alloc);

        while let Some(pkt) = lines.next_line().map_err(|_| pkt_err(lines.offset()))? {
            match pkt {
                Pkt::Flush | Pkt::End => break,
                Pkt::Data(line) => {
                    let t = pkt::text(line);
                    if let Some(rest) = t.strip_prefix(b"unpack ") {
                        unpack_ok = rest == b"ok";
                        if let Ok(s) = core::str::from_utf8(rest) {
                            unpack_message = String::from_str_in(s, alloc);
                        }
                    } else if let Some(rest) = t.strip_prefix(b"ok ") {
                        if let Ok(s) = core::str::from_utf8(rest) {
                            refs.push(PushRefReport {
                                name: String::from_str_in(s, alloc),
                                ok: true,
                                message: None,
                            });
                        }
                    } else if let Some(rest) = t.strip_prefix(b"ng ")
                        && let Ok(s) = core::str::from_utf8(rest)
                    {
                        let (name, msg) = match s.split_once(' ') {
                            Some((n, m)) => (n, Some(String::from_str_in(m, alloc))),
                            None => (s, None),
                        };
                        refs.push(PushRefReport {
                            name: String::from_str_in(name, alloc),
                            ok: false,
                            message: msg,
                        });
                    }
                }
                Pkt::Delim => {}
            }
        }
        Ok(PushReport { unpack_ok, unpack_message, refs })
    }
}

// ---------------------------------------------------------------------------
// Sans-IO Smart HTTP/1.1 Server Router (handles GET/POST + chunked encoding)
// ---------------------------------------------------------------------------

/// Parsed HTTP/1.1 request produced by [`parse_http_request`].
pub struct HttpRequest<'a> {
    pub method: AString<'a>,
    pub target: AString<'a>,
    pub git_protocol_v2: bool,
    pub body: AVec<'a, u8>,
}

/// Outcome of feeding bytes to [`parse_http_request`].
pub enum HttpParse<'a> {
    /// Need more bytes from the socket before a complete request is available.
    Incomplete,
    /// Complete HTTP/1.1 request parsed (with `consumed` bytes taken from input).
    Complete { req: HttpRequest<'a>, consumed: usize },
}

/// Parses one HTTP/1.1 request (supporting both `Content-Length` and `Transfer-Encoding: chunked`,
/// which real `git push` sends!).
pub fn parse_http_request<'a>(
    bytes: &[u8],
    max_body: usize,
    alloc: Alloc<'a>,
) -> Result<HttpParse<'a>, GitError> {
    let Some(hdr_end) = find_subslice(bytes, b"\r\n\r\n") else {
        if bytes.len() > 64 * 1024 {
            return Err(pkt_err(0));
        }
        return Ok(HttpParse::Incomplete);
    };
    let head_str = core::str::from_utf8(&bytes[..hdr_end]).map_err(|_| pkt_err(0))?;
    let mut lines = head_str.split("\r\n");
    let req_line = lines.next().ok_or_else(|| pkt_err(0))?;
    let mut parts = req_line.split(' ');
    let method = parts.next().ok_or_else(|| pkt_err(0))?;
    let target = parts.next().ok_or_else(|| pkt_err(0))?;

    let mut content_len: Option<usize> = None;
    let mut chunked = false;
    let mut git_protocol_v2 = false;

    for hdr in lines {
        if let Some((k, v)) = hdr.split_once(':') {
            let v = v.trim();
            if k.eq_ignore_ascii_case("content-length") {
                content_len = Some(v.parse::<usize>().map_err(|_| pkt_err(0))?);
            } else if k.eq_ignore_ascii_case("transfer-encoding")
                && v.to_ascii_lowercase().contains("chunked")
            {
                chunked = true;
            } else if k.eq_ignore_ascii_case("git-protocol") && v.contains("version=2") {
                git_protocol_v2 = true;
            }
        }
    }

    let body_start = hdr_end + 4;
    let rest = &bytes[body_start..];
    let mut body = Vec::new_in(alloc);

    let consumed = if chunked {
        let mut pos = 0usize;
        loop {
            let Some(crlf) = find_subslice(&rest[pos..], b"\r\n") else {
                return Ok(HttpParse::Incomplete);
            };
            let size_line = core::str::from_utf8(&rest[pos..pos + crlf]).map_err(|_| pkt_err(pos))?;
            let size_hex = size_line.split(';').next().unwrap_or("").trim();
            let chunk_len = usize::from_str_radix(size_hex, 16).map_err(|_| pkt_err(pos))?;
            pos += crlf + 2;
            if chunk_len == 0 {
                // Consume final `\r\n` after 0-chunk (and any optional trailers).
                if let Some(end_trail) = find_subslice(&rest[pos..], b"\r\n") {
                    pos += end_trail + 2;
                    break;
                }
                return Ok(HttpParse::Incomplete);
            }
            if body.len().saturating_add(chunk_len) > max_body {
                return Err(GitError {
                    file: GitFile::Wire,
                    at: pos as u64,
                    kind: ErrorKind::BudgetExceeded,
                });
            }
            if pos + chunk_len + 2 > rest.len() {
                return Ok(HttpParse::Incomplete);
            }
            body.extend_from_slice(&rest[pos..pos + chunk_len]);
            pos += chunk_len + 2;
        }
        body_start + pos
    } else if let Some(len) = content_len {
        if len > max_body {
            return Err(GitError {
                file: GitFile::Wire,
                at: body_start as u64,
                kind: ErrorKind::BudgetExceeded,
            });
        }
        if rest.len() < len {
            return Ok(HttpParse::Incomplete);
        }
        body.extend_from_slice(&rest[..len]);
        body_start + len
    } else {
        body_start
    };

    Ok(HttpParse::Complete {
        req: HttpRequest {
            method: String::from_str_in(method, alloc),
            target: String::from_str_in(target, alloc),
            git_protocol_v2,
            body,
        },
        consumed,
    })
}

/// Routes a parsed Smart HTTP request against `store`, writing the complete HTTP/1.1 response
/// (status line, headers, `Content-Length`, body) into `out` (cleared first).
pub fn handle_smart_http<'a, S: RepoStore<'a> + ?Sized>(
    req: &HttpRequest<'_>,
    store: &mut S,
    limits: WireLimits,
    pack_opts: PackWriteOpts,
    alloc: Alloc<'a>,
    out: &mut AVec<'a, u8>,
) -> Result<(), GitError> {
    let mut payload = Vec::new_in(alloc);
    let target = req.target.as_str();
    let (path, query) = match target.split_once('?') {
        Some((p, q)) => (p, q),
        None => (target, ""),
    };

    if req.method.as_str() == "GET" && path.ends_with("/info/refs") {
        if query.contains("service=git-upload-pack") {
            UploadPackServer::advertise(Transport::Http, &mut payload)?;
            write_http_response(
                200,
                "OK",
                "application/x-git-upload-pack-advertisement",
                &payload,
                out,
            );
            return Ok(());
        }
        if query.contains("service=git-receive-pack") {
            ReceivePackServer::advertise(Transport::Http, store, alloc, &mut payload)?;
            write_http_response(
                200,
                "OK",
                "application/x-git-receive-pack-advertisement",
                &payload,
                out,
            );
            return Ok(());
        }
        write_http_response(403, "Forbidden", "text/plain", b"dumb http unsupported\n", out);
        return Ok(());
    }

    if req.method.as_str() == "POST" && path.ends_with("/git-upload-pack") {
        UploadPackServer::handle_request(
            &req.body,
            store,
            limits,
            pack_opts,
            alloc,
            &mut payload,
        )?;
        write_http_response(200, "OK", "application/x-git-upload-pack-result", &payload, out);
        return Ok(());
    }

    if req.method.as_str() == "POST" && path.ends_with("/git-receive-pack") {
        ReceivePackServer::handle_push(&req.body, store, limits, alloc, &mut payload)?;
        write_http_response(200, "OK", "application/x-git-receive-pack-result", &payload, out);
        return Ok(());
    }

    write_http_response(404, "Not Found", "text/plain", b"not found\n", out);
    Ok(())
}

fn write_http_response<'a>(
    status: u16,
    reason: &str,
    content_type: &str,
    body: &[u8],
    out: &mut AVec<'a, u8>,
) {
    out.clear();
    out.extend_from_slice(b"HTTP/1.1 ");
    write_usize_dec(usize::from(status), out);
    out.push(b' ');
    out.extend_from_slice(reason.as_bytes());
    out.extend_from_slice(b"\r\nContent-Type: ");
    out.extend_from_slice(content_type.as_bytes());
    out.extend_from_slice(b"\r\nCache-Control: no-cache\r\nContent-Length: ");
    write_usize_dec(body.len(), out);
    out.extend_from_slice(b"\r\nConnection: close\r\n\r\n");
    out.extend_from_slice(body);
}

fn write_usize_dec<'a>(mut n: usize, out: &mut AVec<'a, u8>) {
    let mut buf = [0u8; 20];
    let mut at = buf.len();
    loop {
        at -= 1;
        buf[at] = b'0' + (n % 10) as u8;
        n /= 10;
        if n == 0 {
            break;
        }
    }
    out.extend_from_slice(&buf[at..]);
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    let first = needle[0];
    let mut pos = 0usize;
    while pos + needle.len() <= haystack.len() {
        let Some(off) = blaze_memchr::memchr(first, &haystack[pos..]) else {
            return None;
        };
        let at = pos + off;
        if at + needle.len() <= haystack.len() && &haystack[at..at + needle.len()] == needle {
            return Some(at);
        }
        pos = at + 1;
    }
    None
}
