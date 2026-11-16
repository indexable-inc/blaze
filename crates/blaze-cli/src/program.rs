//! `blaze run`'s program, after its check: the package's source files read through bstd and
//! handed to the walking skeleton's straight line (`blaze_ladder::run`), which places the code
//! in this process and calls `main` (crates/blaze-cli/tests/ladder/README.md, route (e)).

use core::ffi::c_void;
use core::ptr::NonNull;

use blaze_ladder::refusal::SourceFile;
use blaze_own::Vec;
use bstd::Cx;
use bstd::fs::{Dir, ReadWrite};
use bstd::hosting::Hosting;
use bstd::io::Io;
use bstd::mem::{Bytes, Frame};

use crate::CliError;

/// The crate root the skeleton runs: a binary package's.
const ROOT: &str = "src/main.rs";

/// What `blaze run` hands the program: the command's world, the grants named on its command line
/// (`--grant name=value`, run.md), and the words after `--`.
pub struct Launch<'a> {
    pub io: &'a Io,
    pub grants: &'a [(&'a str, &'a str)],
    pub args: &'a [&'a str],
}

/// The bytes of the exchange a hosted program's calls pass through (bstd `Hosting`).
const EXCHANGE: usize = 4096;

/// Runs the package at `root`: the status `main` ended with. A hosted program (its entry bstd's
/// `bstd_hosted_start`) reaches the world only through the host table made here, over the
/// command's `Io` and only what `launch` grants; the host lives until the program has returned.
pub fn run(cx: &Cx, root: &Dir<ReadWrite>, launch: &Launch<'_>, frame: &Frame) -> Result<u8, CliError> {
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
    // bstd's host takes its arguments and grants as owned strings: once per run, Global.
    let args = launch.args.iter().map(|arg| std::string::String::from(*arg)).collect();
    let grants = launch.grants.iter().map(|(name, value)| (std::string::String::from(*name), std::string::String::from(*value))).collect();
    // clone: the command's world, a refcount.
    let hosting = Hosting::new(launch.io.clone(), args, grants, EXCHANGE);
    let table = hosting.table();
    let host = NonNull::from(&table).cast::<c_void>();
    let status = blaze_ladder::run::run(&files, ROOT, Some(host)).map_err(CliError::Program);
    // The table points into the host: both are dropped only after the program returned.
    drop(table);
    drop(hosting);
    status
}
