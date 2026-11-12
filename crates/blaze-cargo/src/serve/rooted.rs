//! Where a core's message becomes one a person reads: the names in it written as paths.
//!
//! A core (the store, above all) names files relative to its driver's root, which it does not
//! know, and prints them so in its errors (`cloning dev/shm/home/v2/store/placed/87…`). The
//! daemon is the driver: before such a message is told to a client or logged for a person,
//! every name in it under one of the directories the daemon knows the message is about is
//! written as a path under the root ([`rooted`], through `FileName::under`). One function,
//! applied where the store's answers are taken ([`super::daemon`]).

use std::path::Path;

use blaze_core::FileName;

/// Whether `byte` can be part of a name: a name in a message starts where the byte before
/// it cannot.
fn of_a_name(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'.' | b'_' | b'-' | b'+' | b'@' | b'~')
}

/// `why`, a core's message, with every name under one of `dirs` written as a path under
/// `root`. A name is found where one of `dirs` starts a word (the byte before it is no part
/// of a name, so a path already written from the root, `/dev/shm/…`, is left as it is) and is
/// followed by `/` or by the word's end. Nothing else of the message changes.
pub(crate) fn rooted(why: &str, root: &Path, dirs: &[&FileName]) -> String {
    let bytes = why.as_bytes();
    let mut out = String::with_capacity(why.len() + 16); // alloc: a message for a person, once per error
    let mut at = 0;
    while at < bytes.len() {
        let starts_word = at == 0 || !of_a_name(bytes[at - 1]);
        let dir = dirs.iter().find(|dir| {
            let dir = dir.as_str().as_bytes();
            starts_word
                && bytes[at..].starts_with(dir)
                && bytes.get(at + dir.len()).is_none_or(|&next| next == b'/' || !of_a_name(next))
        });
        match dir {
            Some(dir) => {
                out.push_str(&dir.under(root).to_string());
                at += dir.as_str().len();
            }
            None => {
                // A whole character: `at` is always on a boundary, as a name's bytes are
                // ASCII or whole characters copied here.
                let next = why[at..].chars().next().expect("a character at a boundary");
                out.push(next);
                at += next.len_utf8();
            }
        }
    }
    out
}

/// The directory `file` is in, as a name; `None` for a name of one part.
pub(crate) fn dir_of(file: &FileName) -> Option<FileName> {
    FileName::new(file.as_str().rsplit_once('/')?.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn name(it: &str) -> FileName {
        FileName::new(it).unwrap()
    }

    #[test]
    fn names_under_the_known_directories_become_paths_and_paths_stay() {
        let (store, out) = (name("dev/shm/home/v2/store"), name("dev/shm/home/build/a/debug"));
        let why = "cloning dev/shm/home/v2/store/placed/871e to dev/shm/home/build/a/debug/.ha.placing: the file system of /dev/shm/home/build/a/debug/.ha.placing (tmpfs) cannot share blocks with /dev/shm/home/v2/store/placed/871e";
        let said = rooted(why, Path::new("/"), &[&store, &out]);
        assert_eq!(
            said,
            "cloning /dev/shm/home/v2/store/placed/871e to /dev/shm/home/build/a/debug/.ha.placing: the file system of /dev/shm/home/build/a/debug/.ha.placing (tmpfs) cannot share blocks with /dev/shm/home/v2/store/placed/871e"
        );
        // A name that only starts as the directory does is another name.
        assert_eq!(rooted("dev/shm/home/v2/stores/x", Path::new("/"), &[&store]), "dev/shm/home/v2/stores/x");
        assert_eq!(rooted("`dev/shm/home/v2/store`", Path::new("/r"), &[&store]), "`/r/dev/shm/home/v2/store`");
        assert_eq!(dir_of(&name("a/b/c")), Some(name("a/b")));
        assert_eq!(dir_of(&name("a")), None);
    }

    /// The invariant: a store's error that crosses the daemon's boundary names its files as
    /// paths that are there, under the driver's root. A real refusal of the store on the
    /// simulation (a disk with no room under the build directory, a second place of a value
    /// at a path that holds it), rooted as the daemon roots it: every path the message names
    /// under the store's directory or the output's is absolute, and with the root taken off
    /// is a file or a directory of the simulation's file system.
    #[test]
    fn a_stores_error_names_paths_that_are_there_once_it_crosses_the_boundary() {
        use blaze_core::sim::{Faults, Sim};
        let mut sim = Sim::new(9, Faults::default());
        let config = blaze_cas::Config {
            dir: name("home/v2/store"),
            layers: Vec::new(),
            tags: 1 << 40,
            load_tags: (1 << 40) + (1 << 32),
            loader: blaze_cas::Loader::Pumped,
        };
        let mut effects = Vec::new();
        let mut store = blaze_cas::Store::open(config, &mut effects);
        let drive = |sim: &mut Sim, store: &mut blaze_cas::Store, effects: &mut Vec<blaze_core::Effect>| {
            blaze_core::run_until_idle(sim, effects, |completion, effects| {
                if store.owns(completion.tag) {
                    store.complete(completion, effects);
                }
            })
            .expect("the effects end");
        };
        drive(&mut sim, &mut store, &mut effects);
        store.put(blaze_cas::Key(blaze_ids::ContentId([7; 32])), blaze_cas::Kind::Code, &[9; 4000]).unwrap();
        store.flush(&mut effects);
        drive(&mut sim, &mut store, &mut effects);
        let to = name("home/build/a/debug/app");
        let key = blaze_cas::Key(blaze_ids::ContentId([7; 32]));
        store.place(&key, to.clone(), &mut effects).unwrap();
        drive(&mut sim, &mut store, &mut effects);
        assert_eq!(store.placed()[0].result, Ok(()));
        // No room under the build directory: the next place of it there is refused.
        sim.fill(name("home/build"), 0);
        store.place(&key, to.clone(), &mut effects).unwrap();
        drive(&mut sim, &mut store, &mut effects);
        let placed = store.placed();
        let why = placed[0].result.clone().expect_err("a place on a full disk is refused");
        let root = Path::new("/");
        let dirs = [store.dir().clone(), dir_of(&to).unwrap()];
        let said = rooted(&why, root, &[&dirs[0], &dirs[1]]);
        let mut named = 0;
        for word in said.split(|it: char| !(it.is_ascii() && of_a_name(it as u8))) {
            let Some(rest) = word.strip_prefix('/') else {
                assert!(
                    !dirs.iter().any(|dir| word.starts_with(dir.as_str())),
                    "a name left as the core printed it: `{word}` in: {said}"
                );
                continue;
            };
            let rest = rest.trim_end_matches(|it| it == '.' || it == ':');
            if !dirs.iter().any(|dir| rest.starts_with(dir.as_str())) {
                continue;
            }
            named += 1;
            let file = name(rest);
            assert!(
                sim.exists(&file) || sim.exists(&dir_of(&file).unwrap()) || file == dirs[1] || file == dirs[0],
                "`/{rest}` is named and is not there: {said}"
            );
        }
        assert!(named > 0, "the refusal names no path under the store or the output: {said}");
    }
}
