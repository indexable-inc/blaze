//! Every edit against the text edit an agent would make instead: the same bytes and VFS root,
//! a tree equal to a fresh parse of them, and the same changed items as a whole-file diff of
//! the two finds. Rename and replace over a module graph, against the front end.

mod budget;
mod db;
mod edits;
mod rename;
mod replace;
mod retarget;

use std::collections::BTreeMap;

use blaze_core::FileName;
use blaze_ids::ContentId;
use blaze_vfs::{Mode, Op, Vfs};
use syntax::{Edition, SourceFile, SyntaxKind, TextRange, WalkEvent, view::Node};

/// Every node and token of a tree, by kind and range, bodies parsed.
pub(crate) fn shape(node: Node<'_>) -> Vec<(SyntaxKind, TextRange)> {
    node.preorder_with_tokens()
        .filter_map(|event| match event {
            WalkEvent::Enter(it) => Some((it.kind(), it.text_range())),
            WalkEvent::Leave(_) => None,
        })
        .collect()
}

/// `tree`, reparsed in place, is what a fresh parse of `text` gives, errors and all.
#[track_caller]
pub(crate) fn assert_fresh(tree: &syntax::Parse<SourceFile>, text: &str) {
    let fresh = SourceFile::parse(text, Edition::CURRENT);
    assert_eq!(shape(tree.root()), shape(fresh.root()), "{text}");
    let errors = |parse: &syntax::Parse<SourceFile>| {
        let mut all: Vec<(TextRange, String)> =
            parse.errors_with_bodies().iter().map(|it| (it.range(), it.to_string())).collect();
        all.sort_unstable_by_key(|(range, error)| (range.start(), range.end(), error.clone()));
        all
    };
    assert_eq!(errors(tree), errors(&fresh), "{text}");
}

fn write(path: &str, bytes: &str) -> Op {
    Op::Write { mode: Mode::Regular, path: path.to_owned(), bytes: bytes.as_bytes().to_vec() }
}

/// The roots `ops` and plain writes of `after` make from a tree of `before`: equal when the
/// edit's bytes are the text edit's.
pub(crate) fn roots(before: &BTreeMap<String, String>, ops: &[Op], after: &BTreeMap<String, String>) -> (ContentId, ContentId) {
    let (mut vfs, _) = Vfs::recover(FileName::new("journal").unwrap(), 0, &[]);
    let writes: Vec<Op> = before.iter().map(|(path, text)| write(path, text)).collect();
    let base = vfs.apply(vfs.empty(), &writes).unwrap();
    let edited = vfs.apply(base, ops).unwrap();
    let written: Vec<Op> = after.iter().map(|(path, text)| write(path, text)).collect();
    let from_empty = vfs.apply(vfs.empty(), &written).unwrap();
    (edited, from_empty)
}

/// `ops` applied to `files` as the VFS applies them, in order.
pub(crate) fn applied(files: &BTreeMap<String, String>, ops: &[Op]) -> BTreeMap<String, String> {
    let mut files = files.clone();
    for op in ops {
        match op {
            Op::Edit { at, len, path, bytes } => {
                let text = files.get_mut(path).expect("an edited file");
                let at = *at as usize;
                text.replace_range(at..at + *len as usize, std::str::from_utf8(bytes).unwrap());
            }
            Op::Rename { to, from } => {
                let moved: Vec<String> = files
                    .keys()
                    .filter(|it| *it == from || it.starts_with(&format!("{from}/")))
                    .cloned()
                    .collect();
                for old in moved {
                    let text = files.remove(&old).unwrap();
                    files.insert(format!("{to}{}", &old[from.len()..]), text);
                }
            }
            Op::Write { .. } | Op::Delete { .. } => panic!("an edit made {op:?}"),
        }
    }
    files
}
