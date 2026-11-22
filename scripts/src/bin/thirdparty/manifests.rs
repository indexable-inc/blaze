//! The manifests' shape (`manifests-check`, `manifests-report`): every member names each
//! dependency through the root's `[workspace.dependencies]`, and the root names every member
//! there once, by its path. A hard check, not a ratchet: the tree holds it today. Read with the
//! same graph as the count (one concept, one home): the members are [`members_of`]'s.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use super::{DEP_TABLES, DepsError, Fixture, Place, ROOT, Tree, expect, get_bool, get_str, get_strs, get_table, line_of, manifest_path, members_of, read_toml, toml};

pub(super) const MANIFESTS_DOC: &str = r#"
    command   manifests-check   every member's manifest names each dependency, in every table
                                (`[dependencies]`, `[dev-dependencies]`, `[build-dependencies]`,
                                each `[target.<cfg>.*]` form; inline, dotted or a sub-table) as
                                `name.workspace = true` or `name = { workspace = true, .. }` with
                                no key but `features`, `optional` and `default-features`; a
                                `path`, `version`, `git`, `branch`, `rev`, `tag`, `registry` or
                                `package` is refused naming the manifest, the table, the entry
                                and the line to write. The root: every directory with a
                                Cargo.toml under crates/ and tools/astlog/, and scripts/, is a
                                member (less `NOT_MEMBERS`, which must stay out), and has
                                exactly one `[workspace.dependencies]` entry `name = { path =
                                "<dir>" }` whose name is its package's; an entry whose path has
                                no manifest, or a second entry for one package, is refused;
                                the table's keys go down in byte order (descents held at
                                `DESCENTS`: only goes down). `[package]` keys the root's
                                `[workspace.package]` defines and `[lints] workspace = true`
                                that a member does not inherit are counted, not refused
              manifests-report  per member: the entries not on `workspace = true`, the keys
                                not inherited
"#;

/// Directories with a Cargo.toml that are not members, and must stay out, each with why.
const NOT_MEMBERS: [(&str, &str); 2] = [
    ("crates/blaze-hvbox", "a Hypervisor.framework sandbox kept out of git and out of the workspace until its owner says (it panicked the Mac's kernel, 2026-11-16)"),
    ("crates/harness", "its own workspace with its own lock: a macOS app and daemon blaze does not build (crates/harness/README.md)"),
];

/// The directories whose every child with a Cargo.toml is a member, and the single members.
const MEMBER_PARENTS: [&str; 2] = ["crates", "tools/astlog"];
const MEMBER_DIRS: [&str; 1] = ["scripts"];

/// The keys an entry may hold beside `workspace = true`.
const ALLOWED: [&str; 4] = ["workspace", "features", "optional", "default-features"];

/// A member's entries that name a package the root cannot carry under the entry's key, each
/// with why: `(member dir, key, why)`. Their form is `key = { package = "..", path = ".." }`.
const RENAMED: [(&str, &str, &str); 2] = [
    (
        "crates/rustc_abi",
        "bitflags",
        "the code says `bitflags::`, and the root's `bitflags` is the third-party package vendor forks resolve; `workspace = true` cannot rename (cargo ignores `package` beside it), so it goes when the code names `blaze_bitflags`",
    ),
    (
        "crates/rustc_lexer",
        "memchr",
        "the code says `memchr::`, and the root's `memchr` is the third-party package vendor forks resolve; it goes when the code names `blaze_memchr`",
    ),
];

/// The descents of `[workspace.dependencies]`'s keys in byte order on 2026-11-20 (none sorted
/// it yet: it grew in the order crates were added). Only goes down; a sort of the table, when
/// no writer is in the root manifest, makes it 0 and then this is 0 for good.
const DESCENTS: usize = 81;

/// The `[package]` keys a member inherits when the root's `[workspace.package]` defines them.
const PACKAGE_KEYS: [&str; 6] = ["edition", "version", "license", "rust-version", "authors", "repository"];

/// What is wrong with a manifest: the file, line, table and entry, and what to write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum ManifestProblem {
    /// A member's entry not on `workspace = true`.
    NotInherited { at: Place, table: String, key: String, keys: Vec<String>, write: String },
    /// An entry on `workspace = true` with a key beside it that cargo does not take there.
    ExtraKey { at: Place, table: String, key: String, extra: String, write: String },
    /// A member's entry with no root entry of its key.
    NoRootEntry { at: Place, key: String },
    /// A directory with a manifest that no `members` pattern covers.
    NotAMember { dir: String },
    /// A directory that must stay out is a member.
    MustStayOut { dir: String, why: &'static str },
    /// A member with no root entry by its path.
    NoEntry { dir: String, package: String },
    /// A root entry whose path has no manifest.
    DeadPath { at: Place, key: String, path: String },
    /// A root entry whose package is not its directory's.
    WrongName { at: Place, key: String, package: String, found: String },
    /// Two root entries for one package.
    TwoEntries { at: Place, package: String, first: usize },
    /// More descents in the root's table than `DESCENTS`.
    Unsorted { descents: usize, first: Place, key: String, after: String },
}

impl fmt::Display for ManifestProblem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ManifestProblem::NotInherited { at, table, key, keys, write } => {
                write!(f, "{at}: [{table}] `{key}` has {}, not `workspace = true`: write `{write}`", keys.join(", "))
            }
            ManifestProblem::ExtraKey { at, table, key, extra, write } => {
                write!(f, "{at}: [{table}] `{key}` has `{extra}` beside `workspace = true` (only features, optional, default-features): write `{write}`")
            }
            ManifestProblem::NoRootEntry { at, key } => write!(f, "{at}: `{key}` is in no entry of {ROOT}'s [workspace.dependencies]: add one there"),
            ManifestProblem::NotAMember { dir } => write!(f, "{dir}/Cargo.toml is no member of {ROOT}'s [workspace] (add it to `members`, or to NOT_MEMBERS in scripts/src/bin/thirdparty/manifests.rs with why)"),
            ManifestProblem::MustStayOut { dir, why } => write!(f, "{dir} is a member of {ROOT}'s [workspace], and must stay out: {why}"),
            ManifestProblem::NoEntry { dir, package } => {
                write!(f, "the member {dir} has no entry in {ROOT}'s [workspace.dependencies]: add `{package} = {{ path = \"{dir}\" }}`")
            }
            ManifestProblem::DeadPath { at, key, path } => write!(f, "{at}: [workspace.dependencies] `{key}`'s path {path:?} has no Cargo.toml"),
            ManifestProblem::WrongName { at, key, package, found } => {
                write!(f, "{at}: [workspace.dependencies] `{key}` names the package `{package}`, and its path's manifest is `{found}`")
            }
            ManifestProblem::TwoEntries { at, package, first } => {
                write!(f, "{at}: a second [workspace.dependencies] entry for the package `{package}` (the first at line {first})")
            }
            ManifestProblem::Unsorted { descents, first, key, after } => write!(
                f,
                "{first}: [workspace.dependencies] has {descents} keys that sort before the one above them, over the {DESCENTS} held (the first new one: `{key}` after `{after}`): put an entry where its name sorts"
            ),
        }
    }
}

/// What the check read: the problems, and per member its counts.
pub(super) struct Shape {
    pub(super) problems: Vec<ManifestProblem>,
    /// Per member's package name: (entries not on `workspace = true`, `[package]` keys and
    /// `[lints]` not inherited).
    pub(super) per_crate: BTreeMap<String, (usize, usize)>,
    pub(super) entries: usize,
    pub(super) root_entries: usize,
    pub(super) descents: usize,
}

/// The line to write for an entry `key` that keeps `features`, `optional` and
/// `default-features`.
fn replacement(key: &str, features: Option<&[String]>, optional: bool, default_features: Option<bool>) -> String {
    let mut parts = vec!["workspace = true".to_owned()];
    if let Some(features) = features {
        let quoted: Vec<String> = features.iter().map(|it| format!("{it:?}")).collect();
        parts.push(format!("features = [{}]", quoted.join(", ")));
    }
    if optional {
        parts.push("optional = true".to_owned());
    }
    if let Some(on) = default_features {
        parts.push(format!("default-features = {on}"));
    }
    match parts.len() {
        1 => format!("{key}.workspace = true"),
        _ => format!("{key} = {{ {} }}", parts.join(", ")),
    }
}

/// The dependency tables of a manifest's `table`, each with its name as written.
fn dep_tables<'d, 't>(file: &str, table: toml::Node<'d, 't>) -> Result<Vec<(String, toml::Node<'d, 't>)>, DepsError> {
    let mut out = Vec::new();
    for (name, _) in DEP_TABLES {
        if let Some(it) = get_table(table, name, file)? {
            out.push((name.to_owned(), it));
        }
    }
    if let Some(targets) = get_table(table, "target", file)? {
        for target in targets.children() {
            for (name, _) in DEP_TABLES {
                if let Some(it) = get_table(target, name, file)? {
                    out.push((format!("target.'{}'.{name}", target.key()), it));
                }
            }
        }
    }
    Ok(out)
}

/// The problems of one member's dependency `value` (key `key`, in `table` of the member `dir`).
fn entry_problems(
    dir: &str,
    file: &str,
    table: &str,
    value: toml::Node<'_, '_>,
    root_keys: &BTreeSet<String>,
    out: &mut Vec<ManifestProblem>,
) -> Result<bool, DepsError> {
    let key = value.key();
    let at = Place { file: file.to_owned(), line: line_of(value) };
    if !value.is_table() {
        let write = replacement(key, None, false, None);
        out.push(ManifestProblem::NotInherited { at, table: table.to_owned(), key: key.to_owned(), keys: vec!["a version".to_owned()], write });
        return Ok(false);
    }
    let features = get_strs(value, "features", file)?.map(|(_, them)| them);
    let optional = get_bool(value, "optional", file)?.unwrap_or(false);
    let default_features = match get_bool(value, "default-features", file)? {
        Some(it) => Some(it),
        None => get_bool(value, "default_features", file)?,
    };
    let write = replacement(key, features.as_deref(), optional, default_features);
    let keys: Vec<String> = value.children().map(|it| it.key().to_owned()).collect();
    if get_bool(value, "workspace", file)? == Some(true) {
        if let Some(extra) = keys.iter().find(|it| !ALLOWED.contains(&it.as_str()) && it.as_str() != "default_features") {
            out.push(ManifestProblem::ExtraKey { at, table: table.to_owned(), key: key.to_owned(), extra: extra.clone(), write });
            return Ok(false);
        }
        if !root_keys.contains(key) {
            out.push(ManifestProblem::NoRootEntry { at, key: key.to_owned() });
            return Ok(false);
        }
        return Ok(true);
    }
    let renamed = RENAMED.iter().any(|(d, k, _)| *d == dir && *k == key);
    if renamed && keys.iter().all(|it| it == "package" || it == "path") {
        return Ok(false);
    }
    let named: Vec<String> = keys.iter().filter(|it| !ALLOWED.contains(&it.as_str())).map(|it| format!("`{it}`")).collect();
    out.push(ManifestProblem::NotInherited { at, table: table.to_owned(), key: key.to_owned(), keys: named, write });
    Ok(false)
}

/// The manifests of `tree` checked: the members' entries, then the root's.
pub(super) fn shape(tree: &dyn Tree) -> Result<Shape, DepsError> {
    let Some(text) = tree.read(ROOT)? else { return Err(DepsError::Missing { path: ROOT.to_owned(), what: "the root manifest".to_owned() }) };
    let doc = read_toml(ROOT, &text)?;
    let root = doc.root();
    let Some(ws) = get_table(root, "workspace", ROOT)? else {
        return Err(DepsError::Shape { at: Place { file: ROOT.to_owned(), line: 1 }, key: "workspace".to_owned(), want: "a table: the root is no workspace" });
    };
    let members = members_of(tree, root, ws)?;
    let mut problems = Vec::new();
    // The root's entries: by key, by path, by package.
    let mut root_keys: BTreeSet<String> = BTreeSet::new();
    let mut by_path: BTreeMap<String, String> = BTreeMap::new();
    let mut by_package: BTreeMap<String, usize> = BTreeMap::new();
    let mut ordered: Vec<(String, usize)> = Vec::new();
    if let Some(deps) = get_table(ws, "dependencies", ROOT)? {
        for value in deps.children() {
            let key = value.key().to_owned();
            let line = line_of(value);
            ordered.push((key.clone(), line));
            root_keys.insert(key.clone());
            let at = Place { file: ROOT.to_owned(), line };
            let package = if value.is_table() { get_str(value, "package", ROOT)?.unwrap_or_else(|| key.clone()) } else { key.clone() };
            if let Some(first) = by_package.get(&package) {
                problems.push(ManifestProblem::TwoEntries { at: at.clone(), package: package.clone(), first: *first });
            } else {
                by_package.insert(package.clone(), line);
            }
            let Some(path) = (if value.is_table() { get_str(value, "path", ROOT)? } else { None }) else { continue };
            let path = path.trim_end_matches('/').to_owned();
            let Some(manifest) = tree.read(&manifest_path(&path))? else {
                problems.push(ManifestProblem::DeadPath { at, key, path });
                continue;
            };
            let found = read_toml(&manifest_path(&path), &manifest)?;
            let name = get_table(found.root(), "package", &manifest_path(&path))?.map(|it| get_str(it, "name", &manifest_path(&path))).transpose()?.flatten();
            match name {
                Some(name) if name == package => {}
                Some(name) => problems.push(ManifestProblem::WrongName { at, key: key.clone(), package: package.clone(), found: name }),
                None => problems.push(ManifestProblem::WrongName { at, key: key.clone(), package: package.clone(), found: "no package".to_owned() }),
            }
            by_path.insert(path, key);
        }
    }
    let descents: Vec<usize> = (1..ordered.len()).filter(|at| ordered[*at].0 < ordered[*at - 1].0).collect();
    if descents.len() > DESCENTS {
        let at = descents[DESCENTS];
        problems.push(ManifestProblem::Unsorted {
            descents: descents.len(),
            first: Place { file: ROOT.to_owned(), line: ordered[at].1 },
            key: ordered[at].0.clone(),
            after: ordered[at - 1].0.clone(),
        });
    }
    // The directories that must be members, and the ones that must stay out.
    let mut dirs: Vec<String> = Vec::new();
    for parent in MEMBER_PARENTS {
        dirs.extend(tree.manifest_dirs(parent)?);
    }
    for dir in MEMBER_DIRS {
        if tree.read(&manifest_path(dir))?.is_some() {
            dirs.push(dir.to_owned());
        }
    }
    for dir in &dirs {
        if let Some((_, why)) = NOT_MEMBERS.iter().find(|(it, _)| *it == dir.as_str()) {
            if members.contains(dir) {
                problems.push(ManifestProblem::MustStayOut { dir: dir.clone(), why: *why });
            }
        } else if !members.contains(dir) {
            problems.push(ManifestProblem::NotAMember { dir: dir.clone() });
        }
    }
    // Each member: its root entry, its entries, its inherited keys.
    let package_keys: Vec<&str> = match get_table(ws, "package", ROOT)? {
        Some(it) => PACKAGE_KEYS.iter().copied().filter(|key| it.get(key).is_some()).collect(),
        None => Vec::new(),
    };
    let mut per_crate = BTreeMap::new();
    let mut entries = 0;
    for dir in &members {
        let file = manifest_path(dir);
        let Some(text) = tree.read(&file)? else { return Err(DepsError::Missing { path: file, what: "a member's manifest".to_owned() }) };
        let doc = read_toml(&file, &text)?;
        let table = doc.root();
        let package = get_table(table, "package", &file)?;
        let name = match package {
            Some(it) => get_str(it, "name", &file)?.unwrap_or_default(),
            None => String::new(),
        };
        if !dir.is_empty() && !by_path.contains_key(dir) {
            problems.push(ManifestProblem::NoEntry { dir: dir.clone(), package: name.clone() });
        }
        let mut off = 0;
        for (table_name, deps) in dep_tables(&file, table)? {
            for value in deps.children() {
                entries += 1;
                if !entry_problems(dir, &file, &table_name, value, &root_keys, &mut problems)? {
                    off += 1;
                }
            }
        }
        let mut not_inherited = 0;
        if let Some(package) = package {
            for key in &package_keys {
                let inherits = package.get(key).is_some_and(|it| it.is_table() && it.get("workspace").and_then(|w| w.as_bool()) == Some(true));
                if !inherits {
                    not_inherited += 1;
                }
            }
        }
        let lints = get_table(table, "lints", &file)?.and_then(|it| it.get("workspace")).and_then(|it| it.as_bool()) == Some(true);
        if !lints {
            not_inherited += 1;
        }
        per_crate.insert(name, (off, not_inherited));
    }
    Ok(Shape { problems, per_crate, entries, root_entries: ordered.len(), descents: descents.len() })
}

/// The report: a row a member, then the totals.
pub(super) fn report(shape: &Shape) -> String {
    let mut out = String::from("crate\tnot-workspace\tnot-inherited\n");
    for (name, (off, not_inherited)) in &shape.per_crate {
        out.push_str(&format!("{name}\t{off}\t{not_inherited}\n"));
    }
    let off: usize = shape.per_crate.values().map(|it| it.0).sum();
    let not: usize = shape.per_crate.values().map(|it| it.1).sum();
    let crates_not = shape.per_crate.values().filter(|it| it.1 > 0).count();
    out.push_str(&format!(
        "(all)\t{off}\t{not}\n{} members, {} dependency entries ({off} not on `workspace = true`; {} renames allowed by name); [workspace.dependencies]: {} entries, {} descents; [package] and [lints] not inherited: {not} keys in {crates_not} members (counted, not refused)\n",
        shape.per_crate.len(),
        shape.entries,
        RENAMED.len(),
        shape.root_entries,
        shape.descents
    ));
    out
}

/// The selftest's cases: each form read, each failure refused.
pub(super) fn selftest() {
    let fixture = |files: &[(&str, &str)]| Fixture { files: files.iter().map(|(path, text)| ((*path).to_owned(), (*text).to_owned())).collect() };
    let read = |files: &[(&str, &str)]| shape(&fixture(files)).unwrap_or_else(|e| blaze_script::fail("thirdparty selftest", &format!("the fixture reads: {e}")));
    let root = "[workspace]\nmembers = [\"crates/*\", \"scripts\"]\nexclude = [\"crates/blaze-hvbox\", \"crates/harness\"]\n\n[workspace.package]\nedition = \"2024\"\n\n[workspace.dependencies]\na = { path = \"crates/a\" }\nb = { path = \"crates/b\" }\nscripts = { path = \"scripts\" }\nserde = \"1\"\n";
    let a = "[package]\nname = \"a\"\nedition.workspace = true\n\n[lints]\nworkspace = true\n\n[dependencies]\nb.workspace = true\nserde = { workspace = true, features = [\"derive\"], optional = true }\n\n[dependencies.x]\nworkspace = true\n\n[target.'cfg(unix)'.dev-dependencies]\nb = { workspace = true, default-features = false }\n";
    let b = "[package]\nname = \"b\"\nedition = \"2021\"\n";
    let scripts = "[package]\nname = \"scripts\"\n[lints]\nworkspace = true\n";
    let hv = "[package]\nname = \"blaze-hvbox\"\n";
    let base: Vec<(&str, &str)> = vec![("Cargo.toml", root), ("crates/a/Cargo.toml", a), ("crates/b/Cargo.toml", b), ("scripts/Cargo.toml", scripts), ("crates/blaze-hvbox/Cargo.toml", hv)];
    let with = |path: &str, text: &str| -> Shape {
        let mut files: Vec<(&str, &str)> = base.iter().copied().filter(|(it, _)| *it != path).collect();
        files.push((path, text));
        read(&files)
    };
    let first = |shape: &Shape| shape.problems.first().cloned();
    // `x` has no root entry; with one, the three forms and a target table hold.
    let missing_x = with("Cargo.toml", root);
    expect(matches!(first(&missing_x), Some(ManifestProblem::NoRootEntry { ref key, .. }) if key == "x"), "an entry with no root entry is refused");
    let root_x = root.replace("serde = \"1\"\n", "serde = \"1\"\nx = { path = \"crates/x\" }\n");
    let mut files: Vec<(&str, &str)> = base.iter().copied().filter(|(it, _)| *it != "Cargo.toml").collect();
    files.push(("Cargo.toml", root_x.as_str()));
    files.push(("crates/x/Cargo.toml", "[package]\nname = \"x\"\n"));
    let good = read(&files);
    expect(good.problems.is_empty(), "inline, dotted, sub-table and target forms on `workspace = true` hold");
    expect(good.per_crate.get("b") == Some(&(0, 2)) && good.per_crate.get("a") == Some(&(0, 0)), "a member not inheriting `edition` nor `[lints]` is counted twice, not refused");
    // A path entry: refused with the line to write, features and default-features kept.
    let path = with("crates/a/Cargo.toml", &a.replace("b.workspace = true", "b = { path = \"../b\", features = [\"f\"], default-features = false }"));
    expect(
        path.problems.iter().any(|it| matches!(it, ManifestProblem::NotInherited { at, table, key, write, .. }
            if at.line == 9 && table == "dependencies" && key == "b" && write == "b = { workspace = true, features = [\"f\"], default-features = false }")),
        "a path entry is refused with its replacement",
    );
    let version = with("crates/a/Cargo.toml", &a.replace("b.workspace = true", "b = \"1.0\""));
    expect(version.problems.iter().any(|it| matches!(it, ManifestProblem::NotInherited { write, .. } if write == "b.workspace = true")), "a version string is refused");
    for key in ["git = \"u\"", "branch = \"m\"", "rev = \"r\"", "tag = \"t\"", "registry = \"r\"", "version = \"1\""] {
        let one = with("crates/a/Cargo.toml", &a.replace("b.workspace = true", &format!("b = {{ {key} }}")));
        expect(one.problems.iter().any(|it| matches!(it, ManifestProblem::NotInherited { key, .. } if key == "b")), "each source key is refused");
    }
    let extra = with("crates/a/Cargo.toml", &a.replace("b.workspace = true", "b = { workspace = true, package = \"c\" }"));
    expect(extra.problems.iter().any(|it| matches!(it, ManifestProblem::ExtraKey { extra, .. } if extra == "package")), "`package` beside `workspace = true` is refused");
    let target = with("crates/a/Cargo.toml", &a.replace("b = { workspace = true, default-features = false }", "b = { path = \"../b\" }"));
    expect(target.problems.iter().any(|it| matches!(it, ManifestProblem::NotInherited { table, .. } if table == "target.'cfg(unix)'.dev-dependencies")), "a target table is read");
    // The root: a member with no entry, a dead path, a wrong name, two entries, hvbox in.
    let no_entry = with("Cargo.toml", &root.replace("b = { path = \"crates/b\" }\n", ""));
    expect(no_entry.problems.iter().any(|it| matches!(it, ManifestProblem::NoEntry { dir, .. } if dir == "crates/b")), "a member with no root entry is refused");
    let dead = with("Cargo.toml", &root.replace("serde = \"1\"\n", "serde = \"1\"\nz = { path = \"crates/z\" }\n"));
    expect(dead.problems.iter().any(|it| matches!(it, ManifestProblem::DeadPath { key, .. } if key == "z")), "an entry whose path has no manifest is refused");
    let wrong = with("Cargo.toml", &root.replace("b = { path = \"crates/b\" }", "bee = { path = \"crates/b\" }"));
    expect(wrong.problems.iter().any(|it| matches!(it, ManifestProblem::WrongName { key, .. } if key == "bee")), "an entry not named as its package is refused");
    let two = with("Cargo.toml", &root.replace("serde = \"1\"\n", "serde = \"1\"\nb2 = { package = \"b\", path = \"crates/b\" }\n"));
    expect(two.problems.iter().any(|it| matches!(it, ManifestProblem::TwoEntries { package, .. } if package == "b")), "two entries for one package are refused");
    let hv_in = with("Cargo.toml", &root.replace("exclude = [\"crates/blaze-hvbox\", \"crates/harness\"]", "exclude = []"));
    expect(hv_in.problems.iter().any(|it| matches!(it, ManifestProblem::MustStayOut { dir, .. } if dir == "crates/blaze-hvbox")), "hvbox in the workspace is refused");
    let not_member = with("Cargo.toml", &root.replace("\"crates/*\", ", "\"crates/a\", \"crates/b\", "));
    expect(!not_member.problems.iter().any(|it| matches!(it, ManifestProblem::NotAMember { .. })), "members by name cover their directories");
    let tools = with("tools/astlog/t/Cargo.toml", "[package]\nname = \"t\"\n");
    expect(tools.problems.iter().any(|it| matches!(it, ManifestProblem::NotAMember { dir } if dir == "tools/astlog/t")), "a tools/astlog crate outside `members` is refused");
    expect(replacement("k", None, true, None) == "k = { workspace = true, optional = true }", "optional is kept");
}
