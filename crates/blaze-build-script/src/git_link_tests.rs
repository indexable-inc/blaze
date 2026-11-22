//! `status` on a checkout holding tracked symlinks, as a copy (D77) serves it: every stat
//! differs from the index (the inode and the ctime), so each entry is compared by content. A
//! link's content is its target text, compared with its blob byte for byte as git's
//! `ce_compare_link` does; the link is never followed, so a target outside the checkout is only
//! text. Tempo tracks two links (`CLAUDE.md`, `.claude/skills`), and its build scripts ask
//! `git status --porcelain`.
//!
//! Each test passes as `test git_link_tests::<name> ... ok`. Without the link comparison (the
//! old `SymlinkNotBuilt` refusal) the first fails with that refusal at its `unwrap`.

use blaze_git_fetch::{Kind, Oid, sha1::sha1};
use blaze_own::{AString, AVec, Alloc, Vec};

use crate::git::{FileKind, FileStat, GitFiles, LinkUnread, Refused};
use crate::git_objects::status;

/// The tree HEAD names; the index's cache tree records the same, so no staged change is seen.
const TREE: Oid = Oid([0x22; 20]);

/// The index file's mtime: later than every entry's, so no entry is racy.
const INDEX_TIME: i64 = 2000;
/// Every worktree node's mtime and ctime, and the index's record of them.
const NODE_TIME: i64 = 1000;

#[derive(Clone, Copy)]
enum Node {
    File(&'static [u8]),
    /// A symlink and its target text.
    Link(&'static [u8]),
}

impl Node {
    /// What git stores as the entry's blob: a file's bytes, a link's target text.
    fn blob(self) -> &'static [u8] {
        match self {
            Node::File(bytes) | Node::Link(bytes) => bytes,
        }
    }
}

/// A served checkout: `.git/index`, and the worktree's nodes in index order (sorted by bytes).
struct Served<'a> {
    index: AVec<'a, u8>,
    nodes: [(&'static str, Node); 3],
    /// Added to every node's inode: nonzero is a copy, whose stats all differ from the index.
    ino_shift: u64,
}

/// Tempo's two links (one target leaves the checkout: it is text, never followed) and a file.
const TEMPO_LIKE: [(&str, Node); 3] = [
    (".claude/skills", Node::Link(b"../../outside/skills")),
    ("CLAUDE.md", Node::Link(b"AGENTS.md")),
    ("a.txt", Node::File(b"hello\n")),
];

impl<'a> Served<'a> {
    /// The index written from `nodes` (version 2, index-format.txt): per entry ctime, mtime,
    /// dev, ino, mode, uid, gid, size (four bytes each), the blob id, 16 bits of flags (the name
    /// length), the path NUL-padded to a multiple of 8; then a `TREE` extension whose root line
    /// is `\0<entries> 0\n` and the root id; then the SHA-1 of all before it.
    fn new(nodes: [(&'static str, Node); 3], ino_shift: u64, alloc: Alloc<'a>) -> Self {
        let mut index = Vec::new_in(alloc);
        index.extend_from_slice(b"DIRC");
        index.extend_from_slice(&2u32.to_be_bytes());
        index.extend_from_slice(&(nodes.len() as u32).to_be_bytes());
        for (at, (path, node)) in nodes.iter().enumerate() {
            let start = index.len();
            let mode = match node {
                Node::File(_) => 0o100644,
                Node::Link(_) => 0o120000,
            };
            let size = node.blob().len() as u32;
            for field in [NODE_TIME as u32, 0, NODE_TIME as u32, 0, 0, at as u32 + 1, mode, 0, 0, size] {
                index.extend_from_slice(&field.to_be_bytes());
            }
            index.extend_from_slice(&Oid::of(Kind::Blob, node.blob()).0);
            index.extend_from_slice(&(path.len() as u16).to_be_bytes());
            index.extend_from_slice(path.as_bytes());
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
        Served { index, nodes, ino_shift }
    }

    fn node(&self, rel: &str) -> Option<(usize, Node)> {
        self.nodes.iter().position(|(path, _)| *path == rel).map(|at| (at, self.nodes[at].1))
    }
}

impl GitFiles for Served<'_> {
    fn read(&mut self, rel: &str, into: &mut AVec<'_, u8>) -> bool {
        if rel == ".git/index" {
            into.extend_from_slice(&self.index);
            return true;
        }
        // A link is not a file to `read`: its text comes only through `read_link`.
        let Some((_, Node::File(bytes))) = self.node(rel) else { return false };
        into.extend_from_slice(bytes);
        true
    }

    fn read_range(&mut self, _rel: &str, _at: u64, _len: u64, _into: &mut AVec<'_, u8>) -> bool {
        false
    }

    fn stat(&mut self, rel: &str) -> Option<FileStat> {
        let stat = |kind, size, mtime, ino| FileStat { kind, size, mtime: (mtime, 0), ctime: (NODE_TIME, 0), ino, uid: 0, gid: 0, executable: false };
        if rel == ".git/index" {
            return Some(stat(FileKind::File, self.index.len() as u64, INDEX_TIME, 0));
        }
        let (at, node) = self.node(rel)?;
        let kind = match node {
            Node::File(_) => FileKind::File,
            Node::Link(_) => FileKind::Symlink,
        };
        Some(stat(kind, node.blob().len() as u64, NODE_TIME, at as u64 + 1 + self.ino_shift))
    }

    fn list<'b>(&mut self, _rel: &str, _alloc: Alloc<'b>, _into: &mut AVec<'b, AString<'b>>) {}

    fn within<'p>(&self, _path: &'p str) -> Option<&'p str> {
        None
    }

    fn read_link(&mut self, rel: &str, into: &mut AVec<'_, u8>) -> Result<(), LinkUnread> {
        let Some((_, Node::Link(target))) = self.node(rel) else { return Err(LinkUnread::NotALink) };
        into.extend_from_slice(target);
        Ok(())
    }
}

/// A reader that does not serve `read_link`: the trait's default answers for it.
struct NoLinks<'s, 'a>(&'s mut Served<'a>);

impl GitFiles for NoLinks<'_, '_> {
    fn read(&mut self, rel: &str, into: &mut AVec<'_, u8>) -> bool {
        self.0.read(rel, into)
    }
    fn read_range(&mut self, rel: &str, at: u64, len: u64, into: &mut AVec<'_, u8>) -> bool {
        self.0.read_range(rel, at, len, into)
    }
    fn stat(&mut self, rel: &str) -> Option<FileStat> {
        self.0.stat(rel)
    }
    fn list<'b>(&mut self, rel: &str, alloc: Alloc<'b>, into: &mut AVec<'b, AString<'b>>) {
        self.0.list(rel, alloc, into)
    }
    fn within<'p>(&self, path: &'p str) -> Option<&'p str> {
        self.0.within(path)
    }
}

fn porcelain(files: &mut impl GitFiles, alloc: Alloc<'_>) -> Result<std::string::String, Refused> {
    status(".git", ".git", "", false, false, TREE, files, alloc).map(|out| out.as_str().to_owned())
}

/// In a copy every stat differs; links whose target text equals their blob are clean, the one
/// outside the checkout included (text, never followed). A changed target is ` M`; the same
/// tree with stats equal to the index (not a copy) is clean without reading a link.
#[test]
fn a_copied_link_is_compared_by_its_target_text() {
    let alloc = blaze_own::testing::heap().alloc();
    let mut copy = Served::new(TEMPO_LIKE, 100, alloc);
    assert_eq!(porcelain(&mut copy, alloc).unwrap(), "");
    copy.nodes[1].1 = Node::Link(b"README.md");
    assert_eq!(porcelain(&mut copy, alloc).unwrap(), " M CLAUDE.md\n");
    // Same length, other bytes: the comparison is of the text, not the size.
    copy.nodes[1].1 = Node::Link(b"AGENTS.mD");
    assert_eq!(porcelain(&mut copy, alloc).unwrap(), " M CLAUDE.md\n");
    let mut original = Served::new(TEMPO_LIKE, 0, alloc);
    assert_eq!(porcelain(&mut NoLinks(&mut original), alloc).unwrap(), "");
}

/// A link replaced by a file keeps git's own answer refused by name (a type change).
#[test]
fn a_link_become_a_file_is_a_type_change() {
    let alloc = blaze_own::testing::heap().alloc();
    let mut copy = Served::new(TEMPO_LIKE, 100, alloc);
    copy.nodes[1].1 = Node::File(b"AGENTS.md");
    assert!(matches!(porcelain(&mut copy, alloc), Err(Refused::TypeChanged)));
}

/// A reader that cannot read a link's target is refused by name, never answered clean.
#[test]
fn a_reader_without_read_link_is_refused_by_name() {
    let alloc = blaze_own::testing::heap().alloc();
    let mut copy = Served::new(TEMPO_LIKE, 100, alloc);
    assert!(matches!(porcelain(&mut NoLinks(&mut copy), alloc), Err(Refused::LinkUnread(LinkUnread::ReaderCannot))));
}
