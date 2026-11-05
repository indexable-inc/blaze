//! What a build script says of its own failure, beyond cargo's one line of text.
//!
//! A script that fails prints `cargo::error=MESSAGE` as cargo wants, and may also print
//! `cargo:blaze-report=FIELD=TEXT` lines: the single colon is cargo's old metadata form, which
//! cargo keeps as metadata for a package's dependents and never refuses, so the script still
//! builds under cargo, and blaze reads them into the failure's [`crate::Report`]:
//!
//! - `headline=TEXT`: the cause in plain words;
//! - `rule=ID`: the rule ([`crate::rules`]);
//! - `fact.KEY=TEXT`: one line of what was expected or found;
//! - `fix=COMMAND`: a new way out, its first command; `fix+=COMMAND`: its next command;
//!   `note=TEXT`: what to do in the last way out where no command does it.
//!
//! TEXT marks code in backticks ([`crate::Text::marked`]).

use crate::{Fact, Fix, Text};

/// The directive's prefix on a line of a script's stdout.
pub const DIRECTIVE: &str = "cargo:blaze-report=";

/// What a script said in its `cargo:blaze-report=` lines.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct Said {
    pub headline: Option<Text>,
    pub rule: Option<String>,
    pub facts: Vec<Fact>,
    pub fixes: Vec<Fix>,
    /// Each line blaze could not read, whole: a fact of the report, never dropped.
    pub unread: Vec<String>,
}

/// What `stdout`'s `cargo:blaze-report=` lines say, in order; `None` when it has none.
pub fn read(stdout: &str) -> Option<Said> {
    let mut said = None;
    for line in stdout.lines() {
        let line = line.strip_suffix('\r').unwrap_or(line);
        let Some(value) = line.strip_prefix(DIRECTIVE) else { continue };
        let said = said.get_or_insert_with(Said::default);
        let unread = |said: &mut Said| said.unread.push(line.to_owned()); // alloc: a failure's, once
        let Some((field, text)) = value.split_once('=') else {
            unread(said);
            continue;
        };
        match field {
            "headline" => said.headline = Some(Text::marked(text)),
            "rule" => said.rule = Some(text.to_owned()), // alloc: as above
            "fix" => said.fixes.push(Fix { note: None, lines: vec![text.to_owned()] }), // alloc: as above
            "fix+" => match said.fixes.last_mut() {
                Some(fix) => fix.lines.push(text.to_owned()), // alloc: as above
                None => unread(said),
            },
            "note" => match said.fixes.last_mut() {
                Some(fix) => fix.note = Some(Text::marked(text)),
                None => unread(said),
            },
            _ => match field.strip_prefix("fact.") {
                Some(key) => {
                    said.facts.push(Fact { key: key.to_owned(), text: Text::marked(text) })
                } // alloc: as above
                None => unread(said),
            },
        }
    }
    said
}
