//! Thin CLI driver for `blaze-ctx`.
//!
//! All indexing, parsing, graph construction, hybrid retrieval, budget packing, and `blaze-tab`
//! rendering live in the `#![no_std]` sans-IO `blaze_ctx` library core. This binary performs only
//! CLI argument parsing, directory walking, `.blaze/ctx-index.cbor` file IO, and stdout writing.

#![forbid(unsafe_code)]

use std::env;
use std::fs;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Instant;

use blaze_ctx::{
    CacheError, CrateSnapshot, CtxIndex, DEFAULT_BUDGET_TOKENS, FileSnapshot, OutputFormat, QueryOpts, RepoSnapshot,
    area_report, area_to_value, doc_outline, domain_map_to_value, index_stats_to_value, outline_to_value, render_value,
    search, search_result_to_value,
};
use blaze_own::{Alloc, Heap};

const INDEX_REL_PATH: &str = ".blaze/ctx-index.cbor";

fn main() -> ExitCode {
    let alloc = Heap::process().alloc();
    let args: Vec<String> = env::args().skip(1).collect();
    match run(&args, alloc) {
        Ok(()) => ExitCode::SUCCESS,
        Err(msg) => {
            let mut err = io::stderr().lock();
            let _ = writeln!(err, "blaze-ctx: {msg}");
            ExitCode::FAILURE
        }
    }
}

fn run(args: &[String], alloc: Alloc<'static>) -> Result<(), String> {
    let Some(cmd) = args.first().map(String::as_str) else {
        print_usage()?;
        return Ok(());
    };
    let rest = &args[1..];
    match cmd {
        "-h" | "--help" | "help" => print_usage(),
        "index" => cmd_index(rest, alloc),
        "query" => cmd_query(rest, alloc),
        "outline" => cmd_outline(rest, alloc),
        "area" => cmd_area(rest, alloc),
        "map" => cmd_map(rest, alloc),
        other => Err(format!("unknown subcommand '{other}' (expected index|query|outline|area|map)")),
    }
}

fn print_usage() -> Result<(), String> {
    let mut out = io::stdout().lock();
    writeln!(
        out,
        "blaze-ctx — hierarchical repo context & hybrid search (blaze-tab)\n\n\
         USAGE:\n\
         \x20 blaze-ctx index [--root <dir>] [--format tab|json]\n\
         \x20 blaze-ctx query <text> [--root <dir>] [--budget <tokens>] [--format tab|json] [--no-expand]\n\
         \x20 blaze-ctx outline <doc-path> [--root <dir>] [--format tab|json]\n\
         \x20 blaze-ctx area <crate-or-topic> [--root <dir>] [--format tab|json]\n\
         \x20 blaze-ctx map [--root <dir>] [--format tab|json]"
    )
    .map_err(|e| format!("stdout write failed: {e}"))
}

fn parse_common_flags(
    args: &[String],
    allow_budget: bool,
) -> Result<(Vec<String>, PathBuf, OutputFormat, usize, bool), String> {
    let mut positional = Vec::new();
    let mut root: Option<PathBuf> = None;
    let mut format = OutputFormat::Tab;
    let mut budget = DEFAULT_BUDGET_TOKENS;
    let mut expand_hops = true;

    let mut i = 0usize;
    while i < args.len() {
        match args[i].as_str() {
            "--root" => {
                let val = args.get(i + 1).ok_or_else(|| "--root requires a directory path".to_owned())?;
                root = Some(PathBuf::from(val));
                i += 2;
            }
            "--format" => {
                let val = args.get(i + 1).ok_or_else(|| "--format requires 'tab' or 'json'".to_owned())?;
                format = OutputFormat::from_label(val)
                    .ok_or_else(|| format!("unsupported format '{val}' (expected 'tab' or 'json')"))?;
                i += 2;
            }
            "--budget" if allow_budget => {
                let val = args.get(i + 1).ok_or_else(|| "--budget requires a positive integer".to_owned())?;
                budget = val
                    .parse::<usize>()
                    .map_err(|_| format!("invalid --budget '{val}': expected positive integer"))?;
                if budget == 0 {
                    return Err("--budget must be > 0".to_owned());
                }
                i += 2;
            }
            "--no-expand" if allow_budget => {
                expand_hops = false;
                i += 1;
            }
            flag if flag.starts_with("--") => {
                return Err(format!("unknown flag '{flag}'"));
            }
            pos => {
                positional.push(pos.to_owned());
                i += 1;
            }
        }
    }

    let root = match root {
        Some(r) => r,
        None => env::current_dir().map_err(|e| format!("failed to read current_dir: {e}"))?,
    };
    Ok((positional, root, format, budget, expand_hops))
}

// ---------------------------------------------------------------------------
// Subcommands
// ---------------------------------------------------------------------------

fn cmd_index(args: &[String], alloc: Alloc<'static>) -> Result<(), String> {
    let (pos, root, format, _, _) = parse_common_flags(args, false)?;
    if !pos.is_empty() {
        return Err(format!("unexpected positional arguments for 'index': {:?}", pos));
    }

    let started = Instant::now();
    let cache_path = root.join(INDEX_REL_PATH);

    // Load existing cache if present; report & rebuild cleanly on schema version mismatch (Rule 26).
    let existing = if cache_path.exists() {
        let bytes = fs::read(&cache_path)
            .map_err(|e| format!("failed to read existing cache {}: {e}", cache_path.display()))?;
        match CtxIndex::from_cbor(&bytes, alloc) {
            Ok(idx) => Some(idx),
            Err(CacheError::SchemaVersionMismatch { found, expected }) => {
                let mut err = io::stderr().lock();
                let _ = writeln!(
                    err,
                    "blaze-ctx: cache {} is schema v{found} (expected v{expected}); rebuilding cleanly",
                    cache_path.display()
                );
                None
            }
            Err(e) => {
                let mut err = io::stderr().lock();
                let _ = writeln!(
                    err,
                    "blaze-ctx: cache {} unreadable ({e}); rebuilding cleanly",
                    cache_path.display()
                );
                None
            }
        }
    } else {
        None
    };

    let owned = collect_repo_files(&root, alloc)?;
    let excluded_refs: Vec<&str> = owned.excluded.iter().map(String::as_str).collect();
    let crate_snaps: Vec<CrateSnapshot<'_>> = owned
        .crates
        .iter()
        .map(|c| CrateSnapshot {
            dir: c.dir.as_str(),
            cargo_toml: c.cargo_toml.as_slice(),
            entry_path: c.entry_path.as_deref(),
            entry_bytes: c.entry_bytes.as_deref(),
        })
        .collect();
    let doc_snaps: Vec<FileSnapshot<'_>> = owned
        .docs
        .iter()
        .map(|f| FileSnapshot { path: f.path.as_str(), bytes: f.bytes.as_slice() })
        .collect();
    let howto_snaps: Vec<FileSnapshot<'_>> = owned
        .howtos
        .iter()
        .map(|f| FileSnapshot { path: f.path.as_str(), bytes: f.bytes.as_slice() })
        .collect();
    let annoyance_snaps: Vec<FileSnapshot<'_>> = owned
        .annoyances
        .iter()
        .map(|f| FileSnapshot { path: f.path.as_str(), bytes: f.bytes.as_slice() })
        .collect();

    let snapshot = RepoSnapshot {
        excluded_paths: &excluded_refs,
        crates: &crate_snaps,
        docs: &doc_snaps,
        howtos: &howto_snaps,
        annoyances: &annoyance_snaps,
    };

    let (index, stats) = CtxIndex::build_incremental(existing.as_ref(), &snapshot, alloc);

    let cache_bytes = if existing.is_some() && stats.reindexed == 0 && stats.removed == 0 {
        fs::metadata(&cache_path)
            .map(|m| usize::try_from(m.len()).unwrap_or(0))
            .unwrap_or(0)
    } else {
        let cbor = index.to_cbor();
        if let Some(parent) = cache_path.parent() {
            fs::create_dir_all(parent)
                .map_err(|e| format!("failed to create cache dir {}: {e}", parent.display()))?;
        }
        let tmp_path = cache_path.with_extension("cbor.tmp");
        fs::write(&tmp_path, cbor.as_slice())
            .map_err(|e| format!("failed to write temp cache {}: {e}", tmp_path.display()))?;
        fs::rename(&tmp_path, &cache_path)
            .map_err(|e| format!("failed to move cache into {}: {e}", cache_path.display()))?;
        cbor.len()
    };

    let elapsed_ms = started.elapsed().as_secs_f64() * 1000.0;
    let val = index_stats_to_value(stats, INDEX_REL_PATH, cache_bytes, elapsed_ms, alloc);
    let rendered = render_value(&val, format, alloc).map_err(|e| format!("{e}"))?;
    write_stdout(rendered.as_str())
}

fn load_index(root: &Path, alloc: Alloc<'static>) -> Result<CtxIndex<'static>, String> {
    let cache_path = root.join(INDEX_REL_PATH);
    let bytes = fs::read(&cache_path).map_err(|e| {
        format!(
            "cannot read index {} ({e}); run `blaze-ctx index --root {}` first",
            cache_path.display(),
            root.display()
        )
    })?;
    CtxIndex::from_cbor(&bytes, alloc)
        .map_err(|e| format!("failed to load {} ({e}); rebuild with `blaze-ctx index`", cache_path.display()))
}

fn cmd_query(args: &[String], alloc: Alloc<'static>) -> Result<(), String> {
    let (pos, root, format, budget, expand_hops) = parse_common_flags(args, true)?;
    if pos.is_empty() {
        return Err("query requires search text, e.g. `blaze-ctx query \"refinement serialization\"`".to_owned());
    }
    let query_text = pos.join(" ");
    let index = load_index(&root, alloc)?;
    let mut opts = QueryOpts::new(&query_text, budget);
    opts.expand_hops = expand_hops;
    let res = search(&index, &opts, alloc).map_err(|e| format!("{e}"))?;
    let val = search_result_to_value(&res, alloc);
    let rendered = render_value(&val, format, alloc).map_err(|e| format!("{e}"))?;
    write_stdout(rendered.as_str())
}

fn cmd_outline(args: &[String], alloc: Alloc<'static>) -> Result<(), String> {
    let (pos, root, format, _, _) = parse_common_flags(args, false)?;
    let Some(doc_path) = pos.first() else {
        return Err("outline requires a document path, e.g. `blaze-ctx outline docs/v2/plan/design/ctx.md`".to_owned());
    };
    let index = load_index(&root, alloc)?;
    let report = doc_outline(&index, doc_path, alloc)
        .ok_or_else(|| format!("document '{doc_path}' not found in index ({} docs indexed)", index.docs.len()))?;
    let val = outline_to_value(&report, alloc);
    let rendered = render_value(&val, format, alloc).map_err(|e| format!("{e}"))?;
    write_stdout(rendered.as_str())
}

fn cmd_area(args: &[String], alloc: Alloc<'static>) -> Result<(), String> {
    let (pos, root, format, _, _) = parse_common_flags(args, false)?;
    let Some(target) = pos.first() else {
        return Err("area requires a crate or topic name, e.g. `blaze-ctx area blaze-ty`".to_owned());
    };
    let index = load_index(&root, alloc)?;
    let report = area_report(&index, target, alloc);
    let val = area_to_value(&report, alloc);
    let rendered = render_value(&val, format, alloc).map_err(|e| format!("{e}"))?;
    write_stdout(rendered.as_str())
}

fn cmd_map(args: &[String], alloc: Alloc<'static>) -> Result<(), String> {
    let (pos, root, format, _, _) = parse_common_flags(args, false)?;
    if !pos.is_empty() {
        return Err(format!("unexpected positional arguments for 'map': {:?}", pos));
    }
    let index = load_index(&root, alloc)?;
    let val = domain_map_to_value(
        index.domains.as_slice(),
        index.crates.len(),
        index.docs.len(),
        index.notes.len(),
        index.edges.len(),
        alloc,
    );
    let rendered = render_value(&val, format, alloc).map_err(|e| format!("{e}"))?;
    write_stdout(rendered.as_str())
}

fn write_stdout(text: &str) -> Result<(), String> {
    let mut out = io::stdout().lock();
    writeln!(out, "{text}").map_err(|e| format!("stdout write failed: {e}"))
}

// ---------------------------------------------------------------------------
// Filesystem Snapshot Collector (Thin IO Shell)
// ---------------------------------------------------------------------------

struct OwnedCrate {
    dir: String,
    cargo_toml: Vec<u8>,
    entry_path: Option<String>,
    entry_bytes: Option<Vec<u8>>,
}

struct OwnedFile {
    path: String,
    bytes: Vec<u8>,
}

struct OwnedRepo {
    excluded: Vec<String>,
    crates: Vec<OwnedCrate>,
    docs: Vec<OwnedFile>,
    howtos: Vec<OwnedFile>,
    annoyances: Vec<OwnedFile>,
}

fn collect_repo_files(root: &Path, alloc: Alloc<'static>) -> Result<OwnedRepo, String> {
    let excluded = read_workspace_excludes(root, alloc);
    let crates = collect_crates(root)?;
    let mut docs = collect_files_recursive(&root.join("docs"), root, "md", &["upstream"])?;
    // Skip generated doc index `docs/README.md` (not a primary design doc; avoids noisy edges).
    docs.retain(|f| f.path != "docs/README.md");
    let howtos = collect_files_recursive(&root.join("agents/howto"), root, "json", &[])?;
    let annoyances = collect_files_recursive(&root.join("agents/annoyances"), root, "json", &[])?;

    Ok(OwnedRepo { excluded, crates, docs, howtos, annoyances })
}

fn read_workspace_excludes(root: &Path, alloc: Alloc<'static>) -> Vec<String> {
    let mut excluded = Vec::new();
    let root_toml = root.join("Cargo.toml");
    let Ok(text) = fs::read_to_string(&root_toml) else {
        return excluded;
    };
    let Ok(doc) = blaze_toml::Document::parse_in(&text, blaze_toml::Limits::CARGO, alloc) else {
        return excluded;
    };
    if let Some(ws) = doc.root().get("workspace")
        && let Some(exc) = ws.get("exclude")
    {
        for item in exc.children() {
            if let Some(s) = item.as_str() {
                excluded.push(s.to_owned());
            }
        }
    }
    excluded
}

fn collect_crates(root: &Path) -> Result<Vec<OwnedCrate>, String> {
    let crates_dir = root.join("crates");
    if !crates_dir.is_dir() {
        return Ok(Vec::new());
    }
    let mut entries: Vec<PathBuf> = fs::read_dir(&crates_dir)
        .map_err(|e| format!("failed to read {}: {e}", crates_dir.display()))?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    entries.sort();

    let mut out = Vec::new();
    for dir in entries {
        let cargo_path = dir.join("Cargo.toml");
        if !cargo_path.is_file() {
            continue;
        }
        let Ok(cargo_toml) = fs::read(&cargo_path) else {
            continue;
        };
        let rel_dir = to_rel_posix(&dir, root);
        let lib_rs = dir.join("src/lib.rs");
        let main_rs = dir.join("src/main.rs");
        let (entry_path, entry_bytes) = if lib_rs.is_file() {
            (Some(to_rel_posix(&lib_rs, root)), fs::read(&lib_rs).ok())
        } else if main_rs.is_file() {
            (Some(to_rel_posix(&main_rs, root)), fs::read(&main_rs).ok())
        } else {
            (None, None)
        };
        out.push(OwnedCrate { dir: rel_dir, cargo_toml, entry_path, entry_bytes });
    }
    Ok(out)
}

fn collect_files_recursive(
    start: &Path,
    root: &Path,
    ext: &str,
    skip_dirs: &[&str],
) -> Result<Vec<OwnedFile>, String> {
    if !start.is_dir() {
        return Ok(Vec::new());
    }
    let mut stack = vec![start.to_path_buf()];
    let mut paths = Vec::new();
    while let Some(dir) = stack.pop() {
        let Ok(rd) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in rd.filter_map(Result::ok) {
            let p = entry.path();
            if p.is_dir() {
                if let Some(name) = p.file_name().and_then(|n| n.to_str())
                    && !name.starts_with('.')
                    && !skip_dirs.contains(&name)
                {
                    stack.push(p);
                }
            } else if p.is_file()
                && p.extension().and_then(|e| e.to_str()) == Some(ext)
            {
                paths.push(p);
            }
        }
    }
    paths.sort();

    let mut files = Vec::with_capacity(paths.len());
    for p in paths {
        if let Ok(bytes) = fs::read(&p) {
            files.push(OwnedFile { path: to_rel_posix(&p, root), bytes });
        }
    }
    Ok(files)
}

fn to_rel_posix(path: &Path, root: &Path) -> String {
    let rel = path.strip_prefix(root).unwrap_or(path);
    rel.to_string_lossy().replace('\\', "/")
}
