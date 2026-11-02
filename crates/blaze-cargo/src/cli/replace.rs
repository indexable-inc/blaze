//! `blaze check --replace FROM=TO`: every reference that resolves to the item `FROM` names,
//! retargeted to the item `TO` names ([`Edit::Replace`], which blaze-intent makes through the
//! front end's name resolution), in the checkout as saved. The daemon opens the checkout's
//! head, makes the edits in its front end and commits them to the head; they are written to
//! the checkout's files here, which the daemon's watcher then sees as the head already has
//! them.

use std::path::{Component, Path};

use blaze_api::{Edit, Edited, Request, ResponseRef, ViewRef};
use blaze_vfs::Op;

use super::{daemon::Start, observe, place};

const REPLACE_USAGE: &str = "`--replace FROM=TO`: each a def path from a crate's name \
                             (`--replace std::sync::Mutex=parking_lot::Mutex`)";

/// The edits each `--replace`'s `FROM=TO` asks for, in order.
pub(crate) fn edits(pairs: &[String]) -> Result<Vec<Edit>, String> {
    let mut edits = Vec::with_capacity(pairs.len()); // alloc: CLI edge, the few edits of one command line
    for pair in pairs {
        let Some((from, to)) = pair
            .split_once('=')
            .filter(|(from, to)| !from.trim().is_empty() && !to.trim().is_empty())
        else {
            return Err(format!("`{pair}` is no FROM=TO: {REPLACE_USAGE}"));
        };
        edits.push(Edit::Replace { to: to.trim().to_owned(), from: from.trim().to_owned() });
    }
    Ok(edits)
}

/// `edits` made in the checkout at `root`, committed to its head and written to its files.
pub fn replace(root: &Path, edits: Vec<Edit>) -> Result<Edited, String> {
    let head = place::utf8(root)?;
    let mut base = None;
    // clone: the head's name, in each of the two requests
    observe::ask(
        Request::Open { view: ViewRef::Head(head.clone()) },
        Start::IfMissing,
        |response| {
            if let ResponseRef::Opened { root } = response {
                base = Some(root);
            }
            Ok(())
        },
    )?;
    let base = base.ok_or("the daemon answered the checkout's open with no root")?;
    let (mut edited, mut committed) = (None, false);
    observe::ask(Request::Edit { base, head, edits }, Start::Never, |response| {
        match response {
            ResponseRef::Edited(it) => edited = Some(it),
            ResponseRef::Committed { .. } => committed = true,
            _ => {}
        }
        Ok(())
    })?;
    let edited = edited.ok_or("the daemon answered the edit with no edits")?;
    if !committed {
        return Err(
            "the daemon made the edits and did not commit them to the checkout's head".to_owned()
        );
    }
    write(root, &edited.ops)?;
    Ok(edited)
}

/// `ops`, as the VFS applies them in order, made in the files under `root`: each file read
/// once, its edits made in memory, then written whole through a file beside it renamed over
/// it, keeping its permissions; then each rename. A path that leaves `root`, or an edit past a
/// file's end (it changed after the daemon read it), is an error before anything is written.
pub(crate) fn write(root: &Path, ops: &[Op]) -> Result<(), String> {
    let inside = |path: &str| {
        let ok = !path.is_empty()
            && Path::new(path).components().all(|it| matches!(it, Component::Normal(_)));
        if ok {
            Ok(root.join(path))
        } else {
            Err(format!("`{path}` is no path inside the checkout"))
        }
    };
    let mut files: Vec<(&str, Vec<u8>)> = Vec::new(); // alloc: CLI edge, the files one fix edits
    let mut renames = Vec::new(); // alloc: CLI edge, a renamed module's files
    for op in ops {
        match op {
            Op::Edit { at, len, path, bytes } => {
                let at_file = match files.iter().position(|(it, _)| *it == path.as_str()) {
                    Some(at_file) => at_file,
                    None => {
                        let read = std::fs::read(inside(path)?)
                            .map_err(|error| format!("{path}: {error}"))?;
                        files.push((path.as_str(), read));
                        files.len() - 1
                    }
                };
                let text = &mut files[at_file].1;
                let start = usize::try_from(*at).ok();
                let end =
                    start.zip(usize::try_from(*len).ok()).and_then(|(at, len)| at.checked_add(len));
                match (start, end) {
                    (Some(start), Some(end)) if end <= text.len() => {
                        drop(text.splice(start..end, bytes.iter().copied()))
                    }
                    _ => {
                        return Err(format!(
                            "{path}: an edit past the file's end: it changed after the daemon read it"
                        ));
                    }
                }
            }
            Op::Rename { to, from } => renames.push((inside(from)?, inside(to)?)),
            Op::Write { path, .. } | Op::Delete { path } => {
                return Err(format!(
                    "the daemon's edit writes or deletes {path} whole, which `--replace` and `--fix` do not do"
                ));
            }
        }
    }
    for (path, bytes) in &files {
        let file = root.join(path);
        let beside = file.with_file_name(format!(
            ".{}.blaze-edit",
            file.file_name().map(|it| it.to_string_lossy()).unwrap_or_default()
        ));
        let permissions =
            std::fs::metadata(&file).map_err(|error| format!("{path}: {error}"))?.permissions();
        std::fs::write(&beside, bytes).map_err(|error| format!("{}: {error}", beside.display()))?;
        std::fs::set_permissions(&beside, permissions)
            .map_err(|error| format!("{}: {error}", beside.display()))?;
        std::fs::rename(&beside, &file).map_err(|error| format!("{path}: {error}"))?;
    }
    for (from, to) in renames {
        if to.exists() {
            return Err(format!(
                "{} is there already: the rename of {} would replace it",
                to.display(),
                from.display()
            ));
        }
        std::fs::rename(&from, &to).map_err(|error| format!("{}: {error}", from.display()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use blaze_api::Edit;
    use blaze_vfs::Op;

    use super::{edits, write};

    fn pairs(pairs: &[&str]) -> Vec<String> {
        pairs.iter().map(|it| (*it).to_owned()).collect()
    }

    #[test]
    fn each_replace_is_one_edit_in_order() {
        let edits_of = |it: &[&str]| edits(&pairs(it));
        let made = edits_of(&["std::sync::Mutex=parking_lot::Mutex", "a::B = c::D"]).unwrap();
        assert_eq!(
            made,
            [
                Edit::Replace {
                    to: "parking_lot::Mutex".to_owned(),
                    from: "std::sync::Mutex".to_owned()
                },
                Edit::Replace { to: "c::D".to_owned(), from: "a::B".to_owned() },
            ]
        );
        for wrong in ["a::B", "=c::D", "a::B=", " = "] {
            assert!(edits_of(&[wrong]).is_err(), "{wrong:?}");
        }
    }

    #[test]
    fn edits_are_made_in_order_in_each_file_and_a_path_outside_is_refused() {
        let root = std::env::temp_dir().join(format!("blaze-replace-{}", std::process::id()));
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/lib.rs"), "use std::sync::Mutex;\nfn f(_: Mutex) {}\n")
            .unwrap();
        let edit = |at: u64, len: u64, bytes: &str| Op::Edit {
            at,
            len,
            path: "src/lib.rs".to_owned(),
            bytes: bytes.as_bytes().to_vec(),
        };
        write(&root, &[edit(4, 16, "parking_lot::Mutex"), edit(0, 0, "// fixed\n")]).unwrap();
        let text = std::fs::read_to_string(root.join("src/lib.rs")).unwrap();
        assert_eq!(text, "// fixed\nuse parking_lot::Mutex;\nfn f(_: Mutex) {}\n");
        let outside = Op::Edit { at: 0, len: 0, path: "../x.rs".to_owned(), bytes: Vec::new() };
        assert!(write(&root, &[outside]).is_err());
        assert!(write(&root, &[edit(999, 1, "")]).is_err(), "an edit past the end");
        assert_eq!(
            std::fs::read_to_string(root.join("src/lib.rs")).unwrap(),
            text,
            "a refused fix writes nothing"
        );
        std::fs::remove_dir_all(&root).unwrap();
    }
}
