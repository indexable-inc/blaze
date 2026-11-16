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
use bstd::path::Path;

use crate::CliError;
use crate::sweep::{self, LeafKind, Rows};

/// The crate root the skeleton runs: a binary package's.
pub(crate) const ROOT: &str = "src/main.rs";

/// Where the root package's sources are: the folder of [`ROOT`].
const SOURCES: &str = "src/";

/// What `blaze run` hands the program: the command's world, the grants named on its command line
/// (`--grant name=value`, run.md), and the words after `--`.
pub struct Launch<'a> {
    pub io: &'a Io,
    pub grants: &'a [(&'a str, &'a str)],
    pub args: &'a [&'a str],
}

/// The bytes of the exchange a hosted program's calls pass through (bstd `Hosting`).
const EXCHANGE: usize = 4096;

/// The program's source files, each path with its text, sorted by path: the files the
/// request's walk opened (`rows`' slots that are present, from `Effect::Open` this command or
/// the leaf table's rows on a repeat) under the root package's `src/`. Nothing is listed: what
/// else the checkout holds (a `target/` of a million entries) is never looked at, so the cost
/// is the program's own files' (rule 77). The one home of the set, for the run and for the
/// program's refusals (`main.rs`, `program_refusals`).
pub fn sources<'f>(cx: &Cx, root: &Dir<ReadWrite>, rows: &Rows<'_>, frame: &'f Frame) -> Result<Vec<(&'f str, &'f str), &'f Frame>, CliError> {
    let mut texts: Vec<(&'f str, &'f str), &'f Frame> = Vec::new_in(frame);
    for leaf in rows.of(LeafKind::Slot).filter(|leaf| !leaf.absent() && leaf.flags & sweep::BOTH == 0) {
        let real = sweep::real_path(rows.path(leaf), leaf.flags, frame);
        let path = core::str::from_utf8(real.leak()).map_err(|_| CliError::Text { what: "a path is not UTF-8" })?;
        if !path.starts_with(SOURCES) {
            continue;
        }
        let mut bytes = Bytes::new_in(frame, "a source file of the program");
        root.read_all(cx, &Path::new(path)?, &mut bytes)?;
        let text = core::str::from_utf8(Vec::from_slice_in(bytes.as_slice(), frame).leak())
            .map_err(|_| CliError::Text { what: "a source file of the program is not UTF-8" })?;
        texts.push((path, text));
    }
    texts.sort_unstable_by(|a, b| a.0.cmp(b.0));
    Ok(texts)
}

/// Runs the package at `root`: the status `main` ended with. A hosted program (its entry bstd's
/// `bstd_hosted_start`) reaches the world only through the host table made here, over the
/// command's `Io` and only what `launch` grants; the host lives until the program has returned.
pub fn run(cx: &Cx, root: &Dir<ReadWrite>, launch: &Launch<'_>, rows: &Rows<'_>, frame: &Frame) -> Result<u8, CliError> {
    let cx = cx.span("blaze run: the program");
    let texts = sources(&cx, root, rows, frame)?;
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
