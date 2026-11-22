//! blaze's own folder and cargo's tagged build folder are no part of the checkout a script is
//! served (blaze_build_script::served). The turso case (c1851p): a script of a member read
//! `.blaze/v2/profile.txt`, blaze rewrote it at the command's end, and the next check stopped
//! "changed while the command ran". Rules: what blaze writes under a checkout's `.blaze/v2` is
//! blaze's state, never a package's input (blaze-cli clean.rs `CheckoutEntry`); a cargo build
//! folder is the one whose `CACHEDIR.TAG` begins "Signature: 8a477f597d28d172789f06886806bc55"
//! (the Cache Directory Tagging spec), and a `target` without it is a package's folder (rule 4).

use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};

use blaze_ids::ContentId;

use super::{RealRoot, Tree, TreeRead};

const TAG: &[u8] = b"Signature: 8a477f597d28d172789f06886806bc55\n# This file is a cache directory tag created by cargo.\n";

fn put(root: &Path, rel: &str, bytes: &[u8]) {
    let path = root.join(rel);
    std::fs::create_dir_all(path.parent().expect("a file of the fixture has a folder")).expect("the fixture's folder is made");
    std::fs::write(path, bytes).expect("the fixture's file is written");
}

/// `<tmp>/blaze-served-<pid>-<name>` with a member's sources, emptied first; `blazes`: with
/// blaze's folder and a cargo-tagged `target`.
fn checkout(name: &str, blazes: bool) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("blaze-served-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    put(&dir, "Cargo.toml", b"[package]\nname = \"a\"\n");
    put(&dir, "build.rs", b"fn main() {}\n");
    put(&dir, "src/lib.rs", b"\n");
    put(&dir, "src/.blaze/kept.txt", b"not at the root\n");
    put(&dir, ".blazer/kept.txt", b"another name\n");
    if blazes {
        put(&dir, ".blaze/.gitignore", b"*\n");
        put(&dir, ".blaze/v2/profile.txt", b"profile 1\n");
        put(&dir, "target/CACHEDIR.TAG", TAG);
        put(&dir, "target/debug/build/a-0/output", b"cargo's\n");
    }
    dir
}

fn tree(dir: &Path) -> Tree {
    Tree::Dir(RealRoot::resolve(dir).expect("the fixture's folder resolves"))
}

fn answer(tree: &Tree, read: TreeRead, rel: &str) -> Option<ContentId> {
    read.answer(tree, rel).expect("the fixture's paths stay in the tree")
}

/// THE CAUSE REMOVED: a checkout holding `.blaze/` and a cargo-tagged `target/` is served
/// exactly as its twin without them: the root's listing has the same answer, and neither
/// folder nor anything in it, by name or through a link, is there. Rewriting the profile (what
/// every command does at its end) moves no answer a script could have read.
///
/// CONTROL: `src/.blaze/` and `.blazer/` are the package's and served, and an edit of one moves
/// its answer. A `target/` with sources and no tag (or a tag that is not the signature) is a
/// package's folder, served; tagging it later moves the answers that saw it (the root's
/// listing, its file), so the tag is a fact the read list holds.
#[test]
fn a_script_is_never_served_blazes_folder_or_cargos_tagged_outputs() {
    use blaze_build_script::served::{RootName, root_name, served_at_root, tagged};
    assert_eq!((root_name(b".blaze"), root_name(b"target"), root_name(b"targets"), root_name(b".blazer")), (RootName::Blaze, RootName::Target, RootName::Other, RootName::Other));
    assert!(tagged(Some(TAG)) && tagged(Some(&TAG[..43])) && !tagged(Some(&TAG[..42])) && !tagged(Some(b"signature: 8a477f597d28d172789f06886806bc55")) && !tagged(None));
    assert!(!served_at_root(b".blaze", None) && !served_at_root(b"target", Some(TAG)) && served_at_root(b"target", None) && served_at_root(b"src", None));

    let with = checkout("with", true);
    let twin = checkout("twin", false);
    let (ours, theirs) = (tree(&with), tree(&twin));
    let root = answer(&ours, TreeRead::Listing, "");
    assert!(root.is_some(), "the root is a folder");
    assert_eq!(root, answer(&theirs, TreeRead::Listing, ""), "TWIN: the root's listing is the one without `.blaze` and `target`");
    for rel in [".blaze", ".blaze/v2/profile.txt", ".blaze/.gitignore", "target", "target/CACHEDIR.TAG", "target/debug/build/a-0/output"] {
        for read in [TreeRead::File, TreeRead::Listing, TreeRead::Stat] {
            assert_eq!(answer(&ours, read, rel), None, "`{rel}` is absent to a script");
        }
    }
    symlink(with.join(".blaze"), with.join("src/via")).expect("the link is made");
    assert_eq!(answer(&ours, TreeRead::File, "src/via/v2/profile.txt"), None, "through a link too");

    let kept = answer(&ours, TreeRead::File, "src/.blaze/kept.txt");
    assert!(kept.is_some() && answer(&ours, TreeRead::File, ".blazer/kept.txt").is_some(), "CONTROL: the package's own `.blaze` names are served");
    put(&with, ".blaze/v2/profile.txt", b"profile 2, written at the command's end\n");
    assert_eq!(answer(&ours, TreeRead::Listing, ""), root, "a rewritten profile moves no answer");
    assert_eq!(answer(&ours, TreeRead::File, ".blaze/v2/profile.txt"), None);
    put(&with, "src/.blaze/kept.txt", b"edited\n");
    assert_ne!(answer(&ours, TreeRead::File, "src/.blaze/kept.txt"), kept, "CONTROL: an edit of a served file moves its answer");

    // A package's own `target/`: served while untagged (or tagged with anything but the
    // signature), absent once cargo's tag is in it.
    put(&twin, "target/gen.rs", b"pub fn f() {}\n");
    let source = answer(&theirs, TreeRead::File, "target/gen.rs");
    let listed = answer(&theirs, TreeRead::Listing, "");
    assert!(source.is_some() && answer(&theirs, TreeRead::Listing, "target").is_some(), "CONTROL: an untagged `target` is served");
    put(&twin, "target/CACHEDIR.TAG", b"not a signature\n");
    assert_eq!(answer(&theirs, TreeRead::File, "target/gen.rs"), source, "CONTROL: a tag without the signature serves it still");
    put(&twin, "target/CACHEDIR.TAG", TAG);
    assert_eq!(answer(&theirs, TreeRead::File, "target/gen.rs"), None, "tagged: absent");
    assert_ne!(answer(&theirs, TreeRead::Listing, ""), listed, "tagging moves the root's listing, so a run that listed it is rerun");

    let _ = std::fs::remove_dir_all(&with);
    let _ = std::fs::remove_dir_all(&twin);
}
