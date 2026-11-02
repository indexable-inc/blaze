//! The ad-hoc code signature ld64 makes (`LC_CODE_SIGNATURE`, linker-signed): a blob of
//! blobs with one code directory, the SHA-256 of every 4 KiB page of the file before the
//! signature. macOS on arm64 runs no page without it, and checks every page it maps.
//!
//! The pages are hashed where they are, in the regions the core placed ([`View`]): nothing
//! assembles the file first. They are hashed in parallel, and an incremental link hashes only
//! the pages the core wrote since the last signature, and the headers' and `__LINKEDIT`'s
//! pages whose bytes differ from that signature's ([`Cache`]).

#![forbid(unsafe_code)]

use blaze_graph::par;
use blaze_ids::radix;
use smallvec::SmallVec;

use super::sha256;

const SUPER_BLOB: u32 = 0xfade_0cc0;
const CODE_DIRECTORY: u32 = 0xfade_0c02;
/// The code directory's version with the executable segment's fields.
const VERSION: u32 = 0x0002_0400;
const ADHOC: u32 = 0x2;
const LINKER_SIGNED: u32 = 0x2_0000;
const EXECSEG_MAIN_BINARY: u64 = 0x1;
const HASH_SHA256: u8 = 2;
const HASH: usize = 32;
const PAGE_SHIFT: u8 = 12;
pub(super) const PAGE: usize = 1 << PAGE_SHIFT;
/// The super blob's header and its one index entry.
const BLOB_HEADER: usize = 12 + 8;
/// The code directory's fields up to its identifier.
const DIRECTORY_HEADER: usize = 88;
/// Pages one parallel task hashes: 256 KiB, so a task's work outweighs its scheduling.
const GROUP: usize = 64;

/// How long the signature of a file of `limit` bytes before it is.
pub(super) fn len(identifier: &str, limit: u64) -> u64 {
    let pages = (limit as usize).div_ceil(PAGE);
    (BLOB_HEADER + DIRECTORY_HEADER + identifier.len() + 1 + pages * HASH).next_multiple_of(16)
        as u64
}

/// The file before the signature, as the byte ranges it is made of: zero between them.
pub(super) struct View<'a> {
    /// By offset, not overlapping, none empty, each ending at most at `len`: so every offset
    /// is a `usize`.
    parts: Vec<(u64, &'a [u8])>,
    len: usize,
    /// The chained fixups' slots, (address, value) by address, laid over the parts: each 8
    /// bytes at `address - base`, within `len`, so never across a page.
    slots: &'a [(u64, u64)],
    base: u64,
}

impl<'a> View<'a> {
    /// The file of `len` bytes holding `parts` at their offsets; an error naming the offsets
    /// when two overlap or one runs past `len`.
    pub(super) fn new(len: u64, mut parts: Vec<(u64, &'a [u8])>) -> Result<View<'a>, String> {
        parts.retain(|it| !it.1.is_empty());
        if !parts.is_sorted_by_key(|it| it.0) {
            radix::sort_copy_by_key(&mut parts, |it| it.0);
        }
        let mut end = 0;
        for &(at, bytes) in &parts {
            if at < end {
                // alloc: error path
                return Err(format!("the file's bytes at {at:#x} overlap those before {end:#x}"));
            }
            end = at + bytes.len() as u64;
        }
        let Some(len) = usize::try_from(len).ok().filter(|_| end <= len) else {
            // alloc: error path
            return Err(format!("the file's bytes run to {end:#x}, past its {len:#x} signed bytes"));
        };
        Ok(View { parts, len, slots: &[], base: 0 })
    }

    /// The view with `slots` (address, the 8 bytes the file holds there) laid over its parts,
    /// each at its address less `base`; an error naming the first not 8-aligned, out of
    /// order or past the file.
    pub(super) fn with_slots(self, slots: &'a [(u64, u64)], base: u64) -> Result<View<'a>, String> {
        let mut end = 0;
        for &(addr, _) in slots {
            let at = addr.wrapping_sub(base);
            if addr < base || at % 8 != 0 || at < end || at + 8 > self.len as u64 {
                // alloc: error path
                return Err(format!("a chained slot at {addr:#x} is not 8-aligned, in order and in the file"));
            }
            end = at + 8;
        }
        Ok(View { slots, base, ..self })
    }

    /// The file of `bytes`.
    #[cfg(test)]
    pub(super) fn whole(bytes: &'a [u8]) -> View<'a> {
        View { parts: vec![(0, bytes)], len: bytes.len(), slots: &[], base: 0 }
    }

    pub(super) fn len(&self) -> usize {
        self.len
    }

    /// Page `nth`: borrowed when one part holds all of it and no slot is in it, else gathered
    /// into `buf`.
    fn page<'b>(&'b self, nth: usize, buf: &'b mut [u8; PAGE]) -> &'b [u8] {
        let start = nth * PAGE;
        let end = (start + PAGE).min(self.len);
        let base = self.base;
        let slot = self.slots.partition_point(|it| ((it.0 - base) as usize) < start);
        let slots = &self.slots[slot..];
        let slots = &slots[..slots.partition_point(|it| ((it.0 - base) as usize) < end)];
        let first = self.parts.partition_point(|&(at, bytes)| at as usize + bytes.len() <= start);
        if let Some(&(at, bytes)) = self.parts.get(first)
            && at as usize <= start
            && at as usize + bytes.len() >= end
        {
            let at = at as usize;
            if slots.is_empty() {
                return &bytes[start - at..end - at];
            }
            let buf = &mut buf[..end - start];
            buf.copy_from_slice(&bytes[start - at..end - at]);
            for &(addr, value) in slots {
                let at = (addr - base) as usize - start;
                buf[at..at + 8].copy_from_slice(&value.to_le_bytes());
            }
            return buf;
        }
        let buf = &mut buf[..end - start];
        buf.fill(0);
        for &(at, bytes) in &self.parts[first..] {
            let at = at as usize;
            if at >= end {
                break;
            }
            let (from, to) = (at.max(start), (at + bytes.len()).min(end));
            buf[from - start..to - start].copy_from_slice(&bytes[from - at..to - at]);
        }
        for &(addr, value) in slots {
            let at = (addr - base) as usize - start;
            buf[at..at + 8].copy_from_slice(&value.to_le_bytes());
        }
        buf
    }
}

/// The last signature's hashes, and the pages of it the core doesn't track (the headers and
/// `__LINKEDIT`, which the writer makes): a page is hashed again when the core wrote it since
/// ([`Since`]) or, for a kept page, when its bytes differ. Exact (bytes compared, not
/// fingerprints), so a signature never depends on what is cached. 32 bytes per page of the
/// image, plus 4 KiB per kept page.
#[derive(Default)]
pub(super) struct Cache {
    /// Each kept page as last signed, `PAGE` bytes, in the order of `kept`.
    pages: Vec<u8>,
    /// The kept pages' hashes, in the same order.
    kept_hashes: Vec<[u8; HASH]>,
    /// The pages kept, as ranges of page indexes, ascending.
    kept: SmallVec<[(u32, u32); 4]>,
    hashes: Vec<[u8; HASH]>,
    /// The length of the file last signed.
    len: usize,
    /// The synthesis ([`crate::model::Placed::synthesis`]) last signed; 0 for none.
    synthesis: u64,
}

/// What changed since the signature a [`Cache`] holds.
pub(super) struct Since<'a> {
    /// The synthesis these changes are from ([`crate::model::Delta::from`]): used only when
    /// it is the cache's.
    pub from: u64,
    /// The file's byte ranges written since, (offset, length), by offset, not overlapping.
    pub written: &'a [(u64, u64)],
}

/// A signature against a [`Cache`].
pub(super) struct Cached<'a> {
    pub cache: &'a mut Cache,
    /// The synthesis this signature is of, which the cache records.
    pub synthesis: u64,
    /// `None` hashes every page.
    pub since: Option<Since<'a>>,
    /// The file's byte ranges whose pages are kept and compared, (offset, length): the
    /// headers and `__LINKEDIT`.
    pub kept: &'a [(u64, u64)],
}

/// Whether a page (from `start`, `PAGE` bytes) overlaps a range of `written`.
fn touched(written: &[(u64, u64)], start: u64) -> bool {
    let at = written.partition_point(|it| it.0 + it.1 <= start);
    written.get(at).is_some_and(|it| it.0 < start + PAGE as u64)
}

/// The pages of `ranges` in a file of `len` bytes, as merged ranges of page indexes.
fn page_ranges(ranges: &[(u64, u64)], len: usize) -> SmallVec<[(u32, u32); 4]> {
    let mut out: SmallVec<[(u32, u32); 4]> = SmallVec::new();
    for &(at, size) in ranges.iter().filter(|it| it.1 != 0) {
        let first = (at / PAGE as u64) as u32;
        let end = (at + size).min(len as u64).div_ceil(PAGE as u64) as u32;
        if first >= end {
            continue;
        }
        match out.last_mut() {
            Some(last) if first <= last.1 => last.1 = last.1.max(end),
            _ => out.push((first, end)),
        }
    }
    out
}

/// The `k`th page of `ranges`, and back: a page's place among them.
fn nth_page(ranges: &[(u32, u32)], mut k: usize) -> usize {
    for &(first, end) in ranges {
        let len = (end - first) as usize;
        if k < len {
            return first as usize + k;
        }
        k -= len;
    }
    unreachable!("a kept page past the kept ranges")
}

fn place_of(ranges: &[(u32, u32)], page: usize) -> Option<usize> {
    let mut before = 0;
    for &(first, end) in ranges {
        if (first as usize..end as usize).contains(&page) {
            return Some(before + page - first as usize);
        }
        before += (end - first) as usize;
    }
    None
}

/// The signature of `file` (every byte before the signature), whose `__TEXT` is `text_len`
/// bytes from `text_at`; `main` for an executable. Appended to `out`.
pub(super) fn signature(
    file: &View<'_>,
    identifier: &str,
    text_at: u64,
    text_len: u64,
    main: bool,
    cache: Option<Cached<'_>>,
    out: &mut Vec<u8>,
) {
    let pages = file.len().div_ceil(PAGE);
    let hashes_at = DIRECTORY_HEADER + identifier.len() + 1;
    let directory_len = hashes_at + pages * HASH;
    let total = len(identifier, file.len() as u64) as usize;

    let start = out.len();
    out.reserve(total);
    let be32 = |out: &mut Vec<u8>, value: u32| out.extend_from_slice(&value.to_be_bytes());
    be32(out, SUPER_BLOB);
    be32(out, (BLOB_HEADER + directory_len) as u32);
    be32(out, 1);
    // The code directory's slot, and where it is.
    be32(out, 0);
    be32(out, BLOB_HEADER as u32);

    be32(out, CODE_DIRECTORY);
    be32(out, directory_len as u32);
    be32(out, VERSION);
    be32(out, ADHOC | LINKER_SIGNED);
    be32(out, hashes_at as u32);
    be32(out, DIRECTORY_HEADER as u32);
    // No special slots: a linker's signature has no requirements or entitlements.
    be32(out, 0);
    be32(out, pages as u32);
    be32(out, file.len() as u32);
    out.extend_from_slice(&[HASH as u8, HASH_SHA256, 0, PAGE_SHIFT]);
    // spare2, scatterOffset, teamOffset, spare3.
    for _ in 0..4 {
        be32(out, 0);
    }
    // codeLimit64: the 32-bit limit says it.
    out.extend_from_slice(&0u64.to_be_bytes());
    out.extend_from_slice(&text_at.to_be_bytes());
    out.extend_from_slice(&text_len.to_be_bytes());
    let flags = if main { EXECSEG_MAIN_BINARY } else { 0 };
    out.extend_from_slice(&flags.to_be_bytes());
    debug_assert_eq!(out.len() - start, BLOB_HEADER + DIRECTORY_HEADER);
    out.extend_from_slice(identifier.as_bytes());
    out.push(0);

    let at = out.len();
    out.resize(start + total, 0);
    let slots = &mut out[at..at + pages * HASH];
    match cache {
        None => hash(file, slots),
        Some(cache) => hash_cached(file, slots, cache),
    }
}

fn hash(file: &View<'_>, slots: &mut [u8]) {
    par::for_each(par::enumerate(par::chunks_mut(slots, HASH * GROUP)), |(group, slots)| {
        let mut buf = [0u8; PAGE];
        for (nth, slot) in slots.chunks_exact_mut(HASH).enumerate() {
            slot.copy_from_slice(&sha256::digest(file.page(group * GROUP + nth, &mut buf)));
        }
    });
}

fn hash_cached(file: &View<'_>, slots: &mut [u8], cached: Cached<'_>) {
    let pages = slots.len() / HASH;
    let Cached { cache, synthesis, since, kept } = cached;
    let kept = page_ranges(kept, file.len());
    let kept_len: usize = kept.iter().map(|it| (it.1 - it.0) as usize).sum();
    // `None`: every page is hashed.
    let written = since
        .filter(|it| it.from != 0 && it.from == cache.synthesis)
        .filter(|_| cache.len == file.len() && cache.hashes.len() == pages && cache.kept == kept)
        .map(|it| it.written);
    cache.synthesis = 0;
    cache.pages.resize(kept_len * PAGE, 0);
    cache.kept_hashes.resize(kept_len, [0; HASH]);
    cache.hashes.resize(pages, [0; HASH]);

    let ranges = &kept[..];
    par::for_each(
        par::enumerate(par::zip(par::chunks_mut(&mut cache.pages, PAGE), &mut cache.kept_hashes)),
        |(k, (old, hash))| {
            let page = nth_page(ranges, k);
            let mut buf = [0u8; PAGE];
            let bytes = file.page(page, &mut buf);
            let old = &mut old[..bytes.len()];
            let again = match written {
                None => true,
                Some(written) => touched(written, (page * PAGE) as u64) || old != bytes,
            };
            if again {
                old.copy_from_slice(bytes);
                *hash = sha256::digest(bytes);
            }
        },
    );

    let kept_hashes = &cache.kept_hashes;
    par::for_each(
        par::enumerate(par::zip(par::chunks_mut(slots, HASH * GROUP), par::chunks_mut(&mut cache.hashes, GROUP))),
        |(group, (slots, hashes))| {
            let mut buf = [0u8; PAGE];
            for (nth, (slot, hash)) in slots.chunks_exact_mut(HASH).zip(hashes.iter_mut()).enumerate() {
                let page = group * GROUP + nth;
                if let Some(k) = place_of(ranges, page) {
                    *hash = kept_hashes[k];
                } else if written.is_none_or(|written| touched(written, (page * PAGE) as u64)) {
                    *hash = sha256::digest(file.page(page, &mut buf));
                }
                slot.copy_from_slice(hash);
            }
        },
    );
    cache.len = file.len();
    cache.kept = kept;
    cache.synthesis = synthesis;
}

#[cfg(test)]
mod tests {
    use super::{Cache, Cached, PAGE, Since, View, signature};

    fn noise(seed: u64, len: usize) -> Vec<u8> {
        let mut out = vec![0; len];
        blaze_ids::rng::SplitMix64::new(seed).fill(&mut out);
        out
    }

    /// The file a view stands for, assembled.
    fn assembled(len: usize, parts: &[(u64, &[u8])]) -> Vec<u8> {
        let mut file = vec![0; len];
        for &(at, bytes) in parts {
            file[at as usize..at as usize + bytes.len()].copy_from_slice(bytes);
        }
        file
    }

    #[test]
    fn a_view_signs_as_the_assembled_file() {
        let (a, b, c) = (noise(1, 100), noise(2, 3 * PAGE + 7), noise(3, PAGE));
        let len = 6 * PAGE + 300;
        // Parts within one page, across pages, page-aligned, and gaps of zeros between.
        let parts: Vec<(u64, &[u8])> = vec![
            (4 * PAGE as u64 + 10, &c[..]),
            (0, &a[..]),
            (PAGE as u64 - 3, &b[..]),
            (6 * PAGE as u64, &[][..]),
        ];
        let view = View::new(len as u64, parts.clone()).unwrap();
        let file = assembled(len, &parts);
        let (mut want, mut seen) = (Vec::new(), Vec::new());
        signature(&View::whole(&file), "x", 0, 0x4000, true, None, &mut want);
        signature(&view, "x", 0, 0x4000, true, None, &mut seen);
        assert_eq!(seen, want);
    }

    #[test]
    fn overlapping_or_long_parts_are_errors() {
        let (a, b) = ([1u8; 10], [2u8; 10]);
        assert!(View::new(100, vec![(0, &a[..]), (5, &b[..])]).is_err());
        assert!(View::new(15, vec![(10, &a[..])]).is_err());
    }

    #[test]
    fn the_cache_changes_nothing() {
        let mut cache = Cache::default();
        let mut file = noise(7, 70 * PAGE + 123);
        for round in 1..9u64 {
            // What the core says it wrote, and whether it knows (a stale token says it doesn't).
            let mut written: Vec<(u64, u64)> = Vec::new();
            let mut from = round - 1;
            match round {
                // A page written, the file grown, shrunk, a page written back, a kept page
                // changed unsaid, a stale token, unchanged.
                2 => {
                    file[5 * PAGE + 9] ^= 0xff;
                    written.push((5 * PAGE as u64 + 9, 1));
                }
                3 => file.extend_from_slice(&noise(round, 3 * PAGE + 11)),
                4 => file.truncate(40 * PAGE + 5),
                5 => {
                    file[5 * PAGE + 9] ^= 0xff;
                    written.push((5 * PAGE as u64, 16));
                }
                6 => {
                    let at = file.len() - 3;
                    file[at] ^= 0xff;
                }
                7 => {
                    file[20 * PAGE] ^= 0xff;
                    from = 1;
                }
                _ => {}
            }
            let kept = [(0, PAGE as u64), (file.len() as u64 - 2 * PAGE as u64, 2 * PAGE as u64)];
            let since = Some(Since { from, written: &written });
            let (mut want, mut seen) = (Vec::new(), Vec::new());
            signature(&View::whole(&file), "id", 0, 0x4000, false, None, &mut want);
            let cached = Cached { cache: &mut cache, synthesis: round, since, kept: &kept };
            signature(&View::whole(&file), "id", 0, 0x4000, false, Some(cached), &mut seen);
            assert_eq!(seen, want, "round {round}");
        }
    }

    #[test]
    fn slots_are_signed_over_their_parts() {
        let (a, b) = (noise(1, 3 * PAGE), noise(2, PAGE));
        let len = 5 * PAGE;
        let parts: Vec<(u64, &[u8])> = vec![(0, &a[..]), (4 * PAGE as u64, &b[..])];
        let slots = [(0x1000_0008, 7u64), (0x1000_0000 + 2 * PAGE as u64, u64::MAX)];
        let view = View::new(len as u64, parts.clone()).unwrap().with_slots(&slots, 0x1000_0000).unwrap();
        let mut file = assembled(len, &parts);
        for &(addr, value) in &slots {
            let at = (addr - 0x1000_0000) as usize;
            file[at..at + 8].copy_from_slice(&value.to_le_bytes());
        }
        let (mut want, mut seen) = (Vec::new(), Vec::new());
        signature(&View::whole(&file), "x", 0, 0x4000, true, None, &mut want);
        signature(&view, "x", 0, 0x4000, true, None, &mut seen);
        assert_eq!(seen, want);
        let unaligned = [(0x1000_0004, 0u64)];
        assert!(View::new(len as u64, parts).unwrap().with_slots(&unaligned, 0x1000_0000).is_err());
    }
}
