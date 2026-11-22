//! `status` under `* text=auto`, as tempo's checkout has it (its top `.gitattributes` is
//! `* text=auto`, `.gitattributes !filter !diff` and `linguist-language` rows; its one tracked
//! file holding a CR is `zones/assets/header.png`), served as a copy (D77): every stat differs
//! from the index, so each file is hashed and compared with its entry's blob.
//!
//! The rule, gitattributes(5) on `text=auto`: "Git decides by itself whether the file is text or
//! binary. If it is text and the file was not already in Git with CRLF endings, line endings are
//! converted on checkin"; the decision is git convert.c's `crlf_to_git` (binary by
//! `convert_is_binary`, "already in Git with CRLF" by `has_crlf_in_index`).
//!
//! Each test passes as `test git_attributes_tests::<name> ... ok`. Without the conversion (the
//! old blanket `AttributesNotApplied` on any CR) `a_binary_file_with_cr_is_hashed_raw` and the
//! two text cases fail with that refusal at their `unwrap`; converting without asking the index
//! blob fails `a_text_file_whose_blob_has_crlf_is_compared_raw`; never converting fails
//! `a_crlf_text_file_whose_blob_is_lf_is_clean`.

use blaze_git_fetch::{Kind, Oid, sha1::sha1};
use blaze_own::{AString, AVec, Alloc, Vec};

use crate::git::{FileKind, FileStat, GitFiles, Refused};
use crate::git_attributes::{Attribute, AttributesFile, TextStats, sets_text_auto, text_stats};
use crate::git_objects::status;

/// The tree HEAD names; the index's cache tree records the same, so no staged change is seen.
const TREE: Oid = Oid([0x33; 20]);
/// The index file's mtime: later than every entry's, so no entry is racy.
const INDEX_TIME: i64 = 2000;
const NODE_TIME: i64 = 1000;

/// tempo's top `.gitattributes`, byte for byte.
const TEMPO_ATTRIBUTES: &[u8] = b"* text=auto\n.gitattributes !filter !diff\n\n.env.example linguist-language=Dotenv\nDockerfile.* linguist-language=Dockerfile\nbiome.json linguist-language=JSON-with-Comments\n";

/// A PNG's first bytes: the signature holds `\r\n`, a lone `\x1a`, a lone `\n`, then NULs.
const PNG: &[u8] = b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDR\x00\x00\x00\x10";

/// A tracked file: its worktree bytes and the blob its index entry names.
#[derive(Clone, Copy)]
struct Node {
    path: &'static str,
    work: &'static [u8],
    blob: &'static [u8],
}

/// A served copy: `.git/index`, the loose blobs, and the worktree in index order.
struct Served<'a> {
    index: AVec<'a, u8>,
    nodes: [Node; 3],
}

/// `.gitattributes`, `a.txt`, `header.png`: sorted by bytes, as an index holds them.
fn tree(attributes: &'static [u8], text_work: &'static [u8], text_blob: &'static [u8]) -> [Node; 3] {
    [
        Node { path: ".gitattributes", work: attributes, blob: attributes },
        Node { path: "a.txt", work: text_work, blob: text_blob },
        Node { path: "header.png", work: PNG, blob: PNG },
    ]
}

impl<'a> Served<'a> {
    /// The index (version 2, index-format.txt) as in git_link_tests: each entry's ctime, mtime,
    /// dev, ino, mode, uid, gid, size, blob id, flags (name length) and NUL-padded path; a
    /// `TREE` extension whose root is [`TREE`]; the SHA-1 of all before it. The size is the
    /// worktree file's, as `git add` records it; the inode is `at + 1`, and the worktree serves
    /// `at + 101`, so every stat differs (a copy).
    fn new(nodes: [Node; 3], alloc: Alloc<'a>) -> Self {
        let mut index = Vec::new_in(alloc);
        index.extend_from_slice(b"DIRC");
        index.extend_from_slice(&2u32.to_be_bytes());
        index.extend_from_slice(&(nodes.len() as u32).to_be_bytes());
        for (at, node) in nodes.iter().enumerate() {
            let start = index.len();
            for field in [NODE_TIME as u32, 0, NODE_TIME as u32, 0, 0, at as u32 + 1, 0o100644, 0, 0, node.work.len() as u32] {
                index.extend_from_slice(&field.to_be_bytes());
            }
            index.extend_from_slice(&Oid::of(Kind::Blob, node.blob).0);
            index.extend_from_slice(&(node.path.len() as u16).to_be_bytes());
            index.extend_from_slice(node.path.as_bytes());
            let length = index.len() - start;
            for _ in 0..(length + 8) / 8 * 8 - length {
                index.push(0);
            }
        }
        let root_line: &[u8] = b"\x003 0\n";
        index.extend_from_slice(b"TREE");
        index.extend_from_slice(&((root_line.len() + TREE.0.len()) as u32).to_be_bytes());
        index.extend_from_slice(root_line);
        index.extend_from_slice(&TREE.0);
        let sum = sha1(&index);
        index.extend_from_slice(&sum);
        Served { index, nodes }
    }
}

/// Whether `rel` is the loose object file of `blob` (`.git/objects/<2 hex>/<38 hex>`).
fn is_loose_of(rel: &str, blob: &[u8]) -> bool {
    let hex = Oid::of(Kind::Blob, blob).hex();
    let Some(rest) = rel.strip_prefix(".git/objects/") else { return false };
    let rest = rest.as_bytes();
    rest.len() == 41 && rest[..2] == hex[..2] && rest[2] == b'/' && rest[3..] == hex[2..]
}

/// A loose object file: zlib (RFC 1950) of `blob <len>\0<body>` in one stored deflate block
/// (RFC 1951 BTYPE 00: `01 LEN NLEN bytes`), then the Adler-32, big-endian.
fn put_loose(body: &[u8], into: &mut AVec<'_, u8>) {
    let mut whole = std::format!("blob {}\0", body.len()).into_bytes();
    whole.extend_from_slice(body);
    into.extend_from_slice(&[0x78, 0x01, 0x01]);
    let len = whole.len() as u16;
    into.extend_from_slice(&len.to_le_bytes());
    into.extend_from_slice(&(!len).to_le_bytes());
    into.extend_from_slice(&whole);
    let (mut a, mut b) = (1u32, 0u32);
    for byte in &whole {
        a = (a + u32::from(*byte)) % 65521;
        b = (b + a) % 65521;
    }
    into.extend_from_slice(&((b << 16) | a).to_be_bytes());
}

impl GitFiles for Served<'_> {
    fn read(&mut self, rel: &str, into: &mut AVec<'_, u8>) -> bool {
        if rel == ".git/index" {
            into.extend_from_slice(&self.index);
            return true;
        }
        if let Some(node) = self.nodes.iter().find(|node| is_loose_of(rel, node.blob)) {
            put_loose(node.blob, into);
            return true;
        }
        let Some(node) = self.nodes.iter().find(|node| node.path == rel) else { return false };
        into.extend_from_slice(node.work);
        true
    }

    fn read_range(&mut self, _rel: &str, _at: u64, _len: u64, _into: &mut AVec<'_, u8>) -> bool {
        false
    }

    fn stat(&mut self, rel: &str) -> Option<FileStat> {
        let stat = |size, mtime, ino| FileStat { kind: FileKind::File, size, mtime: (mtime, 0), ctime: (NODE_TIME, 0), ino, uid: 0, gid: 0, executable: false };
        if rel == ".git/index" {
            return Some(stat(self.index.len() as u64, INDEX_TIME, 0));
        }
        let at = self.nodes.iter().position(|node| node.path == rel)?;
        Some(stat(self.nodes[at].work.len() as u64, NODE_TIME, at as u64 + 101))
    }

    fn list<'b>(&mut self, _rel: &str, _alloc: Alloc<'b>, _into: &mut AVec<'b, AString<'b>>) {}

    fn within<'p>(&self, _path: &'p str) -> Option<&'p str> {
        None
    }
}

fn porcelain(nodes: [Node; 3], alloc: Alloc<'_>) -> Result<std::string::String, Refused> {
    let mut served = Served::new(nodes, alloc);
    status(".git", ".git", "", false, false, TREE, &mut served, alloc).map(|out| out.as_str().to_owned())
}

/// header.png holds CRs and NULs; under `text=auto` it is binary (a NUL, and a lone CR), so its
/// raw bytes are hashed and equal its blob: clean. This is tempo-node's and zone-node's case.
#[test]
fn a_binary_file_with_cr_is_hashed_raw() {
    let alloc = blaze_own::testing::heap().alloc();
    assert_eq!(porcelain(tree(TEMPO_ATTRIBUTES, b"a\n", b"a\n"), alloc).unwrap(), "");
}

/// A text file checked out with CRLF whose blob is LF: clean converts CRLF to LF, which hashes
/// to the blob: clean. A blob of other text is ` M`.
#[test]
fn a_crlf_text_file_whose_blob_is_lf_is_clean() {
    let alloc = blaze_own::testing::heap().alloc();
    assert_eq!(porcelain(tree(TEMPO_ATTRIBUTES, b"a\r\nb\r\n", b"a\nb\n"), alloc).unwrap(), "");
    assert_eq!(porcelain(tree(TEMPO_ATTRIBUTES, b"a\r\nb\r\n", b"x\n"), alloc).unwrap(), " M a.txt\n");
}

/// A text file whose index blob itself holds CRLF ("already in Git with CRLF endings") is
/// hashed raw: the same CRLF bytes are clean (a converting clean would hash `a\nb\n` and call
/// it changed), and other bytes are ` M`.
#[test]
fn a_text_file_whose_blob_has_crlf_is_compared_raw() {
    let alloc = blaze_own::testing::heap().alloc();
    assert_eq!(porcelain(tree(TEMPO_ATTRIBUTES, b"a\r\nb\r\n", b"a\r\nb\r\n"), alloc).unwrap(), "");
    assert_eq!(porcelain(tree(TEMPO_ATTRIBUTES, b"a\r\nb\r\n", b"a\r\nb\n"), alloc).unwrap(), " M a.txt\n");
}

/// With no `text` attribute, git asks `core.autocrlf`, which blaze does not read (a global
/// config may set it): a CR is still refused by name, never hashed raw on a guess.
#[test]
fn a_cr_with_no_text_attribute_is_refused() {
    let alloc = blaze_own::testing::heap().alloc();
    assert_eq!(porcelain(tree(b"*.md diff\n", b"a\n", b"a\n"), alloc), Err(Refused::AttributesNotApplied));
}

/// Each byte-changing form blaze does not apply is refused by its name; tempo's own files
/// (top and `zones/`) are read through.
#[test]
fn an_unbuilt_attribute_is_refused_by_name() {
    let alloc = blaze_own::testing::heap().alloc();
    assert_eq!(sets_text_auto(TEMPO_ATTRIBUTES, AttributesFile::Top), Ok(true));
    assert_eq!(sets_text_auto(b".env.example linguist-language=Dotenv\ndocker/Dockerfile.chef linguist-language=Dockerfile\n", AttributesFile::Nested { top_sets_auto: false }), Ok(false));
    assert_eq!(sets_text_auto(b"\"a b\" -text\n", AttributesFile::Top), Err(Refused::AttributeNotBuilt(Attribute::Text)));
    let cases: [(&[u8], Attribute); 11] = [
        (b"*.sh text\n", Attribute::Text),
        (b"*.bin -text\n", Attribute::Text),
        (b"* text=auto\n*.txt !text\n", Attribute::Text),
        (b"*.rs text=auto\n", Attribute::TextAutoNotEverywhere),
        (b"*.bat eol=crlf\n", Attribute::Eol),
        (b"*.c crlf=input\n", Attribute::Crlf),
        (b"*.psd filter=lfs diff=lfs merge=lfs -text\n", Attribute::Filter),
        (b"*.c ident\n", Attribute::Ident),
        (b"*.txt working-tree-encoding=UTF-16\n", Attribute::WorkingTreeEncoding),
        (b"*.png binary\n", Attribute::Binary),
        (b"[attr]lfs filter=lfs\n", Attribute::MacroDefinition),
    ];
    for (text, which) in cases {
        assert_eq!(sets_text_auto(text, AttributesFile::Top), Err(Refused::AttributeNotBuilt(which)));
    }
    // In a folder's file with no top rule, `* text=auto` covers only that folder: refused, not
    // widened.
    assert_eq!(sets_text_auto(b"* text=auto\n", AttributesFile::Nested { top_sets_auto: false }), Err(Refused::AttributeNotBuilt(Attribute::TextAutoNotEverywhere)));
    // Through `status`: the refusal reaches the script by name.
    assert_eq!(porcelain(tree(b"* text=auto\n*.txt eol=lf\n", b"a\n", b"a\n"), alloc), Err(Refused::AttributeNotBuilt(Attribute::Eol)));
}

/// tempo's shape: the top `.gitattributes` and `sub/.gitattributes` (tempo's `zones/`) both hold
/// `* text=auto`. gitattributes(5): the nested file's `*` is every path under `sub/`, and it
/// overrides the top file there, but with the same value, so every path is under `text=auto`
/// and a CRLF copy of an LF blob is clean. A nested `* text=auto` with no top rule would put
/// `text=auto` on `sub/` only, which is pattern matching and is refused by name. A nested
/// `-text` would override the top rule for its folder, and that is refused by name too.
#[test]
fn a_nested_text_auto_under_the_top_rule_is_accepted() {
    let alloc = blaze_own::testing::heap().alloc();
    let nodes = |top: &'static [u8], sub: &'static [u8]| {
        [
            Node { path: ".gitattributes", work: top, blob: top },
            Node { path: "a.txt", work: b"a\r\nb\r\n", blob: b"a\nb\n" },
            Node { path: "sub/.gitattributes", work: sub, blob: sub },
        ]
    };
    assert_eq!(porcelain(nodes(TEMPO_ATTRIBUTES, b"* text=auto\n.env.example linguist-language=Dotenv\n"), alloc).unwrap(), "");
    assert_eq!(porcelain(nodes(b"*.md diff\n", b"* text=auto\n"), alloc), Err(Refused::AttributeNotBuilt(Attribute::TextAutoNotEverywhere)));
    assert_eq!(porcelain(nodes(TEMPO_ATTRIBUTES, b"* -text\n"), alloc), Err(Refused::AttributeNotBuilt(Attribute::Text)));
    assert_eq!(porcelain(nodes(TEMPO_ATTRIBUTES, b"*.txt text=auto\n"), alloc), Err(Refused::AttributeNotBuilt(Attribute::TextAutoNotEverywhere)));
}

/// convert.c `gather_stats` and `convert_is_binary` at their edges: one non-printable byte in
/// 127 printable ones is binary, in 128 it is text; a final Ctrl-Z is not counted; a lone CR
/// and a NUL are binary.
#[test]
fn text_stats_follow_gather_stats() {
    let mut bytes = [b'x'; 129];
    bytes[0] = 0x01;
    assert!(text_stats(&bytes[..128]).binary());
    assert!(!text_stats(&bytes).binary());
    assert_eq!(text_stats(b"a\x1a"), TextStats { printable: 1, ..TextStats::default() });
    assert!(!text_stats(b"a\x1a").binary());
    assert_eq!(text_stats(b"a\r\nb\rc\n"), TextStats { crlf: 1, lone_cr: 1, printable: 3, ..TextStats::default() });
    assert!(text_stats(b"a\rb").binary());
    assert!(text_stats(b"a\0b").binary());
    assert!(!text_stats(b"\x08\t\x1b\x0c").binary());
}
