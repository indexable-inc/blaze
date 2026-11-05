//! `blaze fmt`: blaze's formatter, run in this process; no rustfmt or cargo-fmt is started.
//!
//! - Given `.rs` files before any `--` it is rustfmt over them ([`crate::cli::main`]): `blaze
//!   fmt [--check] src/main.rs`. Each file is formatted with the edition of the target, or else
//!   the package, of the workspace it belongs to, as cargo-fmt gives each target's edition,
//!   unless `--edition` is passed. A file in no package gets rustfmt's own rule: rustfmt.toml's
//!   `edition`, else 2015.
//! - Otherwise it is cargo-fmt over the workspace's targets ([`crate::cargo_fmt::main`]): the
//!   current package, or the whole workspace at its root, `-p NAME`, `--all` (with local path
//!   dependencies), `--check` (exit 1 with the diff), `--manifest-path`, `-v`/`-q`, and
//!   rustfmt's own options and files after `--`.
//!
//! Either way the nearest rustfmt.toml or .rustfmt.toml is read for each file; an option in it
//! that blaze fmt does not run is an error naming the option and the file
//! (`Config::from_toml_for_style_edition`), never ignored.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use blaze_manifest::features::Edition;
use blaze_manifest::metadata::root_manifest;
use blaze_manifest::{Config, Context, Workspace};

/// Runs `blaze fmt` with `args`, the words after `fmt`, and returns its exit code.
pub fn main(args: Vec<String>) -> i32 {
    if !names_files(&args) {
        return crate::cargo_fmt::main(args);
    }
    if picks_edition(&args) {
        return crate::cli::main(args);
    }
    let groups = match by_edition(&args) {
        Ok(groups) => groups,
        Err(error) => {
            eprintln!("error: blaze fmt: {error:#}");
            return 1;
        }
    };
    // The words that are not files, then each edition's files: one rustfmt run per edition, as
    // cargo-fmt runs one per edition of the targets. The first failing run's code is the exit
    // code, and every run happens.
    let rest: Vec<&String> = args // alloc: the command line without its files
        .iter()
        .enumerate()
        .filter(|(at, _)| !is_file_at(&args, *at))
        .map(|(_, it)| it)
        .collect();
    let mut code = 0;
    for (edition, files) in groups {
        // alloc: one command line per edition
        let mut words: Vec<String> = rest.iter().map(|it| (*it).clone()).collect();
        words.extend(files);
        if let Some(edition) = edition {
            words.extend(["--edition".to_owned(), edition.to_string()]);
        }
        let status = crate::cli::main(words);
        if code == 0 {
            code = status;
        }
    }
    code
}

/// Whether `args` name source files before `--`. cargo-fmt takes no positional argument, and
/// none of its values (a package, a manifest, a message format) ends in `.rs`.
fn names_files(args: &[String]) -> bool {
    files_of(args).next().is_some()
}

/// The `.rs` files `args` name before `--`.
fn files_of(args: &[String]) -> impl Iterator<Item = &str> {
    (0..args.len())
        .filter(|at| is_file_at(args, *at))
        .map(|at| args[at].as_str())
}

/// Whether the word at `at` is a file: a word ending in `.rs`, not an option, before any `--`.
fn is_file_at(args: &[String], at: usize) -> bool {
    let word = &args[at];
    !args[..at].iter().any(|it| it == "--") && !word.starts_with('-') && word.ends_with(".rs")
}

/// Whether the command line says the edition itself, or prints instead of formatting: then
/// rustfmt reads it as it is.
fn picks_edition(args: &[String]) -> bool {
    args.iter().any(|it| {
        it == "--edition"
            || it.starts_with("--edition=")
            || it == "--print-config"
            || it.starts_with("--print-config=")
    })
}

/// The files `args` name, grouped by the edition of the target or package each belongs to
/// (`None`: in no package), in a fixed order.
fn by_edition(args: &[String]) -> anyhow::Result<BTreeMap<Option<Edition>, Vec<String>>> {
    let mut packages = Packages::default();
    let mut groups: BTreeMap<Option<Edition>, Vec<String>> = BTreeMap::new();
    for file in files_of(args) {
        let edition = packages.edition_of(Path::new(file))?;
        groups.entry(edition).or_default().push(file.to_owned()); // alloc: the file's name
    }
    Ok(groups)
}

/// The workspaces read so far, by their root, each with its members' editions.
#[derive(Default)]
struct Packages {
    /// The root of the workspace each manifest found so far is in.
    roots: BTreeMap<PathBuf, PathBuf>,
    /// Each workspace's members: their directories, packages' editions, targets' sources.
    read: BTreeMap<PathBuf, Vec<Member>>,
}

struct Member {
    dir: PathBuf,
    edition: Edition,
    targets: Vec<(PathBuf, Edition)>,
}

impl Packages {
    /// The edition `file` is formatted with: its target's when it is a target's source, else its
    /// package's (the member whose directory holds it most closely); `None` when it is in no
    /// package, or does not exist (rustfmt then says so, as for any file it can't open).
    fn edition_of(&mut self, file: &Path) -> anyhow::Result<Option<Edition>> {
        let Ok(file) = file.canonicalize() else {
            return Ok(None);
        };
        let Some(manifest) = file
            .ancestors()
            .skip(1)
            .map(|dir| dir.join("Cargo.toml"))
            .find(|it| it.is_file())
        else {
            return Ok(None);
        };
        let members = self.members(&manifest)?;
        let owner = members
            .iter()
            .filter(|m| file.starts_with(&m.dir))
            .max_by_key(|m| m.dir.components().count());
        Ok(owner.map(|m| {
            m.targets
                .iter()
                .find(|(src, _)| *src == file)
                .map_or(m.edition, |(_, edition)| *edition)
        }))
    }

    /// The members of the workspace `manifest` is in, each workspace read once.
    fn members(&mut self, manifest: &Path) -> anyhow::Result<&[Member]> {
        let root = match self.roots.get(manifest) {
            Some(root) => root.clone(),
            None => {
                let (root, members) = read_members(manifest).map_err(|error| {
                    anyhow::anyhow!(
                        "reading the workspace of {} for the edition of the files in it: {error:#}",
                        manifest.display()
                    )
                })?;
                self.roots.insert(manifest.to_owned(), root.clone());
                self.read.entry(root.clone()).or_insert(members);
                root
            }
        };
        Ok(self.read[&root].as_slice())
    }
}

/// The root of the workspace `manifest` is in, and its members, as blaze-manifest reads them
/// (what cargo-fmt reads from `cargo metadata --no-deps`).
fn read_members(manifest: &Path) -> anyhow::Result<(PathBuf, Vec<Member>)> {
    let ctx = Context::new(Config::from_process()?);
    let ws = Workspace::new(&root_manifest(Some(manifest), &ctx)?, &ctx)?;
    let canonical = |path: &Path| path.canonicalize().unwrap_or_else(|_| path.to_owned());
    let members = ws
        .members()
        .map(|(path, m)| Member {
            dir: canonical(path.parent().unwrap_or(path)),
            edition: m.edition,
            targets: m
                .targets
                .iter()
                .filter_map(|t| Some((canonical(t.src_path.as_ref()?), t.edition)))
                .collect(), // alloc: the member's targets, kept for the run
        })
        .collect(); // alloc: the workspace's members, kept for the run
    Ok((ws.root().to_owned(), members))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(text: &str) -> Vec<String> {
        text.split_whitespace().map(str::to_owned).collect()
    }

    #[test]
    fn files_are_rustfmt_and_the_rest_cargo_fmt() {
        assert!(names_files(&words("src/main.rs")));
        assert!(names_files(&words("--check --edition 2021 a.rs b.rs")));
        assert!(!names_files(&words("--check -p ripgrep")));
        assert!(!names_files(&words("--all -- --config max_width=60 x.rs")));
        assert!(!names_files(&words("--manifest-path a/Cargo.toml")));
        assert!(!names_files(&[]));
        let line = words("--check a.rs --config x=1 b.rs -- c.rs");
        let named: Vec<&str> = files_of(&line).collect();
        assert_eq!(named, ["a.rs", "b.rs"]);
    }

    #[test]
    fn an_edition_on_the_command_line_wins() {
        assert!(picks_edition(&words("--edition 2018 a.rs")));
        assert!(picks_edition(&words("--edition=2018 a.rs")));
        assert!(!picks_edition(&words("--check a.rs")));
    }
}
