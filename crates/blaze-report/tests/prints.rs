//! The guard of the one writer (`blaze_report::out`): std's `println!`, `print!`, `eprintln!`
//! and `eprint!` panic when their stream's reader is gone (`blaze --version | head -1`), so a
//! command's path writes with `blaze_report::outln!`, `out!`, `errln!` and `err!`, which end
//! the process quietly then. This holds each crate to the number of std's macros it has left
//! (`prints.txt`): a crate's number may fall, and its row is then lowered in the same change;
//! it never rises, and a crate with no row has none. It reads the sources as text; a ban in
//! `.clippy.toml` (`disallowed-macros`) takes its place once every row is 0.

use std::{collections::BTreeMap, fs, path::Path};

/// How many of std's print macros `line` calls.
fn count(line: &str) -> u32 {
    let mut found = 0;
    let mut from = 0;
    while let Some(at) = line[from..].find("print") {
        let at = from + at;
        from = at + 5;
        let after = &line[from..];
        if !(after.starts_with("!(") || after.starts_with("ln!(")) {
            continue;
        }
        // `print` or `eprint`, and before it nothing of a longer name.
        let before = &line[..at];
        let before = before.strip_suffix('e').unwrap_or(before);
        if before.chars().next_back().is_some_and(|c| c.is_ascii_alphanumeric() || c == '_') {
            continue;
        }
        found += 1;
    }
    found
}

fn walk(dir: &Path, total: &mut u32) {
    let Ok(entries) = fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        if path.is_dir() {
            if name != "tests" {
                walk(&path, total);
            }
        } else if path.extension().is_some_and(|it| it == "rs") && name != "tests.rs" {
            let text = fs::read(&path).map(|it| String::from_utf8_lossy(&it).into_owned()).unwrap_or_default();
            *total += text.lines().filter(|it| !it.trim_start().starts_with("//")).map(count).sum::<u32>();
        }
    }
}

#[test]
fn the_matcher_finds_stds_print_macros_and_nothing_else() {
    assert_eq!(count(r#"println!("a"); eprintln!("b"); print!("c"); eprint!("d");"#), 4);
    assert_eq!(count(r#"std::println!("a")"#), 1);
    assert_eq!(count(r#"blaze_report::outln!("a"); my_print!("b"); sprintln!("c"); fingerprint!(x)"#), 0);
    assert_eq!(count("let print = 1; print(x)"), 0);
}

/// The control: a crate that gains one of std's print macros is over its row.
#[test]
fn no_crate_has_more_of_stds_print_macros() {
    let crates = Path::new(env!("CARGO_MANIFEST_DIR")).parent().expect("crates/");
    let table = fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/prints.txt")).unwrap();
    let allowed: BTreeMap<&str, u32> = table
        .lines()
        .filter(|it| !it.starts_with('#') && !it.is_empty())
        .map(|it| {
            let (name, n) = it.split_once(' ').expect("a crate and its number");
            (name, n.parse().expect("a number"))
        })
        .collect();
    let mut wrong = Vec::new();
    let mut names: Vec<_> = fs::read_dir(crates).unwrap().flatten().map(|it| it.file_name()).collect();
    names.sort();
    for name in names {
        let name = name.to_string_lossy().into_owned();
        // harness is a workspace of its own.
        if name == "harness" {
            continue;
        }
        let mut found = 0;
        walk(&crates.join(&name).join("src"), &mut found);
        let row = allowed.get(name.as_str()).copied().unwrap_or(0);
        if found > row {
            wrong.push(format!(
                "{name} has {found}, its row allows {row}: write with `blaze_report::outln!`, `out!`, `errln!` or `err!` (std's macros panic when the reader is gone)"
            ));
        } else if found < row {
            wrong.push(format!("{name} has {found} left, its row says {row}: lower the row in tests/prints.txt"));
        }
    }
    assert!(wrong.is_empty(), "{}", wrong.join("\n"));
}
