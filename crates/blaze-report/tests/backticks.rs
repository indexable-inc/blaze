//! The guard of rich text (docs/terminal.md, "Rich text"): a message that puts an
//! interpolation between backticks by hand (`` format!("no package `{name}`") ``) is meaning
//! written as punctuation, which a renderer then shows raw or parses back. Such a message is
//! written with `blaze_report::say!`, whose arguments are typed (`Code(name)`).
//!
//! The tree had thousands of them when the type was made, so this holds each crate to the
//! number it has left (`backticks.txt`): a crate's number may fall and its row is then lowered
//! in the same change; it never rises, and a crate with no row has none. It reads the tree's
//! sources as text; a blaze lint (`blaze::backtick_interpolation`) would do it by the
//! `format_args!` it sees, and is not written yet.

use std::{collections::BTreeMap, fs, path::Path};

/// How many times `line` puts one `{..}` between backticks.
fn count(line: &str) -> u32 {
    let mut found = 0;
    let mut rest = line;
    while let Some(at) = rest.find("`{") {
        rest = &rest[at + 2..];
        if let Some(end) = rest.find(['}', '`'])
            && rest[end..].starts_with("}`")
        {
            found += 1;
            rest = &rest[end + 2..];
        }
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
fn the_matcher_finds_an_interpolation_between_backticks_and_nothing_else() {
    assert_eq!(count(r#"format!("no package `{name}` in `{root}`")"#), 2);
    assert_eq!(count(r#"format!("run `blaze explain {name}`")"#), 0);
    assert_eq!(count(r#"format!("{name} is `a {} b`", x)"#), 0);
    assert_eq!(count(r#"format!("`{}`", x)"#), 1);
}

/// The control: a crate that gains one such message is over its row.
#[test]
fn no_crate_has_more_messages_with_an_interpolation_between_backticks() {
    let crates = Path::new(env!("CARGO_MANIFEST_DIR")).parent().expect("crates/");
    let table = fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/backticks.txt")).unwrap();
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
                "{name} has {found}, its row allows {row}: write the new message with `blaze_report::say!` (its arguments are typed, so nothing is put between backticks by hand)"
            ));
        } else if found < row {
            wrong.push(format!("{name} has {found} left, its row says {row}: lower the row in tests/backticks.txt"));
        }
    }
    assert!(wrong.is_empty(), "{}", wrong.join("\n"));
}
