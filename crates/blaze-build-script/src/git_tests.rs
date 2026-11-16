//! Tests of the `.git` readers (D54) on fixtures the test writes itself, from git's own
//! format documentation (pack-format.txt, index-format.txt) and source; never by running git.
//! Every hostile input must end in a typed refusal, never a panic, and no object may come back
//! whose bytes do not hash to its id.

use std::collections::BTreeMap;

use blaze_git_fetch::{DeltaWhy, Kind, Oid, PackWhy, sha1::sha1, store::StoreWhy};
use blaze_own::{AString, AVec, Alloc, String as OString, Vec as OVec};
use blaze_sim::{Broke, Config, Tape};

use crate::git::{FileKind, FileStat, GitFile, GitFiles, Refused, StoreFile, Vcs, answer, question};
use crate::git_objects::{MOST_DEPTH, PLANT, Plant, Store, parse_index, status};

/// A served tree in memory; absolute paths name it from `/served`.
#[derive(Clone, Default)]
struct Mem {
    files: BTreeMap<std::string::String, (std::vec::Vec<u8>, i64)>,
}

impl Mem {
    fn put(&mut self, path: &str, bytes: &[u8]) {
        self.put_at(path, bytes, 1000);
    }
    fn put_at(&mut self, path: &str, bytes: &[u8], mtime: i64) {
        self.files.insert(path.to_string(), (bytes.to_vec(), mtime));
    }
    fn ino(&self, rel: &str) -> u64 {
        self.files.keys().position(|k| k == rel).map_or(0, |at| at as u64 + 1)
    }
}

impl GitFiles for Mem {
    fn read(&mut self, rel: &str, into: &mut AVec<'_, u8>) -> bool {
        self.files.get(rel).map(|(bytes, _)| into.extend_from_slice(bytes)).is_some()
    }
    fn read_range(&mut self, rel: &str, at: u64, len: u64, into: &mut AVec<'_, u8>) -> bool {
        let Some((bytes, _)) = self.files.get(rel) else { return false };
        let Some(end) = at.checked_add(len).filter(|end| *end <= bytes.len() as u64) else { return false };
        into.extend_from_slice(&bytes[at as usize..end as usize]);
        true
    }
    fn stat(&mut self, rel: &str) -> Option<FileStat> {
        let file = |size: u64, mtime: i64, ino: u64| FileStat { kind: FileKind::File, size, mtime: (mtime, 0), ctime: (1000, 0), ino, uid: 0, gid: 0, executable: false };
        if let Some((bytes, mtime)) = self.files.get(rel) {
            return Some(file(bytes.len() as u64, *mtime, self.ino(rel)));
        }
        let prefix = format!("{rel}/");
        self.files.keys().any(|k| rel.is_empty() || k.starts_with(&prefix)).then(|| FileStat { kind: FileKind::Dir, ..file(0, 1000, 0) })
    }
    fn list<'a>(&mut self, rel: &str, alloc: Alloc<'a>, into: &mut AVec<'a, AString<'a>>) {
        let prefix = if rel.is_empty() { std::string::String::new() } else { format!("{rel}/") };
        let mut last: Option<&str> = None;
        for key in self.files.keys() {
            let Some(rest) = key.strip_prefix(&prefix) else { continue };
            let name = rest.split('/').next().unwrap_or(rest);
            if last != Some(name) {
                into.push(OString::from_str_in(name, alloc));
                last = Some(name);
            }
        }
    }
    fn within<'p>(&self, path: &'p str) -> Option<&'p str> {
        path.strip_prefix("/served/")
    }
}

// ---- writers, from git's documentation ----

/// zlib (RFC 1950) of `data` in stored deflate blocks (RFC 1951 BTYPE 00): `78 01`, blocks of
/// at most 65535 bytes as `final LEN NLEN bytes`, then the Adler-32 big-endian.
fn zlib(data: &[u8]) -> std::vec::Vec<u8> {
    let mut out = vec![0x78, 0x01];
    let mut chunks: std::vec::Vec<&[u8]> = data.chunks(65535).collect();
    if chunks.is_empty() {
        chunks.push(&[]);
    }
    let last = chunks.len() - 1;
    for (at, chunk) in chunks.iter().enumerate() {
        out.push(u8::from(at == last));
        let len = chunk.len() as u16;
        out.extend_from_slice(&len.to_le_bytes());
        out.extend_from_slice(&(!len).to_le_bytes());
        out.extend_from_slice(chunk);
    }
    let (mut a, mut b) = (1u32, 0u32);
    for byte in data {
        a = (a + u32::from(*byte)) % 65521;
        b = (b + a) % 65521;
    }
    out.extend_from_slice(&((b << 16) | a).to_be_bytes());
    out
}

/// A loose object file: zlib of `<kind> <len>\0<body>`.
fn loose(kind: &str, body: &[u8]) -> std::vec::Vec<u8> {
    let mut whole = format!("{kind} {}\0", body.len()).into_bytes();
    whole.extend_from_slice(body);
    zlib(&whole)
}

fn loose_path(oid: &Oid) -> std::string::String {
    let hex = std::string::String::from_utf8(oid.hex().to_vec()).unwrap();
    format!(".git/objects/{}/{}", &hex[..2], &hex[2..])
}

fn hex(oid: &Oid) -> std::string::String {
    std::string::String::from_utf8(oid.hex().to_vec()).unwrap()
}

fn commit(seconds: &str, zone: &str, message: &str) -> std::vec::Vec<u8> {
    format!("tree {}\nauthor A <a@b> {seconds} {zone}\ncommitter C <c@d> {seconds} {zone}\n\n{message}\n", "4b825dc642cb6eb9a060e54bf8d69288fbee4904").into_bytes()
}

/// A pack entry's header (pack-format.txt): type in bits 4-6 of the first byte, the size's low
/// 4 bits, then 7 bits per byte while the high bit is set.
fn entry_header(kind: u8, size: u64) -> std::vec::Vec<u8> {
    let mut out = vec![];
    let mut c = (kind << 4) | (size & 15) as u8;
    let mut s = size >> 4;
    while s != 0 {
        out.push(c | 0x80);
        c = (s & 0x7f) as u8;
        s >>= 7;
    }
    out.push(c);
    out
}

/// An `OFS_DELTA`'s distance back: 7 bits per byte, most significant first, each continuation
/// adding one before the shift (pack-format.txt "offset encoding").
fn ofs(back: u64) -> std::vec::Vec<u8> {
    let mut out = vec![(back & 127) as u8];
    let mut o = back >> 7;
    while o != 0 {
        o -= 1;
        out.insert(0, 0x80 | (o & 127) as u8);
        o >>= 7;
    }
    out
}

/// A delta's size: 7 bits per byte, least significant first.
fn varint(mut n: u64, out: &mut std::vec::Vec<u8>) {
    loop {
        let byte = (n & 0x7f) as u8;
        n >>= 7;
        if n == 0 {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}

/// A delta: base size, result size, a copy of `base[offset..offset + len]` (`0x80` with the
/// offset's and size's present bytes flagged) then an insert of `tail` (`len` then bytes).
fn delta(base_len: u64, result_len: u64, offset: u32, len: u32, tail: &[u8]) -> std::vec::Vec<u8> {
    let mut out = vec![];
    varint(base_len, &mut out);
    varint(result_len, &mut out);
    let mut op = 0x80u8;
    let mut args = vec![];
    for i in 0..4 {
        if (offset >> (8 * i)) & 0xff != 0 {
            op |= 1 << i;
            args.push((offset >> (8 * i)) as u8);
        }
    }
    for i in 0..3 {
        if (len >> (8 * i)) & 0xff != 0 {
            op |= 0x10 << i;
            args.push((len >> (8 * i)) as u8);
        }
    }
    out.push(op);
    out.extend(args);
    for chunk in tail.chunks(127) {
        out.push(chunk.len() as u8);
        out.extend_from_slice(chunk);
    }
    out
}

/// A version 2 pack: `PACK`, version, count, the entries, the SHA-1 of all before it.
struct Pack {
    bytes: std::vec::Vec<u8>,
    count: u32,
}

impl Pack {
    fn new() -> Pack {
        Pack { bytes: b"PACK\0\0\0\x02\0\0\0\0".to_vec(), count: 0 }
    }
    fn raw(&mut self, header: &[u8], data: &[u8]) -> u64 {
        let at = self.bytes.len() as u64;
        self.bytes.extend_from_slice(header);
        self.bytes.extend_from_slice(data);
        self.count += 1;
        at
    }
    fn whole(&mut self, kind: u8, body: &[u8]) -> u64 {
        self.raw(&entry_header(kind, body.len() as u64), &zlib(body))
    }
    fn ofs_delta(&mut self, base_at: u64, delta: &[u8]) -> u64 {
        let mut header = entry_header(6, delta.len() as u64);
        header.extend(ofs(self.bytes.len() as u64 - base_at));
        self.raw(&header, &zlib(delta))
    }
    fn ref_delta(&mut self, base: &Oid, delta: &[u8]) -> u64 {
        let mut header = entry_header(7, delta.len() as u64);
        header.extend_from_slice(&base.0);
        self.raw(&header, &zlib(delta))
    }
    fn finish(mut self) -> (std::vec::Vec<u8>, [u8; 20]) {
        self.bytes[8..12].copy_from_slice(&self.count.to_be_bytes());
        let sum = sha1(&self.bytes);
        self.bytes.extend_from_slice(&sum);
        (self.bytes, sum)
    }
}

/// A version 2 pack index (pack-format.txt): `\377tOc`, version 2, 256 cumulative fan-out
/// counts, the sorted ids, a CRC-32 each (zeros here: blaze reads none), 4-byte offsets (high
/// bit: an index into the 8-byte table, used for every entry when `large`), the 8-byte table,
/// the pack's checksum, the index's own.
fn idx(entries: &[(Oid, u64)], large: bool, pack_sum: [u8; 20]) -> std::vec::Vec<u8> {
    let mut sorted = entries.to_vec();
    sorted.sort_by(|a, b| a.0.0.cmp(&b.0.0));
    let mut out = b"\xfftOc\0\0\0\x02".to_vec();
    for byte in 0..256usize {
        let count = sorted.iter().filter(|(id, _)| usize::from(id.0[0]) <= byte).count() as u32;
        out.extend_from_slice(&count.to_be_bytes());
    }
    for (id, _) in &sorted {
        out.extend_from_slice(&id.0);
    }
    out.extend(std::iter::repeat_n(0u8, 4 * sorted.len()));
    for (at, (_, offset)) in sorted.iter().enumerate() {
        let small = if large { 0x8000_0000 | at as u32 } else { *offset as u32 };
        out.extend_from_slice(&small.to_be_bytes());
    }
    if large {
        for (_, offset) in &sorted {
            out.extend_from_slice(&offset.to_be_bytes());
        }
    }
    out.extend_from_slice(&pack_sum);
    let sum = sha1(&out);
    out.extend_from_slice(&sum);
    out
}

fn install(mem: &mut Mem, pack: &[u8], idx: &[u8], sum: [u8; 20]) {
    let name = hex(&Oid(sum));
    mem.put(&format!(".git/objects/pack/pack-{name}.pack"), pack);
    mem.put(&format!(".git/objects/pack/pack-{name}.idx"), idx);
}

fn read(mem: &mut Mem, oid: &Oid) -> Result<(Kind, std::vec::Vec<u8>), Refused> {
    let alloc = Alloc::global();
    let store = Store::open(".git", mem, alloc)?;
    let mut out = OVec::new_in(alloc);
    let kind = store.read(oid, mem, alloc, &mut out)?;
    Ok((kind, out.to_vec()))
}

fn ask(mem: &mut Mem, folder: &str, args: &[&str]) -> Result<std::string::String, Refused> {
    answer(question(args), folder, Vcs::Served, mem, Alloc::global()).map(|a| a.stdout.as_str().to_string())
}

/// A pack holding a whole commit, an `OFS_DELTA` and a `REF_DELTA` on it; the three ids.
fn three() -> (std::vec::Vec<u8>, [u8; 20], [(Oid, u64); 3]) {
    let base = commit("1700000000", "+0530", "base");
    let base_id = Oid::of(Kind::Commit, &base);
    let keep = base.len() as u32 - 5;
    let mut ofs_body = base[..keep as usize].to_vec();
    ofs_body.extend_from_slice(b"ofs!\n");
    let mut ref_body = base[..keep as usize].to_vec();
    ref_body.extend_from_slice(b"ref!\n");
    let mut pack = Pack::new();
    let at = pack.whole(1, &base);
    let ofs_at = pack.ofs_delta(at, &delta(base.len() as u64, ofs_body.len() as u64, 0, keep, b"ofs!\n"));
    let ref_at = pack.ref_delta(&base_id, &delta(base.len() as u64, ref_body.len() as u64, 0, keep, b"ref!\n"));
    let (bytes, sum) = pack.finish();
    (bytes, sum, [(base_id, at), (Oid::of(Kind::Commit, &ofs_body), ofs_at), (Oid::of(Kind::Commit, &ref_body), ref_at)])
}

fn three_installed(large: bool) -> (Mem, [(Oid, u64); 3]) {
    let (pack, sum, ids) = three();
    let mut mem = Mem::default();
    install(&mut mem, &pack, &idx(&ids, large, sum), sum);
    (mem, ids)
}

// ---- (a) objects ----

/// A loose object is zlib of `commit <len>\0<body>`, found at `objects/<2 hex>/<38 hex>`; `log
/// -1 --pretty=format:` reads HEAD's committer line (git 2.51 `DATE_ISO8601_STRICT` for `%cI`).
#[test]
fn a_loose_commit_answers_log() {
    let body = commit("1700000000", "+0000", "one");
    let id = Oid::of(Kind::Commit, &body);
    let mut mem = Mem::default();
    mem.put(&loose_path(&id), &loose("commit", &body));
    mem.put(".git/HEAD", format!("{}\n", hex(&id)).as_bytes());
    assert_eq!(ask(&mut mem, "", &["log", "-1", "--pretty=format:%cI %cs %ct %H"]).unwrap(), format!("2023-11-14T22:13:20Z 2023-11-14 1700000000 {}", hex(&id)));
}

/// A packed base, an `OFS_DELTA` (base named by its distance back) and a `REF_DELTA` (base
/// named by id) on it, read through a version 2 index with 4-byte offsets and again with every
/// offset in the 8-byte table.
#[test]
fn packed_objects_and_both_delta_kinds_read_back() {
    for large in [false, true] {
        let (mut mem, ids) = three_installed(large);
        for (id, _) in ids {
            let (kind, body) = read(&mut mem, &id).unwrap();
            assert_eq!((kind, Oid::of(kind, &body)), (Kind::Commit, id));
        }
    }
}

/// `describe --always` with no tag prints git's abbreviation: at least 7 hex digits.
#[test]
fn describe_always_without_tags_prints_seven_digits() {
    let (mut mem, ids) = three_installed(false);
    mem.put(".git/HEAD", format!("{}\n", hex(&ids[1].0)).as_bytes());
    mem.put(".git/refs/heads/.keep", b"");
    assert_eq!(ask(&mut mem, "", &["describe", "--always"]).unwrap().trim_end(), &hex(&ids[1].0)[..7]);
}

fn chain(depth: usize) -> (Mem, Oid) {
    let base = commit("1700000000", "+0000", "chain");
    let id = Oid::of(Kind::Commit, &base);
    let mut pack = Pack::new();
    let mut at = pack.whole(1, &base);
    let copy = delta(base.len() as u64, base.len() as u64, 0, base.len() as u32, b"");
    for _ in 0..depth {
        at = pack.ofs_delta(at, &copy);
    }
    let (bytes, sum) = pack.finish();
    let mut mem = Mem::default();
    install(&mut mem, &bytes, &idx(&[(id, at)], false, sum), sum);
    (mem, id)
}

/// A chain of `OFS_DELTA`s is read to git's `pack.depth` ceiling, 4095, and refused by name past it.
#[test]
fn a_delta_chain_at_the_cap_reads_and_one_past_is_refused() {
    let (mut mem, id) = chain(MOST_DEPTH);
    assert_eq!(read(&mut mem, &id).unwrap().0, Kind::Commit);
    let (mut mem, id) = chain(MOST_DEPTH + 1);
    assert!(matches!(read(&mut mem, &id), Err(Refused::DepthOver { .. })));
}

/// `objects/info/alternates` names another object folder, relative to `objects`.
#[test]
fn an_alternate_object_folder_is_read() {
    let body = commit("1700000000", "+0000", "alt");
    let id = Oid::of(Kind::Commit, &body);
    let mut mem = Mem::default();
    mem.put(".git/objects/info/alternates", b"../../shared/objects\n");
    let hex = hex(&id);
    mem.put(&format!("shared/objects/{}/{}", &hex[..2], &hex[2..]), &loose("commit", &body));
    assert_eq!(Oid::of(Kind::Commit, &read(&mut mem, &id).unwrap().1), id);
}

/// A linked worktree's `.git` file says `gitdir: <main>/.git/worktrees/<name>`, whose
/// `commondir` leads back to the main repository: allowed while it stays inside the served
/// tree (read-only, and its files are the fact's reads); outside it, refused by name.
#[test]
fn a_linked_worktree_reads_its_main_repository_and_not_outside() {
    let body = commit("1700000000", "-0330", "wt");
    let id = Oid::of(Kind::Commit, &body);
    let mut mem = Mem::default();
    let hex = hex(&id);
    mem.put(&format!("main/.git/objects/{}/{}", &hex[..2], &hex[2..]), &loose("commit", &body));
    mem.put("main/.git/HEAD", b"ref: refs/heads/main\n");
    mem.put("main/.git/worktrees/wt/HEAD", format!("{hex}\n").as_bytes());
    mem.put("main/.git/worktrees/wt/commondir", b"../..\n");
    mem.put("wt/.git", b"gitdir: /served/main/.git/worktrees/wt\n");
    assert_eq!(ask(&mut mem, "wt", &["log", "-1", "--pretty=format:%cI"]).unwrap(), "2023-11-14T18:43:20-03:30");
    mem.put("wt/.git", b"gitdir: /elsewhere/.git/worktrees/wt\n");
    assert!(matches!(ask(&mut mem, "wt", &["log", "-1", "--pretty=format:%cI"]), Err(Refused::Outside(GitFile::Gitdir))));
}

// ---- (d) dates ----

/// `%cI` is the committer time in its own zone; `Z` for zone 0, else `±HH:MM`. git's commit
/// format stores an unsigned count of seconds (`parse_timestamp` is `strtoumax`), so a date
/// before 1970 cannot be written: a `-` there is a malformed committer line.
#[test]
fn committer_dates_in_every_zone() {
    for (seconds, zone, want) in [("0", "+0000", "1970-01-01T00:00:00Z"), ("1700000000", "+0530", "2023-11-15T03:43:20+05:30"), ("1700000000", "-0330", "2023-11-14T18:43:20-03:30")] {
        let body = commit(seconds, zone, "d");
        let id = Oid::of(Kind::Commit, &body);
        let mut mem = Mem::default();
        mem.put(&loose_path(&id), &loose("commit", &body));
        mem.put(".git/HEAD", format!("{}\n", hex(&id)).as_bytes());
        assert_eq!(ask(&mut mem, "", &["log", "-1", "--pretty=format:%cI"]).unwrap(), want);
    }
    let body = commit("-5", "+0000", "d");
    let id = Oid::of(Kind::Commit, &body);
    let mut mem = Mem::default();
    mem.put(&loose_path(&id), &loose("commit", &body));
    mem.put(".git/HEAD", format!("{}\n", hex(&id)).as_bytes());
    assert!(matches!(ask(&mut mem, "", &["log", "-1", "--pretty=format:%cI"]), Err(Refused::Commit { .. })));
}

// ---- (b) hostile objects ----

fn refused_why(result: Result<(Kind, std::vec::Vec<u8>), Refused>) -> (StoreFile, StoreWhy) {
    match result {
        Err(Refused::Store { file, why, .. }) => (file, why),
        other => panic!("not a store refusal: {:?}", other.map(|(kind, _)| kind)),
    }
}

/// An index shorter than its header, fan-out and tables say.
#[test]
fn hostile_a_truncated_index() {
    let (pack, sum, ids) = three();
    let mut mem = Mem::default();
    install(&mut mem, &pack, &idx(&ids, false, sum)[..1100], sum);
    assert!(matches!(refused_why(read(&mut mem, &ids[0].0)), (StoreFile::Idx(_), StoreWhy::IdxShort)));
}

/// The fan-out table is cumulative: a count smaller than the one before it.
#[test]
fn hostile_a_fanout_that_is_not_monotonic() {
    let (pack, sum, ids) = three();
    let mut bytes = idx(&ids, false, sum);
    bytes[8..12].copy_from_slice(&u32::MAX.to_be_bytes());
    let mut mem = Mem::default();
    install(&mut mem, &pack, &bytes, sum);
    assert!(matches!(refused_why(read(&mut mem, &ids[0].0)), (StoreFile::Idx(_), StoreWhy::IdxFanout { byte: 1 })));
}

/// An offset at or past the pack's trailer.
#[test]
fn hostile_an_offset_past_the_packs_end() {
    let (pack, sum, ids) = three();
    let mut bytes = idx(&[(ids[0].0, pack.len() as u64 + 100)], false, sum);
    let mut mem = Mem::default();
    install(&mut mem, &pack, &bytes, sum);
    assert!(matches!(refused_why(read(&mut mem, &ids[0].0)), (StoreFile::Idx(_), StoreWhy::IdxOffset { .. })));
    // The same through the 8-byte table, and an 8-byte index past the table.
    bytes = idx(&[(ids[0].0, u64::MAX)], true, sum);
    install(&mut mem, &pack, &bytes, sum);
    assert!(matches!(refused_why(read(&mut mem, &ids[0].0)), (StoreFile::Idx(_), StoreWhy::IdxOffset { .. })));
    bytes = idx(&[(ids[0].0, 12)], true, sum);
    let small = 8 + 1024 + 20 + 4;
    bytes[small..small + 4].copy_from_slice(&0x8000_0005u32.to_be_bytes());
    install(&mut mem, &pack, &bytes, sum);
    assert!(matches!(refused_why(read(&mut mem, &ids[0].0)), (StoreFile::Idx(_), StoreWhy::IdxOffset { .. })));
}

fn one_delta(delta_bytes: &[u8]) -> (Mem, Oid) {
    let base = commit("1700000000", "+0000", "base");
    let mut pack = Pack::new();
    let at = pack.whole(1, &base);
    let top = pack.ofs_delta(at, delta_bytes);
    let (bytes, sum) = pack.finish();
    let id = Oid([7; 20]);
    let mut mem = Mem::default();
    install(&mut mem, &bytes, &idx(&[(id, top)], false, sum), sum);
    (mem, id)
}

/// A delta's first size must equal its base's length.
#[test]
fn hostile_a_delta_whose_base_size_does_not_match() {
    let len = commit("1700000000", "+0000", "base").len() as u64;
    let (mut mem, id) = one_delta(&delta(len + 1, 4, 0, 4, b""));
    assert!(matches!(refused_why(read(&mut mem, &id)).1, StoreWhy::Entry(PackWhy::Delta(DeltaWhy::BaseSize { .. }))));
}

/// A copy instruction must lie inside its base.
#[test]
fn hostile_a_delta_copy_past_its_base() {
    let len = commit("1700000000", "+0000", "base").len() as u64;
    let (mut mem, id) = one_delta(&delta(len, len + 10, 0, len as u32 + 10, b""));
    assert!(matches!(refused_why(read(&mut mem, &id)).1, StoreWhy::Entry(PackWhy::Delta(DeltaWhy::CopyPast { .. }))));
}

fn one_raw(header: &[u8], data: &[u8]) -> (Mem, Oid) {
    let mut pack = Pack::new();
    let at = pack.raw(header, data);
    let (bytes, sum) = pack.finish();
    let id = Oid([9; 20]);
    let mut mem = Mem::default();
    install(&mut mem, &bytes, &idx(&[(id, at)], false, sum), sum);
    (mem, id)
}

/// The inflated bytes may not exceed the entry's declared size: the declared size is the cap.
#[test]
fn hostile_an_entry_inflating_past_its_declared_size() {
    let (mut mem, id) = one_raw(&entry_header(3, 10), &zlib(&[b'x'; 100_000]));
    assert!(matches!(refused_why(read(&mut mem, &id)).1, StoreWhy::Entry(PackWhy::Inflate(_) | PackWhy::Size { .. })));
}

/// A declared size of 2^60 is refused before any allocation: its size varint runs past the
/// 57 bits the header reader accepts (a header refusal); 2^40 parses and is over the cap.
#[test]
fn hostile_a_declared_size_of_two_to_the_sixty() {
    let (mut mem, id) = one_raw(&entry_header(3, 1 << 60), &zlib(b"x"));
    assert!(matches!(refused_why(read(&mut mem, &id)).1, StoreWhy::Entry(PackWhy::Header)));
    let (mut mem, id) = one_raw(&entry_header(3, 1 << 40), &zlib(b"x"));
    assert!(matches!(refused_why(read(&mut mem, &id)).1, StoreWhy::Entry(PackWhy::ObjectOver { .. })));
}

/// An object's bytes must hash to the id it was found under, packed or loose.
#[test]
fn hostile_an_object_that_does_not_hash_to_its_id() {
    let (pack, sum, ids) = three();
    let mut mem = Mem::default();
    let wrong = Oid([0x42; 20]);
    install(&mut mem, &pack, &idx(&[(wrong, ids[0].1)], false, sum), sum);
    assert!(matches!(refused_why(read(&mut mem, &wrong)), (StoreFile::Pack(_), StoreWhy::Id { .. })));
    let mut mem = Mem::default();
    mem.put(&loose_path(&wrong), &loose("commit", b"tree x\n"));
    assert!(matches!(refused_why(read(&mut mem, &wrong)), (StoreFile::Loose(_), StoreWhy::Id { .. })));
}

// ---- (e) the index ----

const TREE: Oid = Oid([0x11; 20]);

/// `.git/index` (index-format.txt): `DIRC`, version, count; per entry ctime, mtime, dev, ino,
/// mode, uid, gid, size (4 bytes each), the id, 16 bits of flags (name length in the low 12),
/// then the path NUL-padded to a multiple of 8 (v2), or in v4 an offset varint of bytes to drop
/// from the previous path, the rest and one NUL; then a `TREE` extension whose root line is
/// `\0<entries> <subtrees>\n` and the root id; then a checksum.
fn index(version: u32, entries: &[(&str, &[u8], i64)]) -> std::vec::Vec<u8> {
    let mut out = b"DIRC".to_vec();
    out.extend_from_slice(&version.to_be_bytes());
    out.extend_from_slice(&(entries.len() as u32).to_be_bytes());
    let mut previous = "";
    for (at, (path, content, mtime)) in entries.iter().enumerate() {
        let start = out.len();
        for field in [1000, 0, *mtime as u32, 0, 0, at as u32 + 1, 0o100644, 0, 0, content.len() as u32] {
            out.extend_from_slice(&u32::to_be_bytes(field));
        }
        out.extend_from_slice(&Oid::of(Kind::Blob, content).0);
        out.extend_from_slice(&(path.len().min(0xfff) as u16).to_be_bytes());
        if version == 4 {
            let shared = previous.bytes().zip(path.bytes()).take_while(|(a, b)| a == b).count();
            out.extend(ofs((previous.len() - shared) as u64));
            out.extend_from_slice(&path.as_bytes()[shared..]);
            out.push(0);
        } else {
            out.extend_from_slice(path.as_bytes());
            let length = out.len() - start;
            out.extend(std::iter::repeat_n(0u8, (length + 8) / 8 * 8 - length));
        }
        previous = path;
    }
    let mut tree = b"\x002 0\n".to_vec();
    tree.extend_from_slice(&TREE.0);
    out.extend_from_slice(b"TREE");
    out.extend_from_slice(&(tree.len() as u32).to_be_bytes());
    out.extend(tree);
    let sum = sha1(&out);
    out.extend_from_slice(&sum);
    out
}

/// A worktree whose files were written at time 1000 and an index at time 2000. Files listed in
/// the index carry the inode `Mem` gives them, so their stat matches.
fn worktree(version: u32, entries: &[(&str, &[u8], i64)]) -> Mem {
    let mut mem = Mem::default();
    mem.put(".git/HEAD", b"ref: refs/heads/main\n");
    for (path, content, _) in entries {
        mem.put(path, content);
    }
    // Inodes are positions among all keys; write the index after, then fix each entry's ino.
    mem.put_at(".git/index", &[], 2000);
    let mut bytes = index(version, entries);
    let mut at = 12;
    let mut previous = "";
    for (path, _, _) in entries {
        let ino = mem.ino(path) as u32;
        bytes[at + 20..at + 24].copy_from_slice(&ino.to_be_bytes());
        let shared = previous.bytes().zip(path.bytes()).take_while(|(a, b)| a == b).count();
        at += if version == 4 { 62 + ofs((previous.len() - shared) as u64).len() + (path.len() - shared) + 1 } else { (62 + path.len() + 8) / 8 * 8 };
        previous = path;
    }
    mem.put_at(".git/index", &bytes, 2000);
    mem
}

fn porcelain(mem: &mut Mem, untracked: bool) -> Result<std::string::String, Refused> {
    status(".git", ".git", "", untracked, TREE, mem, Alloc::global()).map(|s| s.as_str().to_string())
}

/// A file whose stat equals its entry's, older than the index, is clean without being read.
#[test]
fn index_a_clean_worktree() {
    let mut mem = worktree(2, &[("a.txt", b"hello\n", 1000), ("src/b.rs", b"fn b() {}\n", 1000)]);
    assert_eq!(porcelain(&mut mem, true).unwrap(), "");
    // A config like tempo's: `filemode = true` and another key `false` (a wrong refusal once).
    mem.put(".git/config", b"[core]\n\tfilemode = true\n\tbare = false\n");
    assert_eq!(porcelain(&mut mem, true).unwrap(), "");
    mem.put(".git/config", b"[core]\n\tfileMode = false\n");
    assert!(matches!(porcelain(&mut mem, true), Err(Refused::ConfigNotRead)));
}

/// git's racy rule: an entry not older than the index is hashed even when its stat matches; a
/// changed file of the same size and time is ` M`. A missing file is ` D`.
#[test]
fn index_the_racy_case_is_hashed_and_a_deleted_file_shows() {
    let mut mem = worktree(3, &[("a.txt", b"hello\n", 2000), ("b.txt", b"same\n", 2000), ("c.txt", b"gone\n", 1000)]);
    mem.put_at("a.txt", b"jello\n", 2000);
    mem.put_at("b.txt", b"same\n", 2000);
    mem.files.remove("c.txt");
    assert_eq!(porcelain(&mut mem, false).unwrap(), " M a.txt\n D c.txt\n");
}

/// Version 4 compresses each path against the one before it.
#[test]
fn index_version_four_paths() {
    let mut mem = worktree(4, &[("src/lib.rs", b"a\n", 1000), ("src/main.rs", b"b\n", 1000), ("zz", b"c\n", 1000)]);
    assert_eq!(porcelain(&mut mem, false).unwrap(), "");
    mem.put_at("src/main.rs", b"B\n", 2000);
    assert_eq!(porcelain(&mut mem, false).unwrap(), " M src/main.rs\n");
}

/// A file that is not tracked is refused by name when untracked files count (blaze has no
/// ignore rules), and not looked for under `--untracked-files=no`.
#[test]
fn index_an_untracked_file_is_refused() {
    let mut mem = worktree(2, &[("a.txt", b"hello\n", 1000)]);
    mem.put("new.txt", b"x");
    assert!(matches!(porcelain(&mut mem, true), Err(Refused::Untracked)));
    assert_eq!(porcelain(&mut mem, false).unwrap(), "");
}

/// An entry path git would never write (`verify_path`): `..`, absolute, `.git` inside.
#[test]
fn hostile_index_paths_outside_the_worktree() {
    for path in ["../x", "/etc/passwd", "a/../../x", ".git/config", "a//b", "A/.GIT/x"] {
        let bytes = index(2, &[(path, b"x", 1000)]);
        assert!(matches!(parse_index(&bytes, Alloc::global()), Err(Refused::IndexPath { at: 12 })), "{path}");
    }
}

/// A v4 entry whose path varint would start at the checksum (found by reading: it indexed past
/// the end before the fix).
#[test]
fn hostile_index_v4_entry_ending_at_the_checksum() {
    let mut bytes = b"DIRC\0\0\0\x04\0\0\0\x01".to_vec();
    bytes.extend(std::iter::repeat_n(0u8, 62));
    bytes.extend(std::iter::repeat_n(0x80u8, 20));
    assert!(matches!(parse_index(&bytes, Alloc::global()), Err(Refused::IndexMalformed { .. })));
}

// ---- (c) property ----

/// Random bytes, and random mutations (flips, truncation, a replaced span) of a valid pack,
/// its index and a `.git/index`, never panic, and an object read back always hashes to its id.
#[test]
fn mutated_packs_and_indexes_never_panic_nor_lie() {
    blaze_sim::check(Config::DEFAULT, |tape| mutated_case(tape, Plant::None));
}

/// The control of the object oracle: with the hash check of a returned object removed, a
/// mutated pack gives an object that does not hash to its id, and the property says so.
#[test]
fn the_mutation_property_catches_an_unchecked_object_hash() {
    blaze_sim::control(Config::DEFAULT, Plant::None, Plant::NoObjectHashCheck, "does not hash to its id", mutated_case);
}

/// The control of the panic oracle: with an index entry's length used before it is checked,
/// a mutated `.git/index` makes the reader panic, and the property says so.
#[test]
fn the_mutation_property_catches_an_index_length_used_unchecked() {
    blaze_sim::control(Config::DEFAULT, Plant::None, Plant::IndexEntryLengthUnchecked, "panicked", mutated_case);
}

/// One case of the mutation property with `plant` planted: `Broke::Oracle` names an object
/// that does not hash to its id, or a reader that panicked.
fn mutated_case(tape: &Tape, plant: Plant) -> Result<(), Broke> {
    PLANT.with(|it| it.set(plant));
    let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| mutated_reads(tape)));
    PLANT.with(|it| it.set(Plant::None));
    match caught {
        Ok(Ok(())) => Ok(()),
        Ok(Err(id)) => Err(Broke::Oracle(format!("object {id:?} read back does not hash to its id"))),
        Err(_) => Err(Broke::Oracle("a reader panicked on mutated bytes".to_owned())),
    }
}

/// Reads every object of a mutated pack and parses a mutated index; `Err` is an object id
/// whose bytes, returned as read, do not hash to it.
fn mutated_reads(tape: &Tape) -> Result<(), Oid> {
    let (pack, sum, ids) = three();
    let good_idx = idx(&ids, false, sum);
    let good_index = index(2, &[("a.txt", b"hello\n", 1000), ("b/c.rs", b"c\n", 1000)]);
    {
        let mut files = [pack.clone(), good_idx.clone(), good_index.clone()];
        let which = tape.below(3, "file") as usize;
        let file = &mut files[which];
        if which == 0 && tape.coin("well-formed lie") {
            // Sound zlib and headers, but the base object's bytes are not the ones its id
            // names: only the hash check of the object read back can refuse it.
            // The same three entries at the same offsets, so the index's bounds still fit.
            let mut base = commit("1700000000", "+0530", "base");
            let base_id = Oid::of(Kind::Commit, &base);
            let len = base.len() as u64;
            let keep = base.len() as u32 - 5;
            let at = tape.below(len, "lie at") as usize;
            base[at] ^= 1 << tape.below(8, "lie bit");
            let mut lie = Pack::new();
            let base_at = lie.whole(1, &base);
            lie.ofs_delta(base_at, &delta(len, u64::from(keep) + 5, 0, keep, b"ofs!\n"));
            lie.ref_delta(&base_id, &delta(len, u64::from(keep) + 5, 0, keep, b"ref!\n"));
            *file = lie.finish().0;
        }
        if tape.coin("random") {
            let mut noise = vec![0u8; tape.below(200, "len") as usize];
            tape.fill(&mut noise, "noise");
            *file = noise;
        }
        while tape.more(4, "mutation") {
            if file.is_empty() {
                break;
            }
            let at = tape.below(file.len() as u64, "at") as usize;
            match tape.below(3, "kind") {
                0 => file[at] ^= 1 << tape.below(8, "bit"),
                1 => file.truncate(at),
                _ => file[at] = tape.int::<u8>("byte"),
            }
        }
        let mut mem = Mem::default();
        install(&mut mem, &files[0], &files[1], sum);
        for (id, _) in ids {
            if let Ok((kind, body)) = read(&mut mem, &id)
                && Oid::of(kind, &body) != id
            {
                return Err(id);
            }
        }
        let _ = parse_index(&files[2], Alloc::global());
    }
    Ok(())
}
