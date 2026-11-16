//! The crate's promises, each with its oracle and a planted bug:
//!
//! - SHA-1 is FIPS 180-4's, and an object id is git's;
//! - a pack built here (whole objects, offset deltas, reference deltas before their bases)
//!   reads back to the files it was built from;
//! - a corrupted pack, a missing base and an oversized object are refused;
//! - a tree name that could leave the checkout is refused;
//! - the protocol's requests are v2's bytes, and its answers are read as v2 says.

use std::{
    alloc::Global,
    collections::BTreeMap,
    format,
    string::{String, ToString},
    vec,
    vec::Vec as StdVec,
};

use blaze_own::Vec;
use blaze_sim::{Broke, Config, Tape, check, control};

use crate::{
    Kind, Mode, Oid, Pack, PackLimits, PackWhy, Pkt, TreeLimits, TreeWhy, check_advertisement, fetch_request,
    fetched_pack, files, find_ref, ls_refs,
    plant::{self, Plant},
    sha1::{Sha1, sha1},
};

const PACK: PackLimits = PackLimits { most_objects: 10_000, most_object: 1 << 24, most_total: 1 << 26, most_depth: 64 };
const TREE: TreeLimits = TreeLimits { most_files: 10_000, most_path: 4096, most_depth: 64 };

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[test]
fn sha1_known_answers() {
    assert_eq!(hex(&sha1(b"")), "da39a3ee5e6b4b0d3255bfef95601890afd80709");
    assert_eq!(hex(&sha1(b"abc")), "a9993e364706816aba3e25717850c26c9cd0d89d");
    assert_eq!(
        hex(&sha1(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq")),
        "84983e441c3bd26ebaae4aa1f95129e5e54670f1"
    );
    let million = vec![b'a'; 1_000_000];
    assert_eq!(hex(&sha1(&million)), "34aa973cd4c4daa4f61eeb2bdbad27316534016f");
    // Fed in odd pieces, the same.
    let mut h = Sha1::new();
    for piece in million.chunks(1013) {
        h.update(piece);
    }
    assert_eq!(hex(&h.finish()), "34aa973cd4c4daa4f61eeb2bdbad27316534016f");
    // `git hash-object` of a file holding "hello world\n".
    assert_eq!(Oid::of(Kind::Blob, b"hello world\n").to_string(), "3b18e512dba79e4c8300dd08aeb37f8e728b8dad");
    assert_eq!(Oid::of(Kind::Tree, b"").to_string(), "4b825dc642cb6eb9a060e54bf8d69288fbee4904");
}

// ---- a pack writer, for the tests only ----

fn zlib(bytes: &[u8]) -> StdVec<u8> {
    let mut out = Vec::new_in(Global);
    blaze_deflate::zlib(bytes, blaze_deflate::Level::Default, Global, &mut out).expect("deflated");
    out.as_slice().to_vec()
}

fn entry_header(number: u8, size: usize, out: &mut StdVec<u8>) {
    let mut size = size as u64;
    let mut byte = (number << 4) | (size & 15) as u8;
    size >>= 4;
    while size > 0 {
        out.push(byte | 0x80);
        byte = (size & 0x7f) as u8;
        size >>= 7;
    }
    out.push(byte);
}

fn ofs(back: u64, out: &mut StdVec<u8>) {
    let mut bytes = vec![(back & 0x7f) as u8];
    let mut n = back >> 7;
    while n > 0 {
        n -= 1;
        bytes.push(0x80 | (n & 0x7f) as u8);
        n >>= 7;
    }
    bytes.reverse();
    out.extend_from_slice(&bytes);
}

fn varint(mut n: usize, out: &mut StdVec<u8>) {
    while n >= 0x80 {
        out.push((n & 0x7f) as u8 | 0x80);
        n >>= 7;
    }
    out.push(n as u8);
}

#[derive(Clone, Debug)]
enum Op {
    Copy(usize, usize),
    Insert(StdVec<u8>),
}

fn delta(base: &[u8], ops: &[Op]) -> (StdVec<u8>, StdVec<u8>) {
    let mut target = StdVec::new();
    let mut body = StdVec::new();
    for op in ops {
        match op {
            Op::Copy(off, len) => {
                target.extend_from_slice(&base[*off..off + len]);
                let mut cmd = 0x80u8;
                let mut args = StdVec::new();
                for i in 0..4 {
                    let b = (off >> (8 * i)) as u8;
                    if b != 0 {
                        cmd |= 1 << i;
                        args.push(b);
                    }
                }
                for i in 0..3 {
                    let b = (len >> (8 * i)) as u8;
                    if b != 0 {
                        cmd |= 0x10 << i;
                        args.push(b);
                    }
                }
                body.push(cmd);
                body.extend_from_slice(&args);
            }
            Op::Insert(bytes) => {
                target.extend_from_slice(bytes);
                body.push(bytes.len() as u8);
                body.extend_from_slice(bytes);
            }
        }
    }
    let mut out = StdVec::new();
    varint(base.len(), &mut out);
    varint(target.len(), &mut out);
    out.extend_from_slice(&body);
    (out, target)
}

#[derive(Clone, Copy, Debug)]
enum How {
    Whole,
    Ofs(usize),
    Ref(usize),
}

/// A pack of `objects` (kind, bytes, how), reference deltas listed first when `early`.
fn write_pack(objects: &[(Kind, StdVec<u8>, How)], deltas: &[StdVec<u8>], early: &[bool]) -> StdVec<u8> {
    let number = |k: Kind| match k {
        Kind::Commit => 1,
        Kind::Tree => 2,
        Kind::Blob => 3,
        Kind::Tag => 4,
    };
    let mut order: StdVec<usize> = (0..objects.len()).filter(|&i| early[i]).collect();
    order.extend((0..objects.len()).filter(|&i| !early[i]));
    let mut out = StdVec::from(&b"PACK"[..]);
    out.extend_from_slice(&2u32.to_be_bytes());
    out.extend_from_slice(&(objects.len() as u32).to_be_bytes());
    let mut at = vec![0u64; objects.len()];
    for i in order {
        at[i] = out.len() as u64;
        let (kind, bytes, how) = &objects[i];
        match how {
            How::Whole => {
                entry_header(number(*kind), bytes.len(), &mut out);
                out.extend_from_slice(&zlib(bytes));
            }
            How::Ofs(base) => {
                entry_header(6, deltas[i].len(), &mut out);
                ofs(at[i] - at[*base], &mut out);
                out.extend_from_slice(&zlib(&deltas[i]));
            }
            How::Ref(base) => {
                entry_header(7, deltas[i].len(), &mut out);
                out.extend_from_slice(&Oid::of(objects[*base].0, &objects[*base].1).0);
                out.extend_from_slice(&zlib(&deltas[i]));
            }
        }
    }
    let trailer = sha1(&out);
    out.extend_from_slice(&trailer);
    out
}

fn tree_bytes(entries: &mut StdVec<(String, u32, Oid)>) -> StdVec<u8> {
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    let mut out = StdVec::new();
    for (name, mode, oid) in entries.iter() {
        out.extend_from_slice(format!("{mode:o} {name}\0").as_bytes());
        out.extend_from_slice(&oid.0);
    }
    out
}

struct Built {
    pack: StdVec<u8>,
    commit: Oid,
    files: BTreeMap<String, (Mode, StdVec<u8>)>,
}

const DIRS: [&str; 4] = ["", "src", "src/bin", "docs"];

/// A random commit: files in nested directories, many blobs deltas of earlier ones.
fn build(tape: &Tape) -> Built {
    let n = tape.int_in(1usize..=10, "files");
    let mut objects: StdVec<(Kind, StdVec<u8>, How)> = StdVec::new();
    let mut deltas: StdVec<StdVec<u8>> = StdVec::new();
    let mut early = StdVec::new();
    let mut files = BTreeMap::new();
    let mut dirs: BTreeMap<&str, StdVec<(String, u32, Oid)>> = BTreeMap::new();
    for i in 0..n {
        let dir = DIRS[tape.index(DIRS.len(), "dir")];
        let name = format!("f{i}.rs");
        let path = if dir.is_empty() { name.clone() } else { format!("{dir}/{name}") };
        let blobs: StdVec<usize> = (0..objects.len()).collect();
        let (content, how, d) = match blobs.is_empty() || !tape.coin("delta") {
            true => (tape.bytes(0..=600, "content"), How::Whole, StdVec::new()),
            false => {
                let base = blobs[tape.index(blobs.len(), "base")];
                let base_bytes = objects[base].1.clone();
                let ops = tape.vec(1..=8, "ops", |t| {
                    if !base_bytes.is_empty() && t.coin("copy") {
                        let off = t.index(base_bytes.len(), "offset");
                        let len = t.int_in(1..=(base_bytes.len() - off).min(300), "len");
                        Op::Copy(off, len)
                    } else {
                        Op::Insert(t.bytes(1..=100, "insert"))
                    }
                });
                let (d, target) = delta(&base_bytes, &ops);
                let how = if tape.coin("by ref") { How::Ref(base) } else { How::Ofs(base) };
                (target, how, d)
            }
        };
        early.push(matches!(how, How::Ref(_)) && tape.coin("early"));
        let mode = match tape.index(3, "mode") {
            0 => (Mode::File, 0o100644),
            1 => (Mode::Exec, 0o100755),
            _ => (Mode::Symlink, 0o120000),
        };
        let oid = Oid::of(Kind::Blob, &content);
        dirs.entry(dir).or_default().push((name, mode.1, oid));
        files.insert(path, (mode.0, content.clone()));
        objects.push((Kind::Blob, content, how));
        deltas.push(d);
    }
    // Trees, deepest first.
    let mut tree_of: BTreeMap<&str, Oid> = BTreeMap::new();
    for dir in ["src/bin", "docs", "src", ""] {
        let mut entries = dirs.remove(dir).unwrap_or_default();
        for (child, name) in [("src/bin", "bin"), ("src", "src"), ("docs", "docs")] {
            let parent = child.rsplit_once('/').map_or("", |(p, _)| p);
            if parent == dir
                && let Some(&oid) = tree_of.get(child)
            {
                entries.push((name.to_string(), 0o040000, oid));
            }
        }
        if entries.is_empty() && !dir.is_empty() {
            continue;
        }
        let bytes = tree_bytes(&mut entries);
        tree_of.insert(dir, Oid::of(Kind::Tree, &bytes));
        objects.push((Kind::Tree, bytes, How::Whole));
        deltas.push(StdVec::new());
        early.push(false);
    }
    let commit = format!("tree {}\nauthor a <a> 0 +0000\ncommitter a <a> 0 +0000\n\nm\n", tree_of[""]).into_bytes();
    let commit_oid = Oid::of(Kind::Commit, &commit);
    objects.push((Kind::Commit, commit, How::Whole));
    deltas.push(StdVec::new());
    early.push(false);
    Built { pack: write_pack(&objects, &deltas, &early), commit: commit_oid, files }
}

fn read_back(built: &Built) -> Result<BTreeMap<String, (Mode, StdVec<u8>)>, String> {
    let pack = Pack::read_in(&built.pack, PACK, Global).map_err(|e| e.to_string())?;
    let mut got = BTreeMap::new();
    files(&pack, built.commit, TREE, Global, |file| {
        got.insert(file.path.to_string(), (file.mode, file.bytes.to_vec()));
    })
    .map_err(|e| e.to_string())?;
    Ok(got)
}

fn round_trip(tape: &Tape) -> Result<(), Broke> {
    let built = build(tape);
    match read_back(&built) {
        Ok(got) if got == built.files => Ok(()),
        Ok(_) => Err(Broke::Oracle(String::from("the files read back differ from those packed"))),
        Err(e) => Err(Broke::Oracle(format!("the files read back differ: refused: {e}"))),
    }
}

#[test]
fn a_pack_with_deltas_reads_back_to_its_files() {
    check(Config::cases(64), round_trip);
}

#[test]
fn the_oracle_catches_a_copy_one_byte_late() {
    control(Config::cases(64), Plant::Clean, Plant::CopyOffsetOffByOne, "differ", |tape, p| {
        plant::with(p, || round_trip(tape))
    });
}

fn corruption(tape: &Tape) -> Result<(), Broke> {
    let mut built = build(tape);
    // A byte flipped in an entry, past the header: the trailer must catch it.
    let at = 12 + tape.index(built.pack.len() - 32, "byte");
    built.pack[at] ^= 1 << tape.index(8, "bit");
    match Pack::read_in(&built.pack, PACK, Global) {
        Err(e) if e.why == PackWhy::Checksum => Ok(()),
        _ => Err(Broke::Oracle(format!("a byte flipped at {at} was not refused by the trailer"))),
    }
}

#[test]
fn a_corrupted_pack_is_refused() {
    check(Config::cases(32), corruption);
}

#[test]
fn the_oracle_catches_an_unchecked_trailer() {
    control(Config::cases(32), Plant::Clean, Plant::TrailerUnchecked, "not refused", |tape, p| {
        plant::with(p, || corruption(tape))
    });
}

#[test]
fn a_missing_base_and_an_oversized_object_are_refused() {
    let base = b"the base".to_vec();
    let (d, target) = delta(&base, &[Op::Copy(0, 3), Op::Insert(b"!".to_vec())]);
    // The delta's base is not in the pack: a thin pack.
    let objects = vec![(Kind::Blob, base, How::Whole), (Kind::Blob, target, How::Ref(0))];
    let full = write_pack(&objects, &[StdVec::new(), d], &[false, false]);
    let alone = {
        let mut out = StdVec::from(&full[..8]);
        out.extend_from_slice(&1u32.to_be_bytes());
        // The first entry: a one-byte header (blob, size 8) and its zlib stream.
        let first = 12 + 1 + zlib(b"the base").len();
        out.extend_from_slice(&full[first..full.len() - 20]);
        let trailer = sha1(&out);
        out.extend_from_slice(&trailer);
        out
    };
    let refused = Pack::read_in(&alone, PACK, Global).map(|_| ()).map_err(|e| e.why);
    assert_eq!(refused, Err(PackWhy::MissingBase { base: Oid::of(Kind::Blob, b"the base") }));
    let small = PackLimits { most_object: 4, ..PACK };
    let refused = Pack::read_in(&full, small, Global).map(|_| ()).map_err(|e| e.why);
    assert_eq!(refused, Err(PackWhy::ObjectOver { size: 8, most: 4 }));
    let ok = Pack::read_in(&full, PACK, Global).expect("read");
    assert_eq!(ok.get(&Oid::of(Kind::Blob, b"the!")).map(|o| o.bytes), Some(&b"the!"[..]));
}

fn one_entry_tree(name: &[u8], mode: u32) -> (StdVec<u8>, Oid) {
    let blob = b"x".to_vec();
    let mut tree = format!("{mode:o} ").into_bytes();
    tree.extend_from_slice(name);
    tree.push(0);
    tree.extend_from_slice(&Oid::of(Kind::Blob, &blob).0);
    let commit = format!("tree {}\n\nm\n", Oid::of(Kind::Tree, &tree)).into_bytes();
    let oid = Oid::of(Kind::Commit, &commit);
    let objects =
        vec![(Kind::Blob, blob, How::Whole), (Kind::Tree, tree, How::Whole), (Kind::Commit, commit, How::Whole)];
    (write_pack(&objects, &[StdVec::new(), StdVec::new(), StdVec::new()], &[false; 3]), oid)
}

fn walk(name: &[u8], mode: u32) -> Result<usize, TreeWhy> {
    let (bytes, commit) = one_entry_tree(name, mode);
    let pack = Pack::read_in(&bytes, PACK, Global).expect("read");
    let mut n = 0;
    files(&pack, commit, TREE, Global, |_| n += 1).map_err(|e| e.why)?;
    Ok(n)
}

#[test]
fn names_that_leave_the_checkout_are_refused() {
    assert_eq!(walk(b"ok.rs", 0o100644), Ok(1));
    for name in [&b".."[..], b".", b".git", b".GIT", b"a\\b", b"", b"\xff"] {
        assert_eq!(walk(name, 0o100644), Err(TreeWhy::Name), "{name:?}");
    }
    assert_eq!(walk(b"sub", 0o160000), Err(TreeWhy::Submodule));
    assert_eq!(walk(b"odd", 0o100600), Err(TreeWhy::Mode { mode: 0o100600 }));
}

fn parent_refused(_: &Tape) -> Result<(), Broke> {
    match walk(b"..", 0o100644) {
        Err(TreeWhy::Name) => Ok(()),
        _ => Err(Broke::Oracle(String::from("a tree entry `..` was not refused"))),
    }
}

#[test]
fn the_oracle_catches_a_parent_name_taken() {
    control(Config::cases(1), Plant::Clean, Plant::ParentNameTaken, "not refused", |tape, p| {
        plant::with(p, || parent_refused(tape))
    });
}

fn pkt_lines(bytes: &[u8]) -> StdVec<String> {
    let mut lines = crate::Lines::new(bytes);
    let mut out = StdVec::new();
    while let Some(line) = lines.next_line().expect("pkt-line") {
        out.push(match line {
            Pkt::Data(d) => String::from_utf8_lossy(d).into_owned(),
            Pkt::Flush => String::from("<flush>"),
            Pkt::Delim => String::from("<delim>"),
            Pkt::End => String::from("<end>"),
        });
    }
    out
}

#[test]
fn requests_are_v2s_bytes() {
    let mut out = Vec::new_in(Global);
    let want = Oid::of(Kind::Blob, b"");
    fetch_request(want, 1, &mut out).expect("written");
    assert_eq!(
        pkt_lines(&out),
        [
            "command=fetch\n".to_string(),
            "agent=blaze\n".into(),
            "object-format=sha1\n".into(),
            "<delim>".into(),
            format!("want {want}\n"),
            "deepen 1\n".into(),
            "ofs-delta\n".into(),
            "no-progress\n".into(),
            "done\n".into(),
            "<flush>".into(),
        ]
    );
    assert!(out.starts_with(b"0012command=fetch\n"));
    let mut out = Vec::new_in(Global);
    ls_refs(&["refs/heads/main"], &mut out).expect("written");
    assert!(pkt_lines(&out).contains(&"ref-prefix refs/heads/main\n".to_string()));
}

fn pkt(payload: &[u8], out: &mut StdVec<u8>) {
    out.extend_from_slice(format!("{:04x}", payload.len() + 4).as_bytes());
    out.extend_from_slice(payload);
}

#[test]
fn answers_are_read_as_v2_says() {
    let mut ad = StdVec::new();
    pkt(b"# service=git-upload-pack\n", &mut ad);
    ad.extend_from_slice(b"0000");
    pkt(b"version 2\n", &mut ad);
    pkt(b"agent=git/github\n", &mut ad);
    pkt(b"ls-refs=unborn\n", &mut ad);
    pkt(b"fetch=shallow wait-for-done filter\n", &mut ad);
    ad.extend_from_slice(b"0000");
    assert_eq!(check_advertisement(&ad), Ok(()));
    let no_shallow = {
        let mut ad = StdVec::new();
        pkt(b"version 2\n", &mut ad);
        pkt(b"ls-refs\n", &mut ad);
        pkt(b"fetch\n", &mut ad);
        ad.extend_from_slice(b"0000");
        ad
    };
    assert!(check_advertisement(&no_shallow).is_err());

    let tag = Oid::of(Kind::Tag, b"t");
    let commit = Oid::of(Kind::Commit, b"c");
    let mut refs = StdVec::new();
    pkt(format!("{commit} refs/heads/main\n").as_bytes(), &mut refs);
    pkt(format!("{tag} refs/tags/v1 peeled:{commit}\n").as_bytes(), &mut refs);
    refs.extend_from_slice(b"0000");
    assert_eq!(find_ref(&refs, "refs/tags/v1"), Ok(Some(commit)));
    assert_eq!(find_ref(&refs, "refs/heads/main"), Ok(Some(commit)));
    assert_eq!(find_ref(&refs, "refs/heads/other"), Ok(None));

    let mut answer = StdVec::new();
    pkt(b"shallow-info\n", &mut answer);
    pkt(format!("shallow {commit}\n").as_bytes(), &mut answer);
    answer.extend_from_slice(b"0001");
    pkt(b"packfile\n", &mut answer);
    pkt(b"\x01PACK", &mut answer);
    pkt(b"\x02counting objects\n", &mut answer);
    pkt(b"\x01rest", &mut answer);
    answer.extend_from_slice(b"0000");
    let mut got = Vec::new_in(Global);
    fetched_pack(&answer, 1 << 20, &mut got).expect("demuxed");
    assert_eq!(&got[..], b"PACKrest");
    let mut error = StdVec::new();
    pkt(b"packfile\n", &mut error);
    pkt(b"\x03fatal: no such object\n", &mut error);
    assert_eq!(fetched_pack(&error, 1 << 20, &mut Vec::new_in(Global)), Err(crate::PktWhy::Remote));
}
