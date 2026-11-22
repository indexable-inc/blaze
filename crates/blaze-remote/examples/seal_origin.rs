//! Seals a checkout's local records into an origin folder (remote-read.md, "Measured"): every
//! file under `<checkout>/.blaze/v2/records/` (named by its key's 64 hex digits) goes into plain
//! segments of at most 4 MiB, and `blaze_remote::read::publish` lays out their index pages,
//! summaries, the manifest and a head signed by the seed's key. The objects are written to
//! `<origin>`, which must not exist yet. Prints the public key to pass as `--origin-key`.
//!
//! `seal_origin <checkout> <origin> <seed: 64 hex digits>`. A tool run by hand and in the
//! measurement, standing in for CI's writer: only CI and the remote workers write a shared origin.

#![feature(allocator_api)]

use std::alloc::Global;
use std::path::PathBuf;

use blaze_cas::plain::{Namespace, seal_segment};
use blaze_ids::ContentId;
use blaze_keyed::{exec::RecordRef, kind::Tag};
use blaze_own::Erased;
use blaze_remote::Signer;
use blaze_remote::read::publish;

/// The tag records of blaze-cli's file store are sealed under: one kind, as that store has.
const TAG: Tag = Tag(1);
const SEGMENT_MOST: usize = 4 << 20;
const PAGE_ENTRIES: u32 = 64;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let [_, checkout, origin, seed] = args.as_slice() else {
        eprintln!("usage: seal_origin <checkout> <origin> <seed: 64 hex digits>");
        std::process::exit(2);
    };
    let signer = Signer::from_hex("the seed", seed).unwrap_or_else(|error| {
        eprintln!("seal_origin: {error}");
        std::process::exit(2);
    });
    let records_dir = PathBuf::from(checkout).join(".blaze/v2/records");
    let mut records: Vec<(ContentId, Vec<u8>)> = Vec::new();
    let listing = std::fs::read_dir(&records_dir).unwrap_or_else(|error| panic!("{}: {error}", records_dir.display()));
    for entry in listing {
        let path = entry.expect("a directory entry").path();
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        let Some(key) = blaze_format::hex::decode::<32>(name) else {
            panic!("{}: not a record's name (64 hex digits)", path.display())
        };
        let bytes = std::fs::read(&path).unwrap_or_else(|error| panic!("{}: {error}", path.display()));
        records.push((ContentId(key), bytes));
    }
    records.sort_by_key(|(key, _)| *key);
    let mut segments: Vec<Vec<u8>> = Vec::new();
    let mut from = 0;
    while from < records.len() {
        let mut to = from;
        let mut size = 0;
        while to < records.len() && (to == from || size + records[to].1.len() <= SEGMENT_MOST) {
            size += records[to].1.len();
            to += 1;
        }
        let refs: Vec<(RecordRef, &[u8])> = records[from..to].iter().map(|(key, bytes)| (RecordRef { tag: TAG, key: *key }, bytes.as_slice())).collect();
        let mut out = Vec::new();
        seal_segment(Namespace::PUBLIC, &refs, &mut out).expect("a segment of records read whole");
        segments.push(out);
        from = to;
    }
    let refs: Vec<&[u8]> = segments.iter().map(|s| s.as_slice()).collect();
    let global = Erased::new(Global);
    let objects = publish(Namespace::PUBLIC, 1, &signer, &refs, PAGE_ENTRIES, global.handle()).unwrap_or_else(|error| {
        eprintln!("seal_origin: {error}");
        std::process::exit(1);
    });
    std::fs::create_dir(origin).unwrap_or_else(|error| panic!("{origin}: {error} (the origin folder must not exist yet)"));
    let mut total = 0;
    for object in objects.iter() {
        let path = PathBuf::from(origin).join(object.object.name().as_str());
        std::fs::write(&path, object.bytes.as_slice()).unwrap_or_else(|error| panic!("{}: {error}", path.display()));
        total += object.bytes.len();
    }
    let record_bytes: usize = records.iter().map(|(_, b)| b.len()).sum();
    println!("sealed {} records ({record_bytes} bytes) into {} segments, {} objects, {total} bytes", records.len(), segments.len(), objects.len());
    println!("origin key: {}", blaze_format::hex::encode(&signer.public().0));
}
