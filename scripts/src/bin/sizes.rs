//! sizes: no source file grows past a bound, and those already past it only shrink (rule
//! one-concept-one-home: crates/blaze-cargo/src/serve/job.rs passed 10,000 lines and eight lanes
//! collided in it on 2026-11-12; a file that size is several concepts in one place). `DOC` below
//! is what its usage error prints.
//!
//!     blaze run -q -p scripts --bin sizes -- '{"command": "check", "base": "origin/main"}'
//!     blaze run -q -p scripts --bin sizes -- '{"command": "write"}'
//!     blaze run -q -p scripts --bin sizes -- selftest
//!
//! The bound is 1,500 lines ([`BOUND`]): about where review of one file and the collisions of
//! lanes writing in it began in job.rs and daemon.rs, and three times the median file of the
//! tree, so a file under it is one concept with its tests beside it. The shape is allocs.rs's: a
//! list (scripts/sizes.txt) of the files over the bound with their line counts, which only
//! shrinks; a file not listed is held to the bound. It runs with no compiler, a gate step of the
//! `scripts` job (.github/workflows/blaze.yaml), beside `allocs`.

/// What the usage error prints.
const DOC: &str = r#"sizes: no source file grows past the bound, and those past it only shrink (rule one-concept-one-home).

    blaze run -q -p scripts --bin sizes -- '{"command": "check", "base": "<rev>"}'
    blaze run -q -p scripts --bin sizes -- '{"command": "write"}'
    blaze run -q -p scripts --bin sizes -- selftest

    command   check   every `.rs` file under crates/ over 1500 lines is a row of scripts/sizes.txt,
                      and has its row's count: one over it is refused, one under it must lower its
                      row (`write`), a row whose file is gone or back under the bound is removed;
                      a file with no row is held to the bound; with `base`, no row is above the
                      base's, and no row is new since it (CI)
              write   write the list from the tree as it is
    root      the checkout (".")
    base      the commit the change is against, or null: its list (`git show`) holds every row
              down and says which files may be over the bound at all

A file's lines are its newline characters, as `wc -l` counts them. A directory named `target` or
`vendor` is not read: built output and vendored code are not this tree's to split. Tests count:
a test file lanes collide in is as much a collision as any other.

The row is `<path><TAB><lines>`, the path from the root, rows sorted by path.
"#;

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use blaze_script::{
    TempDir, fail, git,
    input::{self, Fields, Input, InputError},
    read_text,
};

const SCRIPT: &str = "sizes";
/// The list, from the root.
const LIST: &str = "scripts/sizes.txt";
/// The most lines a file not listed may have.
const BOUND: usize = 1500;
/// The directories not read.
const SKIPPED: [&str; 2] = ["target", "vendor"];
/// What every message about a file over its row ends with.
const RULE: &str = "rule one-concept-one-home: a file past 1500 lines is several concepts in one place, and lanes \
                    collide in it; move a concept to a module of its own (a file beside it, `mod <name>;`)";
/// The list's first lines.
const HEADER: &str = "# The .rs files under crates/ over the bound of 1500 lines (a directory named `target` or `vendor` is not read),\n\
                      # each with its line count. Only shrinks: a listed file may not grow, a file that shrinks lowers its row,\n\
                      # and a file not listed is held to the bound (rule one-concept-one-home: a file eight lanes collide in is\n\
                      # several concepts). Written by scripts/src/bin/sizes.rs (`{\"command\": \"write\"}`), checked in CI.\n\
                      # file\tlines\n";

struct Args {
    command: String,
    root: String,
    base: Option<String>,
}

impl Input for Args {
    const FIELDS: &'static [&'static str] = &["command", "root", "base"];

    fn parse(f: &Fields<'_>) -> Result<Args, InputError> {
        Ok(Args {
            command: f.one_of("command", &["check", "write"], None)?,
            root: f.str("root", Some("."))?,
            base: f.str_or_null("base")?,
        })
    }
}

/// The lines of `bytes`: its newline characters, as `wc -l` counts them.
fn lines_of(bytes: &[u8]) -> usize {
    bytes.iter().filter(|it| **it == b'\n').count()
}

/// Every `.rs` file under crates/, by its path from the root, with its lines; none under a
/// directory of [`SKIPPED`]. Directories that are links are not entered.
fn sizes(root: &Path) -> BTreeMap<String, usize> {
    let mut out = BTreeMap::new();
    let mut todo = vec![vec!["crates".to_owned()]];
    // Bounded: each directory is listed once, and links to directories are not followed.
    while let Some(dir) = todo.pop() {
        let full: PathBuf = dir.iter().fold(root.to_path_buf(), |path, part| path.join(part));
        let listed = std::fs::read_dir(&full).unwrap_or_else(|e| fail(SCRIPT, &format!("{} could not be listed: {e}", full.display())));
        for entry in listed {
            let entry = entry.unwrap_or_else(|e| fail(SCRIPT, &format!("{} could not be listed: {e}", full.display())));
            let name = entry.file_name().to_string_lossy().into_owned();
            let mut parts = dir.clone();
            parts.push(name.clone());
            let is_dir = entry.file_type().is_ok_and(|t| t.is_dir());
            if is_dir {
                if !SKIPPED.contains(&name.as_str()) {
                    todo.push(parts);
                }
            } else if name.ends_with(".rs") {
                let path = parts.join("/");
                let bytes = std::fs::read(entry.path()).unwrap_or_else(|e| fail(SCRIPT, &format!("{path} could not be read: {e}")));
                out.insert(path, lines_of(&bytes));
            }
        }
    }
    out
}

/// The files over the bound, with their lines.
fn over(sizes: &BTreeMap<String, usize>) -> BTreeMap<String, usize> {
    sizes.iter().filter(|(_, lines)| **lines > BOUND).map(|(path, lines)| (path.clone(), *lines)).collect()
}

/// The list as `write` writes it: the header, then `<path><TAB><lines>` per file over the bound.
fn render(over: &BTreeMap<String, usize>) -> String {
    let mut out = HEADER.to_owned();
    for (path, lines) in over {
        out.push_str(&format!("{path}\t{lines}\n"));
    }
    out
}

/// The list's rows; `Err` names the line that is not a row, a comment or blank.
fn parse_rows(text: &str) -> Result<BTreeMap<String, usize>, String> {
    let mut rows = BTreeMap::new();
    for (at, line) in text.split('\n').enumerate() {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let row = match line.split('\t').collect::<Vec<_>>().as_slice() {
            [path, lines] if !path.is_empty() => lines.parse::<usize>().ok().map(|n| (*path, n)),
            _ => None,
        };
        let Some((path, lines)) = row else {
            return Err(format!("{LIST}:{}: {line:?} is not `<path><TAB><lines>`", at + 1));
        };
        if rows.insert(path.to_owned(), lines).is_some() {
            return Err(format!("{LIST}:{}: `{path}` has two rows", at + 1));
        }
    }
    Ok(rows)
}

/// What is wrong: `sizes` every file's lines, `rows` the list's, `base_rows` the base's list
/// (when a base was given and has one).
fn problems(
    sizes: &BTreeMap<String, usize>,
    rows: &BTreeMap<String, usize>,
    base_rows: Option<&BTreeMap<String, usize>>,
) -> Vec<String> {
    let mut out = Vec::new();
    for (path, lines) in sizes {
        match rows.get(path) {
            None if *lines > BOUND => {
                out.push(format!("{path} has {lines} lines, over the bound of {BOUND} a file with no row in {LIST} is held to ({RULE})"));
            }
            None => {}
            Some(held) if lines > held => {
                out.push(format!("{path} has {lines} lines, over its row's {held} in {LIST}: the list only shrinks ({RULE})"));
            }
            Some(held) if lines < held => {
                let lower = if *lines > BOUND { format!("lower its row to {lines}") } else { "remove its row".to_owned() };
                out.push(format!("{path} has {lines} lines, under its row's {held} in {LIST}: {lower} (command `write`)"));
            }
            Some(_) => {}
        }
    }
    for (path, held) in rows {
        if !sizes.contains_key(path) {
            out.push(format!("{LIST} has a row for {path} ({held} lines), which is no file under crates/: remove it (command `write`)"));
        } else if *held <= BOUND {
            out.push(format!("{LIST}'s row for {path} is {held} lines, not over the bound of {BOUND}: remove it (command `write`)"));
        }
    }
    if let Some(base) = base_rows {
        for (path, held) in rows {
            match base.get(path) {
                None => out.push(format!(
                    "{LIST}: {path} has a row ({held} lines) and had none at the base: a file not listed is held to {BOUND} lines ({RULE})"
                )),
                Some(was) if held > was => {
                    out.push(format!("{LIST}: the row of {path} is {held} and was {was} at the base: the list only shrinks ({RULE})"));
                }
                Some(_) => {}
            }
        }
    }
    out
}

/// The base's rows: `None` when it has no list, before the ratchet began.
fn at_base(root: &Path, base: &str) -> Result<Option<BTreeMap<String, usize>>, String> {
    let root_arg = root.to_string_lossy();
    let dir = root_arg.as_ref();
    let commit = format!("{base}^{{commit}}");
    git(root, &["-C", dir, "rev-parse", "--verify", "--quiet", commit.as_str()])
        .map_err(|e| format!("the base {base} is no commit here (fetch it): {e}"))?;
    let listed = git(root, &["-C", dir, "ls-tree", "--name-only", base, "--", LIST])?;
    if listed.trim().is_empty() {
        return Ok(None);
    }
    let spec = format!("{base}:{LIST}");
    let text = git(root, &["-C", dir, "show", spec.as_str()])?;
    parse_rows(&text).map(Some).map_err(|e| format!("at {base}: {e}"))
}

fn main_with(it: &Args) {
    let root: PathBuf = std::fs::canonicalize(&it.root).unwrap_or_else(|e| fail(SCRIPT, &format!("the root {} is no directory: {e}", it.root)));
    if !root.join("crates").is_dir() {
        fail(SCRIPT, &format!("{} has no crates/ directory", root.display()));
    }
    let sizes = sizes(&root);
    let over = over(&sizes);
    let text = render(&over);
    if it.command == "write" {
        if let Err(e) = std::fs::write(root.join(LIST), &text) {
            fail(SCRIPT, &format!("{LIST} is not written: {e}"));
        }
        println!("{SCRIPT}: wrote {LIST}: {} files over {BOUND} lines", over.len());
        return;
    }
    let Some(listed) = read_text(&root.join(LIST)) else {
        fail(SCRIPT, &format!("{LIST} could not be read: every file over the bound is held to its row there (command `write` makes it)"))
    };
    let rows = parse_rows(&listed).unwrap_or_else(|e| fail(SCRIPT, &e));
    let base_rows = match &it.base {
        Some(base) => {
            let rows = at_base(&root, base).unwrap_or_else(|e| fail(SCRIPT, &e));
            if rows.is_none() {
                println!("{SCRIPT}: {base} has no {LIST}: the ratchet begins with this change, so no row is compared with the base");
            }
            rows
        }
        None => None,
    };
    let mut wrong = problems(&sizes, &rows, base_rows.as_ref());
    if wrong.is_empty() && listed != text {
        wrong.push(format!("{LIST} is not as command `write` writes it (its header, then a sorted `<path><TAB><lines>` row per file over {BOUND}): run it"));
    }
    if !wrong.is_empty() {
        let lines: Vec<String> = wrong.iter().map(|w| format!("{SCRIPT}: {w}")).collect();
        eprintln!("{}", lines.join("\n"));
        std::process::exit(1);
    }
    println!("{SCRIPT}: {} files over {BOUND} lines, each at its row in {LIST}; every other file is under the bound", over.len());
}

fn expect(holds: bool, what: &str) {
    blaze_script::expect(SCRIPT, holds, what);
}

fn selftest() {
    expect(lines_of(b"a\nb\n") == 2 && lines_of(b"a\nb") == 1 && lines_of(b"") == 0, "lines are newlines, as `wc -l` counts");

    // A tree: one file over the bound, one at it, one under a skipped directory.
    let tmp = TempDir::new("sizes-selftest-").unwrap_or_else(|e| fail(&format!("{SCRIPT} selftest"), &e.to_string()));
    let put = |path: &str, lines: usize| {
        let full = tmp.path().join(path);
        let made = full.parent().map_or(Ok(()), std::fs::create_dir_all).and_then(|()| std::fs::write(&full, "x\n".repeat(lines)));
        made.unwrap_or_else(|e| fail(&format!("{SCRIPT} selftest"), &format!("{path} is not written: {e}")));
    };
    put("crates/a/src/big.rs", BOUND + 10);
    put("crates/a/src/edge.rs", BOUND);
    put("crates/a/target/out.rs", BOUND * 2);
    put("crates/b/vendor/c/lib.rs", BOUND * 2);
    put("crates/a/README.md", BOUND * 2);
    let found = sizes(tmp.path());
    expect(
        found == BTreeMap::from([("crates/a/src/big.rs".to_owned(), BOUND + 10), ("crates/a/src/edge.rs".to_owned(), BOUND)]),
        &format!("the tree's files, target/ and vendor/ not read: {found:?}"),
    );
    let rows = parse_rows(&render(&over(&found))).unwrap_or_else(|e| fail(&format!("{SCRIPT} selftest"), &e));
    expect(rows == BTreeMap::from([("crates/a/src/big.rs".to_owned(), BOUND + 10)]), &format!("the rows read back: {rows:?}"));
    expect(problems(&found, &rows, Some(&rows)).is_empty(), "the tree as recorded is right");

    // Planted: the listed file grew by one line.
    let mut grown = found.clone();
    grown.insert("crates/a/src/big.rs".to_owned(), BOUND + 11);
    let said = problems(&grown, &rows, None);
    expect(said.len() == 1 && said[0].contains("over its row's 1510") && said[0].contains("one-concept-one-home"), &format!("a listed file that grew: {said:?}"));
    // Planted: a file not listed passes the bound by one.
    let mut new = found.clone();
    new.insert("crates/a/src/edge.rs".to_owned(), BOUND + 1);
    let said = problems(&new, &rows, None);
    expect(said.len() == 1 && said[0].contains("edge.rs has 1501 lines, over the bound"), &format!("a new file over the bound: {said:?}"));
    // `write` would list it; the base, which had no row for it, still refuses it.
    let written = over(&new);
    let said = problems(&new, &written, Some(&rows));
    expect(said.len() == 1 && said[0].contains("had none at the base"), &format!("a row new since the base: {said:?}"));
    // Planted: a row raised above the base's.
    let raised = BTreeMap::from([("crates/a/src/big.rs".to_owned(), BOUND + 11)]);
    let said = problems(&grown, &raised, Some(&rows));
    expect(said.len() == 1 && said[0].contains("was 1510 at the base"), &format!("a row raised: {said:?}"));
    // A file that shrank lowers its row; one back under the bound, or gone, removes it.
    let mut shrunk = found.clone();
    shrunk.insert("crates/a/src/big.rs".to_owned(), BOUND + 2);
    let said = problems(&shrunk, &rows, None);
    expect(said.len() == 1 && said[0].contains("lower its row to 1502"), &format!("a file that shrank: {said:?}"));
    shrunk.insert("crates/a/src/big.rs".to_owned(), BOUND);
    let said = problems(&shrunk, &rows, None);
    expect(said.iter().any(|it| it.contains("remove its row")), &format!("a file back under the bound: {said:?}"));
    shrunk.remove("crates/a/src/big.rs");
    let said = problems(&shrunk, &rows, None);
    expect(said.len() == 1 && said[0].contains("which is no file"), &format!("a file gone: {said:?}"));
    expect(
        parse_rows("a\tx\n").is_err() && parse_rows("a\t1\na\t2\n").is_err() && parse_rows("\t1\n").is_err() && parse_rows("a 1\n").is_err(),
        "a bad row is refused",
    );
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
