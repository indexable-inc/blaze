//! `blaze inspect <file.rs[:start-end|::item]> [--lines <start-end>] [--json] [--no-docs]
//! [--no-outline] [--no-types] [--all-exprs] [--no-coercions] [--no-defaults] [--no-try]
//! [--no-bindings] [--edition <ed>]`:
//!
//! LLM-first file and slice viewer combining `blaze-file-items` outlines, `front2` semantic inlay
//! hints, and workspace-aware LSP-style inline hover documentation.
//!
//! All filesystem reads go strictly through `bstd::fs::Fs` / `bstd::fs::Dir` (zero `std::fs` or
//! `std::path::Path`), and all output allocations live in the request's `Frame` or the caller's
//! `Heap`.

use blaze_hash::HashSet;
use blaze_ladder::front2::inspect::BLAZE_EDITION;
use blaze_ladder::front2::inspect_view::{
    ContextFile, InspectTarget, LineRange, ViewOpts, collect_import_hints, inspect_file_view,
    render_llm_json, render_llm_view,
};
use blaze_own::{Box as OwnBox, String as OwnString, Vec as OwnVec};
use bstd::args::Args;
use bstd::cx::Cx;
use bstd::entry::Status;
use bstd::fs::{Dir, Fs, FsKind, ReadWrite};
use bstd::mem::{Bytes, Frame};
use bstd::out::Out;
use bstd::path::Path;

use crate::CliError;

/// Maximum workspace/sibling context files read for cross-file hover documentation.
const MAX_CONTEXT_FILES: usize = 32;

/// Maximum combined bytes across all loaded context files (rule 18 bounded work).
const MAX_CONTEXT_BYTES: usize = 2 * 1024 * 1024;

/// Parsed CLI options for `blaze inspect`.
struct InspectArgs<'a> {
    target: Option<&'a str>,
    lines_flag: Option<LineRange>,
    json: bool,
    view_opts: ViewOpts,
    edition: edition::PackageEdition,
}

fn parse_args(args: &Args) -> Result<InspectArgs<'_>, CliError> {
    let mut parsed = InspectArgs {
        target: None,
        lines_flag: None,
        json: false,
        view_opts: ViewOpts::default(),
        edition: BLAZE_EDITION,
    };
    let mut at = 2usize;
    while let Some(arg) = args.get(at) {
        at += 1;
        match arg {
            "--json" => parsed.json = true,
            "--no-docs" => parsed.view_opts.show_docs = false,
            "--no-outline" => parsed.view_opts.show_outline = false,
            "--no-types" => parsed.view_opts.show_types = false,
            "--all-exprs" => parsed.view_opts.inspect.all_exprs = true,
            "--no-coercions" => parsed.view_opts.inspect.coercions = false,
            "--no-defaults" => parsed.view_opts.inspect.defaults = false,
            "--no-try" => parsed.view_opts.inspect.try_sites = false,
            "--no-bindings" => parsed.view_opts.inspect.bindings = false,
            "--lines" => {
                let Some(val) = args.get(at) else {
                    return Err(CliError::Usage("`blaze inspect --lines` takes a range (e.g. 10-80 or 42)"));
                };
                at += 1;
                parsed.lines_flag = Some(LineRange::parse(val).ok_or(CliError::Usage(
                    "`blaze inspect --lines` expects 1-based range like `10-80`, `10..=80`, or `42`",
                ))?);
            }
            _ if let Some(val) = arg.strip_prefix("--lines=") => {
                parsed.lines_flag = Some(LineRange::parse(val).ok_or(CliError::Usage(
                    "`blaze inspect --lines` expects 1-based range like `10-80`, `10..=80`, or `42`",
                ))?);
            }
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
                return Err(CliError::Usage(
                    "`blaze inspect`: unknown flag (supported: --lines, --json, --no-docs, --no-outline, --no-types, --all-exprs, --no-coercions, --no-defaults, --no-try, --no-bindings, --edition)",
                ));
            }
            _ if parsed.target.is_some() => {
                return Err(CliError::Usage(
                    "`blaze inspect` takes at most one target (<file.rs[:start-end|::item]> or directory)",
                ));
            }
            _ => parsed.target = Some(arg),
        }
    }
    Ok(parsed)
}

/// Splits `<path[::item_filter]>` or `<path:line-range>` into `(path, target_slice)`.
fn split_target<'a>(raw: &'a str, lines_flag: Option<LineRange>) -> Result<(&'a str, InspectTarget<'a>), CliError> {
    if let Some((path, item)) = raw.split_once("::")
        && !item.is_empty()
    {
        if let Some(lines) = lines_flag {
            return Ok((path, InspectTarget::Lines(lines)));
        }
        return Ok((path, InspectTarget::Item(item)));
    }
    // Check trailing `:start-end` / `:42` (only after the last `/`, so drive letters/paths aren't split).
    let last_slash = raw.rfind('/').map_or(0, |i| i + 1);
    if let Some(rel_colon) = raw[last_slash..].rfind(':') {
        let colon = last_slash + rel_colon;
        let path = &raw[..colon];
        let spec = &raw[colon + 1..];
        if let Some(range) = LineRange::parse(spec) {
            return Ok((path, InspectTarget::Lines(lines_flag.unwrap_or(range))));
        }
        return Err(CliError::Usage(
            "`blaze inspect`: invalid `:line` slice (expected `:42`, `:10-80`, or `:10..=80`)",
        ));
    }
    Ok((raw, lines_flag.map_or(InspectTarget::WholeFile, InspectTarget::Lines)))
}

/// Reads `src/main.rs` if present in `dir`, otherwise `src/lib.rs`, returning the relative file name.
fn read_dir_entry(cx: &Cx, dir: &Dir<ReadWrite>, out: &mut Bytes<'_>) -> Result<&'static str, CliError> {
    let main_path = Path::new("src/main.rs")?;
    match dir.read_all(cx, &main_path, out) {
        Ok(_) => return Ok("src/main.rs"),
        Err(e) if matches!(e.kind, FsKind::NotFound) => {}
        Err(e) => return Err(e.into()),
    }
    let lib_path = Path::new("src/lib.rs")?;
    dir.read_all(cx, &lib_path, out)?;
    Ok("src/lib.rs")
}

/// Reads an absolute `.rs` file or directory entry (`src/main.rs` / `src/lib.rs`) using pure `bstd::fs`.
fn read_absolute<'f>(
    cx: &Cx,
    fs: &Fs,
    raw: &str,
    frame: &'f Frame,
    out: &mut Bytes<'_>,
) -> Result<OwnString<&'f Frame>, CliError> {
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
        Ok(OwnString::from_str_in(trimmed, frame))
    } else {
        let parent = if folder.is_empty() { "/" } else { folder };
        let dir = fs.home(cx, parent, &Path::new(leaf)?).map_err(|_| CliError::Text {
            what: "inspect target directory could not be opened",
        })?;
        let entry = read_dir_entry(cx, &dir, out)?;
        let mut display = OwnString::with_capacity_in(trimmed.len() + 1 + entry.len(), frame);
        display.push_str(trimmed);
        display.push('/');
        display.push_str(entry);
        Ok(display)
    }
}

/// Reads the target file (or `src/main.rs` / `src/lib.rs` inside a directory) through `bstd::fs`,
/// returning its clean display path (relative to workspace root when possible).
fn read_target_source<'f>(
    cx: &Cx,
    fs: &Fs,
    path_arg: Option<&str>,
    frame: &'f Frame,
    out: &mut Bytes<'_>,
) -> Result<(OwnString<&'f Frame>, bool), CliError> {
    let Some(raw) = path_arg else {
        let root = fs.root();
        let entry = read_dir_entry(cx, &root, out)?;
        return Ok((OwnString::from_str_in(entry, frame), true));
    };
    if raw.starts_with('/') {
        let display = read_absolute(cx, fs, raw, frame, out)?;
        return Ok((display, false));
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
        let entry = read_dir_entry(cx, &root, out)?;
        return Ok((OwnString::from_str_in(entry, frame), true));
    }
    let rel_path = Path::new(clean.as_str())?;
    let root = fs.root();
    if let Ok(Some(meta)) = root.stat(cx, &rel_path)
        && meta.dir
    {
        let sub = root.subdir(&rel_path);
        let entry = read_dir_entry(cx, &sub, out)?;
        clean.push('/');
        clean.push_str(entry);
        return Ok((clean, true));
    }
    root.read_all(cx, &rel_path, out)?;
    Ok((clean, true))
}

/// Owned context file bytes kept in the request's `Frame`.
struct LoadedContext<'f> {
    path: OwnString<&'f Frame>,
    crate_name: OwnString<&'f Frame>,
    text: OwnString<&'f Frame>,
}

/// Loads referenced sibling modules and workspace crate source files through `bstd::fs::Dir`
/// (bounded by [`MAX_CONTEXT_FILES`] and [`MAX_CONTEXT_BYTES`]) so `inspect_file_view` can attach
/// cross-file LSP hover documentation.
fn load_workspace_context<'f>(
    cx: &Cx,
    fs: &Fs,
    target_rel: &str,
    source: &str,
    edition: edition::PackageEdition,
    frame: &'f Frame,
) -> OwnVec<LoadedContext<'f>, &'f Frame> {
    let mut loaded: OwnVec<LoadedContext<'f>, &'f Frame> = OwnVec::new_in(frame);
    let mut seen: HashSet<OwnString<&'f Frame>, &'f Frame> = HashSet::new_in(frame);
    seen.insert(OwnString::from_str_in(target_rel, frame));

    let root = fs.root();
    let hints = collect_import_hints(source, edition, frame);
    let (target_dir, target_leaf) = target_rel.rsplit_once('/').unwrap_or(("", target_rel));
    let target_stem = target_leaf.strip_suffix(".rs").unwrap_or(target_leaf);
    let crate_src_dir = find_crate_src_dir(target_rel);

    let mut total_bytes = 0usize;

    let mut try_load = |rel: &str, crate_name: &str, loaded: &mut OwnVec<LoadedContext<'f>, &'f Frame>, seen: &mut HashSet<OwnString<&'f Frame>, &'f Frame>| {
        if loaded.len() >= MAX_CONTEXT_FILES || total_bytes >= MAX_CONTEXT_BYTES {
            return;
        }
        let cleaned = clean_rel_path(rel, frame);
        if cleaned.is_empty() || seen.contains(&cleaned) {
            return;
        }
        let Ok(path) = Path::new(cleaned.as_str()) else { return };
        let mut buf = Bytes::new_in(frame, "inspect context");
        if root.read_all(cx, &path, &mut buf).is_err() {
            return;
        }
        if total_bytes.saturating_add(buf.as_slice().len()) > MAX_CONTEXT_BYTES {
            return;
        }
        let Ok(text_str) = core::str::from_utf8(buf.as_slice()) else { return };
        total_bytes = total_bytes.saturating_add(text_str.len());
        seen.insert(OwnString::from_str_in(cleaned.as_str(), frame));
        loaded.push(LoadedContext {
            path: cleaned,
            crate_name: OwnString::from_str_in(crate_name, frame),
            text: OwnString::from_str_in(text_str, frame),
        });
    };

    // 1. Sibling `mod foo;` declarations.
    for (mod_name, explicit_path) in hints.mods.iter() {
        if let Some(explicit) = explicit_path {
            let joined = join_rel(target_dir, explicit.as_str(), frame);
            try_load(joined.as_str(), "crate", &mut loaded, &mut seen);
            continue;
        }
        // Standard Rust module paths: `<dir>/<name>.rs`, `<dir>/<name>/mod.rs`, and `<dir>/<stem>/<name>.rs`.
        let mut c1 = join_rel(target_dir, mod_name.as_str(), frame);
        c1.push_str(".rs");
        try_load(c1.as_str(), "crate", &mut loaded, &mut seen);

        let mut c2 = join_rel(target_dir, mod_name.as_str(), frame);
        c2.push_str("/mod.rs");
        try_load(c2.as_str(), "crate", &mut loaded, &mut seen);

        if target_leaf != "lib.rs" && target_leaf != "main.rs" && target_leaf != "mod.rs" {
            let mut sub_dir = join_rel(target_dir, target_stem, frame);
            sub_dir.push('/');
            sub_dir.push_str(mod_name.as_str());
            sub_dir.push_str(".rs");
            try_load(sub_dir.as_str(), "crate", &mut loaded, &mut seen);
        }
    }

    // 2. Expanded `use` paths (`crate::...`, `super::...`, and workspace crates `crates/<kebab>/src/...`).
    let mut saw_crate_or_super = false;
    for path_segs in hints.use_paths.iter() {
        let Some(first) = path_segs.first().map(OwnString::as_str) else { continue };
        let second = path_segs.get(1).map(OwnString::as_str);
        let third = path_segs.get(2).map(OwnString::as_str);

        if first == "crate" {
            saw_crate_or_super = true;
            if let Some(src_dir) = crate_src_dir {
                if let Some(s) = second {
                    let mut p1 = join_rel(src_dir, s, frame);
                    p1.push_str(".rs");
                    try_load(p1.as_str(), "crate", &mut loaded, &mut seen);

                    let mut p2 = join_rel(src_dir, s, frame);
                    p2.push_str("/mod.rs");
                    try_load(p2.as_str(), "crate", &mut loaded, &mut seen);

                    if let Some(t) = third {
                        let mut p3 = join_rel(src_dir, s, frame);
                        p3.push('/');
                        p3.push_str(t);
                        p3.push_str(".rs");
                        try_load(p3.as_str(), "crate", &mut loaded, &mut seen);
                    }
                }
                let root_lib = join_rel(src_dir, "lib.rs", frame);
                try_load(root_lib.as_str(), "crate", &mut loaded, &mut seen);
            }
        } else if first == "super" {
            saw_crate_or_super = true;
            let parent_dir = target_dir.rsplit_once('/').map_or("", |(p, _)| p);
            if let Some(s) = second {
                let mut p1 = join_rel(parent_dir, s, frame);
                p1.push_str(".rs");
                try_load(p1.as_str(), "crate", &mut loaded, &mut seen);

                let mut p2 = join_rel(target_dir, s, frame);
                p2.push_str(".rs");
                try_load(p2.as_str(), "crate", &mut loaded, &mut seen);
            }
            if !target_dir.is_empty() {
                let mut parent_rs = OwnString::from_str_in(target_dir, frame);
                parent_rs.push_str(".rs");
                try_load(parent_rs.as_str(), "crate", &mut loaded, &mut seen);

                let parent_mod = join_rel(target_dir, "mod.rs", frame);
                try_load(parent_mod.as_str(), "crate", &mut loaded, &mut seen);
            }
        } else if !matches!(first, "core" | "alloc" | "std" | "self") {
            // Map Rust crate ident (`blaze_core`, `bstd`) to workspace crate folder (`crates/blaze-core`, `crates/blaze-std`).
            let folder_name = workspace_crate_folder(first, frame);
            let mut ws_src = OwnString::with_capacity_in(7 + folder_name.len() + 4, frame);
            ws_src.push_str("crates/");
            ws_src.push_str(folder_name.as_str());
            ws_src.push_str("/src");

            if let Some(s) = second {
                let mut m1 = join_rel(ws_src.as_str(), s, frame);
                m1.push_str(".rs");
                try_load(m1.as_str(), first, &mut loaded, &mut seen);

                let mut m2 = join_rel(ws_src.as_str(), s, frame);
                m2.push_str("/mod.rs");
                try_load(m2.as_str(), first, &mut loaded, &mut seen);

                if let Some(t) = third {
                    let mut m3 = join_rel(ws_src.as_str(), s, frame);
                    m3.push('/');
                    m3.push_str(t);
                    m3.push_str(".rs");
                    try_load(m3.as_str(), first, &mut loaded, &mut seen);
                }
            }
            let ws_lib = join_rel(ws_src.as_str(), "lib.rs", frame);
            try_load(ws_lib.as_str(), first, &mut loaded, &mut seen);
        }
    }

    // If the file is inside a crate and didn't already load its crate root, load `lib.rs` / `main.rs` once.
    if !saw_crate_or_super
        && let Some(src_dir) = crate_src_dir
    {
        let root_lib = join_rel(src_dir, "lib.rs", frame);
        try_load(root_lib.as_str(), "crate", &mut loaded, &mut seen);
    }

    loaded
}

fn find_crate_src_dir(rel: &str) -> Option<&str> {
    if let Some(idx) = rel.find("/src/") {
        return rel.get(..idx + 4);
    }
    if rel.starts_with("src/") {
        return Some("src");
    }
    None
}

fn workspace_crate_folder<'f>(ident: &str, frame: &'f Frame) -> OwnString<&'f Frame> {
    if ident == "bstd" {
        return OwnString::from_str_in("blaze-std", frame);
    }
    let mut out = OwnString::with_capacity_in(ident.len(), frame);
    for ch in ident.chars() {
        out.push(if ch == '_' { '-' } else { ch });
    }
    out
}

fn join_rel<'f>(dir: &str, leaf: &str, frame: &'f Frame) -> OwnString<&'f Frame> {
    if dir.is_empty() {
        return OwnString::from_str_in(leaf, frame);
    }
    let mut out = OwnString::with_capacity_in(dir.len() + 1 + leaf.len(), frame);
    out.push_str(dir);
    out.push('/');
    out.push_str(leaf);
    out
}

fn clean_rel_path<'f>(raw: &str, frame: &'f Frame) -> OwnString<&'f Frame> {
    let mut parts: OwnVec<&str, &'f Frame> = OwnVec::new_in(frame);
    for seg in raw.split('/') {
        if seg.is_empty() || seg == "." {
            continue;
        }
        if seg == ".." {
            if parts.pop().is_none() {
                return OwnString::new_in(frame);
            }
        } else {
            parts.push(seg);
        }
    }
    let mut out = OwnString::with_capacity_in(raw.len(), frame);
    for (i, seg) in parts.iter().enumerate() {
        if i > 0 {
            out.push('/');
        }
        out.push_str(seg);
    }
    out
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
    let (path_arg, target) = match parsed.target {
        Some(raw) => {
            let (p, t) = split_target(raw, parsed.lines_flag)?;
            (Some(p), t)
        }
        None => (None, parsed.lines_flag.map_or(InspectTarget::WholeFile, InspectTarget::Lines)),
    };

    let mut raw_bytes = Bytes::new_in(frame, "inspect source");
    let (display_path, is_workspace_rel) = read_target_source(cx, fs, path_arg, frame, &mut raw_bytes)?;
    let source = core::str::from_utf8(raw_bytes.as_slice()).map_err(|_| CliError::Text {
        what: "inspect target file is not valid UTF-8",
    })?;

    let loaded_ctx = if parsed.view_opts.show_docs && is_workspace_rel {
        load_workspace_context(cx, fs, display_path.as_str(), source, parsed.edition, frame)
    } else {
        OwnVec::new_in(frame)
    };
    let mut ctx_refs: OwnVec<ContextFile<'_>, &Frame> = OwnVec::with_capacity_in(loaded_ctx.len(), frame);
    for c in loaded_ctx.iter() {
        ctx_refs.push(ContextFile {
            path: c.path.as_str(),
            crate_name: c.crate_name.as_str(),
            text: c.text.as_str(),
        });
    }

    let store_alloc = heap.alloc();
    let view = inspect_file_view(
        display_path.as_str(),
        source,
        parsed.edition,
        target,
        ctx_refs.as_slice(),
        parsed.view_opts,
        store_alloc,
        frame,
    )
    .map_err(|e| CliError::Inspect(OwnBox::new_in(e, store_alloc)))?;

    let rendered = if parsed.json {
        render_llm_json(&view, frame)
    } else {
        render_llm_view(&view, frame)
    };
    out.write_all(cx, rendered.as_bytes())?;
    Ok(Status::OK)
}
