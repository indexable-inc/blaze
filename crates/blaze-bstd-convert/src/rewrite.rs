//! Lossless manifest (`Cargo.toml`) and Rust AST (`syntax`) rewriter.
//!
//! Every Rust source edit is computed from exact `syntax::TextRange` byte spans, validated to be
//! non-overlapping, and applied back-to-front so untouched code, comments, and formatting are
//! preserved byte-for-byte. Every `Cargo.toml` edit uses `blaze-toml::Document` + `Edit`.
//!
//! Features:
//! - Grouped `use std::{...}` splitting into `core::{...}`, `bstd::{...}`, and remaining `std::{...}`.
//! - Explicit allocator rewriting (`Vec::new_in`, `Vec::with_capacity_in`, `String::new_in`,
//!   `format_in!`, `vec_in!`, `Map::new(MapOpts { .. })`) with automatic `alloc: bstd::mem::Alloc<'_>`
//!   parameter threading on non-trait functions.
//! - Direct capability rewriting (`Instant::now()` -> `clock.now()`, `env::var("K")` ->
//!   `env.get(&bstd::var!("K"))`) with automatic narrowed capability parameter threading.
//! - Automatic disambiguation of colliding state-machine names across files of one crate.
//! - Graduation of zero-`std`, zero-external-dep crates to `#![cfg_attr(not(test), no_std)]`.

use alloc::{
    borrow::ToOwned,
    collections::{BTreeMap, BTreeSet},
    format,
    string::{String, ToString},
    vec,
    vec::Vec,
};
use core::fmt::Write as _;

use blaze_own::Alloc;
use blaze_toml::{Document, Edit, Limits};
use syntax::{
    AstNode, Edition, SourceFile, SyntaxKind,
    ast::{self, HasArgList, HasGenericArgs, HasGenericParams, HasName, HasVisibility},
};

use crate::{
    analyze::{CrateAnalysis, DepSection, WorkspaceAnalysis, WorkspaceSnapshot},
    catalog::{BstdFeature, is_c_interop_crate, is_std_core_module, lookup_crate},
    questions::{AllocStrategy, CInteropChoice, ComptimeChoice, Decisions, DepChoice, SmLiftMode},
    sm_compiler::{
        SmAllocMode, compile_file_functions_opts, compile_struct_sm, generate_blaze_c_toml_section,
        generate_c_interop_scaffold, generate_comptime_scaffold, write_newtypes_scaffold,
    },
};

/// One byte-range text replacement derived from an AST node's `TextRange`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpanEdit {
    pub start: usize,
    pub end: usize,
    pub replacement: String,
}

/// Apply non-overlapping [`SpanEdit`]s to `src` back-to-front.
/// Inner edits contained within an already-selected outer edit are skipped deterministically.
pub fn apply_span_edits(src: &str, mut edits: Vec<SpanEdit>) -> String {
    if edits.is_empty() {
        return src.to_owned();
    }
    edits.sort_by(|a, b| a.start.cmp(&b.start).then_with(|| b.end.cmp(&a.end)));

    let mut filtered: Vec<SpanEdit> = Vec::with_capacity(edits.len());
    let mut last_end = 0usize;
    for e in edits {
        if e.start <= e.end && e.end <= src.len() && e.start >= last_end {
            last_end = e.end;
            filtered.push(e);
        }
    }

    let mut out = src.to_owned();
    for e in filtered.into_iter().rev() {
        out.replace_range(e.start..e.end, &e.replacement);
    }
    out
}

/// Per-crate conversion summary.
#[derive(Debug, Clone)]
pub struct CrateConversionSummary {
    pub crate_name: String,
    pub external_deps_before: usize,
    pub external_deps_after: usize,
    pub removed_deps: Vec<String>,
    pub kept_deps: Vec<String>,
    pub added_bstd: bool,
    pub bstd_features: Vec<String>,
    pub added_blaze_crates: Vec<String>,
    pub ast_edits_applied: usize,
    pub lifted_machines: Vec<String>,
    pub refactored_structs: Vec<String>,
    pub comptime_scaffold_generated: bool,
    pub c_interop_scaffold_generated: bool,
    pub newtype_scaffold_generated: bool,
    pub graduated_no_std: bool,
}

/// Output of converting a workspace (pure sans-IO: caller writes `files` to disk).
#[derive(Debug, Clone)]
pub struct ConversionResult {
    pub files: BTreeMap<String, String>,
    pub crate_summaries: Vec<CrateConversionSummary>,
    pub crates_at_zero_before: usize,
    pub crates_at_zero_after: usize,
    pub total_external_edges_before: usize,
    pub total_external_edges_after: usize,
    pub total_lifted_machines: usize,
    pub total_refactored_structs: usize,
    pub total_parameterized_structs: usize,
    pub total_cross_crate_call_rewrites: usize,
    pub total_comptime_scaffolds: usize,
    pub total_c_interop_scaffolds: usize,
    pub total_newtype_scaffolds: usize,
}

/// Convert the whole workspace (or only crates matching `only_crates`) using `decisions`.
pub fn convert_workspace(
    snapshot: &WorkspaceSnapshot,
    analysis: &WorkspaceAnalysis,
    decisions: &Decisions,
    only_crates: Option<&BTreeSet<String>>,
) -> ConversionResult {
    let mut out_files = snapshot.files.clone();
    let mut crate_summaries = Vec::new();
    let mut crates_at_zero_before = 0usize;
    let mut crates_at_zero_after = 0usize;
    let mut total_external_edges_before = 0usize;
    let mut total_external_edges_after = 0usize;
    let mut total_lifted_machines = 0usize;
    let mut total_refactored_structs = 0usize;
    let mut total_comptime_scaffolds = 0usize;
    let mut total_c_interop_scaffolds = 0usize;
    let mut total_newtype_scaffolds = 0usize;

    let mut ws_files: Vec<WorkspaceFile> = Vec::new();
    for krate in &analysis.crates {
        if let Some(filter) = only_crates {
            if !filter.contains(&krate.name) {
                continue;
            }
        }
        let summary_idx = crate_summaries.len();
        let summary = convert_one_crate(krate, decisions, &mut out_files);
        let crate_ident = krate.name.replace('-', "_");
        for rs_path in &krate.rs_files {
            if let Some(src) = out_files.get(rs_path).cloned() {
                ws_files.push(WorkspaceFile {
                    summary_idx,
                    crate_ident: crate_ident.clone(),
                    path: rs_path.clone(),
                    src,
                });
            }
        }
        if summary.external_deps_before == 0 {
            crates_at_zero_before += 1;
        }
        if summary.external_deps_after == 0 {
            crates_at_zero_after += 1;
        }
        total_external_edges_before += summary.external_deps_before;
        total_external_edges_after += summary.external_deps_after;
        total_lifted_machines += summary.lifted_machines.len();
        total_refactored_structs += summary.refactored_structs.len();
        if summary.comptime_scaffold_generated {
            total_comptime_scaffolds += 1;
        }
        if summary.c_interop_scaffold_generated {
            total_c_interop_scaffolds += 1;
        }
        if summary.newtype_scaffold_generated {
            total_newtype_scaffolds += 1;
        }
        crate_summaries.push(summary);
    }

    // SOTA 2: parameterize plain owned-heap structs with `<'a>` and explicit `bstd::mem::Alloc<'a>`
    // fields + update matching `impl<'a>` blocks when enabled.
    let alloc_strategy = decisions.alloc_strategy();
    let mut total_parameterized_structs = 0usize;
    if alloc_strategy == AllocStrategy::ExplicitAllocHandle && decisions.parameterize_struct_fields() {
        for wf in &mut ws_files {
            if wf.path == "build.rs" || wf.path.ends_with("/build.rs") {
                continue;
            }
            let mut updated = String::new();
            let stats = rewrite_struct_alloc_fields_into(&wf.src, &mut updated);
            if stats.structs_changed > 0 {
                wf.src = updated;
                total_parameterized_structs += stats.structs_changed;
                if let Some(summary) = crate_summaries.get_mut(wf.summary_idx) {
                    summary.ast_edits_applied += stats.edits_applied;
                }
            }
        }
    }

    // SOTA 1 (Pass B): whole-workspace cross-crate call-graph fixpoint threading `alloc` / `cx` /
    // `clock` / `env` / `rng`, explicit allocator lifetimes & return types, and intra- + cross-crate
    // call-site argument forwarding.
    let total_cross_crate_call_rewrites = propagate_and_rewrite_workspace_capabilities(
        &mut ws_files,
        alloc_strategy,
        &mut |summary_idx, edits| {
            if let Some(summary) = crate_summaries.get_mut(summary_idx) {
                summary.ast_edits_applied += edits;
            }
        },
    );
    for wf in ws_files {
        out_files.insert(wf.path, wf.src);
    }

    ConversionResult {
        files: out_files,
        crate_summaries,
        crates_at_zero_before,
        crates_at_zero_after,
        total_external_edges_before,
        total_external_edges_after,
        total_lifted_machines,
        total_refactored_structs,
        total_parameterized_structs,
        total_cross_crate_call_rewrites,
        total_comptime_scaffolds,
        total_c_interop_scaffolds,
        total_newtype_scaffolds,
    }
}

fn convert_one_crate(
    krate: &CrateAnalysis,
    decisions: &Decisions,
    out_files: &mut BTreeMap<String, String>,
) -> CrateConversionSummary {
    let ext_normal: Vec<&str> = krate
        .deps
        .iter()
        .filter(|d| d.section == DepSection::Normal && !d.is_workspace_internal)
        .map(|d| d.package.as_str())
        .collect();
    let external_deps_before = ext_normal.len();

    let mut removed_deps = Vec::new();
    let mut kept_deps = Vec::new();
    let mut bstd_features: BTreeSet<BstdFeature> = BTreeSet::new();
    let mut extra_blaze_crates: BTreeSet<String> = BTreeSet::new();
    let mut active_crate_rewrites: Vec<(&'static str, &'static str)> = Vec::new();

    for pkg in &ext_normal {
        match decisions.dep_choice(pkg) {
            DepChoice::ReplaceBstd => {
                removed_deps.push((*pkg).to_owned());
                if let Some(m) = lookup_crate(pkg) {
                    if m.bstd_feature != BstdFeature::Core {
                        bstd_features.insert(m.bstd_feature);
                    }
                    if let Some(extra) = m.extra_blaze_crate {
                        extra_blaze_crates.insert(extra.to_owned());
                    }
                    active_crate_rewrites.extend_from_slice(m.path_rewrites);
                }
            }
            DepChoice::WrapSansIoTrait | DepChoice::KeepExternal => {
                kept_deps.push((*pkg).to_owned());
            }
        }
    }

    let mut ast_edits_applied = 0usize;
    let mut lifted_machines_code = String::new();
    let mut lifted_machines = Vec::new();
    let mut used_machine_names: BTreeSet<String> = BTreeSet::new();
    let mut refactored_structs = Vec::new();
    let mut still_uses_std = false;

    let lift_mode = decisions.sm_lift_mode();
    let alloc_strategy = decisions.alloc_strategy();
    let split_structs = decisions.get("struct:split_mixed_io") != Some("report_only");
    let sm_alloc_mode = match alloc_strategy {
        AllocStrategy::KeepAllocForNow => SmAllocMode::PlainAlloc,
        AllocStrategy::ExplicitAllocHandle | AllocStrategy::CxFrameScratch => SmAllocMode::BstdExplicitAlloc,
    };

    // Count how many candidate functions share the same bare name in this crate so we only
    // prefix colliding names (keeping single-occurrence names crisp like `EmitMachine`).
    let mut cand_fn_counts: BTreeMap<&str, usize> = BTreeMap::new();
    for f in &krate.functions {
        if f.is_sm_candidate() {
            *cand_fn_counts.entry(f.name.as_str()).or_default() += 1;
        }
    }

    // Pass A: per-file path, grouped-use, direct implicit-alloc, and direct capability rewrites.
    // (Pass B whole-workspace capability/allocator propagation runs in `convert_workspace`.)
    for (file_idx, rs_path) in krate.rs_files.iter().enumerate() {
        let Some(slot) = out_files.get_mut(rs_path) else {
            continue;
        };
        let (rewritten, edit_count, has_remaining_std) =
            rewrite_rust_source_expressions(slot, &active_crate_rewrites, alloc_strategy);
        ast_edits_applied += edit_count;
        if has_remaining_std {
            still_uses_std = true;
        }
        let src = core::mem::replace(slot, rewritten);

        if lift_mode == SmLiftMode::LiftAllCandidates {
            let file_stem = file_disambiguator(rs_path, file_idx);
            let candidates_in_file: Vec<&str> = krate
                .functions
                .iter()
                .filter(|f| f.file == *rs_path && f.is_sm_candidate())
                .map(|f| f.name.as_str())
                .collect();
            if candidates_in_file.is_empty() {
                continue;
            }
            let any_collision = candidates_in_file
                .iter()
                .any(|name| cand_fn_counts.get(name).copied().unwrap_or(0) > 1);
            let prefix = if any_collision { Some(file_stem.as_str()) } else { None };
            let compiled = compile_file_functions_opts(&src, None, prefix, sm_alloc_mode);
            for sm in compiled {
                if !candidates_in_file.contains(&sm.fn_name.as_str()) {
                    continue;
                }
                if !used_machine_names.insert(sm.machine_name.clone()) {
                    // Two functions with identical name inside the same file (e.g. `impl A::run` and `impl B::run`):
                    // skip duplicate emission rather than producing a colliding struct definition.
                    continue;
                }
                lifted_machines.push(sm.machine_name);
                let _ = writeln!(lifted_machines_code, "// --- Source: {rs_path} ---");
                lifted_machines_code.push_str(&sm.sans_io_code);
                lifted_machines_code.push('\n');
                lifted_machines_code.push_str(&sm.bstd_driver_code);
                lifted_machines_code.push('\n');
                lifted_machines_code.push_str(&sm.sim_test_code);
                lifted_machines_code.push('\n');
            }
        }
    }

    if split_structs {
        let mut used_struct_names: BTreeSet<&str> = BTreeSet::new();
        for cand in &krate.struct_sm_candidates {
            if !used_struct_names.insert(cand.name.as_str()) {
                continue;
            }
            refactored_structs.push(format!("{}Core", cand.name));
            lifted_machines_code.push_str(&compile_struct_sm(cand));
            lifted_machines_code.push('\n');
        }
    }

    if !lifted_machines_code.is_empty() {
        let mut header = String::from(
            "//! Auto-generated explicit sans-IO state machines and thin `bstd` drivers.\n\
             //! Pure state machines carry `#![blaze::sans_io]` and take explicit `bstd::mem::Alloc` + narrowed capabilities.\n\
             #![forbid(unsafe_code)]\n\n",
        );
        header.push_str(&lifted_machines_code);
        let sm_path = if krate.dir.is_empty() {
            "src/sans_io_machines.rs".to_owned()
        } else {
            format!("{}/src/sans_io_machines.rs", krate.dir)
        };
        out_files.insert(sm_path, header);
    }

    let mut comptime_scaffold_generated = false;
    if (krate.is_proc_macro || !krate.proc_macro_entries.is_empty())
        && decisions.comptime_proc_macro_choice(&krate.name) == ComptimeChoice::ComptimeReflection
    {
        let scaffold = generate_comptime_scaffold(&krate.name, &krate.proc_macro_entries);
        let path = if krate.dir.is_empty() {
            "src/comptime_migration.rs".to_owned()
        } else {
            format!("{}/src/comptime_migration.rs", krate.dir)
        };
        out_files.insert(path, scaffold);
        comptime_scaffold_generated = true;
    }

    let mut c_interop_scaffold_generated = false;
    let mut remove_build_deps: Vec<String> = Vec::new();
    let mut blaze_c_toml_section: Option<String> = None;
    if let Some(cinfo) = &krate.c_interop {
        if decisions.c_interop_choice(&krate.name) == CInteropChoice::BlazeCImport {
            let scaffold = generate_c_interop_scaffold(&krate.name, cinfo);
            let path = if krate.dir.is_empty() {
                "src/c_interop_migration.rs".to_owned()
            } else {
                format!("{}/src/c_interop_migration.rs", krate.dir)
            };
            out_files.insert(path, scaffold);
            c_interop_scaffold_generated = true;
            for dep in &krate.deps {
                if dep.section == DepSection::Build && is_c_interop_crate(&dep.package) {
                    remove_build_deps.push(dep.package.clone());
                }
            }
            if !cinfo.source_files.is_empty() || !cinfo.include_dirs.is_empty() || cinfo.is_cpp {
                blaze_c_toml_section = Some(generate_blaze_c_toml_section(cinfo));
            }
        }
    }

    let mut newtype_scaffold_generated = false;
    if !krate.newtype_candidates.is_empty() && decisions.emit_newtypes_module() {
        let mut scaffold = String::new();
        write_newtypes_scaffold(&krate.name, &krate.newtype_candidates, &mut scaffold);
        let path = if krate.dir.is_empty() {
            "src/newtypes.rs".to_owned()
        } else {
            format!("{}/src/newtypes.rs", krate.dir)
        };
        if !out_files.contains_key(&path) {
            out_files.insert(path, scaffold);
            newtype_scaffold_generated = true;
        }
    }

    let needs_bstd = !removed_deps.is_empty()
        || krate.bstd_std_sites > 0
        || !lifted_machines.is_empty()
        || !refactored_structs.is_empty();
    let feat_names: Vec<String> = bstd_features
        .iter()
        .filter_map(|f| f.as_cargo_feature().map(str::to_owned))
        .collect();
    let extra_list: Vec<String> = extra_blaze_crates.into_iter().collect();

    if let Some(manifest_src) = out_files.get_mut(&krate.manifest_path) {
        *manifest_src = rewrite_manifest(
            manifest_src,
            &removed_deps,
            &remove_build_deps,
            needs_bstd,
            decisions.bstd_path.as_deref(),
            &feat_names,
            &extra_list,
            blaze_c_toml_section.as_deref(),
        );
    }

    let mut graduated_no_std = false;
    if !still_uses_std && kept_deps.is_empty() && !krate.is_proc_macro {
        let lib_path = if krate.dir.is_empty() {
            "src/lib.rs".to_owned()
        } else {
            format!("{}/src/lib.rs", krate.dir)
        };
        if let Some(lib_src) = out_files.get_mut(&lib_path) {
            if !lib_src.contains("#![no_std]") && !lib_src.contains("#![cfg_attr(not(test), no_std)]") {
                *lib_src = format!("#![cfg_attr(not(test), no_std)]\n{lib_src}");
                graduated_no_std = true;
            }
        }
    }

    CrateConversionSummary {
        crate_name: krate.name.clone(),
        external_deps_before,
        external_deps_after: kept_deps.len(),
        removed_deps,
        kept_deps,
        added_bstd: needs_bstd,
        bstd_features: feat_names,
        added_blaze_crates: extra_list,
        ast_edits_applied,
        lifted_machines,
        refactored_structs,
        comptime_scaffold_generated,
        c_interop_scaffold_generated,
        newtype_scaffold_generated,
        graduated_no_std,
    }
}

fn file_disambiguator(rs_path: &str, file_idx: usize) -> String {
    let trimmed = rs_path.strip_suffix(".rs").unwrap_or(rs_path);
    let stem = trimmed.rsplit('/').next().unwrap_or("file");
    let parent = trimmed
        .rsplit('/')
        .nth(1)
        .filter(|p| !matches!(*p, "src" | "crates"))
        .unwrap_or("");
    if parent.is_empty() {
        format!("{stem}_{file_idx}")
    } else {
        format!("{parent}_{stem}")
    }
}

/// Map a single leaf path under `std::` for `#![no_std]` foundation crates (`core` + `alloc`,
/// without pulling in `bstd` so crates below `bstd` never create circular dependencies).
pub fn map_std_leaf_no_std_core(rest: &str) -> (&'static str, String) {
    let mod_name = rest.split("::").next().unwrap_or("");
    if rest.starts_with("borrow::Cow") || rest.starts_with("borrow::ToOwned") {
        return ("alloc", rest.to_owned());
    }
    if is_std_core_module(mod_name) && !rest.contains("OsStr") && !rest.contains("CString") {
        return ("core", rest.to_owned());
    }
    if rest.starts_with("sync::atomic") || rest.starts_with("time::Duration") {
        return ("core", rest.to_owned());
    }
    if rest.starts_with("alloc::")
        || rest == "alloc"
        || rest.starts_with("boxed::")
        || rest.starts_with("string::")
        || rest.starts_with("vec::")
        || rest.starts_with("rc::")
        || rest.starts_with("sync::Arc")
        || rest.starts_with("sync::Weak")
        || rest.starts_with("collections::BTreeMap")
        || rest.starts_with("collections::BTreeSet")
        || rest.starts_with("collections::VecDeque")
        || rest.starts_with("collections::BinaryHeap")
        || rest.starts_with("collections::btree_map")
        || rest.starts_with("collections::btree_set")
        || rest.starts_with("collections::vec_deque")
        || rest == "format"
    {
        return ("alloc", rest.to_owned());
    }
    ("std", rest.to_owned())
}

/// Rewrite one Rust source file to `#![no_std]` (`core::` + `alloc::`), preserving whitespace
/// inside generic arguments and splitting grouped `use std::{...};` imports cleanly.
/// Returns `(rewritten_source, edits_applied, has_remaining_std_use, needs_alloc_extern)`.
pub fn rewrite_rust_no_std_core(src: &str) -> (String, usize, bool, bool) {
    let parse = SourceFile::parse(src, Edition::CURRENT);
    let root = parse.tree();
    let mut edits = Vec::new();
    let mut has_remaining_std = false;
    let mut needs_alloc = false;
    let mut grouped_use_ranges: Vec<(usize, usize)> = Vec::new();
    let mut alloc_prelude_needed: BTreeSet<&'static str> = BTreeSet::new();
    let mut existing_imports: BTreeSet<String> = BTreeSet::new();
    let mut local_defs: BTreeSet<String> = BTreeSet::new();

    for node in root.syntax().descendants() {
        if node.ancestors().any(|a| is_cfg_test_item(&a)) {
            continue;
        }
        match node.kind() {
            SyntaxKind::STRUCT | SyntaxKind::ENUM | SyntaxKind::TYPE_ALIAS | SyntaxKind::TRAIT => {
                if let Some(name) = node.children().find(|c| c.kind() == SyntaxKind::NAME) {
                    local_defs.insert(name.text().to_string());
                }
            }
            SyntaxKind::USE => {
                if let Some(use_item) = ast::Use::cast(node) {
                    if let Some(tree) = use_item.use_tree() {
                        let mut leaves = Vec::new();
                        flatten_use_tree("", &tree, &mut leaves);
                        for l in &leaves {
                            let bare = l.split(" as ").last().unwrap_or(l);
                            if let Some(id) = bare.rsplit("::").next() {
                                existing_imports.insert(id.trim().to_owned());
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }

    for node in root.syntax().descendants() {
        // Never rewrite code inside `#[cfg(test)]` modules/functions (tests keep `std`).
        if node.ancestors().any(|a| is_cfg_test_item(&a)) {
            continue;
        }
        match node.kind() {
            SyntaxKind::ATTR => {
                // `#[path = "../..."]` shims include files outside this crate's `src/`: treat as
                // non-automatic so we don't mark external `std` code `#![no_std]` blindly.
                let txt = node.text().to_string();
                if txt.contains("path =") && txt.contains("..") {
                    has_remaining_std = true;
                }
            }
            SyntaxKind::USE => {
                if let Some(use_item) = ast::Use::cast(node) {
                    let Some(tree) = use_item.use_tree() else { continue };
                    let raw = use_item.syntax().text().to_string();
                    if !raw.contains('{') {
                        continue;
                    }
                    let mut leaves = Vec::new();
                    flatten_use_tree("", &tree, &mut leaves);
                    if !leaves.iter().any(|l| l.starts_with("std::") || l.starts_with("::std::")) {
                        continue;
                    }
                    let mut core_leaves = Vec::new();
                    let mut alloc_leaves = Vec::new();
                    let mut std_leaves = Vec::new();
                    for leaf in leaves {
                        let (path_part, alias_part) = match leaf.split_once(" as ") {
                            Some((p, a)) => (p, Some(a)),
                            None => (leaf.as_str(), None),
                        };
                        if let Some(rest) = path_part.strip_prefix("std::").or_else(|| path_part.strip_prefix("::std::")) {
                            let (root_krate, mapped) = map_std_leaf_no_std_core(rest);
                            let full = match alias_part {
                                Some(a) => format!("{mapped} as {a}"),
                                None => mapped,
                            };
                            match root_krate {
                                "core" => core_leaves.push(full),
                                "alloc" => {
                                    needs_alloc = true;
                                    alloc_leaves.push(full);
                                }
                                _ => {
                                    has_remaining_std = true;
                                    std_leaves.push(full);
                                }
                            }
                        } else {
                            std_leaves.push(leaf);
                        }
                    }
                    if core_leaves.is_empty() && alloc_leaves.is_empty() {
                        continue;
                    }
                    let vis_prefix = raw.split("use ").next().unwrap_or("").to_owned();
                    let mut rebuilt = Vec::new();
                    if !core_leaves.is_empty() {
                        if core_leaves.len() == 1 {
                            rebuilt.push(format!("{vis_prefix}use core::{};", core_leaves[0]));
                        } else {
                            rebuilt.push(format!("{vis_prefix}use core::{{{}}};", core_leaves.join(", ")));
                        }
                    }
                    if !alloc_leaves.is_empty() {
                        if alloc_leaves.len() == 1 {
                            rebuilt.push(format!("{vis_prefix}use alloc::{};", alloc_leaves[0]));
                        } else {
                            rebuilt.push(format!("{vis_prefix}use alloc::{{{}}};", alloc_leaves.join(", ")));
                        }
                    }
                    if !std_leaves.is_empty() {
                        if std_leaves.len() == 1 {
                            rebuilt.push(format!("{vis_prefix}use std::{};", std_leaves[0]));
                        } else {
                            rebuilt.push(format!("{vis_prefix}use std::{{{}}};", std_leaves.join(", ")));
                        }
                    }
                    let range = use_item.syntax().text_range();
                    let start: usize = range.start().into();
                    let end: usize = range.end().into();
                    grouped_use_ranges.push((start, end));
                    edits.push(SpanEdit { start, end, replacement: rebuilt.join("\n") });
                }
            }
            SyntaxKind::PATH => {
                if node.parent().is_some_and(|p| p.kind() == SyntaxKind::PATH) {
                    continue;
                }
                let range = node.text_range();
                let start: usize = range.start().into();
                let end: usize = range.end().into();
                if grouped_use_ranges.iter().any(|&(gs, ge)| start >= gs && end <= ge) {
                    continue;
                }
                let raw = node.text().to_string();
                let (raw_head, raw_tail) = match raw.find('<') {
                    Some(idx) => (&raw[..idx], &raw[idx..]),
                    None => (raw.as_str(), ""),
                };
                let compact_head: String = raw_head.chars().filter(|c| !c.is_whitespace()).collect();
                if let Some(rest) = compact_head.strip_prefix("std::") {
                    let (root_krate, mapped) = map_std_leaf_no_std_core(rest);
                    if root_krate == "std" {
                        has_remaining_std = true;
                    } else {
                        if root_krate == "alloc" {
                            needs_alloc = true;
                        }
                        edits.push(SpanEdit {
                            start,
                            end,
                            replacement: format!("{root_krate}::{mapped}{raw_tail}"),
                        });
                    }
                } else {
                    let first_seg = compact_head.split("::").next().unwrap_or("");
                    if !existing_imports.contains(first_seg) && !local_defs.contains(first_seg) {
                        match first_seg {
                            "Vec" => {
                                needs_alloc = true;
                                alloc_prelude_needed.insert("vec::Vec");
                            }
                            "String" => {
                                needs_alloc = true;
                                alloc_prelude_needed.insert("string::String");
                            }
                            "Box" => {
                                needs_alloc = true;
                                alloc_prelude_needed.insert("boxed::Box");
                            }
                            "ToOwned" => {
                                needs_alloc = true;
                                alloc_prelude_needed.insert("borrow::ToOwned");
                            }
                            "ToString" => {
                                needs_alloc = true;
                                alloc_prelude_needed.insert("string::ToString");
                            }
                            _ => {}
                        }
                    }
                }
            }
            SyntaxKind::METHOD_CALL_EXPR => {
                if let Some(mc) = ast::MethodCallExpr::cast(node) {
                    let m = mc.name_ref().map(|n| n.text().to_string()).unwrap_or_default();
                    if m == "to_owned" && !existing_imports.contains("ToOwned") {
                        needs_alloc = true;
                        alloc_prelude_needed.insert("borrow::ToOwned");
                    } else if m == "to_string" && !existing_imports.contains("ToString") {
                        needs_alloc = true;
                        alloc_prelude_needed.insert("string::ToString");
                    }
                }
            }
            SyntaxKind::MACRO_CALL => {
                if let Some(mc) = ast::MacroCall::cast(node) {
                    let mname = mc.path().map(|p| p.syntax().text().to_string()).unwrap_or_default();
                    let leaf = mname.rsplit("::").next().unwrap_or(&mname).trim();
                    if leaf == "format" && !existing_imports.contains("format") {
                        needs_alloc = true;
                        alloc_prelude_needed.insert("format");
                    } else if leaf == "vec" && !existing_imports.contains("vec") {
                        needs_alloc = true;
                        alloc_prelude_needed.insert("vec");
                    } else if matches!(leaf, "println" | "eprintln" | "print" | "eprint" | "thread_local") {
                        has_remaining_std = true;
                    }
                }
            }
            _ => {}
        }
    }

    let mut count = edits.len();
    let mut rewritten = apply_span_edits(src, edits);

    // Inject missing `alloc` prelude imports cleanly right after inner attributes / doc comments!
    if !has_remaining_std && !alloc_prelude_needed.is_empty() {
        let items: Vec<&str> = alloc_prelude_needed.into_iter().collect();
        let use_line = if items.len() == 1 {
            format!("use alloc::{};\n", items[0])
        } else {
            format!("use alloc::{{{}}};\n", items.join(", "))
        };
        let pos = after_header_insert_pos(&rewritten);
        let mut with_use = String::with_capacity(rewritten.len() + use_line.len() + 1);
        with_use.push_str(&rewritten[..pos]);
        with_use.push_str(&use_line);
        with_use.push_str(&rewritten[pos..]);
        rewritten = with_use;
        count += 1;
    }

    (rewritten, count, has_remaining_std, needs_alloc)
}

fn after_header_insert_pos(src: &str) -> usize {
    let mut pos = 0usize;
    let mut in_attr = false;
    for line in src.split_inclusive('\n') {
        let t = line.trim();
        if in_attr {
            pos += line.len();
            if t.ends_with(")]") || t.ends_with(']') {
                in_attr = false;
            }
            continue;
        }
        if t.starts_with("//!") || t.starts_with("//") || t.is_empty() || t.starts_with("extern crate ") {
            pos += line.len();
        } else if t.starts_with("#![") {
            pos += line.len();
            if !(t.ends_with(")]") || t.ends_with(']')) {
                in_attr = true;
            }
        } else {
            break;
        }
    }
    pos
}

fn is_cfg_test_item(node: &syntax::SyntaxNode) -> bool {
    if !matches!(
        node.kind(),
        SyntaxKind::MODULE
            | SyntaxKind::FN
            | SyntaxKind::IMPL
            | SyntaxKind::MACRO_CALL
            | SyntaxKind::STRUCT
            | SyntaxKind::ENUM
            | SyntaxKind::STATIC
            | SyntaxKind::CONST
    ) {
        return false;
    }
    for child in node.children() {
        if child.kind() == SyntaxKind::ATTR {
            let t = child.text().to_string();
            if t.contains("cfg(test)") || t.contains("#[test]") || t.contains("::test]") {
                return true;
            }
        }
    }
    false
}

/// Insert `#![cfg_attr(not(test), no_std)]` (and optional `extern crate alloc;`) cleanly after
/// leading crate doc comments (`//!`) and inner attributes (`#![...]`).
pub fn insert_no_std_header(lib_src: &str, add_extern_alloc: bool) -> String {
    let already_no_std = lib_src.contains("#![no_std]") || lib_src.contains("#![cfg_attr(not(test), no_std)]");
    let already_alloc = lib_src.contains("extern crate alloc;");
    if already_no_std && (!add_extern_alloc || already_alloc) {
        return lib_src.to_owned();
    }

    // Find the byte offset right after leading `//!` doc comments and blank lines.
    let mut insert_pos = 0usize;
    for line in lib_src.split_inclusive('\n') {
        let t = line.trim();
        if t.starts_with("//!") || t.starts_with("//") || t.is_empty() {
            insert_pos += line.len();
        } else {
            break;
        }
    }

    let mut header = String::new();
    if !already_no_std {
        header.push_str("#![cfg_attr(not(test), no_std)]\n");
    }
    let mut out = String::with_capacity(lib_src.len() + 64);
    out.push_str(&lib_src[..insert_pos]);
    out.push_str(&header);
    out.push_str(&lib_src[insert_pos..]);

    if add_extern_alloc && !already_alloc {
        // Place `extern crate alloc;` after all `#![...]` inner attributes.
        let mut alloc_pos = 0usize;
        let mut in_attr = false;
        for line in out.split_inclusive('\n') {
            let t = line.trim();
            if in_attr {
                alloc_pos += line.len();
                if t.ends_with(")]") || t.ends_with(']') {
                    in_attr = false;
                }
                continue;
            }
            if t.starts_with("//!") || t.starts_with("//") || t.is_empty() {
                alloc_pos += line.len();
            } else if t.starts_with("#![") {
                alloc_pos += line.len();
                if !(t.ends_with(")]") || t.ends_with(']')) {
                    in_attr = true;
                }
            } else {
                break;
            }
        }
        let mut final_out = String::with_capacity(out.len() + 32);
        final_out.push_str(&out[..alloc_pos]);
        final_out.push_str("\nextern crate alloc;\n");
        final_out.push_str(&out[alloc_pos..]);
        return final_out;
    }

    out
}

/// Map a single leaf path under `std::` (e.g. `mem::swap`, `collections::HashMap`, `fs::read`)
/// into its `(target_root, leaf_path)` (`"core"`, `"bstd"`, or `"std"` if still std-only).
fn map_std_leaf(rest: &str) -> (&'static str, String) {
    let mod_name = rest.split("::").next().unwrap_or("");
    if is_std_core_module(mod_name) && !rest.contains("OsStr") && !rest.contains("CString") {
        return ("core", rest.to_owned());
    }
    if rest.starts_with("sync::atomic") {
        return ("core", rest.to_owned());
    }
    match rest {
        "collections::HashMap" => ("bstd", "map::HashMap".to_owned()),
        "collections::HashSet" => ("bstd", "set::HashSet".to_owned()),
        "collections::BTreeMap" => ("bstd", "map::BTreeMap".to_owned()),
        "collections::BTreeSet" => ("bstd", "set::BTreeSet".to_owned()),
        "time::Duration" => ("core", "time::Duration".to_owned()),
        "time::Instant" => ("bstd", "clock::Instant".to_owned()),
        "sync::Mutex" => ("bstd", "sync::Lock".to_owned()),
        "sync::RwLock" => ("bstd", "sync::ReadLock".to_owned()),
        "sync::Arc" => ("bstd", "boxed::Arc".to_owned()),
        "rc::Rc" => ("bstd", "boxed::Rc".to_owned()),
        _ => ("std", rest.to_owned()),
    }
}

/// Flatten a `use` tree into individual `(full_path, alias)` leaves so grouped `use std::{...};`
/// imports can be cleanly partitioned into `core::`, `bstd::`, and remaining `std::`.
fn flatten_use_tree(prefix: &str, tree: &ast::UseTree, out: &mut Vec<String>) {
    let seg = tree.path().map(|p| {
        p.syntax()
            .text()
            .to_string()
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect::<String>()
    });
    let combined = match (prefix.is_empty(), seg.as_deref()) {
        (true, Some(s)) => s.to_owned(),
        (false, Some("self")) => prefix.to_owned(),
        (false, Some(s)) => format!("{prefix}::{s}"),
        (_, None) => prefix.to_owned(),
    };
    if let Some(list) = tree.use_tree_list() {
        for child in list.use_trees() {
            flatten_use_tree(&combined, &child, out);
        }
    } else {
        let mut leaf = combined;
        if tree.star_token().is_some() {
            if !leaf.is_empty() {
                leaf.push_str("::*");
            }
        }
        if let Some(rename) = tree.rename() {
            let rtxt = rename.syntax().text().to_string();
            leaf.push(' ');
            leaf.push_str(rtxt.trim());
        }
        if !leaf.is_empty() {
            out.push(leaf);
        }
    }
}

/// Rewrite one Rust source file via `syntax::SourceFile::parse` + span-checked edits:
/// - Pass A: splits & rewrites grouped `use std::{...}`, third-party crate paths, `std::` paths,
///   implicit-heap calls (`Vec::new`, `Vec::with_capacity`, `String::new`, `String::with_capacity`,
///   `String::from`, `Box::new`, `Arc::new`, `Rc::new`, `BTreeMap::new`, `BTreeSet::new`,
///   `HashMap::new`, `HashMap::with_capacity`, `HashSet::new`, `"lit".to_string()`, `.to_vec()`,
///   `format!`, `vec!`), and ambient capability calls (`Instant::now()`, `SystemTime::now()`,
///   `env::var("K")`, `rand::thread_rng()`).
/// - Pass B: propagates explicit `alloc` / `cx` / `clock` / `env` / `rng` parameters across the
///   call graph, adds explicit allocator lifetimes (`'a`) + rewrites heap return types, and forwards
///   arguments at intra-crate call sites.
pub fn rewrite_rust_source(
    src: &str,
    crate_rewrites: &[(&str, &str)],
    alloc_strategy: AllocStrategy,
) -> (String, usize, bool) {
    let (pass_a, count_a, has_remaining_std) =
        rewrite_rust_source_expressions(src, crate_rewrites, alloc_strategy);
    let mut bufs = vec![WorkspaceFile {
        summary_idx: 0,
        crate_ident: "local".to_owned(),
        path: "src/lib.rs".to_owned(),
        src: pass_a,
    }];
    let mut count_b = 0usize;
    let _ = propagate_and_rewrite_workspace_capabilities(
        &mut bufs,
        alloc_strategy,
        &mut |_, edits| count_b += edits,
    );
    (bufs.into_iter().next().map(|wf| wf.src).unwrap_or_default(), count_a + count_b, has_remaining_std)
}

/// Pass A: rewrite paths, grouped `use`, direct implicit-alloc expressions, and direct capability
/// calls inside threadable functions without modifying function signatures yet.
fn rewrite_rust_source_expressions(
    src: &str,
    crate_rewrites: &[(&str, &str)],
    alloc_strategy: AllocStrategy,
) -> (String, usize, bool) {
    let parse = SourceFile::parse(src, Edition::CURRENT);
    let root = parse.tree();
    let mut edits = Vec::new();
    let mut has_remaining_std = false;

    // Pre-collect `use` statements with grouped `std::{...}` trees so we rewrite the whole `use`
    // statement cleanly without inner `PATH` edits conflicting.
    let mut grouped_use_ranges: Vec<(usize, usize)> = Vec::new();
    let mut needs_collect_in_import = false;

    for node in root.syntax().descendants() {
        match node.kind() {
            SyntaxKind::ATTR => {
                // `#[path = "../..."]` shims include files outside this crate's `src/`: treat as
                // non-automatic so we don't mark external `std` code `#![no_std]` blindly.
                let txt = node.text().to_string();
                if txt.contains("path =") && txt.contains("..") {
                    has_remaining_std = true;
                }
            }
            SyntaxKind::USE => {
                if let Some(use_item) = ast::Use::cast(node) {
                    let Some(tree) = use_item.use_tree() else { continue };
                    let raw = use_item.syntax().text().to_string();
                    if !raw.contains('{') {
                        continue;
                    }
                    let mut leaves = Vec::new();
                    flatten_use_tree("", &tree, &mut leaves);
                    let any_std = leaves.iter().any(|l| l.starts_with("std::") || l.starts_with("::std::"));
                    if !any_std {
                        continue;
                    }
                    let mut core_leaves = Vec::new();
                    let mut bstd_leaves = Vec::new();
                    let mut std_leaves = Vec::new();
                    for leaf in leaves {
                        let (path_part, alias_part) = match leaf.split_once(" as ") {
                            Some((p, a)) => (p, Some(a)),
                            None => (leaf.as_str(), None),
                        };
                        if let Some(rest) = path_part.strip_prefix("std::").or_else(|| path_part.strip_prefix("::std::")) {
                            let (root_krate, mapped) = map_std_leaf(rest);
                            let full = match alias_part {
                                Some(a) => format!("{mapped} as {a}"),
                                None => mapped,
                            };
                            match root_krate {
                                "core" => core_leaves.push(full),
                                "bstd" => bstd_leaves.push(full),
                                _ => {
                                    has_remaining_std = true;
                                    std_leaves.push(full);
                                }
                            }
                        } else {
                            std_leaves.push(leaf);
                        }
                    }

                    if core_leaves.is_empty() && bstd_leaves.is_empty() {
                        continue;
                    }

                    let vis_prefix = raw
                        .split("use ")
                        .next()
                        .unwrap_or("")
                        .to_owned();
                    let mut rebuilt = Vec::new();
                    if !core_leaves.is_empty() {
                        if core_leaves.len() == 1 {
                            rebuilt.push(format!("{vis_prefix}use core::{};", core_leaves[0]));
                        } else {
                            rebuilt.push(format!("{vis_prefix}use core::{{{}}};", core_leaves.join(", ")));
                        }
                    }
                    if !bstd_leaves.is_empty() {
                        if bstd_leaves.len() == 1 {
                            rebuilt.push(format!("{vis_prefix}use bstd::{};", bstd_leaves[0]));
                        } else {
                            rebuilt.push(format!("{vis_prefix}use bstd::{{{}}};", bstd_leaves.join(", ")));
                        }
                    }
                    if !std_leaves.is_empty() {
                        if std_leaves.len() == 1 {
                            rebuilt.push(format!("{vis_prefix}use std::{};", std_leaves[0]));
                        } else {
                            rebuilt.push(format!("{vis_prefix}use std::{{{}}};", std_leaves.join(", ")));
                        }
                    }
                    let range = use_item.syntax().text_range();
                    let start: usize = range.start().into();
                    let end: usize = range.end().into();
                    grouped_use_ranges.push((start, end));
                    edits.push(SpanEdit { start, end, replacement: rebuilt.join("\n") });
                }
            }
            SyntaxKind::PATH => {
                if node.parent().is_some_and(|p| p.kind() == SyntaxKind::PATH) {
                    continue;
                }
                let range = node.text_range();
                let start: usize = range.start().into();
                let end: usize = range.end().into();
                if grouped_use_ranges.iter().any(|&(gs, ge)| start >= gs && end <= ge) {
                    continue;
                }
                let raw = node.text().to_string();
                // Keep generic arguments (`<...>`) untouched with their original whitespace so
                // types like `RefCell<Vec<(String, &'static str, *const Sampler)>>` stay intact!
                let (raw_head, raw_tail) = match raw.find('<') {
                    Some(idx) => (&raw[..idx], &raw[idx..]),
                    None => (raw.as_str(), ""),
                };
                let compact_head: String = raw_head.chars().filter(|c| !c.is_whitespace()).collect();

                let mut matched_third_party = false;
                for &(old_pfx, new_pfx) in crate_rewrites {
                    if compact_head == old_pfx {
                        edits.push(SpanEdit {
                            start,
                            end,
                            replacement: format!("{new_pfx}{raw_tail}"),
                        });
                        matched_third_party = true;
                        break;
                    } else if let Some(rest) = compact_head.strip_prefix(old_pfx) {
                        if old_pfx.ends_with("::") || rest.starts_with("::") {
                            edits.push(SpanEdit {
                                start,
                                end,
                                replacement: format!("{new_pfx}{rest}{raw_tail}"),
                            });
                            matched_third_party = true;
                            break;
                        }
                    }
                }
                if matched_third_party {
                    continue;
                }

                if let Some(rest) = compact_head.strip_prefix("std::") {
                    let (root_krate, mapped) = map_std_leaf(rest);
                    if root_krate == "std" {
                        has_remaining_std = true;
                    } else {
                        edits.push(SpanEdit {
                            start,
                            end,
                            replacement: format!("{root_krate}::{mapped}{raw_tail}"),
                        });
                    }
                }
            }
            SyntaxKind::CALL_EXPR => {
                let inside_threadable = enclosing_threadable_fn(&node).is_some();
                if !inside_threadable {
                    continue;
                }
                let Some(call) = ast::CallExpr::cast(node) else { continue };
                let txt = call.syntax().text().to_string();
                let compact: String = txt.chars().filter(|c| !c.is_whitespace()).collect();

                // 1. Direct capability call rewrites (`Instant::now()`, `SystemTime::now()`,
                //    `env::var("K")`, `rand::thread_rng()`, `rand::rng()`).
                if compact == "Instant::now()" || compact == "std::time::Instant::now()" {
                    let range = call.syntax().text_range();
                    edits.push(SpanEdit {
                        start: range.start().into(),
                        end: range.end().into(),
                        replacement: "clock.now()".to_owned(),
                    });
                    continue;
                }
                if compact == "SystemTime::now()" || compact == "std::time::SystemTime::now()" {
                    let range = call.syntax().text_range();
                    edits.push(SpanEdit {
                        start: range.start().into(),
                        end: range.end().into(),
                        replacement: "clock.wall()".to_owned(),
                    });
                    continue;
                }
                if compact == "rand::thread_rng()" || compact == "rand::rng()" || compact == "thread_rng()" {
                    let range = call.syntax().text_range();
                    edits.push(SpanEdit {
                        start: range.start().into(),
                        end: range.end().into(),
                        replacement: "rng".to_owned(),
                    });
                    continue;
                }
                if compact.starts_with("env::var(") || compact.starts_with("std::env::var(") {
                    if let Some(arg) = call.arg_list().and_then(|al| al.args().next()) {
                        let a_txt = arg.syntax().text().to_string();
                        let a_trim = a_txt.trim();
                        if a_trim.starts_with('"') && a_trim.ends_with('"') && a_trim.len() >= 2 {
                            let range = call.syntax().text_range();
                            edits.push(SpanEdit {
                                start: range.start().into(),
                                end: range.end().into(),
                                replacement: format!("env.get(&bstd::var!({a_trim}))"),
                            });
                            continue;
                        }
                    }
                }

                // 2. Explicit allocator call rewrites (`Vec`, `String`, `Box`, `Arc`, `Rc`,
                //    `BTreeMap`, `BTreeSet`, `HashMap`, `HashSet`).
                if alloc_strategy == AllocStrategy::KeepAllocForNow {
                    continue;
                }
                let alloc_arg = match alloc_strategy {
                    AllocStrategy::ExplicitAllocHandle => "alloc",
                    AllocStrategy::CxFrameScratch => "cx.frame().alloc()",
                    AllocStrategy::KeepAllocForNow => unreachable!(),
                };
                let first_arg = || {
                    call.arg_list()
                        .and_then(|al| al.args().next())
                        .map(|a| a.syntax().text().to_string().trim().to_owned())
                };
                let replacement = match compact.as_str() {
                    "Vec::new()" => Some(format!("bstd::vec::Vec::new_in({alloc_arg})")),
                    "String::new()" => Some(format!("bstd::mem::String::new_in({alloc_arg})")),
                    "BTreeMap::new()" => Some(format!("bstd::map::BTreeMap::new_in({alloc_arg})")),
                    "BTreeSet::new()" => Some(format!("bstd::set::BTreeSet::new_in({alloc_arg})")),
                    "HashMap::new()" | "FxHashMap::default()" => Some(format!(
                        "bstd::map::Map::new(bstd::map::MapOpts {{ hasher: bstd::hash::FixedState, alloc: {alloc_arg}, capacity: None }})"
                    )),
                    "HashSet::new()" | "FxHashSet::default()" => Some(format!(
                        "bstd::set::Set::new(bstd::set::SetOpts {{ hasher: bstd::hash::FixedState, alloc: {alloc_arg}, capacity: None }})"
                    )),
                    _ if compact.starts_with("Vec::with_capacity(") => first_arg()
                        .map(|a| format!("bstd::vec::Vec::with_capacity_in({a}, {alloc_arg})")),
                    _ if compact.starts_with("String::with_capacity(") => first_arg()
                        .map(|a| format!("bstd::mem::String::with_capacity_in({a}, {alloc_arg})")),
                    _ if compact.starts_with("String::from(") => first_arg()
                        .map(|a| format!("bstd::mem::String::from_str_in({a}, {alloc_arg})")),
                    _ if compact.starts_with("Box::new(") => first_arg()
                        .map(|a| format!("bstd::boxed::Box::new_in({a}, {alloc_arg})")),
                    _ if compact.starts_with("Arc::new(") => first_arg()
                        .map(|a| format!("bstd::boxed::Arc::new_in({a}, {alloc_arg})")),
                    _ if compact.starts_with("Rc::new(") => first_arg()
                        .map(|a| format!("bstd::boxed::Rc::new_in({a}, {alloc_arg})")),
                    _ if compact.starts_with("HashMap::with_capacity(") => first_arg().map(|a| {
                        format!(
                            "bstd::map::Map::new(bstd::map::MapOpts {{ hasher: bstd::hash::FixedState, alloc: {alloc_arg}, capacity: Some({a}) }})"
                        )
                    }),
                    _ if compact.starts_with("HashSet::with_capacity(") => first_arg().map(|a| {
                        format!(
                            "bstd::set::Set::new(bstd::set::SetOpts {{ hasher: bstd::hash::FixedState, alloc: {alloc_arg}, capacity: Some({a}) }})"
                        )
                    }),
                    _ => None,
                };
                if let Some(rep) = replacement {
                    let range = call.syntax().text_range();
                    edits.push(SpanEdit {
                        start: range.start().into(),
                        end: range.end().into(),
                        replacement: rep,
                    });
                }
            }
            SyntaxKind::METHOD_CALL_EXPR => {
                if alloc_strategy == AllocStrategy::KeepAllocForNow {
                    continue;
                }
                if enclosing_threadable_fn(&node).is_none() {
                    continue;
                }
                let Some(mc) = ast::MethodCallExpr::cast(node) else { continue };
                let Some(mname_ref) = mc.name_ref() else { continue };
                let mname = mname_ref.text().to_string();
                let has_args = mc.arg_list().is_some_and(|al| al.args().next().is_some());
                if has_args {
                    continue;
                }
                let Some(recv) = mc.receiver() else { continue };
                let recv_txt = recv.syntax().text().to_string();
                let recv_trim = recv_txt.trim();
                let alloc_arg = match alloc_strategy {
                    AllocStrategy::ExplicitAllocHandle => "alloc",
                    AllocStrategy::CxFrameScratch => "cx.frame().alloc()",
                    AllocStrategy::KeepAllocForNow => unreachable!(),
                };
                // SOTA 3: `.collect::<Vec<_>>()` / `.collect::<Vec<T>>()` -> `.collect_in(alloc)`.
                if mname == "collect" {
                    if let Some(gal) = mc.generic_arg_list() {
                        let gtxt: String = gal.syntax().text().to_string().chars().filter(|c| !c.is_whitespace()).collect();
                        let is_std_vec = (gtxt.starts_with("::<Vec<")
                            || gtxt.starts_with("::<std::vec::Vec<")
                            || gtxt.starts_with("::<alloc::vec::Vec<"))
                            && !gtxt.contains(',');
                        if is_std_vec {
                            let start: usize = mname_ref.syntax().text_range().start().into();
                            let end: usize = mc.syntax().text_range().end().into();
                            edits.push(SpanEdit {
                                start,
                                end,
                                replacement: format!("collect_in({alloc_arg})"),
                            });
                            needs_collect_in_import = true;
                            continue;
                        }
                    }
                }
                let replacement = match mname.as_str() {
                    "to_string" | "to_owned"
                        if recv_trim.starts_with('"') && recv_trim.ends_with('"') && recv_trim.len() >= 2 =>
                    {
                        Some(format!("bstd::mem::String::from_str_in({recv_trim}, {alloc_arg})"))
                    }
                    "to_vec" => Some(format!("bstd::vec::Vec::from_slice_in(&({recv_trim}), {alloc_arg})")),
                    _ => None,
                };
                if let Some(rep) = replacement {
                    let range = mc.syntax().text_range();
                    edits.push(SpanEdit {
                        start: range.start().into(),
                        end: range.end().into(),
                        replacement: rep,
                    });
                }
            }
            SyntaxKind::MACRO_CALL => {
                if alloc_strategy == AllocStrategy::KeepAllocForNow {
                    continue;
                }
                if enclosing_threadable_fn(&node).is_none() {
                    continue;
                }
                let Some(mc) = ast::MacroCall::cast(node) else { continue };
                let mname = mc.path().map(|p| p.syntax().text().to_string()).unwrap_or_default();
                let leaf = mname.rsplit("::").next().unwrap_or(&mname).trim();
                let alloc_arg = match alloc_strategy {
                    AllocStrategy::ExplicitAllocHandle => "alloc",
                    AllocStrategy::CxFrameScratch => "cx.frame().alloc()",
                    AllocStrategy::KeepAllocForNow => unreachable!(),
                };
                let replacement = match leaf {
                    "format" => mc.token_tree().map(|tt| {
                        let raw = tt.syntax().text().to_string();
                        let inner = strip_outer_delim(&raw);
                        format!("bstd::mem::format_in!({alloc_arg}, {inner})")
                    }),
                    "vec" => mc.token_tree().map(|tt| {
                        let raw = tt.syntax().text().to_string();
                        let inner = strip_outer_delim(&raw);
                        if inner.is_empty() {
                            format!("bstd::vec::Vec::new_in({alloc_arg})")
                        } else {
                            format!("bstd::vec::vec_in![{alloc_arg}; {inner}]")
                        }
                    }),
                    _ => None,
                };
                if let Some(rep) = replacement {
                    let range = mc.syntax().text_range();
                    edits.push(SpanEdit {
                        start: range.start().into(),
                        end: range.end().into(),
                        replacement: rep,
                    });
                }
            }
            _ => {}
        }
    }

    if needs_collect_in_import && !src.contains("bstd::vec::CollectIn") {
        let pos = after_header_insert_pos(src);
        edits.push(SpanEdit {
            start: pos,
            end: pos,
            replacement: "use bstd::vec::CollectIn as _;\n".to_owned(),
        });
    }

    let count = edits.len();
    let rewritten = apply_span_edits(src, edits);
    (rewritten, count, has_remaining_std)
}

/// Capability or explicit allocator parameter threaded across a crate's call graph.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
enum CapParam {
    Alloc,
    Cx,
    Clock,
    Env,
    Rng,
}

impl CapParam {
    const fn param_name(self) -> &'static str {
        match self {
            Self::Alloc => "alloc",
            Self::Cx => "cx",
            Self::Clock => "clock",
            Self::Env => "env",
            Self::Rng => "rng",
        }
    }

    fn param_decl(self, alloc_lt: Option<&str>) -> String {
        match self {
            Self::Alloc => match alloc_lt {
                Some(lt) => format!("alloc: bstd::mem::Alloc<{lt}>"),
                None => "alloc: bstd::mem::Alloc<'_>".to_owned(),
            },
            Self::Cx => "cx: &bstd::Cx".to_owned(),
            Self::Clock => "clock: &bstd::clock::Clock".to_owned(),
            Self::Env => "env: &bstd::env::Env".to_owned(),
            Self::Rng => "rng: &mut bstd::rng::Rng".to_owned(),
        }
    }
}

/// Upper bound on call-graph capability propagation iterations (`bounded-progress`).
const MAX_CAP_PROP_ITERS: usize = 32;

#[derive(Debug, Clone)]
struct FnCapInfo {
    file_idx: usize,
    summary_idx: usize,
    crate_ident: String,
    fn_start: usize,
    name: String,
    is_pub: bool,
    is_method: bool,
    already_has: BTreeSet<CapParam>,
    needed: BTreeSet<CapParam>,
    /// Intra-crate free-fn calls (`foo(...)` or `mod::foo(...)`).
    free_callees: BTreeSet<String>,
    /// Cross-crate qualified or imported calls `(target_crate_ident, fn_name)`.
    cross_crate_callees: BTreeSet<(String, String)>,
    method_callees: BTreeSet<String>,
}

/// One workspace Rust source file buffered for whole-workspace Pass B propagation.
struct WorkspaceFile {
    summary_idx: usize,
    crate_ident: String,
    path: String,
    src: String,
}

/// Named result of [`rewrite_struct_alloc_fields_into`] (`newtypes-and-named-returns`, rule 85).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StructAllocStats {
    pub structs_changed: usize,
    pub edits_applied: usize,
}

/// Extract `use other_crate::...::item [as alias];` imports in a file for cross-crate call resolution.
fn collect_cross_crate_imports(
    root: &ast::SourceFile,
    known_crates: &[&str],
    own_crate: &str,
    emit: &mut impl FnMut(&str, &str, &str),
) {
    for node in root.syntax().descendants() {
        if node.kind() != SyntaxKind::USE {
            continue;
        }
        let Some(use_item) = ast::Use::cast(node) else { continue };
        let Some(tree) = use_item.use_tree() else { continue };
        let mut leaves = Vec::new();
        flatten_use_tree("", &tree, &mut leaves);
        for leaf in leaves {
            let (path_part, alias_part) = match leaf.split_once(" as ") {
                Some((p, a)) => (p.trim(), Some(a.trim())),
                None => (leaf.trim(), None),
            };
            let cleaned = path_part.strip_prefix("::").unwrap_or(path_part);
            let mut segs = cleaned.split("::");
            let Some(first) = segs.next() else { continue };
            if first == own_crate || !known_crates.contains(&first) {
                continue;
            }
            let Some(last) = cleaned.rsplit("::").next() else { continue };
            if last == "*" || !last.starts_with(|c: char| c.is_ascii_lowercase()) {
                continue;
            }
            let local_name = alias_part.unwrap_or(last);
            emit(local_name, first, last);
        }
    }
}

/// Pass B (SOTA 1): whole-workspace cross-crate call-graph fixpoint that:
/// 1. Detects direct capability/allocator uses introduced by Pass A in every threadable function
///    (including `.collect_in(alloc)`),
/// 2. Propagates those parameters upward through both intra-crate AND cross-crate workspace-internal
///    callers (`other_crate::foo(...)` and `use other_crate::foo; foo(...)`) to a fixpoint,
/// 3. Rewrites every affected function's `ParamList`, generic lifetime (`'a` / `'alloc`), and
///    owned-heap return types (`String` / `Vec<T>` / `Box<T>` / `HashMap` / `BTreeMap` / etc.),
/// 4. Forwards the added arguments at every intra-crate and cross-crate call site (passing
///    `bstd::mem::Alloc::global()` at `main` / `#[test]` root boundaries).
///
/// Invokes `on_crate_edit(summary_idx, edits)` for each modified file and returns the total
/// cross-crate call-site rewrites applied.
fn propagate_and_rewrite_workspace_capabilities(
    files: &mut [WorkspaceFile],
    alloc_strategy: AllocStrategy,
    on_crate_edit: &mut impl FnMut(usize, usize),
) -> usize {
    let known_crates: BTreeSet<String> = files.iter().map(|wf| wf.crate_ident.clone()).collect();
    let known_crates_slice: Vec<&str> = known_crates.iter().map(String::as_str).collect();

    // 1. Collect all threadable functions & per-file cross-crate imports across the workspace.
    let mut fns: Vec<FnCapInfo> = Vec::new();
    let mut file_imports: Vec<BTreeMap<String, (String, String)>> = Vec::with_capacity(files.len());

    for (file_idx, wf) in files.iter().enumerate() {
        let summary_idx = &wf.summary_idx;
        let crate_ident = &wf.crate_ident;
        let path = &wf.path;
        let src = &wf.src;
        if path == "build.rs" || path.ends_with("/build.rs") {
            file_imports.push(BTreeMap::new());
            continue;
        }
        let parse = SourceFile::parse(src, Edition::CURRENT);
        let root = parse.tree();
        let mut imports: BTreeMap<String, (String, String)> = BTreeMap::new();
        collect_cross_crate_imports(
            &root,
            &known_crates_slice,
            crate_ident,
            &mut |local, krate, item| {
                imports.insert(local.to_owned(), (krate.to_owned(), item.to_owned()));
            },
        );
        for node in root.syntax().descendants() {
            let Some(func) = ast::Fn::cast(node) else { continue };
            if !is_threadable_fn(&func) {
                continue;
            }
            let Some(name_node) = func.name() else { continue };
            let name = name_node.text().to_string();
            let is_method = func
                .param_list()
                .is_some_and(|pl| pl.self_param().is_some());
            let is_pub = !is_method
                && func
                    .visibility()
                    .is_some_and(|v| v.syntax().text().to_string().trim() == "pub");

            let mut already_has = BTreeSet::new();
            if let Some(plist) = func.param_list() {
                for p in plist.params() {
                    if let Some(pat) = p.pat() {
                        let pname = pat.syntax().text().to_string();
                        let pname = pname.trim().trim_start_matches("mut ").trim();
                        for cap in [CapParam::Alloc, CapParam::Cx, CapParam::Clock, CapParam::Env, CapParam::Rng] {
                            if pname == cap.param_name() {
                                already_has.insert(cap);
                            }
                        }
                    }
                }
            }

            let mut needed = BTreeSet::new();
            let mut free_callees = BTreeSet::new();
            let mut cross_crate_callees = BTreeSet::new();
            let mut method_callees = BTreeSet::new();
            if let Some(body) = func.body() {
                scan_body_caps_and_callees(
                    body.syntax(),
                    alloc_strategy,
                    crate_ident,
                    &known_crates,
                    &imports,
                    &mut needed,
                    &mut free_callees,
                    &mut cross_crate_callees,
                    &mut method_callees,
                );
            }

            fns.push(FnCapInfo {
                file_idx,
                summary_idx: *summary_idx,
                crate_ident: crate_ident.clone(),
                fn_start: func.syntax().text_range().start().into(),
                name,
                is_pub,
                is_method,
                already_has,
                needed,
                free_callees,
                cross_crate_callees,
                method_callees,
            });
        }
        file_imports.push(imports);
    }

    // Build unambiguous lookup tables using borrowed `&str` keys (single pass: `Some(idx)` = unique,
    // `None` = ambiguous).
    let mut free_fn_idx: BTreeMap<(usize, &str), Option<usize>> = BTreeMap::new();
    let mut method_idx: BTreeMap<(usize, &str), Option<usize>> = BTreeMap::new();
    let mut ws_pub_fn_idx: BTreeMap<(&str, &str), Option<usize>> = BTreeMap::new();
    for (idx, f) in fns.iter().enumerate() {
        if !f.is_method {
            free_fn_idx.entry((f.summary_idx, f.name.as_str())).and_modify(|s| *s = None).or_insert(Some(idx));
            if f.is_pub {
                ws_pub_fn_idx.entry((f.crate_ident.as_str(), f.name.as_str())).and_modify(|s| *s = None).or_insert(Some(idx));
            }
        } else if !is_common_std_method(&f.name) {
            method_idx.entry((f.summary_idx, f.name.as_str())).and_modify(|s| *s = None).or_insert(Some(idx));
        }
    }

    // 2. Bounded workspace-wide call-graph fixpoint: propagate callee capabilities to callers.
    let mut needed_per_fn: Vec<BTreeSet<CapParam>> =
        fns.iter().map(|f| f.needed.iter().copied().collect()).collect();
    for _ in 0..MAX_CAP_PROP_ITERS {
        let mut changed = false;
        let snapshot: BTreeMap<usize, BTreeSet<CapParam>> =
            needed_per_fn.iter().enumerate().map(|(i, s)| (i, s.iter().copied().collect())).collect();
        for (needed, f) in needed_per_fn.iter_mut().zip(&fns) {
            let before = needed.len();
            for callee in &f.free_callees {
                if let Some(&Some(cidx)) = free_fn_idx.get(&(f.summary_idx, callee.as_str()))
                    && let Some(caps) = snapshot.get(&cidx)
                {
                    needed.extend(caps.iter().copied());
                }
            }
            for (target_crate, callee) in &f.cross_crate_callees {
                if let Some(&Some(cidx)) = ws_pub_fn_idx.get(&(target_crate.as_str(), callee.as_str()))
                    && let Some(caps) = snapshot.get(&cidx)
                {
                    needed.extend(caps.iter().copied());
                }
            }
            for callee in &f.method_callees {
                if let Some(&Some(cidx)) = method_idx.get(&(f.summary_idx, callee.as_str()))
                    && let Some(caps) = snapshot.get(&cidx)
                {
                    needed.extend(caps.iter().copied());
                }
            }
            if needed.len() > before {
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }

    // Map `(file_idx, fn_start)` -> newly added `CapParam`s (excluding already_has).
    let mut added_by_fn: BTreeMap<(usize, usize), BTreeSet<CapParam>> = BTreeMap::new();
    let mut added_by_free_name: BTreeMap<(usize, &str), BTreeSet<CapParam>> = BTreeMap::new();
    let mut added_by_method_name: BTreeMap<(usize, &str), BTreeSet<CapParam>> = BTreeMap::new();
    let mut added_by_ws_pub_fn: BTreeMap<(&str, &str), BTreeSet<CapParam>> = BTreeMap::new();
    for (needed, f) in needed_per_fn.iter().zip(&fns) {
        let added: BTreeSet<CapParam> =
            needed.difference(&f.already_has).copied().collect();
        if added.is_empty() {
            continue;
        }
        added_by_fn.insert((f.file_idx, f.fn_start), added.iter().copied().collect());
        if !f.is_method {
            if matches!(free_fn_idx.get(&(f.summary_idx, f.name.as_str())), Some(Some(_))) {
                added_by_free_name.insert((f.summary_idx, f.name.as_str()), added.iter().copied().collect());
            }
            if f.is_pub && matches!(ws_pub_fn_idx.get(&(f.crate_ident.as_str(), f.name.as_str())), Some(Some(_))) {
                added_by_ws_pub_fn.insert((f.crate_ident.as_str(), f.name.as_str()), added);
            }
        } else if matches!(method_idx.get(&(f.summary_idx, f.name.as_str())), Some(Some(_))) {
            added_by_method_name.insert((f.summary_idx, f.name.as_str()), added);
        }
    }

    if added_by_fn.is_empty() {
        return 0;
    }

    // 3. Emit signature edits + intra-crate & cross-crate call-site argument forwarding.
    let mut cross_crate_rewrites = 0usize;

    for (file_idx, (wf, imports)) in files.iter_mut().zip(&file_imports).enumerate() {
        let summary_idx = &wf.summary_idx;
        let crate_ident = &wf.crate_ident;
        let path = &wf.path;
        let src = &mut wf.src;
        if path == "build.rs" || path.ends_with("/build.rs") {
            continue;
        }
        let parse = SourceFile::parse(src, Edition::CURRENT);
        let root = parse.tree();
        let mut edits: Vec<SpanEdit> = Vec::new();

        for node in root.syntax().descendants() {
            match node.kind() {
                SyntaxKind::FN => {
                    let Some(func) = ast::Fn::cast(node) else { continue };
                    let fn_start: usize = func.syntax().text_range().start().into();
                    let Some(added) = added_by_fn.get(&(file_idx, fn_start)) else { continue };
                    let Some(plist) = func.param_list() else { continue };
                    let Some(rparen) = plist.r_paren_token() else { continue };

                    let mut alloc_lt: Option<&'static str> = None;
                    if added.contains(&CapParam::Alloc) {
                        if let Some(ret_ty) = func.ret_type().and_then(|r| r.ty()) {
                            let raw_ret = ret_ty.syntax().text().to_string();
                            let chosen_lt = pick_fresh_alloc_lifetime(&func);
                            if let Some(rewritten_ret) = rewrite_return_type_with_alloc(&raw_ret, chosen_lt) {
                                alloc_lt = Some(chosen_lt);
                                if !fn_declares_lifetime(&func, chosen_lt) {
                                    if let Some(gpl) = func.generic_param_list() {
                                        if let Some(langle) = gpl.l_angle_token() {
                                            let pos: usize = langle.text_range().end().into();
                                            let has_any = gpl.generic_params().next().is_some();
                                            let rep = if has_any {
                                                format!("{chosen_lt}, ")
                                            } else {
                                                chosen_lt.to_owned()
                                            };
                                            edits.push(SpanEdit { start: pos, end: pos, replacement: rep });
                                        }
                                    } else if let Some(name_tok) = func.name() {
                                        let pos: usize = name_tok.syntax().text_range().end().into();
                                        edits.push(SpanEdit {
                                            start: pos,
                                            end: pos,
                                            replacement: format!("<{chosen_lt}>"),
                                        });
                                    }
                                }
                                let rrange = ret_ty.syntax().text_range();
                                edits.push(SpanEdit {
                                    start: rrange.start().into(),
                                    end: rrange.end().into(),
                                    replacement: rewritten_ret,
                                });
                            }
                        }
                    }

                    let ins_pos: usize = rparen.text_range().start().into();
                    let has_existing = plist.self_param().is_some() || plist.params().next().is_some();
                    let decls: Vec<String> = added.iter().map(|c| c.param_decl(alloc_lt)).collect();
                    let before = src[..ins_pos.min(src.len())].trim_end();
                    let has_trailing_comma = before.ends_with(',');
                    let text = if !has_existing {
                        decls.join(", ")
                    } else if has_trailing_comma {
                        format!(" {}, ", decls.join(", "))
                    } else {
                        format!(", {}", decls.join(", "))
                    };
                    edits.push(SpanEdit { start: ins_pos, end: ins_pos, replacement: text });
                }
                SyntaxKind::CALL_EXPR => {
                    let Some(call) = ast::CallExpr::cast(node) else { continue };
                    let Some(expr) = call.expr() else { continue };
                    let raw_callee = expr.syntax().text().to_string();
                    let compact: String = raw_callee.chars().filter(|c| !c.is_whitespace()).collect();
                    let cleaned = compact.strip_prefix("::").unwrap_or(&compact);
                    let leaf = cleaned.rsplit("::").next().unwrap_or(cleaned);
                    if let Some((prefix, _)) = cleaned.rsplit_once("::") {
                        let parent_seg = prefix.rsplit("::").next().unwrap_or(prefix);
                        if parent_seg.starts_with(|c: char| c.is_ascii_uppercase()) {
                            continue;
                        }
                    }

                    // Resolve either cross-crate (`other_crate::...::fn` or imported `use other_crate::fn`)
                    // or intra-crate (`fn` / `mod::fn`).
                    let first_seg = cleaned.split("::").next().unwrap_or("");
                    let (callee_added, is_cross_crate) = if cleaned.contains("::")
                        && first_seg != crate_ident.as_str()
                        && known_crates.contains(first_seg)
                    {
                        match added_by_ws_pub_fn.get(&(first_seg, leaf)) {
                            Some(a) => (Some(a), true),
                            None => (None, false),
                        }
                    } else if !cleaned.contains("::") {
                        if let Some((target_crate, orig_fn)) = imports.get(leaf) {
                            match added_by_ws_pub_fn.get(&(target_crate.as_str(), orig_fn.as_str())) {
                                Some(a) => (Some(a), true),
                                None => (added_by_free_name.get(&(*summary_idx, leaf)), false),
                            }
                        } else {
                            (added_by_free_name.get(&(*summary_idx, leaf)), false)
                        }
                    } else {
                        (added_by_free_name.get(&(*summary_idx, leaf)), false)
                    };

                    let Some(callee_added) = callee_added else { continue };
                    let Some(arg_list) = call.arg_list() else { continue };
                    let Some(rparen) = arg_list.r_paren_token() else { continue };
                    if let Some(args_str) =
                        format_forwarded_args(call.syntax(), callee_added, file_idx, &added_by_fn)
                    {
                        let ins_pos: usize = rparen.text_range().start().into();
                        let has_args = arg_list.args().next().is_some();
                        let before = src[..ins_pos.min(src.len())].trim_end();
                        let has_trailing_comma = before.ends_with(',');
                        let text = if !has_args {
                            args_str
                        } else if has_trailing_comma {
                            format!(" {args_str}, ")
                        } else {
                            format!(", {args_str}")
                        };
                        edits.push(SpanEdit { start: ins_pos, end: ins_pos, replacement: text });
                        if is_cross_crate {
                            cross_crate_rewrites += 1;
                        }
                    }
                }
                SyntaxKind::METHOD_CALL_EXPR => {
                    let Some(mc) = ast::MethodCallExpr::cast(node) else { continue };
                    let Some(mname_ref) = mc.name_ref() else { continue };
                    let mname = mname_ref.text().to_string();
                    let Some(callee_added) = added_by_method_name.get(&(*summary_idx, mname.as_str())) else { continue };
                    let Some(arg_list) = mc.arg_list() else { continue };
                    let Some(rparen) = arg_list.r_paren_token() else { continue };
                    if let Some(args_str) =
                        format_forwarded_args(mc.syntax(), callee_added, file_idx, &added_by_fn)
                    {
                        let ins_pos: usize = rparen.text_range().start().into();
                        let has_args = arg_list.args().next().is_some();
                        let before = src[..ins_pos.min(src.len())].trim_end();
                        let has_trailing_comma = before.ends_with(',');
                        let text = if !has_args {
                            args_str
                        } else if has_trailing_comma {
                            format!(" {args_str}, ")
                        } else {
                            format!(", {args_str}")
                        };
                        edits.push(SpanEdit { start: ins_pos, end: ins_pos, replacement: text });
                    }
                }
                _ => {}
            }
        }

        if !edits.is_empty() {
            on_crate_edit(*summary_idx, edits.len());
            *src = apply_span_edits(src, edits);
        }
    }

    cross_crate_rewrites
}

/// SOTA 2: Parameterize plain owned-heap structs (`struct Foo { items: Vec<String>, ... }`) with
/// an explicit allocator lifetime `struct Foo<'a>` and `bstd::mem::Alloc<'a>` field types, and
/// update matching inherent `impl<'a> Foo<'a>` blocks in the same file.
///
/// Skips `#[repr(C)]`, `#[repr(transparent)]`, `Serialize`/`Deserialize`/`Copy` derives, structs
/// already carrying `'a` or an `Allocator` parameter, and `#[cfg(test)]` items.
/// Writes the rewritten source into `out` and returns [`StructAllocStats`].
pub fn rewrite_struct_alloc_fields_into(
    src: &str,
    out: &mut impl core::fmt::Write,
) -> StructAllocStats {
    let parse = SourceFile::parse(src, Edition::CURRENT);
    let root = parse.tree();
    let mut edits: Vec<SpanEdit> = Vec::new();
    let mut parameterized_structs: BTreeSet<String> = BTreeSet::new();

    // 1. Inspect structs and rewrite owned-heap fields + add `'a` lifetime param.
    for node in root.syntax().descendants() {
        let Some(st) = ast::Struct::cast(node) else { continue };
        let st_syn = st.syntax();
        if st_syn.ancestors().any(|a| is_cfg_test_item(&a)) || is_cfg_test_item(st_syn) {
            continue;
        }
        // Skip structs nested inside function bodies.
        if st_syn.ancestors().skip(1).any(|a| a.kind() == SyntaxKind::FN) {
            continue;
        }
        let Some(name_tok) = st.name() else { continue };
        let sname = name_tok.text().to_string();

        let attr_txt: String = st
            .syntax()
            .children()
            .filter(|c| c.kind() == SyntaxKind::ATTR)
            .map(|c| c.text().to_string())
            .collect();
        if attr_txt.contains("repr(C")
            || attr_txt.contains("repr(transparent")
            || attr_txt.contains("Serialize")
            || attr_txt.contains("Deserialize")
            || attr_txt.contains("Copy")
        {
            continue;
        }

        if let Some(gpl) = st.generic_param_list() {
            let gtxt = gpl.syntax().text().to_string();
            if gtxt.contains("'a") || gtxt.contains("Allocator") || gtxt.contains("Alloc") {
                continue;
            }
        }

        let Some(ast::FieldList::RecordFieldList(rfl)) = st.field_list() else {
            continue;
        };

        let mut field_edits: Vec<SpanEdit> = Vec::new();
        for field in rfl.fields() {
            if let Some(ty) = field.ty() {
                let raw = ty.syntax().text().to_string();
                if let Some(rewritten) = rewrite_return_type_with_alloc(&raw, "'a") {
                    let r = ty.syntax().text_range();
                    field_edits.push(SpanEdit {
                        start: r.start().into(),
                        end: r.end().into(),
                        replacement: rewritten,
                    });
                }
            }
        }
        if field_edits.is_empty() {
            continue;
        }

        // Insert `'a` into struct's generic param list.
        if let Some(gpl) = st.generic_param_list() {
            if let Some(langle) = gpl.l_angle_token() {
                let pos: usize = langle.text_range().end().into();
                let has_any = gpl.generic_params().next().is_some();
                let rep = if has_any { "'a, ".to_owned() } else { "'a".to_owned() };
                edits.push(SpanEdit { start: pos, end: pos, replacement: rep });
            }
        } else {
            let pos: usize = name_tok.syntax().text_range().end().into();
            edits.push(SpanEdit { start: pos, end: pos, replacement: "<'a>".to_owned() });
        }
        edits.extend(field_edits);
        parameterized_structs.insert(sname);
    }

    if parameterized_structs.is_empty() {
        let _ = out.write_str(src);
        return StructAllocStats::default();
    }

    // 2. Update matching inherent `impl` blocks in the same file (`impl Foo` -> `impl<'a> Foo<'a>`).
    for node in root.syntax().descendants() {
        let Some(imp) = ast::Impl::cast(node) else { continue };
        if imp.trait_().is_some() || imp.syntax().ancestors().any(|a| is_cfg_test_item(&a)) {
            continue;
        }
        let Some(self_ty) = imp.self_ty() else { continue };
        let stxt = self_ty.syntax().text().to_string();
        let st_trim = stxt.trim();
        let head = st_trim.split('<').next().unwrap_or(st_trim).trim();
        if !parameterized_structs.contains(head) {
            continue;
        }
        if st_trim.contains("'a") {
            continue;
        }

        // Insert `'a` on `impl` generics.
        if let Some(gpl) = imp.generic_param_list() {
            if !gpl.syntax().text().to_string().contains("'a") {
                if let Some(langle) = gpl.l_angle_token() {
                    let pos: usize = langle.text_range().end().into();
                    let has_any = gpl.generic_params().next().is_some();
                    let rep = if has_any { "'a, ".to_owned() } else { "'a".to_owned() };
                    edits.push(SpanEdit { start: pos, end: pos, replacement: rep });
                }
            }
        } else if let Some(impl_tok) = imp.impl_token() {
            let pos: usize = impl_tok.text_range().end().into();
            edits.push(SpanEdit { start: pos, end: pos, replacement: "<'a>".to_owned() });
        }

        // Insert `'a` on `SelfTy` (`Foo` -> `Foo<'a>`, `Foo<T>` -> `Foo<'a, T>`).
        let srange = self_ty.syntax().text_range();
        let new_self_ty = if let Some(open) = st_trim.find('<') {
            format!("{}<'a, {}", &st_trim[..open], &st_trim[open + 1..])
        } else {
            format!("{st_trim}<'a>")
        };
        edits.push(SpanEdit {
            start: srange.start().into(),
            end: srange.end().into(),
            replacement: new_self_ty,
        });
    }

    let edits_applied = edits.len();
    let structs_changed = parameterized_structs.len();
    let rewritten = apply_span_edits(src, edits);
    let _ = out.write_str(&rewritten);
    StructAllocStats { structs_changed, edits_applied }
}

fn format_forwarded_args(
    call_node: &syntax::SyntaxNode,
    callee_added: &BTreeSet<CapParam>,
    file_idx: usize,
    added_by_fn: &BTreeMap<(usize, usize), BTreeSet<CapParam>>,
) -> Option<String> {
    let enclosing = call_node.ancestors().skip(1).find_map(ast::Fn::cast)?;
    let fn_start: usize = enclosing.syntax().text_range().start().into();
    let caller_is_threadable = is_threadable_fn(&enclosing);
    let caller_has_added = added_by_fn.get(&(file_idx, fn_start));

    let mut existing_params: BTreeSet<String> = BTreeSet::new();
    if let Some(plist) = enclosing.param_list() {
        for p in plist.params() {
            if let Some(pat) = p.pat() {
                let pname = pat.syntax().text().to_string();
                existing_params.insert(pname.trim().trim_start_matches("mut ").trim().to_owned());
            }
        }
    }

    let mut args = Vec::with_capacity(callee_added.len());
    for &cap in callee_added {
        let in_scope = existing_params.contains(cap.param_name())
            || (caller_is_threadable && caller_has_added.is_some_and(|s| s.contains(&cap)));
        if in_scope {
            args.push(cap.param_name());
        } else if !caller_is_threadable && cap == CapParam::Alloc {
            // Root boundary (`main` / `#[test]`): supply explicit global allocator handle.
            args.push("bstd::mem::Alloc::global()");
        } else {
            // Non-threadable caller missing a non-global capability: do not emit broken syntax.
            return None;
        }
    }
    Some(args.join(", "))
}

#[expect(
    clippy::too_many_arguments,
    reason = "single-pass body scanner recording direct capability uses and intra/cross-crate callees"
)]
fn scan_body_caps_and_callees(
    body: &syntax::SyntaxNode,
    alloc_strategy: AllocStrategy,
    own_crate: &str,
    known_crates: &BTreeSet<String>,
    imports: &BTreeMap<String, (String, String)>,
    needed: &mut BTreeSet<CapParam>,
    free_callees: &mut BTreeSet<String>,
    cross_crate_callees: &mut BTreeSet<(String, String)>,
    method_callees: &mut BTreeSet<String>,
) {
    for node in body.descendants() {
        // Don't bleed nested `fn` bodies into the outer function.
        if node.kind() == SyntaxKind::FN {
            continue;
        }
        if node.ancestors().skip(1).take_while(|a| *a != *body).any(|a| a.kind() == SyntaxKind::FN) {
            continue;
        }
        match node.kind() {
            SyntaxKind::CALL_EXPR => {
                let Some(call) = ast::CallExpr::cast(node) else { continue };
                let Some(expr) = call.expr() else { continue };
                let raw = expr.syntax().text().to_string();
                let compact: String = raw.chars().filter(|c| !c.is_whitespace()).collect();
                let cleaned = compact.strip_prefix("::").unwrap_or(&compact);
                if cleaned.starts_with("bstd::vec::Vec::")
                    || cleaned.starts_with("bstd::mem::String::")
                    || cleaned.starts_with("bstd::boxed::Box::new_in")
                    || cleaned.starts_with("bstd::boxed::Arc::new_in")
                    || cleaned.starts_with("bstd::boxed::Rc::new_in")
                    || cleaned.starts_with("bstd::map::BTreeMap::new_in")
                    || cleaned.starts_with("bstd::set::BTreeSet::new_in")
                    || cleaned.starts_with("bstd::map::Map::new")
                    || cleaned.starts_with("bstd::set::Set::new")
                {
                    let body_txt = call.syntax().text().to_string();
                    if body_txt.contains("cx.frame().alloc()") {
                        needed.insert(CapParam::Cx);
                    } else if body_txt.contains("alloc") && alloc_strategy != AllocStrategy::KeepAllocForNow {
                        needed.insert(CapParam::Alloc);
                    }
                    continue;
                }
                let leaf = cleaned.rsplit("::").next().unwrap_or(cleaned);
                let is_type_assoc = cleaned
                    .rsplit_once("::")
                    .is_some_and(|(pfx, _)| pfx.rsplit("::").next().unwrap_or(pfx).starts_with(|c: char| c.is_ascii_uppercase()));
                if !is_type_assoc && !leaf.is_empty() && leaf.starts_with(|c: char| c.is_ascii_lowercase()) {
                    let first_seg = cleaned.split("::").next().unwrap_or("");
                    if cleaned.contains("::") && first_seg != own_crate && known_crates.contains(first_seg) {
                        cross_crate_callees.insert((first_seg.to_owned(), leaf.to_owned()));
                    } else if !cleaned.contains("::") {
                        if let Some((target_crate, orig_fn)) = imports.get(leaf) {
                            cross_crate_callees.insert((target_crate.clone(), orig_fn.clone()));
                        } else {
                            free_callees.insert(leaf.to_owned());
                        }
                    } else {
                        free_callees.insert(leaf.to_owned());
                    }
                }
            }
            SyntaxKind::METHOD_CALL_EXPR => {
                let Some(mc) = ast::MethodCallExpr::cast(node) else { continue };
                let Some(mname_ref) = mc.name_ref() else { continue };
                let mname = mname_ref.text().to_string();
                if mname == "collect_in" {
                    let mtxt = mc.syntax().text().to_string();
                    if mtxt.contains("cx.frame().alloc()") {
                        needed.insert(CapParam::Cx);
                    } else if alloc_strategy != AllocStrategy::KeepAllocForNow {
                        needed.insert(CapParam::Alloc);
                    }
                    continue;
                }
                if let Some(recv) = mc.receiver() {
                    let rtxt = recv.syntax().text().to_string();
                    let rtrim = rtxt.trim();
                    if rtrim == "clock" && matches!(mname.as_str(), "now" | "wall" | "after" | "sleep") {
                        needed.insert(CapParam::Clock);
                        continue;
                    }
                    if rtrim == "env" && matches!(mname.as_str(), "get" | "may_read" | "each") {
                        needed.insert(CapParam::Env);
                        continue;
                    }
                    if rtrim == "rng" {
                        needed.insert(CapParam::Rng);
                        continue;
                    }
                }
                method_callees.insert(mname);
            }
            SyntaxKind::MACRO_CALL => {
                let Some(mc) = ast::MacroCall::cast(node) else { continue };
                let mpath = mc.path().map(|p| p.syntax().text().to_string()).unwrap_or_default();
                let compact: String = mpath.chars().filter(|c| !c.is_whitespace()).collect();
                if compact == "bstd::mem::format_in" || compact == "bstd::vec::vec_in" {
                    let txt = mc.syntax().text().to_string();
                    if txt.contains("cx.frame().alloc()") {
                        needed.insert(CapParam::Cx);
                    } else {
                        needed.insert(CapParam::Alloc);
                    }
                }
            }
            SyntaxKind::PATH_EXPR => {
                if node.text().to_string().trim() == "rng" {
                    needed.insert(CapParam::Rng);
                }
            }
            _ => {}
        }
    }
}

fn is_common_std_method(name: &str) -> bool {
    matches!(
        name,
        "new"
            | "default"
            | "len"
            | "is_empty"
            | "push"
            | "pop"
            | "get"
            | "get_mut"
            | "insert"
            | "remove"
            | "clear"
            | "extend"
            | "contains"
            | "clone"
            | "fmt"
            | "from"
            | "into"
            | "iter"
            | "iter_mut"
            | "into_iter"
            | "next"
            | "map"
            | "filter"
            | "collect"
            | "unwrap"
            | "expect"
            | "ok"
            | "err"
            | "as_str"
            | "as_bytes"
            | "as_slice"
            | "to_string"
            | "to_owned"
            | "to_vec"
            | "read"
            | "write"
            | "flush"
            | "eq"
            | "cmp"
            | "hash"
            | "drop"
    )
}

fn fn_declares_lifetime(func: &ast::Fn, lt: &str) -> bool {
    func.generic_param_list()
        .is_some_and(|g| g.syntax().text().to_string().contains(lt))
}

fn pick_fresh_alloc_lifetime(func: &ast::Fn) -> &'static str {
    let fn_has_a = fn_declares_lifetime(func, "'a");
    let impl_has_a = func.syntax().ancestors().skip(1).find_map(ast::Impl::cast).is_some_and(|imp| {
        imp.generic_param_list()
            .is_some_and(|g| g.syntax().text().to_string().contains("'a"))
    });
    if fn_has_a || impl_has_a {
        "'alloc"
    } else {
        "'a"
    }
}

/// Recursively rewrite an owned-heap return type (`String`, `Vec<T>`, `Box<T>`, `Arc<T>`, `Rc<T>`,
/// `BTreeMap<K, V>`, `BTreeSet<T>`, `HashMap<K, V>`, `HashSet<T>`, nested inside `Result`,
/// `Option`, tuples, etc.) into explicit-allocator `bstd` types parameterized by `bstd::mem::Alloc<lt>`.
/// Returns `None` if no heap container was rewritten.
pub fn rewrite_return_type_with_alloc(ty_text: &str, lt: &str) -> Option<String> {
    let (rewritten, changed) = rewrite_type_rec(ty_text.trim(), lt);
    if changed { Some(rewritten) } else { None }
}

fn rewrite_type_rec(t: &str, lt: &str) -> (String, bool) {
    let t = t.trim();
    if t.is_empty()
        || t.starts_with('&')
        || t.starts_with("impl ")
        || t.starts_with("dyn ")
        || t == "Self"
        || t.contains("bstd::mem::Alloc")
    {
        return (t.to_owned(), false);
    }
    // Tuple `(A, B, ...)`
    if t.starts_with('(') && t.ends_with(')') {
        let inner = &t[1..t.len() - 1];
        let parts = split_top_level_commas(inner);
        let mut any = false;
        let mut out = Vec::with_capacity(parts.len());
        for p in parts {
            let (rp, ch) = rewrite_type_rec(p, lt);
            any |= ch;
            out.push(rp);
        }
        return if any {
            (format!("({})", out.join(", ")), true)
        } else {
            (t.to_owned(), false)
        };
    }

    // Bare `String` / `std::string::String` / `alloc::string::String`
    if matches!(t, "String" | "std::string::String" | "alloc::string::String") {
        return (format!("bstd::mem::String<bstd::mem::Alloc<{lt}>>"), true);
    }

    // Generic path `Head<Arg1, Arg2, ...>`
    if let Some(open) = t.find('<') {
        if t.ends_with('>') {
            let head = t[..open].trim();
            let args_raw = &t[open + 1..t.len() - 1];
            let raw_args = split_top_level_commas(args_raw);
            let mut new_args = Vec::with_capacity(raw_args.len());
            let mut inner_changed = false;
            for a in &raw_args {
                let (ra, ch) = rewrite_type_rec(a, lt);
                inner_changed |= ch;
                new_args.push(ra);
            }
            let alloc_ty = format!("bstd::mem::Alloc<{lt}>");
            match (head, new_args.len()) {
                ("Vec" | "std::vec::Vec" | "alloc::vec::Vec", 1) => {
                    return (format!("bstd::vec::Vec<{}, {alloc_ty}>", new_args[0]), true);
                }
                ("Box" | "std::boxed::Box" | "alloc::boxed::Box", 1) => {
                    return (format!("bstd::boxed::Box<{}, {alloc_ty}>", new_args[0]), true);
                }
                ("Arc" | "std::sync::Arc" | "alloc::sync::Arc", 1) => {
                    return (format!("bstd::boxed::Arc<{}, {alloc_ty}>", new_args[0]), true);
                }
                ("Rc" | "std::rc::Rc" | "alloc::rc::Rc", 1) => {
                    return (format!("bstd::boxed::Rc<{}, {alloc_ty}>", new_args[0]), true);
                }
                ("BTreeMap" | "std::collections::BTreeMap" | "alloc::collections::BTreeMap", 2) => {
                    return (format!("bstd::map::BTreeMap<{}, {}, {alloc_ty}>", new_args[0], new_args[1]), true);
                }
                ("BTreeSet" | "std::collections::BTreeSet" | "alloc::collections::BTreeSet", 1) => {
                    return (format!("bstd::set::BTreeSet<{}, {alloc_ty}>", new_args[0]), true);
                }
                ("HashMap" | "std::collections::HashMap" | "FxHashMap" | "bstd::map::HashMap", 2) => {
                    return (
                        format!(
                            "bstd::map::Map<{}, {}, bstd::hash::FixedState, {alloc_ty}>",
                            new_args[0], new_args[1]
                        ),
                        true,
                    );
                }
                ("HashSet" | "std::collections::HashSet" | "FxHashSet" | "bstd::set::HashSet", 1) => {
                    return (
                        format!("bstd::set::Set<{}, bstd::hash::FixedState, {alloc_ty}>", new_args[0]),
                        true,
                    );
                }
                _ => {
                    if inner_changed {
                        return (format!("{head}<{}>", new_args.join(", ")), true);
                    }
                }
            }
        }
    }

    (t.to_owned(), false)
}

fn split_top_level_commas(s: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut depth_angle = 0usize;
    let mut depth_paren = 0usize;
    let mut depth_bracket = 0usize;
    let mut start = 0usize;
    for (i, b) in s.bytes().enumerate() {
        match b {
            b'<' => depth_angle += 1,
            b'>' => depth_angle = depth_angle.saturating_sub(1),
            b'(' => depth_paren += 1,
            b')' => depth_paren = depth_paren.saturating_sub(1),
            b'[' => depth_bracket += 1,
            b']' => depth_bracket = depth_bracket.saturating_sub(1),
            b',' if depth_angle == 0 && depth_paren == 0 && depth_bracket == 0 => {
                parts.push(s[start..i].trim());
                start = i + 1;
            }
            _ => {}
        }
    }
    let tail = s[start..].trim();
    if !tail.is_empty() {
        parts.push(tail);
    }
    parts
}

fn strip_outer_delim(t: &str) -> &str {
    let trimmed = t.trim();
    if trimmed.len() >= 2
        && matches!(
            (trimmed.as_bytes()[0], trimmed.as_bytes()[trimmed.len() - 1]),
            (b'(', b')') | (b'[', b']') | (b'{', b'}')
        )
    {
        trimmed[1..trimmed.len() - 1].trim()
    } else {
        trimmed
    }
}

fn is_threadable_fn(func: &ast::Fn) -> bool {
    let fname = func.name().map(|n| n.text().to_string()).unwrap_or_default();
    if fname == "main" || fname.is_empty() {
        return false;
    }
    if func.body().is_none() || func.abi().is_some() {
        return false;
    }
    // Skip trait impl methods (trait fixes signature).
    for outer in func.syntax().ancestors().skip(1) {
        if outer.kind() == SyntaxKind::TRAIT {
            return false;
        }
        if let Some(imp) = ast::Impl::cast(outer) {
            if imp.trait_().is_some() {
                return false;
            }
            break;
        }
    }
    // Skip `#[test]` or `#[cfg(test)]` functions.
    let fn_txt = func.syntax().text().to_string();
    if fn_txt.contains("#[test]") || fn_txt.contains("::test]") {
        return false;
    }
    if func.syntax().ancestors().any(|a| is_cfg_test_item(&a)) {
        return false;
    }
    true
}

/// Find the enclosing threadable non-trait, non-`main`, non-`#[test]` function.
fn enclosing_threadable_fn(node: &syntax::SyntaxNode) -> Option<ast::Fn> {
    for anc in node.ancestors().skip(1) {
        if let Some(func) = ast::Fn::cast(anc) {
            return if is_threadable_fn(&func) { Some(func) } else { None };
        }
    }
    None
}

/// Losslessly rewrite a crate's `Cargo.toml` using `blaze-toml::Document` + `Edit`.
#[expect(
    clippy::too_many_arguments,
    reason = "single manifest transformation entry point covering normal deps, build deps, bstd features, and blaze C units"
)]
pub fn rewrite_manifest(
    src: &str,
    remove_deps: &[String],
    remove_build_deps: &[String],
    add_bstd: bool,
    bstd_path: Option<&str>,
    bstd_features: &[String],
    extra_blaze_crates: &[String],
    blaze_c_section: Option<&str>,
) -> String {
    let alloc = Alloc::global();
    let Ok(doc) = Document::parse_in(src, Limits::CARGO, alloc) else {
        return src.to_owned();
    };

    let mut current = src.to_owned();
    let mut edit = Edit::new_in(&doc, alloc);
    let mut any_removed = false;
    if let Some(deps_table) = doc.root().get("dependencies") {
        for item in deps_table.children() {
            let pkg_name = item
                .get("package")
                .and_then(|p| p.as_str())
                .unwrap_or(item.key());
            if remove_deps.iter().any(|r| r == pkg_name || r == item.key()) {
                if edit.remove(item).is_ok() {
                    any_removed = true;
                }
            }
        }
    }
    if !remove_build_deps.is_empty() {
        if let Some(bdeps_table) = doc.root().get("build-dependencies") {
            for item in bdeps_table.children() {
                let pkg_name = item
                    .get("package")
                    .and_then(|p| p.as_str())
                    .unwrap_or(item.key());
                if remove_build_deps.iter().any(|r| r == pkg_name || r == item.key()) {
                    if edit.remove(item).is_ok() {
                        any_removed = true;
                    }
                }
            }
        }
    }
    if any_removed {
        if let Ok(next) = edit.finish() {
            current = next.as_str().to_owned();
        }
    }

    let has_bstd = doc
        .root()
        .get("dependencies")
        .is_some_and(|t| t.get("bstd").is_some() || t.get("blaze-std").is_some());

    let mut additions = String::new();
    if add_bstd && !has_bstd {
        let base_spec = match bstd_path {
            Some(path) => format!("package = \"blaze-std\", path = \"{path}\""),
            None => "package = \"blaze-std\", workspace = true".to_owned(),
        };
        if bstd_features.is_empty() {
            let _ = writeln!(additions, "bstd = {{ {base_spec} }}");
        } else {
            let _ = write!(additions, "bstd = {{ {base_spec}, features = [");
            for (i, f) in bstd_features.iter().enumerate() {
                if i > 0 {
                    additions.push_str(", ");
                }
                let _ = write!(additions, "\"{f}\"");
            }
            additions.push_str("] }\n");
        }
    }
    for extra in extra_blaze_crates {
        let already = doc
            .root()
            .get("dependencies")
            .is_some_and(|t| t.get(extra.as_str()).is_some());
        if !already {
            let _ = writeln!(additions, "{extra} = {{ workspace = true }}");
        }
    }

    if !additions.is_empty() {
        if let Some(pos) = find_dependencies_insert_pos(&current) {
            let mut out = String::with_capacity(current.len() + additions.len() + 2);
            out.push_str(&current[..pos]);
            if !out.ends_with('\n') {
                out.push('\n');
            }
            out.push_str(&additions);
            out.push_str(&current[pos..]);
            current = out;
        } else {
            if !current.ends_with('\n') {
                current.push('\n');
            }
            current.push_str("\n[dependencies]\n");
            current.push_str(&additions);
        }
    }

    if let Some(c_sec) = blaze_c_section {
        if !current.contains("[package.metadata.blaze.c]") {
            if !current.ends_with('\n') {
                current.push('\n');
            }
            current.push('\n');
            current.push_str(c_sec);
        }
    }

    current
}

fn find_dependencies_insert_pos(toml_src: &str) -> Option<usize> {
    let mut offset = 0usize;
    let mut in_deps = false;
    for line in toml_src.split_inclusive('\n') {
        let trimmed = line.trim();
        if trimmed == "[dependencies]" {
            in_deps = true;
            offset += line.len();
            continue;
        }
        if in_deps && trimmed.starts_with('[') && !trimmed.starts_with("[[") {
            return Some(offset);
        }
        offset += line.len();
    }
    if in_deps { Some(toml_src.len()) } else { None }
}
