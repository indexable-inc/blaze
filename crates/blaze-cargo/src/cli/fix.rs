//! `blaze check --fix`: every machine-applicable suggestion the check gave (rustc's lints and
//! errors, clippy's, blaze's), or only those of each `--lint NAME`, made in the checkout. The
//! check runs on the checkout's head pinned to one root ([`pin`]); the suggestions are made as
//! ops on that root and checked again as a what-if ([`ViewRef::WhatIf`]), up to [`PASSES`]
//! times as `cargo fix` does, each pass's suggestions taken from the check of the one before.
//! An error the what-if has that the first check did not stops it with nothing written; else
//! the ops are committed to the head and written to the checkout's files as `check --replace`
//! writes its edits ([`super::replace::write`]).
//!
//! A suggestion is made whole or not at all: one with a part that is not machine-applicable,
//! that is outside the checkout (in a dependency or a macro's expansion of one), or that
//! overlaps one made before it in the pass is left, and said as left. blaze reads no version
//! control: a fix that makes a new error writes nothing.

use std::{
    collections::BTreeSet,
    path::{Component, Path},
};

use blaze_api::{Applicability, DiagnosticRef, Level, Request, ResponseRef, Targets, ViewRef};
use blaze_ids::ContentId;
use blaze_vfs::{Op, View};

use super::{
    daemon::{self, Start},
    observe, replace,
};

/// How many times, at most, the suggestions are made and the result checked again.
pub const PASSES: usize = 4;

/// One part of a suggestion: a byte range of a file in the checkout and its replacement.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Part {
    file: String,
    start: u32,
    end: u32,
    text: String,
}

impl Part {
    /// Whether making both would edit the same bytes, or insert at the same place.
    fn overlaps(&self, other: &Part) -> bool {
        self.file == other.file
            && ((self.start < other.end && other.start < self.end) || self.start == other.start)
    }
}

/// What a check said that `--fix` reads: its machine-applicable suggestions, in the order
/// given, and its errors by what they are and where.
#[derive(Default, Debug, PartialEq, Eq)]
pub struct Found {
    /// The `--lint`s whose suggestions are made, by their code; every diagnostic's when empty.
    lints: Vec<String>,
    suggestions: Vec<Vec<Part>>,
    /// Each a suggestion that can't be made: not machine-applicable in part, or outside.
    left: usize,
    /// An error by its code, its first primary span's file and its message: its place moves
    /// as the fixes edit the file, so the bytes are not part of it.
    errors: BTreeSet<(Option<String>, String, String)>,
}

impl Found {
    /// What a check said, its suggestions those of `lints` alone when any is named.
    pub fn new(lints: &[String]) -> Found {
        Found { lints: lints.to_vec(), ..Found::default() } // alloc: CLI edge, the few `--lint`s, once
    }

    /// Whether the suggestions of a diagnostic with `code` are made.
    fn wanted(&self, code: Option<&str>) -> bool {
        self.lints.is_empty() || code.is_some_and(|code| self.lints.iter().any(|it| it == code))
    }

    pub fn add(&mut self, diagnostic: &DiagnosticRef<'_>) {
        if diagnostic.level == Level::Error
            && let Some(at) = diagnostic.spans.iter().find(|it| it.primary)
        {
            // alloc: an error's identity, once per error of the check
            self.errors.insert((diagnostic.code.map(str::to_owned), at.file.to_owned(), diagnostic.message.to_owned()));
        }
        if !self.wanted(diagnostic.code) {
            return;
        }
        for child in diagnostic.children.iter() {
            let replaced = || child.spans.iter().filter(|it| it.replacement.is_some());
            if replaced().next().is_none() {
                continue;
            }
            let whole = replaced().all(|it| it.applicability == Some(Applicability::MachineApplicable) && inside(it.file));
            if !whole {
                self.left += 1;
                continue;
            }
            // alloc: a suggestion's parts, once per suggestion
            let parts = replaced()
                .map(|it| Part {
                    file: it.file.to_owned(),
                    start: it.byte_start,
                    end: it.byte_end,
                    text: it.replacement.unwrap_or_default().to_owned(),
                })
                .collect();
            self.suggestions.push(parts);
        }
    }

    /// The ops that make each suggestion that overlaps none made before it, a suggestion two
    /// units gave (a lib and its test) once; and how many suggestions they make. In each file
    /// the last edit comes first, so each op's bytes are where the check saw them.
    fn ops(&self) -> (Vec<Op>, usize) {
        let mut made: Vec<&Part> = Vec::new();
        let mut seen: BTreeSet<&[Part]> = BTreeSet::new();
        let mut count = 0;
        for suggestion in &self.suggestions {
            if !seen.insert(suggestion.as_slice()) || suggestion.iter().any(|part| made.iter().any(|it| it.overlaps(part))) {
                continue;
            }
            made.extend(suggestion);
            count += 1;
        }
        made.sort_by(|a, b| a.file.cmp(&b.file).then(b.start.cmp(&a.start)));
        let ops = made
            .into_iter()
            .map(|it| Op::Edit {
                at: u64::from(it.start),
                len: u64::from(it.end.saturating_sub(it.start)),
                path: it.file.clone(),
                bytes: it.text.clone().into_bytes(),
            })
            .collect();
        (ops, count)
    }
}

/// Whether `file`, as a diagnostic names it, is a file of the checkout.
fn inside(file: &str) -> bool {
    !file.is_empty() && Path::new(file).components().all(|it| matches!(it, Component::Normal(_)))
}

/// The checkout's head a `--fix` run checks, pinned to the root it is at now, and the check's
/// targets for checking the fixes again.
pub struct Pinned {
    root: ContentId,
    head: String,
    targets: Targets,
}

/// `request`, the check of a checkout's head, turned to the root the head is at now, so the
/// suggestions it gives are of the bytes the fixes are made to.
pub fn pin(request: &mut Request) -> Result<Pinned, String> {
    let Request::Check { view, targets } = request else {
        return Err("`--fix` makes the suggestions of a check".to_owned());
    };
    let ViewRef::Head(head) = view else {
        return Err("`--fix` makes its suggestions in a checkout's head".to_owned());
    };
    let head = head.clone(); // clone: the head's name, kept for the commit
    let mut root = None;
    observe::ask(Request::Open { view: ViewRef::Head(head.clone()) }, Start::IfMissing, |response| {
        if let ResponseRef::Opened { root: it } = response {
            root = Some(it);
        }
        Ok(())
    })?;
    let root = root.ok_or("the daemon answered the checkout's open with no root")?;
    *view = ViewRef::Root(View { root });
    Ok(Pinned { root, head, targets: targets.clone() })
}

/// What `--fix` did.
pub struct Fixed {
    /// Suggestions made, and the files they edited.
    pub made: usize,
    pub files: Vec<String>,
    /// The suggestions of the check of the fixed code: none made, as that check came after.
    pub left: usize,
    /// The last check's errors: those of the first that the fixes did not take away.
    pub errors: usize,
}

/// The suggestions `found` (the first check of `pinned`) has made, checked again, committed and
/// written under `root`, the checkout.
pub fn run(found: Found, pinned: &Pinned, root: &Path) -> Result<Fixed, String> {
    let before = found.errors.clone(); // clone: the first check's errors, once
    let (mut made, mut count, mut last) = (Vec::new(), 0, found);
    for _ in 0..PASSES {
        let (ops, more) = last.ops();
        if ops.is_empty() {
            break;
        }
        let mut candidate = made.clone(); // clone: the ops so far, once a pass
        candidate.extend(ops);
        let after = checked(pinned, &candidate, &last.lints)?;
        let new: Vec<String> = after
            .errors
            .difference(&before)
            .map(|(code, file, message)| match code {
                Some(code) => format!("error[{code}] in {file}: {message}"),
                None => format!("error in {file}: {message}"),
            })
            .collect();
        if !new.is_empty() {
            return Err(format!(
                "the suggestions made would give {} error(s) the check did not have, so nothing is written:\n{}",
                new.len(),
                new.join("\n")
            ));
        }
        (made, count, last) = (candidate, count + more, after);
    }
    let mut files: Vec<String> = made
        .iter()
        .filter_map(|it| match it {
            Op::Edit { path, .. } => Some(path.clone()),
            _ => None,
        })
        .collect();
    files.sort_unstable();
    files.dedup();
    if !made.is_empty() {
        let mut rebased = None;
        let commit = Request::Commit { ops: made.clone(), base: pinned.root, head: pinned.head.clone() };
        observe::ask(commit, Start::Never, |response| {
            if let ResponseRef::Committed { rebased: it, .. } = response {
                rebased = Some(it);
            }
            Ok(())
        })?;
        match rebased {
            None => return Err("the daemon answered the fixes' commit with no commit".to_owned()),
            Some(true) => {
                return Err(
                    "the checkout changed while its suggestions were made and checked: nothing is written; run again"
                        .to_owned(),
                );
            }
            Some(false) => replace::write(root, &made)?,
        }
    }
    let unmade: BTreeSet<&[Part]> = last.suggestions.iter().map(Vec::as_slice).collect();
    Ok(Fixed { made: count, files, left: last.left + unmade.len(), errors: last.errors.len() })
}

/// The check of `pinned`'s root with `ops` made, as a what-if: what `--fix` reads of it, the
/// suggestions of `lints` alone as in the first. A daemon's error (the ops don't apply, the
/// check could not run) is the error.
fn checked(pinned: &Pinned, ops: &[Op], lints: &[String]) -> Result<Found, String> {
    let view = ViewRef::WhatIf { ops: ops.to_vec(), base: Box::new(ViewRef::Root(View { root: pinned.root })) };
    let request = Request::Check { view, targets: pinned.targets.clone() };
    let mut client = match daemon::connect(Start::Never) {
        Ok(Some(client)) => client,
        Ok(None) => return Err("the daemon stopped while the suggestions were made".to_owned()),
        Err(error) => return Err(format!("{error:#}")),
    };
    let mut found = Found::new(lints);
    let mut errors = Vec::new();
    daemon::Api::call(&mut client, request, |response, _| match response {
        ResponseRef::Diagnostic(diagnostic) => found.add(&diagnostic),
        // alloc: error path, the daemon's errors, said once it is done
        ResponseRef::Error(error) => errors.push(error.what.to_owned()),
        _ => {}
    })
    .map_err(|error| error.to_string())?;
    if !errors.is_empty() {
        return Err(format!("checking the suggestions made: {}", errors.join("\n")));
    }
    Ok(found)
}

#[cfg(test)]
mod tests {
    use super::{Found, Part};
    use blaze_vfs::Op;

    fn part(file: &str, start: u32, end: u32, text: &str) -> Part {
        Part { file: file.to_owned(), start, end, text: text.to_owned() }
    }

    #[test]
    fn a_suggestion_overlapping_one_before_it_or_given_twice_is_left_and_each_file_is_edited_from_its_end() {
        let found = Found {
            lints: Vec::new(),
            suggestions: vec![
                vec![part("src/lib.rs", 4, 9, "a"), part("src/lib.rs", 20, 20, "b")],
                vec![part("src/lib.rs", 4, 9, "a"), part("src/lib.rs", 20, 20, "b")],
                vec![part("src/lib.rs", 8, 12, "c")],
                vec![part("src/lib.rs", 20, 25, "d")],
                vec![part("src/main.rs", 0, 1, "e")],
                vec![part("src/lib.rs", 9, 9, "f")],
            ],
            left: 0,
            errors: Default::default(),
        };
        let (ops, count) = found.ops();
        assert_eq!(count, 3);
        let edit = |at: u64, len: u64, path: &str, bytes: &str| Op::Edit {
            at,
            len,
            path: path.to_owned(),
            bytes: bytes.as_bytes().to_vec(),
        };
        assert_eq!(
            ops,
            [
                edit(20, 0, "src/lib.rs", "b"),
                edit(9, 0, "src/lib.rs", "f"),
                edit(4, 5, "src/lib.rs", "a"),
                edit(0, 1, "src/main.rs", "e"),
            ]
        );
    }

    #[test]
    fn each_lint_narrows_the_suggestions_made_and_none_makes_every_one() {
        let every = Found::new(&[]);
        assert!(every.wanted(Some("unused_imports")) && every.wanted(Some("clippy::needless_return")) && every.wanted(None));
        let lints = ["clippy::disallowed_types".to_owned(), "unused_imports".to_owned()];
        let narrowed = Found::new(&lints);
        assert!(narrowed.wanted(Some("clippy::disallowed_types")) && narrowed.wanted(Some("unused_imports")));
        assert!(!narrowed.wanted(Some("clippy::needless_return")) && !narrowed.wanted(None));
    }

    #[test]
    fn only_a_file_of_the_checkout_is_fixed() {
        assert!(super::inside("crates/a/src/lib.rs"));
        for outside in ["", "/rustc/abc/library/core/src/lib.rs", "../x.rs", "./a.rs"] {
            assert!(!super::inside(outside), "{outside}");
        }
    }
}
