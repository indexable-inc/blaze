use std::alloc::Global;
use std::path::{Path, PathBuf};
use std::string::{String, ToString};
use std::vec::Vec;

use super::*;

#[test]
fn patterns_match_as_glob_s_do() {
    let cases: &[(&str, &str, bool)] = &[
        ("", "", true),
        ("", "a", false),
        ("abc", "abc", true),
        ("abc", "abd", false),
        ("a?c", "abc", true),
        ("a?c", "ac", false),
        ("?", "/", true),
        ("*", "", true),
        ("*", "any/thing", true),
        ("a*", "a", true),
        ("a*c", "abbbc", true),
        ("a*c", "abbbd", false),
        ("*.rs", ".rs", true),
        ("*a*a*a*a*a*a*a*a*b", "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", false),
        ("crates/*", "crates/a", true),
        ("crates/*", "crates/a/b", true),
        ("[ab]", "a", true),
        ("[ab]", "c", false),
        ("[a-c]x", "bx", true),
        ("[a-c]x", "dx", false),
        ("[!a-c]", "d", true),
        ("[!a-c]", "b", false),
        ("[]]", "]", true),
        ("[!]]", "]", false),
        ("[!]]", "x", true),
        ("[-a]", "-", true),
        ("[a-]", "-", true),
        ("**", "", true),
        ("**", "a/b/c", true),
        ("a/**/b", "a/b", true),
        ("a/**/b", "a/x/y/b", true),
        ("a/**/b", "a/xb", false),
        ("**/b", "b", true),
        ("**/b", "x/b", true),
        ("**/b", "xb", false),
        ("a/**", "a/", true),
        ("a/**", "a/x/y", true),
        ("a/**/**/b", "a/b", true),
        ("**/", "a", true),
        ("**/", "a/b", true),
        ("a/**", "a/xyz", true),
        ("**/b", "a/c", false),
        ("é?", "éé", true),
    ];
    for &(pattern, text, want) in cases {
        let parsed = Pattern::new_in(pattern, Global).unwrap_or_else(|e| panic!("{pattern}: {e}"));
        assert_eq!(parsed.matches(text), want, "{pattern:?} on {text:?}");
        assert_eq!(naive(&parsed, text), want, "naive {pattern:?} on {text:?}");
    }
}

#[test]
fn bad_patterns_are_glob_s_errors() {
    let cases: &[(&str, usize, Refusal)] = &[
        ("***", 2, Refusal::Wildcards),
        ("a/***", 4, Refusal::Wildcards),
        ("a**", 0, Refusal::RecursiveWildcards),
        ("**a", 2, Refusal::RecursiveWildcards),
        ("a/**b", 4, Refusal::RecursiveWildcards),
        ("[", 0, Refusal::InvalidRange),
        ("[a", 0, Refusal::InvalidRange),
        ("x[]", 1, Refusal::InvalidRange),
        ("[!a", 0, Refusal::InvalidRange),
        ("[!]", 0, Refusal::InvalidRange),
    ];
    for &(pattern, pos, refusal) in cases {
        assert_eq!(Pattern::new_in(pattern, Global), Err(PatternError { pos, refusal }), "{pattern:?}");
    }
    let shown = Pattern::new_in("[", Global).unwrap_err().to_string();
    assert_eq!(shown, "Pattern syntax error near position 0: invalid range pattern");
}

#[test]
fn metachars_are_glob_s() {
    assert!(!Pattern::new_in("abc", Global).unwrap().has_metachars());
    for it in ["a*", "?", "[a]"] {
        assert!(Pattern::new_in(it, Global).unwrap().has_metachars(), "{it}");
    }
}

/// glob 0.3's matcher, backtracking, under its default options: the oracle for the table.
fn naive(pattern: &Pattern<'_, Global>, text: &str) -> bool {
    #[derive(PartialEq)]
    enum Got {
        Match,
        Sub,
        Entire,
    }
    // glob's `follows_separator` is read only under options cargo does not set, but for `**`.
    fn from(tokens: &[Token<Global>], mut text: std::str::Chars<'_>) -> Got {
        for (at, token) in tokens.iter().enumerate() {
            match token {
                Token::AnySequence | Token::AnyRecursiveSequence => {
                    match from(&tokens[at + 1..], text.clone()) {
                        Got::Sub => {}
                        got => return got,
                    }
                    while let Some(c) = text.next() {
                        if *token == Token::AnyRecursiveSequence && !is_separator(c) {
                            continue;
                        }
                        match from(&tokens[at + 1..], text.clone()) {
                            Got::Sub => {}
                            got => return got,
                        }
                    }
                }
                one => {
                    let Some(c) = text.next() else { return Got::Entire };
                    let takes = match one {
                        Token::Char(it) => *it == c,
                        Token::AnyChar => true,
                        Token::AnyWithin(classes) => Class::holds(classes, c),
                        Token::AnyExcept(classes) => !Class::holds(classes, c),
                        Token::AnySequence | Token::AnyRecursiveSequence => false,
                    };
                    if !takes {
                        return Got::Sub;
                    }
                }
            }
        }
        if text.next().is_none() { Got::Match } else { Got::Sub }
    }
    from(&pattern.tokens, text.chars()) == Got::Match
}

const PIECES: &[&str] = &["a", "b", "/", ".", "*", "?", "**/", "[ab]", "[!a]", "[a-b]", "]"];
const LETTERS: &[char] = &['a', 'b', 'c', '/', '.'];

/// The table matcher and the backtracker agree on every pattern that parses.
#[test]
fn the_table_is_the_backtracker() {
    blaze_sim::check_kept(blaze_sim::Config::DEFAULT, blaze_sim::sim_kept!(), |tape| {
        let pattern: String = tape.vec(0..=6, "pattern", |tape| *tape.one_of(PIECES, "piece")).concat();
        let text: String = tape.vec(0..=8, "text", |tape| *tape.one_of(LETTERS, "char")).into_iter().collect();
        if let Ok(parsed) = Pattern::new_in(&pattern, Global) {
            assert_eq!(parsed.matches(&text), naive(&parsed, &text), "{pattern:?} on {text:?}");
        }
    });
}

/// A tree in memory: each path a file (`false`) or a directory (`true`).
struct Mem(std::collections::BTreeMap<PathBuf, bool>);

impl Mem {
    fn of(paths: &[&str]) -> Mem {
        let mut out = std::collections::BTreeMap::new();
        for it in paths {
            let path = Path::new(it.trim_end_matches('/'));
            for dir in path.ancestors().skip(1) {
                out.insert(dir.to_path_buf(), true);
            }
            out.insert(path.to_path_buf(), it.ends_with('/'));
        }
        Mem(out)
    }
}

impl Tree<Global> for Mem {
    type Error = std::io::Error;

    fn exists(&self, path: &str) -> bool {
        self.0.contains_key(Path::new(path))
    }

    fn is_dir(&self, path: &str) -> bool {
        self.0.get(Path::new(path)) == Some(&true)
    }

    fn list(&self, path: &str, alloc: Global) -> Result<OwnVec<(OwnString<Global>, bool), Global>, Self::Error> {
        let path = Path::new(path);
        if self.0.get(path) != Some(&true) {
            return Err(std::io::Error::from(std::io::ErrorKind::NotFound));
        }
        let mut out = OwnVec::new_in(alloc);
        for (child, dir) in &self.0 {
            if child.parent() == Some(path) {
                let name = child.file_name().unwrap().to_str().unwrap();
                out.push((OwnString::from_str_in(name, alloc), *dir));
            }
        }
        Ok(out)
    }
}

fn walked(tree: &Mem, pattern: &str) -> Vec<String> {
    glob_in(tree, pattern, Global).unwrap().into_iter().map(|it| it.unwrap().as_str().to_string()).collect()
}

#[test]
fn a_walk_is_glob_s() {
    let tree = Mem::of(&["/w/m/a/Cargo.toml", "/w/m/a/src/", "/w/m/b/", "/w/m/.c/", "/w/m/file", "/w/n/"]);
    let cases: &[(&str, &[&str])] = &[
        ("/w/m/*", &["/w/m/.c", "/w/m/a", "/w/m/b", "/w/m/file"]),
        ("/w/m/*/", &["/w/m/.c", "/w/m/a", "/w/m/b"]),
        // A trailing `**` is the directories under the base: neither it nor a file.
        ("/w/m/**", &["/w/m/.c", "/w/m/a", "/w/m/a/src", "/w/m/b"]),
        ("/w/m/**/Cargo.toml", &["/w/m/a/Cargo.toml"]),
        ("/w/[mn]", &["/w/m", "/w/n"]),
        ("/w/m/a", &["/w/m/a"]),
        ("/w/m/zz", &[]),
        // `.` and `..` are added after the listing, so they come first, `..` before `.`.
        ("/w/m/.*", &["/w/m/..", "/w/m/.", "/w/m/.c"]),
    ];
    for &(pattern, want) in cases {
        assert_eq!(walked(&tree, pattern), want, "{pattern}");
    }
}
