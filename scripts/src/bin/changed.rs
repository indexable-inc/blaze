//! changed: the packages a change touches and the packages that depend on them, in the order to
//! check them. The port of `harness/v2/changed.py`, deleted once both were run on the same tree
//! and agreed (docs/v2/plan/design/scripts.md, "Ports and their parity"). `DOC` below is what its
//! usage error prints.
//!
//!     blaze run -q -p scripts --bin changed -- '{"base": "origin/main"}'
//!     blaze run -q -p scripts --bin changed -- selftest
//!
//! Where this differed from the Python: a manifest that is not TOML is refused in the toml
//! crate's words, not tomllib's.

/// What the usage error prints: harness/v2/changed.py's docstring, with this script's commands.
const DOC: &str = r#"changed.py: the packages a change touches and the packages that depend on them, in the order
to check them: what `blaze check -p ...` takes to check a change in minutes, not the workspace
in an hour.

    blaze run -q -p scripts --bin changed -- '{"base": "origin/main"}'
    blaze run -q -p scripts --bin changed -- '{"base": "HEAD~3", "print": "args", "dependents": false}'
    blaze run -q -p scripts --bin changed -- selftest

The rule it serves: code is checked as it is written, a commit at a time, on the packages it
touched (docs/contributing.md, "Checking a change fast"); thousands of lines nobody compiled are
found out by an hour-long build, one error at a time (2026-11-10: a shadowed binding, then a
struct named like its crate).

The input is one JSON object:

    base         the ref the change is measured from: every file that differs between its merge
                 base with HEAD and the working tree (committed or not, untracked too) is the change
    direct       measure from `base` itself, not from its merge base with HEAD: for a shallow
                 checkout, which has no merge base to find (false)
    root         the checkout (".")
    dependents   also the workspace packages that depend on a changed one, through any number of
                 normal, build or dev dependencies (true)
    print        `json` (the default): one object, `{"changed", "dependents", "order", "all",
                 "why_all", "args", "elsewhere"}`; `packages`: `order`, a name a line; `args`:
                 `-p <name>` for each of `order` on one line, or `--workspace` when `all`

`changed` is the packages a changed file is in (the member directory that holds it), `dependents`
those that depend on one, and `order` both with every package after the packages it depends on
(normal and build dependencies; a dev-dependency orders nothing, as cargo lets those cycle), so
the first error is the earliest cause. A file that every package's build reads (`EVERYTHING`: the
root manifest, the lock file, the lints' and the formatter's configuration, the pinned toolchain)
makes it all of them: `all` is true and `why_all` names the file. `elsewhere` counts the changed
files in no package (docs, harness scripts, workflows).

A `print` of `args` that would name no package prints nothing and exits 0: there is nothing to
check. A base that is no ref, or a checkout that is no workspace, stops naming it.
"#;

use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};

use blaze_script::{
    TempDir, fail,
    input::{self, Fields, Input, InputError},
    os_error,
    pyfmt::{repr_str, repr_strs},
    pyjson::dumps_indent,
    pypath::{glob, join, normpath, relpath},
    run,
    toml::{Table, Value as Toml},
};
use serde_json::json;

const SCRIPT: &str = "changed.py";
/// What every package's check reads besides its own files.
const EVERYTHING: [&str; 6] = ["Cargo.toml", "Cargo.lock", ".clippy.toml", ".rustfmt.toml", "harness/toolchain", ".cargo/"];
const TABLES: [&str; 3] = ["dependencies", "build-dependencies", "dev-dependencies"];

struct Args {
    base: String,
    direct: bool,
    root: String,
    dependents: bool,
    print: String,
}

impl Input for Args {
    const FIELDS: &'static [&'static str] = &["base", "direct", "root", "dependents", "print"];

    fn parse(f: &Fields<'_>) -> Result<Args, InputError> {
        Ok(Args {
            base: f.str("base", None)?,
            direct: f.bool("direct", Some(false))?,
            root: f.str("root", Some("."))?,
            dependents: f.bool("dependents", Some(true))?,
            print: f.one_of("print", &["json", "packages", "args"], Some("json"))?,
        })
    }
}

/// A workspace member: its name, its directory from the root (`/` between parts), and the
/// members it depends on, the dev-dependencies apart.
#[derive(Debug, Clone)]
struct Package {
    name: String,
    dir: String,
    needs: BTreeSet<String>,
    dev_needs: BTreeSet<String>,
}

/// Every dependency table of a manifest with its kind: its own and each `[target.*]`'s.
fn tables(manifest: &Table) -> Vec<(&'static str, &Table)> {
    let mut holders = vec![manifest];
    if let Some(Toml::Table(targets)) = manifest.get("target") {
        holders.extend(targets.values().filter_map(Toml::as_table));
    }
    let mut out = Vec::new();
    for holder in holders {
        for kind in TABLES {
            if let Some(Toml::Table(table)) = holder.get(kind) {
                out.push((kind, table));
            }
        }
    }
    out
}

fn read_toml(path: &Path) -> Result<Table, String> {
    let shown = path.display().to_string();
    let text = std::fs::read_to_string(path).map_err(|e| os_error(&e, &shown))?;
    blaze_script::toml::from_str::<Table>(&text).map_err(|e| e.to_string().trim_end().to_owned())
}

fn strs(value: Option<&Toml>) -> Vec<&str> {
    value.and_then(Toml::as_array).map(|items| items.iter().filter_map(Toml::as_str).collect()).unwrap_or_default()
}

/// The member the dependency `key = spec` of the member in `directory` is, if it is one.
fn member<'f>(
    found: &'f [(String, (String, Table))],
    shared: Option<&Table>,
    directory: &str,
    key: &str,
    spec: Option<&Toml>,
) -> Option<&'f str> {
    let spec = spec?.as_table()?;
    if spec.get("workspace").and_then(Toml::as_bool) == Some(true) {
        return member(found, shared, "", key, shared.and_then(|it| it.get(key)));
    }
    let path = spec.get("path")?.as_str()?;
    let rel = normpath(&join(directory, path));
    found.iter().find(|(dir, _)| *dir == rel).map(|(_, (name, _))| name.as_str())
}

/// The workspace's members (the root manifest's `members` less its `exclude`), each with the
/// members it depends on: by a `path`, or by `workspace = true` through the root's
/// `[workspace.dependencies]` entry of that name.
fn workspace(root: &Path) -> Result<Vec<Package>, String> {
    let top = read_toml(&root.join("Cargo.toml"))?;
    let ws = match top.get("workspace") {
        Some(Toml::Table(ws)) if !ws.is_empty() => ws,
        _ => return Err(format!("{} has no [workspace]", root.join("Cargo.toml").display())),
    };
    let root_s = root.to_string_lossy().into_owned();
    let excluded: BTreeSet<String> = strs(ws.get("exclude")).into_iter().map(normpath).collect();
    // A dict of Python's: the first insertion of a directory fixes its place.
    let mut found: Vec<(String, (String, Table))> = Vec::new();
    for pattern in strs(ws.get("members")) {
        let mut paths = glob(&join(&root_s, pattern));
        paths.sort();
        for path in paths {
            let rel = normpath(&relpath(&path, &root_s));
            let manifest = Path::new(&path).join("Cargo.toml");
            if excluded.contains(&rel) || !manifest.is_file() {
                continue;
            }
            let toml = read_toml(&manifest)?;
            let name = toml.get("package").and_then(|it| it.get("name")).and_then(Toml::as_str).map(str::to_owned);
            if let Some(name) = name {
                match found.iter_mut().find(|(dir, _)| *dir == rel) {
                    Some(slot) => slot.1 = (name, toml),
                    None => found.push((rel, (name, toml))),
                }
            }
        }
    }
    let shared = ws.get("dependencies").and_then(Toml::as_table);
    let mut out: Vec<Package> = Vec::new();
    for (directory, (name, toml)) in &found {
        let (mut needs, mut dev) = (BTreeSet::new(), BTreeSet::new());
        for (kind, table) in tables(toml) {
            for (key, spec) in table {
                if let Some(it) = member(&found, shared, directory, key, Some(spec))
                    && it != name
                {
                    let into = if kind == "dev-dependencies" { &mut dev } else { &mut needs };
                    into.insert(it.to_owned());
                }
            }
        }
        let dev_needs = dev.difference(&needs).cloned().collect();
        out.push(Package { name: name.clone(), dir: directory.clone(), needs, dev_needs });
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

/// The package whose directory holds `file` (the deepest, where one is inside another).
fn owner<'p>(packages: &'p [Package], file: &str) -> Option<&'p str> {
    let mut best: Option<&Package> = None;
    for it in packages {
        if file.starts_with(&format!("{}/", it.dir)) && best.is_none_or(|b| it.dir.len() > b.dir.len()) {
            best = Some(it);
        }
    }
    best.map(|it| it.name.as_str())
}

fn everything(file: &str) -> bool {
    EVERYTHING.iter().any(|it| if it.ends_with('/') { file.starts_with(it) } else { file == *it })
}

/// `chosen` with each package after those of `chosen` it needs (dev-dependencies order
/// nothing); among packages free to go, by name. A cycle of normal dependencies is refused.
fn ordered(packages: &[Package], chosen: &BTreeSet<String>) -> Result<Vec<String>, String> {
    let by_name: BTreeMap<&str, &Package> = packages.iter().map(|it| (it.name.as_str(), it)).collect();
    let mut left: BTreeMap<&str, BTreeSet<&str>> = chosen
        .iter()
        .map(|name| {
            let needs = by_name[name.as_str()].needs.iter().filter(|it| chosen.contains(*it));
            (name.as_str(), needs.map(String::as_str).collect())
        })
        .collect();
    let mut out = Vec::new();
    while !left.is_empty() {
        let free: Vec<&str> = left.iter().filter(|(_, needs)| needs.is_empty()).map(|(name, _)| *name).collect();
        if free.is_empty() {
            let names: Vec<&str> = left.keys().copied().collect();
            return Err(format!("the packages {} depend on each other in a cycle", names.join(", ")));
        }
        for name in &free {
            left.remove(name);
            out.push((*name).to_owned());
        }
        for needs in left.values_mut() {
            for name in &free {
                needs.remove(name);
            }
        }
    }
    Ok(out)
}

#[derive(Debug, Clone)]
struct Found {
    changed: Vec<String>,
    dependents: Vec<String>,
    order: Vec<String>,
    all: bool,
    why_all: String,
    elsewhere: usize,
}

impl Found {
    fn args(&self) -> Vec<String> {
        if self.all {
            vec!["--workspace".to_owned()]
        } else {
            self.order.iter().flat_map(|name| ["-p".to_owned(), name.clone()]).collect()
        }
    }

    /// The dataclass's `repr`, as the selftest's messages print it.
    fn repr(&self) -> String {
        format!(
            "Found(changed={}, dependents={}, order={}, all={}, why_all={}, elsewhere={})",
            repr_strs(&self.changed),
            repr_strs(&self.dependents),
            repr_strs(&self.order),
            if self.all { "True" } else { "False" },
            repr_str(&self.why_all),
            self.elsewhere
        )
    }
}

/// What the change of `files` (paths from the root, `/` between parts) asks to be checked.
fn affected(packages: &[Package], files: &[&str], dependents: bool) -> Result<Found, String> {
    let names: BTreeSet<String> = packages.iter().map(|it| it.name.clone()).collect();
    let whole = files.iter().find(|file| everything(file)).copied().unwrap_or("");
    let owners: Vec<Option<&str>> = files.iter().map(|file| owner(packages, file)).collect();
    let changed: BTreeSet<String> =
        if whole.is_empty() { owners.iter().flatten().map(|it| (*it).to_owned()).collect() } else { names };
    let mut reached = changed.clone();
    if dependents {
        loop {
            let more: Vec<String> = packages
                .iter()
                .filter(|it| {
                    !reached.contains(&it.name) && it.needs.iter().chain(&it.dev_needs).any(|n| reached.contains(n))
                })
                .map(|it| it.name.clone())
                .collect();
            if more.is_empty() {
                break;
            }
            reached.extend(more);
        }
    }
    let order = ordered(packages, &reached)?;
    Ok(Found {
        changed: changed.iter().cloned().collect(),
        dependents: reached.difference(&changed).cloned().collect(),
        order,
        all: !whole.is_empty(),
        why_all: whole.to_owned(),
        elsewhere: owners.iter().zip(files).filter(|(it, file)| it.is_none() && !everything(file)).count(),
    })
}

fn git(root: &Path, args: &[&str]) -> Result<String, String> {
    let shown = root.display().to_string();
    let mut argv = vec!["git", "-C", shown.as_str()];
    argv.extend_from_slice(args);
    let ran = run(&argv, root)?;
    if ran.status != 0 {
        return Err(format!("`git {}` in {shown} exited {}: {}", args.join(" "), ran.status, ran.err().trim()));
    }
    Ok(ran.out())
}

/// Every file that differs between the merge base of `base` and HEAD (with `direct`, `base`
/// itself) and the working tree, the untracked ones too.
fn changed_files(root: &Path, base: &str, direct: bool) -> Result<Vec<String>, String> {
    let fork = if direct {
        git(root, &["rev-parse", "--verify", &format!("{base}^{{commit}}")])?
    } else {
        git(root, &["merge-base", base, "HEAD"])?
    };
    let differing = git(root, &["diff", "--name-only", "-z", fork.trim()])?;
    let untracked = git(root, &["ls-files", "--others", "--exclude-standard", "-z"])?;
    let all: BTreeSet<&str> = differing.split('\0').chain(untracked.split('\0')).filter(|it| !it.is_empty()).collect();
    Ok(all.into_iter().map(str::to_owned).collect())
}

fn main_with(args: &Args) {
    // `Path(root).resolve()`: a root that is not there is found out when it is read.
    let root: PathBuf = std::fs::canonicalize(&args.root)
        .unwrap_or_else(|_| std::env::current_dir().unwrap_or_default().join(&args.root));
    let found = workspace(&root).and_then(|packages| {
        let files = changed_files(&root, &args.base, args.direct)?;
        let files: Vec<&str> = files.iter().map(String::as_str).collect();
        affected(&packages, &files, args.dependents)
    });
    let found = found.unwrap_or_else(|e| fail(SCRIPT, &e));
    match args.print.as_str() {
        "packages" => {
            for name in &found.order {
                println!("{name}");
            }
        }
        "args" => {
            if !found.args().is_empty() {
                println!("{}", found.args().join(" "));
            }
        }
        _ => {
            let value = json!({
                "changed": &found.changed,
                "dependents": &found.dependents,
                "order": &found.order,
                "all": found.all,
                "why_all": &found.why_all,
                "args": found.args(),
                "elsewhere": found.elsewhere,
            });
            println!("{}", dumps_indent(&value, 1));
        }
    }
}

// The self-test: a workspace written to a temporary directory; no git, no blaze.

fn expect(holds: bool, what: &str) {
    blaze_script::expect(SCRIPT, holds, what);
}

fn affected_or_fail(packages: &[Package], files: &[&str], dependents: bool) -> Found {
    affected(packages, files, dependents).unwrap_or_else(|e| fail(SCRIPT, &e))
}

fn selftest() {
    let manifests = [
        (
            "Cargo.toml",
            "[workspace]\nmembers = [\"crates/*\", \"tools/lint/cli\"]\nexclude = [\"crates/app-shell\"]\n\
             [workspace.dependencies]\nids = { path = \"crates/ids\" }\nhash = { path = \"crates/hash\" }\n\
             serde = \"1\"\nlinker = { path = \"crates/link\", package = \"link\" }\n",
        ),
        ("crates/hash/Cargo.toml", "[package]\nname = \"hash\"\n[dev-dependencies]\ncargo-front = { path = \"../cargo\" }\n"),
        ("crates/ids/Cargo.toml", "[package]\nname = \"ids\"\n[dependencies]\nhash.workspace = true\nserde.workspace = true\n"),
        ("crates/link/Cargo.toml", "[package]\nname = \"link\"\n[dependencies]\nids = { workspace = true }\n"),
        ("crates/codegen/Cargo.toml", "[package]\nname = \"codegen\"\n[build-dependencies]\nids.workspace = true\n"),
        (
            "crates/cargo/Cargo.toml",
            "[package]\nname = \"cargo-front\"\n[dependencies]\nlinker.workspace = true\n\
             [target.'cfg(unix)'.dependencies]\ncodegen = { path = \"../codegen\" }\n",
        ),
        ("crates/lone/Cargo.toml", "[package]\nname = \"lone\"\n"),
        ("crates/app-shell/Cargo.toml", "[package]\nname = \"app-shell\"\n[dependencies]\nids = { path = \"../ids\" }\n"),
        (
            "tools/lint/cli/Cargo.toml",
            "[package]\nname = \"lint-cli\"\n[dependencies]\nhash = { path = \"../../../crates/hash\" }\n",
        ),
    ];
    let packages = {
        let tmp = TempDir::new("bz-changed-").unwrap_or_else(|e| fail(SCRIPT, &e));
        let root = tmp.path();
        for (relative, text) in manifests {
            let path = root.join(relative);
            let written = path
                .parent()
                .map_or(Ok(()), std::fs::create_dir_all)
                .and_then(|()| std::fs::write(&path, text));
            written.unwrap_or_else(|e| fail(SCRIPT, &os_error(&e, &path.display().to_string())));
        }
        workspace(root).unwrap_or_else(|e| fail(SCRIPT, &e))
    };
    let by_name: BTreeMap<&str, &Package> = packages.iter().map(|it| (it.name.as_str(), it)).collect();
    let names: Vec<&str> = by_name.keys().copied().collect();
    expect(
        names == ["cargo-front", "codegen", "hash", "ids", "link", "lint-cli", "lone"],
        &format!("the members, the excluded one left out: {}", repr_strs(&names)),
    );
    let needs = |name: &str| by_name[name].needs.iter().cloned().collect::<Vec<String>>();
    expect(
        needs("ids") == ["hash"] && needs("link") == ["ids"] && needs("codegen") == ["ids"],
        "a `workspace = true` dependency is the member the root names, and a registry crate is none",
    );
    expect(needs("cargo-front") == ["codegen", "link"], "a renamed dependency and a target's are followed");
    expect(
        by_name["hash"].dev_needs.iter().eq(["cargo-front"]) && needs("hash").is_empty(),
        "a dev-dependency is kept apart",
    );
    expect(needs("lint-cli") == ["hash"], "a path out of the member's directory is followed");

    // The finding in miniature: a type moved into `ids` (a struct named like its crate) and its
    // two users edited; all three and everything over them are checked, `ids` before its users
    // (`hash`, which `ids` needs, depends on a changed package only to test itself, and goes first).
    let found = affected_or_fail(
        &packages,
        &["crates/ids/src/keyed.rs", "crates/link/src/lib.rs", "crates/codegen/src/delta.rs", "docs/lints.md"],
        true,
    );
    expect(
        found.changed == ["codegen", "ids", "link"] && found.dependents == ["cargo-front", "hash", "lint-cli"],
        &format!("the changed packages and what depends on them, through a dev-dependency too: {}", found.repr()),
    );
    expect(
        found.order == ["hash", "ids", "lint-cli", "codegen", "link", "cargo-front"],
        &format!("each after what it needs; a dev-dependency's cycle orders nothing: {}", repr_strs(&found.order)),
    );
    let args = found.args();
    expect(
        args.get(..4).is_some_and(|it| *it == ["-p", "hash", "-p", "ids"])
            && args.len() == 12
            && !found.all
            && found.elsewhere == 1,
        &format!("the arguments, and the doc counted elsewhere: {}", found.repr()),
    );
    let alone = affected_or_fail(&packages, &["crates/ids/src/lib.rs"], false);
    expect(alone.order == ["ids"] && alone.dependents.is_empty(), "without dependents, the changed alone");
    let leaf = affected_or_fail(&packages, &["crates/lone/src/lib.rs", "crates/lone/tests/it.rs"], true);
    expect(leaf.order == ["lone"] && leaf.changed == ["lone"], "a package nothing depends on is checked alone");
    let none = affected_or_fail(&packages, &["crates/app-shell/src/main.rs", "README.md"], true);
    expect(
        none.order.is_empty() && none.elsewhere == 2,
        "a file of an excluded directory or of none asks for no check",
    );
    expect(
        affected_or_fail(&packages, &["tools/lint/cli/src/main.rs"], true).order == ["lint-cli"],
        "a member outside crates/",
    );
    for file in ["Cargo.lock", ".clippy.toml", "harness/toolchain", ".cargo/config.toml"] {
        let whole = affected_or_fail(&packages, &[file, "crates/lone/src/lib.rs"], true);
        expect(
            whole.all
                && whole.why_all == file
                && whole.args() == ["--workspace"]
                && whole.order.len() == packages.len(),
            &format!("{file} is read by every package's check: {}", whole.repr()),
        );
    }
    expect(!affected_or_fail(&packages, &["crates/ids/Cargo.toml"], true).all, "a member's own manifest is its own");
    let package = |name: &str, need: &str| Package {
        name: name.to_owned(),
        dir: name.to_owned(),
        needs: BTreeSet::from([need.to_owned()]),
        dev_needs: BTreeSet::new(),
    };
    let cycle = [package("a", "b"), package("b", "a")];
    match affected(&cycle, &["a/src/lib.rs"], true) {
        Ok(_) => expect(false, "a cycle of normal dependencies is refused"),
        Err(e) => expect(e.contains("a, b"), &format!("the cycle is named: {e}")),
    }
    let unknown = input::read(r#"{"base": "main", "since": "x"}"#).and_then(|it| input::of::<Args>(&it, ""));
    expect(unknown.is_err(), "an unknown field is refused");
    println!("{SCRIPT} selftest: ok");
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.as_slice() {
        [one] if one == "selftest" => selftest(),
        [one] => main_with(&input::parse::<Args>(one, SCRIPT)),
        _ => {
            eprintln!("usage: {SCRIPT} '<json>' | selftest\n\n{DOC}");
            std::process::exit(1);
        }
    }
}
