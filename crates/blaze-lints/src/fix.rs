//! Every machine-applicable suggestion of a crate's findings made in its files. Each
//! pass splices the fixes into the front end's trees (reparsing only what they touched), sets the
//! texts and their trees in the database, and lints again, which recomputes only the bodies the
//! fixes changed. It ends when no finding has a fix left, or fails naming what it could not do;
//! fixes that leave the crate with an error it did not have are refused, and the texts restored.

use std::collections::{BTreeMap, BTreeSet};

use base_db::{Db, EditionedFileId, FileId, FileText};
use blaze_diagnostics::model::{Applicability, Code, Diag, Level, Span};
use blaze_intent::file::Working;
use syntax::{TextRange, TextSize};

use crate::{
    check::{self, NameOf, Setup},
    diag,
};

/// The passes a fix gets: a fix whose text another lint then rewrites settles in the second, and
/// one that keeps making new findings is the error [`FixError::Unsettled`].
const PASSES: u32 = 4;

/// What [`fix`] changed: each file's new text, in the order of the files' names.
#[derive(Debug, PartialEq, Eq)]
pub struct Fixed {
    pub files: Vec<(FileId, String)>,
    /// The suggestions made.
    pub applied: usize,
    /// The passes it took, the last of which found nothing left to fix.
    pub passes: u32,
}

#[derive(Debug, PartialEq, Eq)]
pub enum FixError {
    Unnamed(diag::Unnamed),
    /// The fixes left the crate with an error it did not have; the texts are as they were.
    Broke { at: String, error: String, by: Vec<String> },
    /// Fixes still rewriting the same text after the last pass.
    Conflict { at: String, first: String, second: String },
    /// Findings with a fix still there after the last pass.
    Unsettled { at: String, lint: String, message: String },
    /// A fix the file's text refuses (outside it, or splitting a character).
    Splice { at: String, lint: String, why: String },
}

impl std::fmt::Display for FixError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FixError::Unnamed(it) => it.fmt(f),
            FixError::Broke { at, error, by } => {
                write!(f, "{at}: the fixes of {} leave the error `{error}`, so none is made", by.join(", "))
            }
            FixError::Conflict { at, first, second } => {
                write!(f, "{at}: the fixes of `{first}` and `{second}` rewrite the same text after {PASSES} passes")
            }
            FixError::Unsettled { at, lint, message } => {
                write!(f, "{at}: `{lint}` still has a fix for `{message}` after {PASSES} passes")
            }
            FixError::Splice { at, lint, why } => write!(f, "{at}: the fix of `{lint}` can't be made: {why}"),
        }
    }
}

impl std::error::Error for FixError {}

impl From<diag::Unnamed> for FixError {
    fn from(it: diag::Unnamed) -> FixError {
        FixError::Unnamed(it)
    }
}

/// One finding's fix: its parts, disjoint, in the order of the text.
struct Planned<'f> {
    lint: &'f str,
    at: &'f Span,
    parts: Vec<(TextRange, &'f str)>,
}

/// Makes the fixes of `krate`'s findings in its files, setting the new texts in `db`.
pub fn fix(db: &mut Db, krate: hir::Crate, setup: &Setup, name_of: NameOf<'_>) -> Result<Fixed, FixError> {
    let files = files_of(db, krate, name_of);
    let before = errors(db, krate, &files);
    let originals: Vec<(FileId, String)> = files.values().map(|&(file, _)| (file, db.file_text(file).text(db).to_string())).collect();
    let mut by: BTreeSet<String> = BTreeSet::new();
    let mut changed: BTreeMap<FileId, String> = BTreeMap::new();
    let mut applied = 0;
    let mut conflict = None;
    for pass in 1..=PASSES {
        conflict = None;
        let found = check::check(db, krate, setup, name_of)?;
        let mut planned: BTreeMap<FileId, Vec<Planned<'_>>> = BTreeMap::new();
        for found in &found {
            let diagnostic = &found.diagnostic;
            let lint = diagnostic.lint_name().unwrap_or("");
            if let Some(parts) = machine_fix(diagnostic)
                && let Some(at) = diagnostic.span.primary.first()
                && let Some(&(file, _)) = files.get(at.file.as_str())
            {
                planned.entry(file).or_default().push(Planned { lint, at, parts });
            }
        }
        if planned.is_empty() {
            return settled(db, krate, &files, &before, &originals, by, Fixed { files: changed.into_iter().collect(), applied, passes: pass });
        }
        let edition = krate.edition(db);
        let mut made = Vec::with_capacity(planned.len());
        for (file, mut fixes) in planned {
            fixes.sort_by_key(|it| (it.parts[0].0.start(), it.parts[0].0.end()));
            fixes.dedup_by(|a, b| a.parts == b.parts);
            let mut taken: Vec<&Planned<'_>> = Vec::with_capacity(fixes.len());
            for fix in &fixes {
                let clash = taken.iter().find(|it| overlaps(&it.parts, &fix.parts));
                match clash {
                    Some(it) => conflict = Some((it.at.clone(), it.lint.to_owned(), fix.lint.to_owned())),
                    None => {
                        by.insert(fix.lint.to_owned());
                        taken.push(fix);
                    }
                }
            }
            let id = EditionedFileId::new(db, file, edition);
            let parse = id.parse(db).clone(); // clone: two `Arc` counts, the working copy's
            let text = db.file_text(file);
            let appended = text.appended_root_module(db);
            let name = files.values().find(|(it, _)| *it == file).map_or("", |(_, name)| name.as_str());
            let end = text.text(db).len() - appended.map_or(0, str::len);
            let mut working = Working::new(name, edition, end, parse);
            let mut parts: Vec<(TextRange, &str, &Planned<'_>)> =
                taken.iter().flat_map(|fix| fix.parts.iter().map(move |&(range, text)| (range, text, *fix))).collect();
            parts.sort_by_key(|&(range, ..)| std::cmp::Reverse((range.start(), range.end())));
            for (range, text, fix) in parts {
                working.splice(range, text.to_owned()).map_err(|why| FixError::Splice {
                    at: at_of(db, &files, Some(fix.at)),
                    lint: fix.lint.to_owned(),
                    why,
                })?;
            }
            applied += taken.len();
            made.push((file, appended, working));
        }
        db.batch(|db| {
            for (file, _, working) in &made {
                db.set_file_text(*file, working.bytes());
            }
            for (file, appended, working) in &made {
                if working.parse.errors().is_empty() {
                    let content = FileText::content_of(working.bytes(), *appended);
                    EditionedFileId::new(db, *file, edition).seed_parse(db, content, &working.parse);
                }
            }
        });
        for (file, _, working) in made {
            changed.insert(file, working.bytes().to_owned());
        }
    }
    let found = check::check(db, krate, setup, name_of)?;
    if let Some((at, first, second)) = conflict {
        return Err(FixError::Conflict { at: at_of(db, &files, Some(&at)), first, second });
    }
    let left = found.iter().map(|it| &it.diagnostic).find(|it| machine_fix(it).is_some());
    match left {
        None => settled(db, krate, &files, &before, &originals, by, Fixed { files: changed.into_iter().collect(), applied, passes: PASSES }),
        Some(diagnostic) => Err(FixError::Unsettled {
            at: at_of(db, &files, diagnostic.span.primary.first()),
            lint: diagnostic.lint_name().unwrap_or("").to_owned(),
            message: diagnostic.message.clone(),
        }),
    }
}

/// The first machine-applicable substitution of `diagnostic`, its parts in the order of the text.
fn machine_fix(diagnostic: &Diag) -> Option<Vec<(TextRange, &str)>> {
    if diagnostic.suggestions_disabled {
        return None;
    }
    let substitution = diagnostic
        .suggestions
        .iter()
        .filter(|it| it.applicability == Applicability::MachineApplicable)
        .find_map(|it| it.substitutions.first())?;
    let mut parts: Vec<_> = substitution
        .iter()
        .map(|(span, text)| (TextRange::new(TextSize::new(span.lo), TextSize::new(span.hi)), text.as_str()))
        .collect();
    parts.sort_by_key(|&(range, _)| (range.start(), range.end()));
    (!parts.is_empty()).then_some(parts)
}

/// Whether two fixes touch the same text; two insertions at one place clash too, as their order
/// would be a guess.
fn overlaps(a: &[(TextRange, &str)], b: &[(TextRange, &str)]) -> bool {
    a.iter().any(|&(x, _)| {
        b.iter().any(|&(y, _)| x.start() < y.end() && y.start() < x.end() || x.start() == y.start())
    })
}

/// `fixed`, unless the crate now has an error it did not have before the fixes: then the texts
/// are restored and the error named.
fn settled(
    db: &mut Db,
    krate: hir::Crate,
    files: &BTreeMap<String, (FileId, String)>,
    before: &[(String, Diag)],
    originals: &[(FileId, String)],
    by: BTreeSet<String>,
    fixed: Fixed,
) -> Result<Fixed, FixError> {
    if fixed.files.is_empty() {
        return Ok(fixed);
    }
    let mut had: BTreeMap<&str, usize> = BTreeMap::new();
    for (key, _) in before {
        *had.entry(key.as_str()).or_default() += 1;
    }
    let after = errors(db, krate, files);
    let new = after.iter().find(|(key, _)| match had.get_mut(key.as_str()) {
        Some(count) if *count > 0 => {
            *count -= 1;
            false
        }
        _ => true,
    });
    let Some((_, error)) = new else { return Ok(fixed) };
    let broke = FixError::Broke {
        at: at_of(db, files, error.span.primary.first()),
        error: error.message.clone(),
        by: by.into_iter().collect(),
    };
    db.batch(|db| {
        for (file, text) in originals {
            if fixed.files.iter().any(|(it, _)| it == file) {
                db.set_file_text(*file, text);
            }
        }
    });
    Err(broke)
}

/// The crate's errors as rustc gives them, each keyed by its code and message, which a fix
/// elsewhere in the file does not change.
fn errors(db: &Db, krate: hir::Crate, files: &BTreeMap<String, (FileId, String)>) -> Vec<(String, Diag)> {
    let paths: std::collections::HashMap<FileId, String> = files.values().map(|(file, name)| (*file, name.clone())).collect();
    let options = blaze_diagnostics::check::Options::default();
    blaze_diagnostics::check_crate(db, krate, blaze_diagnostics::Files::new(&paths, ""), &options)
        .into_iter()
        // The closing "aborting due to N previous errors; M warnings emitted" counts the others,
        // so a fix that only changes a warning would read as a new error.
        .filter(|it| it.level == Level::Error && !(it.code.is_none() && it.message.starts_with("aborting due to ")))
        .map(|it| {
            let code = match &it.code {
                Some(Code::Error(code) | Code::Lint(code)) => code.as_str(),
                _ => "",
            };
            (format!("{code} {}", it.message), it)
        })
        .collect()
}

/// The crate's files by the names `name_of` gives them, which are the names its spans carry.
fn files_of(db: &Db, krate: hir::Crate, name_of: NameOf<'_>) -> BTreeMap<String, (FileId, String)> {
    let mut files = BTreeMap::new();
    for module in krate.modules(db) {
        let Some(file) = module.as_source_file_id(db) else { continue };
        let file = file.file_id(db);
        if let Some(name) = name_of(file) {
            files.insert(name.clone(), (file, name));
        }
    }
    files
}

/// `file:line` of `span`, as the text is now.
fn at_of(db: &Db, files: &BTreeMap<String, (FileId, String)>, span: Option<&Span>) -> String {
    let Some(span) = span else { return "the crate".to_owned() };
    let line = files.get(span.file.as_str()).map(|&(file, _)| {
        let text = db.file_text(file);
        let text = text.text(db);
        let lo = (span.lo as usize).min(text.len());
        1 + text.as_bytes()[..lo].iter().filter(|&&it| it == b'\n').count()
    });
    match line {
        Some(line) => format!("{}:{line}", span.file),
        None => span.file.clone(),
    }
}
