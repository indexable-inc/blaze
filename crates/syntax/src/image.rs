//! A file's parse as stored bytes read in place: base-db keeps the parses of library files in
//! the daemon's image and maps them back at a restart instead of lexing and parsing the files
//! again (docs/v2/plan/lanes/tree-store/design.md).
//!
//! A parse's record, little-endian, from a multiple of 8, every offset from its start: a head
//! of [`HEAD`] `u32`s (magic, the record's length, [`TREE_VERSION`], the outline's edition
//! plus one or 0 for a full parse, how many errors, where their rows are, where their messages
//! are and how long, where the tree's record is and how long, two 0s), the lexer's and
//! parser's errors as rows `[start, end, message offset, message length]`, their messages,
//! and the tree's record (`syntax_tree`'s `FlatTree::write_image`).

use std::{ops::Range, sync::Arc};

use blaze_bytes::{Bytes, le::get_u32 as u32_at};
pub use syntax_tree::ImageError;

use crate::{
    Edition, FlatTree, Parse, RustLanguage, SourceFile, SyntaxError, SyntaxKind, Text,
    TextRange, TextSize, Tree, parsing,
};

/// What a stored tree depends on besides its text, its edition and the tree record's layout
/// (`syntax_tree::IMAGE_LAYOUT`): the lexer, the parser, the grammar's kinds and how a tree
/// is built of their output. Bump it with any change to the tree or the errors some text
/// parses to; `the_tree_version_names_the_trees_parsed` fails when a file of the parser's test
/// data parses to another tree under the same version.
pub const TREE_VERSION: u32 = 2;

const MAGIC: u32 = u32::from_le_bytes(*b"bzp1");
const HEAD: usize = 12;
/// An error row's `u32`s.
const ERROR: usize = 4;

/// Appends `parse` (its tree, the bodies parsed so far, and its lexer's and parser's errors)
/// to `out` as a record [`read`] maps back: from the next multiple of 8 of `out`'s length,
/// which is where in `out` it is.
pub fn write(parse: &Parse<SourceFile>, out: &mut Vec<u8>) -> Range<usize> {
    pad(out);
    let start = out.len();
    out.resize(start + HEAD * 4, 0);
    let mut head = [0u32; HEAD];
    let errors = parse.errors.as_deref().unwrap_or(&[]);
    // alloc: the errors' messages, once per parse stored
    let messages: Vec<String> = errors.iter().map(ToString::to_string).collect();
    head[5] = offset(out, start);
    let mut message = 0;
    for (error, text) in errors.iter().zip(&messages) {
        let range = error.range();
        let len = to_u32(text.len());
        for word in [range.start().into(), range.end().into(), message, len] {
            out.extend_from_slice(&u32::to_le_bytes(word));
        }
        message += len;
    }
    head[6] = offset(out, start);
    head[7] = message;
    for text in &messages {
        out.extend_from_slice(text.as_bytes());
    }
    let tree = parse.tree.as_ref().expect("a parse holds its tree").flat().write_image(out);
    pad(out);
    head[0] = MAGIC;
    head[1] = offset(out, start);
    head[2] = TREE_VERSION;
    head[3] = parse.outline.map_or(0, |edition| edition as u32 + 1);
    head[4] = to_u32(errors.len());
    head[8] = to_u32(tree.start - start);
    head[9] = to_u32(tree.len());
    for (n, word) in head.iter().enumerate() {
        out[start + 4 * n..start + 4 * n + 4].copy_from_slice(&word.to_le_bytes());
    }
    start..out.len()
}

/// The parse of `text` in `edition` (an outline when `outline`) stored at byte `at` of `bytes`
/// by [`write`], its tree read in place from `bytes`. An error says what about the bytes is
/// not that parse: another parser's tree ([`TREE_VERSION`]), another edition's or kind of
/// parse, or a tree of another text's length.
pub fn read(
    bytes: &Bytes,
    at: usize,
    text: &Text,
    edition: Edition,
    outline: bool,
) -> Result<Parse<SourceFile>, ImageError> {
    let wrong = |what| ImageError { at, what };
    let mut head = [0u32; HEAD];
    for (n, word) in head.iter_mut().enumerate() {
        *word = u32_at(bytes, at + 4 * n).ok_or(wrong("a parse record past its bytes' end"))?;
    }
    if head[0] != MAGIC {
        return Err(wrong("not a parse record of this layout"));
    }
    if head[2] != TREE_VERSION {
        return Err(wrong("a tree of another parser version"));
    }
    if head[3] != if outline { edition as u32 + 1 } else { 0 } {
        return Err(wrong("a tree of another edition or kind of parse"));
    }
    let len = head[1] as usize;
    if at.checked_add(len).is_none_or(|end| end > bytes.len()) {
        return Err(wrong("a parse record's length past its bytes' end"));
    }
    let inside = |offset: u32, size: usize| {
        (offset as usize).checked_add(size).is_some_and(|end| end <= len)
    };
    let (count, rows, messages, messages_len) = (head[4] as usize, head[5], head[6], head[7]);
    let (tree_at, tree_len) = (head[8], head[9] as usize);
    let fits = inside(rows, count.saturating_mul(ERROR * 4))
        && inside(messages, messages_len as usize)
        && inside(tree_at, tree_len)
        && tree_at as usize >= HEAD * 4;
    if !fits {
        return Err(wrong("a part of a parse record outside it"));
    }
    let messages = &bytes[at + messages as usize..at + messages as usize + messages_len as usize];
    // alloc: the parse's errors, as parsing makes them
    let mut errors = Vec::with_capacity(count);
    for n in 0..count {
        let row = at + rows as usize + ERROR * 4 * n;
        let word = |k: usize| u32_at(bytes, row + 4 * k).map_or(0, |it| it as usize);
        let (start, end, message) = (word(0), word(1), word(2));
        let message = messages.get(message..message.saturating_add(word(3)));
        let message = message.and_then(|it| std::str::from_utf8(it).ok());
        let (Some(message), true) = (message, start <= end && end <= text.len()) else {
            return Err(wrong("an error of a parse that is not one"));
        };
        let range = TextRange::new(TextSize::from(start as u32), TextSize::from(end as u32));
        errors.push(SyntaxError::new(message, range));
    }
    let tree = FlatTree::from_image(
        bytes,
        at + tree_at as usize,
        text.clone(), // clone: a count of the file's text, which the tree shares
        parsing::expand_lazy,
    )?;
    let file = <RustLanguage as syntax_tree::Language>::kind_to_raw(SyntaxKind::SOURCE_FILE);
    let whole = TextRange::up_to(TextSize::of(text.as_str()));
    if tree.is_token(0) || tree.kind(0) != file || tree.range(0) != whole {
        return Err(wrong("a tree that is not a file of this text's length"));
    }
    // alloc: the parse's shared tree, once per file read
    Ok(Parse::with_outline(Tree::new(Arc::new(tree)), errors, outline.then_some(edition)))
}

fn pad(out: &mut Vec<u8>) {
    out.resize(out.len().next_multiple_of(8), 0);
}

fn to_u32(n: usize) -> u32 {
    u32::try_from(n).expect("a parse record of under 4 GiB")
}

fn offset(out: &[u8], start: usize) -> u32 {
    to_u32(out.len() - start)
}

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeMap,
        fs,
        path::{Path, PathBuf},
        sync::Arc,
    };

    use blaze_bytes::Bytes;
    use blaze_graph::par;
    use test_utils::project_root;

    use super::{TREE_VERSION, read, write};
    use crate::{AstNode, Edition, Parse, SourceFile, SyntaxNode, Text};

    /// The `.rs` files under `dir`, at any depth, sorted.
    fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
        for entry in fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                rust_files(&path, out);
            } else if path.extension().is_some_and(|it| it == "rs") {
                out.push(path);
            }
        }
    }

    /// The parser's test data, as paths under it and texts.
    fn corpus() -> Vec<(String, String)> {
        let root = project_root().into_std_path_buf().join("crates/parser/test_data");
        let mut paths = Vec::new();
        rust_files(&root, &mut paths);
        paths.sort();
        assert!(paths.len() > 400, "the parser's test data: {}", paths.len());
        paths
            .into_iter()
            .map(|path| {
                let name = path.strip_prefix(&root).unwrap().to_string_lossy().replace('\\', "/");
                (name, fs::read_to_string(&path).unwrap())
            })
            .collect()
    }

    /// Every element of the tree a cursor walks, parsing every lazy body, as kind and range.
    fn elements(root: SyntaxNode) -> Vec<(crate::SyntaxKind, crate::TextRange)> {
        root.descendants_with_tokens().map(|it| (it.kind(), it.text_range())).collect()
    }

    fn stored(parse: &Parse<SourceFile>) -> (Bytes, usize) {
        let mut out = vec![0; 8];
        let at = write(parse, &mut out).start;
        (Bytes::from(Arc::<[u8]>::from(out)), at)
    }

    /// Every file of the parser's test data, parsed in full and as an outline with some bodies
    /// walked, reads back from its record as the same parse: the same tree (the bodies walked
    /// before it was stored parsed, the rest not), the same errors with every body's, and the
    /// same elements walked; and a record is refused for another edition or kind of parse.
    #[test]
    fn a_stored_parse_reads_back_as_a_fresh_parse() {
        let edition = Edition::CURRENT;
        par::for_each(corpus(), |(name, text)| {
            let text = Text::from(text);
            for outline in [false, true] {
                let parse = match outline {
                    true => SourceFile::parse_outline(text.clone(), edition),
                    false => SourceFile::parse_shared(text.clone(), edition),
                };
                let tree = parse.tree.as_ref().unwrap().flat().clone();
                let walked: Vec<u32> = tree.lazies().step_by(2).collect();
                for &at in &walked {
                    let _ = tree.part(at);
                }
                let (bytes, at) = stored(&parse);
                let read = read(&bytes, at, &text, edition, outline)
                    .unwrap_or_else(|it| panic!("{name}: {it}"));
                let mapped = read.tree.as_ref().unwrap().flat().clone();
                assert!(mapped.is_read_in_place(), "{name}");
                assert!(mapped.source().ptr_eq(&text), "{name}: over the file's text");
                assert_eq!(read, parse, "{name}");
                for at in tree.lazies() {
                    assert_eq!(mapped.is_parsed(at), walked.contains(&at), "{name}: {at}");
                }
                assert_eq!(read.errors_with_bodies(), parse.errors_with_bodies(), "{name}");
                let fresh = SourceFile::parse_shared(text.clone(), edition);
                assert_eq!(elements(read.tree().syntax().clone()), elements(fresh.syntax_node()));
                let other = read_other(&bytes, at, &text, edition, outline);
                assert_eq!(other, Some("a tree of another edition or kind of parse"), "{name}");
            }
        });
    }

    fn read_other(
        bytes: &Bytes,
        at: usize,
        text: &Text,
        edition: Edition,
        outline: bool,
    ) -> Option<&'static str> {
        read(bytes, at, text, edition, !outline).err().map(|it| it.what)
    }

    /// The digest of what `text` parses to, in full and as an outline with every body
    /// walked: each tree's subtree hash (its elements' words and its text) and its errors.
    fn digest(text: &str) -> String {
        let edition = Edition::CURRENT;
        let mut hasher = blaze_ids::Hasher::new("blaze v2 syntax tree version guard 1");
        for parse in [
            SourceFile::parse_shared(Text::from(text), edition),
            SourceFile::parse_outline(Text::from(text), edition),
        ] {
            let tree = parse.tree.as_ref().unwrap().flat().clone();
            hasher.update(&tree.subtree_hash(0).to_le_bytes());
            for at in tree.lazies() {
                hasher.update(&tree.part(at).subtree_hash(0).to_le_bytes());
            }
            for error in parse.errors_with_bodies() {
                let range = error.range();
                hasher.update(&u32::from(range.start()).to_le_bytes());
                hasher.update(&u32::from(range.end()).to_le_bytes());
                hasher.part(error.to_string().as_bytes());
            }
        }
        format!("{:032x}", hasher.finish().fingerprint().0)
    }

    /// `test_data/tree_version` is the version and the digest of each file of the parser's
    /// test data under it: a file whose trees changed under the same [`TREE_VERSION`] fails
    /// here, as stored trees of the old parser would be read as this one's. Bumping the
    /// version (or a new file) records the digests again; the first run records them.
    #[test]
    fn the_tree_version_names_the_trees_parsed() {
        let path = project_root().into_std_path_buf().join("crates/syntax/test_data/tree_version");
        let digests: BTreeMap<String, String> =
            par::map(corpus(), |(name, text)| (name, digest(&text))).into_iter().collect();
        let stored = fs::read_to_string(&path).unwrap_or_default();
        let mut lines = stored.lines();
        if lines.next() == Some(TREE_VERSION.to_string().as_str()) {
            let changed: Vec<&str> = lines
                .filter_map(|line| line.split_once(' '))
                .filter(|(name, was)| digests.get(*name).is_some_and(|now| now != was))
                .map(|(name, _)| name)
                .collect();
            assert!(
                changed.is_empty(),
                "the parser makes other trees of {changed:?} under TREE_VERSION {TREE_VERSION}: \
                 bump syntax::image::TREE_VERSION, which records them again"
            );
        }
        let mut now = format!("{TREE_VERSION}\n");
        for (name, digest) in &digests {
            now.push_str(&format!("{name} {digest}\n"));
        }
        if now != stored {
            fs::write(&path, now).unwrap();
        }
    }
}
