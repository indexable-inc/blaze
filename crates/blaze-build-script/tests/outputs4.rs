//! The `bzbsout4` outputs record (NATIVE.md 3.8, "Large outputs"): "a file over 1 MiB is stored
//! as its own record (content id) and the outputs record names it by id and length; the 64 MiB
//! limit becomes 1 GiB in total, 4096 files. bzbsout3 records are an old home: reported and
//! rebuilt (rule 26)." Rule 26 (AGENTS.md): "A restore equals a fresh build in bytes; an old
//! home is reported and rebuilt."

#![feature(allocator_api)]

use blaze_build_script::outputs::{FileLimits, Outputs, RecordError};
use blaze_build_script::outputs4::{BLOB_OVER, BlobError, Blobs, FileRef, LAYOUT4, Outputs4Ref, encode4};
use blaze_ids::ContentId;
use blaze_own::{AVec, Alloc, String, Vec};

const OUT: &str = "/target/debug/build/x-0123456789abcdef/out";

/// The test's store of blobs: each kept once under the content id of its bytes.
struct Shelf<'a> {
    kept: AVec<'a, (ContentId, AVec<'a, u8>)>,
    puts: usize,
}

impl<'a> Shelf<'a> {
    fn new(alloc: Alloc<'a>) -> Shelf<'a> {
        Shelf { kept: Vec::new_in(alloc), puts: 0 }
    }

    fn put(&mut self, bytes: &[u8], alloc: Alloc<'a>) -> ContentId {
        self.puts += 1;
        let id = ContentId::of("blaze v2 test outputs4 blob", bytes);
        if !self.kept.iter().any(|(k, _)| *k == id) {
            let mut owned = Vec::with_capacity_in(bytes.len(), alloc);
            owned.extend_from_slice(bytes);
            self.kept.push((id, owned));
        }
        id
    }
}

impl Blobs for Shelf<'_> {
    fn get(&self, id: ContentId, _len: u64) -> Result<&[u8], BlobError> {
        self.kept.iter().find(|(k, _)| *k == id).map(|(_, b)| &b[..]).ok_or(BlobError::Missing { id })
    }
}

/// A store that answers every id with three bytes: the planted wrong blob.
struct Liar;

impl Blobs for Liar {
    fn get(&self, _id: ContentId, _len: u64) -> Result<&[u8], BlobError> {
        Ok(b"abc")
    }
}

fn filled(len: usize, alloc: Alloc<'_>) -> AVec<'_, u8> {
    let mut bytes = Vec::with_capacity_in(len, alloc);
    let mut at = 0usize;
    while at < len {
        bytes.push((at % 251) as u8);
        at += 1;
    }
    bytes
}

/// "a file over 1 MiB is stored as its own record (content id) and the outputs record names it
/// by id and length" (NATIVE.md 3.8): a 2 MiB file becomes a Blob, a 10 byte file and a file of
/// exactly 1 MiB (not over it) stay Inline, and the record does not hold the blob's bytes.
/// Prints: "test small_inline_large_blob ... ok".
#[test]
fn small_inline_large_blob() {
    let alloc = Alloc::global();
    let big = filled(2 << 20, alloc);
    let edge = filled(BLOB_OVER as usize, alloc);
    let mut out = Outputs::new_in(OUT, alloc);
    let limits = FileLimits::DEFAULT;
    out.add_file("big.a", &big, limits, alloc).unwrap_or_else(|e| panic!("{e:?}"));
    out.add_file("edge.bin", &edge, limits, alloc).unwrap_or_else(|e| panic!("{e:?}"));
    out.add_file("small.rs", b"0123456789", limits, alloc).unwrap_or_else(|e| panic!("{e:?}"));
    let mut shelf = Shelf::new(alloc);
    let mut record = Vec::new_in(alloc);
    encode4(&out, &mut |bytes| shelf.put(bytes, alloc), &mut record);
    assert_eq!(shelf.puts, 1, "exactly the one file over BLOB_OVER is a blob");
    assert_eq!(&record[..8], &LAYOUT4.to_le_bytes()[..]);
    assert!((record.len() as u64) < BLOB_OVER + (64 << 10), "the record holds the 1 MiB inline file, not the 2 MiB blob: {}", record.len());
    let opened = Outputs4Ref::open(&record).unwrap_or_else(|e| panic!("{e:?}"));
    let id = ContentId::of("blaze v2 test outputs4 blob", &big);
    assert_eq!(opened.file_ref("big.a"), Some(FileRef::Blob { id, len: 2 << 20 }));
    assert_eq!(opened.file_ref("small.rs"), Some(FileRef::Inline(b"0123456789")));
    assert_eq!(opened.file_ref("edge.bin"), Some(FileRef::Inline(&edge[..])));
    assert_eq!(opened.file_ref("missing.rs"), None);
    let mut path = String::from_str_in(OUT, alloc);
    path.push_str("/big.a");
    assert_eq!(opened.file_at_ref(&path), Some(Some(FileRef::Blob { id, len: 2 << 20 })));
    assert_eq!(opened.file_at_ref("/elsewhere/big.a"), None);
    match opened.file_ref("big.a").map(|f| f.bytes(&shelf)) {
        Some(Ok(bytes)) => assert_eq!(bytes, &big[..]),
        other => panic!("the blob is read back from the store: {other:?}"),
    }
}

/// Rule 26: "A restore equals a fresh build in bytes; an old home is reported and rebuilt." The
/// record read back with its blobs is the outputs it was made from, and encodes to the same
/// bytes; every proper prefix is refused; a `bzbsout3` record is refused as an old home by
/// name; a missing blob and a blob of the wrong length are refused by name (the `Liar` store
/// is the planted bug the length check catches). Prints: "test round_trip ... ok".
#[test]
fn round_trip() {
    let alloc = Alloc::global();
    let mut out = Outputs::new_in(OUT, alloc);
    out.cfgs.push((String::from_str_in("has_asm", alloc), None));
    out.cfgs.push((String::from_str_in("feature", alloc), Some(String::from_str_in("x", alloc))));
    out.env.push((String::from_str_in("N", alloc), String::from_str_in("v", alloc)));
    out.link_libs.push(String::from_str_in("static=crypto", alloc));
    out.warnings.push(String::from_str_in("w", alloc));
    out.c_requests.push(String::from_str_in("cc\0-c\0a.c", alloc));
    let big = filled((2 << 20) + 3, alloc);
    let limits = FileLimits::DEFAULT;
    out.add_file("lib/libcrypto.a", &big, limits, alloc).unwrap_or_else(|e| panic!("{e:?}"));
    out.add_file("bindings.rs", b"pub const A: u32 = 1;", limits, alloc).unwrap_or_else(|e| panic!("{e:?}"));
    let mut shelf = Shelf::new(alloc);
    let mut record = Vec::new_in(alloc);
    encode4(&out, &mut |bytes| shelf.put(bytes, alloc), &mut record);
    let opened = Outputs4Ref::open(&record).unwrap_or_else(|e| panic!("{e:?}"));
    assert_eq!(opened.out_dir(), OUT);
    let owned = opened.to_owned(&shelf, alloc).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(owned, out);
    let mut again = Vec::new_in(alloc);
    encode4(&owned, &mut |bytes| shelf.put(bytes, alloc), &mut again);
    assert_eq!(&record[..], &again[..]);
    assert_eq!(opened.crate_digest(), Outputs4Ref::open(&again).map(|r| r.crate_digest()).unwrap_or_else(|e| panic!("{e:?}")));
    for cut in [0, 7, 8, record.len() / 2, record.len() - 1] {
        assert!(Outputs4Ref::open(&record[..cut]).is_err(), "prefix {cut}");
    }
    // The old home: the same outputs in `outputs::LAYOUT`.
    let mut old = Vec::new_in(alloc);
    out.encode(&mut old);
    let tag = blaze_build_script::outputs::LAYOUT;
    assert_eq!(Outputs4Ref::open(&old).err(), Some(RecordError::OldHome { tag }));
    // A blob the store does not have, and one of the wrong length.
    let id = ContentId::of("blaze v2 test outputs4 blob", &big);
    assert_eq!(opened.to_owned(&Shelf::new(alloc), alloc).err(), Some(BlobError::Missing { id }));
    assert_eq!(opened.to_owned(&Liar, alloc).err(), Some(BlobError::Length { id, want: (2 << 20) + 3, got: 3 }));
}
