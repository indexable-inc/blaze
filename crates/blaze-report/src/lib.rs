//! Everything blaze says, as data (docs/v2/plan/design/structured-output.md): a [`Report`] is one
//! thing blaze tells a person or an agent (an error, a warning, a note), with no text made
//! ahead of time. Its parts are typed ([`Span`]: a package, a path, a hash, a piece of code),
//! so one renderer ([`render`]) decides how each looks on this terminal, in a CI log or a pipe,
//! and an agent reads the same value as JSON ([`json`]) or as the API's CBOR.
//!
//! The layout a terminal gets, for a build script that could not find LLVM:
//!
//! ```text
//! error: llvm-sys can't find LLVM  [llvm: pin]
//!   ┌ build script of llvm-sys 231.0.1  (vendor/llvm-sys)
//!   │ needs LLVM 23.1.4 (e152d9fa), pinned in crates/hir-ty-codegen/llvm.json
//!   │ LLVM_SYS_231_PREFIX is not set
//!   └ blocks 247 packages, including blaze-engine
//!
//!   fix   nix develop
//!    or   python3 .github/scripts/llvm.py '{"command": "fetch", "target": "aarch64-apple-darwin", "dir": "~/.blaze/llvm"}'
//!         export LLVM_SYS_231_PREFIX=~/.blaze/llvm
//!
//!   more  blaze explain llvm-sys
//! ```
//!
//! - the headline says the cause in plain words, and the rule that refused it in brackets
//!   (`blaze explain <rule>` prints its long text, [`rules`]);
//! - the box says where ([`Report::at`]), what was expected and found ([`Report::facts`]), and
//!   what it holds back ([`Report::blocks`]: a count and at most three names, the full list in
//!   the data and in `blaze explain <package>`);
//! - each fix is commands to paste, filled in for this machine;
//! - paths are shown from the workspace or `~`, hashes cut to 8 characters; the data keeps
//!   them whole.
//!
//! Every struct declares its fields in the order of their keys' encoding (shorter names first,
//! then bytewise), as blaze-api's types do, so the API's deterministic CBOR writes each as it
//! comes.
//!
//! - [`render`]: the renderer, a pure function of a report, the terminal's [`Caps`] and the
//!   [`Roots`] paths are shown from.
//! - [`rules`]: every rule a report can name, with its long text.
//! - [`script`]: what a build script says of its own failure (`cargo:blaze-report=` lines).
//! - [`terminal`]: what this terminal can do (colour, hyperlinks, a progress bar,
//!   notifications) from its environment and `[terminal]` of blaze's `config.toml`, and the
//!   escape sequences for each.
//!
//! Rendering is for error paths and the end of a command: its allocations are once per report
//! said, from the global heap.

#![forbid(unsafe_code)]

mod render;
pub mod rules;
pub mod script;
pub mod terminal;
#[cfg(test)]
mod tests;

pub use render::{Caps, Roots, json, json_message, render, render_into};

use serde::{Deserialize, Serialize};

/// How bad it is.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Severity {
    Error,
    Warning,
    Note,
}

impl Severity {
    /// The word a line starts with.
    pub fn label(self) -> &'static str {
        match self {
            Severity::Error => "error",
            Severity::Warning => "warning",
            Severity::Note => "note",
        }
    }
}

/// One thing blaze says. Build it with [`Report::error`] (or [`Report::warning`],
/// [`Report::note`]) and the builder methods; [`Report::text`] is the one constructor for
/// words not yet given their structure, which the plan's inventory counts down.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct Report {
    /// What it is about, as precisely as known: `build script of llvm-sys 231.0.1
    /// (vendor/llvm-sys)`, a file, a query.
    pub at: Option<Text>,
    /// A file holding all that was said (a build script's output, a worker's log), absolute.
    pub log: Option<String>,
    /// The rule that refused it ([`rules`]): `sandbox:link-dir`, `llvm:pin`.
    pub rule: Option<String>,
    /// What was expected and what was found, one line each.
    pub facts: Vec<Fact>,
    /// Ways out, each an alternative: the first is the one to try.
    pub fixes: Vec<Fix>,
    /// What it held back.
    pub blocks: Option<Blocks>,
    /// What it failed because of, outermost first.
    pub causes: Vec<Report>,
    /// The cause in plain words.
    pub headline: Text,
    pub severity: Severity,
}

impl Report {
    pub fn new(severity: Severity, headline: Text) -> Report {
        Report {
            at: None,
            log: None,
            rule: None,
            facts: Vec::new(),
            fixes: Vec::new(),
            blocks: None,
            causes: Vec::new(),
            headline,
            severity,
        }
    }

    pub fn error(headline: Text) -> Report {
        Report::new(Severity::Error, headline)
    }

    pub fn warning(headline: Text) -> Report {
        Report::new(Severity::Warning, headline)
    }

    pub fn note(headline: Text) -> Report {
        Report::new(Severity::Note, headline)
    }

    /// An error that is words only: the headline `what` as it is (lines and all), `at` as
    /// plain words. For a caller not yet ported to a structured report; each such call is
    /// counted in docs/v2/plan/design/structured-output.md's inventory.
    pub fn text(what: impl Into<String>, at: Option<String>) -> Report {
        let mut report = Report::error(Text::plain(what));
        report.at = at.map(Text::plain);
        report
    }

    /// Whether it is words only, as [`Report::text`] makes: no rule, fact, fix, block, log
    /// or cause, and a headline of plain words.
    pub fn is_text(&self) -> bool {
        self.rule.is_none()
            && self.facts.is_empty()
            && self.fixes.is_empty()
            && self.blocks.is_none()
            && self.causes.is_empty()
            && self.log.is_none()
            && self.headline.is_plain()
            && self.at.as_ref().is_none_or(Text::is_plain)
    }

    pub fn at(mut self, at: Text) -> Report {
        self.at = Some(at);
        self
    }

    pub fn rule(mut self, rule: &str) -> Report {
        self.rule = Some(rule.to_owned()); // alloc: a report's, once
        self
    }

    pub fn fact(mut self, key: &str, text: Text) -> Report {
        self.facts.push(Fact { key: key.to_owned(), text }); // alloc: as above
        self
    }

    pub fn fix(mut self, fix: Fix) -> Report {
        self.fixes.push(fix);
        self
    }

    pub fn log(mut self, path: &str) -> Report {
        self.log = Some(path.to_owned()); // alloc: as above
        self
    }

    pub fn blocks(mut self, blocks: Blocks) -> Report {
        self.blocks = Some(blocks);
        self
    }

    pub fn cause(mut self, cause: Report) -> Report {
        self.causes.push(cause);
        self
    }

    /// The name of the first package [`Report::at`] names: what `blaze explain <package>` finds
    /// the report by.
    pub fn subject(&self) -> Option<&str> {
        self.at.as_ref()?.0.iter().find_map(|span| match span {
            Span::Package { name, .. } => Some(name.as_str()),
            _ => None,
        })
    }

    /// The report as words on lines with nothing styled and nothing shortened: the headline,
    /// then each fact, block, fix, log and cause on a line of its own, indented. For a caller
    /// that still puts an error into a sentence of its own ([`Report::text`]'s give back their
    /// words exactly).
    pub fn flat(&self) -> String {
        let mut out = String::new(); // alloc: a report's words, once
        self.flat_into(&mut out);
        out
    }

    /// [`Report::flat`] at the end of `out`.
    pub fn flat_into(&self, out: &mut String) {
        self.headline.flat_into(out);
        for fact in &self.facts {
            out.push_str("\n  ");
            fact.text.flat_into(out);
        }
        if let Some(blocks) = &self.blocks {
            out.push_str("\n  ");
            blocks.text().flat_into(out);
        }
        for fix in &self.fixes {
            if let Some(note) = &fix.note {
                out.push_str("\n  fix: ");
                note.flat_into(out);
            }
            for line in &fix.lines {
                out.push_str("\n  fix: ");
                out.push_str(line);
            }
        }
        if let Some(log) = &self.log {
            out.push_str("\n  log: ");
            out.push_str(log);
        }
        for cause in &self.causes {
            out.push_str("\n  caused by: ");
            cause.flat_into(out);
        }
    }
}

/// Words with their parts typed. Never formatted ahead: the renderer styles each [`Span`].
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug, Default)]
pub struct Text(pub Vec<Span>);

impl Text {
    pub fn new() -> Text {
        Text(Vec::new())
    }

    pub fn plain(words: impl Into<String>) -> Text {
        Text(vec![Span::Plain(words.into())]) // alloc: a report's, once
    }

    /// `words` with each part in backticks a [`Span::Code`]: how a build script or another
    /// program that only prints text marks code. A backtick left open is a word.
    pub fn marked(words: &str) -> Text {
        let mut text = Text::new();
        let parts = words.split('`').count();
        for (at, part) in words.split('`').enumerate() {
            let code = at % 2 == 1;
            // An odd backtick count leaves the last part open: it is words, its backtick too.
            if code && at + 1 == parts {
                text.0.push(Span::Plain(format!("`{part}"))); // alloc: as above
            } else if code {
                text.0.push(Span::Code(part.to_owned())); // alloc: as above
            } else if !part.is_empty() {
                text.0.push(Span::Plain(part.to_owned())); // alloc: as above
            }
        }
        text
    }

    pub fn push(&mut self, span: Span) {
        self.0.push(span);
    }

    pub fn words(mut self, words: &str) -> Text {
        self.0.push(Span::Plain(words.to_owned())); // alloc: as above
        self
    }

    pub fn code(mut self, code: &str) -> Text {
        self.0.push(Span::Code(code.to_owned())); // alloc: as above
        self
    }

    pub fn path(mut self, path: &str) -> Text {
        self.0.push(Span::Path(path.to_owned())); // alloc: as above
        self
    }

    pub fn var(mut self, name: &str) -> Text {
        self.0.push(Span::Var(name.to_owned())); // alloc: as above
        self
    }

    pub fn hash(mut self, hash: &str) -> Text {
        self.0.push(Span::Hash(hash.to_owned())); // alloc: as above
        self
    }

    pub fn version(mut self, version: &str) -> Text {
        self.0.push(Span::Version(version.to_owned())); // alloc: as above
        self
    }

    pub fn count(mut self, count: u64) -> Text {
        self.0.push(Span::Count(count));
        self
    }

    /// A package by name, and its version unless it is a workspace member's (`None`).
    pub fn package(mut self, name: &str, version: Option<&str>) -> Text {
        // alloc: as above
        self.0.push(Span::Package { name: name.to_owned(), version: version.map(str::to_owned) });
        self
    }

    /// Only [`Span::Plain`] parts.
    pub fn is_plain(&self) -> bool {
        self.0.iter().all(|span| matches!(span, Span::Plain(_)))
    }

    pub fn flat(&self) -> String {
        let mut out = String::new(); // alloc: a report's words, once
        self.flat_into(&mut out);
        out
    }

    /// The words with nothing styled and nothing shortened: code in backticks, paths and
    /// hashes whole.
    pub fn flat_into(&self, out: &mut String) {
        for span in &self.0 {
            match span {
                Span::Plain(words) => out.push_str(words),
                Span::Code(code) | Span::Var(code) => {
                    out.push('`');
                    out.push_str(code);
                    out.push('`');
                }
                Span::Path(it) | Span::Version(it) | Span::Hash(it) => out.push_str(it),
                Span::Package { name, version } => {
                    out.push_str(name);
                    if let Some(version) = version {
                        out.push(' ');
                        out.push_str(version);
                    }
                }
                Span::Count(count) => push_number(out, *count),
                Span::Took(ns) => render::took_into(out, *ns),
            }
        }
    }
}

/// `n` in decimal at the end of `out`.
fn push_number(out: &mut String, n: u64) {
    use std::fmt::Write as _;
    // Writing to a String cannot fail.
    let _ = write!(out, "{n}");
}

/// One typed part of a [`Text`].
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub enum Span {
    /// Words, in the terminal's own colour.
    Plain(String),
    /// Code, a command or a manifest line: a code span on a terminal, backticks elsewhere.
    Code(String),
    /// A file or directory, absolute where known: shown from the workspace or `~`, a link
    /// where the terminal has them.
    Path(String),
    /// A package: its name, bold, and its version, dim; `None` for a workspace member's.
    Package {
        name: String,
        version: Option<String>,
    },
    Version(String),
    /// A content hash or commit, whole: shown as its first 8 characters.
    Hash(String),
    /// An environment variable's name, shown as code.
    Var(String),
    Count(u64),
    /// A time, in nanoseconds: `0.12s`, `38s`, `1m12s`.
    Took(u64),
}

/// One line of what was expected or found. `key` names it for data (`needs`, `found`,
/// `stderr`); the line is `text` alone, which reads on its own.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct Fact {
    pub key: String,
    pub text: Text,
}

/// One way out: commands to paste, in order (`lines`, each one shell line, filled in for this
/// machine), and what to do where no command does it (`note`: a manifest to edit).
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct Fix {
    pub note: Option<Text>,
    pub lines: Vec<String>,
}

impl Fix {
    /// Commands to paste.
    pub fn run(lines: &[&str]) -> Fix {
        Fix { note: None, lines: lines.iter().map(|it| (*it).to_owned()).collect() } // alloc: a report's, once
    }

    /// What to do where no command does it.
    pub fn note(note: Text) -> Fix {
        Fix { note: Some(note), lines: Vec::new() }
    }
}

/// What a failure held back: `count` of them (`what`: `packages`, `units`, `tests`), every
/// one in `all` where the producer knows them, and `notable`, the at most three a person
/// should see named (the one that matters first).
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct Blocks {
    pub all: Vec<String>,
    pub what: String,
    pub count: u64,
    pub notable: Vec<String>,
}

/// The most names a rendering lists; the rest are a count, and `blaze explain`'s.
pub const NAMED: usize = 3;

impl Blocks {
    /// `blocks 247 packages, including blaze-engine`.
    pub fn text(&self) -> Text {
        let mut text = Text::new().words("blocks ").count(self.count).words(" ").words(&self.what);
        for (at, name) in self.notable.iter().take(NAMED).enumerate() {
            text = text.words(if at == 0 { ", including " } else { ", " }).package(name, None);
        }
        text
    }

    /// Whether a rendering leaves some of them unnamed.
    pub fn more(&self) -> bool {
        self.count > self.notable.len().min(NAMED) as u64
    }
}
