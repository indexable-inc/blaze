//! Apply byte-range edits to source files and render a unified diff.
//!
//! A consumer derives a set of [`Edit`]s (a byte range in some file, and the
//! text to splice in its place) from whatever analysis it runs, then hands them
//! here to be checked for overlap, applied, and diffed. The logic is corpus-
//! agnostic on purpose: `astlog` produces edits from tree-sitter rewrite
//! templates, and in ix `scipql` from Soufflé `edit` relations over a SCIP
//! index, and both share this one apply/diff/overlap path.
//!
//! Edits are keyed by `file`, an index into the `files`/`paths` slice the caller
//! passes, so the caller owns the file numbering. Callers must sort edits before
//! [`check_overlaps`] (apply is order-independent within a file because it
//! splices right-to-left).

use std::path::PathBuf;

use snafu::Snafu;

/// A source file the applier reads: its path and current contents. A named
/// alias (not a bare `(PathBuf, String)`) so signatures returning these stay
/// clear and the anonymous-tuple lint is satisfied.
pub type Source = (PathBuf, String);

/// One pending replacement of the byte range `start..end` in file `file` (an
/// index into the slice passed to [`apply`]/[`check_overlaps`]).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Edit {
    pub file: usize,
    pub start: usize,
    pub end: usize,
    pub replacement: String,
}

/// A file with all of its edits applied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileRewrite {
    pub path: PathBuf,
    pub content: String,
}

/// Two edits in the same file cover overlapping byte ranges, so applying both
/// is ambiguous. Reported instead of silently producing a corrupt splice.
#[derive(Debug, Snafu)]
#[snafu(display(
    "overlapping edits in {}: [{first_start}, {first_end}) overlaps [{second_start}, {second_end})",
    path.display()
))]
pub struct OverlapError {
    pub path: PathBuf,
    pub first_start: usize,
    pub first_end: usize,
    pub second_start: usize,
    pub second_end: usize,
}

/// Fail if any two edits in the same file overlap.
///
/// `paths` is indexed by [`Edit::file`] (only used to name the file in the
/// error). `edits` must be sorted ascending by `(file, start, end)`; the check
/// then only has to compare each edit against its immediate successor.
///
/// # Errors
///
/// Returns [`OverlapError`] for the first overlapping pair found.
pub fn check_overlaps(paths: &[PathBuf], edits: &[Edit]) -> Result<(), OverlapError> {
    for pair in edits.windows(2) {
        let [first, second] = pair else {
            continue;
        };
        // `edits` is sorted, so the pair overlaps exactly when the later edit
        // starts before the earlier one ends.
        if first.file == second.file && second.start < first.end {
            return OverlapSnafu {
                path: paths.get(first.file).cloned().unwrap_or_default(),
                first_start: first.start,
                first_end: first.end,
                second_start: second.start,
                second_end: second.end,
            }
            .fail();
        }
    }
    Ok(())
}

/// Apply edits, returning only the files that changed.
///
/// `files` is `(path, contents)` indexed by [`Edit::file`]. Within each file the
/// edits are spliced right-to-left so earlier offsets stay valid as later ranges
/// are replaced; the caller is responsible for non-overlap (see
/// [`check_overlaps`]).
#[must_use]
pub fn apply(files: &[Source], edits: &[Edit]) -> Vec<FileRewrite> {
    let mut by_file: Vec<Vec<&Edit>> = vec![Vec::new(); files.len()];
    for edit in edits {
        // An edit naming a file outside `files` has no text to splice into, so
        // there is nothing to apply it to; see the `file` note on [`Edit`].
        if let Some(bucket) = by_file.get_mut(edit.file) {
            bucket.push(edit);
        }
    }
    // `by_file` was built with one bucket per file, so zipping pairs each
    // bucket with its own source without indexing either.
    files
        .iter()
        .zip(by_file)
        .filter(|(_, edits)| !edits.is_empty())
        .map(|((path, text), mut file_edits)| {
            file_edits.sort();
            let mut content = text.clone();
            for edit in file_edits.into_iter().rev() {
                content.replace_range(edit.start..edit.end, &edit.replacement);
            }
            FileRewrite { path: path.clone(), content }
        })
        .collect()
}

/// Unified diff of every rewrite against its original file contents.
///
/// `files` is the same `(path, contents)` slice given to [`apply`]; each
/// rewrite's original is looked up by path.
#[must_use]
pub fn unified_diff(files: &[Source], rewrites: &[FileRewrite]) -> String {
    let mut out = String::new();
    for rewrite in rewrites {
        let original = files
            .iter()
            .find(|(path, _)| *path == rewrite.path)
            .map_or("", |(_, text)| text.as_str());
        let label = rewrite.path.display();
        let diff = similar::TextDiff::from_lines(original, &rewrite.content)
            .unified_diff()
            .header(&format!("a/{label}"), &format!("b/{label}"))
            .to_string();
        out.push_str(&diff);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(name: &str, text: &str) -> Source {
        (PathBuf::from(name), text.to_owned())
    }

    #[test]
    fn applies_multiple_edits_in_one_file_right_to_left() {
        let files = [file("a.rs", "let x = old_name(1);\nlet y = old_name(2);\n")];
        let edits = [
            Edit { file: 0, start: 8, end: 16, replacement: "new_name".to_owned() },
            Edit { file: 0, start: 29, end: 37, replacement: "new_name".to_owned() },
        ];
        let rewrites = apply(&files, &edits);
        assert_eq!(rewrites.len(), 1);
        assert_eq!(rewrites[0].content, "let x = new_name(1);\nlet y = new_name(2);\n");
    }

    #[test]
    fn unchanged_files_are_omitted() {
        let files = [file("a.rs", "keep me\n"), file("b.rs", "edit me\n")];
        let edits = [Edit { file: 1, start: 0, end: 4, replacement: "swap".to_owned() }];
        let rewrites = apply(&files, &edits);
        assert_eq!(rewrites.len(), 1);
        assert_eq!(rewrites[0].path, PathBuf::from("b.rs"));
    }

    #[test]
    fn overlapping_edits_are_rejected() {
        let paths = [PathBuf::from("a.rs")];
        let edits = [
            Edit { file: 0, start: 0, end: 5, replacement: "x".to_owned() },
            Edit { file: 0, start: 3, end: 8, replacement: "y".to_owned() },
        ];
        let error = check_overlaps(&paths, &edits).unwrap_err();
        assert_eq!(error.first_end, 5);
        assert_eq!(error.second_start, 3);
    }

    #[test]
    fn adjacent_edits_do_not_overlap() {
        let paths = [PathBuf::from("a.rs")];
        let edits = [
            Edit { file: 0, start: 0, end: 5, replacement: "x".to_owned() },
            Edit { file: 0, start: 5, end: 8, replacement: "y".to_owned() },
        ];
        assert!(check_overlaps(&paths, &edits).is_ok());
    }

    #[test]
    fn diff_has_unified_header() {
        let files = [file("a.rs", "old\n")];
        let rewrites = [FileRewrite { path: PathBuf::from("a.rs"), content: "new\n".to_owned() }];
        let diff = unified_diff(&files, &rewrites);
        assert!(diff.contains("a/a.rs"));
        assert!(diff.contains("-old"));
        assert!(diff.contains("+new"));
    }
}
