//! What a unit said, as its record keeps it ([`Said`]), and the one place it is drawn for a
//! client ([`draw`]).
//!
//! How a diagnostic looks (in colour or not, in how many columns, a line each or whole, how
//! many refused functions its error names: [`Drawing`]) is the client's, and no input of what
//! a unit's compile makes: no key holds it (rule 21) and no record holds text drawn with it.
//! A record keeps each diagnostic as what it says: the check's as the check gave it
//! ([`blaze_diagnostics::Diag`]: its spans, labels, code, notes and suggestions), a line of
//! cargo's or blaze's own as its level and message, a refusal of the code generator as every
//! function it names. A build that made the unit and one that found its record draw them the
//! same way, here, from that record, each for its own client: so a build asked for at another
//! width, in another colour or a line each finds every record and compiles nothing, and what
//! it prints is what a build from nothing at that width prints, byte for byte.
//!
//! A core (rule 14): [`draw`] reads no file. The text of each file the diagnostics point into
//! is given by its caller (`read`: the job's view, whose content the record's key covers), or
//! is in the record itself where no view holds it ([`Said::texts`]).

#![forbid(unsafe_code)]

use blaze_api::{ApiError, Diagnostic, Level, UnitOf};
use blaze_diagnostics::{Diag, emit};
use serde::{Deserialize, Serialize};

use super::{compile, say};

/// How one client is shown diagnostics: what its request says of its terminal and its
/// `--message-format`. Never an input of a unit's compile or of any key.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub(crate) struct Drawing {
    /// In colour ([`blaze_api::Cargo::color`]).
    pub(crate) color: bool,
    /// A line each (`--message-format=short`, `json-diagnostic-short`).
    pub(crate) short: bool,
    /// The columns ([`blaze_api::Cargo::width`]); none is rustc's default.
    pub(crate) width: Option<u32>,
    /// How many refused functions a unit's error names ([`blaze_api::Cargo::refused`]); none
    /// is every one.
    pub(crate) refused: Option<u32>,
}

impl Drawing {
    /// How [`blaze_diagnostics::emit`] writes a diagnostic for this client.
    pub(crate) fn style(self) -> emit::Style {
        let mut style = emit::Style { ansi: self.color, short: self.short, ..Default::default() };
        if let Some(width) = self.width {
            style.width = usize::try_from(width).unwrap_or(usize::MAX);
        }
        style
    }
}

/// One thing a unit said, as its record keeps it: what it says, never how it is drawn.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub(crate) enum Item {
    /// A diagnostic of code, as the check gave it.
    Code(Diag),
    /// A line of cargo's or blaze's own, of no span ([`say::cargo`]).
    Line { level: Level, message: String },
    /// The code generator refused functions the unit reaches: how many, and each one's line
    /// (`name: why`), every one. How many are named is the client's ([`Drawing::refused`]).
    Refused { reached: usize, lines: Vec<String> },
    /// A function of the toolchain the front end failed on: the job's report of it, an error
    /// of no unit saying `message`, then each of the front end's errors of its body.
    Toolchain { message: String, diags: Vec<Diag> },
}

impl Item {
    /// A line at `level` saying `message`.
    pub(crate) fn line(level: Level, message: String) -> Item {
        Item::Line { level, message }
    }

    /// An error of blaze's that is not rustc's: the unit can't be made, and says why.
    pub(crate) fn refused(message: String) -> Item {
        Item::Line { level: Level::Error, message }
    }

    /// Whether it fails its unit.
    pub(crate) fn is_error(&self) -> bool {
        match self {
            Item::Code(diag) => diag.level == blaze_diagnostics::model::Level::Error,
            Item::Line { level, .. } => *level == Level::Error,
            Item::Refused { .. } | Item::Toolchain { .. } => true,
        }
    }
}

/// Each file a record's diagnostics point into, by the name they give it, sorted, with every
/// offset a span of theirs has in it and that offset's line and byte column, sorted by offset
/// ([`compile::Placed`]).
pub(crate) type Places = Vec<(String, Vec<(u32, u32, u32)>)>;

/// Everything a unit said, as its record keeps it: in the order it is shown.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug, Default)]
pub(crate) struct Said {
    pub(crate) items: Vec<Item>,
    /// The text of each file they point into that is not a file of the view at its path (a
    /// build script's output, a crate root with a module appended, a file of the toolchain's
    /// the database alone has), by the name they give it, each once: what [`draw`] reads in
    /// place of the view's.
    pub(crate) texts: Vec<(String, String)>,
    /// For a record under its narrow key, whose key holds no file's bytes: where each end of
    /// each span is by line and column, which every text of that key has it at. [`draw`]
    /// finds the spans' offsets in the files as they are before it draws
    /// ([`compile::moved`]). Only where every item is [`Item::Code`].
    pub(crate) places: Option<Places>,
}

impl Said {
    /// `text` as the text of the file the diagnostics name `name`, unless one is kept.
    pub(crate) fn text(&mut self, name: &str, text: &str) {
        if !self.texts.iter().any(|it| it.0 == name) {
            // alloc: a file no view holds, once per unit whose diagnostics point into one
            self.texts.push((name.to_owned(), text.to_owned()));
        }
    }

    /// Whether any of it fails the unit.
    pub(crate) fn failed(&self) -> bool {
        self.items.iter().any(Item::is_error)
    }

    /// The diagnostics of code in it, in order: the check's.
    pub(crate) fn code(&self) -> impl Iterator<Item = &Diag> {
        self.items.iter().filter_map(|it| match it {
            Item::Code(diag) => Some(diag),
            _ => None,
        })
    }
}

/// The unit [`draw`] draws the diagnostics of.
pub(crate) struct Of<'a> {
    /// Its index in the job's graph, for an error to name.
    pub(crate) unit: usize,
    /// Its target's name, which an error is of.
    pub(crate) target: &'a str,
    /// As a refusal names it ([`compile::span_unit`]).
    pub(crate) named: &'a str,
    /// As the API names it.
    pub(crate) of: &'a UnitOf,
}

/// `said` as `drawing`'s client is shown it: each item as the API gives a diagnostic, its
/// text drawn in that client's colour, columns and format, in order. `read` gives the text of
/// a file of the view by the name the diagnostics give it, as the view has it now; a file
/// `said` keeps the text of is never asked of it. The one place a unit's diagnostics are
/// drawn: a unit just made and one found in the store alike.
///
/// A span that can't be placed in its file (the file unread, or shorter than the span) is an
/// error naming the diagnostic and the file, and nothing is drawn (rule 3).
pub(crate) fn draw(
    said: &Said,
    read: &dyn Fn(&str) -> Option<String>,
    of: &Of<'_>,
    drawing: Drawing,
) -> Result<Vec<Diagnostic>, ApiError> {
    if said.items.is_empty() {
        return Ok(Vec::new()); // alloc: empty, allocates nothing
    }
    // clone: the diagnostics to draw, once per unit that has any
    let mut code: Vec<Diag> = said.code().cloned().collect();
    if let Some(places) = &said.places {
        code = compile::moved(read, of.unit, places, code)?;
    }
    let toolchain = said.items.iter().filter_map(|it| match it {
        Item::Toolchain { diags, .. } => Some(diags.as_slice()),
        _ => None,
    });
    let mut sources = emit::Sources::default();
    for (name, text) in &said.texts {
        // clone: a file no view holds, once per unit whose diagnostics point into one
        sources.add(name, Some(text.clone()));
    }
    for name in sources.unread(code.iter().chain(toolchain.flatten())) {
        // A name of a text that is not the file at its path is never the view's file.
        let text = match emit::names_text(name) {
            true => None,
            false => read(name),
        };
        sources.add(name, text);
    }
    let style = drawing.style();
    // clone: the unit's name, on the error path
    let failed = |why: String| ApiError::new(why, Some(of.target.to_owned()));
    let mut code = code.iter();
    let mut out = Vec::with_capacity(said.items.len()); // alloc: what the unit says, once
    for item in &said.items {
        out.push(match item {
            Item::Code(_) => {
                let Some(diag) = code.next() else {
                    return Err(failed(format!(
                        "unit {}: a diagnostic of its record was lost as its spans were placed",
                        of.unit
                    )));
                };
                // clone: the unit as the API names it, once per diagnostic
                blaze_diagnostics::api::diagnostic(diag, &sources, style, Some(of.of.clone()))
                    // alloc: error path, naming the diagnostic whose span can't be placed
                    .map_err(|why| {
                        failed(format!(
                            "{why}, in the diagnostic `{}`",
                            diag.message.lines().next().unwrap_or_default()
                        ))
                    })?
            }
            Item::Line { level, message } => {
                // clone: the API's owned message, once per line
                say::cargo(*level, message.clone(), Some(of.of.clone()), drawing.color)
            }
            Item::Refused { reached, lines } => {
                let what = crate::refused_error(
                    of.named,
                    *reached,
                    lines.iter().cloned(), // clone: a line each one named
                    drawing.refused,
                );
                say::refused(what, Some(of.of.clone()), drawing.color)
            }
            Item::Toolchain { message, diags } => {
                // clone: the API's owned message, once per report
                let mut report = say::refused(message.clone(), None, drawing.color);
                for diag in diags {
                    let it = blaze_diagnostics::api::diagnostic(diag, &sources, style, None)
                        // alloc: the error path
                        .map_err(|why| {
                            failed(format!(
                                "{why}, in the front end's error `{}` that the report `{}` carries",
                                diag.message,
                                message.lines().next().unwrap_or_default()
                            ))
                        })?;
                    report.rendered.push_str(&it.rendered);
                    report.children.push(it);
                }
                report
            }
        });
    }
    Ok(out)
}
