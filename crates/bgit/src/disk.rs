//! Native disk driver for `.bgit` (`BGOB` slabs, `BGIX` fanout index, `BGRF` single-file binary
//! reftable, `CGPH` commit-graph) using **read-only `mmap`** (`blaze_core::Mapping::map_file`)
//! and **multi-core parallel Zstd / PACK v2 encoding** across Apple Silicon cores.

use std::{
    alloc::Global,
    fs,
    io::{self, Write as _},
    path::{Path, PathBuf},
    thread,
};

use blaze_core::Mapping;
use blaze_git::{
    ErrorKind, GitError, GitFile,
    commit_graph::CommitGraph,
    infect::{InfectBuilder, InfectStats},
    objects::TreeIter,
    pack_write::{
        PackWriteOpts, WrittenPackEntry, assemble_pack_from_chunks, encode_pack_chunk,
        write_pack_from_store, write_pack_index_v2,
    },
    refs::parse_gitdir_file,
    slab::{
        BorrowedRefTarget, RefTableView, SlabCompression, SlabIndex, SlabIndexEntry, SlabPayload,
        SlabReader, SlabWriter, VerifiedIndex, build_commit_graph_from_store, decode_reftable,
        encode_reftable, encode_slab_index, encode_slab_record_with_oid, write_slab_header,
    },
    storage::{
        MemStore, ObjectStore, RefEntry, RefStore, RefTarget, RefUpdate, RefUpdateStatus,
    },
};
use blaze_git_fetch::{Kind as ObjectKind, Oid, PackLimits, store as git_store};
use blaze_own::{AVec, Alloc, Erased, String as OwnString, Vec as OwnVec};

/// Atomic temp-file + fsync + rename helper so disk writes never leave torn `.bgit` files.
pub(crate) fn write_file_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("tmp.bgit");
    {
        let mut f = fs::File::create(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    fs::rename(&tmp, path)?;
    Ok(())
}

/// Resolves the `.git` directory of a normal repo, linked worktree, or bare repository.
pub(crate) fn locate_git_dir(repo_root: &Path) -> io::Result<PathBuf> {
    let dot_git = repo_root.join(".git");
    if dot_git.is_dir() {
        return Ok(dot_git);
    }
    if dot_git.is_file() {
        let raw = fs::read(&dot_git)?;
        let target = parse_gitdir_file(&raw)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("{e}")))?;
        let p = Path::new(target);
        return Ok(if p.is_absolute() { p.to_path_buf() } else { repo_root.join(p) });
    }
    if repo_root.join("HEAD").is_file() && repo_root.join("objects").is_dir() {
        return Ok(repo_root.to_path_buf());
    }
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        format!("no .git or bare git repository found at {}", repo_root.display()),
    ))
}

/// Returns the `.bgit` storage directory path for `repo_root` (works for both worktree and bare).
pub(crate) fn bgit_dir_for(repo_root: &Path) -> PathBuf {
    if repo_root.file_name().is_some_and(|n| n == ".bgit")
        || repo_root.join("VERSION").is_file()
    {
        repo_root.to_path_buf()
    } else {
        repo_root.join(".bgit")
    }
}

/// Maps a non-empty file read-only via `blaze_core::Mapping::map_file` (zero heap copy, page-cache
/// backed, shared across processes without charging `phys_footprint`).
fn map_readonly(path: &Path) -> io::Result<Mapping> {
    let meta = fs::metadata(path)?;
    if meta.len() == 0 {
        return Ok(Mapping::copy(&[]));
    }
    Mapping::map_file(path)
}

/// Lock-free, thread-safe read-only view over mapped `.bgit` slabs + `BGIX` index, used by worker
/// threads during parallel PACK v2 encoding (`write_pack_parallel`).
struct MappedSlabView<'m, 'a> {
    slabs: &'m [(u16, Mapping)],
    index: Option<(&'m Mapping, VerifiedIndex)>,
    alloc: Alloc<'a>,
}

impl<'m, 'a> MappedSlabView<'m, 'a> {
    #[inline]
    fn idx(&self) -> Option<SlabIndex<'m>> {
        let (m, proof) = self.index?;
        SlabIndex::from_verified(m.bytes(), proof).ok()
    }

    #[inline]
    fn slab_bytes(&self, slab_id: u16) -> Option<&'m [u8]> {
        self.slabs.iter().find(|(id, _)| *id == slab_id).map(|(_, m)| m.bytes())
    }
}

impl<'a> ObjectStore<'a> for MappedSlabView<'_, 'a> {
    fn has_object(&self, oid: &Oid) -> bool {
        self.idx().is_some_and(|i| i.lookup(oid).is_some())
    }

    fn read_object(&self, oid: &Oid, out: &mut AVec<'a, u8>) -> Result<Option<ObjectKind>, GitError> {
        let Some(idx) = self.idx() else {
            return Ok(None);
        };
        let Some(loc) = idx.lookup(oid) else {
            return Ok(None);
        };
        let Some(slab_b) = self.slab_bytes(loc.slab_id) else {
            return Err(GitError { file: GitFile::Slab, at: loc.offset, kind: ErrorKind::Missing });
        };
        let reader = SlabReader::parse(slab_b)?;
        let rec = reader.read_at(loc.offset, 512 << 20, self.alloc, out)?;
        if let SlabPayload::Borrowed(slice) = rec.payload {
            out.clear();
            out.extend_from_slice(slice);
        }
        Ok(Some(rec.kind))
    }

    fn object_size(&self, oid: &Oid, _alloc: Alloc<'a>) -> Result<Option<u64>, GitError> {
        Ok(self.idx().and_then(|idx| idx.lookup(oid)).map(|loc| u64::from(loc.raw_len)))
    }

    fn write_object(&mut self, _kind: ObjectKind, _body: &[u8]) -> Result<Oid, GitError> {
        Err(GitError { file: GitFile::Slab, at: 0, kind: ErrorKind::UnexpectedReply })
    }

    fn list_oids(&self, out: &mut AVec<'a, Oid>) -> Result<(), GitError> {
        if let Some(idx) = self.idx() {
            for i in 0..idx.len() {
                let raw = idx.oid_at(i);
                out.push(Oid(raw.try_into().expect("20 bytes")));
            }
        }
        Ok(())
    }
}

/// Native on-disk `.bgit` repository store implementing [`ObjectStore`] and [`RefStore`].
///
/// Immutable slabs (`objects/slab-NNNN.bgob`), index (`objects/index.bgix`), and commit-graph
/// (`graph.bggp`) are mapped **read-only** via `blaze_core::Mapping::map_file`:
/// - `open` is strictly **O(1)** page faults even on multi-gigabyte monorepos (touches only the
///   16-byte slab headers and 1,040-byte `BGIX` fanout header).
/// - Untouched objects are never paged into memory; accessed pages live in the OS page cache
///   (shared across processes, zero `phys_footprint` overhead).
/// - Newly written objects stage in `overlay` and flush atomically via temp + fsync + rename.
pub(crate) struct BgitDiskStore<'a> {
    pub(crate) bgit_dir: PathBuf,
    alloc: Alloc<'a>,
    slabs: Vec<(u16, Mapping)>,
    index: Option<(Mapping, VerifiedIndex)>,
    graph: Option<Mapping>,
    /// Read-only `mmap` of `refs.bgref` (`BGRF` v1/v2) for zero-copy O(log R) ref lookups.
    reftable: Option<Mapping>,
    /// True once refs have been copied into `overlay` (happens lazily on first ref mutation).
    refs_materialized: bool,
    overlay: MemStore<'a>,
    dirty_objects: bool,
    dirty_refs: bool,
    compression: SlabCompression,
}

impl<'a> BgitDiskStore<'a> {
    /// Initializes a brand-new empty `.bgit` repository at `repo_root`.
    pub(crate) fn init(repo_root: &Path, alloc: Alloc<'a>) -> io::Result<BgitDiskStore<'a>> {
        let bgit_dir = bgit_dir_for(repo_root);
        fs::create_dir_all(bgit_dir.join("objects"))?;
        write_file_atomic(&bgit_dir.join("VERSION"), b"BGIT0001\n")?;

        let mut overlay = MemStore::new(alloc);
        overlay
            .write_head(&RefTarget::Symbolic(OwnString::from_str_in("refs/heads/main", alloc)))
            .map_err(|e| io::Error::other(format!("{e}")))?;

        let mut store = BgitDiskStore {
            bgit_dir,
            alloc,
            slabs: Vec::new(),
            index: None,
            graph: None,
            reftable: None,
            refs_materialized: true,
            overlay,
            dirty_objects: true,
            dirty_refs: true,
            compression: SlabCompression::AutoZstd,
        };
        store.flush()?;
        Ok(store)
    }

    /// Opens an existing `.bgit` repository on disk with O(1) read-only `mmap` (zero ref decoding!).
    pub(crate) fn open(repo_root: &Path, alloc: Alloc<'a>) -> io::Result<BgitDiskStore<'a>> {
        let bgit_dir = bgit_dir_for(repo_root);
        let ver_path = bgit_dir.join("VERSION");
        if !ver_path.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("no .bgit store at {} (run `bgit infect` first)", bgit_dir.display()),
            ));
        }

        let mut slabs: Vec<(u16, Mapping)> = Vec::new();
        let obj_dir = bgit_dir.join("objects");
        if obj_dir.is_dir() {
            let mut slab_files: Vec<(u16, PathBuf)> = Vec::new();
            for entry in fs::read_dir(&obj_dir)? {
                let entry = entry?;
                let name = entry.file_name();
                let name_str = name.to_string_lossy();
                if let Some(stem) = name_str.strip_prefix("slab-").and_then(|s| s.strip_suffix(".bgob"))
                    && let Ok(id) = stem.parse::<u16>()
                {
                    slab_files.push((id, entry.path()));
                }
            }
            slab_files.sort_by_key(|(id, _)| *id);
            for (id, path) in slab_files {
                let mapped = map_readonly(&path)?;
                let _ = SlabReader::parse(mapped.bytes())
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("{e}")))?;
                slabs.push((id, mapped));
            }
        }

        let idx_path = obj_dir.join("index.bgix");
        let index = if idx_path.is_file() {
            let mapped = map_readonly(&idx_path)?;
            // O(1) header + fanout check: touches only 1,040 bytes (1 page)!
            let proof = SlabIndex::parse_header_fast(mapped.bytes(), 4_000_000)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("{e}")))?
                .proof();
            Some((mapped, proof))
        } else {
            None
        };

        let cg_path = bgit_dir.join("graph.bggp");
        let graph = if cg_path.is_file() {
            let mapped = map_readonly(&cg_path)?;
            if mapped.bytes().is_empty() { None } else { Some(mapped) }
        } else {
            None
        };

        let rt_path = bgit_dir.join("refs.bgref");
        let reftable = if rt_path.is_file() {
            let mapped_rt = map_readonly(&rt_path)?;
            // O(1) header + offset-table bounds check; zero ref decoding!
            let _ = RefTableView::parse_fast(mapped_rt.bytes(), 1_000_000)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("{e}")))?;
            Some(mapped_rt)
        } else {
            None
        };

        let overlay = MemStore::new(alloc);
        Ok(BgitDiskStore {
            bgit_dir,
            alloc,
            slabs,
            index,
            graph,
            reftable,
            refs_materialized: false,
            overlay,
            dirty_objects: false,
            dirty_refs: false,
            compression: SlabCompression::AutoZstd,
        })
    }

    #[inline]
    fn reftable_view(&self) -> Option<RefTableView<'_>> {
        let mapped = self.reftable.as_ref()?;
        RefTableView::parse_fast(mapped.bytes(), 1_000_000).ok()
    }

    /// Lazily materializes all refs from the mmap'd `refs.bgref` into `overlay` on first mutation.
    fn ensure_refs_materialized(&mut self) -> Result<(), GitError> {
        if self.refs_materialized {
            return Ok(());
        }
        if let Some(mapped) = self.reftable.as_ref() {
            let view = RefTableView::parse_fast(mapped.bytes(), 1_000_000)?;
            if let Some(h) = view.head()? {
                let target = match h {
                    BorrowedRefTarget::Direct(o) => RefTarget::Direct(o),
                    BorrowedRefTarget::Symbolic(s) => {
                        RefTarget::Symbolic(OwnString::from_str_in(s, self.alloc))
                    }
                };
                self.overlay.write_head(&target)?;
            }
            let mut refs = OwnVec::new_in(self.alloc);
            view.list_prefix("", self.alloc, &mut refs)?;
            for r in refs {
                self.overlay.set_ref(r.name.as_str(), r.target)?;
            }
        }
        self.refs_materialized = true;
        Ok(())
    }

    #[inline]
    fn verified_index(&self) -> Option<SlabIndex<'_>> {
        let (mapped, proof) = self.index.as_ref()?;
        SlabIndex::from_verified(mapped.bytes(), *proof).ok()
    }

    /// Appends OIDs of all objects written into the in-memory overlay since the last `flush()`.
    pub(crate) fn overlay_oids(&self, out: &mut AVec<'a, Oid>) -> io::Result<()> {
        self.overlay
            .list_oids(out)
            .map_err(|e| io::Error::other(format!("{e}")))
    }

    /// Verifies the full CRC-32 checksum of `index.bgix` and every record across all slabs.
    pub(crate) fn verify_full(&self) -> io::Result<u32> {
        if let Some(mapped_rt) = self.reftable.as_ref() {
            let _ = decode_reftable(mapped_rt.bytes(), 1_000_000, self.alloc)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("refs.bgref: {e}")))?;
        }
        if let Some((mapped, _)) = self.index.as_ref() {
            let _ = SlabIndex::parse(mapped.bytes(), 4_000_000)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("index.bgix: {e}")))?;
        }
        let mut checked = 0u32;
        let mut scratch = OwnVec::new_in(self.alloc);
        for (id, mapped) in &self.slabs {
            let reader = SlabReader::parse(mapped.bytes())
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("slab-{id:04}: {e}")))?;
            let mut entries = OwnVec::new_in(self.alloc);
            reader
                .scan_headers(*id, &mut entries)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("slab-{id:04}: {e}")))?;
            for e in entries {
                let rec = reader
                    .read_at(e.offset, 512 << 20, self.alloc, &mut scratch)
                    .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, format!("slab-{id:04} @{}: {err}", e.offset)))?;
                let bytes: &[u8] = match rec.payload {
                    SlabPayload::Borrowed(s) => s,
                    SlabPayload::InOut => &scratch,
                };
                if Oid::of(rec.kind, bytes) != rec.oid {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("SHA-1 mismatch for {} in slab-{id:04}", rec.oid),
                    ));
                }
                checked += 1;
            }
        }
        Ok(checked)
    }

    /// Borrows the mapped `CGPH` commit-graph if present (zero-copy over mmap).
    pub(crate) fn commit_graph(&self) -> Option<CommitGraph<'_, 'a>> {
        let mapped = self.graph.as_ref()?;
        CommitGraph::from_layers(&[mapped.bytes()], self.alloc).ok()
    }

    /// Clones the read-only `Mapping` handle to `graph.bggp` in O(1) (Arc refcount bump, zero copy).
    pub(crate) fn graph_mapping(&self) -> Option<Mapping> {
        self.graph.clone()
    }

    /// Number of indexed objects across slabs plus overlay.
    pub(crate) fn object_count(&self) -> usize {
        let slab_n = self.verified_index().map_or(0, |i| i.len() as usize);
        slab_n + self.overlay.object_count()
    }

    fn find_slab_bytes(&self, slab_id: u16) -> Option<&[u8]> {
        self.slabs.iter().find(|(id, _)| *id == slab_id).map(|(_, m)| m.bytes())
    }

    /// Encodes `oids` into a valid Git PACK v2 stream using all available CPU cores when
    /// `oids.len() >= 64` and objects live in read-only mapped slabs (lock-free concurrent reads!).
    pub(crate) fn write_pack_parallel(
        &self,
        oids: &[Oid],
        opts: PackWriteOpts,
        out: &mut AVec<'a, u8>,
    ) -> Result<AVec<'a, WrittenPackEntry>, GitError> {
        let workers = thread::available_parallelism().map_or(1, |n| n.get()).min(16);
        if oids.len() < 64 || workers <= 1 || self.overlay.object_count() > 0 {
            return write_pack_from_store(self, oids, opts, self.alloc, out);
        }

        let chunk_size = oids.len().div_ceil(workers);
        let slices: Vec<&[Oid]> = oids.chunks(chunk_size).collect();
        let slabs = &self.slabs;
        let index = self.index.as_ref().map(|(m, p)| (m, *p));

        let mut results: Vec<Result<(Vec<u8>, Vec<WrittenPackEntry>), GitError>> =
            Vec::with_capacity(slices.len());

        thread::scope(|s| {
            let mut handles = Vec::with_capacity(slices.len());
            for slice in slices {
                handles.push(s.spawn(move || {
                    let erased = Erased::new(Global);
                    let w_alloc = erased.handle();
                    let view = MappedSlabView { slabs, index, alloc: w_alloc };
                    let chunk = encode_pack_chunk(&view, slice, opts, w_alloc)?;
                    Ok((chunk.bytes.as_slice().to_vec(), chunk.entries.as_slice().to_vec()))
                }));
            }
            for h in handles {
                results.push(h.join().expect("pack worker thread"));
            }
        });

        let mut owned_chunks: Vec<(Vec<u8>, Vec<WrittenPackEntry>)> =
            Vec::with_capacity(results.len());
        for res in results {
            owned_chunks.push(res?);
        }
        let borrowed: Vec<(&[u8], &[WrittenPackEntry])> = owned_chunks
            .iter()
            .map(|(b, e)| (b.as_slice(), e.as_slice()))
            .collect();

        assemble_pack_from_chunks(&borrowed, opts.most_objects, self.alloc, out)
    }

    /// Flushes any staged objects, ref updates, and rebuilt index/commit-graph atomically to disk.
    pub(crate) fn flush(&mut self) -> io::Result<()> {
        if self.dirty_objects {
            let mut new_oids = OwnVec::new_in(self.alloc);
            self.overlay
                .list_oids(&mut new_oids)
                .map_err(|e| io::Error::other(format!("{e}")))?;

            let mut new_entries: AVec<'a, SlabIndexEntry> = OwnVec::new_in(self.alloc);
            let mut new_commits: AVec<'a, blaze_git::slab::GraphCommitInput<'a>> =
                OwnVec::new_in(self.alloc);
            if !new_oids.is_empty() {
                let next_id = self.slabs.last().map_or(0, |(id, _)| id.saturating_add(1));
                let mut writer = SlabWriter::new(next_id, self.compression, self.alloc);
                let mut buf = OwnVec::new_in(self.alloc);
                for oid in &new_oids {
                    if let Some(kind) = self
                        .overlay
                        .read_object(oid, &mut buf)
                        .map_err(|e| io::Error::other(format!("{e}")))?
                    {
                        if kind == ObjectKind::Commit
                            && let Ok(parsed) = git_store::Commit::parse(&buf)
                        {
                            let mut parents = OwnVec::new_in(self.alloc);
                            for p in parsed.parents() {
                                parents.push(p);
                            }
                            new_commits.push(blaze_git::slab::GraphCommitInput {
                                oid: *oid,
                                tree: parsed.tree,
                                commit_time: u64::try_from(parsed.committed).unwrap_or(0),
                                parents,
                            });
                        }
                        writer
                            .append_with_oid(*oid, kind, &buf)
                            .map_err(|e| io::Error::other(format!("{e}")))?;
                    }
                }
                let (slab_bytes, entries) = writer.into_parts();
                new_entries = entries;
                let slab_path = self
                    .bgit_dir
                    .join("objects")
                    .join(format!("slab-{next_id:04}.bgob"));
                write_file_atomic(&slab_path, &slab_bytes)?;
                let mapped = map_readonly(&slab_path)?;
                self.slabs.push((next_id, mapped));
            }

            // Incremental BGIX index merge: reuse existing verified index entries in O(N_index)
            // instead of re-scanning gigabytes of historical slabs from disk!
            let mut all_entries: AVec<'a, SlabIndexEntry> = OwnVec::new_in(self.alloc);
            if let Some((idx_mapped, proof)) = self.index.as_ref()
                && let Ok(old_idx) = SlabIndex::from_verified(idx_mapped.bytes(), *proof)
            {
                let old_len = old_idx.len();
                all_entries.reserve(old_len as usize + new_entries.len());
                for i in 0..old_len {
                    let oid_bytes = old_idx.oid_at(i);
                    let mut raw = [0u8; 20];
                    raw.copy_from_slice(oid_bytes);
                    let loc = old_idx.loc_at(i);
                    all_entries.push(SlabIndexEntry {
                        oid: Oid(raw),
                        slab_id: loc.slab_id,
                        offset: loc.offset,
                        raw_len: loc.raw_len,
                    });
                }
                all_entries.extend(new_entries);
            } else {
                for (id, mapped) in &self.slabs {
                    let reader = SlabReader::parse(mapped.bytes())
                        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("{e}")))?;
                    reader
                        .scan_headers(*id, &mut all_entries)
                        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("{e}")))?;
                }
            }
            let mut idx_out = OwnVec::new_in(self.alloc);
            encode_slab_index(&all_entries, self.alloc, &mut idx_out)
                .map_err(|e| io::Error::other(format!("{e}")))?;
            let idx_path = self.bgit_dir.join("objects").join("index.bgix");
            write_file_atomic(&idx_path, &idx_out)?;
            let idx_mapped = map_readonly(&idx_path)?;
            let proof = SlabIndex::parse_header_fast(idx_mapped.bytes(), 4_000_000)
                .map_err(|e| io::Error::other(format!("{e}")))?
                .proof();
            self.index = Some((idx_mapped, proof));

            // Incremental CGPH commit-graph update: reuse existing mmap'd `graph.bggp` in ~3 ms
            // (zero historical object decompression!) and skip completely if no new commits arrived!
            if !new_commits.is_empty() || self.graph.is_none() {
                let mut cg_out = OwnVec::new_in(self.alloc);
                let built_incremental = if let Some(cg_mapped) = self.graph.as_ref()
                    && let Ok(layer) = blaze_git::commit_graph::CommitGraphLayer::parse(cg_mapped.bytes(), 0)
                {
                    let old_n = layer.len();
                    let mut all_commits =
                        OwnVec::with_capacity_in(old_n as usize + new_commits.len(), self.alloc);
                    let mut ppos = OwnVec::new_in(self.alloc);
                    for i in 0..old_n {
                        let oid = layer.oid_at_local(i);
                        let gc = layer.commit_at_local(i);
                        ppos.clear();
                        layer.parents_of(&gc, &mut ppos);
                        let mut parents = OwnVec::with_capacity_in(ppos.len(), self.alloc);
                        for &p in &ppos {
                            if p < old_n {
                                parents.push(layer.oid_at_local(p));
                            }
                        }
                        all_commits.push(blaze_git::slab::GraphCommitInput {
                            oid,
                            tree: gc.tree,
                            commit_time: gc.commit_time,
                            parents,
                        });
                    }
                    all_commits.extend(new_commits);
                    let _ = blaze_git::slab::encode_commit_graph(&all_commits, self.alloc, &mut cg_out)
                        .map_err(|e| io::Error::other(format!("{e}")))?;
                    true
                } else {
                    false
                };
                if !built_incremental {
                    let _ = build_commit_graph_from_store(self, self.alloc, &mut cg_out)
                        .map_err(|e| io::Error::other(format!("{e}")))?;
                }
                let cg_path = self.bgit_dir.join("graph.bggp");
                write_file_atomic(&cg_path, &cg_out)?;
                self.graph = if cg_out.is_empty() { None } else { Some(map_readonly(&cg_path)?) };
            }

            // Clear overlay objects now persisted in slabs (preserve refs in overlay!).
            let head = self
                .overlay
                .read_head(self.alloc)
                .map_err(|e| io::Error::other(format!("{e}")))?;
            let mut refs = OwnVec::new_in(self.alloc);
            self.overlay
                .list_refs("", self.alloc, &mut refs)
                .map_err(|e| io::Error::other(format!("{e}")))?;
            let mut fresh = MemStore::new(self.alloc);
            if let Some(h) = head {
                fresh
                    .write_head(&h)
                    .map_err(|e| io::Error::other(format!("{e}")))?;
            }
            for r in refs {
                fresh
                    .set_ref(r.name.as_str(), r.target)
                    .map_err(|e| io::Error::other(format!("{e}")))?;
            }
            self.overlay = fresh;
            self.dirty_objects = false;
        }

        if self.dirty_refs {
            let _ = self.ensure_refs_materialized();
            let head = self
                .overlay
                .read_head(self.alloc)
                .map_err(|e| io::Error::other(format!("{e}")))?;
            let mut refs = OwnVec::new_in(self.alloc);
            self.overlay
                .list_refs("", self.alloc, &mut refs)
                .map_err(|e| io::Error::other(format!("{e}")))?;
            let mut rt_out = OwnVec::new_in(self.alloc);
            encode_reftable(head.as_ref(), &refs, self.alloc, &mut rt_out)
                .map_err(|e| io::Error::other(format!("{e}")))?;
            let rt_path = self.bgit_dir.join("refs.bgref");
            write_file_atomic(&rt_path, &rt_out)?;
            self.reftable = Some(map_readonly(&rt_path)?);
            self.dirty_refs = false;
        }
        Ok(())
    }

    /// Resolves a Git revision expression:
    /// - 40-hex OID or unambiguous short hex prefix (`>= 4` hex chars)
    /// - `"HEAD"` or `"@"`
    /// - full refname, `refs/heads/<r>`, `refs/tags/<r>`, `refs/remotes/origin/<r>`, `refs/remotes/<r>`
    /// - suffix chain: `~`, `~N`, `^`, `^N` (`^0` peels to commit, `^1`..`^N` selects Nth parent),
    ///   `^{commit}`, `^{tree}`, `^{}` (peel annotated tags)
    pub(crate) fn resolve_rev(&self, rev: &str) -> Result<Option<Oid>, GitError> {
        let rev = rev.trim();
        if rev.is_empty() {
            return Ok(None);
        }
        // Split base revision from trailing `~` / `^` suffix chain.
        let split_at = rev.find(['~', '^']).unwrap_or(rev.len());
        let (base_raw, suffix) = rev.split_at(split_at);
        let base_str = if base_raw == "@" { "HEAD" } else { base_raw };
        let Some(mut oid) = self.resolve_base_rev(base_str)? else {
            return Ok(None);
        };
        if suffix.is_empty() {
            return Ok(Some(oid));
        }

        let mut rest = suffix;
        let mut buf = OwnVec::new_in(self.alloc);
        while !rest.is_empty() {
            if let Some(after) = rest.strip_prefix('~') {
                let digits_len = after.bytes().take_while(|b| b.is_ascii_digit()).count();
                let count: usize = if digits_len == 0 {
                    1
                } else {
                    after[..digits_len].parse().unwrap_or(usize::MAX)
                };
                rest = &after[digits_len..];
                if let Some(peeled) = blaze_git::storage::peel_tag(self, oid, 8, self.alloc)? {
                    oid = peeled;
                }
                for _ in 0..count {
                    if self.read_object(&oid, &mut buf)? != Some(ObjectKind::Commit) {
                        return Ok(None);
                    }
                    let Ok(commit) = git_store::Commit::parse(&buf) else {
                        return Ok(None);
                    };
                    let Some(parent) = commit.parents().next() else {
                        return Ok(None);
                    };
                    oid = parent;
                }
            } else if let Some(after) = rest.strip_prefix("^{") {
                let Some(end) = after.find('}') else {
                    return Ok(None);
                };
                let target_kind = &after[..end];
                rest = &after[end + 1..];
                if let Some(peeled) = blaze_git::storage::peel_tag(self, oid, 8, self.alloc)? {
                    oid = peeled;
                }
                match target_kind {
                    "" | "commit" => {
                        if self.read_object(&oid, &mut buf)? != Some(ObjectKind::Commit) {
                            return Ok(None);
                        }
                    }
                    "tree" => {
                        match self.read_object(&oid, &mut buf)? {
                            Some(ObjectKind::Tree) => {}
                            Some(ObjectKind::Commit) => {
                                let Ok(commit) = git_store::Commit::parse(&buf) else {
                                    return Ok(None);
                                };
                                oid = commit.tree;
                            }
                            _ => return Ok(None),
                        }
                    }
                    _ => return Ok(None),
                }
            } else if let Some(after) = rest.strip_prefix('^') {
                let digits_len = after.bytes().take_while(|b| b.is_ascii_digit()).count();
                let parent_idx: usize = if digits_len == 0 {
                    1
                } else {
                    after[..digits_len].parse().unwrap_or(usize::MAX)
                };
                rest = &after[digits_len..];
                if let Some(peeled) = blaze_git::storage::peel_tag(self, oid, 8, self.alloc)? {
                    oid = peeled;
                }
                if parent_idx == 0 {
                    if self.read_object(&oid, &mut buf)? != Some(ObjectKind::Commit) {
                        return Ok(None);
                    }
                } else {
                    if self.read_object(&oid, &mut buf)? != Some(ObjectKind::Commit) {
                        return Ok(None);
                    }
                    let Ok(commit) = git_store::Commit::parse(&buf) else {
                        return Ok(None);
                    };
                    let Some(parent) = commit.parents().nth(parent_idx - 1) else {
                        return Ok(None);
                    };
                    oid = parent;
                }
            } else {
                return Ok(None);
            }
        }
        Ok(Some(oid))
    }

    fn resolve_base_rev(&self, rev: &str) -> Result<Option<Oid>, GitError> {
        if let Some(oid) = Oid::parse_hex(rev.as_bytes()) {
            return Ok(Some(oid));
        }
        if let Some(oid) = self.resolve_ref(rev)? {
            return Ok(Some(oid));
        }
        let branch = format!("refs/heads/{rev}");
        if let Some(oid) = self.resolve_ref(&branch)? {
            return Ok(Some(oid));
        }
        let tag = format!("refs/tags/{rev}");
        if let Some(oid) = self.resolve_ref(&tag)? {
            return Ok(Some(oid));
        }
        let remote = format!("refs/remotes/origin/{rev}");
        if let Some(oid) = self.resolve_ref(&remote)? {
            return Ok(Some(oid));
        }
        let remote_full = format!("refs/remotes/{rev}");
        if let Some(oid) = self.resolve_ref(&remote_full)? {
            return Ok(Some(oid));
        }
        // Short hex OID prefix (`>= 4` ASCII hex chars, unambiguous match across store).
        if rev.len() >= 4
            && rev.len() < 40
            && rev.bytes().all(|b| b.is_ascii_hexdigit())
        {
            let lower = rev.to_ascii_lowercase();
            let mut all = OwnVec::new_in(self.alloc);
            self.list_oids(&mut all)?;
            let mut found: Option<Oid> = None;
            for oid in all {
                let hex = oid.hex();
                let hex_str = core::str::from_utf8(&hex).expect("ascii hex");
                if hex_str.starts_with(&lower) {
                    if found.is_some() {
                        // Ambiguous prefix.
                        return Ok(None);
                    }
                    found = Some(oid);
                }
            }
            return Ok(found);
        }
        Ok(None)
    }

    /// Peels `rev` (including annotated tags) to a commit OID, returning `None` if not a commit.
    pub(crate) fn resolve_commit(&self, rev: &str) -> Result<Option<Oid>, GitError> {
        let Some(oid) = self.resolve_rev(rev)? else {
            return Ok(None);
        };
        let oid = blaze_git::storage::peel_tag(self, oid, 8, self.alloc)?.unwrap_or(oid);
        let mut buf = OwnVec::new_in(self.alloc);
        if self.read_object(&oid, &mut buf)? == Some(ObjectKind::Commit) {
            Ok(Some(oid))
        } else {
            Ok(None)
        }
    }
}

/// Resolves the common `.git` directory for a normal repo or linked worktree, if `.git` exists.
pub(crate) fn resolve_common_git_dir(repo_root: &Path) -> Option<PathBuf> {
    let dot_git = repo_root.join(".git");
    if dot_git.is_dir() {
        return Some(dot_git);
    }
    if dot_git.is_file() {
        let raw = fs::read(&dot_git).ok()?;
        let target = parse_gitdir_file(&raw).ok()?;
        let gd = {
            let p = Path::new(target);
            if p.is_absolute() { p.to_path_buf() } else { repo_root.join(p) }
        };
        let common = gd.join("commondir");
        if common.is_file() {
            let c = fs::read_to_string(&common).ok()?;
            let c = c.trim();
            let p = Path::new(c);
            return Some(if p.is_absolute() { p.to_path_buf() } else { gd.join(p) });
        }
        return Some(gd);
    }
    None
}

/// Mirror newly written overlay objects into `.git/objects` as loose zlib objects so plain `git`
/// commands immediately see objects written by `bgit`.
pub(crate) fn mirror_overlay_objects_to_git<'a>(
    store: &BgitDiskStore<'a>,
    repo_root: &Path,
    alloc: Alloc<'a>,
) {
    let Some(git_dir) = resolve_common_git_dir(repo_root) else {
        return;
    };
    let obj_root = git_dir.join("objects");
    if !obj_root.is_dir() {
        return;
    }
    let mut oids = OwnVec::new_in(alloc);
    if store.overlay_oids(&mut oids).is_err() {
        return;
    }
    let mut body = OwnVec::new_in(alloc);
    let mut loose = OwnVec::new_in(alloc);
    for oid in oids {
        let hex = oid.hex();
        let hex_str = core::str::from_utf8(&hex).expect("ascii hex");
        let dir = obj_root.join(&hex_str[..2]);
        let dst = dir.join(&hex_str[2..]);
        if dst.is_file() {
            continue;
        }
        let Ok(Some(kind)) = store.read_object(&oid, &mut body) else {
            continue;
        };
        if blaze_git::pack_write::encode_loose_object(kind, &body, alloc, &mut loose).is_err() {
            continue;
        }
        let _ = fs::create_dir_all(&dir);
        let tmp = dir.join(format!("tmp_bgit_{}", &hex_str[2..]));
        if fs::write(&tmp, loose.as_slice()).is_ok() {
            let _ = fs::rename(&tmp, &dst);
        }
    }
}

/// Mirror a branch/tag ref update into `.git/<ref>` when `.git` exists so `git` and `bgit` stay in sync.
pub(crate) fn mirror_ref_to_git(repo_root: &Path, full_ref: &str, oid: Oid) {
    let Some(git_dir) = resolve_common_git_dir(repo_root) else {
        return;
    };
    if !full_ref.starts_with("refs/") || full_ref.contains("..") {
        return;
    }
    let ref_path = git_dir.join(full_ref);
    if let Some(parent) = ref_path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let _ = write_file_atomic(&ref_path, format!("{oid}\n").as_bytes());
}

/// Remove a loose/packed ref in `.git` when deleted through `bgit` so `git` sees the deletion.
pub(crate) fn delete_ref_in_git(repo_root: &Path, full_ref: &str) {
    let Some(git_dir) = resolve_common_git_dir(repo_root) else {
        return;
    };
    if !full_ref.starts_with("refs/") || full_ref.contains("..") {
        return;
    }
    let ref_path = git_dir.join(full_ref);
    let _ = fs::remove_file(&ref_path);
    // Also strip from `.git/packed-refs` if present.
    let packed = git_dir.join("packed-refs");
    if let Ok(raw) = fs::read_to_string(&packed) {
        let mut changed = false;
        let mut out = String::with_capacity(raw.len());
        let mut skip_peel = false;
        for line in raw.lines() {
            if skip_peel {
                if line.starts_with('^') {
                    continue;
                }
                skip_peel = false;
            }
            if let Some((_, name)) = line.split_once(' ')
                && name == full_ref
            {
                changed = true;
                skip_peel = true;
                continue;
            }
            out.push_str(line);
            out.push('\n');
        }
        if changed {
            let _ = write_file_atomic(&packed, out.as_bytes());
        }
    }
}

/// Mirror a `HEAD` update into `.git/HEAD` (worktree-local git dir) when `.git` exists.
pub(crate) fn mirror_head_to_git(repo_root: &Path, target: &RefTarget<'_>) {
    let Ok(git_dir) = locate_git_dir(repo_root) else {
        return;
    };
    let line = match target {
        RefTarget::Symbolic(sym) => format!("ref: {}\n", sym.as_str()),
        RefTarget::Direct(oid) => format!("{oid}\n"),
    };
    let _ = write_file_atomic(&git_dir.join("HEAD"), line.as_bytes());
}

impl<'a> ObjectStore<'a> for BgitDiskStore<'a> {
    fn has_object(&self, oid: &Oid) -> bool {
        if self.overlay.has_object(oid) {
            return true;
        }
        self.verified_index().is_some_and(|idx| idx.lookup(oid).is_some())
    }

    fn read_object(&self, oid: &Oid, out: &mut AVec<'a, u8>) -> Result<Option<ObjectKind>, GitError> {
        if self.overlay.has_object(oid) {
            return self.overlay.read_object(oid, out);
        }
        let Some(idx) = self.verified_index() else {
            return Ok(None);
        };
        let Some(loc) = idx.lookup(oid) else {
            return Ok(None);
        };
        let Some(slab_b) = self.find_slab_bytes(loc.slab_id) else {
            return Err(GitError { file: GitFile::Slab, at: loc.offset, kind: ErrorKind::Missing });
        };
        let reader = SlabReader::parse(slab_b)?;
        let rec = reader.read_at(loc.offset, 512 << 20, self.alloc, out)?;
        if let SlabPayload::Borrowed(slice) = rec.payload {
            out.clear();
            out.extend_from_slice(slice);
        }
        Ok(Some(rec.kind))
    }

    fn object_size(&self, oid: &Oid, alloc: Alloc<'a>) -> Result<Option<u64>, GitError> {
        if self.overlay.has_object(oid) {
            return self.overlay.object_size(oid, alloc);
        }
        Ok(self.verified_index().and_then(|idx| idx.lookup(oid)).map(|loc| u64::from(loc.raw_len)))
    }

    fn write_object(&mut self, kind: ObjectKind, body: &[u8]) -> Result<Oid, GitError> {
        let oid = Oid::of(kind, body);
        if self.has_object(&oid) {
            return Ok(oid);
        }
        self.dirty_objects = true;
        self.overlay.write_object(kind, body)
    }

    fn list_oids(&self, out: &mut AVec<'a, Oid>) -> Result<(), GitError> {
        if let Some(idx) = self.verified_index() {
            for i in 0..idx.len() {
                let raw = idx.oid_at(i);
                out.push(Oid(raw.try_into().expect("20 bytes")));
            }
        }
        self.overlay.list_oids(out)?;
        out.sort_unstable();
        let mut w = 0usize;
        for r in 0..out.len() {
            if w == 0 || out[r] != out[w - 1] {
                out[w] = out[r];
                w += 1;
            }
        }
        out.truncate(w);
        Ok(())
    }
}

impl<'a> RefStore<'a> for BgitDiskStore<'a> {
    fn read_head(&self, alloc: Alloc<'a>) -> Result<Option<RefTarget<'a>>, GitError> {
        if !self.refs_materialized {
            let Some(view) = self.reftable_view() else {
                return Ok(None);
            };
            return Ok(view.head()?.map(|t| match t {
                BorrowedRefTarget::Direct(o) => RefTarget::Direct(o),
                BorrowedRefTarget::Symbolic(s) => RefTarget::Symbolic(OwnString::from_str_in(s, alloc)),
            }));
        }
        self.overlay.read_head(alloc)
    }

    fn write_head(&mut self, target: &RefTarget<'_>) -> Result<(), GitError> {
        self.ensure_refs_materialized()?;
        self.dirty_refs = true;
        self.overlay.write_head(target)
    }

    fn read_ref(&self, name: &str, alloc: Alloc<'a>) -> Result<Option<RefTarget<'a>>, GitError> {
        if !self.refs_materialized {
            let Some(view) = self.reftable_view() else {
                return Ok(None);
            };
            return Ok(view.get(name)?.map(|e| match e.target {
                BorrowedRefTarget::Direct(o) => RefTarget::Direct(o),
                BorrowedRefTarget::Symbolic(s) => RefTarget::Symbolic(OwnString::from_str_in(s, alloc)),
            }));
        }
        self.overlay.read_ref(name, alloc)
    }

    fn resolve_ref(&self, name: &str) -> Result<Option<Oid>, GitError> {
        if !self.refs_materialized {
            let Some(view) = self.reftable_view() else {
                return Ok(None);
            };
            return view.resolve(name);
        }
        self.overlay.resolve_ref(name)
    }

    fn list_refs(
        &self,
        prefix: &str,
        alloc: Alloc<'a>,
        out: &mut AVec<'a, RefEntry<'a>>,
    ) -> Result<(), GitError> {
        if !self.refs_materialized {
            if let Some(view) = self.reftable_view() {
                view.list_prefix(prefix, alloc, out)?;
            }
        } else {
            self.overlay.list_refs(prefix, alloc, out)?;
        }
        for entry in out.iter_mut() {
            // Only tag refs can point to annotated tag objects; avoid decompressing thousands of
            // branch commits during `bgit branch` on large monorepos!
            if entry.peeled.is_none()
                && entry.name.as_str().starts_with("refs/tags/")
                && let Some(oid) = match &entry.target {
                    RefTarget::Direct(o) => Some(*o),
                    RefTarget::Symbolic(_) => self.resolve_ref(entry.name.as_str())?,
                }
            {
                entry.peeled = blaze_git::storage::peel_tag(self, oid, 8, alloc)?;
            }
        }
        Ok(())
    }

    fn apply_ref_updates(
        &mut self,
        updates: &[RefUpdate<'_>],
        atomic: bool,
        alloc: Alloc<'a>,
    ) -> Result<AVec<'a, RefUpdateStatus>, GitError> {
        self.ensure_refs_materialized()?;
        let res = self.overlay.apply_ref_updates(updates, atomic, alloc)?;
        if res.iter().any(|s| *s == RefUpdateStatus::Ok) {
            self.dirty_refs = true;
        }
        Ok(res)
    }
}

/// Converts an existing `.git` repository at `repo_root` into a native `.bgit` store on disk,
/// inflating loose objects and Zstd-compressing `BGOB` slab records **in parallel across all
/// available CPU cores** with byte-for-byte deterministic output!
pub(crate) fn infect_repo_on_disk<'a>(
    repo_root: &Path,
    compression: SlabCompression,
    alloc: Alloc<'a>,
) -> io::Result<InfectStats> {
    let git_dir = locate_git_dir(repo_root)?;
    let common_dir = resolve_common_git_dir(repo_root).unwrap_or_else(|| git_dir.clone());
    let workers = thread::available_parallelism().map_or(1, |n| n.get()).min(16);

    // Local disk `.git` repositories (e.g. monorepos with dozens of packfiles) can exceed the 2 GiB
    // untrusted network fetch cap; use an explicit monorepo-sized ingest budget here.
    let local_pack_limits = PackLimits {
        most_objects: 16_000_000,
        most_object: 1 << 30,
        most_total: 64 << 30,
        most_depth: 4096,
        most_cache: 128 << 20,
    };
    let mut builder = InfectBuilder::new(local_pack_limits, alloc);
    let mut loose_count = 0u32;
    let mut packfiles_in = 0u32;
    let mut packed_objects_in = 0u32;

    // 1. Collect packfiles and unpack via `InfectBuilder` (objects live in `common_dir` for linked worktrees).
    let pack_dir = common_dir.join("objects").join("pack");
    if pack_dir.is_dir() {
        let mut packs: Vec<PathBuf> = Vec::new();
        for entry in fs::read_dir(&pack_dir)? {
            let entry = entry?;
            let p = entry.path();
            if p.extension().is_some_and(|e| e == "pack") {
                packs.push(p);
            }
        }
        packs.sort();
        for pack_path in packs {
            let mapped = map_readonly(&pack_path)?;
            let n = builder
                .feed_packfile(mapped.bytes())
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("{}: {e}", pack_path.display())))?;
            packfiles_in += 1;
            packed_objects_in += n;
        }
    }

    // 2. Collect loose object paths and inflate + SHA-1 verify them IN PARALLEL across cores!
    let mut loose_paths: Vec<(Oid, PathBuf)> = Vec::new();
    let objects_dir = common_dir.join("objects");
    if objects_dir.is_dir() {
        for fan in fs::read_dir(&objects_dir)? {
            let fan = fan?;
            let fan_name = fan.file_name();
            let fan_str = fan_name.to_string_lossy();
            if fan_str.len() != 2 || !fan_str.bytes().all(|b| b.is_ascii_hexdigit()) {
                continue;
            }
            if !fan.file_type()?.is_dir() {
                continue;
            }
            for obj in fs::read_dir(fan.path())? {
                let obj = obj?;
                let rest = obj.file_name();
                let rest_str = rest.to_string_lossy();
                if rest_str.len() != 38 {
                    continue;
                }
                let full_hex = format!("{fan_str}{rest_str}");
                if let Some(oid) = Oid::parse_hex(full_hex.as_bytes()) {
                    loose_paths.push((oid, obj.path()));
                }
            }
        }
    }

    // Inflate loose objects in parallel!
    let mut extra_objects: Vec<(Oid, ObjectKind, Vec<u8>)> = Vec::new();
    if !loose_paths.is_empty() {
        loose_count = loose_paths.len() as u32;
        let chunk_sz = loose_paths.len().div_ceil(workers);
        let chunks: Vec<&[(Oid, PathBuf)]> = loose_paths.chunks(chunk_sz).collect();
        let mut results: Vec<io::Result<Vec<(Oid, ObjectKind, Vec<u8>)>>> =
            Vec::with_capacity(chunks.len());

        thread::scope(|s| {
            let mut handles = Vec::with_capacity(chunks.len());
            for chunk in chunks {
                handles.push(s.spawn(move || -> io::Result<Vec<(Oid, ObjectKind, Vec<u8>)>> {
                    let erased = Erased::new(Global);
                    let w_alloc = erased.handle();
                    let mut whole = OwnVec::new_in(w_alloc);
                    let mut out = Vec::with_capacity(chunk.len());
                    for (oid, path) in chunk {
                        let compressed = fs::read(path)?;
                        let (kind, body_start) = git_store::loose(
                            &compressed,
                            oid,
                            PackLimits::SHIPPED.most_object,
                            w_alloc,
                            &mut whole,
                        )
                        .map_err(|(_, why)| {
                            io::Error::new(
                                io::ErrorKind::InvalidData,
                                format!("loose {oid}: {why:?}"),
                            )
                        })?;
                        out.push((*oid, kind, whole[body_start..].to_vec()));
                    }
                    Ok(out)
                }));
            }
            for h in handles {
                results.push(h.join().expect("loose inflate worker"));
            }
        });

        for res in results {
            extra_objects.extend(res?);
        }
    }

    // 3. Ingest HEAD (worktree-local `git_dir`), packed-refs, and loose refs (`common_dir`).
    let head_path = git_dir.join("HEAD");
    if head_path.is_file() {
        let head_bytes = fs::read(&head_path)?;
        builder
            .feed_head(&head_bytes)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("HEAD: {e}")))?;
    }
    let packed_refs_path = common_dir.join("packed-refs");
    if packed_refs_path.is_file() {
        let pr_bytes = fs::read(&packed_refs_path)?;
        builder
            .feed_packed_refs(&pr_bytes)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("packed-refs: {e}")))?;
    }
    let refs_dir = common_dir.join("refs");
    if refs_dir.is_dir() {
        walk_loose_refs(&refs_dir, "refs", &mut builder)?;
    }
    if git_dir != common_dir {
        let wt_refs_dir = git_dir.join("refs");
        if wt_refs_dir.is_dir() {
            walk_loose_refs(&wt_refs_dir, "refs", &mut builder)?;
        }
    }

    // Merge packed objects (in builder.store()) and parallel-inflated loose objects into one
    // deterministically sorted unique object table, AND populate a MemStore for commit-graph +
    // tag peeling.
    let mut packed_oids = OwnVec::new_in(alloc);
    builder
        .store()
        .list_oids(&mut packed_oids)
        .map_err(|e| io::Error::other(format!("{e}")))?;
    let mut obj_buf = OwnVec::new_in(alloc);
    let mut all_objects: Vec<(Oid, ObjectKind, Vec<u8>)> =
        Vec::with_capacity(packed_oids.len() + extra_objects.len());
    for oid in &packed_oids {
        if let Some(kind) = builder
            .store()
            .read_object(oid, &mut obj_buf)
            .map_err(|e| io::Error::other(format!("{e}")))?
        {
            all_objects.push((*oid, kind, obj_buf.as_slice().to_vec()));
        }
    }
    all_objects.extend(extra_objects);
    all_objects.sort_by(|a, b| a.0.cmp(&b.0));
    all_objects.dedup_by(|a, b| a.0 == b.0);

    // 4. Parallel Zstd compression + BGOB record encoding across all cores (byte-for-byte
    // deterministic because chunks are concatenated in sorted OID order!).
    let mut slab_bytes = OwnVec::new_in(alloc);
    write_slab_header(&mut slab_bytes);
    let mut index_entries: AVec<'a, SlabIndexEntry> = OwnVec::with_capacity_in(all_objects.len(), alloc);
    let mut raw_object_bytes = 0u64;
    let mut stored_slab_bytes = 0u64;

    if !all_objects.is_empty() {
        let chunk_sz = all_objects.len().div_ceil(workers);
        let chunks: Vec<&[(Oid, ObjectKind, Vec<u8>)]> = all_objects.chunks(chunk_sz).collect();

        struct ChunkOut {
            bytes: Vec<u8>,
            entries: Vec<(Oid, u64, u32)>,
            raw_sum: u64,
            stored_sum: u64,
        }

        let mut chunk_results: Vec<Result<ChunkOut, GitError>> = Vec::with_capacity(chunks.len());
        thread::scope(|s| {
            let mut handles = Vec::with_capacity(chunks.len());
            for chunk in chunks {
                handles.push(s.spawn(move || -> Result<ChunkOut, GitError> {
                    let erased = Erased::new(Global);
                    let w_alloc = erased.handle();
                    let mut buf = OwnVec::new_in(w_alloc);
                    let mut entries = Vec::with_capacity(chunk.len());
                    let mut raw_sum = 0u64;
                    let mut stored_sum = 0u64;
                    for (oid, kind, body) in chunk {
                        let rel_off = buf.len() as u64;
                        let rec = encode_slab_record_with_oid(
                            *oid,
                            *kind,
                            body,
                            compression,
                            w_alloc,
                            &mut buf,
                        )?;
                        raw_sum = raw_sum.saturating_add(u64::from(rec.raw_len));
                        stored_sum = stored_sum.saturating_add(u64::from(rec.stored_len));
                        entries.push((*oid, rel_off, rec.raw_len));
                    }
                    Ok(ChunkOut { bytes: buf.as_slice().to_vec(), entries, raw_sum, stored_sum })
                }));
            }
            for h in handles {
                chunk_results.push(h.join().expect("zstd slab worker"));
            }
        });

        for res in chunk_results {
            let c = res.map_err(|e| io::Error::other(format!("slab encode: {e}")))?;
            let base_off = slab_bytes.len() as u64;
            slab_bytes.extend_from_slice(&c.bytes);
            raw_object_bytes = raw_object_bytes.saturating_add(c.raw_sum);
            stored_slab_bytes = stored_slab_bytes.saturating_add(c.stored_sum);
            for (oid, rel_off, raw_len) in c.entries {
                index_entries.push(SlabIndexEntry {
                    oid,
                    slab_id: 0,
                    offset: base_off + rel_off,
                    raw_len,
                });
            }
        }
    }

    let mut index_bytes = OwnVec::new_in(alloc);
    let unique_objects_out = encode_slab_index(&index_entries, alloc, &mut index_bytes)
        .map_err(|e| io::Error::other(format!("{e}")))?;

    // Build CGPH commit-graph directly from commits in `all_objects` (no extra store copy!).
    let mut cg_commits = OwnVec::new_in(alloc);
    for (oid, kind, body) in &all_objects {
        if *kind != ObjectKind::Commit {
            continue;
        }
        let parsed = git_store::Commit::parse(body).map_err(|(at, why)| {
            io::Error::new(io::ErrorKind::InvalidData, format!("commit {oid} @{at}: {why:?}"))
        })?;
        let mut parents = OwnVec::new_in(alloc);
        for p in parsed.parents() {
            parents.push(p);
        }
        cg_commits.push(blaze_git::slab::GraphCommitInput {
            oid: *oid,
            tree: parsed.tree,
            commit_time: u64::try_from(parsed.committed).unwrap_or(0),
            parents,
        });
    }
    let mut commit_graph_bytes = OwnVec::new_in(alloc);
    let commits_in_graph =
        blaze_git::slab::encode_commit_graph(&cg_commits, alloc, &mut commit_graph_bytes)
            .map_err(|e| io::Error::other(format!("{e}")))?;

    // Write slab + index + graph first, then open read-only view to peel any annotated tags into BGRF!
    let bgit_dir = repo_root.join(".bgit");
    fs::create_dir_all(bgit_dir.join("objects"))?;
    write_file_atomic(&bgit_dir.join("VERSION"), b"BGIT0001\n")?;
    let slab_path = bgit_dir.join("objects").join("slab-0000.bgob");
    let idx_path = bgit_dir.join("objects").join("index.bgix");
    write_file_atomic(&slab_path, &slab_bytes)?;
    write_file_atomic(&idx_path, &index_bytes)?;
    write_file_atomic(&bgit_dir.join("graph.bggp"), &commit_graph_bytes)?;

    let mapped_slab = map_readonly(&slab_path)?;
    let mapped_idx = map_readonly(&idx_path)?;
    let idx_proof = SlabIndex::parse_header_fast(mapped_idx.bytes(), 4_000_000)
        .map_err(|e| io::Error::other(format!("{e}")))?
        .proof();
    let slabs_slice = [(0u16, mapped_slab)];
    let view = MappedSlabView { slabs: &slabs_slice, index: Some((&mapped_idx, idx_proof)), alloc };

    let head = builder
        .store()
        .read_head(alloc)
        .map_err(|e| io::Error::other(format!("{e}")))?;
    let mut refs = OwnVec::new_in(alloc);
    builder
        .store()
        .list_refs("", alloc, &mut refs)
        .map_err(|e| io::Error::other(format!("{e}")))?;
    for r in refs.iter_mut() {
        if r.peeled.is_none()
            && r.name.as_str().starts_with("refs/tags/")
            && let Some(oid) = match &r.target {
                RefTarget::Direct(o) => Some(*o),
                RefTarget::Symbolic(_) => builder.store().resolve_ref(r.name.as_str()).ok().flatten(),
            }
        {
            r.peeled = blaze_git::storage::peel_tag(&view, oid, 8, alloc).ok().flatten();
        }
    }
    let refs_out = refs.len() as u32;
    let mut reftable_bytes = OwnVec::new_in(alloc);
    encode_reftable(head.as_ref(), &refs, alloc, &mut reftable_bytes)
        .map_err(|e| io::Error::other(format!("{e}")))?;
    write_file_atomic(&bgit_dir.join("refs.bgref"), &reftable_bytes)?;

    // Ensure `.bgit/` is excluded in `.git/info/exclude` (local-only, never touches `.gitignore`)
    // so `git status` and shell prompts stay clean after infecting a `.git` repository.
    ensure_bgit_excluded_in_git(&git_dir)?;
    // Install lightweight `.git/hooks/reference-transaction` hook so plain `git merge`/`git pull`
    // automatically updates tracking worktrees and deletes merged worktrees (no-op if no worktrees).
    let _ = crate::worktree_driver::install_git_worktree_hooks(repo_root);

    Ok(InfectStats {
        loose_objects_in: loose_count,
        packfiles_in,
        packed_objects_in,
        unique_objects_out,
        commits_in_graph,
        refs_out,
        raw_object_bytes,
        stored_slab_bytes,
    })
}

/// Fast O(delta) incremental refresh of an existing `.bgit` store from `.git` when `.git` refs or
/// objects have advanced (e.g. after running plain `git commit`, `git fetch`, or `git branch`).
///
/// Unlike [`infect_repo_on_disk`], this skips all historical packfiles and loose-object fanout
/// directories older than `.bgit/refs.bgref` (with a 2s mtime safety window), appends only newly
/// created objects as a tiny delta slab, and rewrites `.bgit/refs.bgref` in `BGRF` v2 format in
/// milliseconds even on 11+ GiB monorepos!
pub(crate) fn refresh_bgit_incremental_on_disk<'a>(
    repo_root: &Path,
    alloc: Alloc<'a>,
) -> io::Result<()> {
    let git_dir = locate_git_dir(repo_root)?;
    let common_dir = resolve_common_git_dir(repo_root).unwrap_or_else(|| git_dir.clone());
    let bgit_dir = repo_root.join(".bgit");
    let bgref_path = bgit_dir.join("refs.bgref");
    let cutoff = fs::metadata(&bgref_path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.checked_sub(std::time::Duration::from_secs(2)));

    let mut store = BgitDiskStore::open(repo_root, alloc)?;
    let mut added_objects = false;

    // 1. Ingest only newly written packfiles (`mtime >= cutoff`).
    let pack_dir = common_dir.join("objects").join("pack");
    if pack_dir.is_dir() {
        let local_pack_limits = PackLimits {
            most_objects: 16_000_000,
            most_object: 1 << 30,
            most_total: 64 << 30,
            most_depth: 4096,
            most_cache: 128 << 20,
        };
        for entry in fs::read_dir(&pack_dir)? {
            let entry = entry?;
            let p = entry.path();
            if p.extension().is_none_or(|e| e != "pack") {
                continue;
            }
            if let Some(cut) = cutoff
                && let Ok(mt) = entry.metadata().and_then(|m| m.modified())
                && mt < cut
            {
                continue;
            }
            let mapped = map_readonly(&p)?;
            let mut tmp = InfectBuilder::new(local_pack_limits, alloc);
            tmp.feed_packfile(mapped.bytes())
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("{}: {e}", p.display())))?;
            let mut oids = OwnVec::new_in(alloc);
            tmp.store()
                .list_oids(&mut oids)
                .map_err(|e| io::Error::other(format!("{e}")))?;
            let mut buf = OwnVec::new_in(alloc);
            for oid in &oids {
                if store.has_object(oid) {
                    continue;
                }
                if let Some(kind) = tmp
                    .store()
                    .read_object(oid, &mut buf)
                    .map_err(|e| io::Error::other(format!("{e}")))?
                {
                    let _ = store
                        .write_object(kind, &buf)
                        .map_err(|e| io::Error::other(format!("{e}")))?;
                    added_objects = true;
                }
            }
        }
    }

    // 2. Content-addressed exact delta over loose objects: `readdir` all 256 fanout dirs and
    //    only inflate objects missing from `index.bgix` (`!store.has_object(&oid)`). Never relies
    //    on parent-dir mtimes (which `git` does not bump when freshening an existing loose object!).
    let objects_dir = common_dir.join("objects");
    if objects_dir.is_dir() {
        let mut whole = OwnVec::new_in(alloc);
        for fan in fs::read_dir(&objects_dir)? {
            let fan = fan?;
            let fan_name = fan.file_name();
            let fan_str = fan_name.to_string_lossy();
            if fan_str.len() != 2 || !fan_str.bytes().all(|b| b.is_ascii_hexdigit()) {
                continue;
            }
            if !fan.file_type().is_ok_and(|t| t.is_dir()) {
                continue;
            }
            for obj in fs::read_dir(fan.path())? {
                let obj = obj?;
                let rest = obj.file_name();
                let rest_str = rest.to_string_lossy();
                if rest_str.len() != 38 {
                    continue;
                }
                let full_hex = format!("{fan_str}{rest_str}");
                let Some(oid) = Oid::parse_hex(full_hex.as_bytes()) else {
                    continue;
                };
                if store.has_object(&oid) {
                    continue;
                }
                let compressed = fs::read(obj.path())?;
                whole.clear();
                let (kind, body_start) = git_store::loose(
                    &compressed,
                    &oid,
                    PackLimits::SHIPPED.most_object,
                    alloc,
                    &mut whole,
                )
                .map_err(|(_, why)| {
                    io::Error::new(io::ErrorKind::InvalidData, format!("loose {oid}: {why:?}"))
                })?;
                let _ = store
                    .write_object(kind, &whole[body_start..])
                    .map_err(|e| io::Error::other(format!("{e}")))?;
                added_objects = true;
            }
        }
    }

    if added_objects {
        store.flush()?;
    }

    // 3. Re-read HEAD, packed-refs, and loose refs -> write fresh `BGRF` v2 `refs.bgref`!
    let mut ref_builder = InfectBuilder::new(PackLimits::SHIPPED, alloc);
    let head_path = git_dir.join("HEAD");
    if head_path.is_file() {
        let head_bytes = fs::read(&head_path)?;
        ref_builder
            .feed_head(&head_bytes)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("HEAD: {e}")))?;
    }
    let packed_refs_path = common_dir.join("packed-refs");
    if packed_refs_path.is_file() {
        let pr_bytes = fs::read(&packed_refs_path)?;
        ref_builder
            .feed_packed_refs(&pr_bytes)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("packed-refs: {e}")))?;
    }
    let refs_dir = common_dir.join("refs");
    if refs_dir.is_dir() {
        walk_loose_refs(&refs_dir, "refs", &mut ref_builder)?;
    }
    if git_dir != common_dir {
        let wt_refs_dir = git_dir.join("refs");
        if wt_refs_dir.is_dir() {
            walk_loose_refs(&wt_refs_dir, "refs", &mut ref_builder)?;
        }
    }

    let head = ref_builder
        .store()
        .read_head(alloc)
        .map_err(|e| io::Error::other(format!("{e}")))?;
    let mut refs = OwnVec::new_in(alloc);
    ref_builder
        .store()
        .list_refs("", alloc, &mut refs)
        .map_err(|e| io::Error::other(format!("{e}")))?;
    for r in refs.iter_mut() {
        if r.peeled.is_none()
            && r.name.as_str().starts_with("refs/tags/")
            && let Some(oid) = match &r.target {
                RefTarget::Direct(o) => Some(*o),
                RefTarget::Symbolic(_) => {
                    ref_builder.store().resolve_ref(r.name.as_str()).ok().flatten()
                }
            }
        {
            r.peeled = blaze_git::storage::peel_tag(&store, oid, 8, alloc).ok().flatten();
        }
    }

    let mut reftable_bytes = OwnVec::new_in(alloc);
    encode_reftable(head.as_ref(), &refs, alloc, &mut reftable_bytes)
        .map_err(|e| io::Error::other(format!("{e}")))?;
    write_file_atomic(&bgref_path, &reftable_bytes)?;
    Ok(())
}

fn ensure_bgit_excluded_in_git(git_dir: &Path) -> io::Result<()> {
    let info_dir = git_dir.join("info");
    let exclude_path = info_dir.join("exclude");
    let existing = fs::read_to_string(&exclude_path).unwrap_or_default();
    let already = existing
        .lines()
        .map(str::trim)
        .any(|l| l == ".bgit" || l == ".bgit/" || l == "/.bgit" || l == "/.bgit/");
    if already {
        return Ok(());
    }
    fs::create_dir_all(&info_dir)?;
    let mut updated = existing;
    if !updated.is_empty() && !updated.ends_with('\n') {
        updated.push('\n');
    }
    updated.push_str(".bgit/\n");
    write_file_atomic(&exclude_path, updated.as_bytes())
}

fn walk_loose_refs<'a>(dir: &Path, prefix: &str, builder: &mut InfectBuilder<'a>) -> io::Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        let full_ref = format!("{prefix}/{name_str}");
        let ft = entry.file_type()?;
        if ft.is_dir() {
            walk_loose_refs(&entry.path(), &full_ref, builder)?;
        } else if ft.is_file() {
            let bytes = fs::read(entry.path())?;
            builder
                .feed_loose_ref(&full_ref, &bytes)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("{full_ref}: {e}")))?;
        }
    }
    Ok(())
}

/// Exports a `.bgit` store cleanly back into a standard Git directory (`pack` + `idx` + `HEAD` +
/// `packed-refs` + `commit-graph`), encoding the PACK v2 stream **in parallel across all cores**!
pub(crate) fn disinfect_repo_on_disk<'a>(
    repo_root: &Path,
    out_git_dir: &Path,
    alloc: Alloc<'a>,
) -> io::Result<()> {
    let store = BgitDiskStore::open(repo_root, alloc)?;
    let mut oids = OwnVec::new_in(alloc);
    store
        .list_oids(&mut oids)
        .map_err(|e| io::Error::other(format!("{e}")))?;

    let mut pack_bytes = OwnVec::new_in(alloc);
    let written = store
        .write_pack_parallel(&oids, PackWriteOpts::FAST, &mut pack_bytes)
        .map_err(|e| io::Error::other(format!("parallel pack: {e}")))?;
    let mut idx_bytes = OwnVec::new_in(alloc);
    write_pack_index_v2(&pack_bytes, &written, alloc, &mut idx_bytes)
        .map_err(|e| io::Error::other(format!("pack idx: {e}")))?;

    let mut head_bytes = String::new();
    match store.read_head(alloc).map_err(|e| io::Error::other(format!("{e}")))? {
        Some(RefTarget::Symbolic(sym)) => {
            head_bytes.push_str("ref: ");
            head_bytes.push_str(sym.as_str());
            head_bytes.push('\n');
        }
        Some(RefTarget::Direct(oid)) => {
            let hex = oid.hex();
            head_bytes.push_str(core::str::from_utf8(&hex).expect("ascii"));
            head_bytes.push('\n');
        }
        None => head_bytes.push_str("ref: refs/heads/main\n"),
    }

    let mut refs = OwnVec::new_in(alloc);
    store
        .list_refs("", alloc, &mut refs)
        .map_err(|e| io::Error::other(format!("{e}")))?;
    let mut packed_refs_bytes = String::from("# pack-refs with: peeled fully-peeled sorted \n");
    for r in &refs {
        let oid = match &r.target {
            RefTarget::Direct(o) => Some(*o),
            RefTarget::Symbolic(_) => store.resolve_ref(r.name.as_str()).ok().flatten(),
        };
        if let Some(oid) = oid {
            let hex = oid.hex();
            packed_refs_bytes.push_str(core::str::from_utf8(&hex).expect("ascii"));
            packed_refs_bytes.push(' ');
            packed_refs_bytes.push_str(r.name.as_str());
            packed_refs_bytes.push('\n');
            if let Some(peeled) = r.peeled {
                let phex = peeled.hex();
                packed_refs_bytes.push('^');
                packed_refs_bytes.push_str(core::str::from_utf8(&phex).expect("ascii"));
                packed_refs_bytes.push('\n');
            }
        }
    }

    fs::create_dir_all(out_git_dir.join("objects").join("pack"))?;
    fs::create_dir_all(out_git_dir.join("objects").join("info"))?;
    fs::create_dir_all(out_git_dir.join("refs").join("heads"))?;
    fs::create_dir_all(out_git_dir.join("refs").join("tags"))?;

    let pack_sha = Oid(pack_bytes[pack_bytes.len() - 20..].try_into().expect("20B"));
    let pack_hex = pack_sha.hex();
    let hex_str = core::str::from_utf8(&pack_hex).expect("ascii");

    write_file_atomic(
        &out_git_dir.join("objects").join("pack").join(format!("pack-{hex_str}.pack")),
        &pack_bytes,
    )?;
    write_file_atomic(
        &out_git_dir.join("objects").join("pack").join(format!("pack-{hex_str}.idx")),
        &idx_bytes,
    )?;
    write_file_atomic(&out_git_dir.join("HEAD"), head_bytes.as_bytes())?;
    write_file_atomic(&out_git_dir.join("packed-refs"), packed_refs_bytes.as_bytes())?;
    if let Some(cg) = store.graph.as_ref()
        && !cg.bytes().is_empty()
    {
        write_file_atomic(&out_git_dir.join("objects").join("info").join("commit-graph"), cg.bytes())?;
    }
    let config = b"[core]\n\trepositoryformatversion = 0\n\tfilemode = true\n\tbare = true\n";
    write_file_atomic(&out_git_dir.join("config"), config)?;
    Ok(())
}

/// Checks out `tree_oid` from `store` into `worktree_dir`.
pub(crate) fn materialize_tree<'a, S: ObjectStore<'a> + ?Sized>(
    store: &S,
    tree_oid: Oid,
    worktree_dir: &Path,
    alloc: Alloc<'a>,
) -> io::Result<u32> {
    let mut stack: Vec<(Oid, PathBuf)> = vec![(tree_oid, worktree_dir.to_path_buf())];
    let mut tree_buf = OwnVec::new_in(alloc);
    let mut blob_buf = OwnVec::new_in(alloc);
    let mut files_written = 0u32;

    while let Some((oid, dir)) = stack.pop() {
        fs::create_dir_all(&dir)?;
        let kind = store
            .read_object(&oid, &mut tree_buf)
            .map_err(|e| io::Error::other(format!("{e}")))?;
        if kind != Some(ObjectKind::Tree) {
            return Err(io::Error::new(io::ErrorKind::InvalidData, format!("{oid} is not a tree")));
        }
        let owned = tree_buf.as_slice().to_vec();
        let mut iter = TreeIter::new(oid, &owned);
        while let Some(e) = iter
            .next_entry()
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, format!("{err}")))?
        {
            let name = core::str::from_utf8(e.name)
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "non-utf8 path"))?;
            if name.is_empty()
                || name == "."
                || name == ".."
                || name == ".git"
                || name == ".bgit"
                || name.contains('/')
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("unsafe tree name {name}"),
                ));
            }
            let target = dir.join(name);
            if e.is_dir() {
                stack.push((e.oid, target));
            } else if (e.mode & 0o170000) == 0o100000 {
                let bkind = store
                    .read_object(&e.oid, &mut blob_buf)
                    .map_err(|err| io::Error::other(format!("{err}")))?;
                if bkind != Some(ObjectKind::Blob) {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("{} not blob", e.oid),
                    ));
                }
                fs::write(&target, blob_buf.as_slice())?;
                #[cfg(unix)]
                if e.mode & 0o111 != 0 {
                    use std::os::unix::fs::PermissionsExt as _;
                    fs::set_permissions(&target, fs::Permissions::from_mode(0o755))?;
                }
                files_written += 1;
            }
        }
    }
    Ok(files_written)
}

/// Ensures `repo_root` has an initialized `.bgit` store: opens `.bgit` if present (O(1) mmap),
/// otherwise automatically infects `.git` in-place across all cores!
pub(crate) fn open_or_infect<'a>(repo_root: &Path, alloc: Alloc<'a>) -> io::Result<BgitDiskStore<'a>> {
    if bgit_dir_for(repo_root).join("VERSION").is_file() {
        return BgitDiskStore::open(repo_root, alloc);
    }
    if locate_git_dir(repo_root).is_ok() {
        let _ = infect_repo_on_disk(repo_root, SlabCompression::AutoZstd, alloc)?;
        return BgitDiskStore::open(repo_root, alloc);
    }
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        format!("no .bgit or .git repository at {}", repo_root.display()),
    ))
}
