//! Sans-IO Git PACK v2 writer (with `OFS_DELTA` compression), `.idx` v2 generator, and
//! thin-pack-capable unpacker into any [`ObjectStore`].
//!
//! 100% wire-compatible with `git index-pack`, `git unpack-objects`, `git-upload-pack`, and
//! `git-receive-pack`, using `blaze-deflate` (zlib + CRC-32) and `blaze-git-fetch::sha1` with
//! zero `std` and zero third-party crates.

#![blaze::sans_io]

use blaze_deflate::{Crc32, Level as DeflateLevel, ZlibEncoder};
use blaze_git_fetch::{
    Kind as ObjectKind, Oid, PackLimits,
    sha1::{Sha1, sha1},
    store::{self, Base},
};
use blaze_hash::{BuildKeyHasher, FixedState, Map, MapOpts};
use blaze_own::{
    AVec, Alloc, Vec,
    planted::{NoPlant, Planted},
};

use crate::{ErrorKind, GitError, GitFile, plant::GitEnginePlant, storage::ObjectStore};

/// Options controlling [`write_pack_from_store`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct PackWriteOpts {
    /// Emit `OFS_DELTA` entries when a smaller delta exists against a recent same-kind base.
    pub use_ofs_delta: bool,
    /// Zlib compression effort (`Level::Fast` for low latency on macOS/LAN).
    pub deflate_level: DeflateLevel,
    /// Maximum objects allowed in one pack.
    pub most_objects: u32,
}

impl PackWriteOpts {
    pub const FAST: PackWriteOpts = PackWriteOpts {
        use_ofs_delta: true,
        deflate_level: DeflateLevel::Fast,
        most_objects: 2_000_000,
    };
}

/// Metadata for one entry written into a PACK v2 stream (used to generate `.idx` v2).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct WrittenPackEntry {
    pub oid: Oid,
    pub offset: u64,
    pub crc32: u32,
}

fn kind_to_pack_num(kind: ObjectKind) -> u8 {
    match kind {
        ObjectKind::Commit => 1,
        ObjectKind::Tree => 2,
        ObjectKind::Blob => 3,
        ObjectKind::Tag => 4,
    }
}

/// Encodes Git's variable-length pack entry header: type (3 bits) + uncompressed size.
pub fn write_pack_entry_header<'a>(type_num: u8, mut size: u64, out: &mut AVec<'a, u8>) {
    let mut byte = ((type_num & 0x07) << 4) | ((size & 0x0f) as u8);
    size >>= 4;
    while size > 0 {
        out.push(byte | 0x80);
        byte = (size & 0x7f) as u8;
        size >>= 7;
    }
    out.push(byte);
}

/// Encodes Git's `OFS_DELTA` negative offset encoding (`((back + 1) << 7) | low` in reverse).
pub fn write_ofs_back<'a>(mut back: u64, out: &mut AVec<'a, u8>) {
    let mut buf = [0u8; 10];
    let mut pos = buf.len() - 1;
    buf[pos] = (back & 0x7f) as u8;
    back >>= 7;
    while back > 0 {
        back -= 1;
        pos -= 1;
        buf[pos] = 0x80 | ((back & 0x7f) as u8);
        back >>= 7;
    }
    out.extend_from_slice(&buf[pos..]);
}

fn write_delta_varint<'a>(mut n: usize, out: &mut AVec<'a, u8>) {
    loop {
        let mut b = (n & 0x7f) as u8;
        n >>= 7;
        if n > 0 {
            b |= 0x80;
        }
        out.push(b);
        if n == 0 {
            break;
        }
    }
}

/// Encodes a standard Git binary delta transforming `base` into `target`.
///
/// Uses a fast 16-byte block hash index over `base` so encoding runs in O(|base| + |target|)
/// time with zero recursion. Returns `None` if the resulting delta is not smaller than `target`.
pub fn encode_delta<'a>(
    base: &[u8],
    target: &[u8],
    alloc: Alloc<'a>,
    out: &mut AVec<'a, u8>,
) -> bool {
    const BLOCK: usize = 16;
    if base.len() < BLOCK || target.len() < BLOCK || base.len() > (16 << 20) || target.len() > (16 << 20)
    {
        return false;
    }
    out.clear();
    write_delta_varint(base.len(), out);
    write_delta_varint(target.len(), out);

    // Index non-overlapping 16-byte chunks of `base` by a fast 64-bit fingerprint.
    let mut index: Map<u64, u32, FixedState, Alloc<'a>> = Map::new(MapOpts {
        hasher: FixedState,
        alloc,
        capacity: Some((base.len() / BLOCK).min(65_536)),
    });
    let mut bpos = 0usize;
    while bpos + BLOCK <= base.len() {
        let h = hash16(&base[bpos..bpos + BLOCK]);
        if !index.contains_key(&h) {
            index.insert(h, bpos as u32);
        }
        bpos += BLOCK;
    }

    let mut pending_lit_start = 0usize;
    let mut pending_lit_len = 0usize;
    let mut tpos = 0usize;

    while tpos < target.len() {
        if out.len() + pending_lit_len >= target.len() {
            return false;
        }
        if tpos + BLOCK <= target.len() {
            let h = hash16(&target[tpos..tpos + BLOCK]);
            if let Some(&b_off_u32) = index.get(&h) {
                let mut b_off = b_off_u32 as usize;
                if &base[b_off..b_off + BLOCK] == &target[tpos..tpos + BLOCK] {
                    // Extend match backwards into pending literals!
                    let mut back = 0usize;
                    while back < pending_lit_len
                        && b_off > 0
                        && base[b_off - 1] == target[tpos - 1]
                    {
                        b_off -= 1;
                        tpos -= 1;
                        back += 1;
                    }
                    pending_lit_len -= back;
                    flush_insert(target, pending_lit_start, pending_lit_len, out);
                    pending_lit_len = 0;

                    // Extend match forwards up to 0x10000 bytes (single copy instruction).
                    let mut mlen = BLOCK + back;
                    while mlen < 0x10000
                        && b_off + mlen < base.len()
                        && tpos + mlen < target.len()
                        && base[b_off + mlen] == target[tpos + mlen]
                    {
                        mlen += 1;
                    }
                    emit_copy(b_off as u32, mlen as u32, out);
                    tpos += mlen;
                    pending_lit_start = tpos;
                    continue;
                }
            }
        }
        if pending_lit_len == 0 {
            pending_lit_start = tpos;
        }
        pending_lit_len += 1;
        tpos += 1;
    }
    flush_insert(target, pending_lit_start, pending_lit_len, out);
    out.len() + 8 < target.len()
}

#[inline]
fn hash16(b: &[u8]) -> u64 {
    let lo = u64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]]);
    let hi = u64::from_le_bytes([b[8], b[9], b[10], b[11], b[12], b[13], b[14], b[15]]);
    lo.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ hi.rotate_left(29).wrapping_mul(0xBF58_476D_1CE4_E5B9)
}

fn flush_insert<'a>(target: &[u8], mut start: usize, mut len: usize, out: &mut AVec<'a, u8>) {
    while len > 0 {
        let take = len.min(127);
        out.push(take as u8);
        out.extend_from_slice(&target[start..start + take]);
        start += take;
        len -= take;
    }
}

fn emit_copy<'a>(offset: u32, size: u32, out: &mut AVec<'a, u8>) {
    let mut cmd: u8 = 0x80;
    let mut extra = [0u8; 7];
    let mut elen = 0usize;

    if offset & 0x0000_00ff != 0 {
        cmd |= 0x01;
        extra[elen] = (offset & 0xff) as u8;
        elen += 1;
    }
    if offset & 0x0000_ff00 != 0 {
        cmd |= 0x02;
        extra[elen] = ((offset >> 8) & 0xff) as u8;
        elen += 1;
    }
    if offset & 0x00ff_0000 != 0 {
        cmd |= 0x04;
        extra[elen] = ((offset >> 16) & 0xff) as u8;
        elen += 1;
    }
    if offset & 0xff00_0000 != 0 {
        cmd |= 0x08;
        extra[elen] = ((offset >> 24) & 0xff) as u8;
        elen += 1;
    }

    let enc_size = if size == 0x10000 { 0 } else { size };
    if enc_size & 0x0000_00ff != 0 {
        cmd |= 0x10;
        extra[elen] = (enc_size & 0xff) as u8;
        elen += 1;
    }
    if enc_size & 0x0000_ff00 != 0 {
        cmd |= 0x20;
        extra[elen] = ((enc_size >> 8) & 0xff) as u8;
        elen += 1;
    }
    if enc_size & 0x00ff_0000 != 0 {
        cmd |= 0x40;
        extra[elen] = ((enc_size >> 16) & 0xff) as u8;
        elen += 1;
    }

    out.push(cmd);
    out.extend_from_slice(&extra[..elen]);
}

/// One chunk of encoded PACK v2 entries produced independently on a worker thread (offsets are
/// chunk-relative and rebased by [`assemble_pack_from_chunks`]).
pub struct EncodedPackChunk<'a> {
    pub bytes: AVec<'a, u8>,
    pub entries: AVec<'a, WrittenPackEntry>,
}

/// Encodes an ordered slice of unique `oids` into a self-contained PACK v2 chunk (with intra-
/// chunk `OFS_DELTA` and `blaze-deflate` zlib compression) so a multi-core driver can encode
/// disjoint OID slices concurrently across all cores over a read-only mmap store and stitch them
/// deterministically via [`assemble_pack_from_chunks`].
pub fn encode_pack_chunk<'a, S: ObjectStore<'a> + ?Sized>(
    store: &S,
    oids: &[Oid],
    opts: PackWriteOpts,
    alloc: Alloc<'a>,
) -> Result<EncodedPackChunk<'a>, GitError> {
    let mut out = Vec::new_in(alloc);
    let mut entries: AVec<'a, WrittenPackEntry> = Vec::with_capacity_in(oids.len(), alloc);
    let mut body = Vec::new_in(alloc);
    let mut delta_buf = Vec::new_in(alloc);

    struct LastBase<'a> {
        offset: u64,
        bytes: AVec<'a, u8>,
    }
    let mut last_blob: Option<LastBase<'a>> = None;
    let mut last_tree: Option<LastBase<'a>> = None;

    for &oid in oids {
        let Some(kind) = store.read_object(&oid, &mut body)? else {
            return Err(GitError {
                file: GitFile::PackWrite,
                at: out.len() as u64,
                kind: ErrorKind::ObjectMissing(oid),
            });
        };
        let entry_start = out.len();

        let mut wrote_delta = false;
        if opts.use_ofs_delta && body.len() >= 64 {
            let cand = match kind {
                ObjectKind::Blob => last_blob.as_ref(),
                ObjectKind::Tree => last_tree.as_ref(),
                _ => None,
            };
            if let Some(base) = cand
                && encode_delta(&base.bytes, &body, alloc, &mut delta_buf)
            {
                // Chunk-relative backward distance equals final pack backward distance!
                let back = (entry_start as u64) - base.offset;
                write_pack_entry_header(6, delta_buf.len() as u64, &mut out);
                write_ofs_back(back, &mut out);
                zlib_append(&delta_buf, opts.deflate_level, alloc, &mut out)?;
                wrote_delta = true;
            }
        }

        if !wrote_delta {
            write_pack_entry_header(kind_to_pack_num(kind), body.len() as u64, &mut out);
            zlib_append(&body, opts.deflate_level, alloc, &mut out)?;
            if opts.use_ofs_delta && body.len() <= (4 << 20) {
                let slot = match kind {
                    ObjectKind::Blob => Some(&mut last_blob),
                    ObjectKind::Tree => Some(&mut last_tree),
                    _ => None,
                };
                if let Some(slot) = slot {
                    let mut saved = Vec::with_capacity_in(body.len(), alloc);
                    saved.extend_from_slice(&body);
                    *slot = Some(LastBase { offset: entry_start as u64, bytes: saved });
                }
            }
        }

        let mut crc = Crc32::new();
        crc.update(&out[entry_start..]);
        entries.push(WrittenPackEntry { oid, offset: entry_start as u64, crc32: crc.finish() });
    }

    Ok(EncodedPackChunk { bytes: out, entries })
}

/// Assembles pre-encoded [`EncodedPackChunk`]s (produced in parallel across worker threads) into
/// one valid Git PACK v2 stream with trailing SHA-1 checksum and rebased `.idx` entry offsets.
pub fn assemble_pack_from_chunks<'a>(
    chunks: &[(&[u8], &[WrittenPackEntry])],
    most_objects: u32,
    alloc: Alloc<'a>,
    out: &mut AVec<'a, u8>,
) -> Result<AVec<'a, WrittenPackEntry>, GitError> {
    out.clear();
    let total_entries: usize = chunks.iter().map(|(_, e)| e.len()).sum();
    let count = u32::try_from(total_entries).map_err(|_| GitError {
        file: GitFile::PackWrite,
        at: 0,
        kind: ErrorKind::BudgetExceeded,
    })?;
    if count > most_objects {
        return Err(GitError {
            file: GitFile::PackWrite,
            at: 0,
            kind: ErrorKind::TooManyEntries { count, most: most_objects },
        });
    }
    let total_bytes: usize = 12 + chunks.iter().map(|(b, _)| b.len()).sum::<usize>() + 20;
    out.reserve(total_bytes);

    out.extend_from_slice(b"PACK");
    out.extend_from_slice(&2u32.to_be_bytes());
    out.extend_from_slice(&count.to_be_bytes());

    let mut entries = Vec::with_capacity_in(total_entries, alloc);
    for &(chunk_bytes, chunk_entries) in chunks {
        let base_off = out.len() as u64;
        out.extend_from_slice(chunk_bytes);
        for e in chunk_entries {
            entries.push(WrittenPackEntry {
                oid: e.oid,
                offset: base_off + e.offset,
                crc32: e.crc32,
            });
        }
    }
    let trailer = sha1(out);
    out.extend_from_slice(&trailer);
    Ok(entries)
}

/// Writes a valid Git PACK v2 stream containing `oids` read from `store` into `out` (cleared
/// first), returning the per-entry `(oid, offset, crc32)` table for optional `.idx` v2 generation.
pub fn write_pack_from_store<'a, S: ObjectStore<'a> + ?Sized>(
    store: &S,
    oids: &[Oid],
    opts: PackWriteOpts,
    alloc: Alloc<'a>,
    out: &mut AVec<'a, u8>,
) -> Result<AVec<'a, WrittenPackEntry>, GitError> {
    write_pack_from_store_planted(store, oids, opts, alloc, out, NoPlant)
}

pub fn write_pack_from_store_planted<'a, S: ObjectStore<'a> + ?Sized, P: Planted<GitEnginePlant>>(
    store: &S,
    oids: &[Oid],
    opts: PackWriteOpts,
    alloc: Alloc<'a>,
    out: &mut AVec<'a, u8>,
    plant: P,
) -> Result<AVec<'a, WrittenPackEntry>, GitError> {
    out.clear();
    // Deduplicate OIDs preserving order.
    let mut seen: Map<Oid, (), BuildKeyHasher, Alloc<'a>> = Map::new(MapOpts {
        hasher: BuildKeyHasher::default(),
        alloc,
        capacity: Some(oids.len()),
    });
    let mut uniq: AVec<'a, Oid> = Vec::with_capacity_in(oids.len(), alloc);
    for &oid in oids {
        if !seen.contains_key(&oid) {
            seen.insert(oid, ());
            uniq.push(oid);
        }
    }
    let count = u32::try_from(uniq.len()).map_err(|_| GitError {
        file: GitFile::PackWrite,
        at: 0,
        kind: ErrorKind::BudgetExceeded,
    })?;
    if count > opts.most_objects {
        return Err(GitError {
            file: GitFile::PackWrite,
            at: 0,
            kind: ErrorKind::TooManyEntries { count, most: opts.most_objects },
        });
    }

    out.extend_from_slice(b"PACK");
    out.extend_from_slice(&2u32.to_be_bytes());
    out.extend_from_slice(&count.to_be_bytes());

    let mut entries: AVec<'a, WrittenPackEntry> = Vec::with_capacity_in(uniq.len(), alloc);
    let mut body = Vec::new_in(alloc);
    let mut delta_buf = Vec::new_in(alloc);

    // Keep the most recent whole object per kind as a quick OFS_DELTA candidate.
    struct LastBase<'a> {
        offset: u64,
        bytes: AVec<'a, u8>,
    }
    let mut last_blob: Option<LastBase<'a>> = None;
    let mut last_tree: Option<LastBase<'a>> = None;

    for oid in uniq {
        let Some(kind) = store.read_object(&oid, &mut body)? else {
            return Err(GitError {
                file: GitFile::PackWrite,
                at: out.len() as u64,
                kind: ErrorKind::ObjectMissing(oid),
            });
        };
        let entry_start = out.len();

        let mut wrote_delta = false;
        if opts.use_ofs_delta && body.len() >= 64 {
            let cand = match kind {
                ObjectKind::Blob => last_blob.as_ref(),
                ObjectKind::Tree => last_tree.as_ref(),
                _ => None,
            };
            if let Some(base) = cand
                && encode_delta(&base.bytes, &body, alloc, &mut delta_buf)
            {
                let back = (entry_start as u64) - base.offset;
                write_pack_entry_header(6, delta_buf.len() as u64, out);
                write_ofs_back(back, out);
                zlib_append(&delta_buf, opts.deflate_level, alloc, out)?;
                wrote_delta = true;
            }
        }

        if !wrote_delta {
            write_pack_entry_header(kind_to_pack_num(kind), body.len() as u64, out);
            zlib_append(&body, opts.deflate_level, alloc, out)?;
            if opts.use_ofs_delta && body.len() <= (4 << 20) {
                let slot = match kind {
                    ObjectKind::Blob => Some(&mut last_blob),
                    ObjectKind::Tree => Some(&mut last_tree),
                    _ => None,
                };
                if let Some(slot) = slot {
                    let mut saved = Vec::with_capacity_in(body.len(), alloc);
                    saved.extend_from_slice(&body);
                    *slot = Some(LastBase { offset: entry_start as u64, bytes: saved });
                }
            }
        }

        let mut crc = Crc32::new();
        crc.update(&out[entry_start..]);
        entries.push(WrittenPackEntry { oid, offset: entry_start as u64, crc32: crc.finish() });
    }

    let mut trailer = sha1(out);
    if plant.has(GitEnginePlant::CorruptPackTrailer) {
        trailer[0] ^= 0xff;
    }
    out.extend_from_slice(&trailer);
    Ok(entries)
}

fn zlib_append<'a>(
    input: &[u8],
    level: DeflateLevel,
    alloc: Alloc<'a>,
    out: &mut AVec<'a, u8>,
) -> Result<(), GitError> {
    let mut enc = ZlibEncoder::new_in(level, alloc);
    enc.write(input, out).map_err(|_| GitError {
        file: GitFile::PackWrite,
        at: out.len() as u64,
        kind: ErrorKind::Compression,
    })?;
    enc.finish(out).map_err(|_| GitError {
        file: GitFile::PackWrite,
        at: out.len() as u64,
        kind: ErrorKind::Compression,
    })
}

/// Encodes one Git loose object (`"<kind> <len>\0<body>"`, zlib-compressed) into `out` and
/// returns its exact SHA-1 [`Oid`].
pub fn encode_loose_object<'a>(
    kind: ObjectKind,
    body: &[u8],
    alloc: Alloc<'a>,
    out: &mut AVec<'a, u8>,
) -> Result<Oid, GitError> {
    let kind_str = match kind {
        ObjectKind::Commit => b"commit".as_slice(),
        ObjectKind::Tree => b"tree".as_slice(),
        ObjectKind::Blob => b"blob".as_slice(),
        ObjectKind::Tag => b"tag".as_slice(),
    };
    let mut len_buf = [0u8; 24];
    let len_slice = format_usize_dec(body.len(), &mut len_buf);
    let mut raw = Vec::with_capacity_in(kind_str.len() + 1 + len_slice.len() + 1 + body.len(), alloc);
    raw.extend_from_slice(kind_str);
    raw.push(b' ');
    raw.extend_from_slice(len_slice);
    raw.push(0);
    raw.extend_from_slice(body);
    let oid = Oid(sha1(&raw));
    out.clear();
    zlib_append(&raw, DeflateLevel::Fast, alloc, out)?;
    Ok(oid)
}

fn format_usize_dec(mut n: usize, buf: &mut [u8; 24]) -> &[u8] {
    let mut i = buf.len();
    if n == 0 {
        i -= 1;
        buf[i] = b'0';
        return &buf[i..];
    }
    while n > 0 {
        i -= 1;
        buf[i] = b'0' + (n % 10) as u8;
        n /= 10;
    }
    &buf[i..]
}

/// Generates a standard Git `.idx` version 2 pack index for `pack_bytes` and `entries`
/// (produced by [`write_pack_from_store`]).
pub fn write_pack_index_v2<'a>(
    pack_bytes: &[u8],
    entries: &[WrittenPackEntry],
    alloc: Alloc<'a>,
    out: &mut AVec<'a, u8>,
) -> Result<(), GitError> {
    if pack_bytes.len() < 32 {
        return Err(GitError { file: GitFile::PackWrite, at: 0, kind: ErrorKind::Malformed });
    }
    let pack_sha = &pack_bytes[pack_bytes.len() - 20..];

    let mut sorted: AVec<'a, WrittenPackEntry> = Vec::with_capacity_in(entries.len(), alloc);
    sorted.extend_from_slice(entries);
    sorted.sort_by(|a, b| a.oid.cmp(&b.oid));

    out.clear();
    out.extend_from_slice(&[0xff, b't', b'O', b'c']);
    out.extend_from_slice(&2u32.to_be_bytes());

    // 256-entry fanout.
    let mut running = 0u32;
    let mut idx = 0usize;
    for byte in 0..256usize {
        while idx < sorted.len() && usize::from(sorted[idx].oid.0[0]) <= byte {
            running += 1;
            idx += 1;
        }
        out.extend_from_slice(&running.to_be_bytes());
    }
    // Sorted OIDs.
    for e in &sorted {
        out.extend_from_slice(&e.oid.0);
    }
    // CRC-32 per entry.
    for e in &sorted {
        out.extend_from_slice(&e.crc32.to_be_bytes());
    }
    // 4-byte offsets + optional 8-byte large offsets.
    let mut large_offsets: AVec<'a, u64> = Vec::new_in(alloc);
    for e in &sorted {
        if e.offset < 0x8000_0000 {
            out.extend_from_slice(&(e.offset as u32).to_be_bytes());
        } else {
            let lidx = large_offsets.len() as u32;
            large_offsets.push(e.offset);
            out.extend_from_slice(&(0x8000_0000 | lidx).to_be_bytes());
        }
    }
    for loff in large_offsets {
        out.extend_from_slice(&loff.to_be_bytes());
    }
    out.extend_from_slice(pack_sha);
    let idx_sha = sha1(out);
    out.extend_from_slice(&idx_sha);
    Ok(())
}

/// Statistics returned by [`unpack_pack_into_store`].
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct UnpackStats {
    pub objects_unpacked: u32,
    pub deltas_resolved: u32,
    pub thin_bases_used: u32,
}

/// Unpacks a Git PACK v2/v3 stream directly into any [`ObjectStore`], supporting BOTH self-
/// contained packs AND thin packs (whose `REF_DELTA` bases already exist in `store`).
///
/// Verifies the trailing PACK SHA-1 checksum and computes/verifies every unpacked object's SHA-1.
pub fn unpack_pack_into_store<'a, S: ObjectStore<'a> + ?Sized>(
    pack: &[u8],
    store: &mut S,
    limits: PackLimits,
    alloc: Alloc<'a>,
) -> Result<UnpackStats, GitError> {
    unpack_pack_into_store_planted(pack, store, limits, alloc, NoPlant)
}

pub fn unpack_pack_into_store_planted<'a, S: ObjectStore<'a> + ?Sized, P: Planted<GitEnginePlant>>(
    pack: &[u8],
    store: &mut S,
    limits: PackLimits,
    alloc: Alloc<'a>,
    plant: P,
) -> Result<UnpackStats, GitError> {
    if pack.len() < 32 || &pack[0..4] != b"PACK" {
        return Err(GitError { file: GitFile::PackWrite, at: 0, kind: ErrorKind::Malformed });
    }
    let version = u32::from_be_bytes([pack[4], pack[5], pack[6], pack[7]]);
    if version != 2 && version != 3 {
        return Err(GitError {
            file: GitFile::PackWrite,
            at: 4,
            kind: ErrorKind::UnsupportedVersion(version),
        });
    }
    let count = u32::from_be_bytes([pack[8], pack[9], pack[10], pack[11]]);
    if count > limits.most_objects {
        return Err(GitError {
            file: GitFile::PackWrite,
            at: 8,
            kind: ErrorKind::TooManyEntries { count, most: limits.most_objects },
        });
    }

    // Verify trailing 20-byte SHA-1 checksum over pack[..len-20].
    let body_end = pack.len() - 20;
    let mut h = Sha1::new();
    h.update(&pack[..body_end]);
    if h.finish() != pack[body_end..] {
        return Err(GitError {
            file: GitFile::PackWrite,
            at: body_end as u64,
            kind: ErrorKind::ChecksumMismatch,
        });
    }

    struct RawEntry<'a> {
        offset: u64,
        base: Base,
        inflated: AVec<'a, u8>,
        resolved: Option<(ObjectKind, Oid)>,
    }

    let mut entries: AVec<'a, RawEntry<'a>> = Vec::with_capacity_in(count as usize, alloc);
    let mut by_offset: Map<u64, usize, FixedState, Alloc<'a>> =
        Map::new(MapOpts { hasher: FixedState, alloc, capacity: Some(count as usize) });

    let mut pos = 12usize;
    let mut total_bytes = 0u64;
    let mut stats = UnpackStats::default();

    for i in 0..count as usize {
        if pos >= body_end {
            return Err(GitError {
                file: GitFile::PackWrite,
                at: pos as u64,
                kind: ErrorKind::Malformed,
            });
        }
        let entry_offset = pos as u64;
        let window = &pack[pos..body_end];
        let head = store::entry_head(window).map_err(|why| GitError {
            file: GitFile::PackWrite,
            at: entry_offset,
            kind: ErrorKind::ObjectStore(why),
        })?;
        if head.size > limits.most_object {
            return Err(GitError {
                file: GitFile::PackWrite,
                at: entry_offset,
                kind: ErrorKind::BudgetExceeded,
            });
        }
        let mut inflated = Vec::new_in(alloc);
        let zlimits = blaze_deflate::Limits {
            most_out: head.size,
            most_ratio: blaze_deflate::MAX_RATIO,
            most_header: 0,
        };
        let consumed =
            blaze_deflate::unzlib_prefix(&window[head.data..], zlimits, alloc, &mut inflated)
                .map_err(|_| GitError {
                    file: GitFile::PackWrite,
                    at: entry_offset + head.data as u64,
                    kind: ErrorKind::Compression,
                })?;
        if inflated.len() as u64 != head.size {
            return Err(GitError {
                file: GitFile::PackWrite,
                at: entry_offset,
                kind: ErrorKind::Malformed,
            });
        }
        pos += head.data + consumed;

        let resolved = match head.base {
            Base::Whole(kind) => {
                total_bytes = total_bytes.saturating_add(inflated.len() as u64);
                if total_bytes > limits.most_total {
                    return Err(GitError {
                        file: GitFile::PackWrite,
                        at: entry_offset,
                        kind: ErrorKind::BudgetExceeded,
                    });
                }
                let oid = store.write_object(kind, &inflated)?;
                if !plant.has(GitEnginePlant::SkipPackOidVerify) && Oid::of(kind, &inflated) != oid {
                    return Err(GitError {
                        file: GitFile::PackWrite,
                        at: entry_offset,
                        kind: ErrorKind::ChecksumMismatch,
                    });
                }
                stats.objects_unpacked += 1;
                // Free whole object bytes now that it is stored!
                inflated.clear();
                Some((kind, oid))
            }
            Base::Ofs { .. } | Base::Ref { .. } => None,
        };
        by_offset.insert(entry_offset, i);
        entries.push(RawEntry { offset: entry_offset, base: head.base, inflated, resolved });
    }

    if pos != body_end {
        return Err(GitError {
            file: GitFile::PackWrite,
            at: pos as u64,
            kind: ErrorKind::Malformed,
        });
    }

    // Resolve all OFS_DELTA and REF_DELTA entries iteratively in rounds (bounded by most_depth).
    let mut base_buf = Vec::new_in(alloc);
    let mut result_buf = Vec::new_in(alloc);
    let mut unresolved = entries.iter().filter(|e| e.resolved.is_none()).count();
    let mut round = 0u16;

    while unresolved > 0 {
        round += 1;
        if round > limits.most_depth {
            return Err(GitError {
                file: GitFile::PackWrite,
                at: 0,
                kind: ErrorKind::DepthExceeded { most: limits.most_depth },
            });
        }
        let mut progressed = 0usize;
        for i in 0..entries.len() {
            if entries[i].resolved.is_some() {
                continue;
            }
            let base_oid_opt: Option<(Oid, bool)> = match entries[i].base {
                Base::Whole(_) => continue,
                Base::Ofs { back } => {
                    let Some(target_off) = entries[i].offset.checked_sub(back) else {
                        return Err(GitError {
                            file: GitFile::PackWrite,
                            at: entries[i].offset,
                            kind: ErrorKind::Malformed,
                        });
                    };
                    let Some(&base_idx) = by_offset.get(&target_off) else {
                        return Err(GitError {
                            file: GitFile::PackWrite,
                            at: entries[i].offset,
                            kind: ErrorKind::Malformed,
                        });
                    };
                    entries[base_idx].resolved.map(|(_, oid)| (oid, false))
                }
                Base::Ref { base } => {
                    if store.has_object(&base) {
                        Some((base, true))
                    } else {
                        None
                    }
                }
            };

            let Some((base_oid, is_thin_ref)) = base_oid_opt else {
                continue;
            };
            let Some(kind) = store.read_object(&base_oid, &mut base_buf)? else {
                return Err(GitError {
                    file: GitFile::PackWrite,
                    at: entries[i].offset,
                    kind: ErrorKind::ObjectMissing(base_oid),
                });
            };
            store::apply_delta(&base_buf, &entries[i].inflated, limits.most_object, &mut result_buf)
                .map_err(|why| GitError {
                    file: GitFile::PackWrite,
                    at: entries[i].offset,
                    kind: ErrorKind::ObjectStore(why),
                })?;
            total_bytes = total_bytes.saturating_add(result_buf.len() as u64);
            if total_bytes > limits.most_total {
                return Err(GitError {
                    file: GitFile::PackWrite,
                    at: entries[i].offset,
                    kind: ErrorKind::BudgetExceeded,
                });
            }
            let oid = store.write_object(kind, &result_buf)?;
            entries[i].inflated.clear();
            entries[i].resolved = Some((kind, oid));
            stats.objects_unpacked += 1;
            stats.deltas_resolved += 1;
            if is_thin_ref {
                stats.thin_bases_used += 1;
            }
            unresolved -= 1;
            progressed += 1;
        }
        if progressed == 0 {
            return Err(GitError {
                file: GitFile::PackWrite,
                at: 0,
                kind: ErrorKind::Malformed,
            });
        }
    }

    Ok(stats)
}
