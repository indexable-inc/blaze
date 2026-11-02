//! The source root content trie ([`RootContent`]): edited in place it gives what building it
//! from its files gives, its shape is the one its files give, and an edit hashes only its path.

use std::collections::BTreeMap;

use blaze_ids::{ContentId, rng::SplitMix64 as Rng};
use blaze_mem::{Frame, Tag, frame};
use blaze_hash::HashMap;
use crate::{FileId, VfsPath};

use super::{MAX_DEPTH, NO_NODE, RootContent, digit};
use crate::{SourceRoot, SourceRootId, content};

const ROOT: SourceRootId = SourceRootId(0);

/// The key and path hash of path number `at`.
fn key(at: u32) -> (Box<[u8]>, ContentId) {
    let key = content::path_key(&VfsPath::new_virtual_path(format!("/r/d{}/m{at}.rs", at % 7)));
    let path = ContentId::of(SourceRoot::PATH_DOMAIN, &key);
    (key, path)
}

fn text(at: u8) -> ContentId {
    ContentId::of("blaze v2 base-db root content test", &[at])
}

/// Each file of a model root: its path number and its text number.
type Model = BTreeMap<u32, (u32, u8)>;

fn built(is_library: bool, model: &Model, old: Option<&RootContent>, frame: &Frame) -> RootContent {
    let mut files = Vec::new_in(frame);
    for (&file, &(path, at)) in model {
        files.push((FileId::from_raw(file), key(path).0, text(at)));
    }
    RootContent::new(is_library, files, old, frame)
}

fn refreshed(content: RootContent, frame: &Frame) -> RootContent {
    let mut contents = HashMap::default();
    contents.insert(ROOT, content);
    RootContent::refresh(&mut contents, &[ROOT], frame);
    contents.remove(&ROOT).expect("the root put in")
}

/// The root's content ID, built from `model` alone.
fn from_scratch(is_library: bool, model: &Model, frame: &Frame) -> ContentId {
    refreshed(built(is_library, model, None, frame), frame).content
}

/// Each node's count is its files', a node is a leaf exactly when it has at most
/// [`SourceRoot::LEAF`] files or is at the last digit, each file is in the leaf its path leads
/// to, and no node is out of date.
fn assert_canonical(content: &RootContent) {
    fn walk(content: &RootContent, node: u32, depth: usize, seen: &mut usize) -> u32 {
        let this = &content.nodes[node as usize];
        assert!(!this.dirty, "node {node} out of date after a refresh");
        let count = match this.leaf {
            true => {
                for &at in &this.entries {
                    let entry = &content.entries[at as usize];
                    assert_eq!(content.files.get(&entry.file), Some(&at));
                    assert_eq!(content.find(&entry.key, &entry.path), Some(at));
                }
                *seen += this.entries.len();
                this.entries.len() as u32
            }
            false => {
                assert!(this.entries.is_empty());
                let mut count = 0;
                for (place, &child) in this.children.iter().enumerate() {
                    if child == NO_NODE {
                        continue;
                    }
                    let under = walk(content, child, depth + 1, seen);
                    assert_ne!(under, 0, "an empty child of node {node}");
                    count += under;
                    let child = &content.nodes[child as usize];
                    if child.leaf {
                        for &at in &child.entries {
                            assert_eq!(digit(&content.entries[at as usize].path, depth), place);
                        }
                    }
                }
                count
            }
        };
        assert_eq!(this.count, count, "node {node}'s count");
        let leaf = count as usize <= SourceRoot::LEAF || depth == MAX_DEPTH;
        assert_eq!(this.leaf, leaf, "node {node} at depth {depth}");
        count
    }
    let mut seen = 0;
    walk(content, 0, 0, &mut seen);
    assert_eq!(seen, content.files.len());
}

fn dirty_nodes(content: &RootContent) -> usize {
    let (mut leaves, mut inner) = (Vec::new(), Vec::new());
    frame::task(Tag::Parse, |frame| {
        let (mut l, mut i) = (Vec::new_in(frame), Vec::new_in(frame));
        let mut deepest = 0;
        content.dirty_nodes(ROOT, 0, 0, &mut l, &mut i, &mut deepest);
        leaves.extend(l.iter().copied());
        inner.extend(i.iter().copied());
    });
    leaves.len() + inner.len()
}

/// An edit of the model, made to the trie too: a file put at a path (taking it from a file
/// that had it), taken out, or given another text.
fn edit(rng: &mut Rng, model: &mut Model, content: &mut RootContent, grow: bool) {
    let file = rng.below(900) as u32;
    let roll = rng.below(10);
    if (grow && roll < 7) || (!grow && roll < 2) {
        let path = rng.below(1200) as u32;
        let at = rng.below(4) as u8;
        model.retain(|&known, &mut (known_path, _)| known == file || known_path != path);
        model.insert(file, (path, at));
        let (key, hash) = key(path);
        content.insert(FileId::from_raw(file), key, hash, text(at));
    } else if roll < 8 {
        let had = model.remove(&file).is_some();
        assert_eq!(content.remove(FileId::from_raw(file)), had);
    } else {
        let at = rng.below(4) as u8;
        let changed = match model.get_mut(&file) {
            Some((_, known)) => std::mem::replace(known, at) != at,
            None => false,
        };
        assert_eq!(content.set(FileId::from_raw(file), text(at)), changed);
    }
}

/// Edited in place, through roots that grow past several levels and shrink back to one, the
/// trie's content ID is the one built from its files, and its shape is theirs.
#[test]
fn a_root_edited_in_place_is_the_root_built_from_its_files() {
    for seed in 0..4 {
        let mut rng = Rng::new(seed);
        let mut model = Model::new();
        frame::task(Tag::Parse, |frame| {
            let mut content = refreshed(built(false, &model, None, frame), frame);
            for step in 0..2400 {
                edit(&mut rng, &mut model, &mut content, step < 1200);
                content = refreshed(content, frame);
                assert_canonical(&content);
                if step % 4 == 0 || step == 2399 {
                    let scratch = from_scratch(false, &model, frame);
                    assert_eq!(content.content, scratch, "seed {seed} step {step}");
                }
            }
        });
    }
}

/// A root set again takes the hash of every node whose files are what they were.
#[test]
fn a_root_set_again_keeps_the_hashes_of_what_is_unchanged() {
    let mut rng = Rng::new(9);
    let mut model = Model::new();
    for file in 0..2000 {
        model.insert(file, (file, rng.below(4) as u8));
    }
    frame::task(Tag::Parse, |frame| {
        let old = refreshed(built(false, &model, None, frame), frame);
        let again = built(false, &model, Some(&old), frame);
        assert_eq!(dirty_nodes(&again), 0, "nothing changed");
        assert!(!again.stale);
        assert_eq!(again.content, old.content);

        model.insert(5000, (5000, 1));
        let grown = built(false, &model, Some(&old), frame);
        // Its path, and the leaves of the leaf it may split.
        let levels = 2 + (2000f64.ln() / 16f64.ln()).ceil() as usize;
        let most = levels + SourceRoot::FANOUT;
        assert!(dirty_nodes(&grown) <= most, "one file added: {} nodes", dirty_nodes(&grown));
        let grown = refreshed(grown, frame);
        assert_canonical(&grown);
        assert_eq!(grown.content, from_scratch(false, &model, frame));

        let library = refreshed(built(true, &model, Some(&grown), frame), frame);
        assert_ne!(library.content, grown.content);
        assert_eq!(library.content, from_scratch(true, &model, frame));
    });
}

#[test]
fn different_files_give_different_ids() {
    frame::task(Tag::Parse, |frame| {
        let one: Model = [(1, (1, 0))].into_iter().collect();
        let other_text: Model = [(1, (1, 1))].into_iter().collect();
        let other_path: Model = [(1, (2, 0))].into_iter().collect();
        let other_file: Model = [(2, (1, 0))].into_iter().collect();
        let empty = from_scratch(false, &Model::new(), frame);
        let ids = [one, other_text, other_path].map(|model| from_scratch(false, &model, frame));
        assert_ne!(ids[0], ids[1]);
        assert_ne!(ids[0], ids[2]);
        assert_ne!(ids[0], empty);
        // The file ID is not content: the same path and text in another file is the same root.
        assert_eq!(from_scratch(false, &other_file, frame), ids[0]);
    });
}

/// An edit of one file of a large root hashes its leaf and the nodes above it, and a text
/// edit, or a file taken out and put back, allocates nothing once warm.
#[test]
fn an_edit_of_a_large_root_hashes_its_path_and_allocates_nothing() {
    const FILES: u32 = 4096;
    let model: Model = (0..FILES).map(|file| (file, (file, 0))).collect();
    frame::task(Tag::Parse, |frame| {
        let mut contents = HashMap::default();
        contents.insert(ROOT, refreshed(built(false, &model, None, frame), frame));
        let file = FileId::from_raw(17);
        let (key17, path17) = key(17);
        let index = contents.get_mut(&ROOT).expect("the root");
        assert!(index.set(file, text(1)));
        let levels = 2 + (FILES as f64).log(16.0).ceil() as usize;
        assert!(dirty_nodes(index) <= levels, "a text edit: {} nodes", dirty_nodes(index));
        RootContent::refresh(&mut contents, &[ROOT], frame);

        let cycle = |contents: &mut HashMap<SourceRootId, RootContent>, key: Box<[u8]>, at: u8| {
            let index = contents.get_mut(&ROOT).expect("the root");
            index.set(file, text(at));
            index.remove(file);
            index.insert(file, key, path17, text(at));
            RootContent::refresh(contents, &[ROOT], frame);
        };
        cycle(&mut contents, key17.clone(), 2);
        let key = key17.clone();
        let ((), used) = blaze_alloc_count::measure(|| cycle(&mut contents, key, 3));
        assert_eq!(used.allocations, 0, "an edit, a removal and an insert, warm: {used:?}");
        let mut model = model.clone();
        model.insert(17, (17, 3));
        assert_eq!(contents[&ROOT].content, from_scratch(false, &model, frame));
    });
}
