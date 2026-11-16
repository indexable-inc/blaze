//! `blaze run`'s program, after its check: the package's source files read through bstd and
//! handed to the walking skeleton's straight line (`blaze_ladder::run`), which places the code
//! in this process and calls `main` (crates/blaze-cli/tests/ladder/README.md, route (e)).

use blaze_ladder::lower::SourceFile;
use blaze_own::Vec;
use bstd::Cx;
use bstd::fs::{Dir, ReadWrite};
use bstd::mem::{Bytes, Frame};

use crate::CliError;

/// The crate root the skeleton runs: a binary package's.
const ROOT: &str = "src/main.rs";

/// Runs the package at `root`: the status `main` ended with.
pub fn run(cx: &Cx, root: &Dir<ReadWrite>, frame: &Frame) -> Result<u8, CliError> {
    let cx = cx.span("blaze run: the program");
    let listed = root.list(&cx, frame)?;
    let mut texts: Vec<(&str, &str), &Frame> = Vec::new_in(frame);
    for (path, meta) in listed.iter() {
        let name = path.as_str();
        if meta.dir || !name.starts_with("src/") || !name.ends_with(".rs") {
            continue;
        }
        let mut bytes = Bytes::new_in(frame, "a source file of the program");
        root.read_all(&cx, path, &mut bytes)?;
        // Both outlive the listing and the buffer: copied into the command's frame, freed with it.
        let kept_path: &str = core::str::from_utf8(Vec::from_slice_in(name.as_bytes(), frame).leak())
            .map_err(|_| CliError::Text { what: "a path bstd listed is not UTF-8" })?;
        let kept_text: &str = core::str::from_utf8(Vec::from_slice_in(bytes.as_slice(), frame).leak())
            .map_err(|_| CliError::Text { what: "a source file of the program is not UTF-8" })?;
        texts.push((kept_path, kept_text));
    }
    texts.sort_unstable_by(|a, b| a.0.cmp(b.0));
    let mut files: Vec<SourceFile<'_>, &Frame> = Vec::with_capacity_in(texts.len(), frame);
    for &(path, text) in texts.iter() {
        files.push(SourceFile { path, text });
    }
    blaze_ladder::run::run(&files, ROOT).map_err(CliError::Program)
}
