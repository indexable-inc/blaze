//! `[source]` replacement as cargo 1.99 does it (`SourceConfigMap` in `src/sources/config.rs`):
//! which source a source's packages come from, and every package of a lock file fetched from
//! there into the directory cargo would have it in.

use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
};

use anyhow::{Context as _, bail};
use rustc_hash::FxHashMap;

use crate::{
    Context, GitReference, Package, Registry, SourceId, SourceKind,
    directory::Directory,
    download::From,
    source_id::{CRATES_IO_REGISTRY, path_of},
};

/// One `[source.<name>]` table, as blaze-manifest reads it: paths already resolved against the
/// directory holding the `.cargo` directory the file is in, as cargo resolves them.
#[derive(Clone, Debug, Default)]
pub struct SourceDef {
    pub replace_with: Option<String>,
    pub directory: Option<PathBuf>,
    pub registry: Option<String>,
    pub local_registry: Option<PathBuf>,
    pub git: Option<String>,
    pub branch: Option<String>,
    pub tag: Option<String>,
    pub rev: Option<String>,
    /// Where the table is defined, which errors name: a file, or an environment variable.
    pub definition: String,
}

/// What of cargo's configuration says where packages come from.
#[derive(Clone, Debug, Default)]
pub struct SourceConfig {
    /// The `[source]` tables.
    pub sources: Vec<(String, SourceDef)>,
    /// `registries.<name>.index` of each `[registries.<name>]`.
    pub registries: FxHashMap<String, String>,
    /// `registries.crates-io.protocol = "git"`: crates.io read from its git index.
    pub crates_io_git: bool,
}

#[derive(Clone)]
struct Replacement {
    id: SourceId,
    /// The source's name and where its `replace-with` is defined.
    with: Option<(String, String)>,
}

/// The sources cargo knows by name, and which of them replaces which.
pub struct SourceMap {
    by_name: FxHashMap<String, Replacement>,
    names: FxHashMap<SourceId, String>,
    registries: FxHashMap<String, String>,
    crates_io: SourceId,
}

impl SourceMap {
    pub fn new(config: &SourceConfig) -> anyhow::Result<SourceMap> {
        let crates_io =
            if config.crates_io_git { SourceId::crates_io_git() } else { SourceId::crates_io() };
        let mut map = SourceMap {
            by_name: FxHashMap::default(),
            names: FxHashMap::default(),
            registries: config.registries.clone(),
            crates_io: crates_io.clone(),
        };
        map.add(CRATES_IO_REGISTRY, Replacement { id: SourceId::crates_io_git(), with: None })?;
        map.add(CRATES_IO_REGISTRY, Replacement { id: crates_io, with: None })?;
        for (name, def) in &config.sources {
            map.add_def(name, def)?;
        }
        Ok(map)
    }

    fn add(&mut self, name: &str, replacement: Replacement) -> anyhow::Result<()> {
        if let Some(old) = self.names.insert(replacement.id.clone(), name.to_owned())
            && name != CRATES_IO_REGISTRY
        {
            bail!(
                "source `{name}` defines source {}, but that source is already defined by `{old}`\n\
                 note: Sources are not allowed to be defined multiple times.",
                replacement.id
            );
        }
        self.by_name.insert(name.to_owned(), replacement);
        Ok(())
    }

    fn add_def(&mut self, name: &str, def: &SourceDef) -> anyhow::Result<()> {
        let url = |it: &str, key: &str| -> anyhow::Result<url::Url> {
            url::Url::parse(it).with_context(|| {
                format!(
                    "configuration key `source.{name}.{key}` specified an invalid URL (in {})",
                    def.definition
                )
            })
        };
        let mut ids = Vec::new();
        if let Some(registry) = &def.registry {
            url(registry, "registry")?;
            ids.push(SourceId::for_registry(registry)?);
        }
        if let Some(path) = &def.local_registry {
            ids.push(SourceId::for_local_registry(path)?);
        }
        if let Some(path) = &def.directory {
            ids.push(SourceId::for_directory(path)?);
        }
        if let Some(git) = &def.git {
            let reference = match (&def.branch, &def.tag, &def.rev) {
                (Some(branch), _, _) => GitReference::Branch(branch.clone()),
                (None, Some(tag), _) => GitReference::Tag(tag.clone()),
                (None, None, Some(rev)) => GitReference::Rev(rev.clone()),
                (None, None, None) => GitReference::DefaultBranch,
            };
            ids.push(SourceId::for_git(&url(git, "git")?, reference)?);
        } else {
            for (key, value) in [("branch", &def.branch), ("tag", &def.tag), ("rev", &def.rev)] {
                if value.is_some() {
                    bail!(
                        "source definition `source.{name}` specifies `{key}`, but that requires a \
                         `git` key to be specified (in {})",
                        def.definition
                    );
                }
            }
        }
        if name == CRATES_IO_REGISTRY && ids.is_empty() {
            ids.push(self.crates_io.clone());
        }
        let id = match ids.len() {
            0 => bail!(
                "no source location specified for `source.{name}`, need `registry`, \
                 `local-registry`, `directory`, or `git` defined"
            ),
            1 => ids.pop().expect("one"),
            _ => bail!("more than one source location specified for `source.{name}`"),
        };
        let with = def.replace_with.clone().map(|it| (it, def.definition.clone()));
        self.add(name, Replacement { id, with })
    }

    /// The source the packages of `id` come from: `id` itself, or the end of its chain of
    /// `replace-with`. `precise` is whether the lock file pins the package (a git source's
    /// commit), which a vendored replacement of a git source needs.
    pub fn replacement(&self, id: &SourceId, precise: bool) -> anyhow::Result<SourceId> {
        let Some(original) = self.names.get(id) else { return Ok(id.clone()) };
        let mut name = original;
        let mut at = "";
        let replaced = loop {
            let Some(replacement) = self.by_name.get(name) else {
                if let Some(id) = self.registry(name) {
                    break id?;
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
                None if replacement.id == *id => return Ok(id.clone()),
                None => break replacement.id.clone(),
            }
            if name == original {
                bail!(
                    "detected a cycle of `replace-with` sources, the source `{name}` is \
                     eventually replaced with itself (configuration in `{at}`)"
                );
            }
        };
        if supports_checksums(id) && !supports_checksums(&replaced) {
            bail!(
                "cannot replace `{original}` with `{name}`, the source `{original}` supports \
                 checksums, but `{name}` does not\n\na lock file compatible with `{original}` \
                 cannot be generated in this situation"
            );
        }
        if requires_precise(id) && !precise {
            bail!(
                "the source {original} requires a lock file to be present first before it can be\n\
                 used against vendored source code\n\nremove the source replacement \
                 configuration, generate a lock file, and then\nrestore the source replacement \
                 configuration to continue the build"
            );
        }
        Ok(replaced)
    }

    /// `[registries.<name>]` as a source, as cargo's `SourceId::alt_registry`.
    fn registry(&self, name: &str) -> Option<anyhow::Result<SourceId>> {
        if name == CRATES_IO_REGISTRY {
            return Some(Ok(SourceId::crates_io_git()));
        }
        let index = self.registries.get(name)?;
        Some(SourceId::for_registry(index).with_context(|| {
            format!("invalid index URL for registry `{name}` defined in configuration")
        }))
    }
}

fn supports_checksums(id: &SourceId) -> bool {
    matches!(
        id.kind(),
        SourceKind::Registry
            | SourceKind::SparseRegistry
            | SourceKind::LocalRegistry
            | SourceKind::Directory
    )
}

fn requires_precise(id: &SourceId) -> bool {
    matches!(id.kind(), SourceKind::Git(_) | SourceKind::Directory)
}

/// A package of a lock file: its source as the lock file names it, the commit a git source is
/// pinned to, and the package.
#[derive(Clone, Debug)]
pub struct Locked {
    pub source: SourceId,
    pub precise: Option<String>,
    pub package: Package,
}

/// Every source a build reads packages from, opened once.
pub struct Sources {
    map: SourceMap,
    context: Arc<Context>,
    registries: Mutex<FxHashMap<SourceId, Arc<Registry>>>,
    directories: Mutex<FxHashMap<SourceId, Arc<Directory>>>,
}

impl Sources {
    pub fn new(config: &SourceConfig, context: Arc<Context>) -> anyhow::Result<Sources> {
        Ok(Sources {
            map: SourceMap::new(config)?,
            context,
            registries: Mutex::default(),
            directories: Mutex::default(),
        })
    }

    pub fn map(&self) -> &SourceMap {
        &self.map
    }

    /// The registry `id` is, remote or local.
    pub fn registry(&self, id: &SourceId) -> anyhow::Result<Arc<Registry>> {
        let mut open = self.registries.lock().expect("sources");
        if let Some(registry) = open.get(id) {
            return Ok(registry.clone());
        }
        let registry = Arc::new(match id.kind() {
            SourceKind::LocalRegistry => {
                Registry::local(id.clone(), &path_of(id)?, self.context.clone())
            }
            _ => Registry::remote(id.clone(), self.context.clone())?,
        });
        open.insert(id.clone(), registry.clone());
        Ok(registry)
    }

    pub fn directory(&self, id: &SourceId) -> anyhow::Result<Arc<Directory>> {
        let mut open = self.directories.lock().expect("sources");
        if let Some(directory) = open.get(id) {
            return Ok(directory.clone());
        }
        let directory = Arc::new(Directory::new(id.clone(), path_of(id)?));
        open.insert(id.clone(), directory.clone());
        Ok(directory)
    }

    /// Every package of `locked` where a build reads it from, in the order given, and where it
    /// came from: unpacked from its registry, replacement or not, into
    /// `$CARGO_HOME/registry/src`, or the directory a vendored copy is in once its files are
    /// checked.
    pub fn fetch(&self, locked: &[Locked]) -> anyhow::Result<Vec<(PathBuf, From)>> {
        let mut by_source: FxHashMap<SourceId, Vec<usize>> = FxHashMap::default();
        for (at, it) in locked.iter().enumerate() {
            let from = self.map.replacement(&it.source, it.precise.is_some())?;
            by_source.entry(from).or_default().push(at);
        }
        let mut roots = vec![(PathBuf::new(), From::Unpacked); locked.len()];
        for (from, at) in by_source {
            match from.kind() {
                SourceKind::Registry | SourceKind::SparseRegistry | SourceKind::LocalRegistry => {
                    let registry = self.registry(&from)?;
                    let packages: Vec<Package> =
                        at.iter().map(|&at| locked[at].package.clone()).collect();
                    for (&at, fetched) in at.iter().zip(registry.fetch(&packages)?) {
                        roots[at] = fetched;
                    }
                }
                SourceKind::Directory => {
                    let directory = self.directory(&from)?;
                    for &at in &at {
                        roots[at] = (directory.fetch(&locked[at].package)?, From::Vendored);
                    }
                }
                SourceKind::Git(_) => {
                    let _lock = crate::PackageCacheLock::acquire(&self.context.cargo_home)?;
                    let mut checkouts: FxHashMap<Option<&str>, crate::git::Checkout> =
                        FxHashMap::default();
                    for &at in &at {
                        let precise = locked[at].precise.as_deref();
                        let checkout = match checkouts.get(&precise) {
                            Some(it) => it,
                            None => {
                                let checkout = crate::git::fetch(&self.context, &from, precise)?;
                                checkouts.entry(precise).or_insert(checkout)
                            }
                        };
                        let package = &locked[at].package;
                        roots[at] = (checkout.package(&package.name, &package.version)?, From::Git);
                    }
                }
                SourceKind::Path => bail!("{from} is read where it is, not fetched"),
            }
        }
        Ok(roots)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The absolute path `unix` is on this machine: `/w` is not one on Windows, where cargo
    /// reads a source's directory from its config as a path under the config's own.
    fn abs(unix: &str) -> PathBuf {
        PathBuf::from(if cfg!(windows) {
            format!("C:{}", unix.replace('/', "\\"))
        } else {
            unix.to_owned()
        })
    }

    fn def(f: impl FnOnce(&mut SourceDef)) -> SourceDef {
        let mut def =
            SourceDef { definition: "/w/.cargo/config.toml".to_owned(), ..SourceDef::default() };
        f(&mut def);
        def
    }

    fn map(sources: Vec<(&str, SourceDef)>) -> anyhow::Result<SourceMap> {
        let mine = ("mine".to_owned(), "sparse+https://mine.example.com/index/".to_owned());
        SourceMap::new(&SourceConfig {
            sources: sources.into_iter().map(|(name, def)| (name.to_owned(), def)).collect(),
            registries: [mine].into_iter().collect(),
            crates_io_git: false,
        })
    }

    fn error<T>(result: anyhow::Result<T>) -> String {
        match result {
            Ok(_) => panic!("no error"),
            Err(err) => format!("{err:#}"),
        }
    }

    #[test]
    fn crates_io_is_read_sparse_unless_told() {
        let map = map(vec![]).unwrap();
        assert_eq!(
            map.replacement(&SourceId::crates_io_git(), false).unwrap(),
            SourceId::crates_io()
        );
        assert_eq!(map.replacement(&SourceId::crates_io(), false).unwrap(), SourceId::crates_io());
        let other = SourceId::for_registry("sparse+https://other.example.com/").unwrap();
        assert_eq!(map.replacement(&other, false).unwrap(), other);

        let git = SourceMap::new(&SourceConfig { crates_io_git: true, ..SourceConfig::default() })
            .unwrap();
        assert_eq!(
            git.replacement(&SourceId::crates_io_git(), false).unwrap(),
            SourceId::crates_io_git()
        );
    }

    #[test]
    fn replaces_as_cargo_does() {
        let vendor = SourceId::for_directory(&abs("/w/vendor")).unwrap();
        let map = map(vec![
            ("crates-io", def(|it| it.replace_with = Some("mirror".to_owned()))),
            (
                "mirror",
                def(|it| {
                    it.registry = Some("sparse+https://mirror.example.com/".to_owned());
                    it.replace_with = Some("vendored".to_owned());
                }),
            ),
            ("vendored", def(|it| it.directory = Some(abs("/w/vendor")))),
        ])
        .unwrap();
        assert_eq!(map.replacement(&SourceId::crates_io_git(), false).unwrap(), vendor);
        assert_eq!(map.replacement(&SourceId::crates_io(), false).unwrap(), vendor);

        let mine = SourceId::for_registry("sparse+https://mine.example.com/index/").unwrap();
        let map =
            self::map(vec![("crates-io", def(|it| it.replace_with = Some("mine".to_owned())))])
                .unwrap();
        assert_eq!(map.replacement(&SourceId::crates_io_git(), false).unwrap(), mine);

        let git = SourceId::for_git(
            &"https://github.com/o/r".parse().unwrap(),
            GitReference::Branch("main".to_owned()),
        )
        .unwrap();
        let map = self::map(vec![
            (
                "o",
                def(|it| {
                    it.git = Some("https://github.com/o/r".to_owned());
                    it.branch = Some("main".to_owned());
                    it.replace_with = Some("vendored".to_owned());
                }),
            ),
            ("vendored", def(|it| it.directory = Some(abs("/w/vendor")))),
        ])
        .unwrap();
        assert_eq!(map.replacement(&git, true).unwrap(), vendor);
        assert!(error(map.replacement(&git, false)).starts_with(
            "the source o requires a lock file to be present first before it can be\nused against vendored"
        ));
    }

    #[test]
    fn refuses_what_cargo_refuses() {
        let to = |name: &str| def(|it| it.replace_with = Some(name.to_owned()));
        let registry = |url: &str, name: &str| {
            def(|it| {
                it.registry = Some(url.to_owned());
                it.replace_with = Some(name.to_owned());
            })
        };
        let map = map(vec![
            ("crates-io", to("a")),
            ("a", registry("sparse+https://a.example.com/", "b")),
            ("b", registry("sparse+https://b.example.com/", "crates-io")),
        ])
        .unwrap();
        assert_eq!(
            error(map.replacement(&SourceId::crates_io(), false)),
            "detected a cycle of `replace-with` sources, the source `crates-io` is eventually \
             replaced with itself (configuration in `/w/.cargo/config.toml`)"
        );
        let map = self::map(vec![("crates-io", to("nowhere"))]).unwrap();
        assert_eq!(
            error(map.replacement(&SourceId::crates_io(), false)),
            "could not find a configured source with the name `nowhere` when attempting to lookup \
             `crates-io` (configuration in `/w/.cargo/config.toml`)"
        );
        let map = self::map(vec![
            ("crates-io", to("g")),
            ("g", def(|it| it.git = Some("https://example.com/g".to_owned()))),
        ])
        .unwrap();
        assert!(error(map.replacement(&SourceId::crates_io(), false)).starts_with(
            "cannot replace `crates-io` with `g`, the source `crates-io` supports checksums, but `g` does not"
        ));
        assert_eq!(
            error(self::map(vec![("x", def(|_| {}))])),
            "no source location specified for `source.x`, need `registry`, `local-registry`, \
             `directory`, or `git` defined"
        );
        assert_eq!(
            error(self::map(vec![(
                "x",
                def(|it| {
                    it.directory = Some(abs("/a"));
                    it.local_registry = Some(abs("/b"));
                })
            )])),
            "more than one source location specified for `source.x`"
        );
        assert_eq!(
            error(self::map(vec![(
                "x",
                def(|it| {
                    it.directory = Some(abs("/a"));
                    it.tag = Some("v1".to_owned());
                })
            )])),
            "source definition `source.x` specifies `tag`, but that requires a `git` key to be \
             specified (in /w/.cargo/config.toml)"
        );
        assert!(
            error(self::map(vec![
                ("x", def(|it| it.directory = Some(abs("/a")))),
                ("y", def(|it| it.directory = Some(abs("/a")))),
            ]))
            .starts_with(&format!(
                "source `y` defines source dir {}, but that source is already defined by `x`",
                abs("/a").display()
            ))
        );
    }
}
