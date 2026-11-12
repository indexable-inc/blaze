//! Cargo's rule, for a run that read what can't be traced (#1450): the clock, randomness,
//! the network, a descriptor it was given. What it read of those can't be seen again, so what
//! the script itself said reruns it is added to what it was seen to read: a path its
//! `rerun-if-changed` names (a directory meaning all below it) and a variable its
//! `rerun-if-env-changed` names. Cargo's rule is added to what it was seen to read of its own
//! package, never put in its place there: a run is never kept for longer than cargo keeps it.
//! What such a run read outside its package (the checkout's other files, through `git`; the
//! toolchain's) is no input of a script that said what reruns it ([`widen`] says why).
//!
//! Cargo's default for a script that names no path, any file of its package, is added only
//! for a run that read a file the sandbox did not see opened ([`Untraceable::Descriptor`]):
//! only then can a file of the package be an input no read names. The clock, randomness and
//! the network are no file: no edit of the package changes what they gave, and every file the
//! run did read is traced, so watching the whole package for them reran the script of a
//! package on every edit of any of its sources (ripgrep's, which runs `git`, on each edit of
//! `crates/core/main.rs`) to read nothing other than before (AGENTS.md `useful-not-parity`:
//! cargo watches the package because it cannot see what a script reads; blaze can).

use std::{
    ffi::{OsStr, OsString},
    path::PathBuf,
};

use blaze_build_script::Watched;
use blaze_core::{Traced, Untraceable};

use crate::trace::{self, Inputs, PACKAGE, TREE};

/// What cargo watches of a run.
#[derive(PartialEq, Eq, Default, Debug)]
pub struct CargoRule {
    /// The paths `rerun-if-changed` named, made whole against the package's directory.
    pub paths: Vec<PathBuf>,
    /// The package's directory, when no `rerun-if-changed` was printed: all of it is watched
    /// for a run that read a file the sandbox did not see opened, and for no other ([`widen`]).
    pub package: Option<PathBuf>,
    /// The variables `rerun-if-env-changed` named.
    pub env: Vec<OsString>,
    /// The package's directory, for a script that said what reruns it (it printed a
    /// `rerun-if-changed` or a `rerun-if-env-changed`): what a run of it that read what
    /// can't be traced read outside this directory is no input of it ([`widen`]). None for a
    /// rule made without the package (a test's), which narrows nothing.
    pub said_in: Option<PathBuf>,
}

/// Adds cargo's rule to what `traced` has, as `inputs` has it now, when the run read what
/// can't be traced; leaves it as it is otherwise.
pub fn widen<I: Inputs>(
    traced: &mut Traced,
    rule: &CargoRule,
    inputs: &mut I,
) -> Result<(), I::Error> {
    if traced.untraceable.is_empty() {
        return Ok(());
    }
    // A script that said what reruns it, and read what can't be traced: what it read
    // outside its own package is no input. Such a run is no function of what it read (the
    // clock moves under it), so reading more of the machine makes its kept result no truer,
    // and real projects rest on cargo's rule here: reth's `reth-node-core` stamps its build
    // time and asks `git` whether the checkout is dirty, prints `rerun-if-changed=build.rs`,
    // and says in its source that debug builds must not run it again. Traced, its run read
    // 2,916 files and 590 listings of the checkout through `git`: every build hashed them
    // all (13 s of a 38 s edit), every edit of any source ran it again, and its new time
    // stamp made its crate and the six above it again (2026-11-12). What it read of its own
    // package stays, with what it named: never kept for longer than cargo keeps it.
    if let (Some(own), None) = (&rule.said_in, &rule.package)
        && !NARROWS_NOTHING.get()
    {
        traced.files.retain(|(path, _)| path.starts_with(own));
        traced.dirs.retain(|(path, _)| path.starts_with(own));
    }
    for path in &rule.paths {
        let is_dir = blaze_mem::frame::task(blaze_mem::Tag::Usercode, |frame| {
            inputs.names(path, &mut trace::Names::new_in(frame))
        })?;
        match is_dir {
            // Cargo watches a directory's every file, at any depth.
            true => {
                if let Some(hash) = inputs.tree(path, false)? {
                    traced.dirs.push((path.join(TREE), hash));
                }
            }
            false => {
                let hash = inputs.file(path)?;
                // clone: once per run kept: the entry is the result's own.
                traced.files.push((path.clone(), hash));
            }
        }
    }
    // Any file of the package, only where a file read went unseen (the module's docs).
    if let Some(package) = &rule.package
        && traced.untraceable.contains(&Untraceable::Descriptor)
    {
        match trace::package(inputs, package)? {
            Some(hash) => traced.dirs.push((package.join(PACKAGE), hash)),
            None => traced.files.push((package.join(PACKAGE), None)),
        }
    }
    for name in &rule.env {
        // alloc: once per run kept: the value is the result's own.
        let value = inputs.env(name).map(OsStr::to_owned);
        // clone: as above, the name.
        traced.env.push((name.clone(), value));
    }
    trace::normalize(traced);
    // A path both read and watched is one input: the watched entry is the read one's value.
    traced.files.dedup_by(|a, b| a.0 == b.0);
    traced.env.dedup_by(|a, b| a.0 == b.0);
    Ok(())
}

impl CargoRule {
    /// The rule of what cargo [`watched`](blaze_build_script::watched) of a script.
    pub fn of(watched: Watched) -> CargoRule {
        // alloc: the rule owns its names, made once as a run ends.
        let env = watched.env.into_iter().map(OsString::from).collect();
        if watched.whole_package {
            // alloc: none, an empty list
            return CargoRule {
                paths: Vec::new(),
                package: watched.paths.into_iter().next(),
                env,
                said_in: None,
            };
        }
        CargoRule { paths: watched.paths, package: None, env, said_in: None }
    }

    /// The rule of a script of the package in `dir`: [`CargoRule::said_in`] set when the
    /// script said what reruns it.
    pub fn of_package(watched: Watched, dir: &std::path::Path) -> CargoRule {
        let mut rule = CargoRule::of(watched);
        if rule.package.is_none() {
            rule.said_in = Some(dir.to_path_buf()); // alloc: once a run kept
        }
        rule
    }
}

thread_local! {
    /// A planted bug, for a test's control: [`widen`] keeps what an untraceable run that said
    /// its reruns read outside its package, as until 2026-11-12. Never set outside a test.
    #[doc(hidden)]
    pub static NARROWS_NOTHING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, path::Path};

    use blaze_build_script::{Output, watched};

    use super::*;
    use crate::{index::Shape, trace::DIRECTORY};

    fn of_directives(rerun_if_changed: &[&str], rerun_if_env_changed: &[&str]) -> CargoRule {
        let output = Output {
            rerun_if_changed: rerun_if_changed.iter().map(PathBuf::from).collect(),
            rerun_if_env_changed: rerun_if_env_changed.iter().map(|it| it.to_string()).collect(),
            ..Output::default()
        };
        CargoRule::of(watched(&output, Path::new("/p")))
    }

    #[derive(Default)]
    struct Fixed {
        files: BTreeMap<PathBuf, [u8; 32]>,
        dirs: BTreeMap<PathBuf, Vec<&'static str>>,
        env: Vec<(OsString, OsString)>,
    }

    impl Inputs for Fixed {
        type Error = String;
        fn entry(&mut self, path: &Path) -> Result<crate::trace::Entry, String> {
            Ok(match (self.dirs.contains_key(path), self.files.get(path)) {
                (true, _) => crate::trace::Entry::Dir,
                (false, Some(hash)) => {
                    crate::trace::Entry::File(blaze_ids::ContentId(*hash), blaze_vfs::Mode::Regular)
                }
                (false, None) => crate::trace::Entry::Missing,
            })
        }
        fn file(&mut self, path: &Path) -> Result<Option<[u8; 32]>, String> {
            if self.dirs.contains_key(path) {
                return Ok(Some(DIRECTORY));
            }
            Ok(self.files.get(path).copied())
        }
        fn dir(&mut self, path: &Path) -> Result<Option<[u8; 32]>, String> {
            Ok(self.dirs.get(path).map(|names| trace::listing(names.iter().map(OsStr::new))))
        }
        fn env(&mut self, name: &OsStr) -> Option<&OsStr> {
            self.env.iter().find(|it| it.0 == name).map(|it| it.1.as_os_str())
        }
        fn env_all(&mut self) -> &[(OsString, OsString)] {
            self.env.sort();
            &self.env
        }
        fn names(&mut self, path: &Path, out: &mut trace::Names<'_>) -> Result<bool, String> {
            let Some(names) = self.dirs.get(path) else { return Ok(false) };
            for name in names {
                out.push(OsStr::new(name), self.dirs.contains_key(&path.join(name)));
            }
            out.sort();
            Ok(true)
        }
    }

    fn fixed() -> Fixed {
        Fixed {
            files: [
                ("/p/build.rs".into(), [1; 32]),
                ("/p/src/lib.rs".into(), [2; 32]),
                ("/p/data/a".into(), [3; 32]),
            ]
            .into(),
            dirs: [
                ("/p".into(), vec!["build.rs", "src", "data"]),
                ("/p/src".into(), vec!["lib.rs"]),
                ("/p/data".into(), vec!["a"]),
            ]
            .into(),
            env: vec![("VERSION".into(), "1".into())],
        }
    }

    #[test]
    fn a_run_traced_whole_keeps_only_what_it_read() {
        let mut traced =
            Traced { files: vec![("/p/build.rs".into(), Some([1; 32]))], ..Traced::default() };
        let before = traced.clone();
        widen(&mut traced, &of_directives(&[], &[]), &mut fixed()).unwrap();
        assert_eq!(traced, before);
    }

    /// The whole package is watched for a file read the sandbox did not see, and for nothing
    /// else that can't be traced: an edit of a file the run never read reruns no script that
    /// read the clock, randomness or the network.
    #[test]
    fn only_an_unseen_file_read_watches_the_whole_package() {
        let read = |what: Untraceable| Traced {
            files: vec![("/p/build.rs".into(), Some([1; 32]))],
            untraceable: vec![what],
            ..Traced::default()
        };
        let mut changed = fixed();
        changed.files.insert("/p/src/lib.rs".into(), [9; 32]);
        for what in [Untraceable::Clock, Untraceable::Random, Untraceable::Network] {
            let mut traced = read(what);
            widen(&mut traced, &of_directives(&[], &[]), &mut fixed()).unwrap();
            assert_eq!(traced, read(what), "{what:?} watches no file it did not read");
            let shape = Shape::of(&traced);
            assert_eq!(shape.values(&mut changed).unwrap(), traced, "{what:?}");
        }
        let mut traced = read(Untraceable::Descriptor);
        widen(&mut traced, &of_directives(&[], &[]), &mut fixed()).unwrap();
        assert_eq!(traced.dirs.len(), 1);
        assert_eq!(traced.dirs[0].0, Path::new("/p").join(PACKAGE));
        let shape = Shape::of(&traced);
        assert_eq!(shape.values(&mut fixed()).unwrap(), traced);
        assert_ne!(shape.values(&mut changed).unwrap(), traced);
    }

    #[test]
    fn a_run_that_read_the_clock_is_watched_by_what_its_script_named() {
        let clock = || Traced {
            files: vec![("/p/build.rs".into(), Some([1; 32]))],
            untraceable: vec![Untraceable::Clock],
            ..Traced::default()
        };
        let mut changed = fixed();
        changed.files.insert("/p/src/lib.rs".into(), [9; 32]);

        let mut named = clock();
        let rule = of_directives(&["data", "build.rs"], &["VERSION"]);
        widen(&mut named, &rule, &mut fixed()).unwrap();
        assert_eq!(named.files, vec![("/p/build.rs".into(), Some([1; 32]))]);
        assert_eq!(named.dirs[0].0, Path::new("/p/data").join(TREE));
        assert_eq!(named.env, vec![("VERSION".into(), Some("1".into()))]);
        let shape = Shape::of(&named);
        let mut data = fixed();
        data.files.insert("/p/data/a".into(), [7; 32]);
        assert_ne!(shape.values(&mut data).unwrap(), named);
        // What the rule does not name is not watched.
        assert_eq!(shape.values(&mut changed).unwrap(), named);

        let mut env_only = clock();
        widen(&mut env_only, &of_directives(&[], &["VERSION"]), &mut fixed()).unwrap();
        assert!(env_only.dirs.is_empty());
        assert_eq!(env_only.env, vec![("VERSION".into(), Some("1".into()))]);
    }
    /// The run of a script that stamps its build's time and asks `git` about the checkout,
    /// and says `rerun-if-changed=build.rs`: what it read, and the same widened by its rule.
    fn stamped(package: Option<&Path>) -> (Traced, Traced) {
        let output = Output {
            rerun_if_changed: vec![PathBuf::from("build.rs")],
            ..Output::default()
        };
        let watched = watched(&output, Path::new("/p"));
        let rule = match package {
            Some(dir) => CargoRule::of_package(watched, dir),
            None => CargoRule::of(watched),
        };
        let read = Traced {
            files: vec![
                ("/p/src/lib.rs".into(), Some([2; 32])),
                ("/w/.git/index".into(), Some([7; 32])),
                ("/w/crates/other/src/lib.rs".into(), Some([8; 32])),
            ],
            dirs: vec![("/w/crates".into(), [9; 32])],
            untraceable: vec![Untraceable::Clock],
            ..Traced::default()
        };
        // clone: a test's trace, before and after
        let mut widened = Traced {
            files: read.files.clone(),
            dirs: read.dirs.clone(),
            env: read.env.clone(),
            untraceable: read.untraceable.clone(),
        };
        widen(&mut widened, &rule, &mut fixed()).unwrap();
        (read, widened)
    }

    /// The invariant: a run that read what can't be traced, of a script that said what
    /// reruns it, is kept by what it named and by what it read of its own package, and by
    /// nothing it read elsewhere: an edit of another crate's source, or `git` writing its
    /// index, changes no input of it, so it is found kept and not run again. A run that read
    /// nothing untraceable keeps every read: it is a function of them.
    #[test]
    fn an_untraceable_run_that_said_its_reruns_is_keyed_by_its_package_alone() {
        let (_, widened) = stamped(Some(Path::new("/p")));
        let files: Vec<&Path> = widened.files.iter().map(|it| it.0.as_path()).collect();
        assert_eq!(files, [Path::new("/p/build.rs"), Path::new("/p/src/lib.rs")]);
        assert!(widened.dirs.is_empty(), "{:?}", widened.dirs);
        // Traceable: nothing is taken out, and nothing added.
        let mut pure = Traced {
            files: vec![("/w/.git/index".into(), Some([7; 32]))],
            ..Traced::default()
        };
        let output =
            Output { rerun_if_changed: vec![PathBuf::from("build.rs")], ..Output::default() };
        let rule = CargoRule::of_package(watched(&output, Path::new("/p")), Path::new("/p"));
        widen(&mut pure, &rule, &mut fixed()).unwrap();
        assert_eq!(pure.files.len(), 1, "{:?}", pure.files);
    }

    /// The control: with nothing narrowed, as until 2026-11-12, the run is keyed by the
    /// checkout's files it read through `git`, so any edit of any source runs it again.
    #[test]
    fn the_oracle_catches_an_untraceable_run_keyed_by_the_whole_checkout() {
        NARROWS_NOTHING.set(true);
        let (_, widened) = stamped(Some(Path::new("/p")));
        NARROWS_NOTHING.set(false);
        assert!(
            widened.files.iter().any(|it| it.0 == Path::new("/w/crates/other/src/lib.rs")),
            "planted: another crate's source is an input of the run: {:?}",
            widened.files
        );
        assert!(!widened.dirs.is_empty(), "planted: the checkout's listings are inputs");
    }

}
