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
        if !self.files.contains_key(rel) {
            return 0;
        }
        // Stable per-path inode (a real filesystem does not renumber unrelated files when another
        // file is created or deleted).
        let mut h: u32 = 2166136261;
        for &b in rel.as_bytes() {
            h ^= u32::from(b);
            h = h.wrapping_mul(16777619);
        }
        u64::from(h.max(1))
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
    write_index(&mut mem, version, entries, 2000);
    mem
}

/// `.git/index` of `entries` written at time `at_time`, each entry's ino the one `Mem` gives
/// its file now (so a file not touched since stats equal to its entry).
fn write_index(mem: &mut Mem, version: u32, entries: &[(&str, &[u8], i64)], at_time: i64) {
    // Inodes are positions among all keys; write the index after, then fix each entry's ino.
    mem.put_at(".git/index", &[], at_time);
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
    mem.put_at(".git/index", &bytes, at_time);
}

fn porcelain(mem: &mut Mem, untracked: bool) -> Result<std::string::String, Refused> {
    status(".git", ".git", "", untracked, false, TREE, mem, Alloc::global()).map(|s| s.as_str().to_string())
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

/// A file that is not tracked is printed in porcelain form when untracked files count, and not
/// looked for under `--untracked-files=no`.
#[test]
fn index_an_untracked_file_is_printed() {
    let mut mem = worktree(2, &[("a.txt", b"hello\n", 1000)]);
    mem.put("new.txt", b"x");
    assert_eq!(porcelain(&mut mem, true).unwrap(), "?? new.txt\n");
    assert_eq!(porcelain(&mut mem, false).unwrap(), "");
}

#[test]
fn index_untracked_uses_negation_nested_and_info_exclude_rules() {
    let mut mem = worktree(2, &[("a.txt", b"hello\n", 1000)]);
    mem.put(".gitignore", b".gitignore\n*.tmp\n!important.tmp\nignored-dir/\n");
    mem.put("ignored.tmp", b"x");
    mem.put("important.tmp", b"x");
    mem.put("ignored-dir/file", b"x");
    mem.put("nested/.gitignore", b".gitignore\n!kept.txt\n");
    mem.put("nested/kept.txt", b"x");
    mem.put("nested/other.txt", b"x");
    mem.put(".git/info/exclude", b"excluded.txt\n");
    mem.put("excluded.txt", b"x");
    assert_eq!(porcelain(&mut mem, true).unwrap(), "?? important.tmp\n?? nested/\n");
    assert_eq!(status(".git", ".git", "", true, true, TREE, &mut mem, Alloc::global()).unwrap().as_str(), "?? important.tmp\n?? nested/kept.txt\n?? nested/other.txt\n");
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

// ---- (f) the status after a change (E2, E3) ----

mod after {
    //! E2: one status reads each ignore file once. E3: the status after a change
    //! (`status_after`) is byte-equal to the whole computation after every step of a model
    //! repository, computes whole only for a typed reason the model also sees, and asks no fact
    //! of a path the journal did not name (beyond the git folder and the ignore files above a
    //! named path): its work is bounded by the changed paths, never a walk.

    use std::collections::{BTreeMap, BTreeSet};
    use std::fmt;

    use blaze_own::{AString, AVec, Alloc};
    use blaze_sim::{Broken, Config, Tape};

    use super::{Mem, TREE, write_index};
    use crate::git::{FileStat, GitFiles};
    use crate::git_objects::{CoveredSince, Gen, Made, StatusAnswer, StatusPlant, WholeWhy, status, status_after, whole};

    /// A fact asked of the reader.
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    enum Fact {
        Read,
        Stat,
        List,
    }

    /// `Mem`, with every fact asked of it kept.
    struct Counting<'m> {
        mem: &'m mut Mem,
        asked: std::vec::Vec<(Fact, std::string::String)>,
    }

    impl<'m> Counting<'m> {
        fn new(mem: &'m mut Mem) -> Self {
            Counting { mem, asked: std::vec::Vec::new() }
        }

        fn count(&self, fact: Fact, path: &str) -> usize {
            self.asked.iter().filter(|(asked, at)| *asked == fact && at == path).count()
        }
    }

    impl GitFiles for Counting<'_> {
        fn read(&mut self, rel: &str, into: &mut AVec<'_, u8>) -> bool {
            self.asked.push((Fact::Read, rel.to_owned()));
            self.mem.read(rel, into)
        }
        fn read_range(&mut self, rel: &str, at: u64, len: u64, into: &mut AVec<'_, u8>) -> bool {
            self.asked.push((Fact::Read, rel.to_owned()));
            self.mem.read_range(rel, at, len, into)
        }
        fn stat(&mut self, rel: &str) -> Option<FileStat> {
            self.asked.push((Fact::Stat, rel.to_owned()));
            self.mem.stat(rel)
        }
        fn list<'a>(&mut self, rel: &str, alloc: Alloc<'a>, into: &mut AVec<'a, AString<'a>>) {
            self.asked.push((Fact::List, rel.to_owned()));
            self.mem.list(rel, alloc, into);
        }
        fn within<'p>(&self, path: &'p str) -> Option<&'p str> {
            self.mem.within(path)
        }
    }

    /// E2: n untracked files in one folder read its `.gitignore` once, as they read the root's
    /// and `info/exclude` once (each was read once per path before).
    #[test]
    fn each_ignore_file_is_read_once_per_status() {
        let alloc = blaze_own::testing::heap().alloc();
        let mut mem = super::worktree(2, &[("a.txt", b"hello\n", 1000)]);
        mem.put(".gitignore", b"*.tmp\n");
        mem.put("d/.gitignore", b"skip.txt\n");
        mem.put("d/skip.txt", b"x");
        mem.put("d/e/deep.tmp", b"x");
        for at in 0..32 {
            mem.put(&format!("d/f{at:02}.txt"), b"x");
            mem.put(&format!("d/e/g{at:02}.txt"), b"x");
        }
        let mut counting = Counting::new(&mut mem);
        let text = status(".git", ".git", "", true, true, TREE, &mut counting, alloc).expect("the status");
        assert!(text.contains("?? d/f00.txt\n") && text.contains("?? d/e/g31.txt\n"), "{text}");
        assert!(!text.contains("skip.txt") && !text.contains("deep.tmp"), "{text}");
        for file in [".git/info/exclude", ".gitignore", "d/.gitignore", "d/e/.gitignore"] {
            assert_eq!(counting.count(Fact::Read, file), 1, "`{file}` read once");
        }
    }

    /// What the oracle saw broken.
    #[derive(Debug)]
    enum StatusBroken {
        /// The answer after the change is not the whole computation's.
        Differs { after: std::string::String, whole: std::string::String },
        /// Computed whole for a reason the model does not see.
        WholeWithoutReason { why: WholeWhy },
        /// Made from the previous answer where the model sees a reason it may not be.
        ReasonIgnored { expected: std::string::String },
        /// A fact asked of a path the journal did not name: work beyond the changed paths.
        Untouched { path: std::string::String },
    }

    impl fmt::Display for StatusBroken {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            match self {
                StatusBroken::Differs { after, whole } => write!(f, "the status after the change differs from the whole computation:\nafter:\n{after}whole:\n{whole}"),
                StatusBroken::WholeWithoutReason { why } => write!(f, "computed whole ({why}) where the model sees no such reason"),
                StatusBroken::ReasonIgnored { expected } => write!(f, "made from the previous answer where it must be whole ({expected})"),
                StatusBroken::Untouched { path } => write!(f, "`{path}` was asked though no change reaches it"),
            }
        }
    }

    blaze_sim::plants! {
        impl StatusPlant breaks StatusBroken {
            None,
            KeepRevertedLine => StatusBroken::Differs { .. },
            NoCollapseUnit => StatusBroken::Differs { .. },
            CoverageIgnored => StatusBroken::ReasonIgnored { .. },
            IgnoringUnchecked => StatusBroken::Differs { .. },
        }
    }

    const BULK: usize = 8;
    const CREATE: [&str; 14] = [
        "a.txt", "b.txt", "src/new.rs", "src/deep/q.rs", "u/x.txt", "u/v/z.txt", "t/one.txt", "t/two/three.txt", "k.tmp", "u/keep.tmp", "nested/n.txt", "nested/m.tmp", ".blaze/x.txt",
        ".blaze/v2/store/s",
    ];
    /// blaze's own folder in the checkout, its ignore file and the bytes that ignore all of it
    /// (blaze-cli sweep.rs `HOME_IGNORE`, `HOME_IGNORE_TEXT`), and the profile every command
    /// writes there.
    const BLAZES: &str = ".blaze";
    const BLAZES_IGNORE: &str = ".blaze/.gitignore";
    const IGNORE_ALL: &[u8] = b"*\n";
    const PROFILE: &str = ".blaze/v2/profile.txt";
    const CONTENTS: [&[u8]; 3] = [b"one\n", b"two two\n", b"3\n"];
    const IGNORE_FILES: [&str; 3] = [".gitignore", "u/.gitignore", "t/.gitignore"];
    const IGNORE_LINES: [&str; 6] = ["*.tmp", "u/", "!keep.tmp", "v/", "three.txt", "two/"];

    /// `p` is `region` or under it.
    fn inside(region: &str, p: &str) -> bool {
        p == region || (p.len() > region.len() && p.as_bytes()[region.len()] == b'/' && p.starts_with(region))
    }

    /// A repository: tracked, untracked and ignored files in nested folders, `.gitignore`
    /// files and an index; the journal of its changes since the last answer, and that answer.
    struct Model {
        mem: Mem,
        /// The index: each entry's path and bytes.
        staged: BTreeMap<std::string::String, std::vec::Vec<u8>>,
        clock: i64,
        /// The journal's identity (a new one after an overflow) and its events in it.
        journal: u8,
        at: u64,
        /// The paths changed since the previous answer, as the journal names them.
        changed: BTreeSet<std::string::String>,
        /// The journal overflowed since the previous answer: what it named before is lost.
        lost: bool,
        previous: Option<StatusAnswer<'static>>,
    }

    impl Model {
        /// `blazes_ignored`: blaze's folder starts with its ignore-all file (else none, and its
        /// files are untracked: `?? .blaze/`).
        fn new(blazes_ignored: bool) -> Model {
            let mut mem = Mem::default();
            mem.put(".git/HEAD", b"ref: refs/heads/main\n");
            mem.put(".git/refs/heads/main", b"0\n");
            if blazes_ignored {
                mem.put(BLAZES_IGNORE, IGNORE_ALL);
            }
            mem.put(PROFILE, b"profile 0\n");
            let mut staged = BTreeMap::new();
            let tracked: [(&str, &[u8]); 5] = [(".gitignore", b"*.tmp\n"), ("a.txt", b"hello\n"), ("nested/x.txt", b"x\n"), ("src/lib.rs", b"lib\n"), ("src/main.rs", b"main\n")];
            for (path, bytes) in tracked {
                mem.put(path, bytes);
                staged.insert(path.to_owned(), bytes.to_vec());
            }
            for at in 0..BULK {
                let path = format!("bulk/f{at}.rs");
                mem.put(&path, b"bulk\n");
                staged.insert(path, b"bulk\n".to_vec());
            }
            mem.put("u/v/y.txt", b"y\n");
            mem.put("w.tmp", b"w\n");
            let mut model = Model { mem, staged, clock: 1000, journal: 0, at: 0, changed: BTreeSet::new(), lost: false, previous: None };
            model.restage(2000);
            model
        }

        fn tick(&mut self) -> i64 {
            self.clock += 1;
            self.clock
        }

        fn change(&mut self, path: &str) {
            self.at += 1;
            self.changed.insert(path.to_owned());
        }

        /// The worktree's files a step may touch: not the git folder's, not the untouched bulk,
        /// not an ignore file (its own step).
        fn files(&self) -> std::vec::Vec<std::string::String> {
            self.mem.files.keys().filter(|path| !path.starts_with(".git/") && !path.starts_with("bulk/") && !path.ends_with(".gitignore")).cloned().collect()
        }

        fn restage(&mut self, at_time: i64) {
            let entries: std::vec::Vec<(&str, &[u8], i64)> = self.staged.iter().map(|(path, bytes)| (path.as_str(), bytes.as_slice(), self.mem.files.get(path).map_or(1000, |(_, mtime)| *mtime))).collect();
            let mut mem = std::mem::take(&mut self.mem);
            write_index(&mut mem, 2, &entries, at_time);
            self.mem = mem;
        }

        fn step(&mut self, tape: &Tape) {
            let content = CONTENTS[tape.below(CONTENTS.len() as u64, "content") as usize];
            match tape.below(11, "step") {
                // An edit.
                0 => {
                    if let Some(path) = tape.pick(&self.files(), "a file to edit").cloned() {
                        let at = self.tick();
                        self.mem.put_at(&path, content, at);
                        self.change(&path);
                    }
                }
                // A revert: a tracked file back to its index bytes.
                1 => {
                    let tracked: std::vec::Vec<std::string::String> = self.staged.keys().filter(|path| !path.starts_with("bulk/") && !path.ends_with(".gitignore")).cloned().collect();
                    if let Some(path) = tape.pick(&tracked, "a file to revert").cloned() {
                        let bytes = self.staged[&path].clone();
                        let at = self.tick();
                        self.mem.put_at(&path, &bytes, at);
                        self.change(&path);
                    }
                }
                // A file created.
                2 => {
                    if let Some(&path) = tape.pick(&CREATE, "a file to create") {
                        let at = self.tick();
                        self.mem.put_at(path, content, at);
                        self.change(path);
                    }
                }
                // A file deleted.
                3 => {
                    if let Some(path) = tape.pick(&self.files(), "a file to delete").cloned() {
                        self.mem.files.remove(&path);
                        self.change(&path);
                    }
                }
                // A rename.
                4 => {
                    let from = tape.pick(&self.files(), "a file to rename").cloned();
                    let to = tape.pick(&CREATE, "its new name").copied();
                    if let (Some(from), Some(to)) = (from, to)
                        && from != to
                        && let Some((bytes, _)) = self.mem.files.remove(&from)
                    {
                        let at = self.tick();
                        self.mem.put_at(to, &bytes, at);
                        self.change(&from);
                        self.change(to);
                    }
                }
                // An ignore rule added.
                5 => {
                    let file = tape.pick(&IGNORE_FILES, "an ignore file").copied().unwrap_or(".gitignore");
                    let line = tape.pick(&IGNORE_LINES, "a rule").copied().unwrap_or("*.tmp");
                    let mut bytes = self.mem.files.get(file).map(|(bytes, _)| bytes.clone()).unwrap_or_default();
                    bytes.extend_from_slice(line.as_bytes());
                    bytes.push(b'\n');
                    let at = self.tick();
                    self.mem.put_at(file, &bytes, at);
                    self.change(file);
                }
                // A path staged (its bytes, or its removal).
                6 => {
                    let mut paths = self.files();
                    paths.extend(self.staged.keys().filter(|path| !path.starts_with("bulk/") && !path.ends_with(".gitignore")).cloned());
                    paths.sort();
                    paths.dedup();
                    if let Some(path) = tape.pick(&paths, "a path to stage").cloned() {
                        match self.mem.files.get(&path) {
                            Some((bytes, _)) => {
                                self.staged.insert(path, bytes.clone());
                            }
                            None => {
                                self.staged.remove(&path);
                            }
                        }
                        let at = 5000 + self.tick();
                        self.restage(at);
                        self.change(".git/index");
                    }
                }
                // A commit: the branch moves (its tree is the index's).
                7 => {
                    let at = self.tick();
                    self.mem.put_at(".git/refs/heads/main", format!("{at}\n").as_bytes(), at);
                    self.change(".git/refs/heads/main");
                }
                // blaze's ignore file made or removed (print.rs `ensure_checkout_ignore` makes
                // it; a person may remove it).
                8 => {
                    let at = self.tick();
                    match self.mem.files.remove(BLAZES_IGNORE) {
                        Some(_) => {}
                        None => self.mem.put_at(BLAZES_IGNORE, IGNORE_ALL, at),
                    }
                    self.change(BLAZES_IGNORE);
                }
                // A command's profile written in blaze's folder, as every command writes it.
                9 => {
                    let at = self.tick();
                    self.mem.put_at(PROFILE, format!("profile {at}\n").as_bytes(), at);
                    self.change(PROFILE);
                }
                // The journal overflows: what it held is lost, a new one starts.
                _ => {
                    self.journal = self.journal.wrapping_add(1);
                    self.at = 0;
                    self.changed.clear();
                    self.lost = true;
                }
            }
        }

        /// The untracked unit the whole computation's walk gives `changed`: in the default mode
        /// the highest folder on its way the index tracks nothing at or under.
        fn unit<'p>(&self, changed: &'p str, all: bool) -> &'p str {
            if all {
                return changed;
            }
            let ends = changed.match_indices('/').map(|(at, _)| at).chain(std::iter::once(changed.len()));
            for end in ends {
                let folder = &changed[..end];
                if !self.staged.keys().any(|path| inside(folder, path)) {
                    return folder;
                }
            }
            changed
        }

        /// Whether a fact of `path` is one a change reaches: the git folder, a named path, a
        /// folder on its way or under its unit, or an ignore file of such a folder.
        fn reached(&self, path: &str, all: bool) -> bool {
            let near = |q: &str| !q.is_empty() && self.changed.iter().any(|changed| inside(q, changed) || inside(changed, q) || inside(self.unit(changed, all), q));
            if path.starts_with(".git/") || path == ".gitignore" {
                return true;
            }
            if let Some(folder) = path.strip_suffix("/.gitignore")
                && near(folder)
            {
                return true;
            }
            near(path)
        }

        fn answer(&mut self, all: bool, plant: StatusPlant, alloc: Alloc<'static>) -> Result<(), Broken<StatusBroken>> {
            let now = Gen { journal: [self.journal; 24], at: self.at };
            let mut expected = std::vec::Vec::new();
            if self.previous.is_none() {
                expected.push(WholeWhy::NoPrevious);
            } else if self.lost {
                expected.push(WholeWhy::NotCovered);
            }
            if self.changed.iter().any(|path| path.starts_with(".git/")) {
                expected.push(WholeWhy::GitFolder);
            }
            if self.changed.iter().any(|path| path.rsplit('/').next() == Some(".gitignore")) {
                expected.push(WholeWhy::RulesChanged);
            }
            let changed: std::vec::Vec<&str> = self.changed.iter().map(|path| path.as_str()).collect();
            let covered = match (&self.previous, self.lost) {
                (None, _) => None,
                // Replayed only from the new journal's start: the previous answer's place is in
                // the lost one.
                (Some(_), true) => Some(CoveredSince::replayed(Gen { journal: [self.journal; 24], at: 0 }, &changed)),
                (Some(previous), false) => Some(CoveredSince::replayed(previous.place, &changed)),
            };
            // The claim blaze-cli makes before its replay: blaze's folder ignores all, its ignore
            // file read now (the model's place). Planted: claimed unread.
            let blazes_ignored = self.mem.files.get(BLAZES_IGNORE).is_some_and(|(bytes, _)| bytes.as_slice() == IGNORE_ALL);
            let covered = covered.map(|covered| match blazes_ignored || plant == StatusPlant::IgnoringUnchecked {
                true => covered.ignoring(BLAZES),
                false => covered,
            });
            let mut counting = Counting::new(&mut self.mem);
            let after = status_after(self.previous.as_ref(), covered.as_ref(), true, all, ".git", ".git", "", |_, _| Ok(TREE), now, &mut counting, alloc, plant)
                .map_err(|why| Broken::Harness(format!("the status after refused: {why:?}")))?;
            let asked = std::mem::take(&mut counting.asked);
            let whole = whole(".git", ".git", "", true, all, TREE, &mut self.mem, alloc).map_err(|why| Broken::Harness(format!("the whole status refused: {why:?}")))?;
            match after.made {
                Made::Whole(why) if !expected.contains(&why) => return Err(Broken::Oracle(StatusBroken::WholeWithoutReason { why })),
                Made::Incremental { .. } if !expected.is_empty() => return Err(Broken::Oracle(StatusBroken::ReasonIgnored { expected: format!("{expected:?}") })),
                _ => {}
            }
            if after.answer.text.as_str() != whole.text.as_str() {
                return Err(Broken::Oracle(StatusBroken::Differs { after: after.answer.text.as_str().to_owned(), whole: whole.text.as_str().to_owned() }));
            }
            if let Made::Incremental { .. } = after.made
                && let Some((_, path)) = asked.iter().find(|(_, path)| !self.reached(path, all))
            {
                return Err(Broken::Oracle(StatusBroken::Untouched { path: path.clone() }));
            }
            self.previous = Some(after.answer);
            self.changed.clear();
            self.lost = false;
            Ok(())
        }
    }

    /// A case: a model repository, its first answer whole, then steps (edit, revert, create,
    /// delete, rename, an ignore rule, a stage, a commit, a journal overflow), answered after
    /// some and at the end. Controls: a reverted edit that keeps its ` M` line, a new file in a
    /// folder already shown `?? dir/` adding its own line, and lost journal coverage taken as
    /// covered.
    fn case(tape: &Tape, plant: StatusPlant) -> Result<(), Broken<StatusBroken>> {
        let alloc = blaze_own::testing::heap().alloc();
        let all = tape.coin("-uall");
        let mut model = Model::new(tape.coin("blaze's folder ignored at the start"));
        model.answer(all, plant, alloc)?;
        let steps = tape.below(16, "steps") + 1;
        for _ in 0..steps {
            model.step(tape);
            if tape.coin("answer now") {
                model.answer(all, plant, alloc)?;
            }
        }
        model.answer(all, plant, alloc)
    }

    #[test]
    fn the_status_after_a_change_is_the_whole_status() {
        blaze_sim::controls(Config::cases(256), case);
    }

    /// The proof's two parts read back as made.
    #[test]
    fn a_coverage_proof_reads_back() {
        let place = Gen { journal: [7; 24], at: 42 };
        let changed = ["a.txt"];
        let covered = CoveredSince::replayed(place, &changed);
        assert_eq!((covered.since(), covered.changed()), (place, &changed[..]));
    }
}
