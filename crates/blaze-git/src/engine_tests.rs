//! Deterministic simulation tests (DST), property tests, and planted-bug oracle controls for
//! `storage`, `slab` (`BGOB`/`BGIX`/`BGRF`/`CGPH`), `merge` (ORT 3-way), `pack_write`, `wire`
//! (client + server + Smart HTTP), and `infect`/`disinfect`.

use blaze_git_fetch::{Kind as ObjectKind, Oid, PackLimits, store::Idx};
use blaze_own::{
    Alloc, String, Vec,
    planted::{NoPlant, Planted},
    testing::heap,
};
use blaze_sim::{Config, check};

use crate::{
    ErrorKind,
    commit_graph::CommitGraph,
    infect::{InfectBuilder, build_bgit_image_from_store, disinfect_from_store},
    merge::{
        BlobMerge, ConflictKind, ConflictStyle, MergeLimits, MergeOpts, MergeStatus,
        find_merge_bases, merge_blobs, merge_blobs_planted, merge_commits, merge_trees,
    },
    pack_write::{
        PackWriteOpts, encode_delta, unpack_pack_into_store_planted, write_pack_from_store_planted,
        write_pack_index_v2,
    },
    plant::GitEnginePlant,
    slab::{
        SlabCompression, SlabIndex, SlabPayload, SlabReader, SlabWriter,
        build_commit_graph_from_store, decode_reftable, encode_reftable, encode_slab_index,
    },
    storage::{
        MemStore, ObjectStore, RefStore, RefTarget, RefUpdate, Signature, TreeBuildEntry,
        encode_commit, encode_tree,
    },
    wire::{
        HttpParse, ReceivePackServer, Transport, UploadPackServer, WireClient, WireLimits,
        handle_smart_http, parse_http_request,
    },
};

fn test_alloc() -> Alloc<'static> {
    heap().alloc()
}

fn sig() -> Signature<'static> {
    Signature { name: "Blaze", email: "dev@ix.dev", timestamp: 1_700_000_000, tz: "+0000" }
}

fn mk_blob<'a>(store: &mut MemStore<'a>, data: &[u8]) -> Oid {
    store.write_object(ObjectKind::Blob, data).expect("write blob")
}

fn mk_tree<'a>(store: &mut MemStore<'a>, entries: &[TreeBuildEntry<'_>], alloc: Alloc<'a>) -> Oid {
    let mut buf = Vec::new_in(alloc);
    encode_tree(entries, alloc, &mut buf).expect("encode tree");
    store.write_object(ObjectKind::Tree, &buf).expect("write tree")
}

fn mk_commit<'a>(
    store: &mut MemStore<'a>,
    tree: Oid,
    parents: &[Oid],
    ts: i64,
    msg: &str,
    alloc: Alloc<'a>,
) -> Oid {
    let mut buf = Vec::new_in(alloc);
    let s = Signature { timestamp: ts, ..sig() };
    encode_commit(tree, parents, s, s, msg, &mut buf);
    store.write_object(ObjectKind::Commit, &buf).expect("write commit")
}

// ---------------------------------------------------------------------------
// 1. Native .bgit Slab / Index / RefTable / CommitGraph DST + planted control
// ---------------------------------------------------------------------------

/// The invariant: every object written into a `BGOB` slab (Raw or AutoZstd) and indexed by `BGIX`
/// round-trips byte-for-byte with CRC-32 protection, and any single-byte corruption is refused.
fn run_slab_roundtrip_and_corruption<P: Planted<GitEnginePlant>>(plant: P) {
    let alloc = test_alloc();
    check(Config::cases(64), |tape| {
        let count = tape.int_in(1..=16, "object_count") as usize;
        let mode = if tape.coin("raw_only") {
            SlabCompression::RawOnly
        } else {
            SlabCompression::AutoZstd
        };
        let mut writer = SlabWriter::new(0, mode, alloc);
        let mut expected: Vec<(Oid, ObjectKind, Vec<u8, Alloc<'_>>), Alloc<'_>> = Vec::new_in(alloc);

        for i in 0..count {
            let kind = match tape.below(4, "kind") {
                0 => ObjectKind::Blob,
                1 => ObjectKind::Tree,
                2 => ObjectKind::Commit,
                _ => ObjectKind::Tag,
            };
            let len = tape.int_in(0..=600, "len") as usize;
            let repeat = tape.coin("compressible");
            let mut body = Vec::with_capacity_in(len, alloc);
            for j in 0..len {
                let b = if repeat { b'A' + ((j / 32) % 4) as u8 } else { ((i * 31 + j * 17) & 0xff) as u8 };
                body.push(b);
            }
            let entry = writer.append(kind, &body).expect("append");
            expected.push((entry.oid, kind, body));
        }

        let (mut slab_bytes, entries) = writer.into_parts();
        let mut idx_bytes = Vec::new_in(alloc);
        let indexed = encode_slab_index(&entries, alloc, &mut idx_bytes).expect("encode index");
        assert!(indexed >= 1);

        let idx = SlabIndex::parse(&idx_bytes, 1024).expect("parse index");
        let reader = SlabReader::parse(&slab_bytes).expect("parse slab");
        let mut scratch = Vec::new_in(alloc);

        for (oid, kind, body) in &expected {
            let loc = idx.lookup(oid).expect("lookup oid");
            assert_eq!(loc.raw_len as usize, body.len());
            let rec = reader
                .read_at_planted(loc.offset, 1 << 20, alloc, &mut scratch, plant)
                .expect("read slab record");
            assert_eq!(rec.oid, *oid);
            assert_eq!(rec.kind, *kind);
            let actual: &[u8] = match rec.payload {
                SlabPayload::Borrowed(s) => s,
                SlabPayload::InOut => &scratch,
            };
            assert_eq!(actual, body.as_slice());
        }

        // Flip one byte inside the first record's payload/header and assert CRC refuses it.
        let first_loc = idx.lookup(&expected[0].0).expect("first loc");
        let flip_at = (first_loc.offset as usize) + 2;
        slab_bytes[flip_at] ^= 0x01;
        let corrupted = SlabReader::parse(&slab_bytes).expect("header intact");
        let res = corrupted.read_at_planted(first_loc.offset, 1 << 20, alloc, &mut scratch, plant);
        assert!(
            matches!(res, Err(e) if e.kind == ErrorKind::ChecksumMismatch),
            "expected ChecksumMismatch on corrupted slab record, got {res:?}"
        );
    });
}

#[test]
fn slab_index_reftable_and_commit_graph_roundtrip() {
    run_slab_roundtrip_and_corruption(NoPlant);

    // Also test BGRF reftable + CGPH commit-graph round-trip!
    let alloc = test_alloc();
    let mut store = MemStore::new(alloc);
    let b1 = mk_blob(&mut store, b"hello\n");
    let t1 = mk_tree(&mut store, &[TreeBuildEntry { mode: 0o100644, name: b"a.txt", oid: b1 }], alloc);
    let c1 = mk_commit(&mut store, t1, &[], 1_700_000_001, "init\n", alloc);
    let c2 = mk_commit(&mut store, t1, &[c1], 1_700_000_002, "second\n", alloc);
    store
        .write_head(&RefTarget::Symbolic(String::from_str_in("refs/heads/main", alloc)))
        .expect("head");
    store.set_ref("refs/heads/main", RefTarget::Direct(c2)).expect("main");

    let mut refs = Vec::new_in(alloc);
    store.list_refs("", alloc, &mut refs).expect("list");
    let head = store.read_head(alloc).expect("read head");
    let mut rt_bytes = Vec::new_in(alloc);
    encode_reftable(head.as_ref(), &refs, alloc, &mut rt_bytes).expect("encode reftable");
    let decoded = decode_reftable(&rt_bytes, 1024, alloc).expect("decode reftable");
    assert_eq!(decoded.head, head);
    assert_eq!(decoded.refs.len(), 1);
    assert_eq!(decoded.refs[0].name.as_str(), "refs/heads/main");
    assert_eq!(decoded.refs[0].target, RefTarget::Direct(c2));

    let mut cg_bytes = Vec::new_in(alloc);
    let n_cg = build_commit_graph_from_store(&store, alloc, &mut cg_bytes).expect("cgph");
    assert_eq!(n_cg, 2);
    let graph = CommitGraph::from_layers(&[&cg_bytes], alloc).expect("parse cgph");
    let g1 = graph.lookup(&c1).expect("c1 in graph");
    let g2 = graph.lookup(&c2).expect("c2 in graph");
    assert_eq!(g1.generation, 1);
    assert_eq!(g2.generation, 2);
    assert_eq!(g2.tree, t1);
}

#[test]
#[should_panic]
fn the_oracle_catches_skip_slab_crc_check() {
    run_slab_roundtrip_and_corruption(GitEnginePlant::SkipSlabCrcCheck);
}

// ---------------------------------------------------------------------------
// 2. ORT 3-Way Merge Engine DST + planted control
// ---------------------------------------------------------------------------

/// The invariant: non-overlapping edits merge cleanly and commute; overlapping differing edits
/// ALWAYS report a conflict with markers.
fn run_blob_merge_oracle<P: Planted<GitEnginePlant>>(plant: P) {
    let alloc = test_alloc();
    let base = b"line1\nline2\nline3\nline4\nline5\n";
    let ours_non = b"OURS1\nline2\nline3\nline4\nline5\n";
    let theirs_non = b"line1\nline2\nline3\nline4\nTHEIRS5\n";
    let m1 = merge_blobs_planted(base, ours_non, theirs_non, MergeOpts::DEFAULT, alloc, plant);
    let m2 = merge_blobs_planted(base, theirs_non, ours_non, MergeOpts::DEFAULT, alloc, plant);
    assert_eq!(m1, m2);
    match m1 {
        BlobMerge::Clean(b) => assert_eq!(b.as_slice(), b"OURS1\nline2\nline3\nline4\nTHEIRS5\n"),
        other => panic!("expected clean merge, got {other:?}"),
    }

    // Identical overlapping edit converges cleanly!
    let both_same = b"line1\nSAME2\nline3\nline4\nline5\n";
    assert!(matches!(
        merge_blobs_planted(base, both_same, both_same, MergeOpts::DEFAULT, alloc, plant),
        BlobMerge::Clean(ref b) if b.as_slice() == both_same
    ));

    // Differing overlapping edit MUST conflict!
    let ours_c = b"line1\nOURS2\nline3\nline4\nline5\n";
    let theirs_c = b"line1\nTHEIRS2\nline3\nline4\nline5\n";
    let res = merge_blobs_planted(
        base,
        ours_c,
        theirs_c,
        MergeOpts { conflict_style: ConflictStyle::Diff3, ..MergeOpts::DEFAULT },
        alloc,
        plant,
    );
    match res {
        BlobMerge::Conflicted(bytes) => {
            let text = core::str::from_utf8(&bytes).expect("utf8");
            assert!(text.contains("<<<<<<< ours\nOURS2\n||||||| base\nline2\n=======\nTHEIRS2\n>>>>>>> theirs\n"));
        }
        other => panic!("expected Conflicted, got {other:?}"),
    }
}

#[test]
fn merge_blobs_ort_trees_lca_renames_and_crisscross() {
    run_blob_merge_oracle(NoPlant);

    let alloc = test_alloc();
    let mut store = MemStore::new(alloc);

    // Build an untouched subtree `untouched/` and a modified file `src/main.rs` plus a rename!
    let big = mk_blob(&mut store, b"untouched content\n");
    let untouched_tree = mk_tree(
        &mut store,
        &[TreeBuildEntry { mode: 0o100644, name: b"keep.txt", oid: big }],
        alloc,
    );
    let main_base = mk_blob(&mut store, b"fn a() {}\nfn b() {}\nfn c() {}\n");
    let ren_base = mk_blob(&mut store, b"rename me v1\n");
    let root_base = mk_tree(
        &mut store,
        &[
            TreeBuildEntry { mode: 0o040000, name: b"untouched", oid: untouched_tree },
            TreeBuildEntry { mode: 0o100644, name: b"main.rs", oid: main_base },
            TreeBuildEntry { mode: 0o100644, name: b"old.txt", oid: ren_base },
        ],
        alloc,
    );
    let c_base = mk_commit(&mut store, root_base, &[], 100, "base\n", alloc);

    // Ours edits top of main.rs and renames old.txt -> new.txt (same OID).
    let main_ours = mk_blob(&mut store, b"fn a_ours() {}\nfn b() {}\nfn c() {}\n");
    let root_ours = mk_tree(
        &mut store,
        &[
            TreeBuildEntry { mode: 0o040000, name: b"untouched", oid: untouched_tree },
            TreeBuildEntry { mode: 0o100644, name: b"main.rs", oid: main_ours },
            TreeBuildEntry { mode: 0o100644, name: b"new.txt", oid: ren_base },
        ],
        alloc,
    );
    let c_ours = mk_commit(&mut store, root_ours, &[c_base], 200, "ours\n", alloc);

    // Theirs edits bottom of main.rs and modifies old.txt!
    let main_theirs = mk_blob(&mut store, b"fn a() {}\nfn b() {}\nfn c_theirs() {}\n");
    let ren_theirs = mk_blob(&mut store, b"rename me v2\n");
    let root_theirs = mk_tree(
        &mut store,
        &[
            TreeBuildEntry { mode: 0o040000, name: b"untouched", oid: untouched_tree },
            TreeBuildEntry { mode: 0o100644, name: b"main.rs", oid: main_theirs },
            TreeBuildEntry { mode: 0o100644, name: b"old.txt", oid: ren_theirs },
        ],
        alloc,
    );
    let c_theirs = mk_commit(&mut store, root_theirs, &[c_base], 300, "theirs\n", alloc);

    // Build commit-graph and run ORT commit merge!
    let mut cg_bytes = Vec::new_in(alloc);
    build_commit_graph_from_store(&store, alloc, &mut cg_bytes).expect("cg");
    let graph = CommitGraph::from_layers(&[&cg_bytes], alloc).expect("parse cg");

    let bases = find_merge_bases(&store, Some(&graph), c_ours, c_theirs, MergeLimits::SHIPPED, alloc)
        .expect("lca");
    assert_eq!(bases.as_slice(), &[c_base]);

    let merged = merge_commits(&mut store, Some(&graph), c_ours, c_theirs, MergeOpts::DEFAULT, alloc)
        .expect("merge commits");
    assert_eq!(merged.status, MergeStatus::MergedClean);
    assert!(merged.conflicts.is_empty());
    assert_eq!(merged.stats.blobs_merged, 1);
    assert_eq!(merged.stats.renames_detected, 1);

    // Verify merged tree has merged main.rs, renamed new.txt with theirs content, and untouched tree!
    let mut tree_buf = Vec::new_in(alloc);
    assert_eq!(store.read_object(&merged.tree, &mut tree_buf).unwrap(), Some(ObjectKind::Tree));
    let mut iter = crate::objects::TreeIter::new(merged.tree, &tree_buf);
    let mut found_main = None;
    let mut found_new = None;
    let mut found_untouched = None;
    while let Some(e) = iter.next_entry().unwrap() {
        match e.name {
            b"main.rs" => found_main = Some(e.oid),
            b"new.txt" => found_new = Some(e.oid),
            b"untouched" => found_untouched = Some(e.oid),
            other => panic!("unexpected entry {:?}", core::str::from_utf8(other)),
        }
    }
    assert_eq!(found_untouched, Some(untouched_tree));
    assert_eq!(found_new, Some(ren_theirs));
    let mut main_out = Vec::new_in(alloc);
    store.read_object(&found_main.unwrap(), &mut main_out).unwrap();
    assert_eq!(main_out.as_slice(), b"fn a_ours() {}\nfn b() {}\nfn c_theirs() {}\n");

    // Fast-forward & AlreadyUpToDate checks.
    let ff = merge_commits(&mut store, Some(&graph), c_base, c_ours, MergeOpts::DEFAULT, alloc).unwrap();
    assert_eq!(ff.status, MergeStatus::FastForward);
    assert_eq!(ff.tree, root_ours);
    let up = merge_commits(&mut store, Some(&graph), c_ours, c_base, MergeOpts::DEFAULT, alloc).unwrap();
    assert_eq!(up.status, MergeStatus::AlreadyUpToDate);
    assert_eq!(up.tree, root_ours);

    // Binary conflict & Directory/File conflict checks.
    let bin_b = mk_blob(&mut store, b"\x00\x01\x02");
    let bin_o = mk_blob(&mut store, b"\x00\x01\x99");
    let bin_t = mk_blob(&mut store, b"\x00\x01\x88");
    assert_eq!(merge_blobs(b"\x00a", b"\x00b", b"\x00c", MergeOpts::DEFAULT, alloc), BlobMerge::BinaryConflict);
    let tb = mk_tree(&mut store, &[TreeBuildEntry { mode: 0o100644, name: b"x", oid: bin_b }], alloc);
    let to = mk_tree(&mut store, &[TreeBuildEntry { mode: 0o100644, name: b"x", oid: bin_o }], alloc);
    let tt = mk_tree(&mut store, &[TreeBuildEntry { mode: 0o040000, name: b"x", oid: untouched_tree }], alloc);
    let df = merge_trees(&mut store, Some(tb), Some(to), Some(tt), MergeOpts::DEFAULT, alloc).unwrap();
    assert_eq!(df.status, MergeStatus::Conflicted);
    assert_eq!(df.conflicts.len(), 1);
    assert_eq!(df.conflicts[0].kind, ConflictKind::DirectoryFile);
    let _ = bin_t;
}

#[test]
#[should_panic]
fn the_oracle_catches_skip_three_way_conflict() {
    run_blob_merge_oracle(GitEnginePlant::SkipThreeWayConflict);
}

// ---------------------------------------------------------------------------
// 3. PACK v2 + Wire Protocol Server & Client + Smart HTTP DST + planted controls
// ---------------------------------------------------------------------------

fn run_pack_and_wire_roundtrip<P: Planted<GitEnginePlant>>(plant: P) {
    let alloc = test_alloc();

    let mut server = MemStore::new(alloc);
    let b1 = mk_blob(&mut server, b"hello world from blaze git engine!\nrepeat line 1\nrepeat line 2\nrepeat line 3\nrepeat line 4\n");
    let b2 = mk_blob(&mut server, b"hello world from blaze git engine!\nrepeat line 1\nMODIFIED line 2\nrepeat line 3\nrepeat line 4\n");
    // Verify delta encoder compresses b1 -> b2 cleanly!
    let mut delta = Vec::new_in(alloc);
    let mut b1_bytes = Vec::new_in(alloc);
    let mut b2_bytes = Vec::new_in(alloc);
    server.read_object(&b1, &mut b1_bytes).unwrap();
    server.read_object(&b2, &mut b2_bytes).unwrap();
    assert!(encode_delta(&b1_bytes, &b2_bytes, alloc, &mut delta));

    let t1 = mk_tree(&mut server, &[TreeBuildEntry { mode: 0o100644, name: b"f.txt", oid: b1 }], alloc);
    let c1 = mk_commit(&mut server, t1, &[], 1_700_000_010, "c1\n", alloc);
    let t2 = mk_tree(&mut server, &[TreeBuildEntry { mode: 0o100644, name: b"f.txt", oid: b2 }], alloc);
    let c2 = mk_commit(&mut server, t2, &[c1], 1_700_000_020, "c2\n", alloc);

    server.write_head(&RefTarget::Symbolic(String::from_str_in("refs/heads/main", alloc))).unwrap();
    server.set_ref("refs/heads/main", RefTarget::Direct(c2)).unwrap();

    // 1. PACK v2 + .idx v2 generation & validation by blaze-git-fetch::store::Idx!
    let mut oids = Vec::new_in(alloc);
    server.list_oids(&mut oids).unwrap();
    let mut pack_bytes = Vec::new_in(alloc);
    let written = write_pack_from_store_planted(
        &server,
        &oids,
        PackWriteOpts::FAST,
        alloc,
        &mut pack_bytes,
        plant,
    )
    .expect("write pack");
    let mut idx_bytes = Vec::new_in(alloc);
    write_pack_index_v2(&pack_bytes, &written, alloc, &mut idx_bytes).expect("write idx");
    let parsed_idx = Idx::parse(&idx_bytes, 1024).expect("parse idx");
    parsed_idx.verify_checksum().expect("idx checksum");
    parsed_idx.verify_ordering().expect("idx ordering");

    let mut unpacked_store = MemStore::new(alloc);
    let stats = unpack_pack_into_store_planted(
        &pack_bytes,
        &mut unpacked_store,
        PackLimits::SHIPPED,
        alloc,
        NoPlant,
    )
    .expect("unpack pack");
    assert_eq!(stats.objects_unpacked as usize, oids.len());

    // 2. UploadPackServer <-> WireClient (v2 advertise, ls-refs, clone fetch, shallow fetch)
    let mut advert = Vec::new_in(alloc);
    UploadPackServer::advertise(Transport::Http, &mut advert).expect("advertise");
    blaze_git_fetch::check_advertisement(&advert).expect("valid v2 advertisement");

    let mut ls_req = Vec::new_in(alloc);
    WireClient::encode_ls_refs(&["refs/heads/", "HEAD"], &mut ls_req).expect("encode ls-refs");
    let mut ls_resp = Vec::new_in(alloc);
    UploadPackServer::handle_request(
        &ls_req,
        &server,
        WireLimits::SHIPPED,
        PackWriteOpts::FAST,
        alloc,
        &mut ls_resp,
    )
    .expect("handle ls-refs");
    let remote_refs = WireClient::parse_ls_refs(&ls_resp, alloc).expect("parse ls-refs");
    assert!(remote_refs.iter().any(|r| r.name.as_str() == "HEAD" && r.oid == Some(c2)));
    assert!(remote_refs.iter().any(|r| r.name.as_str() == "refs/heads/main" && r.oid == Some(c2)));

    let mut fetch_req = Vec::new_in(alloc);
    WireClient::encode_fetch(&[c2], &[], None, &mut fetch_req).expect("encode fetch");
    let mut fetch_resp = Vec::new_in(alloc);
    UploadPackServer::handle_request_planted(
        &fetch_req,
        &server,
        WireLimits::SHIPPED,
        PackWriteOpts::FAST,
        alloc,
        &mut fetch_resp,
        plant,
    )
    .expect("handle fetch");

    let mut client = MemStore::new(alloc);
    let fstats = WireClient::parse_fetch_and_unpack(
        &fetch_resp,
        &mut client,
        WireLimits::SHIPPED,
        alloc,
    )
    .expect("unpack fetched pack");
    assert_eq!(fstats.objects_unpacked, 6);
    assert!(client.has_object(&c1));
    assert!(client.has_object(&c2));

    // 3. ReceivePackServer <-> WireClient (push new commit + atomic CAS rejection control!)
    let b3 = mk_blob(&mut client, b"pushed from client!\n");
    let t3 = mk_tree(&mut client, &[TreeBuildEntry { mode: 0o100644, name: b"f.txt", oid: b3 }], alloc);
    let c3 = mk_commit(&mut client, t3, &[c2], 1_700_000_030, "c3\n", alloc);

    let mut rp_advert_bytes = Vec::new_in(alloc);
    ReceivePackServer::advertise(Transport::Http, &server, alloc, &mut rp_advert_bytes).unwrap();
    let rp_advert = WireClient::parse_receive_pack_advert(&rp_advert_bytes, alloc).unwrap();
    assert!(rp_advert.report_status && rp_advert.sideband_64k && rp_advert.atomic);

    // First try a STALE old-OID push (`c1` instead of `c2`): MUST be rejected by CAS!
    let stale_updates = [RefUpdate {
        name: String::from_str_in("refs/heads/main", alloc),
        old: Some(c1),
        new: c3,
    }];
    let mut stale_req = Vec::new_in(alloc);
    WireClient::encode_push_request(
        &client,
        &rp_advert,
        &stale_updates,
        true,
        WireLimits::SHIPPED,
        PackWriteOpts::FAST,
        alloc,
        &mut stale_req,
    )
    .unwrap();
    let mut stale_resp = Vec::new_in(alloc);
    let stale_handled = ReceivePackServer::handle_push_planted(
        &stale_req,
        &mut server,
        WireLimits::SHIPPED,
        alloc,
        &mut stale_resp,
        plant,
    )
    .unwrap();
    assert_eq!(stale_handled.rejected_refs, 1, "stale CAS push must be rejected");
    assert_eq!(server.resolve_ref("refs/heads/main").unwrap(), Some(c2));

    // Now push with the right old OID (`c2 -> c3`)!
    let good_updates = [RefUpdate {
        name: String::from_str_in("refs/heads/main", alloc),
        old: Some(c2),
        new: c3,
    }];
    let mut push_req = Vec::new_in(alloc);
    WireClient::encode_push_request(
        &client,
        &rp_advert,
        &good_updates,
        true,
        WireLimits::SHIPPED,
        PackWriteOpts::FAST,
        alloc,
        &mut push_req,
    )
    .unwrap();
    let mut push_resp = Vec::new_in(alloc);
    let handled = ReceivePackServer::handle_push_planted(
        &push_req,
        &mut server,
        WireLimits::SHIPPED,
        alloc,
        &mut push_resp,
        plant,
    )
    .unwrap();
    assert_eq!(handled.updated_refs, 1);
    assert_eq!(server.resolve_ref("refs/heads/main").unwrap(), Some(c3));

    let report = WireClient::parse_push_response(&push_resp, true, alloc).unwrap();
    assert!(report.unpack_ok);
    assert_eq!(report.refs.len(), 1);
    assert!(report.refs[0].ok);

    // 4. Smart HTTP chunked request parsing & routing!
    let mut http_raw = Vec::new_in(alloc);
    http_raw.extend_from_slice(
        b"POST /repo.git/git-upload-pack HTTP/1.1\r\nHost: localhost\r\nGit-Protocol: version=2\r\nTransfer-Encoding: chunked\r\n\r\n",
    );
    let hex_len = std::format!("{:x}\r\n", ls_req.len());
    http_raw.extend_from_slice(hex_len.as_bytes());
    http_raw.extend_from_slice(&ls_req);
    http_raw.extend_from_slice(b"\r\n0\r\n\r\n");

    let parsed = parse_http_request(&http_raw, 1 << 20, alloc).unwrap();
    let req = match parsed {
        HttpParse::Complete { req, .. } => req,
        HttpParse::Incomplete => panic!("expected complete chunked http request"),
    };
    assert!(req.git_protocol_v2);
    let mut http_resp = Vec::new_in(alloc);
    handle_smart_http(&req, &mut server, WireLimits::SHIPPED, PackWriteOpts::FAST, alloc, &mut http_resp)
        .unwrap();
    assert!(http_resp.starts_with(b"HTTP/1.1 200 OK\r\n"));
}

#[test]
fn pack_wire_server_client_and_smart_http_work() {
    run_pack_and_wire_roundtrip(NoPlant);
}

#[test]
#[should_panic]
fn the_oracle_catches_skip_cas_old_oid_check() {
    run_pack_and_wire_roundtrip(GitEnginePlant::SkipCasOldOidCheck);
}

#[test]
#[should_panic]
fn the_oracle_catches_corrupt_pack_trailer() {
    run_pack_and_wire_roundtrip(GitEnginePlant::CorruptPackTrailer);
}

// ---------------------------------------------------------------------------
// 4. infect & disinfect full round-trip test
// ---------------------------------------------------------------------------

#[test]
fn infect_and_disinfect_preserve_every_object_and_ref() {
    let alloc = test_alloc();
    let mut orig = MemStore::new(alloc);
    let b1 = mk_blob(&mut orig, b"alpha\nbeta\ngamma\n");
    let t1 = mk_tree(&mut orig, &[TreeBuildEntry { mode: 0o100644, name: b"README.md", oid: b1 }], alloc);
    let c1 = mk_commit(&mut orig, t1, &[], 1_700_000_100, "initial\n", alloc);
    orig.write_head(&RefTarget::Symbolic(String::from_str_in("refs/heads/main", alloc))).unwrap();
    orig.set_ref("refs/heads/main", RefTarget::Direct(c1)).unwrap();

    // Disinfect `orig` to standard `.git` files, then `infect` those `.git` files into `.bgit`!
    let legacy = disinfect_from_store(&orig, PackWriteOpts::FAST, alloc).expect("disinfect");
    let mut builder = InfectBuilder::new(PackLimits::SHIPPED, alloc);
    builder.feed_packfile(&legacy.pack_bytes).expect("feed pack");
    builder.feed_head(legacy.head_bytes.as_str().as_bytes()).expect("feed head");
    builder.feed_packed_refs(legacy.packed_refs_bytes.as_str().as_bytes()).expect("feed refs");
    let infected = builder.finish(SlabCompression::AutoZstd).expect("finish infect");

    assert_eq!(infected.stats.unique_objects_out, 3);
    assert_eq!(infected.stats.commits_in_graph, 1);
    assert_eq!(infected.stats.refs_out, 1);

    // Verify the infected BGOB + BGIX + BGRF + CGPH read back identically!
    let idx = SlabIndex::parse(&infected.index_bytes, 1024).unwrap();
    let reader = SlabReader::parse(&infected.slab_bytes).unwrap();
    let mut out = Vec::new_in(alloc);
    let loc = idx.lookup(&b1).expect("blob in bgix");
    let (oid, kind) = reader.read_into(loc.offset, 1 << 20, alloc, &mut out).unwrap();
    assert_eq!(oid, b1);
    assert_eq!(kind, ObjectKind::Blob);
    assert_eq!(out.as_slice(), b"alpha\nbeta\ngamma\n");

    let rt = decode_reftable(&infected.reftable_bytes, 1024, alloc).unwrap();
    assert_eq!(rt.refs.len(), 1);
    assert_eq!(rt.refs[0].target, RefTarget::Direct(c1));

    let cg = CommitGraph::from_layers(&[&infected.commit_graph_bytes], alloc).unwrap();
    assert_eq!(cg.lookup(&c1).unwrap().tree, t1);

    // Also verify build_bgit_image_from_store matches!
    let direct = build_bgit_image_from_store(&orig, SlabCompression::RawOnly, alloc).unwrap();
    assert_eq!(direct.stats.unique_objects_out, 3);
}

#[test]
fn parallel_slab_records_and_chunked_pack_match_reference() {
    let alloc = test_alloc();
    let mut store = MemStore::new(alloc);
    let mut oids = Vec::new_in(alloc);
    for i in 0..24usize {
        let mut body = Vec::new_in(alloc);
        for _ in 0..20 {
            body.extend_from_slice(b"compressible line repeated for zstd + delta testing ");
            body.push(b'a' + (i % 26) as u8);
            body.push(b'\n');
        }
        oids.push(store.write_object(ObjectKind::Blob, &body).unwrap());
    }
    oids.sort_unstable();

    // 1. Standalone `encode_slab_record_with_oid` must match `SlabWriter` byte-for-byte!
    let mut seq_writer = SlabWriter::new(0, SlabCompression::AutoZstd, alloc);
    let mut par_bytes = Vec::new_in(alloc);
    crate::slab::write_slab_header(&mut par_bytes);
    let mut obj_buf = Vec::new_in(alloc);
    for &oid in &oids {
        let kind = store.read_object(&oid, &mut obj_buf).unwrap().unwrap();
        seq_writer.append_with_oid(oid, kind, &obj_buf).unwrap();
        crate::slab::encode_slab_record_with_oid(
            oid,
            kind,
            &obj_buf,
            SlabCompression::AutoZstd,
            alloc,
            &mut par_bytes,
        )
        .unwrap();
    }
    let (seq_bytes, seq_entries) = seq_writer.into_parts();
    assert_eq!(par_bytes.as_slice(), seq_bytes.as_slice());

    // `SlabIndex::parse_header_fast` agrees with full `SlabIndex::parse`.
    let mut idx_bytes = Vec::new_in(alloc);
    encode_slab_index(&seq_entries, alloc, &mut idx_bytes).unwrap();
    let full = SlabIndex::parse(&idx_bytes, 1024).unwrap();
    let fast = SlabIndex::parse_header_fast(&idx_bytes, 1024).unwrap();
    assert_eq!(full.len(), fast.len());
    for &oid in &oids {
        assert_eq!(full.lookup(&oid), fast.lookup(&oid));
    }

    // 2. Multi-chunk PACK v2 assembly unpacks and indexes identically!
    let mid = oids.len() / 2;
    let c1 = crate::pack_write::encode_pack_chunk(&store, &oids[..mid], PackWriteOpts::FAST, alloc)
        .unwrap();
    let c2 = crate::pack_write::encode_pack_chunk(&store, &oids[mid..], PackWriteOpts::FAST, alloc)
        .unwrap();
    let mut assembled_pack = Vec::new_in(alloc);
    let written = crate::pack_write::assemble_pack_from_chunks(
        &[(&c1.bytes, &c1.entries), (&c2.bytes, &c2.entries)],
        1024,
        alloc,
        &mut assembled_pack,
    )
    .unwrap();
    let mut pidx_bytes = Vec::new_in(alloc);
    write_pack_index_v2(&assembled_pack, &written, alloc, &mut pidx_bytes).unwrap();
    let pidx = Idx::parse(&pidx_bytes, 1024).unwrap();
    pidx.verify_checksum().unwrap();
    pidx.verify_ordering().unwrap();

    let mut dst = MemStore::new(alloc);
    let stats = crate::pack_write::unpack_pack_into_store(
        &assembled_pack,
        &mut dst,
        PackLimits::SHIPPED,
        alloc,
    )
    .unwrap();
    assert_eq!(stats.objects_unpacked as usize, oids.len());
    let mut a_buf = Vec::new_in(alloc);
    let mut b_buf = Vec::new_in(alloc);
    for &oid in &oids {
        assert_eq!(store.read_object(&oid, &mut a_buf).unwrap(), dst.read_object(&oid, &mut b_buf).unwrap());
        assert_eq!(a_buf.as_slice(), b_buf.as_slice());
    }
}

// ---------------------------------------------------------------------------
// 6. LazyVfs CoW overlay, O(dirty) status, write_tree, BGOV manifest, and NFSv3 wire protocol
// ---------------------------------------------------------------------------

#[test]
fn lazy_vfs_cow_status_write_tree_and_nfs3_protocol_roundtrip() {
    use crate::{
        FileStatus, UntrackedMode,
        nfs3::Nfs3Conn,
        vfs::{
            LazyVfs, ROOT_INO, VfsKind, VfsOpts, decode_overlay_status, encode_overlay_status,
        },
    };

    let alloc = test_alloc();
    let mut store = MemStore::new(alloc);

    let b_readme = mk_blob(&mut store, b"hello lazy vfs\n");
    let b_src = mk_blob(&mut store, b"fn main() {}\n");
    let b_untouched = mk_blob(&mut store, b"never hydrated\n");
    let b_link = mk_blob(&mut store, b"README.md");

    let t_src = mk_tree(
        &mut store,
        &[TreeBuildEntry { mode: 0o100644, name: b"main.rs", oid: b_src }],
        alloc,
    );
    let t_untouched = mk_tree(
        &mut store,
        &[TreeBuildEntry { mode: 0o100644, name: b"deep.txt", oid: b_untouched }],
        alloc,
    );
    let root_tree = mk_tree(
        &mut store,
        &[
            TreeBuildEntry { mode: 0o100644, name: b"README.md", oid: b_readme },
            TreeBuildEntry { mode: 0o120000, name: b"doc-link", oid: b_link },
            TreeBuildEntry { mode: 0o040000, name: b"src", oid: t_src },
            TreeBuildEntry { mode: 0o040000, name: b"untouched", oid: t_untouched },
        ],
        alloc,
    );

    // 1. O(1) lazy mount: 0 trees hydrated, 0 blobs read!
    let mut vfs = LazyVfs::new(Some(root_tree), VfsOpts::DEFAULT, alloc);
    assert_eq!(vfs.stats().trees_hydrated, 0);
    assert_eq!(vfs.stats().blobs_read_from_store, 0);

    // 2. Lookup + stat uses `object_size` without inflating blobs!
    let (_readme_ino, readme_attr) = vfs.lookup(ROOT_INO, "README.md", &store).unwrap();
    assert_eq!(readme_attr.kind, VfsKind::File);
    assert_eq!(readme_attr.size, 15);
    assert_eq!(vfs.stats().trees_hydrated, 1); // ONLY root hydrated; `src` and `untouched` untouched!
    assert_eq!(vfs.stats().blobs_read_from_store, 0);

    let (link_ino, link_attr) = vfs.lookup(ROOT_INO, "doc-link", &store).unwrap();
    assert_eq!(link_attr.kind, VfsKind::Symlink);
    let mut link_out = Vec::new_in(alloc);
    vfs.readlink(link_ino, &store, &mut link_out).unwrap();
    assert_eq!(link_out.as_slice(), b"README.md");

    // 3. Copy-on-Write edit, revert-to-clean check, create, mkdir, remove, rename.
    let (src_ino, _) = vfs.lookup(ROOT_INO, "src", &store).unwrap();
    let (main_ino, _) = vfs.lookup(src_ino, "main.rs", &store).unwrap();
    vfs.write(main_ino, 0, b"fn main() { 42 }\n", 1_700_000_100, &store).unwrap();
    let (fresh_ino, _) = vfs.create(ROOT_INO, "new.txt", 0o644, true, 1_700_000_101, &store).unwrap();
    vfs.write(fresh_ino, 0, b"brand new\n", 1_700_000_101, &store).unwrap();
    vfs.remove(ROOT_INO, "README.md", 1_700_000_102, &store).unwrap();

    // `untouched/` was NEVER hydrated — and `dirty_status` must NOT hydrate it!
    let hydrated_before = vfs.stats().trees_hydrated;
    assert_eq!(hydrated_before, 2); // root + src only!
    let (counts, entries) = vfs.dirty_status(UntrackedMode::Normal, &store, alloc).unwrap();
    assert_eq!(vfs.stats().trees_hydrated, hydrated_before);
    assert_eq!(counts.deleted, 1);
    assert_eq!(counts.modified, 1);
    assert_eq!(counts.untracked, 1);
    assert_eq!(entries.len(), 3);
    assert_eq!(entries[0].path.as_str(), "README.md");
    assert_eq!(entries[0].worktree, FileStatus::Deleted);
    assert_eq!(entries[1].path.as_str(), "new.txt");
    assert_eq!(entries[1].worktree, FileStatus::Untracked);
    assert_eq!(entries[2].path.as_str(), "src/main.rs");
    assert_eq!(entries[2].worktree, FileStatus::Modified);

    // 4. `BGOV` v1 manifest round-trip & CRC corruption refusal.
    let mut bgov = Vec::new_in(alloc);
    encode_overlay_status(
        vfs.overlay_generation(),
        Some("main"),
        Some(root_tree),
        counts,
        &entries,
        &mut bgov,
    );
    let decoded = decode_overlay_status(&bgov, alloc).unwrap();
    assert_eq!(decoded.branch.as_ref().map(|s| s.as_str()), Some("main"));
    assert_eq!(decoded.entries.len(), 3);
    // Flip a byte -> CRC-32 must reject!
    bgov[10] ^= 0xff;
    assert!(decode_overlay_status(&bgov, alloc).is_err());

    // 5. `write_tree` materializes ONLY dirty subtrees (reuses `t_untouched` OID directly!).
    let new_root = vfs.write_tree(&mut store).unwrap();
    assert_ne!(new_root, root_tree);
    let mut new_root_raw = Vec::new_in(alloc);
    assert_eq!(store.read_object(&new_root, &mut new_root_raw).unwrap(), Some(ObjectKind::Tree));
    let mut found_untouched_oid = None;
    let mut iter = crate::objects::TreeIter::new(new_root, &new_root_raw);
    while let Some(te) = iter.next_entry().unwrap() {
        if te.name == b"untouched" {
            found_untouched_oid = Some(te.oid);
        }
    }
    assert_eq!(found_untouched_oid, Some(t_untouched));

    // 6. SunRPC/XDR + MOUNTv3 + NFSv3 wire protocol round-trip over fragmented TCP (7-byte chunks).
    let mut conn = Nfs3Conn::new(alloc);
    let mut rpc_req = Vec::new_in(alloc);
    let mut push_call = |xid: u32, prog: u32, vers: u32, proc_id: u32, args: &[u8]| {
        let mut body = Vec::new_in(alloc);
        for w in [xid, 0u32, 2, prog, vers, proc_id, 0, 0, 0, 0] {
            body.extend_from_slice(&w.to_be_bytes());
        }
        body.extend_from_slice(args);
        let rm = 0x8000_0000u32 | (body.len() as u32);
        rpc_req.extend_from_slice(&rm.to_be_bytes());
        rpc_req.extend_from_slice(&body);
    };

    // Call 1: MOUNTv3 MNT("/")
    let mut mnt_args = Vec::new_in(alloc);
    mnt_args.extend_from_slice(&1u32.to_be_bytes());
    mnt_args.extend_from_slice(b"/\0\0\0");
    push_call(101, 100_005, 3, 1, &mnt_args);

    // Call 2: NFSv3 READ on `new.txt` (fresh_ino)
    let mut read_args = Vec::new_in(alloc);
    read_args.extend_from_slice(&8u32.to_be_bytes());
    read_args.extend_from_slice(&fresh_ino.to_be_bytes());
    read_args.extend_from_slice(&0u64.to_be_bytes()); // offset
    read_args.extend_from_slice(&64u32.to_be_bytes()); // count
    push_call(102, 100_003, 3, 6, &read_args);

    // Feed 7 bytes at a time to exercise TCP fragmentation!
    let mut replies = Vec::new_in(alloc);
    for chunk in rpc_req.chunks(7) {
        conn.feed_bytes(chunk);
        conn.poll(&mut vfs, &store, 1_700_000_200, &mut replies).unwrap();
    }
    // Parse both framed SunRPC replies and verify XIDs + payload bytes `"brand new\n"`.
    assert!(replies.len() > 40);
    let r1_len = (u32::from_be_bytes(replies[0..4].try_into().unwrap()) & 0x7fff_ffff) as usize;
    let r1 = &replies[4..4 + r1_len];
    assert_eq!(u32::from_be_bytes(r1[0..4].try_into().unwrap()), 101); // XID 101
    assert_eq!(u32::from_be_bytes(r1[20..24].try_into().unwrap()), 0); // ACCEPT_SUCCESS
    assert_eq!(u32::from_be_bytes(r1[24..28].try_into().unwrap()), 0); // MNT3_OK

    let r2_off = 4 + r1_len;
    let r2_len = (u32::from_be_bytes(replies[r2_off..r2_off + 4].try_into().unwrap()) & 0x7fff_ffff) as usize;
    let r2 = &replies[r2_off + 4..r2_off + 4 + r2_len];
    assert_eq!(u32::from_be_bytes(r2[0..4].try_into().unwrap()), 102); // XID 102
    assert_eq!(u32::from_be_bytes(r2[20..24].try_into().unwrap()), 0); // ACCEPT_SUCCESS
    assert_eq!(u32::from_be_bytes(r2[24..28].try_into().unwrap()), 0); // NFS3_OK
    assert!(r2.windows(10).any(|w| w == b"brand new\n"));
}

// ---------------------------------------------------------------------------
// 7. Worktree BGWT manifest, tracked-branch auto-merge/auto-delete, and FS 3-way planner
// ---------------------------------------------------------------------------

#[test]
fn worktree_bgwt_sync_auto_merge_auto_delete_and_fs_three_way_plan() {
    use crate::worktree::{
        DiskFile, FsAction, FsConflictKind, WorktreeRecord, WorktreeSyncDecision, decode_worktrees,
        encode_worktrees, evaluate_worktree_sync, plan_worktree_fs_update,
    };

    let alloc = test_alloc();
    let mut store = MemStore::new(alloc);

    let b_base = mk_blob(&mut store, b"line1\nline2\nline3\nline4\nline5\n");
    let b_keep = mk_blob(&mut store, b"untouched\n");
    let t_sub = mk_tree(
        &mut store,
        &[TreeBuildEntry { mode: 0o100644, name: b"keep.txt", oid: b_keep }],
        alloc,
    );
    let t0 = mk_tree(
        &mut store,
        &[
            TreeBuildEntry { mode: 0o100644, name: b"shared.txt", oid: b_base },
            TreeBuildEntry { mode: 0o040000, name: b"untouched", oid: t_sub },
        ],
        alloc,
    );
    let c0 = mk_commit(&mut store, t0, &[], 1_700_000_000, "base\n", alloc);
    store.set_ref("refs/heads/main", RefTarget::Direct(c0)).unwrap();
    store.set_ref("refs/heads/feat", RefTarget::Direct(c0)).unwrap();

    // 1. BGWT v1 manifest round-trip & CRC corruption rejection.
    let mut rec = WorktreeRecord {
        name: String::from_str_in("wt-feat", alloc),
        path: String::from_str_in("/tmp/wt-feat", alloc),
        branch: String::from_str_in("refs/heads/feat", alloc),
        track_branch: String::from_str_in("refs/heads/main", alloc),
        base_commit: c0,
        head_commit: c0,
        head_tree: t0,
        is_vfs: false,
        auto_delete_on_merge: true,
    };
    let mut bgwt = Vec::new_in(alloc);
    encode_worktrees(core::slice::from_ref(&rec), &mut bgwt);
    let decoded = decode_worktrees(&bgwt, 64, alloc).unwrap();
    assert_eq!(decoded.len(), 1);
    assert_eq!(decoded[0], rec);
    bgwt[12] ^= 0xff;
    assert!(decode_worktrees(&bgwt, 64, alloc).is_err());

    // 2. `main` advances to c1 while `feat` is still at `base_commit` -> must FastForward, NOT delete!
    let b_main_1 = mk_blob(&mut store, b"line1\nline2\nline3\nline4\nline5_from_main\n");
    let b_added = mk_blob(&mut store, b"new from main\n");
    let t1 = mk_tree(
        &mut store,
        &[
            TreeBuildEntry { mode: 0o100644, name: b"added.txt", oid: b_added },
            TreeBuildEntry { mode: 0o100644, name: b"shared.txt", oid: b_main_1 },
            TreeBuildEntry { mode: 0o040000, name: b"untouched", oid: t_sub },
        ],
        alloc,
    );
    let c1 = mk_commit(&mut store, t1, &[c0], 1_700_000_100, "main update\n", alloc);
    store.set_ref("refs/heads/main", RefTarget::Direct(c1)).unwrap();

    let d1 = evaluate_worktree_sync(&rec, &mut store, None, MergeOpts::DEFAULT, alloc).unwrap();
    assert!(matches!(d1, WorktreeSyncDecision::FastForward { new_commit, .. } if new_commit == c1));

    // 3. Filesystem-aware 3-way projection: local disk edited line1 of `shared.txt`, upstream edited line5!
    let mut disk_shared = Vec::new_in(alloc);
    disk_shared.extend_from_slice(b"line1_from_disk\nline2\nline3\nline4\nline5\n");
    let plan = plan_worktree_fs_update(
        &store,
        t0,
        t1,
        |rel| {
            if rel == "shared.txt" {
                let mut b = Vec::new_in(alloc);
                b.extend_from_slice(&disk_shared);
                Some(DiskFile { mode: 0o100644, bytes: b })
            } else {
                None
            }
        },
        MergeOpts::DEFAULT,
        alloc,
    )
    .unwrap();
    assert!(plan.is_clean());
    assert_eq!(plan.subtrees_skipped, 1); // `untouched/` skipped in O(1)!
    assert_eq!(plan.auto_merged_files, 1);
    assert_eq!(plan.actions.len(), 2);
    assert!(plan.actions.iter().any(|a| matches!(
        a,
        FsAction::WriteFile { path, bytes, .. }
            if path.as_str() == "shared.txt"
                && bytes.as_slice() == b"line1_from_disk\nline2\nline3\nline4\nline5_from_main\n"
    )));
    assert!(plan.actions.iter().any(|a| matches!(
        a,
        FsAction::WriteFile { path, .. } if path.as_str() == "added.txt"
    )));

    // 4. Conflicting local edit on line5 -> reports ContentConflict without clobbering!
    let plan_conflict = plan_worktree_fs_update(
        &store,
        t0,
        t1,
        |rel| {
            if rel == "shared.txt" {
                let mut b = Vec::new_in(alloc);
                b.extend_from_slice(b"line1\nline2\nline3\nline4\nline5_from_disk_conflict\n");
                Some(DiskFile { mode: 0o100644, bytes: b })
            } else {
                None
            }
        },
        MergeOpts::DEFAULT,
        alloc,
    )
    .unwrap();
    assert!(!plan_conflict.is_clean());
    assert_eq!(plan_conflict.conflicts.len(), 1);
    assert_eq!(plan_conflict.conflicts[0].kind, FsConflictKind::ContentConflict);

    // 5. `feat` makes its own commit c2, then `main` merges `feat` in c3 -> MergedIntoTracked (auto-delete)!
    let b_feat = mk_blob(&mut store, b"feat work\n");
    let t_feat = mk_tree(
        &mut store,
        &[
            TreeBuildEntry { mode: 0o100644, name: b"feat.txt", oid: b_feat },
            TreeBuildEntry { mode: 0o100644, name: b"shared.txt", oid: b_base },
            TreeBuildEntry { mode: 0o040000, name: b"untouched", oid: t_sub },
        ],
        alloc,
    );
    let c2 = mk_commit(&mut store, t_feat, &[c0], 1_700_000_200, "feat commit\n", alloc);
    store.set_ref("refs/heads/feat", RefTarget::Direct(c2)).unwrap();
    rec.head_commit = c2;
    rec.head_tree = t_feat;

    // While diverged from c1, `evaluate_worktree_sync` returns `AutoMerge`!
    let d_div = evaluate_worktree_sync(&rec, &mut store, None, MergeOpts::DEFAULT, alloc).unwrap();
    assert!(matches!(
        d_div,
        WorktreeSyncDecision::AutoMerge { status: MergeStatus::MergedClean, .. }
    ));

    // Now merge `feat` (c2) into `main` (c1) -> c3!
    let c3 = mk_commit(&mut store, t1, &[c1, c2], 1_700_000_300, "merge feat into main\n", alloc);
    store.set_ref("refs/heads/main", RefTarget::Direct(c3)).unwrap();
    let d_merged = evaluate_worktree_sync(&rec, &mut store, None, MergeOpts::DEFAULT, alloc).unwrap();
    assert!(matches!(
        d_merged,
        WorktreeSyncDecision::MergedIntoTracked { branch_oid, track_oid }
            if branch_oid == c2 && track_oid == c3
    ));

    // Fast-forward merge: `main` fast-forwards directly to `c2` (so `branch_oid == track_oid != base_commit`) -> MergedIntoTracked!
    store.set_ref("refs/heads/main", RefTarget::Direct(c2)).unwrap();
    let d_ff_merged = evaluate_worktree_sync(&rec, &mut store, None, MergeOpts::DEFAULT, alloc).unwrap();
    assert!(matches!(
        d_ff_merged,
        WorktreeSyncDecision::MergedIntoTracked { branch_oid, track_oid }
            if branch_oid == c2 && track_oid == c2
    ));

    // Brand-new branch with 0 commits (`branch_oid == track_oid == base_commit`) -> UpToDate (never falsely deleted!).
    rec.base_commit = c2;
    let d_fresh = evaluate_worktree_sync(&rec, &mut store, None, MergeOpts::DEFAULT, alloc).unwrap();
    assert!(matches!(d_fresh, WorktreeSyncDecision::UpToDate));
}
