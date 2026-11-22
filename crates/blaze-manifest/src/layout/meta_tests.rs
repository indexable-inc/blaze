//! The `CARGO_PKG_*` metadata table read from manifests (meta.rs): each value as cargo's
//! normalization makes it, through the reader every member and dependency is read by.
//!
//! The cargo book, "Environment variables Cargo sets for crates": "CARGO_PKG_AUTHORS — Colon
//! separated list of authors from the manifest of your package"; "CARGO_PKG_DESCRIPTION — The
//! description from the manifest of your package" (cargo sets it `""` when unstated); "The
//! `[workspace.package]` table ... keys can be inherited by members with `key.workspace =
//! true`" (the cargo book, "Workspaces").
//!
//! Expected (`cargo test -p blaze-manifest --lib -- layout::meta_tests`): 3 passed, printing
//! "a: authors `A <a@x>:B`, description ``; b inherits `W <w@x>`, readme `../../README.md`".

use blaze_mem::{Root, RootOpts};

use super::{Entry, Layout, LayoutError, Need, Pass, PackageMetaVars, Units};

/// The test's root heap, made at its top; every table read borrows it.
fn root() -> Root {
    Root::new(RootOpts::TEST).unwrap_or_else(|e| panic!("the test's root: {e}"))
}

/// Files with their texts; a directory is implied by a file under it.
fn read<'r>(root: &'r Root, files: &[(&'static str, &'static str)], start: &str) -> Result<Units<'r>, LayoutError<'r>> {
    let alloc = root.alloc();
    let mut layout = Layout::new(start, alloc)?;
    for _ in 0..64 {
        match layout.pass()? {
            Pass::Done(units) => return Ok(units),
            Pass::Need(needs) => {
                for need in needs.iter() {
                    match need {
                        Need::Manifest(path) => {
                            let text = files.iter().find(|(p, _)| *p == path.as_str()).map(|(_, t)| *t);
                            layout.manifest(path, text)?;
                        }
                        Need::Listing(dir) => {
                            let mut out: std::vec::Vec<Entry<'static>> = std::vec::Vec::new();
                            for &(path, _) in files {
                                let rest: Option<&'static str> = match dir.is_empty() {
                                    true => Some(path),
                                    false => path.strip_prefix(dir.as_str()).and_then(|rest| rest.strip_prefix('/')),
                                };
                                let Some(rest) = rest else { continue };
                                let (name, is_dir) = match rest.find('/') {
                                    Some(i) => (&rest[..i], true),
                                    None => (rest, false),
                                };
                                if !out.iter().any(|entry| entry.name == name) {
                                    out.push(Entry { name, is_dir });
                                }
                            }
                            layout.listing(dir, if out.is_empty() { None } else { Some(out.as_slice()) });
                        }
                    }
                }
            }
        }
    }
    panic!("the reader did not finish in 64 passes");
}

const ROOT: &str = "[workspace]\nmembers = [\"crates/*\"]\n\n[workspace.package]\nauthors = [\"W <w@x>\"]\nreadme = \"README.md\"\nlicense-file = \"LICENSE\"\nrust-version = \"1.70\"\n";
const A: &str = "[package]\nname = \"a\"\nversion = \"1.2.3-rc.1\"\nauthors = [\"A <a@x>\", \"B\"]\nreadme = false\n";
const B: &str = "[package]\nname = \"b\"\nauthors.workspace = true\nreadme.workspace = true\nlicense-file = { workspace = true }\nrust-version.workspace = true\ndescription = \"the b crate\"\n";
const C: &str = "[package]\nname = \"c\"\n";

fn workspace() -> [(&'static str, &'static str); 9] {
    [
        ("Cargo.toml", ROOT),
        ("README.md", ""),
        ("crates/a/Cargo.toml", A),
        ("crates/a/src/lib.rs", ""),
        ("crates/b/Cargo.toml", B),
        ("crates/b/src/lib.rs", ""),
        ("crates/c/Cargo.toml", C),
        ("crates/c/src/lib.rs", ""),
        ("crates/c/README.txt", ""),
    ]
}

fn vars_of<'u>(units: &'u Units<'_>, name: &str) -> PackageMetaVars<'u> {
    units.members.iter().find(|m| m.name.as_str() == name).unwrap_or_else(|| panic!("no member `{name}`")).meta.vars()
}

/// Written values: `authors` joined with `:`, an unstated key `""`, `readme = false` `""`.
/// An inherited key is the root's; an inherited path is made relative to the member.
#[test]
fn metadata_is_the_manifest_s_joined_empty_or_inherited() {
    let root = root();
    let files = workspace();
    let units = read(&root, &files, "crates/a").unwrap_or_else(|e| panic!("{e}"));
    let a = vars_of(&units, "a");
    assert_eq!(a.authors, "A <a@x>:B", "CARGO_PKG_AUTHORS is the authors joined with `:`");
    assert_eq!(a.description, "", "an unstated description is the empty string");
    assert_eq!(a.readme, "", "`readme = false` is no readme");
    assert_eq!(a.rust_version, "", "not inherited unless asked");
    let b = vars_of(&units, "b");
    assert_eq!(b.authors, "W <w@x>", "`authors.workspace = true` is the root's");
    assert_eq!(b.readme, "../../README.md", "an inherited readme is the root's file, from the member's directory");
    assert_eq!(b.license_file, "../../LICENSE");
    assert_eq!(b.rust_version, "1.70");
    assert_eq!(b.description, "the b crate");
    // CONTROL: `c` states nothing; its README.txt is found as cargo finds it.
    let c = vars_of(&units, "c");
    assert_eq!(c.readme, "README.txt", "an unstated readme is the first default readme file in the package");
    assert_eq!(c.authors, "");
    // The table's names, one per value, in one order.
    let pairs = a.pairs();
    assert_eq!(pairs[0], ("CARGO_PKG_AUTHORS", "A <a@x>:B"));
    assert_eq!(pairs[1], ("CARGO_PKG_DESCRIPTION", ""));
    println!("a: authors `{}`, description `{}`; b inherits `{}`, readme `{}`", a.authors, a.description, b.authors, b.readme);
}

/// A key inherited from a root that does not offer it is refused by name, never `""`.
#[test]
fn an_inherited_key_the_root_lacks_is_refused_by_name() {
    let root = root();
    let mut files = workspace();
    files[4] = ("crates/b/Cargo.toml", "[package]\nname = \"b\"\nhomepage.workspace = true\n");
    match read(&root, &files, "crates/b") {
        Err(LayoutError::NotInherited { key, .. }) => assert_eq!(key, "homepage"),
        Err(other) => panic!("another refusal: {other}"),
        Ok(_) => panic!("an inherited homepage the root lacks was answered"),
    }
}

/// An edit of `authors` moves `CARGO_PKG_AUTHORS` and no other value (rule 23: the cause
/// removed, its effect goes; an unrelated key edited, the authors' value stays).
#[test]
fn an_authors_edit_moves_only_the_authors_value() {
    let root = root();
    let files = workspace();
    let before = read(&root, &files, "crates/a").unwrap_or_else(|e| panic!("{e}"));
    let mut edited = workspace();
    edited[2] = ("crates/a/Cargo.toml", "[package]\nname = \"a\"\nversion = \"1.2.3-rc.1\"\nauthors = [\"A <a@x>\"]\nreadme = false\n");
    let after = read(&root, &edited, "crates/a").unwrap_or_else(|e| panic!("{e}"));
    let (was, now) = (vars_of(&before, "a"), vars_of(&after, "a"));
    assert_ne!(was.authors, now.authors);
    assert_eq!(now.authors, "A <a@x>");
    assert_eq!((was.description, was.homepage, was.readme), (now.description, now.homepage, now.readme));
    let mut other = workspace();
    other[2] = ("crates/a/Cargo.toml", "[package]\nname = \"a\"\nversion = \"1.2.3-rc.1\"\nauthors = [\"A <a@x>\", \"B\"]\nreadme = false\ndescription = \"new\"\n");
    let described = read(&root, &other, "crates/a").unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(vars_of(&described, "a").authors, was.authors, "a description edit leaves the authors' value");
}
