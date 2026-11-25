//! The crate's tests: the digests and listing kept from the old crate, the record form, and the
//! fetch machine against a simulated origin (`sim`).

use crate::digest::{Algo, Blake3, Digest, DigestRefused};
use crate::tree::{self, Entry, Kind};

mod index;
mod local_step;
mod peer_race;
mod sim;

#[test]
fn digests_parse_and_print_as_algo_colon_hex() {
    let sha256 = Digest::of(Algo::Sha256, b"abc");
    assert_eq!(sha256.to_string(), "sha256:ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
    assert_eq!(Digest::parse(&sha256.to_string()), Ok(sha256));
    assert_eq!(Digest::parse(&sha256.to_string().to_uppercase().replace("SHA256", "sha256")), Ok(sha256));
    let sha512 = Digest::of(Algo::Sha512, b"abc");
    assert!(sha512.to_string().starts_with("sha512:ddaf35a193617aba"));
    assert_eq!(Digest::parse(&sha512.to_string()), Ok(sha512));
    let blake3 = Digest::of(Algo::Blake3, b"abc");
    assert_eq!(blake3.to_string(), format!("blake3:{}", Blake3::of(b"abc").hex()));
    assert_eq!(Digest::parse(&blake3.to_string()), Ok(blake3));
}

#[test]
fn weak_unknown_and_malformed_digests_are_refused_by_name() {
    let refused = |text: &str| Digest::parse(text).unwrap_err();
    assert!(
        matches!(refused("md5:900150983cd24fb0d6963f7d28e17f72"), DigestRefused::Weak { algo, .. } if algo == "md5")
    );
    assert!(matches!(refused("sha1:a9993e364706816aba3e25717850c26c9cd0d89d"), DigestRefused::Weak { .. }));
    assert!(matches!(refused("SHA-1:a9993e364706816aba3e25717850c26c9cd0d89d"), DigestRefused::Weak { .. }));
    assert!(matches!(refused("crc32:352441c2"), DigestRefused::Unknown { algo, .. } if algo == "crc32"));
    assert!(matches!(refused("ba7816bf"), DigestRefused::NoAlgo { .. }));
    assert!(matches!(refused("sha256:zz"), DigestRefused::NotHex { algo: Algo::Sha256, .. }));
    assert!(matches!(refused("sha256:ba78"), DigestRefused::Length { digits: 4, .. }));
    assert!(refused("md5:00").to_string().contains("weak"));
}

#[test]
fn a_listing_is_sorted_and_refuses_escapes() {
    let entry = |path: &str, kind| Entry { path: path.to_owned(), kind, blake3: Blake3::of(path.as_bytes()), len: path.len() as u64 };
    let mut one = [entry("b/c", Kind::File), entry("a", Kind::Exec), entry("b/a", Kind::Link)];
    let mut two = [entry("b/a", Kind::Link), entry("b/c", Kind::File), entry("a", Kind::Exec)];
    let listing = tree::listing(&mut one).unwrap();
    assert_eq!(listing, tree::listing(&mut two).unwrap());
    let text = String::from_utf8(listing.clone()).unwrap();
    let paths: Vec<_> = text.lines().skip(1).map(|it| it.rsplit(' ').next().unwrap()).collect();
    assert_eq!(paths, ["a", "b/a", "b/c"]);
    assert_eq!(tree::parse(&listing).unwrap().len(), 3);
    for bad in ["../x", "/x", "a//b", ".git/config", "a\nb"] {
        assert!(tree::listing(&mut [entry(bad, Kind::File)]).is_err(), "{bad:?}");
    }
    assert!(tree::listing(&mut [entry("a", Kind::File), entry("a", Kind::File)]).is_err());
    assert!(tree::listing(&mut [entry("a", Kind::File), entry("a/b", Kind::File)]).is_err());
    // Case aside, as a case-insensitive disk sees them: `A` a link and `a/b` written through it.
    assert!(tree::listing(&mut [entry("A", Kind::Link), entry("a/b", Kind::File)]).is_err());
    assert!(tree::listing(&mut [entry("Readme", Kind::File), entry("README", Kind::File)]).is_err());
    assert!(tree::listing(&mut [entry("x/.GIT/config", Kind::File)]).is_err());
    // One directory spelled two ways is one directory on a case-insensitive disk, ASCII or not,
    // and in either Unicode normal form.
    let twice = |a: &str, b: &str| {
        matches!(
            tree::listing(&mut [entry(a, Kind::File), entry(b, Kind::File)]),
            Err(tree::TreeRefused::Twice { .. })
        )
    };
    assert!(twice("Dir/a", "dir/b"));
    assert!(twice("src/Ä/a", "src/ä/b"));
    assert!(twice("\u{e9}/a", "e\u{301}/b"));
    assert!(twice("\u{c9}", "e\u{301}"));
    assert!(twice("\u{3a3}", "\u{3c2}"));
    assert!(twice("a", "a"));
    assert!(!twice("dir/a", "dir/b"));
    assert!(!twice("a/x", "b/x"));
    assert!(matches!(
        tree::listing(&mut [entry("\u{c4}", Kind::Link), entry("\u{e4}/b", Kind::File)]),
        Err(tree::TreeRefused::Under { .. })
    ));
    // A listing no `listing` writes is refused on read, the same rules over it.
    let hex = Blake3::of(b"x").hex();
    for bad in [
        format!("blaze tree 2\nfile {hex} 1 a\n"),
        format!("blaze tree 3\nlink {hex} 1 A\nfile {hex} 1 a/b\n"),
        format!("blaze tree 3\nfile {hex} 1 README\nfile {hex} 1 Readme\n"),
        format!("blaze tree 3\nfile {hex} 01 a\n"),
        format!("blaze tree 3\nfile {hex} a\n"),
    ] {
        assert!(tree::parse(bad.as_bytes()).is_err(), "{bad:?}");
    }
    assert_eq!(tree::parse(format!("blaze tree 3\nfile {hex} 1 a b\n").as_bytes()).unwrap()[0].path, "a b");
    let sub_parsed = tree::parse(format!("blaze tree 3\nsubmodule {hex} 20 third_party/v8\n").as_bytes()).unwrap();
    assert_eq!(sub_parsed.len(), 1);
    assert_eq!(sub_parsed[0].kind, Kind::Submodule);
    assert_eq!(sub_parsed[0].path, "third_party/v8");
}

#[test]
fn previous_magic_is_refused() {
    let alloc = blaze_own::testing::heap().alloc();
    let mut b = crate::pack::Builder::new(
        crate::pack::Kind::Tree,
        [0u8; 32],
        crate::pack::Limits { most_files: 16, most_bytes: 1 << 20, most_path: 256 },
        64,
        alloc,
    )
    .expect("builder");
    b.file("src/lib.rs", crate::Mode::File, b"pub fn hi() {}\n").expect("add file");
    let mut record = b.finish().expect("finish");
    assert_eq!(&record.as_slice()[..8], crate::pack::MAGIC);
    // Rewriting header magic to previous BLZFPK04 (with valid BLAKE3 footer) must be refused.
    record.as_mut_slice()[..8].copy_from_slice(b"BLZFPK04");
    let body_len = record.len() - crate::pack::FOOTER;
    let new_digest = Blake3::of(&record.as_slice()[..body_len]).0;
    record.as_mut_slice()[body_len..].copy_from_slice(&new_digest);
    assert!(matches!(crate::PackView::open(record.as_slice()), Err(crate::PackRefused::Form { .. })));
    assert!(matches!(crate::PackView::open_verified(record.as_slice()), Err(crate::PackRefused::Form { .. })));
}

#[test]
fn blzfpk04_compresses_small_and_large_rust_files_and_round_trips() {
    let alloc = blaze_own::testing::heap().alloc();
    let small_rs = concat!(
        "// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or\n",
        "// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license\n",
        "#![no_std]\nuse core::fmt::{self, Debug, Display};\n",
        "#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]\n",
        "pub struct Point { pub x: i32, pub y: i32 }\n",
        "impl fmt::Display for Point {\n",
        "    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { write!(f, \"{},{}\", self.x, self.y) }\n",
        "}\n",
    );
    let mut large_rs = blaze_own::Vec::new_in(alloc);
    for i in 0..900u32 {
        let line = format!("pub const fn f_{i}(x: u64) -> u64 {{ x.wrapping_add({i}) }}\n");
        large_rs.extend_from_slice(line.as_bytes());
    }
    let tiny = b"tiny\n";

    let mut b = crate::pack::Builder::new(
        crate::pack::Kind::Tree,
        [7u8; 32],
        crate::pack::Limits { most_files: 16, most_bytes: 4 << 20, most_path: 256 },
        (small_rs.len() + large_rs.len() + tiny.len()) as u64,
        alloc,
    )
    .expect("builder");
    b.file("src/small.rs", crate::Mode::File, small_rs.as_bytes()).expect("small");
    b.file("src/large.rs", crate::Mode::File, large_rs.as_slice()).expect("large");
    b.file("src/tiny.txt", crate::Mode::File, tiny).expect("tiny");
    let record = b.finish().expect("finish");

    let view = crate::PackView::open(record.as_slice()).expect("open");
    assert_eq!(view.len(), 3);
    assert_eq!(view.find_entry("src/small.rs").expect("small entry").codec, crate::Codec::ZstdRustDict);
    assert_eq!(view.find_entry("src/large.rs").expect("large entry").codec, crate::Codec::Zstd);
    assert_eq!(view.find_entry("src/tiny.txt").expect("tiny entry").codec, crate::Codec::Raw);
    assert!(view.data_len() < view.raw_data_len() / 3, "expected >3x compression: stored={} raw={}", view.data_len(), view.raw_data_len());

    assert_eq!(view.read("src/small.rs", alloc).expect("read small").expect("some").bytes.as_slice(), small_rs.as_bytes());
    assert_eq!(view.read("src/large.rs", alloc).expect("read large").expect("some").bytes.as_slice(), large_rs.as_slice());
    assert_eq!(view.read("src/tiny.txt", alloc).expect("read tiny").expect("some").bytes.as_slice(), tiny);
}

#[test]
fn submodule_entries_commit_to_tree_digest_and_fail_closed_on_reads() {
    let alloc = blaze_own::testing::heap().alloc();
    let limits = crate::pack::Limits { most_files: 16, most_bytes: 1 << 20, most_path: 256 };
    let oid_a = [0x11u8; 20];
    let oid_b = [0x22u8; 20];

    let build = |oid: [u8; 20]| {
        let mut b = crate::pack::Builder::new(crate::pack::Kind::Git, [9u8; 32], limits, 128, alloc).expect("builder");
        b.file("Cargo.toml", crate::Mode::File, b"[package]\nname = \"v8\"\n").expect("manifest");
        b.file("src/lib.rs", crate::Mode::File, b"pub fn ok() {}\n").expect("lib");
        b.file("v8", crate::Mode::Submodule, &oid).expect("submodule");
        b.file("third_party/icu", crate::Mode::Submodule, &oid).expect("nested submodule");
        b.finish().expect("finish")
    };

    let rec_a = build(oid_a);
    let rec_b = build(oid_b);
    let view_a = crate::PackView::open(rec_a.as_slice()).expect("open a");
    let view_b = crate::PackView::open(rec_b.as_slice()).expect("open b");
    assert_ne!(view_a.tree(), view_b.tree(), "changing a submodule commit OID must change the tree listing digest");

    // Normal files still read cleanly; prefix-similar non-submodule paths return Ok(None).
    assert_eq!(view_a.read("src/lib.rs", alloc).expect("read lib").expect("some").bytes.as_slice(), b"pub fn ok() {}\n");
    assert!(view_a.read("v8_extra/foo.rs", alloc).expect("not under v8").is_none());
    assert!(view_a.read("missing.rs", alloc).expect("missing").is_none());

    // Metadata lookup exposes the submodule's pinned commit OID cleanly.
    let sub_entry = view_a.find_entry("v8").expect("v8 entry");
    assert_eq!(sub_entry.mode, crate::Mode::Submodule);
    assert_eq!(sub_entry.codec, crate::Codec::Raw);
    assert_eq!(sub_entry.submodule_commit(), Some(oid_a));

    // Reads at or beneath a recorded submodule fail closed naming path, submodule, and commit OID.
    for (bad_path, want_sub) in [("v8", "v8"), ("v8/src/lib.rs", "v8"), ("third_party/icu/Cargo.toml", "third_party/icu")] {
        let err = view_a.read(bad_path, alloc).expect_err("must fail closed");
        assert_eq!(
            err,
            crate::PackRefused::Submodule {
                path: crate::unpack::Cut::of(bad_path.as_bytes()),
                submodule: crate::unpack::Cut::of(want_sub.as_bytes()),
                commit: oid_a,
            }
        );
    }
    let idx = view_a.find_index("v8").expect("v8 index");
    assert!(matches!(view_a.read_at(idx, alloc), Err(crate::PackRefused::Submodule { .. })));

    // Submodule payload that is not 20 bytes is refused.
    let mut bad = crate::pack::Builder::new(crate::pack::Kind::Git, [9u8; 32], limits, 64, alloc).expect("builder");
    bad.file("v8", crate::Mode::Submodule, b"short").expect("begin");
    assert!(matches!(bad.finish(), Err(crate::PackRefused::Form { .. })));

    // Tree listing and parse reject non-20-byte submodule entries.
    let mut bad_entries = [crate::tree::Entry {
        path: alloc::string::String::from("v8"),
        kind: crate::tree::Kind::Submodule,
        blake3: crate::Blake3::of(&oid_a),
        len: 1,
    }];
    assert!(matches!(
        crate::tree::listing(&mut bad_entries),
        Err(crate::tree::TreeRefused::Path { .. })
    ));
    let bad_listing = alloc::format!("blaze tree 3\nsubmodule {} 1 v8\n", crate::Blake3::of(&oid_a).hex());
    assert!(matches!(
        crate::tree::parse(bad_listing.as_bytes()),
        Err(crate::tree::TreeRefused::Line { line: 2, .. })
    ));
}
