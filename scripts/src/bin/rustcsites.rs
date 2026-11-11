//! rustcsites: the places in crates/ that start rustc, rustdoc, cargo or rustup, held to
//! `harness/norustc/sites.json`. The port of `harness/rustcsites.py`, line for line: the same
//! flag, lines printed, file written and exit status (docs/v2/plan/design/scripts.md, "Ports and
//! their parity"). What follows is the Python's docstring.
//!
//!     blaze run -q -p scripts --bin rustcsites [-- --record]
//!
//! rustcsites.py: the places in crates/ that start rustc, rustdoc, cargo or rustup, or read
//! which one to start, counted per file against norustc/sites.json. One more in a file fails:
//! blaze is to run no rustc (docs/no-rustc.md), so no new place may start one, and a place that
//! goes lowers the count.
//!
//!     rustcsites.py [--record]
//!
//! --record writes the counts as they are, after a change that removed a place.
//!
//! Three things look like a place and are not one, and are told apart here, each precisely:
//!
//! - `CARGO_BIN_EXE_<name>`, the variable cargo gives a test with the path of the package's
//!   executable `<name>`: it names that program, not cargo, so only the `<name>` is matched
//!   (`Command::new(env!("CARGO_BIN_EXE_blaze"))` starts blaze; `..._rustc` would still count).
//! - The text of a raw string that spans lines: a test's fixture of Rust source (`r#"..."#`),
//!   which is data a test compiles or checks, never run. No command name spans lines, so a raw
//!   string on one line is still matched.
//! - A line under a comment with an `// asks blaze: <why>` line: a build script asking its `$RUSTC`, which
//!   blaze sets to its own stand-in for rustc (blaze-cargo `serve/unit/script.rs`, the `rustc` of
//!   its tools; docs/no-rustc.md, end state 3), so blaze answers and no rustc starts. The comment
//!   must say why; such places are listed on every run so they stay in view.

use std::path::{Path, PathBuf};

use blaze_script::{
    Map, Value, fail,
    pyjson::dumps_indent,
    pystr::{is_space, is_word, lstrip},
    regex::Regex,
    universal_newlines,
};

const SCRIPT: &str = "rustcsites.py";

/// The checkout: this package's parent, as the Python's was its own directory's.
fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).parent().map_or_else(|| PathBuf::from("."), Path::to_path_buf)
}

fn place() -> Regex {
    Regex::new(concat!(
        r"(?i)Command::new\([^)]*(rustc|rustdoc|cargo|rustup)",
        r"|toolchain::command\([^,]*(rustc|rustdoc|cargo|rustup)",
        r#"|var(_os)?\("(RUSTC|RUSTDOC|CARGO)"\)"#,
        r"|MetadataCommand::new\(\)",
        r"|\.tool\(Tool::(Rustc|Cargo)",
    ))
    .unwrap_or_else(|e| fail(SCRIPT, &e.to_string()))
}

/// Python's `RAW`, `(?<![\w"'])[bc]?r(#*)"(.*?)"\1` with DOTALL, at byte `at` of `text`: the
/// end of the literal and the span of its body, or `None` when none starts there. The
/// lookbehind and the backreference are what the regex crate has not, so it is matched here.
fn raw_at(text: &str, at: usize) -> Option<(usize, usize, usize)> {
    if text[..at].chars().next_back().is_some_and(|c| is_word(c) || c == '"' || c == '\'') {
        return None;
    }
    let bytes = text.as_bytes();
    let mut i = at;
    if matches!(bytes.get(i), Some(b'b' | b'c')) {
        i += 1;
    }
    if bytes.get(i) != Some(&b'r') {
        return None;
    }
    i += 1;
    let hashes = bytes[i..].iter().take_while(|b| **b == b'#').count();
    i += hashes;
    if bytes.get(i) != Some(&b'"') {
        return None;
    }
    let body = i + 1;
    let close = format!("\"{}", "#".repeat(hashes));
    let end = body + text[body..].find(&close)?;
    Some((end + close.len(), body, end))
}

/// `code_of`: `text` with the text of each raw string that spans lines blanked, its lines kept.
fn code_of(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut copied = 0;
    let mut at = 0;
    // Bounded: `at` only grows, by a character or past a whole literal.
    while at < text.len() {
        match raw_at(text, at) {
            Some((end, body, body_end)) => {
                let inner = &text[body..body_end];
                if inner.contains('\n') {
                    out.push_str(&text[copied..body]);
                    out.extend(inner.chars().map(|c| if c == '\n' { '\n' } else { ' ' }));
                    copied = body_end;
                }
                at = end;
            }
            None => at += text[at..].chars().next().map_or(1, char::len_utf8),
        }
    }
    out.push_str(&text[copied..]);
    out
}

/// `^\s*// asks blaze: \S`.
fn asks_blaze_line(line: &str) -> bool {
    lstrip(line).strip_prefix("// asks blaze: ").and_then(|rest| rest.chars().next()).is_some_and(|c| !is_space(c))
}

/// Whether the run of `//` comment lines just above `lines[index]` has an `asks blaze:` line.
fn asks_blaze(lines: &[&str], index: usize) -> bool {
    lines[..index].iter().rev().take_while(|it| lstrip(it).starts_with("//")).any(|it| asks_blaze_line(it))
}

/// The `.rs` files under crates/ in `os.walk`'s order: a directory's files (by name) before its
/// directories (by name, not `target`, not crates/harness, not a link), each path from the
/// root.
fn rust_files(root: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let mut todo = vec!["crates".to_owned()];
    // Bounded: each directory is listed once, and links to directories are not followed.
    while let Some(dir) = todo.pop() {
        let Ok(listed) = std::fs::read_dir(root.join(&dir)) else { continue };
        let mut dirs = Vec::new();
        let mut files = Vec::new();
        for entry in listed.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let link = entry.file_type().is_ok_and(|t| t.is_symlink());
            // `os.walk` counts a link to a directory as a directory, and does not enter it.
            if entry.path().is_dir() {
                if !link && name != "target" && !(dir == "crates" && name == "harness") {
                    dirs.push(name);
                }
            } else {
                files.push(name);
            }
        }
        files.sort();
        out.extend(files.into_iter().filter(|it| it.ends_with(".rs")).map(|it| format!("{dir}/{it}")));
        dirs.sort();
        todo.extend(dirs.into_iter().rev().map(|it| format!("{dir}/{it}")));
    }
    out
}

type Lines = Vec<(String, Vec<usize>)>;

/// The places that start or name rustc, rustdoc, cargo or rustup, by file, and the places
/// marked as asking blaze, by file, each in the walk's order.
fn places(root: &Path) -> (Lines, Lines) {
    let place = place();
    let (mut found, mut asks) = (Vec::new(), Vec::new());
    for rel in rust_files(root) {
        let path = root.join(&rel);
        let text = match std::fs::read(&path).map(String::from_utf8) {
            Ok(Ok(text)) => universal_newlines(text),
            Ok(Err(_)) => fail(SCRIPT, &format!("{} is not UTF-8", path.display())),
            Err(e) => fail(SCRIPT, &blaze_script::os_error(&e, &path.to_string_lossy())),
        };
        let raw_lines: Vec<&str> = text.split('\n').collect();
        let (mut lines, mut asked) = (Vec::new(), Vec::new());
        for (index, line) in code_of(&text).split('\n').enumerate() {
            if !place.is_match(&line.replace("CARGO_BIN_EXE_", "")) {
                continue;
            }
            if asks_blaze(&raw_lines, index) { asked.push(index + 1) } else { lines.push(index + 1) }
        }
        if !lines.is_empty() {
            found.push((rel.clone(), lines));
        }
        if !asked.is_empty() {
            asks.push((rel, asked));
        }
    }
    (found, asks)
}

fn joined(lines: &[usize]) -> String {
    lines.iter().map(usize::to_string).collect::<Vec<_>>().join(", ")
}

fn main() {
    let root = root();
    let sites = root.join("harness/norustc/sites.json");
    let (found, asks) = places(&root);
    for (path, lines) in &asks {
        println!("{path}: {} place(s) ask blaze as `rustc`, not rustc (lines {})", lines.len(), joined(lines));
    }
    let total: usize = found.iter().map(|(_, lines)| lines.len()).sum();
    if std::env::args().skip(1).any(|it| it == "--record") {
        let mut sorted: Vec<&(String, Vec<usize>)> = found.iter().collect();
        sorted.sort_by(|a, b| a.0.cmp(&b.0));
        let counts: Map<String, Value> = sorted.iter().map(|(path, lines)| (path.clone(), Value::from(lines.len()))).collect();
        if let Err(e) = std::fs::write(&sites, dumps_indent(&Value::Object(counts), 1) + "\n") {
            fail(SCRIPT, &blaze_script::os_error(&e, &sites.to_string_lossy()));
        }
        println!("recorded {total} places in {} files", found.len());
        return;
    }
    let recorded: Map<String, Value> = match std::fs::read_to_string(&sites).map(|it| serde_json::from_str::<Map<String, Value>>(&it)) {
        Ok(Ok(recorded)) => recorded,
        Ok(Err(e)) => fail(SCRIPT, &format!("{} is not a JSON object: {e}", sites.display())),
        Err(e) => fail(SCRIPT, &blaze_script::os_error(&e, &sites.to_string_lossy())),
    };
    let was_of = |path: &str| recorded.get(path).and_then(Value::as_i64).unwrap_or(0);
    let mut more = Vec::new();
    for (path, lines) in &found {
        let was = was_of(path);
        let now = i64::try_from(lines.len()).unwrap_or(i64::MAX);
        if now > was {
            more.push(format!("{path}: {now} places, {was} recorded (lines {})", joined(lines)));
        } else if now < was {
            println!("{path}: {now} places, {was} recorded; rustcsites.py --record lowers it");
        }
    }
    for (path, was) in &recorded {
        if !found.iter().any(|(it, _)| it == path) {
            println!("{path}: no place, {was} recorded; rustcsites.py --record lowers it");
        }
    }
    for line in &more {
        println!("NEW: {line}");
    }
    if !more.is_empty() {
        println!("a new place starts rustc, rustdoc, cargo or rustup: docs/no-rustc.md says why that is refused");
        std::process::exit(1);
    }
    println!("{total} places start rustc, rustdoc, cargo or rustup, none new");
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The refinement: `code_of` gives what the Python's `RAW.sub` gave (each expectation is
    /// Python 3.13's), and a place is found where the Python found one.
    #[test]
    fn raw_strings_are_blanked_as_python_blanks_them() {
        assert_eq!(code_of("let a = r#\"x\ncargo\"#;"), "let a = r#\" \n     \"#;");
        assert_eq!(code_of("r\"one line cargo\" r##\"a\"#\nb\"##"), "r\"one line cargo\" r##\"   \n \"##");
        assert_eq!(code_of("xr\"a\nb\" br\"c\nd\""), "xr\"a\nb\" br\" \n \"");
        assert_eq!(code_of("r#\"unclosed\n"), "r#\"unclosed\n");
        assert!(place().is_match("Command::new(\"CARGO\")") && place().is_match("std::env::var_os(\"rustc\")"));
        assert!(!place().is_match(&"Command::new(env!(\"CARGO_BIN_EXE_blaze\"))".replace("CARGO_BIN_EXE_", "")));
        assert!(asks_blaze(&["    // asks blaze: its rustc", "    // more", "x"], 2) && !asks_blaze(&["// asks blaze:  ", "x"], 1));
    }
}
