//! The manifests' parts the walk needs, read from TEXT into plain values (rule 14): the root's
//! members, exclusions, `[workspace.dependencies]` and edition; a crate's name, its producer
//! rows and its dependencies. Every form not handled is refused by name (rule 3).

use blaze_ids::{ContentId, Hasher, producer::Row};
use blaze_own::{Alloc, Vec, VecDeque};
use blaze_toml::{Document, Item, Kind, Limits};

use crate::{Refused, Text, text};

/// A dependency as a crate's manifest names it.
pub(crate) enum Dep<'a> {
    /// `path = ".."`: the crate's directory, relative to the workspace root.
    Path(Text<'a>),
    /// `x.workspace = true`: the root's `[workspace.dependencies]` entry `x`.
    Workspace(Text<'a>),
    /// A crate from outside the workspace (registry or git), by its package name.
    Outside(Text<'a>),
}

/// Where a `[workspace.dependencies]` entry points.
pub(crate) enum Target<'a> {
    Path(Text<'a>),
    Outside(Text<'a>),
}

/// One `[workspace.dependencies]` entry: its key, a digest of its whole value (every key and
/// scalar under it, so a header-form `[workspace.dependencies.x]` table counts whole), and where
/// it points.
pub(crate) struct WsDep<'a> {
    pub key: Text<'a>,
    pub digest: ContentId,
    pub target: Target<'a>,
}

/// The root manifest's parts.
pub(crate) struct Root<'a> {
    /// `[workspace] members`, each a directory or `<dir>/*`.
    pub members: Vec<Text<'a>, Alloc<'a>>,
    pub exclude: Vec<Text<'a>, Alloc<'a>>,
    /// Sorted by key.
    pub deps: Vec<WsDep<'a>, Alloc<'a>>,
    /// A digest of `[workspace.package] edition` (what `edition.workspace = true` reads), if any.
    pub edition: Option<ContentId>,
    /// Each `[patch.<source>] name = { path = ".." }` directory, sorted, each once: a lock row
    /// with no source may be one of them (vendor/quinn-proto).
    pub patches: Vec<Text<'a>, Alloc<'a>>,
}

/// A crate's manifest's parts.
pub(crate) struct Member<'a> {
    pub name: Text<'a>,
    /// `[package.metadata.blaze] producer`: a bit per [`Row::index`].
    pub producer: u8,
    /// Each dependency, and whether it is `optional = true`.
    pub deps: Vec<(Dep<'a>, bool), Alloc<'a>>,
}

const VALUE_DOMAIN: &str = "blaze producers manifest value 1";

/// The dependency tables a crate's build reads: normal and build, also under `[target.*]`.
/// `dev-dependencies` are not in the binary, so not read. `build_dependencies` is cargo's older
/// spelling, still accepted.
const DEP_TABLES: [&str; 3] = ["dependencies", "build-dependencies", "build_dependencies"];

fn parse<'t, 'a>(path: &str, source: &'t str, alloc: Alloc<'a>) -> Result<Document<'t, Alloc<'a>>, Refused<'a>> {
    Document::parse_in(source, Limits::CARGO, alloc).map_err(|error| Refused::Toml { path: text(path, alloc), error })
}

fn strings<'a>(
    path: &str,
    item: Option<Item<'_, '_, Alloc<'a>>>,
    what: &'static str,
    alloc: Alloc<'a>,
) -> Result<Vec<Text<'a>, Alloc<'a>>, Refused<'a>> {
    let mut out = Vec::new_in(alloc);
    let Some(item) = item else { return Ok(out) };
    if item.kind() != Kind::Array {
        return Err(Refused::Shape { path: text(path, alloc), what });
    }
    for element in item.children() {
        let Some(s) = element.as_str() else { return Err(Refused::Shape { path: text(path, alloc), what }) };
        out.push(text(s, alloc));
    }
    Ok(out)
}

/// The root manifest at `Cargo.toml`.
pub(crate) fn root<'a>(source: &str, alloc: Alloc<'a>) -> Result<Root<'a>, Refused<'a>> {
    const PATH: &str = "Cargo.toml";
    let doc = parse(PATH, source, alloc)?;
    let Some(workspace) = doc.root().get("workspace") else {
        return Err(Refused::Shape { path: text(PATH, alloc), what: "a root manifest with no `[workspace]`" });
    };
    let members = strings(PATH, workspace.get("members"), "`workspace.members` as an array of strings", alloc)?;
    let exclude = strings(PATH, workspace.get("exclude"), "`workspace.exclude` as an array of strings", alloc)?;
    let edition = workspace
        .get("package")
        .and_then(|package| package.get("edition"))
        .map(|item| value_digest(source, item, alloc));
    let mut deps = Vec::new_in(alloc);
    if let Some(table) = workspace.get("dependencies") {
        for entry in table.children() {
            let key = entry.key();
            let target = match entry.kind() {
                Kind::String => Target::Outside(text(key, alloc)),
                Kind::Table => match entry.get("path").and_then(|p| p.as_str()) {
                    Some(dir) => Target::Path(join("", dir, PATH, key, alloc)?),
                    None => Target::Outside(text(entry.get("package").and_then(|p| p.as_str()).unwrap_or(key), alloc)),
                },
                _ => {
                    return Err(Refused::Shape {
                        path: text(PATH, alloc),
                        what: "a `[workspace.dependencies]` entry that is neither a version string nor a table",
                    });
                }
            };
            deps.push(WsDep { key: text(key, alloc), digest: value_digest(source, entry, alloc), target });
        }
    }
    deps.sort_unstable_by(|a, b| a.key.as_str().cmp(b.key.as_str()));
    // A patch by git or version keeps a lock row with a source: an outside crate as any other.
    let mut patches = Vec::new_in(alloc);
    if let Some(patch) = doc.root().get("patch") {
        for source in patch.children() {
            for entry in source.children() {
                if let Some(dir) = entry.get("path").and_then(|p| p.as_str()) {
                    patches.push(join("", dir, PATH, entry.key(), alloc)?);
                }
            }
        }
    }
    patches.sort_unstable_by(|a, b| a.as_str().cmp(b.as_str()));
    patches.dedup();
    Ok(Root { members, exclude, deps, edition, patches })
}

/// The manifest of the crate at `dir` (relative to the root), read from `path`.
pub(crate) fn member<'a>(dir: &str, path: &str, source: &str, alloc: Alloc<'a>) -> Result<Member<'a>, Refused<'a>> {
    let doc = parse(path, source, alloc)?;
    let root = doc.root();
    let Some(name) = root.get("package").and_then(|p| p.get("name")).and_then(|n| n.as_str()) else {
        return Err(Refused::Shape { path: text(path, alloc), what: "a crate manifest with no `[package] name` string" });
    };
    let blaze = root.get("package").and_then(|p| p.get("metadata")).and_then(|m| m.get("blaze"));
    let producer = rows(path, blaze.and_then(|b| b.get("producer")), alloc)?;
    // A crate's own files without its closure cover less than the code its producing function
    // runs (rule 21): the list is refused by name, never read as a narrower row.
    if blaze.and_then(|b| b.get("producer-own")).is_some() {
        return Err(Refused::OwnFilesOnly { path: text(path, alloc) });
    }
    let mut deps = Vec::new_in(alloc);
    let mut tables = Vec::new_in(alloc);
    for name in DEP_TABLES {
        if let Some(table) = root.get(name) {
            tables.push(table);
        }
    }
    // Every target's tables, whatever its `cfg`: a dependency of another host's build is a
    // needless miss here, never a stale answer.
    if let Some(targets) = root.get("target") {
        for target in targets.children() {
            for name in DEP_TABLES {
                if let Some(table) = target.get(name) {
                    tables.push(table);
                }
            }
        }
    }
    for table in tables.iter() {
        if table.kind() != Kind::Table {
            return Err(Refused::Shape { path: text(path, alloc), what: "a dependency table that is not a table" });
        }
        for entry in table.children() {
            let key = entry.key();
            let dep = match entry.kind() {
                Kind::String => Dep::Outside(text(key, alloc)),
                Kind::Table => {
                    if entry.get("workspace").and_then(|w| w.as_bool()) == Some(true) {
                        Dep::Workspace(text(key, alloc))
                    } else if let Some(rel) = entry.get("path").and_then(|p| p.as_str()) {
                        Dep::Path(join(dir, rel, path, key, alloc)?)
                    } else {
                        Dep::Outside(text(entry.get("package").and_then(|p| p.as_str()).unwrap_or(key), alloc))
                    }
                }
                _ => {
                    return Err(Refused::Shape {
                        path: text(path, alloc),
                        what: "a dependency that is neither a version string nor a table",
                    });
                }
            };
            // An optional dependency no feature turns on is in no lock: the plan tells it apart.
            let optional = entry.kind() == Kind::Table && entry.get("optional").and_then(|o| o.as_bool()) == Some(true);
            deps.push((dep, optional));
        }
    }
    Ok(Member { name: text(name, alloc), producer, deps })
}

/// A `producer` list as row bits; a name that is no row, or `whole` (which
/// is the build's, never a source's), is refused by name.
fn rows<'a>(path: &str, item: Option<Item<'_, '_, Alloc<'a>>>, alloc: Alloc<'a>) -> Result<u8, Refused<'a>> {
    let mut bits = 0u8;
    for name in strings(path, item, "`[package.metadata.blaze] producer` as an array of strings", alloc)?.iter() {
        match Row::named(name.as_str()) {
            Some(row) if row != Row::Whole => bits |= 1 << row.index(),
            _ => return Err(Refused::UnknownRow { path: text(path, alloc), name: text(name, alloc) }),
        }
    }
    Ok(bits)
}

/// `rel` (a manifest's `path =`) from the directory `base`, both relative to the workspace root,
/// with `.` and `..` resolved: an absolute path, or one that leaves the workspace, is refused
/// (a path of the machine in a digest breaks rule 25).
pub(crate) fn join<'a>(base: &str, rel: &str, manifest: &str, dep: &str, alloc: Alloc<'a>) -> Result<Text<'a>, Refused<'a>> {
    let escapes = || Refused::Escapes { path: text(manifest, alloc), dep: text(dep, alloc) };
    if rel.starts_with('/') {
        return Err(escapes());
    }
    let mut parts: Vec<&str, Alloc<'a>> = Vec::new_in(alloc);
    for part in base.split('/').chain(rel.split('/')) {
        match part {
            "" | "." => {}
            ".." => {
                if parts.pop().is_none() {
                    return Err(escapes());
                }
            }
            part => parts.push(part),
        }
    }
    let mut out = Text::new_in(alloc);
    for (at, part) in parts.iter().enumerate() {
        if at > 0 {
            out.push_str("/");
        }
        out.push_str(part);
    }
    Ok(out)
}

/// A digest of an item's whole value: each node's kind, key, and (a container) its count of
/// children or (a scalar) its text as written, breadth first by a work list (no recursion). The
/// text as written, not the decoded value: a change of spelling is a (needless) miss, never a
/// stale answer.
fn value_digest<'a>(source: &str, item: Item<'_, '_, Alloc<'a>>, alloc: Alloc<'a>) -> ContentId {
    let mut hasher = Hasher::new(VALUE_DOMAIN);
    let mut work = VecDeque::new_in(alloc);
    work.push_back(item);
    while let Some(item) = work.pop_front() {
        hasher.part(item.kind().name().as_bytes()).part(item.key().as_bytes());
        match item.kind() {
            Kind::Table | Kind::Array => {
                hasher.part(&(item.len() as u64).to_le_bytes());
                for child in item.children() {
                    work.push_back(child);
                }
            }
            _ => {
                hasher.part(source.as_bytes().get(item.span()).unwrap_or(&[]));
            }
        }
    }
    hasher.finish()
}
