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
        format!("blaze tree 2\nlink {hex} 1 A\nfile {hex} 1 a/b\n"),
        format!("blaze tree 2\nfile {hex} 1 README\nfile {hex} 1 Readme\n"),
        format!("blaze tree 2\nfile {hex} 01 a\n"),
        format!("blaze tree 2\nfile {hex} a\n"),
    ] {
        assert!(tree::parse(bad.as_bytes()).is_err(), "{bad:?}");
    }
    assert_eq!(tree::parse(format!("blaze tree 2\nfile {hex} 1 a b\n").as_bytes()).unwrap()[0].path, "a b");
}
