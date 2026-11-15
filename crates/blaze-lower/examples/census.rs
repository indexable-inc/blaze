//! What keeps crates from a surface: for each package of a list, every construct that makes a
//! file of it `unplain` ([`blaze_lower::census_of`], the walk that keys use), with where the
//! file is reached from, and the flags that take a surface away in other ways.
//!
//! Input, on stdin, a line per package: `name\tdir\tedition\tlib_root\tother_roots`, the roots
//! absolute, `other_roots` comma-separated (bins, tests, benches, examples, build script).
//! Output, on stdout, tab-separated lines:
//! - `C name dir files unplain shadows whole reaches resolves calls tracks names_location extends`
//! - `D name relpath line construct macro reach`, reach one of `lib` (from the library's root
//!   with no `cfg(test)` on the way), `test` (only through a `cfg(test)` module), `other` (from
//!   another target's root), `none` (no root reaches it; the source root holds it all the same)
//! - `M name macro kind`: a `macro_rules!` (`rules`, `export` with `#[macro_export]`) or a proc
//!   macro (`proc`, `attr`, `derive`) the package defines.

use std::collections::{BTreeMap, HashMap};
use std::io::BufRead;
use std::path::{Path, PathBuf};

use syntax::{Edition, NodeOrToken, SyntaxKind, SyntaxNode};

fn main() {
    let stdin = std::io::stdin();
    for line in stdin.lock().lines() {
        let line = line.expect("stdin");
        let parts: Vec<&str> = line.split('\t').collect();
        let [name, dir, edition, lib, others] = parts[..] else {
            eprintln!("census: a line of five fields, not `{line}`");
            continue;
        };
        let edition = match edition {
            "2015" => Edition::Edition2015,
            "2018" => Edition::Edition2018,
            "2021" => Edition::Edition2021,
            _ => Edition::Edition2024,
        };
        package(name, Path::new(dir), edition, lib, others);
    }
}

/// Where a file is reached from, the narrowest first.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
enum Reach {
    Lib,
    Test,
    Other,
    None,
}

fn package(name: &str, dir: &Path, edition: Edition, lib: &str, others: &str) {
    let mut files = Vec::new();
    rust_files(dir, dir, &mut files);
    files.sort();
    let mut parsed: BTreeMap<PathBuf, (String, SyntaxNode)> = BTreeMap::new();
    for file in &files {
        let Ok(text) = std::fs::read_to_string(file) else { continue };
        let parse = syntax::SourceFile::parse(&text, edition);
        let root = parse.syntax_node();
        parsed.insert(file.clone(), (text, root));
    }
    // Reach of each file from the roots.
    let mut reach: HashMap<PathBuf, Reach> = HashMap::new();
    let walk = |root: &str, from: Reach, reach: &mut HashMap<PathBuf, Reach>| {
        if root.is_empty() {
            return;
        }
        let mut stack = vec![(PathBuf::from(root), true, from)];
        while let Some((file, mod_root, at)) = stack.pop() {
            let file = std::fs::canonicalize(&file).unwrap_or(file);
            if reach.get(&file).is_some_and(|it| *it <= at) {
                continue;
            }
            reach.insert(file.clone(), at);
            let Some((_, node)) = parsed.get(&file) else { continue };
            let base = match mod_root {
                true => file.parent().unwrap().to_path_buf(),
                false => file.with_extension(""),
            };
            modules(node, &file, &base, at, &mut stack);
        }
    };
    walk(lib, Reach::Lib, &mut reach);
    for other in others.split(',') {
        walk(other, Reach::Other, &mut reach);
    }
    let (mut unplain, mut flags) = (false, [false; 8]);
    for (file, (text, root)) in &parsed {
        let (code, denials) = blaze_lower::census_of(root, text, edition);
        unplain |= code.unplain;
        for (flag, it) in flags.iter_mut().zip([
            code.shadows,
            code.whole,
            code.reaches,
            code.resolves,
            code.calls,
            code.tracks,
            code.names_location,
            code.extends,
        ]) {
            *flag |= it;
        }
        let rel = file.strip_prefix(dir).unwrap_or(file).display();
        let at = reach.get(file).copied().unwrap_or(Reach::None);
        for denial in denials {
            let at = format!("{at:?}").to_lowercase();
            println!("D\t{name}\t{rel}\t{}\t{:?}\t{}\t{at}", denial.line, denial.construct, denial.name);
        }
        macros(name, root);
    }
    let flags: Vec<&str> = flags.iter().map(|it| if *it { "1" } else { "0" }).collect();
    println!(
        "C\t{name}\t{}\t{}\t{}\t{}",
        dir.display(),
        parsed.len(),
        u8::from(unplain),
        flags.join("\t")
    );
}

/// The `.rs` files under `dir`, but those of a package nested in it and of `target`.
fn rust_files(top: &Path, dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(kind) = entry.file_type() else { continue };
        let file_name = entry.file_name();
        let named = file_name.to_string_lossy();
        if kind.is_dir() {
            if named.starts_with('.') || named == "target" || path.join("Cargo.toml").exists() {
                continue;
            }
            rust_files(top, &path, out);
        } else if named.ends_with(".rs") {
            out.push(std::fs::canonicalize(&path).unwrap_or(path));
        }
    }
}

fn attrs(node: &SyntaxNode) -> impl Iterator<Item = SyntaxNode> + '_ {
    node.children().filter(|it| it.kind() == SyntaxKind::ATTR)
}

fn tokens(node: &SyntaxNode) -> Vec<String> {
    node.descendants_with_tokens()
        .filter_map(NodeOrToken::into_token)
        .filter(|it| !it.kind().is_trivia())
        .map(|it| it.text().to_owned())
        .collect()
}

/// Whether `attr` is a `cfg` that names `test` (`cfg(test)`, `cfg(all(test, ..))`).
fn test_cfg(attr: &SyntaxNode) -> bool {
    let tokens = tokens(attr);
    tokens.iter().any(|it| it == "cfg")
        && tokens.iter().any(|it| it == "test")
        && !tokens.iter().any(|it| it == "not")
}

/// The out-of-line modules `node`'s items declare, onto `stack`.
fn modules(
    node: &SyntaxNode,
    file: &Path,
    base: &Path,
    at: Reach,
    stack: &mut Vec<(PathBuf, bool, Reach)>,
) {
    for item in node.children() {
        if item.kind() == SyntaxKind::ITEM_LIST {
            modules(&item, file, base, at, stack);
            continue;
        }
        if item.kind() != SyntaxKind::MODULE {
            continue;
        }
        let Some(name) = item.children().find(|it| it.kind() == SyntaxKind::NAME) else { continue };
        let name = name.text().to_string();
        let test = attrs(&item).any(|it| test_cfg(&it));
        let at = match (test, at) {
            (true, Reach::Lib) => Reach::Test,
            _ => at,
        };
        if let Some(list) = item.children().find(|it| it.kind() == SyntaxKind::ITEM_LIST) {
            modules(&list, file, &base.join(&name), at, stack);
            continue;
        }
        let path = attrs(&item).find_map(|attr| {
            let tokens = tokens(&attr);
            let at = tokens.iter().position(|it| it == "path")?;
            let value = tokens.get(at + 2)?;
            Some(value.trim_matches('"').to_owned())
        });
        match path {
            Some(path) => stack.push((file.parent().unwrap().join(path), true, at)),
            None => {
                let flat = base.join(format!("{name}.rs"));
                match flat.exists() {
                    true => stack.push((flat, false, at)),
                    false => stack.push((base.join(&name).join("mod.rs"), true, at)),
                }
            }
        }
    }
}

/// The macros `root` defines, as `M` lines.
fn macros(name: &str, root: &SyntaxNode) {
    for node in root.descendants() {
        match node.kind() {
            SyntaxKind::MACRO_RULES => {
                let Some(named) = node.children().find(|it| it.kind() == SyntaxKind::NAME) else {
                    continue;
                };
                let export = attrs(&node).any(|it| tokens(&it).iter().any(|t| t == "macro_export"));
                let kind = if export { "export" } else { "rules" };
                println!("M\t{name}\t{}\t{kind}", named.text());
            }
            SyntaxKind::FN => {
                let Some(named) = node.children().find(|it| it.kind() == SyntaxKind::NAME) else {
                    continue;
                };
                for attr in attrs(&node) {
                    let tokens = tokens(&attr);
                    let kind = match tokens.get(2).map(String::as_str) {
                        Some("proc_macro") => "proc",
                        Some("proc_macro_attribute") => "attr",
                        Some("proc_macro_derive") => "derive",
                        _ => continue,
                    };
                    println!("M\t{name}\t{}\t{kind}", named.text());
                }
            }
            _ => {}
        }
    }
}
