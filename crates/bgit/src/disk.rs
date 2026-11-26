//! Native disk driver for `.bgit` (`BGOB` slabs, `BGIX` fanout index, `BGRF` single-file binary
//! reftable, `CGPH` commit-graph) and `infect`/`disinfect` bridge for legacy `.git` repositories.

use std::{
    fs,
    io::{self, Write as _},
    path::{Path, PathBuf},
};

use blaze_git::{
    ErrorKind, GitError, GitFile,
    commit_graph::CommitGraph,
    infect::{InfectBuilder, InfectStats, build_bgit_image_from_store, disinfect_from_store},
    objects::TreeIter,
    pack_write::PackWriteOpts,
    refs::parse_gitdir_file,
    slab::{
        SlabCompression, SlabIndex, SlabIndexEntry, SlabPayload, SlabReader, SlabWriter,
        VerifiedIndex, build_commit_graph_from_store, decode_reftable, encode_reftable,
        encode_slab_index,
    },
    storage::{
        MemStore, ObjectStore, RefEntry, RefStore, RefTarget, RefUpdate, RefUpdateStatus,
    },
};
use blaze_git_fetch::{Kind as ObjectKind, Oid, PackLimits};
use blaze_own::{AVec, Alloc, String as OwnString, Vec as OwnVec};

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

/// Native on-disk `.bgit` repository store implementing [`ObjectStore`] and [`RefStore`].
///
/// Immutable slabs (`objects/slab-NNNN.bgob`), index (`objects/index.bgix`), and commit-graph
/// (`graph.bggp`) are held in contiguous buffers for zero-copy `SlabReader` lookups; any newly
/// written objects or ref updates stage in `overlay` and flush atomically via [`Self::flush`].
pub(crate) struct BgitDiskStore<'a> {
    pub(crate) bgit_dir: PathBuf,
    alloc: Alloc<'a>,
    slabs: Vec<(u16, Vec<u8>)>,
    index: Option<(Vec<u8>, VerifiedIndex)>,
    graph_bytes: Option<Vec<u8>>,
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
            graph_bytes: None,
            overlay,
            dirty_objects: true,
            dirty_refs: true,
            compression: SlabCompression::AutoZstd,
        };
        store.flush()?;
        Ok(store)
    }

    /// Opens an existing `.bgit` repository on disk.
    pub(crate) fn open(repo_root: &Path, alloc: Alloc<'a>) -> io::Result<BgitDiskStore<'a>> {
        let bgit_dir = bgit_dir_for(repo_root);
        let ver_path = bgit_dir.join("VERSION");
        if !ver_path.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("no .bgit store at {} (run `bgit infect` first)", bgit_dir.display()),
            ));
        }

        let mut slabs: Vec<(u16, Vec<u8>)> = Vec::new();
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
                let bytes = fs::read(&path)?;
                let _ = SlabReader::parse(&bytes)
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("{e}")))?;
                slabs.push((id, bytes));
            }
        }

        let idx_path = obj_dir.join("index.bgix");
        let index = if idx_path.is_file() {
            let b = fs::read(&idx_path)?;
            let proof = SlabIndex::parse(&b, 4_000_000)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("{e}")))?
                .proof();
            Some((b, proof))
        } else {
            None
        };
        let cg_path = bgit_dir.join("graph.bggp");
        let graph_bytes = if cg_path.is_file() {
            let b = fs::read(&cg_path)?;
            if b.is_empty() { None } else { Some(b) }
        } else {
            None
        };

        let mut overlay = MemStore::new(alloc);
        let rt_path = bgit_dir.join("refs.bgref");
        if rt_path.is_file() {
            let rt_bytes = fs::read(&rt_path)?;
            let decoded = decode_reftable(&rt_bytes, 1_000_000, alloc)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("{e}")))?;
            if let Some(h) = decoded.head {
                overlay
                    .write_head(&h)
                    .map_err(|e| io::Error::other(format!("{e}")))?;
            }
            for r in decoded.refs {
                overlay
                    .set_ref(r.name.as_str(), r.target)
                    .map_err(|e| io::Error::other(format!("{e}")))?;
            }
        }

        Ok(BgitDiskStore {
            bgit_dir,
            alloc,
            slabs,
            index,
            graph_bytes,
            overlay,
            dirty_objects: false,
            dirty_refs: false,
            compression: SlabCompression::AutoZstd,
        })
    }

    #[inline]
    fn verified_index(&self) -> Option<SlabIndex<'_>> {
        let (bytes, proof) = self.index.as_ref()?;
        SlabIndex::from_verified(bytes, *proof).ok()
    }

    /// Borrows the parsed `CGPH` commit-graph if present.
    pub(crate) fn commit_graph(&self) -> Option<CommitGraph<'_, 'a>> {
        let bytes = self.graph_bytes.as_deref()?;
        CommitGraph::from_layers(&[bytes], self.alloc).ok()
    }

    /// Number of indexed objects across slabs plus overlay.
    pub(crate) fn object_count(&self) -> usize {
        let slab_n = self.verified_index().map_or(0, |i| i.len() as usize);
        slab_n + self.overlay.object_count()
    }

    fn find_slab_bytes(&self, slab_id: u16) -> Option<&[u8]> {
        self.slabs.iter().find(|(id, _)| *id == slab_id).map(|(_, b)| b.as_slice())
    }

    /// Flushes any staged objects, ref updates, and rebuilt index/commit-graph atomically to disk.
    pub(crate) fn flush(&mut self) -> io::Result<()> {
        if self.dirty_objects {
            let mut new_oids = OwnVec::new_in(self.alloc);
            self.overlay
                .list_oids(&mut new_oids)
                .map_err(|e| io::Error::other(format!("{e}")))?;

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
                        writer
                            .append_with_oid(*oid, kind, &buf)
                            .map_err(|e| io::Error::other(format!("{e}")))?;
                    }
                }
                let (slab_bytes, _entries) = writer.into_parts();
                let slab_path = self
                    .bgit_dir
                    .join("objects")
                    .join(format!("slab-{next_id:04}.bgob"));
                write_file_atomic(&slab_path, &slab_bytes)?;
                self.slabs.push((next_id, slab_bytes.as_slice().to_vec()));
            }

            // Rebuild merged BGIX index across all slabs.
            let mut all_entries: AVec<'a, SlabIndexEntry> = OwnVec::new_in(self.alloc);
            for (id, bytes) in &self.slabs {
                let reader = SlabReader::parse(bytes)
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("{e}")))?;
                reader
                    .scan_headers(*id, &mut all_entries)
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("{e}")))?;
            }
            let mut idx_out = OwnVec::new_in(self.alloc);
            encode_slab_index(&all_entries, self.alloc, &mut idx_out)
                .map_err(|e| io::Error::other(format!("{e}")))?;
            write_file_atomic(&self.bgit_dir.join("objects").join("index.bgix"), &idx_out)?;
            let idx_vec = idx_out.as_slice().to_vec();
            let proof = SlabIndex::parse(&idx_vec, 4_000_000)
                .map_err(|e| io::Error::other(format!("{e}")))?
                .proof();
            self.index = Some((idx_vec, proof));

            // Rebuild CGPH commit-graph.
            let mut cg_out = OwnVec::new_in(self.alloc);
            let _ = build_commit_graph_from_store(self, self.alloc, &mut cg_out)
                .map_err(|e| io::Error::other(format!("{e}")))?;
            write_file_atomic(&self.bgit_dir.join("graph.bggp"), &cg_out)?;
            self.graph_bytes = if cg_out.is_empty() { None } else { Some(cg_out.as_slice().to_vec()) };

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
            write_file_atomic(&self.bgit_dir.join("refs.bgref"), &rt_out)?;
            self.dirty_refs = false;
        }
        Ok(())
    }

    /// Resolves a revision string: 40-hex OID, `"HEAD"`, full refname, or short branch/tag name.
    pub(crate) fn resolve_rev(&self, rev: &str) -> Result<Option<Oid>, GitError> {
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
        self.resolve_ref(&remote)
    }
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
        // Deduplicate in-place.
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
        self.overlay.read_head(alloc)
    }

    fn write_head(&mut self, target: &RefTarget<'_>) -> Result<(), GitError> {
        self.dirty_refs = true;
        self.overlay.write_head(target)
    }

    fn read_ref(&self, name: &str, alloc: Alloc<'a>) -> Result<Option<RefTarget<'a>>, GitError> {
        self.overlay.read_ref(name, alloc)
    }

    fn resolve_ref(&self, name: &str) -> Result<Option<Oid>, GitError> {
        self.overlay.resolve_ref(name)
    }

    fn list_refs(
        &self,
        prefix: &str,
        alloc: Alloc<'a>,
        out: &mut AVec<'a, RefEntry<'a>>,
    ) -> Result<(), GitError> {
        // Use self (including slabs) for peeling annotated tags!
        self.overlay.list_refs(prefix, alloc, out)?;
        for entry in out.iter_mut() {
            if entry.peeled.is_none()
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
        let res = self.overlay.apply_ref_updates(updates, atomic, alloc)?;
        if res.iter().any(|s| *s == RefUpdateStatus::Ok) {
            self.dirty_refs = true;
        }
        Ok(res)
    }
}

/// Converts an existing `.git` repository at `repo_root` into a native `.bgit` store on disk!
pub(crate) fn infect_repo_on_disk<'a>(
    repo_root: &Path,
    compression: SlabCompression,
    alloc: Alloc<'a>,
) -> io::Result<InfectStats> {
    let git_dir = locate_git_dir(repo_root)?;
    let mut builder = InfectBuilder::new(PackLimits::SHIPPED, alloc);

    // 1. Ingest all packfiles first.
    let pack_dir = git_dir.join("objects").join("pack");
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
            let bytes = fs::read(&pack_path)?;
            builder
                .feed_packfile(&bytes)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("{}: {e}", pack_path.display())))?;
        }
    }

    // 2. Ingest all loose objects (`objects/<aa>/<bb...>`).
    let objects_dir = git_dir.join("objects");
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
                let Some(oid) = Oid::parse_hex(full_hex.as_bytes()) else {
                    continue;
                };
                let compressed = fs::read(obj.path())?;
                builder
                    .feed_loose_object(oid, &compressed)
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("{full_hex}: {e}")))?;
            }
        }
    }

    // 3. Ingest HEAD, packed-refs, and loose refs (in exact Git precedence order!).
    let head_path = git_dir.join("HEAD");
    if head_path.is_file() {
        let head_bytes = fs::read(&head_path)?;
        builder
            .feed_head(&head_bytes)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("HEAD: {e}")))?;
    }
    let packed_refs_path = git_dir.join("packed-refs");
    if packed_refs_path.is_file() {
        let pr_bytes = fs::read(&packed_refs_path)?;
        builder
            .feed_packed_refs(&pr_bytes)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("packed-refs: {e}")))?;
    }
    let refs_dir = git_dir.join("refs");
    if refs_dir.is_dir() {
        walk_loose_refs(&refs_dir, "refs", &mut builder)?;
    }

    // 4. Compile into native `.bgit` image and write atomically!
    let image = builder
        .finish(compression)
        .map_err(|e| io::Error::other(format!("infect finish: {e}")))?;

    let bgit_dir = repo_root.join(".bgit");
    fs::create_dir_all(bgit_dir.join("objects"))?;
    write_file_atomic(&bgit_dir.join("VERSION"), b"BGIT0001\n")?;
    write_file_atomic(&bgit_dir.join("objects").join("slab-0000.bgob"), &image.slab_bytes)?;
    write_file_atomic(&bgit_dir.join("objects").join("index.bgix"), &image.index_bytes)?;
    write_file_atomic(&bgit_dir.join("refs.bgref"), &image.reftable_bytes)?;
    write_file_atomic(&bgit_dir.join("graph.bggp"), &image.commit_graph_bytes)?;

    Ok(image.stats)
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
/// `packed-refs` + `commit-graph`).
pub(crate) fn disinfect_repo_on_disk<'a>(
    repo_root: &Path,
    out_git_dir: &Path,
    alloc: Alloc<'a>,
) -> io::Result<()> {
    let store = BgitDiskStore::open(repo_root, alloc)?;
    let dis = disinfect_from_store(&store, PackWriteOpts::FAST, alloc)
        .map_err(|e| io::Error::other(format!("disinfect: {e}")))?;

    fs::create_dir_all(out_git_dir.join("objects").join("pack"))?;
    fs::create_dir_all(out_git_dir.join("objects").join("info"))?;
    fs::create_dir_all(out_git_dir.join("refs").join("heads"))?;
    fs::create_dir_all(out_git_dir.join("refs").join("tags"))?;

    let pack_sha = Oid(dis.pack_bytes[dis.pack_bytes.len() - 20..].try_into().expect("20B"));
    let pack_hex = pack_sha.hex();
    let hex_str = core::str::from_utf8(&pack_hex).expect("ascii");

    write_file_atomic(
        &out_git_dir.join("objects").join("pack").join(format!("pack-{hex_str}.pack")),
        &dis.pack_bytes,
    )?;
    write_file_atomic(
        &out_git_dir.join("objects").join("pack").join(format!("pack-{hex_str}.idx")),
        &dis.idx_bytes,
    )?;
    write_file_atomic(&out_git_dir.join("HEAD"), dis.head_bytes.as_str().as_bytes())?;
    write_file_atomic(&out_git_dir.join("packed-refs"), dis.packed_refs_bytes.as_str().as_bytes())?;
    if !dis.commit_graph_bytes.is_empty() {
        write_file_atomic(
            &out_git_dir.join("objects").join("info").join("commit-graph"),
            &dis.commit_graph_bytes,
        )?;
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
        // Copy tree bytes locally because `tree_buf` is reused across iterations.
        let owned = tree_buf.as_slice().to_vec();
        let mut iter = TreeIter::new(oid, &owned);
        while let Some(e) = iter
            .next_entry()
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, format!("{err}")))?
        {
            let name = core::str::from_utf8(e.name)
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "non-utf8 path"))?;
            if name.is_empty() || name == "." || name == ".." || name == ".git" || name == ".bgit" || name.contains('/') {
                return Err(io::Error::new(io::ErrorKind::InvalidData, format!("unsafe tree name {name}")));
            }
            let target = dir.join(name);
            if e.is_dir() {
                stack.push((e.oid, target));
            } else if (e.mode & 0o170000) == 0o100000 {
                let bkind = store
                    .read_object(&e.oid, &mut blob_buf)
                    .map_err(|err| io::Error::other(format!("{err}")))?;
                if bkind != Some(ObjectKind::Blob) {
                    return Err(io::Error::new(io::ErrorKind::InvalidData, format!("{} not blob", e.oid)));
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

/// Ensures `repo_root` has an initialized `.bgit` store: opens `.bgit` if present, otherwise
/// automatically infects `.git` in-place if present!
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

/// Helper to build a quick image directly from a [`BgitDiskStore`] for benchmarking/verification.
#[allow(dead_code)]
pub(crate) fn snapshot_stats<'a>(store: &BgitDiskStore<'a>, alloc: Alloc<'a>) -> io::Result<InfectStats> {
    let img = build_bgit_image_from_store(store, SlabCompression::AutoZstd, alloc)
        .map_err(|e| io::Error::other(format!("{e}")))?;
    Ok(img.stats)
}
