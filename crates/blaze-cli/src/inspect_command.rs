//! `blaze inspect <file.rs[::fn_name]> [--json] [--all-exprs] [--no-coercions] [--no-defaults]`:
//! IDE-grade inlay hints for LLMs and humans over any Blaze or Rust source file.
//!
//! All filesystem reads go strictly through `bstd::fs::Fs` / `bstd::fs::Dir` (zero `std::fs` or
//! `std::path::Path`), and all output allocations live in the request's `Frame` or the caller's
//! `Heap`.

use blaze_ladder::front2::inspect::{
    BLAZE_EDITION, InspectOpts, inspect_source, render_annotated, render_json,
};
use blaze_own::{Box as OwnBox, String as OwnString};
use bstd::args::Args;
use bstd::cx::Cx;
use bstd::entry::Status;
use bstd::fs::{Dir, Fs, FsKind, ReadWrite};
use bstd::mem::{Bytes, Frame};
use bstd::out::Out;
use bstd::path::Path;

use crate::CliError;

/// Parsed CLI options for `blaze inspect`.
struct InspectArgs<'a> {
    target: Option<&'a str>,
    json: bool,
    opts: InspectOpts,
    edition: edition::PackageEdition,
}

fn parse_args(args: &Args) -> Result<InspectArgs<'_>, CliError> {
    let mut parsed = InspectArgs {
        target: None,
        json: false,
        opts: InspectOpts::default(),
        edition: BLAZE_EDITION,
    };
    let mut at = 2usize;
    while let Some(arg) = args.get(at) {
        at += 1;
        match arg {
            "--json" => parsed.json = true,
            "--all-exprs" => parsed.opts.all_exprs = true,
            "--no-coercions" => parsed.opts.coercions = false,
            "--no-defaults" => parsed.opts.defaults = false,
            "--no-try" => parsed.opts.try_sites = false,
            "--no-bindings" => parsed.opts.bindings = false,
            "--edition" => {
                let Some(val) = args.get(at) else {
                    return Err(CliError::Usage("`blaze inspect --edition` takes a value (e.g. 2024 or blaze-2026.11)"));
                };
                at += 1;
                parsed.edition = edition::PackageEdition::of_manifest(val)
                    .map_err(|_| CliError::Usage("`blaze inspect --edition` must be 2015, 2018, 2021, 2024, or blaze-2026.11"))?;
            }
            _ if let Some(val) = arg.strip_prefix("--edition=") => {
                parsed.edition = edition::PackageEdition::of_manifest(val)
                    .map_err(|_| CliError::Usage("`blaze inspect --edition` must be 2015, 2018, 2021, 2024, or blaze-2026.11"))?;
            }
            _ if arg.starts_with("--") => {
                return Err(CliError::Usage("`blaze inspect`: unknown flag (supported: --json, --all-exprs, --no-coercions, --no-defaults, --no-try, --no-bindings, --edition)"));
            }
            _ if parsed.target.is_some() => {
                return Err(CliError::Usage("`blaze inspect` takes at most one target (<file.rs[::fn_name]> or directory)"));
            }
            _ => parsed.target = Some(arg),
        }
    }
    Ok(parsed)
}

/// Splits `<path[::item_filter]>` into `(path, Option<item_filter>)`.
fn split_target(raw: &str) -> (&str, Option<&str>) {
    match raw.split_once("::") {
        Some((path, item)) if !item.is_empty() => (path, Some(item)),
        _ => (raw, None),
    }
}

/// Reads `src/main.rs` if present in `dir`, otherwise `src/lib.rs`.
fn read_dir_entry(cx: &Cx, dir: &Dir<ReadWrite>, out: &mut Bytes<'_>) -> Result<(), CliError> {
    let main_path = Path::new("src/main.rs")?;
    match dir.read_all(cx, &main_path, out) {
        Ok(_) => return Ok(()),
        Err(e) if matches!(e.kind, FsKind::NotFound) => {}
        Err(e) => return Err(e.into()),
    }
    let lib_path = Path::new("src/lib.rs")?;
    dir.read_all(cx, &lib_path, out)?;
    Ok(())
}

/// Reads an absolute `.rs` file or directory entry (`src/main.rs` / `src/lib.rs`) using pure `bstd::fs`.
fn read_absolute(cx: &Cx, fs: &Fs, raw: &str, out: &mut Bytes<'_>) -> Result<(), CliError> {
    let trimmed = raw.trim_end_matches('/');
    let Some((folder, leaf)) = trimmed.rsplit_once('/') else {
        return Err(CliError::Text { what: "inspect target path has no folder" });
    };
    if leaf.ends_with(".rs") {
        let Some((parent, name)) = folder.rsplit_once('/') else {
            return Err(CliError::Text { what: "inspect target folder is not beneath a directory" });
        };
        let parent = if parent.is_empty() { "/" } else { parent };
        let dir = fs.home(cx, parent, &Path::new(name)?).map_err(|_| CliError::Text {
            what: "inspect target directory could not be opened",
        })?;
        dir.read_all(cx, &Path::new(leaf)?, out)?;
        Ok(())
    } else {
        let parent = if folder.is_empty() { "/" } else { folder };
        let dir = fs.home(cx, parent, &Path::new(leaf)?).map_err(|_| CliError::Text {
            what: "inspect target directory could not be opened",
        })?;
        read_dir_entry(cx, &dir, out)
    }
}

/// Reads the target file (or `src/main.rs` / `src/lib.rs` inside a directory) through `bstd::fs`.
fn read_target_source(cx: &Cx, fs: &Fs, path_arg: Option<&str>, frame: &Frame, out: &mut Bytes<'_>) -> Result<(), CliError> {
    let Some(raw) = path_arg else {
        let root = fs.root();
        return read_dir_entry(cx, &root, out);
    };
    if raw.starts_with('/') {
        return read_absolute(cx, fs, raw, out);
    }
    let mut clean = OwnString::with_capacity_in(raw.len(), frame);
    for part in raw.split('/').filter(|p| !p.is_empty() && *p != ".") {
        if !clean.is_empty() {
            clean.push('/');
        }
        clean.push_str(part);
    }
    if clean.is_empty() {
        let root = fs.root();
        return read_dir_entry(cx, &root, out);
    }
    let rel_path = Path::new(clean.as_str())?;
    let root = fs.root();
    if let Ok(Some(meta)) = root.stat(cx, &rel_path)
        && meta.dir
    {
        let sub = root.subdir(&rel_path);
        return read_dir_entry(cx, &sub, out);
    }
    root.read_all(cx, &rel_path, out)?;
    Ok(())
}

/// Runs `blaze inspect`.
pub(crate) fn run(
    cx: &Cx,
    args: &Args,
    fs: &Fs,
    out: &Out,
    _err: &Out,
    heap: &'static blaze_own::Heap,
) -> Result<Status, CliError> {
    let parsed = parse_args(args)?;
    let frame = cx.frame();
    let (path_arg, filter_item) = match parsed.target {
        Some(raw) => {
            let (p, f) = split_target(raw);
            (Some(p), f)
        }
        None => (None, None),
    };

    let mut raw_bytes = Bytes::new_in(frame, "inspect source");
    read_target_source(cx, fs, path_arg, frame, &mut raw_bytes)?;
    let source = core::str::from_utf8(raw_bytes.as_slice()).map_err(|_| CliError::Text {
        what: "inspect target file is not valid UTF-8",
    })?;

    let store_alloc = heap.alloc();
    let result = inspect_source(source, parsed.edition, filter_item, parsed.opts, store_alloc, frame)
        .map_err(|e| CliError::Inspect(OwnBox::new_in(e, store_alloc)))?;

    let rendered = if parsed.json {
        render_json(&result, frame)
    } else {
        render_annotated(&result, frame)
    };
    out.write_all(cx, rendered.as_bytes())?;
    Ok(Status::OK)
}
