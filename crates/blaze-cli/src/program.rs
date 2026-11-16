//! `blaze run`'s program, after its check: the package's source files read through bstd and
//! handed to the walking skeleton's straight line (`blaze_ladder::run::run_program`: the new
//! front end over the toolchain's shelf), which places the code in this process and calls
//! `main` (crates/blaze-cli/tests/ladder/README.md, route (e)). A reached function this build
//! cannot make is a stub: the program runs, and stops by name only if it calls one.

use core::ffi::c_void;
use core::ptr::NonNull;

use blaze_ladder::refusal::SourceFile;
use blaze_ladder::run::{NewFront, RunError};
use blaze_manifest::layout::{Entry, Layout, Need, Pass};
use blaze_own::Vec;
use bstd::Cx;
use bstd::env::VarName;
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
///
/// `tables` are the rows to read them from: while the request runs (the program starts before
/// the answer), the last answer's rows as followed and this command's opens, a file in both
/// taken once; after the answer, the rows it kept. A row of a file no `mod` reaches any more is
/// read and never used: the runner walks from [`ROOT`].
pub fn sources<'f>(cx: &Cx, root: &Dir<ReadWrite>, tables: &[&Rows<'_>], frame: &'f Frame) -> Result<Vec<(&'f str, &'f str), &'f Frame>, CliError> {
    let mut paths: Vec<&'f str, &'f Frame> = Vec::new_in(frame);
    for rows in tables {
        for leaf in rows.of(LeafKind::Slot).filter(|leaf| !leaf.absent() && leaf.flags & sweep::BOTH == 0) {
            let real = sweep::real_path(rows.path(leaf), leaf.flags, frame);
            let path = core::str::from_utf8(real.leak()).map_err(|_| CliError::Text { what: "a path is not UTF-8" })?;
            if path.starts_with(SOURCES) {
                paths.push(path);
            }
        }
    }
    paths.sort_unstable();
    paths.dedup();
    let mut texts: Vec<(&'f str, &'f str), &'f Frame> = Vec::with_capacity_in(paths.len(), frame);
    for path in paths.iter().copied() {
        let mut bytes = Bytes::new_in(frame, "a source file of the program");
        root.read_all(cx, &Path::new(path)?, &mut bytes)?;
        let text = core::str::from_utf8(Vec::from_slice_in(bytes.as_slice(), frame).leak())
            .map_err(|_| CliError::Text { what: "a source file of the program is not UTF-8" })?;
        texts.push((path, text));
    }
    Ok(texts)
}

/// Runs the package at `root`: the status `main` ended with. A hosted program (its entry bstd's
/// `bstd_hosted_start`) reaches the world only through the host table made here, over the
/// command's `Io` and only what `launch` grants; the host lives until the program has returned.
///
/// Its files are the root package's `src/` folder, listed: the program starts beside its check
/// (`Gate::Beside`), before the walk has opened a file, so the walk's rows ([`sources`]) are
/// not there yet on a first run. Only `src/` is listed, never the checkout: a `target/` of a
/// million entries beside it costs nothing (it filled the command's frame when the whole
/// checkout was listed). Owed (drive.md, "RUNNER 11"): the runner walks forward from [`ROOT`]
/// through a loader, so a file no `mod` reaches is never read.
pub fn run(cx: &Cx, root: &Dir<ReadWrite>, launch: &Launch<'_>, frame: &Frame) -> Result<u8, CliError> {
    let cx = cx.span("blaze run: the program");
    let src = root.subdir(&Path::new(SOURCES.trim_end_matches('/'))?);
    let listed = match src.list(&cx, frame) {
        Ok(listed) => Some(listed),
        // No `src/`: no file, and the runner says the crate root is not among them.
        Err(error) if error.kind == bstd::fs::FsKind::NotFound => None,
        Err(error) => return Err(error.into()),
    };
    let mut texts: Vec<(&str, &str), &Frame> = Vec::new_in(frame);
    for (path, meta) in listed.iter().flatten() {
        if meta.dir || !path.as_str().ends_with(".rs") {
            continue;
        }
        let mut bytes = Bytes::new_in(frame, "a source file of the program");
        src.read_all(&cx, path, &mut bytes)?;
        // Both outlive the listing and the buffer: copied into the command's frame, freed with it.
        let mut kept = Vec::with_capacity_in(SOURCES.len() + path.as_str().len(), frame);
        kept.extend_from_slice(SOURCES.as_bytes());
        kept.extend_from_slice(path.as_str().as_bytes());
        let kept_path: &str = core::str::from_utf8(kept.leak()).map_err(|_| CliError::Text { what: "a path bstd listed is not UTF-8" })?;
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
    // The new front end's inputs: the package as its manifest names it, and the toolchain's
    // shelf under the home the command was started with (read from the command's own `Io`).
    let (package, edition) = package(&cx, root, frame)?;
    let env = launch.io.env();
    let home = match env.as_ref() {
        Some(env) => env.get(&VarName::new("HOME").map_err(|_| CliError::Text { what: "`HOME` is no variable name" })?).ok().flatten(),
        None => None,
    };
    let shelf = home.map(|home| bstd::mem::format_in!(frame, "{}/{}", home.trim_end_matches('/'), crate::toolchain::SHELF));
    let new = NewFront { package, edition, shelf: shelf.as_ref().map(|shelf| shelf.as_str()) };
    let ran = blaze_ladder::run::run_program(&files, ROOT, Some(host), new).map_err(CliError::Program);
    // The table points into the host: both are dropped only after the program returned.
    drop(table);
    drop(hosting);
    let ran = ran?;
    // The run's closing note, on the error stream (standard output is the program's alone): a
    // reached function that is not built is never passed over in silence, called or not.
    if ran.not_built > 0 {
        launch.io.need_err()?.line(&cx, format_args!("note: {} of {} reachable functions are not built yet; none was called", ran.not_built, ran.reached))?;
    }
    Ok(ran.status)
}

/// The exit status of `blaze run` when blaze itself could not build or start the program, or
/// stopped it at a function that is not built ([`CliError::Ran`]): the tool's own failure (D86),
/// as `env`, `timeout` and `xargs` say it. The program's own status passes through unchanged,
/// and 101 stays a Rust program's panic.
pub(crate) const NOT_RUN: u8 = 125;

/// The passes the package's layout may take (units.rs `MOST_PASSES`; rule 18).
const MOST_PASSES: usize = 64;

/// The root package's name, and its edition as a manifest writes it (`2024`, `blaze-2026.11`):
/// what the runner's front end is given ([`NewFront`]). Read by the workspace's one manifest
/// reader (blaze-manifest `Layout`), so an inherited edition is the workspace's.
///
/// Owed (drive.md, "Run"): the request has laid the workspace before the program starts; when
/// blaze-run's `Parent` starts the program from the units' record, this second read goes.
fn package<'f>(cx: &Cx, root: &Dir<ReadWrite>, frame: &'f Frame) -> Result<(&'f str, &'f str), CliError> {
    let refused = |why: &dyn core::fmt::Display| CliError::Program(RunError::Manifest { why: Box::from(bstd::mem::format_in!(frame, "{why}").as_str()) });
    let kept = |text: &str| -> Result<&'f str, CliError> {
        core::str::from_utf8(Vec::from_slice_in(text.as_bytes(), frame).leak()).map_err(|_| CliError::Text { what: "the package's manifest is not UTF-8" })
    };
    let erased = blaze_own::Erased::new(frame);
    let mut layout = Layout::new("", erased.handle()).map_err(|e| refused(&e))?;
    for _ in 0..MOST_PASSES {
        let needs = match layout.pass().map_err(|e| refused(&e))? {
            Pass::Need(needs) => needs,
            Pass::Done(units) => {
                // The package `blaze run` runs is the checkout's root's own ([`ROOT`]).
                let Some(member) = units.members.iter().find(|member| member.dir.as_str().is_empty()) else {
                    return Err(refused(&"the checkout's root is a workspace with no package of its own, and `blaze run` runs the root's package"));
                };
                let edition = match member.edition.blaze {
                    Some(blaze) => blaze.manifest(),
                    None => kept(bstd::mem::format_in!(frame, "{}", member.edition.syntax).as_str())?,
                };
                return Ok((kept(member.name.as_str())?, edition));
            }
        };
        for need in needs.iter() {
            match need {
                Need::Manifest(path) => {
                    let mut bytes = Bytes::new_in(frame, "a manifest");
                    let text = match root.read_all(cx, &Path::new(path.as_str())?, &mut bytes) {
                        Ok(_) => Some(core::str::from_utf8(bytes.as_slice()).map_err(|_| CliError::Text { what: "a manifest is not UTF-8" })?),
                        Err(error) if error.kind == bstd::fs::FsKind::NotFound => None,
                        Err(error) => return Err(error.into()),
                    };
                    layout.manifest(path.as_str(), text).map_err(|e| refused(&e))?;
                }
                Need::Listing(dir) => {
                    // `Dir::list` gives no entries for a directory that is not there.
                    let listed = match dir.as_str() {
                        "" => root.list(cx, frame)?,
                        named => root.subdir(&Path::new(named)?).list(cx, frame)?,
                    };
                    let mut entries: Vec<Entry<'_>, &Frame> = Vec::with_capacity_in(listed.len(), frame);
                    for (path, meta) in listed.iter() {
                        entries.push(Entry { name: path.as_str(), is_dir: meta.dir });
                    }
                    layout.listing(dir.as_str(), Some(&entries));
                }
            }
        }
    }
    Err(CliError::Text { what: "the package's layout asked for more than 64 passes" })
}
