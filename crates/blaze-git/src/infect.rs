//! Sans-IO `infect` and `disinfect` conversion engine between legacy `.git/` repositories and
//! our native `.bgit/` format (`BGOB` + `BGIX` + `BGRF` + `CGPH`).
//!
//! - [`InfectBuilder`] ingests loose `.git/objects/ab/cdef...`, `.git/objects/pack/*.pack`,
//!   `.git/HEAD`, `.git/packed-refs`, and loose `.git/refs/**` from any driver, deduplicates and
//!   verifies every SHA-1 OID, and emits the complete `.bgit` image (`BGOB` slab + `BGIX` binary
//!   fanout index + `BGRF` single-file binary reftable with pre-peeled tags + `CGPH` v1 binary
//!   commit-graph).
//! - [`disinfect_from_store`] exports any [`RepoStore`] back into standard Git files (`pack.pack`,
//!   `pack.idx`, `HEAD`, `packed-refs`) with zero loss.

#![blaze::sans_io]

use blaze_git_fetch::{Oid, PackLimits, store as git_store};
use blaze_memchr::memchr;
use blaze_own::{AString, AVec, Alloc, String, Vec};

use crate::{
    ErrorKind, GitError, GitFile,
    pack_write::{PackWriteOpts, unpack_pack_into_store, write_pack_from_store, write_pack_index_v2},
    refs::{HeadFile, parse_head, parse_loose_ref, ref_name_safe},
    slab::{
        SlabCompression, SlabWriter, build_commit_graph_from_store, encode_reftable,
        encode_slab_index,
    },
    storage::{MemStore, ObjectStore, RefStore, RefTarget, RepoStore},
};

/// Summary metrics emitted by [`InfectBuilder::finish`].
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct InfectStats {
    pub loose_objects_in: u32,
    pub packfiles_in: u32,
    pub packed_objects_in: u32,
    pub unique_objects_out: u32,
    pub commits_in_graph: u32,
    pub refs_out: u32,
    pub raw_object_bytes: u64,
    pub stored_slab_bytes: u64,
}

/// Complete native `.bgit` repository image emitted by [`InfectBuilder::finish`].
pub struct InfectedImage<'a> {
    pub slab_bytes: AVec<'a, u8>,
    pub index_bytes: AVec<'a, u8>,
    pub reftable_bytes: AVec<'a, u8>,
    pub commit_graph_bytes: AVec<'a, u8>,
    pub stats: InfectStats,
}

/// Sans-IO builder that turns an existing legacy `.git` repository into a native `.bgit` image.
pub struct InfectBuilder<'a> {
    alloc: Alloc<'a>,
    store: MemStore<'a>,
    limits: PackLimits,
    stats: InfectStats,
}

impl<'a> InfectBuilder<'a> {
    pub fn new(limits: PackLimits, alloc: Alloc<'a>) -> InfectBuilder<'a> {
        InfectBuilder { alloc, store: MemStore::new(alloc), limits, stats: InfectStats::default() }
    }

    /// Ingests one loose `.git/objects/<aa>/<bb...>` file, verifying its zlib stream and SHA-1 OID.
    pub fn feed_loose_object(&mut self, oid: Oid, compressed_bytes: &[u8]) -> Result<(), GitError> {
        let mut whole = Vec::new_in(self.alloc);
        let (kind, body_start) =
            git_store::loose(compressed_bytes, &oid, self.limits.most_object, self.alloc, &mut whole)
                .map_err(|(at, why)| GitError {
                    file: GitFile::LooseObject(oid),
                    at,
                    kind: ErrorKind::ObjectStore(why),
                })?;
        self.store.write_object(kind, &whole[body_start..])?;
        self.stats.loose_objects_in += 1;
        Ok(())
    }

    /// Ingests an entire `.git/objects/pack/*.pack` file (including OFS/REF deltas), verifying
    /// pack checksum and every unpacked object's SHA-1.
    pub fn feed_packfile(&mut self, pack_bytes: &[u8]) -> Result<u32, GitError> {
        let unpack = unpack_pack_into_store(pack_bytes, &mut self.store, self.limits, self.alloc)?;
        self.stats.packfiles_in += 1;
        self.stats.packed_objects_in += unpack.objects_unpacked;
        Ok(unpack.objects_unpacked)
    }

    /// Ingests `.git/HEAD` bytes.
    pub fn feed_head(&mut self, head_bytes: &[u8]) -> Result<(), GitError> {
        match parse_head(head_bytes)? {
            HeadFile::Symbolic(sym) => {
                self.store.write_head(&RefTarget::Symbolic(String::from_str_in(sym, self.alloc)))
            }
            HeadFile::Detached(oid) => self.store.write_head(&RefTarget::Direct(oid)),
        }
    }

    /// Ingests `.git/packed-refs` bytes (should be fed BEFORE loose refs so loose refs override
    /// stale entries in `packed-refs`, matching Git's exact ref precedence!).
    pub fn feed_packed_refs(&mut self, packed_refs: &[u8]) -> Result<(), GitError> {
        let mut pos = 0usize;
        while pos < packed_refs.len() {
            let nl = memchr(b'\n', &packed_refs[pos..]).map_or(packed_refs.len(), |i| pos + i);
            let mut line = &packed_refs[pos..nl];
            if let Some(stripped) = line.strip_suffix(b"\r") {
                line = stripped;
            }
            pos = nl + 1;
            if line.is_empty() || line[0] == b'#' || line[0] == b'^' {
                continue;
            }
            if line.len() > 41 && line[40] == b' '
                && let Some(oid) = Oid::parse_hex(&line[..40])
                && let Ok(name) = core::str::from_utf8(&line[41..])
                && ref_name_safe(name)
            {
                self.store.set_ref(name, RefTarget::Direct(oid))?;
            }
        }
        Ok(())
    }

    /// Ingests one loose ref file (e.g. `refs/heads/main`), overriding any earlier `packed-refs`
    /// entry of the same name.
    pub fn feed_loose_ref(&mut self, ref_name: &str, bytes: &[u8]) -> Result<(), GitError> {
        let target = match parse_loose_ref(bytes)? {
            HeadFile::Symbolic(sym) => RefTarget::Symbolic(String::from_str_in(sym, self.alloc)),
            HeadFile::Detached(oid) => RefTarget::Direct(oid),
        };
        self.store.set_ref(ref_name, target)
    }

    /// Directly accesses the staged store (e.g. for verifying imported state before writing).
    pub fn store(&self) -> &MemStore<'a> {
        &self.store
    }

    /// Compiles all ingested objects and refs into the native `.bgit` image (`BGOB` + `BGIX` +
    /// `BGRF` + `CGPH`).
    pub fn finish(self, compression: SlabCompression) -> Result<InfectedImage<'a>, GitError> {
        let mut img = build_bgit_image_from_store(&self.store, compression, self.alloc)?;
        img.stats.loose_objects_in = self.stats.loose_objects_in;
        img.stats.packfiles_in = self.stats.packfiles_in;
        img.stats.packed_objects_in = self.stats.packed_objects_in;
        Ok(img)
    }
}

/// Builds a complete native `.bgit` image (`BGOB` + `BGIX` + `BGRF` + `CGPH`) directly from any
/// [`RepoStore`].
pub fn build_bgit_image_from_store<'a, S: RepoStore<'a> + ?Sized>(
    store: &S,
    compression: SlabCompression,
    alloc: Alloc<'a>,
) -> Result<InfectedImage<'a>, GitError> {
    let mut oids = Vec::new_in(alloc);
    store.list_oids(&mut oids)?;

    let mut slab = SlabWriter::new(0, compression, alloc);
    let mut obj_buf = Vec::new_in(alloc);
    for oid in &oids {
        if let Some(kind) = store.read_object(oid, &mut obj_buf)? {
            slab.append_with_oid(*oid, kind, &obj_buf)?;
        }
    }

    let raw_object_bytes = slab.raw_bytes_total();
    let stored_slab_bytes = slab.stored_bytes_total();
    let (slab_bytes, entries) = slab.into_parts();

    let mut index_bytes = Vec::new_in(alloc);
    let unique_objects_out = encode_slab_index(&entries, alloc, &mut index_bytes)?;

    let head = store.read_head(alloc)?;
    let mut refs = Vec::new_in(alloc);
    store.list_refs("", alloc, &mut refs)?;
    let refs_out = refs.len() as u32;
    let mut reftable_bytes = Vec::new_in(alloc);
    encode_reftable(head.as_ref(), &refs, alloc, &mut reftable_bytes)?;

    let mut commit_graph_bytes = Vec::new_in(alloc);
    let commits_in_graph = build_commit_graph_from_store(store, alloc, &mut commit_graph_bytes)?;

    Ok(InfectedImage {
        slab_bytes,
        index_bytes,
        reftable_bytes,
        commit_graph_bytes,
        stats: InfectStats {
            loose_objects_in: 0,
            packfiles_in: 0,
            packed_objects_in: 0,
            unique_objects_out,
            commits_in_graph,
            refs_out,
            raw_object_bytes,
            stored_slab_bytes,
        },
    })
}

/// Standard `.git/` files emitted by [`disinfect_from_store`].
pub struct DisinfectedGit<'a> {
    pub pack_bytes: AVec<'a, u8>,
    pub idx_bytes: AVec<'a, u8>,
    pub head_bytes: AString<'a>,
    pub packed_refs_bytes: AString<'a>,
    pub commit_graph_bytes: AVec<'a, u8>,
}

/// Exports any [`RepoStore`] (such as `.bgit`) cleanly back into standard `.git` files (`PACK` v2,
/// `.idx` v2, `HEAD`, `packed-refs`, and `commit-graph`).
pub fn disinfect_from_store<'a, S: RepoStore<'a> + ?Sized>(
    store: &S,
    pack_opts: PackWriteOpts,
    alloc: Alloc<'a>,
) -> Result<DisinfectedGit<'a>, GitError> {
    let mut oids = Vec::new_in(alloc);
    store.list_oids(&mut oids)?;

    let mut pack_bytes = Vec::new_in(alloc);
    let written = write_pack_from_store(store, &oids, pack_opts, alloc, &mut pack_bytes)?;
    let mut idx_bytes = Vec::new_in(alloc);
    write_pack_index_v2(&pack_bytes, &written, alloc, &mut idx_bytes)?;

    let mut head_bytes = String::new_in(alloc);
    match store.read_head(alloc)? {
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
        None => {
            head_bytes.push_str("ref: refs/heads/main\n");
        }
    }

    let mut refs = Vec::new_in(alloc);
    store.list_refs("", alloc, &mut refs)?;
    let mut packed_refs_bytes = String::new_in(alloc);
    packed_refs_bytes.push_str("# pack-refs with: peeled fully-peeled sorted \n");
    for r in &refs {
        let oid = match &r.target {
            RefTarget::Direct(o) => Some(*o),
            RefTarget::Symbolic(_) => store.resolve_ref(r.name.as_str())?,
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

    let mut commit_graph_bytes = Vec::new_in(alloc);
    let _ = build_commit_graph_from_store(store, alloc, &mut commit_graph_bytes)?;

    Ok(DisinfectedGit { pack_bytes, idx_bytes, head_bytes, packed_refs_bytes, commit_graph_bytes })
}
