//! `[source]` replacement as cargo 1.99 does it (`SourceConfigMap` in `src/sources/config.rs`):
//! which source a source's packages come from, and how cargo describes a source (each
//! `Source::describe`). One map for the load (`load::source`, from blaze-manifest's
//! configuration) and v1's blaze-registry (from its `SourceConfig`); each reads its tables and
//! adds them here.

use anyhow::bail;
use blaze_hash::HashMap;
use blaze_resolve::{CRATES_IO_REGISTRY, SourceId, SourceKind};
use std::alloc::Global;

#[derive(Clone)]
struct Replacement {
    id: SourceId,
    /// The name of the source that replaces it and where that is defined.
    with: Option<(String, String)>,
}

/// The sources cargo knows by name, and which of them replaces which.
pub struct SourceMap {
    by_name: HashMap<String, Replacement, Global>,
    names: HashMap<SourceId, String, Global>,
    /// The `[registries]` a `replace-with` names, as cargo's `SourceId::alt_registry` reads
    /// them: one whose index is not configured, or is not a registry's URL, is not here, as
    /// cargo's `load` passes over a name `alt_registry` refuses.
    registries: HashMap<String, SourceId, Global>,
    crates_io: SourceId,
}

impl SourceMap {
    /// crates.io alone, its packages read from `crates_io` (its git or its sparse index).
    pub fn new(crates_io: SourceId) -> SourceMap {
        let mut map = SourceMap {
            by_name: HashMap::new_in(Global),
            names: HashMap::new_in(Global),
            registries: HashMap::new_in(Global),
            crates_io,
        };
        for id in [SourceId::crates_io(), crates_io] {
            map.names.insert(id, CRATES_IO_REGISTRY.to_owned()); // alloc: setup, once per configuration
            map.by_name.insert(CRATES_IO_REGISTRY.to_owned(), Replacement { id, with: None }); // alloc: as above
        }
        map
    }

    /// The source crates.io's packages are read from when nothing replaces it.
    pub fn crates_io(&self) -> SourceId {
        self.crates_io
    }

    /// `[source.<name>]` at `id`, replaced by `with` (that source's name and where its
    /// `replace-with` is defined), as cargo's `SourceConfigMap::add`.
    pub fn add(
        &mut self,
        name: &str,
        id: SourceId,
        with: Option<(String, String)>,
    ) -> anyhow::Result<()> {
        if let Some(old) = self.names.insert(id, name.to_owned()) // alloc: setup, one name per `[source]` table
            && name != CRATES_IO_REGISTRY
        {
            bail!(
                "source `{name}` defines source {id}, but that source is already defined by `{old}`\n\
                 note: Sources are not allowed to be defined multiple times."
            );
        }
        self.by_name.insert(name.to_owned(), Replacement { id, with }); // alloc: as above
        Ok(())
    }

    /// `[registries.<name>]` as cargo's `SourceId::alt_registry` makes it, for a `replace-with`
    /// that names it.
    pub fn add_registry(&mut self, name: String, id: SourceId) {
        self.registries.insert(name, id);
    }

    /// The names a `replace-with` gives that no `[source]` defines, in no order: each is a
    /// `[registries]` name, or an error once a source it replaces is read.
    pub fn unknown_replacements(&self) -> Vec<String> {
        self.by_name
            .values()
            .filter_map(|it| it.with.as_ref().map(|(name, _)| name))
            .filter(|name| !self.by_name.contains_key(name.as_str()))
            .cloned()
            .collect() // alloc: setup, once per configuration, one name per `replace-with`
    }

    /// The source the packages of `id` come from: `id` itself, or the end of its chain of
    /// `replace-with`. A vendored replacement of a git source needs `id` to be pinned, as a
    /// lock file pins it to a commit.
    pub fn replacement(&self, id: SourceId) -> anyhow::Result<SourceId> {
        let Some(original) = self.names.get(&id) else { return Ok(id) };
        let mut name = original;
        let mut at = "";
        let replaced = loop {
            let Some(replacement) = self.by_name.get(name) else {
                if let Some(id) = self.registries.get(name) {
                    break *id;
                }
                bail!(
                    "could not find a configured source with the name `{name}` when attempting \
                     to lookup `{original}` (configuration in `{at}`)"
                );
            };
            match &replacement.with {
                Some((with, definition)) => {
                    name = with;
                    at = definition;
                }
                None if replacement.id == id => return Ok(id),
                None => break replacement.id,
            }
            if name == original {
                bail!(
                    "detected a cycle of `replace-with` sources, the source `{name}` is \
                     eventually replaced with itself (configuration in `{at}`)"
                );
            }
        };
        if supports_checksums(id) && !supports_checksums(replaced) {
            bail!(
                "cannot replace `{original}` with `{name}`, the source `{original}` supports \
                 checksums, but `{name}` does not\n\na lock file compatible with `{original}` \
                 cannot be generated in this situation"
            );
        }
        if requires_precise(id) && !id.has_precise() {
            bail!(
                "the source {original} requires a lock file to be present first before it can be\n\
                 used against vendored source code\n\nremove the source replacement \
                 configuration, generate a lock file, and then\nrestore the source replacement \
                 configuration to continue the build"
            );
        }
        Ok(replaced)
    }

    /// The registry `id`'s packages come from, when `id` is a registry and what replaces it is
    /// one too.
    pub fn registry_of(&self, id: SourceId) -> anyhow::Result<Option<SourceId>> {
        if !id.is_registry() {
            return Ok(None);
        }
        let from = self.replacement(id)?;
        Ok(from.is_registry().then_some(from))
    }

    /// As cargo's `ReplacedSource::describe`: the source read, and the one it replaces unless
    /// that is crates.io read over its sparse index.
    pub fn describe(&self, source: SourceId) -> String {
        let from = self.replacement(source).unwrap_or(source);
        if from == source || (from.is_crates_io() && source.is_crates_io()) {
            describe(from)
        } else {
            format!("{} (which is replacing {source})", describe(from)) // alloc: message text, at the edge
        }
    }

    /// Whether `source` is read from another source, crates.io from its sparse index aside.
    pub fn is_replaced(&self, source: SourceId) -> bool {
        let from = self.replacement(source).unwrap_or(source);
        from != source && !(from.is_crates_io() && source.is_crates_io())
    }
}

/// How cargo's sources describe themselves (each `Source::describe`), for messages only.
pub fn describe(id: SourceId) -> String {
    match id.kind() {
        SourceKind::Registry | SourceKind::SparseRegistry | SourceKind::LocalRegistry => {
            id.display_index()
        }
        SourceKind::Directory => match blaze_source::url_file(id.url()) {
            Ok(path) => format!("directory source `{}`", path.display()), // alloc: message text, at the edge
            Err(()) => id.to_string(),                                    // alloc: as above
        },
        SourceKind::Git(_) => format!("Git repository {id}"), // alloc: as above
        SourceKind::Path => match blaze_source::url_file(id.url()) {
            Ok(path) => path.display().to_string(), // alloc: as above
            Err(()) => id.to_string(),              // alloc: as above
        },
    }
}

fn supports_checksums(id: SourceId) -> bool {
    matches!(
        id.kind(),
        SourceKind::Registry
            | SourceKind::SparseRegistry
            | SourceKind::LocalRegistry
            | SourceKind::Directory
    )
}

fn requires_precise(id: SourceId) -> bool {
    matches!(id.kind(), SourceKind::Git(_) | SourceKind::Directory)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn registry(url: &str) -> SourceId {
        SourceId::for_registry(&url.parse().unwrap()).unwrap()
    }

    fn error<T>(result: anyhow::Result<T>) -> String {
        match result {
            Ok(_) => panic!("no error"),
            Err(err) => format!("{err:#}"),
        }
    }

    /// The order cargo 1.99's `SourceConfigMap::add` names the two sources in: the one being
    /// added, then the one that had the location (v1's order; the load's copy had them swapped).
    #[test]
    fn a_source_defined_twice_names_the_second_first() {
        let mut map = SourceMap::new(SourceId::crates_io_sparse());
        let at = registry("sparse+https://a.example.com/");
        map.add("x", at, None).unwrap();
        assert_eq!(
            error(map.add("y", at, None)),
            format!(
                "source `y` defines source {at}, but that source is already defined by `x`\n\
                 note: Sources are not allowed to be defined multiple times."
            )
        );
        // crates.io may be defined again at its own location.
        map.add(
            CRATES_IO_REGISTRY,
            SourceId::crates_io_sparse(),
            Some(("x".to_owned(), "c".to_owned())),
        )
        .unwrap();
    }

    #[test]
    fn crates_io_is_read_from_the_index_given() {
        let map = SourceMap::new(SourceId::crates_io_sparse());
        assert_eq!(map.crates_io(), SourceId::crates_io_sparse());
        assert_eq!(map.replacement(SourceId::crates_io()).unwrap(), SourceId::crates_io_sparse());
        assert_eq!(
            map.replacement(SourceId::crates_io_sparse()).unwrap(),
            SourceId::crates_io_sparse()
        );
        assert!(!map.is_replaced(SourceId::crates_io()));
        let git = SourceMap::new(SourceId::crates_io());
        assert_eq!(git.replacement(SourceId::crates_io()).unwrap(), SourceId::crates_io());
        let other = registry("sparse+https://other.example.com/");
        assert_eq!(map.replacement(other).unwrap(), other);
        assert_eq!(map.registry_of(other).unwrap(), Some(other));
    }

    /// A `replace-with` naming a `[registries]` entry; one whose index cargo's `alt_registry`
    /// refuses is left out by the caller, and is then cargo's "could not find" (cargo's `load`
    /// takes `if let Ok(alt_id) = SourceId::alt_registry(..)`). v1's map read the index lazily
    /// and said "invalid index URL for registry" instead; its callers gave it parsed URLs only,
    /// so no v1 build reached that.
    #[test]
    fn a_replacement_may_be_a_registry_by_name() {
        let mut map = SourceMap::new(SourceId::crates_io_sparse());
        map.add(
            CRATES_IO_REGISTRY,
            SourceId::crates_io_sparse(),
            Some(("mine".to_owned(), "/w/.cargo/config.toml".to_owned())),
        )
        .unwrap();
        assert_eq!(map.unknown_replacements(), ["mine"]);
        assert_eq!(
            error(map.replacement(SourceId::crates_io())),
            "could not find a configured source with the name `mine` when attempting to lookup \
             `crates-io` (configuration in `/w/.cargo/config.toml`)"
        );
        let mine = SourceId::for_alt_registry(
            &"sparse+https://mine.example.com/index/".parse().unwrap(),
            "mine",
        )
        .unwrap();
        map.add_registry("mine".to_owned(), mine);
        assert_eq!(map.replacement(SourceId::crates_io()).unwrap(), mine);
        assert!(map.is_replaced(SourceId::crates_io()));
        assert_eq!(
            map.describe(SourceId::crates_io()),
            format!("{} (which is replacing {})", mine.display_index(), SourceId::crates_io())
        );
    }

    #[test]
    fn sources_describe_themselves_as_cargos_do() {
        let git = SourceId::for_git(
            &"https://github.com/o/r".parse().unwrap(),
            blaze_resolve::GitReference::DefaultBranch,
        )
        .unwrap();
        assert_eq!(describe(git), format!("Git repository {git}"));
        assert_eq!(
            describe(SourceId::crates_io_sparse()),
            SourceId::crates_io_sparse().display_index()
        );
        #[cfg(unix)]
        {
            let dir = SourceId::for_directory(std::path::Path::new("/w/vendor")).unwrap();
            assert_eq!(describe(dir), "directory source `/w/vendor`");
            let path = SourceId::for_path(std::path::Path::new("/w/a")).unwrap();
            assert_eq!(describe(path), "/w/a");
        }
    }
}
