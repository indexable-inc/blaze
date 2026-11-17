//! Each producer's source digest, made as blaze is built (decision D105, piece 2;
//! docs/v2/plan/design/producer-id.md): the thin driver of `blaze_producers::Walk`, the sans-IO
//! core, over the workspace on disk. It writes `$OUT_DIR/producer_sources.rs`, the array of
//! (row name, digest) the entry includes (src/producers.rs), and tells the builder to run it
//! again when any path the digests read changes.
//!
//! A row the walk cannot digest is written `None`, never a panic: a lock of another moment than
//! the sources (a snapshot mix of the tree) refuses the rows that reach the missing name, and a
//! walk that stops (a manifest not there yet) refuses every row. Each is a `cargo::warning`
//! naming the crate and the row, and a comment in the generated file; the entry keys a `None` row
//! by the binary's build id (piece 1's key, which every rebuilt blaze moves), so its records are
//! made again, never read stale. Only the disk failing under the step (not a refusal) stops the
//! build, by name.
//!
//! Under cargo the workspace root is the nearest directory above this crate whose `Cargo.toml`
//! has a `[workspace]` table. When blaze builds blaze, the step runs in its sandbox, which serves
//! a member the workspace above `/workspace/<member dir>` read-only (decision D96; every open,
//! stat and listing lands in the step's read list by its workspace-relative path), so the same
//! walk finds the same root and gives the same bytes: no digest holds a path of the machine.

use std::{
    fmt, fs, io,
    path::{Path, PathBuf},
};

use blaze_own::{Alloc, Vec};
use blaze_producers::{Digest, Entry, LOCK, ROOT_MANIFEST, ROWS, Refused, Step, Walk};

/// Why the step stopped: the disk under it, each naming the path. A walk's refusal is not one.
pub enum Failed {
    /// No directory above the crate has a `Cargo.toml` with a `[workspace]` table.
    NoWorkspace { from: PathBuf },
    /// A file or directory could not be read for a reason other than its absence.
    Io { path: PathBuf, error: io::Error },
    /// A name in a listed directory that is not UTF-8 (a path in a digest is text).
    NotUtf8 { dir: PathBuf },
    /// The workspace root's manifest is no TOML.
    RootToml { manifest: PathBuf, why: std::string::String },
}

impl fmt::Display for Failed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Failed::NoWorkspace { from } => {
                write!(f, "no `Cargo.toml` with `[workspace]` above {}", from.display())
            }
            Failed::Io { path, error } => write!(f, "{}: {error}", path.display()),
            Failed::NotUtf8 { dir } => write!(f, "{}: an entry whose name is not UTF-8", dir.display()),
            Failed::RootToml { manifest, why } => write!(f, "{}: not TOML: {why}", manifest.display()),
        }?;
        f.write_str(" (the producer digests, crates/blaze-cli/build/producers.rs)")
    }
}

impl fmt::Debug for Failed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

/// Walks the workspace above `manifest_dir`, writes `out_dir/producer_sources.rs` (only when its
/// bytes change), prints a `cargo::warning` per refused row and a `cargo::rerun-if-changed` line
/// per watched path.
pub fn write(manifest_dir: &Path, out_dir: &Path, alloc: Alloc<'_>) -> Result<(), Failed> {
    let root = workspace_root(manifest_dir, alloc)?;
    let mut walk = Walk::new(alloc);
    let mut rows = ROWS.map(|row| (row.name(), None::<[u8; 32]>));
    // Each refusal as said: the row (or every row) and why.
    let mut refusals: Vec<std::string::String, Alloc<'_>> = Vec::new_in(alloc);
    let watch = |path: &str| println!("cargo::rerun-if-changed={}", root.join(path).display());
    match drive(&root, &mut walk, alloc)? {
        Ok(()) => match walk.finish() {
            Ok(made) => {
                for (at, (row, digest)) in made.rows.iter().enumerate() {
                    match digest {
                        Digest::Source(digest) => rows[at].1 = Some(*digest),
                        Digest::Refused(why) => refusals.push(format!("row `{}`: {why}", row.name())),
                    }
                }
                for path in made.watch.iter() {
                    watch(path.as_str());
                }
            }
            // A driver bug (an answer the walk did not ask): every row, said.
            Err(why) => {
                refusals.push(format!("every row: {why}"));
                watch(ROOT_MANIFEST);
                watch(LOCK);
            }
        },
        Err(why) => {
            refusals.push(format!("every row: {why}"));
            for path in walk.watched() {
                watch(path.as_str());
            }
            // The file it stopped at (a manifest not written yet): its arrival runs the step again.
            if let Some(path) = why.path() {
                watch(path);
            }
            watch(ROOT_MANIFEST);
            watch(LOCK);
        }
    }
    let mut source = std::string::String::from(
        "// @generated by crates/blaze-cli/build/producers.rs (decision D105): each producer's source\n\
         // digest, or `None` where the step could not say it (each refusal below).\n[\n",
    );
    for (name, digest) in rows {
        match digest {
            Some(digest) => source.push_str(&format!("    ({name:?}, Some({digest:?})),\n")),
            None => source.push_str(&format!("    ({name:?}, None),\n")),
        }
    }
    source.push_str("]\n");
    for refusal in refusals.iter() {
        let line = refusal.replace('\n', " ");
        source.push_str(&format!("// refused: {line}\n"));
        println!(
            "cargo::warning=blaze's producer digests, {line}: that row is keyed by this binary's build id, so its records are made again by every rebuilt blaze"
        );
    }
    let out = out_dir.join("producer_sources.rs");
    let unchanged = fs::read(&out).is_ok_and(|old| old == source.as_bytes());
    if !unchanged {
        fs::write(&out, source.as_bytes()).map_err(|error| Failed::Io { path: out.clone(), error })?;
    }
    Ok(())
}

/// Answers the walk's steps from the disk until it is done: `Ok(Err(why))` is the walk's
/// refusal (the rows are `None`), `Err` the disk failing under it.
fn drive<'a>(root: &Path, walk: &mut Walk<'a>, alloc: Alloc<'_>) -> Result<Result<(), Refused<'a>>, Failed> {
    // Bounded: the walk asks for each manifest, file and directory once, then says `Done`.
    loop {
        let step = match walk.next() {
            Ok(step) => step,
            Err(why) => return Ok(Err(why)),
        };
        let answered = match step {
            Step::Done => return Ok(Ok(())),
            Step::Read(rel) => {
                let path = root.join(rel);
                match fs::read(&path) {
                    Ok(bytes) => walk.read(Some(&bytes)),
                    Err(error) if error.kind() == io::ErrorKind::NotFound => walk.read(None),
                    Err(error) => return Err(Failed::Io { path, error }),
                }
            }
            Step::List(rel) => {
                let dir = root.join(rel);
                match fs::read_dir(&dir) {
                    Ok(listing) => {
                        let mut names: Vec<(std::string::String, bool), Alloc<'_>> = Vec::new_in(alloc);
                        for entry in listing {
                            let entry = entry.map_err(|error| Failed::Io { path: dir.clone(), error })?;
                            let path = entry.path();
                            // `metadata` follows a symbolic link: the walk sees what it points at.
                            let meta = fs::metadata(&path).map_err(|error| Failed::Io { path: path.clone(), error })?;
                            let name = entry.file_name().into_string().map_err(|_| Failed::NotUtf8 { dir: dir.clone() })?;
                            names.push((name, meta.is_dir()));
                        }
                        let mut entries: Vec<Entry<'_>, Alloc<'_>> = Vec::new_in(alloc);
                        for (name, is_dir) in names.iter() {
                            entries.push(Entry { name: name.as_str(), is_dir: *is_dir });
                        }
                        walk.listed(Some(&entries[..]))
                    }
                    Err(error) if error.kind() == io::ErrorKind::NotFound => walk.listed(None),
                    Err(error) => return Err(Failed::Io { path: dir, error }),
                }
            }
        };
        if let Err(why) = answered {
            return Ok(Err(why));
        }
    }
}

/// The nearest directory at or above `from` whose `Cargo.toml` has a `[workspace]` table.
fn workspace_root(from: &Path, alloc: Alloc<'_>) -> Result<PathBuf, Failed> {
    for dir in from.ancestors() {
        let manifest = dir.join("Cargo.toml");
        let text = match fs::read_to_string(&manifest) {
            Ok(text) => text,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(Failed::Io { path: manifest, error }),
        };
        let doc = blaze_toml::Document::parse_in(&text, blaze_toml::Limits::CARGO, alloc)
            .map_err(|error| Failed::RootToml { manifest: manifest.clone(), why: error.to_string() })?;
        if doc.root().get("workspace").is_some() {
            return Ok(dir.to_path_buf());
        }
    }
    Err(Failed::NoWorkspace { from: from.to_path_buf() })
}
