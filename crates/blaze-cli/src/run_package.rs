//! Which workspace a command is in, and what `blaze run` and `blaze test` select in it (run.md,
//! "Run and test read the engine's answer (D147)", piece R3): the workspace's root found by
//! walking up from the folder the command was given, then a member's package and one of its
//! binaries for `run`, the packages (and a binary's tests) for `test`.
//!
//! **The root** ([`locate`]), as cargo finds it (the Cargo book, "The `[workspace]` section":
//! "the root is found by the `workspace` key or by walking parents"; cargo's `find_root`):
//! the package is the nearest folder at or above the one given (`blaze test crates/a`, or
//! `crates/a/src`) that holds a `Cargo.toml`; its root is its own manifest when it has
//! `[workspace]`, else the one `package.workspace` names, else the nearest folder above with
//! `[workspace]` whose `exclude` does not leave it out, else the package alone. The walk up to
//! the start folder is the workspace's one manifest reader's (blaze-manifest `Layout`, started
//! at the package); above the start folder it is the read-only ancestors' (bstd
//! `Fs::ancestors`), which can find a root there but give no folder to check it as (a checkout
//! is a `Dir` beneath the start), so such a root is a refusal naming it and how to start
//! there ([`PickError::RootAboveStart`]), never the package read alone against a root that
//! says otherwise.
//!
//! **The selection** (useful, not parity, rule 5; the words a cargo user already types):
//!
//! - The package: `-p <name>` (`--package`) names a member by its package name; `run` takes
//!   one, `test` any number. Else, in a member's folder ([`Select::member`]), that member
//!   (cargo's current package; `default-members` is the root's alone). Else, for `run`, with
//!   `--bin <name>`, the one member that has a binary of that name. Else the root's own
//!   package when the root is one. Else, under a virtual root, the one package with a binary
//!   among the root's `default-members` when it is written, or among all members when it is not
//!   (cargo's `cargo run` in a virtual workspace with one binary). Else a refusal that lists
//!   every package with a binary: "say which: `blaze run -p <name>`". For `test`, the
//!   defaults are the root's `default-members`, else the root's package, else every member.
//! - The binary: `--bin <name>` names one of the package's. Else its one binary. Else a
//!   refusal that lists them: "say which: `--bin <name>`". For `test`, `--bin <name>` keeps
//!   the selected packages that have that binary (its tests alone run: run_names.rs filters the
//!   units by it), a refusal listing the binaries there are when none has it.
//!
//! Nothing is guessed past that: a name that matches nothing, or matches twice, is a refusal
//! naming what was asked and what there is (rule 3), never another package run instead.
//!
//! The choice is read from the manifests by the workspace's one manifest reader (blaze-manifest
//! `Layout`) laid from the root (the reader the engine's unit graph is made from, so the names
//! are the graph's units' package names, which run_names.rs selects the units by), plus the
//! root's `workspace.default-members`, which that reader leaves out. The program's crate root
//! names the program in its start list (engine_start.rs `RUN_SHAPE`), so two choices never
//! share one (rule 21).

use blaze_drive::Name;
use blaze_manifest::layout::{Entry, Layout, Member, Need, Pass, Target, Units};
use blaze_own::Vec;
use bstd::Cx;
use bstd::fs::{AncestorError, AncestorKind, Dir, Fs, ReadWrite};
use bstd::mem::{Bytes, Frame};
use bstd::path::Path;

use crate::CliError;
use crate::checkout::Checkout;

/// The passes the layout may take (units.rs `MOST_PASSES`; rule 18).
const MOST_PASSES: usize = 64;

/// The manifest's file name in a package's folder.
const MANIFEST: &str = "Cargo.toml";

/// What the command line asks of the choice: `-p <name>` and `--bin <name>`, each optional,
/// and the member's folder the command was given.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Select<'a> {
    /// The last `-p`: the package `run` runs.
    pub package: Option<&'a str>,
    pub bin: Option<&'a str>,
    /// Every `-p`, in the order given: `test` selects them all; `run` refuses more than one
    /// ([`PickError::SeveralPackages`]).
    pub test_packages: &'a [&'a str],
    /// The folder of the member the command was started in or given, relative to the root
    /// ([`Located::member`]); `""` at the root. With no `-p`, the selected package.
    pub member: &'a str,
}

impl Select<'_> {
    /// Neither flag, at the root: the rule's defaults.
    pub const NONE: Select<'static> = Select { package: None, bin: None, test_packages: &[], member: "" };
}

/// Where the command's workspace is ([`locate`]).
pub(crate) struct Located<'f> {
    /// The workspace's root folder: the engine's checkout.
    pub(crate) root: Dir<ReadWrite>,
    /// The root's path beneath the start folder (`""` for the start folder itself), what a
    /// shown path is joined to (engine_check.rs `Place::checkout`).
    pub(crate) at: &'f str,
    /// The package's folder the command was given, relative to the root (`""` at the root):
    /// [`Select::member`].
    pub(crate) member: &'f str,
}

/// The program chosen: its package, binary target name, the package's manifest, the binary's crate
/// root (each checkout-relative) and the package's edition as a manifest writes it (`2024`,
/// `blaze-2026.11`), what the runner's front end is given.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Program<'f> {
    pub package: &'f str,
    pub bin: &'f str,
    pub manifest: &'f str,
    pub root: &'f str,
    pub edition: &'f str,
}

impl<'f> Program<'f> {
    /// The units of the program's package: where `run`'s closure starts (units.rs names each
    /// member's units by its manifest's path).
    pub fn units(&self) -> Name {
        Name::manifest(self.manifest.as_bytes())
    }

    /// The folder of the binary's crate root, with its slash (`src/`, `bin/tempo/src/`): where
    /// the program's own files are (program.rs `sources`).
    pub fn sources(&self) -> &'f str {
        match self.root.rfind('/') {
            Some(at) => &self.root[..=at],
            None => "",
        }
    }
}

type Owned = std::boxed::Box<str, std::alloc::Global>;
type Owneds = std::vec::Vec<Owned, std::alloc::Global>;

/// Why no program was chosen: each names what was asked and what the workspace has.
#[derive(Debug, PartialEq, Eq)]
pub enum PickError {
    /// `-p <asked>` names no member: the members' packages, in the layout's order.
    NoSuchPackage { asked: Owned, packages: Owneds },
    /// The package has no binary target (a library alone).
    NoBinary { package: Owned },
    /// `--bin <asked>` names no binary of `package` (or of any member, `package` `None`): the
    /// binaries there are, each `package/bin`.
    NoSuchBin { asked: Owned, package: Option<Owned>, bins: Owneds },
    /// `--bin <asked>` with no `-p` names a binary of more than one member.
    BinInSeveral { asked: Owned, packages: Owneds },
    /// The package has several binaries and no `--bin`: each `name (root)`.
    WhichBin { package: Owned, bins: Owneds },
    /// A virtual root, no flag and no one package to run: the packages that have a binary.
    WhichPackage { packages: Owneds },
    /// The root's `default-members` names a path no member is at.
    DefaultNotMember { path: Owned },
    /// The root's `default-members` is not an array of strings, at its line and column.
    DefaultMembers { line: usize, column: usize },
    /// The root manifest could not be read as TOML (its `default-members`, its `[workspace]`).
    RootToml(blaze_toml::Error),
    /// No `Cargo.toml` in the folder given (`""`: the start folder) nor in any folder above it.
    NoManifest { start: Owned },
    /// The nearest `Cargo.toml` is above the start folder (`../Cargo.toml`): blaze reads a
    /// checkout only beneath the folder it starts in.
    ManifestAboveStart { manifest: Owned },
    /// The package's workspace root is above the start folder: the root's manifest
    /// (`../Cargo.toml`) and the package's folder from the start (`.` for the start itself).
    RootAboveStart { manifest: Owned, package: Owned },
    /// A manifest above the start folder could not be read as TOML.
    AboveToml { manifest: Owned, error: blaze_toml::Error },
    /// The folders above the start folder could not be read.
    Above(AncestorError),
    /// The root `package.workspace` names is not above the package's folder, so the package is
    /// not beneath the checkout that root is.
    OutsideRoot { package: Owned, root: Owned },
    /// The member's folder the command was given is no member of the root's layout: its
    /// folder, and the members' packages.
    NotAMember { dir: Owned, packages: Owneds },
    /// `blaze run` with more than one `-p`: it runs one program.
    SeveralPackages { asked: Owneds },
    /// `blaze test --bin <asked>`: no selected package has a binary `asked`; the binaries of
    /// the selected packages, each `package/bin`.
    TestBin { asked: Owned, bins: Owneds },
}

/// Each item, back-quoted, comma-separated.
fn listed(f: &mut core::fmt::Formatter<'_>, items: &[Owned]) -> core::fmt::Result {
    for (at, item) in items.iter().enumerate() {
        let comma = if at == 0 { "" } else { ", " };
        write!(f, "{comma}`{item}`")?;
    }
    Ok(())
}

impl core::fmt::Display for PickError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let rule = "run_package.rs";
        match self {
            PickError::NoSuchPackage { asked, packages } => {
                write!(f, "`-p {asked}`: the workspace has no package `{asked}`; its packages: ")?;
                listed(f, packages)?;
                write!(f, " ({rule})")
            }
            PickError::NoBinary { package } => write!(
                f,
                "`blaze run`: the package `{package}` has no binary target (no `src/main.rs` and no `[[bin]]`): it is a library, so there is no program to run; `blaze check` and `blaze test` take it ({rule})"
            ),
            PickError::NoSuchBin { asked, package, bins } => {
                match package {
                    Some(package) => write!(f, "`blaze run --bin {asked}`: the package `{package}` has no binary `{asked}`; its binaries: ")?,
                    None => write!(f, "`blaze run --bin {asked}`: no package of the workspace has a binary `{asked}`; the binaries: ")?,
                }
                listed(f, bins)?;
                write!(f, " ({rule})")
            }
            PickError::BinInSeveral { asked, packages } => {
                write!(f, "`blaze run --bin {asked}`: more than one package has a binary `{asked}`, say which: `blaze run -p <name> --bin {asked}`; the packages: ")?;
                listed(f, packages)?;
                write!(f, " ({rule})")
            }
            PickError::WhichBin { package, bins } => {
                write!(f, "`blaze run`: the package `{package}` has more than one binary, say which: `blaze run --bin <name>`; its binaries: ")?;
                listed(f, bins)?;
                write!(f, " ({rule})")
            }
            PickError::WhichPackage { packages } if packages.is_empty() => {
                write!(f, "`blaze run`: the workspace's root has no package of its own and no member has a binary target, so there is no program to run ({rule})")
            }
            PickError::WhichPackage { packages } => {
                write!(f, "`blaze run`: the workspace's root has no package of its own and no one package is the program, say which: `blaze run -p <name>`; the packages with a binary: ")?;
                listed(f, packages)?;
                write!(f, " ({rule})")
            }
            PickError::DefaultNotMember { path } => {
                write!(f, "the root's `default-members` names `{path}`, which is no member of the workspace ({rule})")
            }
            PickError::DefaultMembers { line, column } => {
                write!(f, "the root's `default-members` at line {line}, column {column} is not an array of paths ({rule})")
            }
            PickError::RootToml(error) => write!(f, "the workspace root's Cargo.toml: {error} ({rule})"),
            PickError::NoManifest { start } => {
                let start: &str = if start.is_empty() { "the folder blaze was started in" } else { &**start };
                write!(f, "no `Cargo.toml` in `{start}` or in any folder above it: there is no package to read ({rule} `locate`)")
            }
            PickError::ManifestAboveStart { manifest } => write!(
                f,
                "the nearest manifest is `{manifest}`, above the folder blaze was started in; blaze reads a checkout only beneath the folder it starts in: start blaze in that manifest's folder ({rule} `locate`)"
            ),
            PickError::RootAboveStart { manifest, package } => write!(
                f,
                "the package in `{package}` is in the workspace whose root is `{manifest}` (the nearest `[workspace]` above it that does not exclude it, cargo's rule), above the folder blaze was started in; blaze reads a checkout only beneath the folder it starts in: start blaze in the root's folder and give `-p <name>` or this member's folder, or make this package its own root with an empty `[workspace]` in its manifest ({rule} `locate`)"
            ),
            PickError::AboveToml { manifest, error } => write!(f, "`{manifest}`, read while looking for the workspace's root above the start folder: {error} ({rule} `locate`)"),
            PickError::Above(error) => write!(f, "looking for the workspace's root above the start folder: {error} ({rule} `locate`)"),
            PickError::OutsideRoot { package, root } => write!(
                f,
                "the package in `{package}` names the workspace root `{root}`, which is not a folder above it, so the package is not beneath the checkout that root is ({rule} `locate`)"
            ),
            PickError::NotAMember { dir, packages } => {
                write!(f, "`{dir}` is no member's folder of the workspace; its packages: ")?;
                listed(f, packages)?;
                write!(f, " ({rule})")
            }
            PickError::SeveralPackages { asked } => {
                write!(f, "`blaze run` runs one program, and was given more than one `-p`: ")?;
                listed(f, asked)?;
                write!(f, " ({rule})")
            }
            PickError::TestBin { asked, bins } if bins.is_empty() => {
                write!(f, "`blaze test --bin {asked}`: no selected package has a binary ({rule})")
            }
            PickError::TestBin { asked, bins } => {
                write!(f, "`blaze test --bin {asked}`: no selected package has a binary `{asked}`; their binaries: ")?;
                listed(f, bins)?;
                write!(f, " ({rule})")
            }
        }
    }
}

fn owned(text: &str) -> Owned {
    Owned::from(text)
}

fn named<'u, 'a>(members: &'u [Member<'a>], asked: &str) -> Result<&'u Member<'a>, PickError> {
    members.iter().find(|member| member.name.as_str() == asked).ok_or_else(|| PickError::NoSuchPackage {
        asked: owned(asked),
        packages: members.iter().map(|member| owned(member.name.as_str())).collect(),
    })
}

/// The member whose folder is `dir` (root-relative); `None` for `""`, the root, whose defaults
/// are the root's rules. A folder no member is at is refused with the members' packages.
fn at_dir<'u, 'a>(members: &'u [Member<'a>], dir: &str) -> Result<Option<&'u Member<'a>>, PickError> {
    if dir.is_empty() {
        return Ok(None);
    }
    match members.iter().find(|member| member.dir.as_str() == dir) {
        Some(member) => Ok(Some(member)),
        None => Err(PickError::NotAMember { dir: owned(dir), packages: members.iter().map(|member| owned(member.name.as_str())).collect() }),
    }
}

/// The package and binary `select` chooses from the workspace's layout, the root's manifest
/// text beside it (for `default-members`).
fn choose<'u, 'a>(units: &'u Units<'a>, root_text: &str, select: Select<'_>, frame: &Frame) -> Result<(&'u Member<'a>, &'u Target<'a>), PickError> {
    let members = units.members.as_slice();
    if select.test_packages.len() > 1 {
        return Err(PickError::SeveralPackages { asked: select.test_packages.iter().map(|asked| owned(asked)).collect() });
    }
    let with_bins = || -> Owneds { members.iter().filter(|member| !member.bins.is_empty()).map(|member| owned(member.name.as_str())).collect() };
    let here = at_dir(members, select.member)?;
    let member = match (select.package, select.bin, here) {
        (Some(asked), _, _) => named(members, asked)?,
        // In a member's folder that member is the package, its binaries the ones `--bin` names.
        (None, _, Some(member)) => member,
        (None, Some(asked), None) => {
            let mut having = members.iter().filter(|member| member.bins.iter().any(|bin| bin.name.as_str() == asked));
            match (having.next(), having.next()) {
                (Some(member), None) => member,
                (Some(_), Some(_)) => {
                    let packages = members.iter().filter(|member| member.bins.iter().any(|bin| bin.name.as_str() == asked)).map(|member| owned(member.name.as_str())).collect();
                    return Err(PickError::BinInSeveral { asked: owned(asked), packages });
                }
                (None, _) => {
                    let bins = members.iter().flat_map(|member| member.bins.iter().map(move |bin| (member, bin)));
                    let bins = bins.map(|(member, bin)| owned(&std::format!("{}/{}", member.name.as_str(), bin.name.as_str()))).collect();
                    return Err(PickError::NoSuchBin { asked: owned(asked), package: None, bins });
                }
            }
        }
        (None, None, None) => match members.iter().find(|member| member.dir.as_str().is_empty()) {
            // The root's own package.
            Some(member) => member,
            None => {
                // The members `default-members` names that have a binary (each once); with none
                // written, every member.
                let mut programs: Vec<&Member<'_>, &Frame> = Vec::new_in(frame);
                let defaults = for_defaults(root_text, members, frame, |member| {
                    if !member.bins.is_empty() && !programs.iter().any(|seen| core::ptr::eq(*seen, member)) {
                        programs.push(member);
                    }
                })?;
                if defaults != DefaultPaths::Listed {
                    programs.extend(members.iter().filter(|member| !member.bins.is_empty()));
                }
                match programs.as_slice() {
                    [member] => *member,
                    _ => return Err(PickError::WhichPackage { packages: with_bins() }),
                }
            }
        },
    };
    let bin = match select.bin {
        Some(asked) => match member.bins.iter().find(|bin| bin.name.as_str() == asked) {
            Some(bin) => bin,
            None => {
                let bins = member.bins.iter().map(|bin| owned(&std::format!("{}/{}", member.name.as_str(), bin.name.as_str()))).collect();
                return Err(PickError::NoSuchBin { asked: owned(asked), package: Some(owned(member.name.as_str())), bins });
            }
        },
        None => match member.bins.as_slice() {
            [] => return Err(PickError::NoBinary { package: owned(member.name.as_str()) }),
            [bin] => bin,
            bins => {
                let bins = bins.iter().map(|bin| owned(&std::format!("{} ({})", bin.name.as_str(), bin.root.as_str()))).collect();
                return Err(PickError::WhichBin { package: owned(member.name.as_str()), bins });
            }
        },
    };
    Ok((member, bin))
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum DefaultPaths {
    Absent,
    Empty,
    Listed,
}

/// Visit the members named by the root's `workspace.default-members`. An explicitly empty
/// array selects zero test packages. The existing single-program picker treats it as absent.
fn for_defaults<'u, 'a>(
    root_text: &str, members: &'u [Member<'a>], frame: &Frame, mut visit: impl FnMut(&'u Member<'a>),
) -> Result<DefaultPaths, PickError> {
    let doc = blaze_toml::Document::parse_in(root_text, blaze_toml::Limits::CARGO, frame).map_err(PickError::RootToml)?;
    let Some(item) = doc.root().get("workspace").and_then(|workspace| workspace.get("default-members")) else { return Ok(DefaultPaths::Absent) };
    let (line, column) = item.line_col();
    if !item.is_array() {
        return Err(PickError::DefaultMembers { line, column });
    }
    let mut paths = DefaultPaths::Empty;
    for entry in item.children() {
        let Some(path) = entry.as_str() else {
            let (line, column) = entry.line_col();
            return Err(PickError::DefaultMembers { line, column });
        };
        let mut path = path.trim_end_matches('/');
        while let Some(rest) = path.strip_prefix("./") {
            path = rest;
        }
        let path = if path == "." { "" } else { path };
        let member = members.iter().find(|member| member.dir.as_str() == path)
            .ok_or_else(|| PickError::DefaultNotMember { path: owned(path) })?;
        visit(member);
        paths = DefaultPaths::Listed;
    }
    Ok(paths)
}

/// The workspace the command was given (`checkout`: the start folder, or a folder beneath it),
/// found as the module says: its root as the checkout, and the package's folder from it.
///
/// COST: one `stat` a folder from the one given up to its package; the layout from the package
/// (its manifests and members' listings, as [`pick`] reads them again from the root); the folders
/// above the start folder only when no root was found beneath it (one `getcwd`, one read a
/// folder, nearest first, as cargo's walk does).
pub(crate) fn locate<'f>(cx: &Cx, fs: &Fs, checkout: &Checkout, frame: &'f Frame) -> Result<Located<'f>, CliError> {
    let cx = cx.span("blaze: the workspace's root");
    let start: &'f str = match checkout {
        Checkout::Here => "",
        Checkout::Under(dir) => kept(dir.as_str(), frame)?,
    };
    let here = fs.root();
    // The package's folder: the nearest at or above `start` beneath the start folder with a
    // manifest (cargo's `find_root_manifest_for_wd`).
    let mut package = None;
    let mut dir = start;
    loop {
        if here.stat(&cx, &Path::new(manifest_in(dir, frame)?)?)?.is_some_and(|meta| !meta.dir) {
            package = Some(dir);
            break;
        }
        match up(dir) {
            Some(above) => dir = above,
            None => break,
        }
    }
    let picked = |why: PickError| CliError::RunPackage(why);
    let Some(package) = package else {
        return Err(match above(&cx, fs, start, Want::Manifest, frame)? {
            Some(manifest) => picked(PickError::ManifestAboveStart { manifest }),
            None => picked(PickError::NoManifest { start: owned(start) }),
        });
    };
    let package_manifest = manifest_in(package, frame)?;
    // The package's own manifest with `[workspace]` is its workspace's root (cargo's
    // `find_root`: a `[workspace]` manifest is the root, the search stops there), so the root is
    // the package and the member its root package: one read, no layout. tempo's every command
    // starts in such a folder: its 17.3 ms `locate` (score-c2315wave73, 212 stats and 53 reads,
    // 1.4 ms of CPU: waiting on the files) read every member's manifest only to learn this, and
    // `units::lay` reads them all again after the journal's place. A member's manifest the
    // layout would refuse is refused there, by the same `Layout`, with its file named.
    // A manifest that is no UTF-8 or no TOML is the layout's to refuse, with its words: below.
    let mut text = Bytes::new_in(frame, "the package's manifest");
    here.read_all(&cx, &Path::new(package_manifest)?, &mut text)?;
    if core::str::from_utf8(text.as_slice()).is_ok_and(|text| has(text, &["workspace"], frame).unwrap_or(false)) {
        let root_dir = match package {
            "" => here,
            at => fs.open_dir(&cx, &Path::new(at)?)?,
        };
        return Ok(Located { root: root_dir, at: package, member: "" });
    }
    let laid = with_layout(&cx, &here, package, frame, |units, root_text| {
        let root_manifest = units.manifests.get(units.root.0 as usize).ok_or(CliError::Text { what: "the layout names no root manifest (run_package.rs)" })?;
        let root = match root_manifest.as_str().strip_suffix(MANIFEST) {
            Some("") => "",
            Some(dir) => match dir.strip_suffix('/') {
                Some(dir) => kept(dir, frame)?,
                None => return Err(CliError::Text { what: "the layout's root manifest is not a folder's `Cargo.toml` (run_package.rs)" }),
            },
            None => return Err(CliError::Text { what: "the layout's root manifest is not a folder's `Cargo.toml` (run_package.rs)" }),
        };
        // The root is a workspace's when the layout found one away from the package, or the
        // package's own manifest has `[workspace]`; else the package stands alone, under the
        // start folder at least.
        let workspace = root_manifest.as_str() != package_manifest || has(root_text, &["workspace"], frame).map_err(picked)?;
        Ok((root, workspace))
    });
    let root = match laid {
        Ok((root, true)) => root,
        Ok((_, false)) => match above(&cx, fs, package, Want::Workspace, frame)? {
            Some(manifest) => return Err(picked(PickError::RootAboveStart { manifest, package: owned(shown_dir(package)) })),
            None => package,
        },
        // A refusal of the layout's (a key `workspace = true` with no root to offer it, say):
        // when nothing beneath the start folder names a root, the root above that claims the
        // package is the cause, and is named; else the layout's words stand.
        Err(refused) => {
            if !root_beneath(&cx, &here, package, frame)?
                && let Some(manifest) = above(&cx, fs, package, Want::Workspace, frame)?
            {
                return Err(picked(PickError::RootAboveStart { manifest, package: owned(shown_dir(package)) }));
            }
            return Err(refused);
        }
    };
    let member = match package.strip_prefix(root) {
        _ if root.is_empty() => package,
        Some("") => "",
        Some(rest) if rest.starts_with('/') => &rest[1..],
        _ => return Err(picked(PickError::OutsideRoot { package: owned(shown_dir(package)), root: owned(shown_dir(root)) })),
    };
    let root_dir = match root {
        "" => here,
        at => fs.open_dir(&cx, &Path::new(at)?)?,
    };
    Ok(Located { root: root_dir, at: root, member })
}

/// The folder above `dir` beneath the start folder; `None` for the start folder itself.
fn up(dir: &str) -> Option<&str> {
    match dir.rfind('/') {
        Some(cut) => Some(&dir[..cut]),
        None if !dir.is_empty() => Some(""),
        None => None,
    }
}

/// `dir`'s manifest (`Cargo.toml` for the start folder, `a/Cargo.toml` beneath it), in `frame`.
fn manifest_in<'f>(dir: &str, frame: &'f Frame) -> Result<&'f str, CliError> {
    match dir {
        "" => Ok(MANIFEST),
        dir => kept(bstd::mem::format_in!(frame, "{dir}/{MANIFEST}").as_str(), frame),
    }
}

/// A folder beneath the start folder as a refusal shows it: `.` for the start folder.
fn shown_dir(dir: &str) -> &str {
    if dir.is_empty() { "." } else { dir }
}

/// Whether `text` (a manifest) has the key at `path` (`["workspace"]`, `["package",
/// "workspace"]`).
fn has(text: &str, path: &[&str], frame: &Frame) -> Result<bool, PickError> {
    let doc = blaze_toml::Document::parse_in(text, blaze_toml::Limits::CARGO, frame).map_err(PickError::RootToml)?;
    let mut item = Some(doc.root());
    for key in path {
        item = item.and_then(|item| item.get(key));
    }
    Ok(item.is_some())
}

/// Whether a manifest beneath the start folder names the package's root: the package's own
/// with `[workspace]` or `package.workspace`, or one in a folder above it (up to the start
/// folder) with `[workspace]`. A manifest that is no TOML counts as naming it: the layout's
/// refusal is about it. Read only when the layout refused, so its reads are not kept.
fn root_beneath(cx: &Cx, here: &Dir<ReadWrite>, package: &str, frame: &Frame) -> Result<bool, CliError> {
    let mut dir = Some(package);
    while let Some(at) = dir {
        let mut bytes = Bytes::new_in(frame, "a manifest");
        match here.read_all(cx, &Path::new(manifest_in(at, frame)?)?, &mut bytes) {
            Ok(_) => {
                let Ok(text) = core::str::from_utf8(bytes.as_slice()) else { return Ok(true) };
                let names = match has(text, &["workspace"], frame) {
                    Ok(workspace) => workspace || (at == package && has(text, &["package", "workspace"], frame).unwrap_or(true)),
                    Err(_) => true,
                };
                if names {
                    return Ok(true);
                }
            }
            Err(error) if error.kind == bstd::fs::FsKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        dir = up(at);
    }
    Ok(false)
}

/// What the walk above the start folder looks for.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Want {
    /// Any manifest: the package's, when none is beneath the start folder.
    Manifest,
    /// A `[workspace]` whose `exclude` does not leave the package out (cargo's `find_root`).
    Workspace,
}

/// The nearest folder above the start folder whose `Cargo.toml` is what `want` asks, as its
/// manifest's name from the start folder (`../../Cargo.toml`); `None` when none has one. The
/// package is `package` beneath the start folder (`""`: the start folder).
fn above(cx: &Cx, fs: &Fs, package: &str, want: Want, frame: &Frame) -> Result<Option<Owned>, CliError> {
    let failed = |error: AncestorError| CliError::RunPackage(PickError::Above(error));
    let ancestors = fs.ancestors(cx).map_err(failed)?;
    let manifest = Path::new(MANIFEST)?;
    for (at, ancestor) in ancestors.iter().enumerate() {
        let mut bytes = Bytes::new_in(frame, "a manifest above the start folder");
        match ancestor.read_all(cx, &manifest, &mut bytes) {
            Ok(_) => {}
            Err(error) if error.kind == AncestorKind::NotFound => continue,
            Err(error) => return Err(failed(error)),
        }
        let mut shown: Vec<u8, &Frame> = Vec::new_in(frame);
        for _ in 0..ancestor.up() {
            shown.extend_from_slice(b"../");
        }
        shown.extend_from_slice(MANIFEST.as_bytes());
        let shown = core::str::from_utf8(shown.as_slice()).map_err(|_| CliError::Text { what: "a path of `..` steps is UTF-8 (run_package.rs)" })?;
        if want == Want::Manifest {
            return Ok(Some(owned(shown)));
        }
        let text = core::str::from_utf8(bytes.as_slice()).map_err(|_| CliError::Text { what: "a manifest above the start folder is not UTF-8 (run_package.rs)" })?;
        let doc = blaze_toml::Document::parse_in(text, blaze_toml::Limits::CARGO, frame)
            .map_err(|error| CliError::RunPackage(PickError::AboveToml { manifest: owned(shown), error }))?;
        let Some(workspace) = doc.root().get("workspace") else { continue };
        // The package's path beneath this folder: the folders on the way down to the start
        // folder, then `package`.
        let mut path: Vec<u8, &Frame> = Vec::new_in(frame);
        for lower in ancestors[..=at].iter().rev() {
            if !path.is_empty() {
                path.push(b'/');
            }
            path.extend_from_slice(lower.toward().as_bytes());
        }
        if !package.is_empty() {
            path.push(b'/');
            path.extend_from_slice(package.as_bytes());
        }
        let path = core::str::from_utf8(path.as_slice()).map_err(|_| CliError::Text { what: "folder names joined by `/` are UTF-8 (run_package.rs)" })?;
        if !left_out(&workspace, path) {
            return Ok(Some(owned(shown)));
        }
    }
    Ok(None)
}

/// cargo's `is_excluded` (layout.rs `excluded`) for a root above the start folder: `path` (the
/// package's folder from the root) is under an `exclude` entry and under no `members` entry,
/// each read as a path, not a glob.
fn left_out(workspace: &blaze_toml::Item<'_, '_, &Frame>, path: &str) -> bool {
    let under = |key: &str| workspace.get(key).is_some_and(|list| list.children().any(|entry| entry.as_str().is_some_and(|entry| beneath(path, entry))));
    under("exclude") && !under("members")
}

/// Whether `path` is `entry` or beneath it, both from one root, `.` and empty parts skipped. An
/// entry with `..` leaves the root, and covers nothing here.
fn beneath(path: &str, entry: &str) -> bool {
    let mut parts = path.split('/').filter(|part| !part.is_empty() && *part != ".");
    for want in entry.split('/').filter(|part| !part.is_empty() && *part != ".") {
        if want == ".." || parts.next() != Some(want) {
            return false;
        }
    }
    true
}

/// The program `select` chooses in the checkout at `root` (the workspace's root, [`locate`]),
/// read by the workspace's manifest reader (blaze-manifest `Layout`) through `root`, every text
/// kept in `frame`.
pub(crate) fn pick<'f>(cx: &Cx, root: &Dir<ReadWrite>, select: Select<'_>, frame: &'f Frame) -> Result<Program<'f>, CliError> {
    let cx = cx.span("blaze run: which program");
    with_layout(&cx, root, "", frame, |units, root_text| {
        let (member, bin) = choose(units, root_text, select, frame).map_err(CliError::RunPackage)?;
        let manifest = units.manifests.get(member.manifest.0 as usize).ok_or(CliError::Text { what: "the layout names a manifest it did not read (run_package.rs)" })?;
        let edition = match member.edition.blaze {
            Some(blaze) => blaze.manifest(),
            None => kept(bstd::mem::format_in!(frame, "{}", member.edition.syntax).as_str(), frame)?,
        };
        Ok(Program {
            package: kept(member.name.as_str(), frame)?,
            bin: kept(bin.name.as_str(), frame)?,
            manifest: kept(manifest.as_str(), frame)?,
            root: kept(bin.root.as_str(), frame)?,
            edition,
        })
    })
}

/// The packages `blaze test` runs the tests of, by name, sorted, each once (run_names.rs
/// `tests_of` selects the unit graph's units by them). Every `-p` given, else the member the
/// command was given ([`Select::member`]), else the root's `default-members`, else the root's
/// package of a real workspace and every member of a virtual one. With `--bin <name>`, the
/// selected packages that have that binary, at least one.
pub(crate) fn test_packages<'f>(cx: &Cx, root: &Dir<ReadWrite>, select: Select<'_>, frame: &'f Frame) -> Result<Vec<&'f str, &'f Frame>, CliError> {
    let cx = cx.span("blaze test: selected packages");
    with_layout(&cx, root, "", frame, |units, root_text| {
        let members = units.members.as_slice();
        let mut selected = Vec::new_in(frame);
        if !select.test_packages.is_empty() {
            for package in select.test_packages {
                selected.push(named(members, package).map_err(CliError::RunPackage)?);
            }
        } else if let Some(member) = at_dir(members, select.member).map_err(CliError::RunPackage)? {
            selected.push(member);
        } else if for_defaults(root_text, members, frame, |member| selected.push(member)).map_err(CliError::RunPackage)? == DefaultPaths::Absent {
            match members.iter().find(|member| member.manifest == units.root) {
                Some(member) => selected.push(member),
                None => selected.extend(members),
            }
        }
        if let Some(asked) = select.bin {
            let has_bin = |member: &&Member<'_>| member.bins.iter().any(|bin| bin.name.as_str() == asked);
            if !selected.iter().any(has_bin) {
                let bins = selected.iter().flat_map(|member| member.bins.iter().map(move |bin| (member, bin)));
                let bins = bins.map(|(member, bin)| owned(bstd::mem::format_in!(frame, "{}/{}", member.name.as_str(), bin.name.as_str()).as_str())).collect();
                return Err(CliError::RunPackage(PickError::TestBin { asked: owned(asked), bins }));
            }
            selected.retain(has_bin);
        }
        selected.sort_unstable_by(|a, b| a.name.as_str().cmp(b.name.as_str()));
        selected.dedup_by(|a, b| core::ptr::eq(*a, *b));
        let mut names = Vec::with_capacity_in(selected.len(), frame);
        for member in selected {
            names.push(kept(member.name.as_str(), frame)?);
        }
        Ok(names)
    })
}

fn kept<'f>(text: &str, frame: &'f Frame) -> Result<&'f str, CliError> {
    Ok(frame.alloc_str(text))
}

/// The layout of the workspace of the package in `start` (beneath `root`; `""` for `root`
/// itself), then `select` over it and the root manifest's text.
fn with_layout<'f, T>(
    cx: &Cx, root: &Dir<ReadWrite>, start: &str, frame: &'f Frame, select: impl FnOnce(&Units<'_>, &str) -> Result<T, CliError>,
) -> Result<T, CliError> {
    let refused = |why: &dyn core::fmt::Display| CliError::Program(crate::ManifestRefused::of(why, frame));
    // Each manifest read (path, text), for the root's `default-members`.
    let mut read: Vec<(&'f str, &'f str), &'f Frame> = Vec::new_in(frame);
    let erased = blaze_own::Erased::new(frame);
    let mut layout = Layout::new(start, erased.handle()).map_err(|e| refused(&e))?;
    for _ in 0..MOST_PASSES {
        let needs = match layout.pass().map_err(|e| refused(&e))? {
            Pass::Need(needs) => needs,
            Pass::Done(units) => {
                let root_path = units.manifests.get(units.root.0 as usize).ok_or(CliError::Text { what: "the layout names no root manifest (run_package.rs)" })?;
                let (_, text) = read.iter().find(|(path, _)| *path == root_path.as_str())
                    .ok_or(CliError::Text { what: "the root manifest is absent from the layout's reads (run_package.rs)" })?;
                return select(&units, text);
            }
        };
        for need in needs.iter() {
            match need {
                Need::Manifest(path) => {
                    let mut bytes = Bytes::new_in(frame, "a manifest");
                    let text = match root.read_all(cx, &Path::new(path.as_str())?, &mut bytes) {
                        Ok(_) => Some(kept(core::str::from_utf8(bytes.as_slice()).map_err(|_| CliError::Text { what: "a manifest is not UTF-8" })?, frame)?),
                        Err(error) if error.kind == bstd::fs::FsKind::NotFound => None,
                        Err(error) => return Err(error.into()),
                    };
                    if let Some(text) = text {
                        read.push((kept(path.as_str(), frame)?, text));
                    }
                    layout.manifest(path.as_str(), text).map_err(|e| refused(&e))?;
                }
                Need::Listing(dir) => {
                    // `Dir::list` gives no entries for a directory that is not there.
                    let listed = match dir.as_str() {
                        "" => root.list(cx, frame)?,
                        named => root.subdir(&Path::new(named)?).list(cx, frame)?,
                    };
                    let mut entries: Vec<Entry<'_>, &Frame> = Vec::with_capacity_in(listed.len(), frame);
                    for (path, meta) in listed.iter() {
                        entries.push(Entry { name: path.as_str(), is_dir: meta.dir });
                    }
                    layout.listing(dir.as_str(), Some(&entries));
                }
            }
        }
    }
    Err(CliError::Text { what: "the workspace's layout asked for more than 64 passes (run_package.rs)" })
}

#[cfg(test)]
mod tests {
    use bstd::fs::{Content, Durable};
    use bstd::io::{Io, Limits, SimWorld};
    use bstd::mem::TASK_FRAME;
    use bstd::test::BUDGET;

    use super::*;

    fn checkout<T>(files: &[(&str, &str)], read: impl FnOnce(&Cx, &Dir<ReadWrite>) -> T) -> T {
        let io = Io::simulated(SimWorld::new(1, Limits::USUAL));
        let cx = io.cx(BUDGET, TASK_FRAME).expect("a root context");
        let root = io.need_fs().expect("files").open_dir(&cx, &Path::new("data").expect("a path")).expect("open");
        for (path, text) in files {
            root.publish(&cx, &Path::new(path).expect("a path"), Content::bytes(text.as_bytes()), Durable::No).expect("write");
        }
        read(&cx, &root)
    }

    /// The choice `select` makes in a simulated checkout of `files` (path, text), its texts
    /// copied out of the command's frame.
    fn picked(files: &[(&str, &str)], select: Select<'_>) -> Result<(std::string::String, std::string::String, std::string::String), PickError> {
        checkout(files, |cx, root| match pick(cx, root, select, cx.frame()) {
            Ok(program) => Ok((program.package.into(), program.manifest.into(), program.root.into())),
            Err(CliError::RunPackage(why)) => Err(why),
            Err(other) => panic!("a refusal that is not the choice's: {other}"),
        })
    }

    fn package(name: &str) -> std::string::String {
        std::format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2024\"\n")
    }

    fn strings(items: &[&str]) -> Owneds {
        items.iter().map(|item| owned(item)).collect()
    }

    fn triple(package: &str, manifest: &str, root: &str) -> (std::string::String, std::string::String, std::string::String) {
        (package.into(), manifest.into(), root.into())
    }

    /// A virtual root over `app` and `tool` (each a binary) and `lib` (a library), with the
    /// root's text `root`.
    fn virtual_three(root: &str, test: impl FnOnce(&[(&str, &str)])) {
        let (app, tool, lib) = (package("app"), package("tool"), package("lib"));
        let files = [
            ("Cargo.toml", root),
            ("app/Cargo.toml", app.as_str()),
            ("app/src/main.rs", "fn main() {}"),
            ("tool/Cargo.toml", tool.as_str()),
            ("tool/src/main.rs", "fn main() {}"),
            ("lib/Cargo.toml", lib.as_str()),
            ("lib/src/lib.rs", ""),
        ];
        test(&files);
    }

    const THREE: &str = "[workspace]\nmembers = [\"app\", \"tool\", \"lib\"]\n";

    #[test]
    fn test_selection_honors_workspace_defaults_before_the_root_package() {
        let project = std::format!("{}\n{THREE}", package("here"));
        let explicit = std::format!("{project}default-members = [\"./tool/\", \"lib\", \"tool\"]\n");
        let empty = std::format!("{project}default-members = []\n");
        for (manifest, expected) in [
            (THREE, &["app", "lib", "tool"][..]),
            (project.as_str(), &["here"][..]),
            (explicit.as_str(), &["lib", "tool"][..]),
            (empty.as_str(), &[][..]),
        ] {
            virtual_three(manifest, |files| {
                let mut files = files.to_vec();
                files.push(("src/main.rs", "fn main() {}"));
                checkout(&files, |cx, root| {
                    let defaults = test_packages(cx, root, Select::NONE, cx.frame()).expect("test's default packages");
                    assert_eq!(defaults.as_slice(), expected);
                    let explicit = test_packages(cx, root, asked(&["tool", "app", "tool"]), cx.frame()).expect("the named packages");
                    assert_eq!(explicit.as_slice(), ["app", "tool"]);
                    assert!(matches!(test_packages(cx, root, asked(&["absent", "app"]), cx.frame()), Err(CliError::RunPackage(PickError::NoSuchPackage { .. }))));
                });
            });
        }
    }

    /// The checkout's root is a package with `src/main.rs`: it is the program, with no flag.
    #[test]
    fn the_root_package_is_the_program() {
        let hello = package("hello");
        let files = [("Cargo.toml", hello.as_str()), ("src/main.rs", "fn main() {}")];
        assert_eq!(picked(&files, Select::NONE), Ok(triple("hello", "Cargo.toml", "src/main.rs")));
        let lib = [("Cargo.toml", hello.as_str()), ("src/lib.rs", "")];
        assert_eq!(picked(&lib, Select::NONE), Err(PickError::NoBinary { package: owned("hello") }));
    }

    /// Under a virtual root `-p` names the member; a name no member has is refused with the
    /// members' names.
    #[test]
    fn a_virtual_root_runs_the_package_p_names() {
        virtual_three(THREE, |files| {
            assert_eq!(picked(files, Select { package: Some("tool"), ..Select::NONE }), Ok(triple("tool", "tool/Cargo.toml", "tool/src/main.rs")));
            assert_eq!(picked(files, Select { package: Some("lib"), ..Select::NONE }), Err(PickError::NoBinary { package: owned("lib") }));
            assert_eq!(
                picked(files, Select { package: Some("nope"), ..Select::NONE }),
                Err(PickError::NoSuchPackage { asked: owned("nope"), packages: strings(&["app", "tool", "lib"]) })
            );
        });
    }

    /// A virtual root whose `default-members` names one package with a binary (and a library):
    /// that package, with no flag. One that names no member is refused by its path.
    #[test]
    fn a_virtual_root_runs_its_one_default_member() {
        virtual_three("[workspace]\nmembers = [\"app\", \"tool\", \"lib\"]\ndefault-members = [\"./tool/\", \"lib\"]\n", |files| {
            assert_eq!(picked(files, Select::NONE), Ok(triple("tool", "tool/Cargo.toml", "tool/src/main.rs")));
        });
        virtual_three("[workspace]\nmembers = [\"app\", \"tool\", \"lib\"]\ndefault-members = [\"gone\"]\n", |files| {
            assert_eq!(picked(files, Select::NONE), Err(PickError::DefaultNotMember { path: owned("gone") }));
        });
    }

    /// A virtual root with two packages that have a binary and neither flag nor default member:
    /// the refusal names both, and says how to choose.
    #[test]
    fn a_virtual_root_with_two_programs_says_which() {
        virtual_three(THREE, |files| {
            let refused = picked(files, Select::NONE);
            assert_eq!(refused, Err(PickError::WhichPackage { packages: strings(&["app", "tool"]) }));
            let words = refused.err().map(|why| why.to_string()).unwrap_or_default();
            assert!(words.contains("say which: `blaze run -p <name>`") && words.contains("`app`, `tool`"), "{words}");
        });
        // Default members that name both are no choice either.
        virtual_three("[workspace]\nmembers = [\"app\", \"tool\", \"lib\"]\ndefault-members = [\"app\", \"tool\"]\n", |files| {
            assert_eq!(picked(files, Select::NONE), Err(PickError::WhichPackage { packages: strings(&["app", "tool"]) }));
        });
        // One package with a binary in the whole workspace is the program.
        let (app, lib) = (package("app"), package("lib"));
        let one = [
            ("Cargo.toml", "[workspace]\nmembers = [\"app\", \"lib\"]\n"),
            ("app/Cargo.toml", app.as_str()),
            ("app/src/main.rs", "fn main() {}"),
            ("lib/Cargo.toml", lib.as_str()),
            ("lib/src/lib.rs", ""),
        ];
        assert_eq!(picked(&one, Select::NONE), Ok(triple("app", "app/Cargo.toml", "app/src/main.rs")));
    }

    /// `--bin` picks among a package's binaries; with none named, several are refused by name;
    /// with no `-p` it finds the one member holding a binary of that name.
    #[test]
    fn bin_picks_among_a_packages_binaries() {
        let two = std::format!("{}\n[[bin]]\nname = \"extra\"\npath = \"src/extra.rs\"\n", package("two"));
        let files = [("Cargo.toml", two.as_str()), ("src/main.rs", "fn main() {}"), ("src/extra.rs", "fn main() {}")];
        assert_eq!(picked(&files, Select { bin: Some("extra"), ..Select::NONE }), Ok(triple("two", "Cargo.toml", "src/extra.rs")));
        assert_eq!(picked(&files, Select { package: Some("two"), bin: Some("two"), ..Select::NONE }), Ok(triple("two", "Cargo.toml", "src/main.rs")));
        let Err(PickError::WhichBin { package, bins }) = picked(&files, Select::NONE) else { panic!("two binaries and no `--bin`") };
        assert_eq!((package, bins.len()), (owned("two"), 2));
        assert!(matches!(picked(&files, Select { package: Some("two"), bin: Some("nope"), ..Select::NONE }), Err(PickError::NoSuchBin { package: Some(_), .. })));
        virtual_three(THREE, |files| {
            assert_eq!(picked(files, Select { bin: Some("app"), ..Select::NONE }), Ok(triple("app", "app/Cargo.toml", "app/src/main.rs")));
            assert!(matches!(picked(files, Select { bin: Some("nope"), ..Select::NONE }), Err(PickError::NoSuchBin { package: None, .. })));
        });
    }

    /// The program's own files are under its crate root's folder.
    #[test]
    fn the_programs_sources_are_its_roots_folder() {
        let program = Program { package: "tempo", bin: "tempo", manifest: "bin/tempo/Cargo.toml", root: "bin/tempo/src/main.rs", edition: "2024" };
        assert_eq!((program.sources(), program.units()), ("bin/tempo/src/", Name::manifest(b"bin/tempo/Cargo.toml")));
    }

    /// The selection of the `-p`s `packages`, at the root.
    fn asked<'a>(packages: &'a [&'a str]) -> Select<'a> {
        Select { test_packages: packages, ..Select::NONE }
    }

    /// A simulated start folder holding `files`, with the folders `above` it, read by `read`.
    fn started<T>(files: &[(&str, &str)], above: std::vec::Vec<blaze_core::sim::SimAbove>, read: impl FnOnce(&Cx, &Fs) -> T) -> T {
        let mut world = SimWorld::new(1, Limits::USUAL);
        world.top = u8::MAX;
        world.above = above;
        let io = Io::simulated(world);
        let cx = io.cx(BUDGET, TASK_FRAME).expect("a root context");
        let fs = io.need_fs().expect("files");
        let root = fs.root();
        for (path, text) in files {
            root.publish(&cx, &Path::new(path).expect("a path"), Content::bytes(text.as_bytes()), Durable::No).expect("write");
        }
        read(&cx, &fs)
    }

    /// One folder above the start folder, holding a `Cargo.toml` of `manifest`; the start
    /// folder is `toward` in it.
    fn above_with(toward: &str, manifest: &str) -> std::vec::Vec<blaze_core::sim::SimAbove> {
        let name = blaze_core::FileName::new("Cargo.toml").expect("a name");
        std::vec![blaze_core::sim::SimAbove { toward: toward.into(), files: std::vec![(name, manifest.as_bytes().to_vec())] }]
    }

    /// A member's manifest that inherits its edition from the root.
    fn inheriting(name: &str) -> std::string::String {
        std::format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition.workspace = true\n")
    }

    const TWO: &str = "[workspace]\nmembers = [\"a\", \"b\"]\n\n[workspace.package]\nedition = \"2021\"\n";

    /// A virtual root over `a` (a binary) and `b` (a library), both with `edition.workspace =
    /// true`, the root's `[workspace.package]` giving 2021.
    fn two_members() -> std::vec::Vec<(&'static str, std::string::String)> {
        std::vec![
            ("Cargo.toml", TWO.into()),
            ("a/Cargo.toml", inheriting("a")),
            ("a/src/main.rs", "fn main() {}".into()),
            ("b/Cargo.toml", inheriting("b")),
            ("b/src/lib.rs", std::string::String::new()),
        ]
    }

    fn as_files<'a>(files: &'a [(&'static str, std::string::String)]) -> std::vec::Vec<(&'static str, &'a str)> {
        files.iter().map(|(path, text)| (*path, text.as_str())).collect()
    }

    /// Run.md R3's first test: `test` given a member's folder of a two-member workspace whose
    /// members write `edition.workspace = true` walks up to the root and reads its
    /// `[workspace.package]`: the root is the checkout, the member its default package, and the
    /// program's edition is the root's 2021. Given the member's `src` (no manifest there), the
    /// nearest manifest is the member's. Control: the member's folder opened as the checkout,
    /// today's way (the layout read from it alone), is the refusal run.md quotes ("the root's
    /// `[workspace]` does not offer it").
    #[test]
    fn a_member_folder_reads_the_roots_workspace_package() {
        let files = two_members();
        started(&as_files(&files), std::vec::Vec::new(), |cx, fs| {
            for given in ["a", "a/src"] {
                let checkout = Checkout::Under(Path::new(given).expect("a path"));
                let located = locate(cx, fs, &checkout, cx.frame()).unwrap_or_else(|error| panic!("{given}: {error}"));
                assert_eq!((located.at, located.member), ("", "a"), "given {given}");
                let here = Select { member: located.member, ..Select::NONE };
                let tested = test_packages(cx, &located.root, here, cx.frame()).unwrap_or_else(|error| panic!("{error}"));
                assert_eq!(tested.as_slice(), ["a"]);
                let program = pick(cx, &located.root, here, cx.frame()).unwrap_or_else(|error| panic!("{error}"));
                assert_eq!((program.package, program.manifest, program.root, program.edition), ("a", "a/Cargo.toml", "a/src/main.rs", "2021"));
            }
            // The start folder is the root: no member, the virtual root's defaults (both).
            let located = locate(cx, fs, &Checkout::Here, cx.frame()).unwrap_or_else(|error| panic!("{error}"));
            assert_eq!((located.at, located.member), ("", ""));
            let tested = test_packages(cx, &located.root, Select::NONE, cx.frame()).unwrap_or_else(|error| panic!("{error}"));
            assert_eq!(tested.as_slice(), ["a", "b"]);
            // Control: the member's folder as the checkout itself.
            let alone = fs.root().subdir(&Path::new("a").expect("a path"));
            match pick(cx, &alone, Select::NONE, cx.frame()) {
                Err(CliError::Program(crate::ManifestRefused { why })) => assert!(why.contains("does not offer it"), "{why}"),
                Err(other) => panic!("the member read alone is refused by its layout, not: {other}"),
                Ok(program) => panic!("the member read alone was chosen: {program:?}"),
            }
        });
    }

    /// A start folder whose manifest has `[workspace]` is its own root, found from that one
    /// manifest: the answer is the layout's (the oracle: `with_layout`'s root, read whole), for
    /// a virtual root and a root package, and a member's manifest the layout refuses is left to
    /// the layout's later read. CONTROL: a member's folder (no `[workspace]` of its own) still
    /// walks the layout up to the root, and the refused member there still refuses.
    #[test]
    fn a_workspace_root_at_the_start_is_found_from_its_own_manifest() {
        let rooted = std::format!("{}\n{TWO}", package("here"));
        for root in [TWO, rooted.as_str()] {
            let mut files = two_members();
            files[0].1 = root.into();
            started(&as_files(&files), std::vec::Vec::new(), |cx, fs| {
                let located = locate(cx, fs, &Checkout::Here, cx.frame()).unwrap_or_else(|error| panic!("{error}"));
                let laid = with_layout(cx, &fs.root(), "", cx.frame(), |units, _| Ok(units.manifests[units.root.0 as usize].as_str() == MANIFEST)).expect("the layout");
                assert!(laid, "the oracle: the layout's root is the start folder's manifest");
                assert_eq!((located.at, located.member), ("", ""));
            });
        }
        // `b`'s manifest is no TOML: the root is still found, and the layout refuses it.
        let mut files = two_members();
        files[3].1 = "[package\n".into();
        started(&as_files(&files), std::vec::Vec::new(), |cx, fs| {
            let located = locate(cx, fs, &Checkout::Here, cx.frame()).unwrap_or_else(|error| panic!("{error}"));
            assert_eq!((located.at, located.member), ("", ""));
            assert!(with_layout(cx, &fs.root(), "", cx.frame(), |_, _| Ok(())).is_err(), "the layout refuses `b`");
            // CONTROL: from `a`, the layout is walked, and it refuses.
            assert!(locate(cx, fs, &Checkout::Under(Path::new("a").expect("a path")), cx.frame()).is_err(), "a member's folder reads the layout");
        });
    }

    /// A root beneath the start folder (`blaze test ws/a`): the root's path is `ws`, the member
    /// `a` from it.
    #[test]
    fn a_root_beneath_the_start_folder_is_the_checkout() {
        let files = two_members();
        let placed: std::vec::Vec<(std::string::String, &str)> = files.iter().map(|(path, text)| (std::format!("ws/{path}"), text.as_str())).collect();
        let placed: std::vec::Vec<(&str, &str)> = placed.iter().map(|(path, text)| (path.as_str(), *text)).collect();
        started(&placed, std::vec::Vec::new(), |cx, fs| {
            let located = locate(cx, fs, &Checkout::Under(Path::new("ws/b").expect("a path")), cx.frame()).unwrap_or_else(|error| panic!("{error}"));
            assert_eq!((located.at, located.member), ("ws", "b"));
            let tested = test_packages(cx, &located.root, Select { member: located.member, ..Select::NONE }, cx.frame()).unwrap_or_else(|error| panic!("{error}"));
            assert_eq!(tested.as_slice(), ["b"]);
        });
    }

    /// `-p a -p b` selects both for `test` (sorted, once), and overrides the member the command
    /// was given; an unknown `-p` is refused naming the candidates; `run` refuses two `-p`.
    #[test]
    fn packages_are_selected_by_every_p() {
        let files = two_members();
        started(&as_files(&files), std::vec::Vec::new(), |cx, fs| {
            let root = fs.root();
            let both = test_packages(cx, &root, asked(&["b", "a", "b"]), cx.frame()).unwrap_or_else(|error| panic!("{error}"));
            assert_eq!(both.as_slice(), ["a", "b"]);
            let over = test_packages(cx, &root, Select { member: "a", ..asked(&["b"]) }, cx.frame()).unwrap_or_else(|error| panic!("{error}"));
            assert_eq!(over.as_slice(), ["b"]);
            match test_packages(cx, &root, asked(&["c"]), cx.frame()) {
                Err(CliError::RunPackage(why)) => {
                    assert_eq!(why, PickError::NoSuchPackage { asked: owned("c"), packages: strings(&["a", "b"]) });
                    let words = why.to_string();
                    assert!(words.contains("`-p c`") && words.contains("`a`, `b`"), "{words}");
                }
                other => panic!("an unknown `-p` is refused by name: {:?}", other.map(|names| names.len())),
            }
            let two = Select { package: Some("b"), ..asked(&["a", "b"]) };
            assert!(matches!(pick(cx, &root, two, cx.frame()), Err(CliError::RunPackage(PickError::SeveralPackages { .. }))));
            // A member folder no member is at is refused with the members.
            assert!(matches!(
                test_packages(cx, &root, Select { member: "c", ..Select::NONE }, cx.frame()),
                Err(CliError::RunPackage(PickError::NotAMember { .. }))
            ));
        });
    }

    /// `test --bin a` keeps the packages with that binary; one no selected package has is
    /// refused with the binaries there are.
    #[test]
    fn test_bin_keeps_the_packages_with_it() {
        let files = two_members();
        started(&as_files(&files), std::vec::Vec::new(), |cx, fs| {
            let root = fs.root();
            let kept = test_packages(cx, &root, Select { bin: Some("a"), ..Select::NONE }, cx.frame()).unwrap_or_else(|error| panic!("{error}"));
            assert_eq!(kept.as_slice(), ["a"]);
            match test_packages(cx, &root, Select { bin: Some("nope"), ..Select::NONE }, cx.frame()) {
                Err(CliError::RunPackage(why)) => assert_eq!(why, PickError::TestBin { asked: owned("nope"), bins: strings(&["a/a"]) }),
                other => panic!("an unknown `--bin` is refused: {:?}", other.map(|names| names.len())),
            }
        });
    }

    /// Started in a member whose root is above the start folder: the root is found by the
    /// walk above and named, never the member read alone. Control: a root above that excludes
    /// the member is no root of it, and the layout's own refusal stands; with no folder above,
    /// a lone package is its own root.
    #[test]
    fn a_root_above_the_start_folder_is_named() {
        let member = inheriting("a");
        let files = [("Cargo.toml", member.as_str()), ("src/lib.rs", "")];
        let claims = "[workspace]\nmembers = [\"a\"]\n\n[workspace.package]\nedition = \"2021\"\n";
        started(&files, above_with("a", claims), |cx, fs| match locate(cx, fs, &Checkout::Here, cx.frame()) {
            Err(CliError::RunPackage(why)) => assert_eq!(why, PickError::RootAboveStart { manifest: owned("../Cargo.toml"), package: owned(".") }),
            Err(other) => panic!("the root above is named, not: {other}"),
            Ok(located) => panic!("a root above was taken as {:?}", (located.at, located.member)),
        });
        let excludes = "[workspace]\nmembers = []\nexclude = [\"a\"]\n";
        started(&files, above_with("a", excludes), |cx, fs| {
            assert!(matches!(locate(cx, fs, &Checkout::Here, cx.frame()), Err(CliError::Program(crate::ManifestRefused { .. }))));
        });
        let lone = package("a");
        let files = [("Cargo.toml", lone.as_str()), ("src/main.rs", "fn main() {}")];
        started(&files, std::vec::Vec::new(), |cx, fs| {
            let located = locate(cx, fs, &Checkout::Here, cx.frame()).unwrap_or_else(|error| panic!("{error}"));
            assert_eq!((located.at, located.member), ("", ""));
        });
        // No manifest anywhere: refused by the start folder.
        started(&[("src/main.rs", "fn main() {}")], std::vec::Vec::new(), |cx, fs| {
            assert!(matches!(locate(cx, fs, &Checkout::Here, cx.frame()), Err(CliError::RunPackage(PickError::NoManifest { .. }))));
        });
    }
}
