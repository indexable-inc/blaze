//! `[source]` replacement as cargo 1.99 does it (`SourceConfigMap` in `src/sources/config.rs`):
//! which source a source's packages come from, and every package of a lock file fetched from
//! there into the directory cargo would have it in.

use std::{path::PathBuf, sync::Arc};

use anyhow::{Context as _, bail};
use blaze_hash::{HashMap, SharedMap};

use blaze_source::{CRATES_IO_REGISTRY, GitReference, SourceId, SourceKind};

use crate::{Context, Package, Registry, directory::Directory, download::From, path_of};

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
    pub registries: HashMap<String, String>,
    /// `registries.crates-io.protocol = "git"`: crates.io read from its git index.
    pub crates_io_git: bool,
}

pub use blaze_units::sources::SourceMap;

/// The map `config` makes, as cargo's `SourceConfigMap::new`: each `[source]` table, then each
/// `[registries]` name a `replace-with` gives, read as cargo's `SourceId::alt_registry` reads
/// it (one it refuses is left out, as cargo's `load` passes over it).
pub fn source_map(config: &SourceConfig) -> anyhow::Result<SourceMap> {
    let crates_io =
        if config.crates_io_git { SourceId::crates_io() } else { SourceId::crates_io_sparse() };
    let mut map = SourceMap::new(crates_io);
    for (name, def) in &config.sources {
        add_def(&mut map, name, def)?;
    }
    for name in map.unknown_replacements() {
        let Some(index) = config.registries.get(&name) else { continue };
        let Ok(url) = url::Url::parse(index) else { continue };
        if let Ok(id) = SourceId::for_alt_registry(&url, &name) {
            map.add_registry(name, id);
        }
    }
    Ok(map)
}

/// One `[source.<name>]` table, as cargo's `SourceConfigMap::add_config`.
fn add_def(map: &mut SourceMap, name: &str, def: &SourceDef) -> anyhow::Result<()> {
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
        ids.push(SourceId::for_source_replacement_registry(&url(registry, "registry")?, name)?);
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
        ids.push(map.crates_io());
    }
    let id = match ids.len() {
        0 => bail!(
            "no source location specified for `source.{name}`, need `registry`, \
             `local-registry`, `directory`, or `git` defined"
        ),
        1 => ids.pop().expect("one"),
        _ => bail!("more than one source location specified for `source.{name}`"),
    };
    map.add(name, id, def.replace_with.clone().map(|it| (it, def.definition.clone())))
}

/// A package of a lock file: its source as the lock file names it, pinned (a git source to its
/// commit), and the package.
#[derive(Clone, Debug)]
pub struct Locked {
    pub source: SourceId,
    pub package: Package,
}

/// Every source a build reads packages from, opened once.
pub struct Sources {
    map: SourceMap,
    context: Arc<Context>,
    registries: SharedMap<SourceId, Arc<Registry>>,
    directories: SharedMap<SourceId, Arc<Directory>>,
}

impl Sources {
    pub fn new(config: &SourceConfig, context: Arc<Context>) -> anyhow::Result<Sources> {
        Ok(Sources {
            map: source_map(config)?,
            context,
            registries: SharedMap::default(),
            directories: SharedMap::default(),
        })
    }

    pub fn map(&self) -> &SourceMap {
        &self.map
    }

    /// The registry `id` is, remote or local. Opening one reads nothing, so of two threads that
    /// open one at once, the one that loses drops its copy and takes the winner's.
    pub fn registry(&self, id: SourceId) -> anyhow::Result<Arc<Registry>> {
        if let Some(registry) = self.registries.pin().get(&id) {
            return Ok(registry.clone());
        }
        let registry = Arc::new(match id.kind() {
            SourceKind::LocalRegistry => Registry::local(id, &path_of(id)?, self.context.clone()),
            _ => Registry::remote(id, self.context.clone())?,
        });
        Ok(self.registries.pin().get_or_insert(id, registry).clone())
    }

    pub fn directory(&self, id: SourceId) -> anyhow::Result<Arc<Directory>> {
        if let Some(directory) = self.directories.pin().get(&id) {
            return Ok(directory.clone());
        }
        let directory = Arc::new(Directory::new(id, path_of(id)?));
        Ok(self.directories.pin().get_or_insert(id, directory).clone())
    }

    /// Every package of `locked` where a build reads it from, in the order given, and where it
    /// came from: unpacked from its registry, replacement or not, into
    /// `$CARGO_HOME/registry/src`, or the directory a vendored copy is in once its files are
    /// checked.
    pub fn fetch(&self, locked: &[Locked]) -> anyhow::Result<Vec<(PathBuf, From)>> {
        let mut by_source: HashMap<SourceId, Vec<usize>> = HashMap::default();
        for (at, it) in locked.iter().enumerate() {
            let from = self.map.replacement(it.source)?;
            by_source.entry(from).or_default().push(at);
        }
        let mut roots = vec![(PathBuf::new(), From::Unpacked); locked.len()];
        for (from, at) in by_source {
            match from.kind() {
                SourceKind::Registry | SourceKind::SparseRegistry | SourceKind::LocalRegistry => {
                    let registry = self.registry(from)?;
                    let packages: Vec<Package> =
                        at.iter().map(|&at| locked[at].package.clone()).collect();
                    for (&at, fetched) in at.iter().zip(registry.fetch(&packages)?) {
                        roots[at] = fetched;
                    }
                }
                SourceKind::Directory => {
                    let directory = self.directory(from)?;
                    for &at in &at {
                        roots[at] = (directory.fetch(&locked[at].package)?, From::Vendored);
                    }
                }
                SourceKind::Git(_) => {
                    let _lock = crate::PackageCacheLock::acquire(&self.context.cargo_home)?;
                    let mut checkouts: HashMap<Option<&str>, crate::git::Checkout> =
                        HashMap::default();
                    for &at in &at {
                        let precise = locked[at].source.precise_git_fragment();
                        let checkout = match checkouts.get(&precise) {
                            Some(it) => it,
                            None => {
                                let checkout = crate::git::fetch(&self.context, from, precise)?;
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
        source_map(&SourceConfig {
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
        assert_eq!(map.replacement(SourceId::crates_io()).unwrap(), SourceId::crates_io_sparse());
        assert_eq!(
            map.replacement(SourceId::crates_io_sparse()).unwrap(),
            SourceId::crates_io_sparse()
        );
        let other = crate::registry_source("sparse+https://other.example.com/").unwrap();
        assert_eq!(map.replacement(other).unwrap(), other);

        let git = source_map(&SourceConfig { crates_io_git: true, ..SourceConfig::default() })
            .unwrap();
        assert_eq!(git.replacement(SourceId::crates_io()).unwrap(), SourceId::crates_io());
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
        assert_eq!(map.replacement(SourceId::crates_io()).unwrap(), vendor);
        assert_eq!(map.replacement(SourceId::crates_io_sparse()).unwrap(), vendor);

        let mine = crate::registry_source("sparse+https://mine.example.com/index/").unwrap();
        let map =
            self::map(vec![("crates-io", def(|it| it.replace_with = Some("mine".to_owned())))])
                .unwrap();
        assert_eq!(map.replacement(SourceId::crates_io()).unwrap(), mine);

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
        assert_eq!(
            map.replacement(git.with_git_precise(Some("0123abcd".to_owned()))).unwrap(),
            vendor
        );
        assert!(error(map.replacement(git)).starts_with(
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
            error(map.replacement(SourceId::crates_io_sparse())),
            "detected a cycle of `replace-with` sources, the source `crates-io` is eventually \
             replaced with itself (configuration in `/w/.cargo/config.toml`)"
        );
        let map = self::map(vec![("crates-io", to("nowhere"))]).unwrap();
        assert_eq!(
            error(map.replacement(SourceId::crates_io_sparse())),
            "could not find a configured source with the name `nowhere` when attempting to lookup \
             `crates-io` (configuration in `/w/.cargo/config.toml`)"
        );
        let map = self::map(vec![
            ("crates-io", to("g")),
            ("g", def(|it| it.git = Some("https://example.com/g".to_owned()))),
        ])
        .unwrap();
        assert!(error(map.replacement(SourceId::crates_io_sparse())).starts_with(
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
