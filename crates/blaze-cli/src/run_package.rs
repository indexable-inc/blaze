//! Which program `blaze run` runs: a workspace member's package and one of its binaries, chosen
//! from the command line and the workspace's layout before the request starts (run.md "End
//! state (D113)", stage 4: `run` checks what that program reaches, blaze-drive run_scope.rs).
//!
//! The rule (useful, not parity, rule 5; the words a cargo user already types):
//!
//! - The package: `-p <name>` (`--package`) names a member by its package name. Else, with
//!   `--bin <name>`, the one member that has a binary of that name. Else the root's own package
//!   when the root is one. Else, under a virtual root, the one package with a binary among the
//!   root's `default-members` when it is written, or among all members when it is not (cargo's
//!   `cargo run` in a virtual workspace with one binary). Else a refusal that lists every
//!   package with a binary: "say which: `blaze run -p <name>`".
//! - The binary: `--bin <name>` names one of the package's. Else its one binary. Else a
//!   refusal that lists them: "say which: `--bin <name>`".
//!
//! Nothing is guessed past that: a name that matches nothing, or matches twice, is a refusal
//! naming what was asked and what there is (rule 3), never another package run instead.
//!
//! The choice is read from the manifests by the workspace's one manifest reader (blaze-manifest
//! `Layout`), plus the root's `workspace.default-members`, which that reader leaves out. Its
//! answer is in the request's keys: the program's crate root names the program (native_image.rs
//! `program_name`, in the request's shape) and the package's manifest names `run`'s closure
//! (`Request::with_member_closure`), so two choices never share a certificate (rule 21).

use blaze_drive::Name;
use blaze_ladder::run::RunError;
use blaze_manifest::layout::{Entry, Layout, Member, Need, Pass, Target, Units};
use blaze_own::Vec;
use bstd::Cx;
use bstd::fs::{Dir, ReadWrite};
use bstd::mem::{Bytes, Frame};
use bstd::path::Path;

use crate::CliError;

/// The passes the layout may take (units.rs `MOST_PASSES`; rule 18).
const MOST_PASSES: usize = 64;

/// What the command line asks of the choice: `-p <name>` and `--bin <name>`, each optional.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Select<'a> {
    pub package: Option<&'a str>,
    pub bin: Option<&'a str>,
}

impl Select<'_> {
    /// Neither flag: the rule's defaults.
    pub const NONE: Select<'static> = Select { package: None, bin: None };
}

/// The program chosen: its package, the package's manifest, the binary's crate root (each
/// checkout-relative) and the package's edition as a manifest writes it (`2024`,
/// `blaze-2026.11`), what the runner's front end is given.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Program<'f> {
    pub package: &'f str,
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
                write!(f, "`blaze run -p {asked}`: the workspace has no package `{asked}`; its packages: ")?;
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
                write!(f, "`blaze run`: the root's `default-members` names `{path}`, which is no member of the workspace ({rule})")
            }
            PickError::DefaultMembers { line, column } => {
                write!(f, "`blaze run`: the root's `default-members` at line {line}, column {column} is not an array of paths ({rule})")
            }
        }
    }
}

fn owned(text: &str) -> Owned {
    Owned::from(text)
}

/// The package and binary `select` chooses from the workspace's layout, the root's manifest
/// text beside it (for `default-members`).
fn choose<'u, 'a>(units: &'u Units<'a>, root_text: &str, select: Select<'_>) -> Result<(&'u Member<'a>, &'u Target<'a>), PickError> {
    let members = units.members.as_slice();
    let with_bins = || -> Owneds { members.iter().filter(|member| !member.bins.is_empty()).map(|member| owned(member.name.as_str())).collect() };
    let member = match (select.package, select.bin) {
        (Some(asked), _) => match members.iter().find(|member| member.name.as_str() == asked) {
            Some(member) => member,
            None => return Err(PickError::NoSuchPackage { asked: owned(asked), packages: members.iter().map(|member| owned(member.name.as_str())).collect() }),
        },
        (None, Some(asked)) => {
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
        (None, None) => match members.iter().find(|member| member.dir.as_str().is_empty()) {
            // The root's own package.
            Some(member) => member,
            None => {
                // The members `default-members` names that have a binary (each once); with none
                // written, every member.
                let defaults = default_members(root_text)?;
                let mut programs: std::vec::Vec<&'u Member<'a>, std::alloc::Global> = std::vec::Vec::new();
                for path in defaults.iter() {
                    let Some(member) = members.iter().find(|member| member.dir.as_str() == path.as_ref()) else {
                        return Err(PickError::DefaultNotMember { path: path.clone() });
                    };
                    if !member.bins.is_empty() && !programs.iter().any(|seen| core::ptr::eq(*seen, member)) {
                        programs.push(member);
                    }
                }
                if defaults.is_empty() {
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

/// The root's `workspace.default-members`, each path as a member's directory is written
/// (root-relative, no `./`, no trailing `/`; `.` is the root's own); empty when none is written.
fn default_members(root_text: &str) -> Result<Owneds, PickError> {
    let mut paths = Owneds::new();
    // The layout read this text already: a manifest it refused never comes here.
    let Ok(doc) = blaze_toml::Document::parse_in(root_text, blaze_toml::Limits::CARGO, std::alloc::Global) else { return Ok(paths) };
    let Some(item) = doc.root().get("workspace").and_then(|workspace| workspace.get("default-members")) else { return Ok(paths) };
    let (line, column) = item.line_col();
    if !item.is_array() {
        return Err(PickError::DefaultMembers { line, column });
    }
    for entry in item.children() {
        let Some(path) = entry.as_str() else {
            let (line, column) = entry.line_col();
            return Err(PickError::DefaultMembers { line, column });
        };
        let mut path = path.trim_end_matches('/');
        while let Some(rest) = path.strip_prefix("./") {
            path = rest;
        }
        paths.push(owned(if path == "." { "" } else { path }));
    }
    Ok(paths)
}

/// The program `select` chooses in the checkout at `root`, read by the workspace's manifest
/// reader (blaze-manifest `Layout`) through `root`, every text kept in `frame`.
///
/// Owed (drive.md, "Run"): the request lays the workspace again (units.rs `lay`); one read for
/// both is the layout's answer handed here, once the choice can wait for it.
pub(crate) fn pick<'f>(cx: &Cx, root: &Dir<ReadWrite>, select: Select<'_>, frame: &'f Frame) -> Result<Program<'f>, CliError> {
    let cx = cx.span("blaze run: which program");
    let refused = |why: &dyn core::fmt::Display| CliError::Program(RunError::Manifest { why: Box::from(bstd::mem::format_in!(frame, "{why}").as_str()) });
    let kept = |text: &str| -> Result<&'f str, CliError> {
        core::str::from_utf8(Vec::from_slice_in(text.as_bytes(), frame).leak()).map_err(|_| CliError::Text { what: "a manifest is not UTF-8" })
    };
    // Each manifest read (path, text), for the root's `default-members`.
    let mut read: Vec<(&'f str, &'f str), &'f Frame> = Vec::new_in(frame);
    let erased = blaze_own::Erased::new(frame);
    let mut layout = Layout::new("", erased.handle()).map_err(|e| refused(&e))?;
    for _ in 0..MOST_PASSES {
        let needs = match layout.pass().map_err(|e| refused(&e))? {
            Pass::Need(needs) => needs,
            Pass::Done(units) => {
                let root_path = units.manifests.get(units.root.0 as usize).map_or("Cargo.toml", |path| path.as_str());
                let root_text = read.iter().find(|(path, _)| *path == root_path).map_or("", |(_, text)| *text);
                let (member, bin) = choose(&units, root_text, select).map_err(CliError::RunPackage)?;
                let manifest = units.manifests.get(member.manifest.0 as usize).ok_or(CliError::Text { what: "the layout names a manifest it did not read (run_package.rs)" })?;
                let edition = match member.edition.blaze {
                    Some(blaze) => blaze.manifest(),
                    None => kept(bstd::mem::format_in!(frame, "{}", member.edition.syntax).as_str())?,
                };
                return Ok(Program { package: kept(member.name.as_str())?, manifest: kept(manifest.as_str())?, root: kept(bin.root.as_str())?, edition });
            }
        };
        for need in needs.iter() {
            match need {
                Need::Manifest(path) => {
                    let mut bytes = Bytes::new_in(frame, "a manifest");
                    let text = match root.read_all(&cx, &Path::new(path.as_str())?, &mut bytes) {
                        Ok(_) => Some(kept(core::str::from_utf8(bytes.as_slice()).map_err(|_| CliError::Text { what: "a manifest is not UTF-8" })?)?),
                        Err(error) if error.kind == bstd::fs::FsKind::NotFound => None,
                        Err(error) => return Err(error.into()),
                    };
                    if let Some(text) = text {
                        read.push((kept(path.as_str())?, text));
                    }
                    layout.manifest(path.as_str(), text).map_err(|e| refused(&e))?;
                }
                Need::Listing(dir) => {
                    // `Dir::list` gives no entries for a directory that is not there.
                    let listed = match dir.as_str() {
                        "" => root.list(&cx, frame)?,
                        named => root.subdir(&Path::new(named)?).list(&cx, frame)?,
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

    /// The choice `select` makes in a simulated checkout of `files` (path, text), its texts
    /// copied out of the command's frame.
    fn picked(files: &[(&str, &str)], select: Select<'_>) -> Result<(std::string::String, std::string::String, std::string::String), PickError> {
        let io = Io::simulated(SimWorld::new(1, Limits::USUAL));
        let cx = io.cx(BUDGET, TASK_FRAME).expect("a root context");
        let root = io.need_fs().expect("files").open_dir(&cx, &Path::new("data").expect("a path")).expect("open");
        for (path, text) in files {
            root.publish(&cx, &Path::new(path).expect("a path"), Content::bytes(text.as_bytes()), Durable::No).expect("write");
        }
        match pick(&cx, &root, select, cx.frame()) {
            Ok(program) => Ok((program.package.into(), program.manifest.into(), program.root.into())),
            Err(CliError::RunPackage(why)) => Err(why),
            Err(other) => panic!("a refusal that is not the choice's: {other}"),
        }
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
            assert_eq!(picked(files, Select { package: Some("tool"), bin: None }), Ok(triple("tool", "tool/Cargo.toml", "tool/src/main.rs")));
            assert_eq!(picked(files, Select { package: Some("lib"), bin: None }), Err(PickError::NoBinary { package: owned("lib") }));
            assert_eq!(
                picked(files, Select { package: Some("nope"), bin: None }),
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
        assert_eq!(picked(&files, Select { package: None, bin: Some("extra") }), Ok(triple("two", "Cargo.toml", "src/extra.rs")));
        assert_eq!(picked(&files, Select { package: Some("two"), bin: Some("two") }), Ok(triple("two", "Cargo.toml", "src/main.rs")));
        let Err(PickError::WhichBin { package, bins }) = picked(&files, Select::NONE) else { panic!("two binaries and no `--bin`") };
        assert_eq!((package, bins.len()), (owned("two"), 2));
        assert!(matches!(picked(&files, Select { package: Some("two"), bin: Some("nope") }), Err(PickError::NoSuchBin { package: Some(_), .. })));
        virtual_three(THREE, |files| {
            assert_eq!(picked(files, Select { package: None, bin: Some("app") }), Ok(triple("app", "app/Cargo.toml", "app/src/main.rs")));
            assert!(matches!(picked(files, Select { package: None, bin: Some("nope") }), Err(PickError::NoSuchBin { package: None, .. })));
        });
    }

    /// The program's own files are under its crate root's folder.
    #[test]
    fn the_programs_sources_are_its_roots_folder() {
        let program = Program { package: "tempo", manifest: "bin/tempo/Cargo.toml", root: "bin/tempo/src/main.rs", edition: "2024" };
        assert_eq!((program.sources(), program.units()), ("bin/tempo/src/", Name::manifest(b"bin/tempo/Cargo.toml")));
    }
}
