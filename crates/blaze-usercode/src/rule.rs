//! Cargo's rule, for a run that read what can't be traced (#1450): the clock, randomness,
//! the network, a descriptor it was given. What it read of those can't be seen again, so what
//! the script itself said reruns it is added to what it was seen to read: a path its
//! `rerun-if-changed` names (a directory meaning all below it) and a variable its
//! `rerun-if-env-changed` names. Cargo's rule is added to what it was seen to read, never put
//! in its place: a run is never kept for longer than either would keep it.
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
            return CargoRule { paths: Vec::new(), package: watched.paths.into_iter().next(), env }; // alloc: none, an empty list
        }
        CargoRule { paths: watched.paths, package: None, env }
    }
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
}
