//! Thin CLI driver for `blaze-bstd-convert`.
//!
//! All analysis, decision planning, state-machine compilation, and AST/manifest rewriting happen
//! in the pure sans-IO library crate; this binary only reads files, prompts stdin/stdout in `ask`
//! mode, and writes converted files to the requested output directory.

#![forbid(unsafe_code)]

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{self, BufRead, Write as _};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use blaze_bstd_convert::{
    Decisions, SmAllocMode, WorkspaceSnapshot, analyze_workspace, build_questions,
    compile_file_functions_opts, convert_workspace,
    report::{
        render_analysis_json, render_analysis_markdown, render_conversion_markdown,
        render_questions_markdown,
    },
};

const USAGE: &str = "\
blaze-bstd-convert: convert Rust workspaces from std + third-party crates onto bstd & explicit sans-IO state machines

USAGE:
  blaze-bstd-convert analyze     <workspace-dir> [--json]
  blaze-bstd-convert questions   <workspace-dir> [--out <decisions.json>] [--md]
  blaze-bstd-convert ask         <workspace-dir> --out <decisions.json>
  blaze-bstd-convert lift        <file.rs> [--fn <name>] [--plain-alloc]
  blaze-bstd-convert convert     <workspace-dir> --out <dest-dir> [--decisions <decisions.json> | --defaults] [--crate <name>]... [--bstd-path <path>]
  blaze-bstd-convert no-std-core <crate-dir>... [--write]
  blaze-bstd-convert verify      <converted-dir>
";

#[expect(
    clippy::print_stdout,
    clippy::print_stderr,
    reason = "thin CLI driver prints reports and diagnostics to stdout/stderr"
)]
fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() || matches!(args[0].as_str(), "-h" | "--help" | "help") {
        print!("{USAGE}");
        return ExitCode::SUCCESS;
    }

    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("error: {err}");
            ExitCode::FAILURE
        }
    }
}

#[expect(
    clippy::print_stdout,
    reason = "thin CLI driver writes command output to stdout"
)]
fn run(args: &[String]) -> Result<(), String> {
    match args[0].as_str() {
        "analyze" => {
            let root = args.get(1).ok_or_else(|| "analyze requires <workspace-dir>".to_owned())?;
            let json = args.iter().any(|a| a == "--json");
            let snap = load_workspace(Path::new(root))?;
            let analysis = analyze_workspace(&snap);
            if json {
                println!("{}", render_analysis_json(&analysis).map_err(|e| e.to_string())?);
            } else {
                print!("{}", render_analysis_markdown(&analysis));
            }
            Ok(())
        }
        "questions" => {
            let root = args.get(1).ok_or_else(|| "questions requires <workspace-dir>".to_owned())?;
            let out_path = flag_value(args, "--out");
            let snap = load_workspace(Path::new(root))?;
            let analysis = analyze_workspace(&snap);
            let questions = build_questions(&analysis);
            if let Some(path) = out_path {
                let defaults = Decisions::from_recommended(&questions);
                let json = defaults.to_json().map_err(|e| e.to_string())?;
                fs::write(path, json).map_err(|e| format!("write {path}: {e}"))?;
                println!("Wrote recommended decisions template ({} questions) to {path}", questions.len());
            } else {
                print!("{}", render_questions_markdown(&questions));
            }
            Ok(())
        }
        "ask" => {
            let root = args.get(1).ok_or_else(|| "ask requires <workspace-dir>".to_owned())?;
            let out_path = flag_value(args, "--out")
                .ok_or_else(|| "ask requires --out <decisions.json>".to_owned())?;
            let snap = load_workspace(Path::new(root))?;
            let analysis = analyze_workspace(&snap);
            let questions = build_questions(&analysis);
            let decisions = run_interactive_qa(&questions)?;
            let json = decisions.to_json().map_err(|e| e.to_string())?;
            fs::write(out_path, json).map_err(|e| format!("write {out_path}: {e}"))?;
            println!("\nSaved {} answers to {out_path}.", decisions.answers.len());
            Ok(())
        }
        "lift" => {
            let file = args.get(1).ok_or_else(|| "lift requires <file.rs>".to_owned())?;
            let only_fn = flag_value(args, "--fn");
            let plain_alloc = args.iter().any(|a| a == "--plain-alloc");
            let mode = if plain_alloc {
                SmAllocMode::PlainAlloc
            } else {
                SmAllocMode::BstdExplicitAlloc
            };
            let src = fs::read_to_string(file).map_err(|e| format!("read {file}: {e}"))?;
            let machines = compile_file_functions_opts(&src, only_fn, None, mode);
            if machines.is_empty() {
                return Err(format!("no effectful / IO functions found in {file}"));
            }
            for sm in &machines {
                println!("// ==========================================================================");
                println!(
                    "// Function `{}` -> `{}` ({} states, max {} live vars saved / {} total locals)",
                    sm.fn_name,
                    sm.machine_name,
                    sm.state_count,
                    sm.liveness.max_live_at_suspend,
                    sm.liveness.total_locals
                );
                println!("// ==========================================================================\n");
                println!("{}", sm.sans_io_code);
                println!("{}", sm.bstd_driver_code);
                println!("{}", sm.sim_test_code);
            }
            Ok(())
        }
        "verify" => {
            let root = args.get(1).ok_or_else(|| "verify requires <converted-dir>".to_owned())?;
            let snap = load_workspace(Path::new(root))?;
            verify_converted_workspace(&snap)
        }
        "no-std-core" => {
            let write = args.iter().any(|a| a == "--write");
            let dirs: Vec<&str> = args[1..]
                .iter()
                .filter(|a| !a.starts_with("--"))
                .map(String::as_str)
                .collect();
            if dirs.is_empty() {
                return Err("no-std-core requires at least one <crate-dir>".to_owned());
            }
            for dir in dirs {
                convert_crate_dir_no_std_core(Path::new(dir), write)?;
            }
            Ok(())
        }
        "convert" => {
            let root = args.get(1).ok_or_else(|| "convert requires <workspace-dir>".to_owned())?;
            let out_dir = flag_value(args, "--out")
                .ok_or_else(|| "convert requires --out <dest-dir> (non-destructive output)".to_owned())?;
            let decisions_path = flag_value(args, "--decisions");
            let use_defaults = args.iter().any(|a| a == "--defaults");
            let bstd_path = flag_value(args, "--bstd-path");
            let only_crates = repeated_flag_values(args, "--crate");

            let snap = load_workspace(Path::new(root))?;
            let analysis = analyze_workspace(&snap);
            let questions = build_questions(&analysis);

            let mut decisions = if let Some(dpath) = decisions_path {
                let text = fs::read_to_string(dpath).map_err(|e| format!("read {dpath}: {e}"))?;
                Decisions::from_json(&text).map_err(|e| e.to_string())?
            } else if use_defaults {
                Decisions::from_recommended(&questions)
            } else {
                return Err(format!(
                    "{} conversion decisions are open; pass `--decisions <file.json>`, run `ask` first, or pass `--defaults` for recommended bstd-first choices",
                    questions.len()
                ));
            };
            if let Some(bp) = bstd_path {
                decisions.bstd_path = Some(bp.to_owned());
            }

            let filter = if only_crates.is_empty() {
                None
            } else {
                Some(only_crates.into_iter().collect::<BTreeSet<_>>())
            };

            let result = convert_workspace(&snap, &analysis, &decisions, filter.as_ref());
            write_output_dir(Path::new(out_dir), &result.files, filter.as_ref(), &analysis)?;
            print!("{}", render_conversion_markdown(&result));
            println!("\nConverted output written to `{out_dir}`.");
            Ok(())
        }
        other => Err(format!("unknown subcommand `{other}`\n\n{USAGE}")),
    }
}

fn flag_value<'a>(args: &'a [String], flag: &str) -> Option<&'a str> {
    args.windows(2)
        .find(|w| w[0] == flag)
        .map(|w| w[1].as_str())
}

fn repeated_flag_values(args: &[String], flag: &str) -> Vec<String> {
    args.windows(2)
        .filter(|w| w[0] == flag)
        .map(|w| w[1].clone())
        .collect()
}

#[expect(
    clippy::print_stdout,
    reason = "interactive Q&A prompts the user on stdout"
)]
fn run_interactive_qa(questions: &[blaze_bstd_convert::Question]) -> Result<Decisions, String> {
    let stdin = io::stdin();
    let mut lines = stdin.lock().lines();
    let mut answers = BTreeMap::new();

    println!("Interactive `blaze-bstd-convert` Q&A ({} decisions). Press Enter for recommended.\n", questions.len());
    for (idx, q) in questions.iter().enumerate() {
        println!("[{}/{}] {} ({})", idx + 1, questions.len(), q.id, q.category.as_str());
        println!("  {}", q.prompt);
        for (oi, opt) in q.options.iter().enumerate() {
            let rec = if opt.recommended { " [default]" } else { "" };
            println!("    {}) {}{} — {}", oi + 1, opt.label, rec, opt.impact);
        }
        print!("  Choice [1-{} / Enter for default]: ", q.options.len());
        let _ = io::stdout().flush();

        let line = match lines.next() {
            Some(Ok(l)) => l.trim().to_owned(),
            _ => String::new(),
        };
        let chosen = if line.is_empty() {
            q.default_key().to_owned()
        } else if let Ok(n) = line.parse::<usize>() {
            q.options
                .get(n.saturating_sub(1))
                .map(|o| o.key.clone())
                .unwrap_or_else(|| q.default_key().to_owned())
        } else if q.options.iter().any(|o| o.key == line) {
            line
        } else {
            q.default_key().to_owned()
        };
        println!("  -> selected `{chosen}`\n");
        answers.insert(q.id.clone(), chosen);
    }

    Ok(Decisions { answers, bstd_path: None })
}

fn load_workspace(root: &Path) -> Result<WorkspaceSnapshot, String> {
    if !root.is_dir() {
        return Err(format!("workspace directory not found: {}", root.display()));
    }
    let mut files = BTreeMap::new();
    let mut stack: Vec<PathBuf> = vec![root.to_path_buf()];
    let mut visited = 0usize;
    const MAX_DIRS: usize = 50_000;

    while let Some(dir) = stack.pop() {
        visited += 1;
        if visited > MAX_DIRS {
            return Err(format!("exceeded {MAX_DIRS} directories walking {}", root.display()));
        }
        let entries = fs::read_dir(&dir).map_err(|e| format!("read_dir {}: {e}", dir.display()))?;
        for entry in entries {
            let entry = entry.map_err(|e| format!("dir entry: {e}"))?;
            let path = entry.path();
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            if matches!(
                name_str.as_ref(),
                ".git" | "target" | "vendor" | "node_modules" | ".cache" | ".venv" | ".cargo"
            ) {
                continue;
            }
            let ftype = entry.file_type().map_err(|e| format!("file_type {}: {e}", path.display()))?;
            if ftype.is_symlink() {
                continue;
            }
            if ftype.is_dir() {
                stack.push(path);
            } else if ftype.is_file() && (name_str == "Cargo.toml" || name_str.ends_with(".rs")) {
                if let Ok(text) = fs::read_to_string(&path) {
                    if let Ok(rel) = path.strip_prefix(root) {
                        let rel_str = rel.to_string_lossy().replace('\\', "/");
                        files.insert(rel_str, text);
                    }
                }
            }
        }
    }

    Ok(WorkspaceSnapshot { files })
}

fn write_output_dir(
    out_dir: &Path,
    files: &BTreeMap<String, String>,
    only_crates: Option<&BTreeSet<String>>,
    analysis: &blaze_bstd_convert::WorkspaceAnalysis,
) -> Result<(), String> {
    // If `--crate` filter is set, write only files belonging to those crates (keeps output clean!).
    let allowed_dirs: Option<Vec<String>> = only_crates.map(|set| {
        analysis
            .crates
            .iter()
            .filter(|c| set.contains(&c.name))
            .map(|c| c.dir.clone())
            .collect()
    });

    for (rel, content) in files {
        if let Some(dirs) = &allowed_dirs {
            let matches = dirs
                .iter()
                .any(|d| d.is_empty() || rel == &format!("{d}/Cargo.toml") || rel.starts_with(&format!("{d}/")));
            if !matches {
                continue;
            }
        }
        let dest = out_dir.join(rel);
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent).map_err(|e| format!("create_dir_all {}: {e}", parent.display()))?;
        }
        fs::write(&dest, content).map_err(|e| format!("write {}: {e}", dest.display()))?;
    }
    Ok(())
}

#[expect(
    clippy::print_stdout,
    reason = "no-std-core subcommand prints per-crate conversion summary"
)]
fn convert_crate_dir_no_std_core(crate_dir: &Path, write: bool) -> Result<(), String> {
    let src_dir = crate_dir.join("src");
    if !src_dir.is_dir() {
        return Err(format!("no src/ directory in {}", crate_dir.display()));
    }
    let snap = load_workspace(crate_dir)?;
    let is_proc_macro = snap.files.get("Cargo.toml").is_some_and(|toml| {
        let alloc = blaze_own::Alloc::global();
        blaze_toml::Document::parse_in(toml, blaze_toml::Limits::CARGO, alloc)
            .ok()
            .and_then(|doc| doc.root().get("lib").and_then(|lib| lib.get("proc-macro")).and_then(|v| v.as_bool()))
            .unwrap_or(false)
    });
    let mut total_edits = 0usize;
    let mut any_remaining_std = is_proc_macro;
    let mut any_needs_alloc = false;
    let mut rewritten_files: BTreeMap<String, String> = BTreeMap::new();

    for (rel, content) in &snap.files {
        if !rel.starts_with("src/") || !rel.ends_with(".rs") || rel.starts_with("src/bin/") || rel.contains("test") {
            continue;
        }
        let (new_src, edits, rem_std, needs_alloc) = blaze_bstd_convert::rewrite::rewrite_rust_no_std_core(content);
        total_edits += edits;
        if rem_std {
            any_remaining_std = true;
        }
        if needs_alloc {
            any_needs_alloc = true;
        }
        if new_src != *content {
            rewritten_files.insert(rel.clone(), new_src);
        }
    }

    let mut graduated = false;
    if !any_remaining_std {
        let lib_rel = "src/lib.rs".to_owned();
        if let Some(lib_src) = rewritten_files
            .get(&lib_rel)
            .cloned()
            .or_else(|| snap.files.get(&lib_rel).cloned())
        {
            let with_hdr = blaze_bstd_convert::rewrite::insert_no_std_header(&lib_src, any_needs_alloc);
            if with_hdr != lib_src {
                rewritten_files.insert(lib_rel, with_hdr);
                graduated = true;
            }
        }
    }

    if write {
        for (rel, content) in &rewritten_files {
            let dest = crate_dir.join(rel);
            fs::write(&dest, content).map_err(|e| format!("write {}: {e}", dest.display()))?;
        }
    }

    println!(
        "{}: {} AST edits across {} files | remaining_std={} | needs_alloc={} | graduated_no_std={} ({})",
        crate_dir.display(),
        total_edits,
        rewritten_files.len(),
        any_remaining_std,
        any_needs_alloc,
        graduated,
        if write { "WROTE" } else { "dry-run" }
    );
    Ok(())
}

#[expect(
    clippy::print_stdout,
    reason = "verify subcommand prints structured verification summary to stdout"
)]
fn verify_converted_workspace(snap: &WorkspaceSnapshot) -> Result<(), String> {
    let mut rs_total = 0usize;
    let mut rs_parse_errors = 0usize;
    let mut toml_total = 0usize;
    let mut toml_errors = 0usize;
    let mut sm_files = 0usize;
    let mut sm_parse_errors = 0usize;
    let mut sm_duplicate_names = 0usize;
    let mut sm_bounded_unreachables = 0usize;
    let mut no_std_crates = 0usize;

    let mut sm_error_samples: Vec<String> = Vec::new();
    let mut rs_error_samples: Vec<String> = Vec::new();
    let alloc = blaze_own::Alloc::global();
    for (rel, content) in &snap.files {
        if rel.ends_with("Cargo.toml") {
            toml_total += 1;
            if blaze_toml::Document::parse_in(content, blaze_toml::Limits::CARGO, alloc).is_err() {
                toml_errors += 1;
            }
        } else if rel.ends_with(".rs") {
            rs_total += 1;
            let parsed = syntax::SourceFile::parse(content, syntax::Edition::CURRENT);
            let errs = parsed.errors().len();
            if errs > 0 {
                rs_parse_errors += 1;
                if rs_error_samples.len() < 10 && !rel.ends_with("/sans_io_machines.rs") {
                    let first = &parsed.errors()[0];
                    let off: usize = first.range().start().into();
                    let line = blaze_bstd_convert::analyze::line_of_offset(content, off);
                    let snippet = content.lines().nth(line.saturating_sub(1)).unwrap_or("").trim();
                    rs_error_samples.push(format!("{rel}:{line}: {first} | `{snippet}`"));
                }
            }
            if rel.ends_with("/src/lib.rs") && (content.contains("#![no_std]") || content.contains("#![cfg_attr(not(test), no_std)]")) {
                no_std_crates += 1;
            }
            if rel.ends_with("/sans_io_machines.rs") {
                sm_files += 1;
                if errs > 0 {
                    sm_parse_errors += 1;
                    if sm_error_samples.len() < 10 {
                        let first = &parsed.errors()[0];
                        let off: usize = first.range().start().into();
                        let line = blaze_bstd_convert::analyze::line_of_offset(content, off);
                        let snippet = content.lines().nth(line.saturating_sub(1)).unwrap_or("").trim();
                        sm_error_samples.push(format!("{rel}:{line}: {first} | `{snippet}`"));
                    }
                }
                if content.contains("unreachable!(\"bounded CFG") {
                    sm_bounded_unreachables += 1;
                }
                let mut seen = BTreeSet::new();
                for line in content.lines() {
                    let t = line.trim();
                    if let Some(rest) = t.strip_prefix("pub struct ").or_else(|| t.strip_prefix("struct ")) {
                        let name = rest.split(|c: char| !c.is_ascii_alphanumeric() && c != '_').next().unwrap_or("");
                        if name.ends_with("Machine") && !seen.insert(name.to_owned()) {
                            sm_duplicate_names += 1;
                        }
                    }
                }
            }
        }
    }

    println!("# blaze-bstd-convert Verification Report\n");
    println!("- **Cargo.toml files parsed cleanly**: {} / {toml_total} ({toml_errors} errors)", toml_total - toml_errors);
    println!("- **Rust `.rs` files parsed cleanly**: {} / {rs_total} ({rs_parse_errors} files with syntax errors)", rs_total - rs_parse_errors);
    println!("- **Generated `sans_io_machines.rs` files**: {sm_files} ({sm_parse_errors} parse errors, {sm_duplicate_names} duplicate machine names, {sm_bounded_unreachables} depth-limit hits)");
    println!("- **Crates graduated to `no_std`**: {no_std_crates}");
    if !sm_error_samples.is_empty() {
        println!("\nSample `sans_io_machines.rs` parse errors:");
        for s in &sm_error_samples {
            println!("  - {s}");
        }
    }
    if !rs_error_samples.is_empty() {
        println!("\nSample rewritten `.rs` parse errors:");
        for s in &rs_error_samples {
            println!("  - {s}");
        }
    }
    Ok(())
}
