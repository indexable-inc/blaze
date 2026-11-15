//! A test file of rustc's test tree, read: its `//@` directives and its `//~` annotations.
//! Pure: text in, values out; nothing here knows what blaze does with a directive (`plan.rs`).
//!
//! The syntax is compiletest's (`src/tools/compiletest/src/directives/line.rs` and `errors.rs`
//! at the pinned commit): a directive is a line whose first non-blank text is `//@`, then an
//! optional `[revision]`, then a name that ends at the first `:` or space; what follows a `:`
//! is its value, what follows a space a remark. An annotation is `//~` anywhere on a line, with
//! an optional `[rev1,rev2]` between the `//` and the `~`, then where it points (`^` lines up,
//! `v` lines down, `|` the line of the annotation before, `?` no line), a kind and a message.

use blaze_script::regex::Regex;

/// One `//@` line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Directive {
    /// The line it is on, from 1.
    pub line: usize,
    /// `[revision]`: the one revision it holds for.
    pub revision: Option<String>,
    pub name: String,
    /// What follows the `:`, trimmed; empty where there is none.
    pub value: String,
}

/// What an annotation expects of a diagnostic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Level {
    Error,
    Warning,
    Note,
    Help,
    Suggestion,
    Raw,
}

impl Level {
    fn of(word: &str) -> Option<Level> {
        Some(match word {
            "ERROR" | "error" => Level::Error,
            "WARN" | "WARNING" | "warn" | "warning" => Level::Warning,
            "NOTE" | "note" => Level::Note,
            "HELP" | "help" => Level::Help,
            "SUGGESTION" => Level::Suggestion,
            "RAW" => Level::Raw,
            _ => return None,
        })
    }

    pub(crate) fn word(self) -> &'static str {
        match self {
            Level::Error => "ERROR",
            Level::Warning => "WARN",
            Level::Note => "NOTE",
            Level::Help => "HELP",
            Level::Suggestion => "SUGGESTION",
            Level::Raw => "RAW",
        }
    }
}

/// One `//~` annotation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Annotation {
    /// The line the diagnostic is expected on; `None` for `//~?` (any line, or none).
    pub line: Option<usize>,
    /// The revisions it holds for; empty: every revision.
    pub revisions: Vec<String>,
    pub level: Level,
    /// The error code the annotation's text carries (`E0308`), if it carries one.
    pub code: Option<String>,
    /// rustc's wording, kept for a report and never compared.
    pub text: String,
}

impl Annotation {
    pub(crate) fn holds_for(&self, revision: Option<&str>) -> bool {
        self.revisions.is_empty() || revision.is_some_and(|it| self.revisions.iter().any(|r| r == it))
    }
}

/// What a file says.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct Parsed {
    pub directives: Vec<Directive>,
    pub annotations: Vec<Annotation>,
    /// Lines that look like a directive or an annotation and are not one, each with why: a
    /// test with any is not run (its expectation cannot be read).
    pub malformed: Vec<String>,
}

impl Parsed {
    /// The directives that hold for `revision`: those with no `[revision]`, and its own.
    pub(crate) fn of<'a>(&'a self, revision: Option<&'a str>) -> impl Iterator<Item = &'a Directive> {
        self.directives.iter().filter(move |it| it.revision.is_none() || it.revision.as_deref() == revision)
    }

    /// The values of every `name` directive that holds for `revision`.
    pub(crate) fn values<'a>(&'a self, revision: Option<&'a str>, name: &'a str) -> impl Iterator<Item = &'a str> {
        self.of(revision).filter(move |it| it.name == name).map(|it| it.value.as_str())
    }

    /// `//@ revisions: a b`: the names, in order; empty for a test without revisions.
    pub(crate) fn revisions(&self) -> Vec<String> {
        self.directives
            .iter()
            .filter(|it| it.name == "revisions" && it.revision.is_none())
            .flat_map(|it| it.value.split_whitespace().map(str::to_owned))
            .collect()
    }
}

/// `source`'s directives and annotations.
pub(crate) fn parse(source: &str) -> Parsed {
    let annotation = Regex::new(r"//(?:\[(?P<revs>[\w\-,]+)])?~(?P<adjust>\?|\||[v\^]*)").expect("a regex that compiles");
    let code = Regex::new(r"\bE\d{4}\b").expect("a regex that compiles");
    let mut out = Parsed::default();
    // The line of the last annotation that was not a `//~|`.
    let mut last: Option<Option<usize>> = None;
    for (index, text) in source.lines().enumerate() {
        let line = index + 1;
        if let Some(rest) = text.trim_start().strip_prefix("//@") {
            match directive(line, rest) {
                Ok(it) => out.directives.push(it),
                Err(why) => out.malformed.push(format!("line {line}: {why}")),
            }
            continue;
        }
        let Some(found) = annotation.captures(text) else { continue };
        let tag = found.get(0).expect("the whole match");
        let rest = text[tag.end()..].trim_start();
        let word_end = rest.find(|c: char| c != '_' && !c.is_ascii_alphabetic()).unwrap_or(rest.len());
        let Some(level) = Level::of(&rest[..word_end]) else {
            // compiletest reads an annotation with no kind as one of an unknown kind and fails
            // the test for it; a `//~` in ordinary text (a string, a comment about annotations)
            // is the same line to both.
            out.malformed.push(format!("line {line}: an annotation with no kind (`{}`)", cut(text.trim(), 60)));
            continue;
        };
        let message = rest[word_end..].trim_start_matches(':').trim();
        let adjust = &found["adjust"];
        let target = match adjust {
            "|" => match last {
                Some(it) => it,
                None => {
                    out.malformed.push(format!("line {line}: `//~|` with no annotation before it"));
                    continue;
                }
            },
            "?" => None,
            _ if adjust.starts_with('v') => Some(line + adjust.len()),
            _ => match line.checked_sub(adjust.len()) {
                Some(it) if it > 0 => Some(it),
                _ => {
                    out.malformed.push(format!("line {line}: an annotation that points above the file"));
                    continue;
                }
            },
        };
        if adjust != "|" {
            last = Some(target);
        }
        out.annotations.push(Annotation {
            line: target,
            revisions: found.name("revs").map(|it| it.as_str().split(',').map(str::to_owned).collect()).unwrap_or_default(),
            level,
            code: code.find(message).map(|it| it.as_str().to_owned()),
            text: message.to_owned(),
        });
    }
    out
}

/// The directive of what follows a line's `//@`.
fn directive(line: usize, after: &str) -> Result<Directive, String> {
    let after = after.trim_start();
    let (revision, raw) = match after.strip_prefix('[') {
        Some(inner) => {
            let (revision, rest) = inner.split_once(']').ok_or("a `[` with no `]`")?;
            if revision.contains(',') {
                return Err(format!("`[{revision}]` names more than one revision"));
            }
            (Some(revision.to_owned()), rest.trim_start())
        }
        None => (None, after),
    };
    let name = raw.split([':', ' ']).next().unwrap_or("");
    if name.is_empty() {
        return Err("a directive with no name".to_owned());
    }
    let value = raw[name.len()..].strip_prefix(':').map_or("", str::trim);
    Ok(Directive { line, revision, name: name.to_owned(), value: value.to_owned() })
}

/// The first `most` characters of `text`.
pub(crate) fn cut(text: &str, most: usize) -> &str {
    match text.char_indices().nth(most) {
        Some((at, _)) => &text[..at],
        None => text,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn note(line: Option<usize>, revisions: &[&str], level: Level, code: Option<&str>, text: &str) -> Annotation {
        Annotation {
            line,
            revisions: revisions.iter().map(|it| (*it).to_owned()).collect(),
            level,
            code: code.map(str::to_owned),
            text: text.to_owned(),
        }
    }

    #[test]
    fn directives_are_read_with_their_revision_name_and_value() {
        let parsed = parse(
            "//@ check-pass\n//@ edition: 2021\n  //@[next] compile-flags: -Znext-solver --cfg a\n//@ [old] run-pass\n\
             //@ needs-unwind the test catches a panic\n//@revisions: old next\nfn main() {} //@ not-one\n",
        );
        let said: Vec<(usize, Option<&str>, &str, &str)> =
            parsed.directives.iter().map(|it| (it.line, it.revision.as_deref(), it.name.as_str(), it.value.as_str())).collect();
        assert_eq!(
            said,
            [
                (1, None, "check-pass", ""),
                (2, None, "edition", "2021"),
                (3, Some("next"), "compile-flags", "-Znext-solver --cfg a"),
                (4, Some("old"), "run-pass", ""),
                (5, None, "needs-unwind", ""),
                (6, None, "revisions", "old next"),
            ]
        );
        assert_eq!(parsed.revisions(), ["old", "next"]);
        assert_eq!(parsed.values(Some("next"), "compile-flags").collect::<Vec<_>>(), ["-Znext-solver --cfg a"]);
        assert_eq!(parsed.values(Some("old"), "compile-flags").count(), 0);
        assert!(parsed.malformed.is_empty());
    }

    #[test]
    fn annotations_point_where_compiletest_says() {
        let parsed = parse(
            "fn f() { let x: u8 = \"\"; } //~ ERROR mismatched types [E0308]\n\
             fn g() {}\n\
             //~^ WARN unused\n\
             //~| NOTE on by default\n\
             //~^^^ ERROR E0599\n\
             //~v HELP below\n\
             fn h() {}\n\
             //~? ERROR aborting\n\
             //[a,b]~^^ ERROR for two revisions\n\
             //~vv SUGGESTION x\n",
        );
        assert_eq!(
            parsed.annotations,
            [
                note(Some(1), &[], Level::Error, Some("E0308"), "mismatched types [E0308]"),
                note(Some(2), &[], Level::Warning, None, "unused"),
                note(Some(2), &[], Level::Note, None, "on by default"),
                note(Some(2), &[], Level::Error, Some("E0599"), "E0599"),
                note(Some(7), &[], Level::Help, None, "below"),
                note(None, &[], Level::Error, None, "aborting"),
                note(Some(7), &["a", "b"], Level::Error, None, "for two revisions"),
                note(Some(12), &[], Level::Suggestion, None, "x"),
            ]
        );
        assert!(parsed.malformed.is_empty());
        assert!(parsed.annotations[6].holds_for(Some("b")));
        assert!(!parsed.annotations[6].holds_for(Some("c")));
        assert!(!parsed.annotations[6].holds_for(None));
        assert!(parsed.annotations[0].holds_for(Some("c")));
    }

    #[test]
    fn what_cannot_be_read_is_listed_not_dropped() {
        let parsed = parse("//~| ERROR first\n//@ [a,b] check-pass\n//@ [a check-pass\nlet s = \"//~ \";\n//~^^^^^^ ERROR up\n//@\n");
        assert!(parsed.directives.is_empty() && parsed.annotations.is_empty(), "{parsed:?}");
        assert_eq!(parsed.malformed.len(), 6, "{:?}", parsed.malformed);
    }
}
