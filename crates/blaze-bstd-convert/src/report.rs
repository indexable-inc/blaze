//! Human-readable Markdown/terminal reports and strict JSON reports (`blaze-json`).

use alloc::{borrow::ToOwned, format, string::String, vec::Vec};
use core::fmt::Write as _;

use blaze_data::{Int, Map, Value};
use blaze_json::{BytesAs, KeyOrder, Style, WriteOptions, to_string_in};
use blaze_own::{Alloc, String as OwnString, Vec as OwnVec};

use crate::{
    analyze::WorkspaceAnalysis,
    catalog::{DepDisposition, lookup_crate},
    questions::Question,
    rewrite::ConversionResult,
};

/// Render a concise, data-rich Markdown summary of [`WorkspaceAnalysis`].
pub fn render_analysis_markdown(analysis: &WorkspaceAnalysis) -> String {
    let mut out = String::new();
    let total_crates = analysis.crates.len();
    let zero_already = analysis
        .crates
        .iter()
        .filter(|c| c.external_normal_deps().is_empty())
        .count();
    let zero_direct = analysis
        .crates
        .iter()
        .filter(|c| c.can_reach_zero_external_deps_direct())
        .count();
    let zero_with_decisions = analysis
        .crates
        .iter()
        .filter(|c| c.can_reach_zero_external_deps_with_decisions())
        .count();

    let _ = writeln!(out, "# blaze-bstd-convert Analysis Report\n");
    let _ = writeln!(out, "- **Workspace crates**: {total_crates} ({} `.rs` files, {} functions)", analysis.total_rs_files, analysis.total_functions);
    let _ = writeln!(out, "- **Crates already at 0 external normal deps**: {zero_already} / {total_crates}");
    let _ = writeln!(out, "- **Crates reaching 0 external deps automatically (`DirectBstd`)**: {zero_direct} / {total_crates}");
    let _ = writeln!(out, "- **Crates reaching 0 external deps with user decisions**: {zero_with_decisions} / {total_crates}");
    let _ = writeln!(out, "- **Direct ambient IO / effect sites**: {}", analysis.total_effect_sites);
    let _ = writeln!(out, "- **Multi-step IO / async functions to compile into sans-IO state machines**: {}", analysis.total_sm_fn_candidates);
    let _ = writeln!(out, "- **Implicit-state / mixed-IO structs to split**: {}", analysis.total_sm_struct_candidates);
    let _ = writeln!(out, "- **Custom proc-macro crates (recommended for `comptime`/`const` reflection)**: {}", analysis.total_proc_macro_crates);
    let _ = writeln!(
        out,
        "- **C/C++ interop crates (`bindgen`/`cc`/`cmake`/`cxx`/`pkg-config`, recommended for `blaze::c::import!` / `blaze::cpp::import!`)**: {}",
        analysis.total_c_interop_crates
    );
    let _ = writeln!(
        out,
        "- **Bare-primitive domain sites (recommended for `bstd::newtype!` / `bstd::id::id!`)**: {}\n",
        analysis.total_newtype_candidates
    );

    // External dependency table sorted by usage count.
    let mut deps: Vec<(&String, &alloc::collections::BTreeSet<String>)> =
        analysis.external_dep_users.iter().collect();
    deps.sort_by(|a, b| b.1.len().cmp(&a.1.len()).then_with(|| a.0.cmp(b.0)));

    if !deps.is_empty() {
        let _ = writeln!(out, "## External Dependencies ({} unique crates)\n", deps.len());
        let _ = writeln!(out, "| Crate | Users | Disposition | `bstd` / `blaze-*` Target |");
        let _ = writeln!(out, "|---|---:|---|---|");
        for (pkg, users) in deps {
            let (disp, target) = match lookup_crate(pkg) {
                Some(m) => (disposition_label(m.disposition), m.target),
                None => ("unknown (ask)", "user decision"),
            };
            let _ = writeln!(out, "| `{pkg}` | {} | {disp} | {target} |", users.len());
        }
        out.push('\n');
    }

    // Top sans-IO state-machine function candidates.
    let mut sm_fns: Vec<(&str, &crate::analyze::FnAnalysis)> = analysis
        .crates
        .iter()
        .flat_map(|c| {
            c.functions
                .iter()
                .filter(|f| f.is_sm_candidate())
                .map(move |f| (c.name.as_str(), f))
        })
        .collect();
    sm_fns.sort_by(|a, b| b.1.direct_effects.len().cmp(&a.1.direct_effects.len()));

    if !sm_fns.is_empty() {
        let _ = writeln!(out, "## Top Sans-IO State-Machine Candidates (showing up to 20 of {})\n", sm_fns.len());
        let _ = writeln!(out, "| Crate | Function | Location | IO Sites | Async | Effects |");
        let _ = writeln!(out, "|---|---|---|---:|---|---|");
        for (cname, f) in sm_fns.into_iter().take(20) {
            let effs: Vec<&str> = f.transitive_effects.iter().map(|e| e.variant_stem()).collect();
            let _ = writeln!(
                out,
                "| `{cname}` | `{}` | `{}:{}` | {} | {} | {} |",
                f.qualified_name,
                f.file,
                f.line,
                f.direct_effects.len(),
                if f.is_async { "yes" } else { "no" },
                effs.join(", ")
            );
        }
        out.push('\n');
    }

    // Custom proc-macro crates.
    let pm_crates: Vec<&crate::analyze::CrateAnalysis> = analysis
        .crates
        .iter()
        .filter(|c| c.is_proc_macro || !c.proc_macro_entries.is_empty())
        .collect();
    if !pm_crates.is_empty() {
        let _ = writeln!(out, "## Custom Proc-Macro Crates -> `comptime` / `const` Reflection\n");
        for c in pm_crates {
            let _ = writeln!(out, "- **`{}`** (`{}`): {} entry point(s)", c.name, c.manifest_path, c.proc_macro_entries.len());
            for e in &c.proc_macro_entries {
                let label = e.derive_name.as_deref().unwrap_or(&e.fn_name);
                let _ = writeln!(
                    out,
                    "  - `#[{}({})]` at `{}:{}` -> {}",
                    e.kind.as_str(),
                    label,
                    e.file,
                    e.line,
                    e.pattern.comptime_recommendation()
                );
            }
        }
        out.push('\n');
    }

    // C / C++ interop crates (`bindgen`, `cc`, `cmake`, `cxx`, `autocxx`, `pkg-config`).
    let c_crates: Vec<(&crate::analyze::CrateAnalysis, &crate::analyze::CInteropInfo)> = analysis
        .crates
        .iter()
        .filter_map(|c| c.c_interop.as_ref().map(|info| (c, info)))
        .collect();
    if !c_crates.is_empty() {
        let _ = writeln!(
            out,
            "## C / C++ Interop Crates -> `blaze::c::import!` / `blaze::cpp::import!`\n"
        );
        for (c, info) in c_crates {
            let tools: Vec<&str> = info.tools.iter().map(String::as_str).collect();
            let _ = writeln!(
                out,
                "- **`{}`** (`{}`): tools `{}` -> replace with `{}` + `[package.metadata.blaze.c]`",
                c.name,
                c.manifest_path,
                tools.join(", "),
                info.import_macro()
            );
            if !info.headers.is_empty() {
                let _ = writeln!(out, "  - headers: `{}`", info.headers.join("`, `"));
            }
            if !info.source_files.is_empty() {
                let _ = writeln!(out, "  - sources: {} file(s)", info.source_files.len());
            }
            if !info.out_dir_includes.is_empty() {
                let _ = writeln!(out, "  - `OUT_DIR` includes: `{}`", info.out_dir_includes.join("`, `"));
            }
        }
        out.push('\n');
    }

    // Implicit-state structs.
    let structs: Vec<(&str, &crate::analyze::StructSmCandidate)> = analysis
        .crates
        .iter()
        .flat_map(|c| c.struct_sm_candidates.iter().map(move |s| (c.name.as_str(), s)))
        .collect();
    if !structs.is_empty() {
        let _ = writeln!(out, "## Implicit-State / Mixed-IO Structs (showing up to 20 of {})\n", structs.len());
        for (cname, s) in structs.into_iter().take(20) {
            let _ = writeln!(
                out,
                "- `{cname}::{}` (`{}:{}`): {} (IO fields: {}, phase: {}, bools: {}, options: {})",
                s.name,
                s.file,
                s.line,
                s.reason(),
                s.io_fields.len(),
                s.phase_fields.len(),
                s.bool_flag_fields.len(),
                s.option_fields.len()
            );
        }
        out.push('\n');
    }

    out
}

/// Render [`Question`]s as clean Markdown for review by a human or agent.
pub fn render_questions_markdown(questions: &[Question]) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "# Conversion Decisions Needed ({} questions)\n", questions.len());
    for (idx, q) in questions.iter().enumerate() {
        let _ = writeln!(out, "## {}. `{}` [{}]\n", idx + 1, q.id, q.category.as_str());
        let _ = writeln!(out, "{}\n", q.prompt);
        if !q.affected.is_empty() {
            let _ = writeln!(out, "Affected (sample):");
            for item in q.affected.iter().take(8) {
                let _ = writeln!(out, "- {item}");
            }
            if q.affected.len() > 8 {
                let _ = writeln!(out, "- ... and {} more", q.affected.len() - 8);
            }
            out.push('\n');
        }
        for opt in &q.options {
            let rec = if opt.recommended { " *(Recommended)*" } else { "" };
            let _ = writeln!(out, "- **`{}`**{rec}: {} — {}", opt.key, opt.label, opt.impact);
        }
        out.push('\n');
    }
    out
}

/// Render [`ConversionResult`] scoreboard as Markdown.
pub fn render_conversion_markdown(res: &ConversionResult) -> String {
    let mut out = String::new();
    let total = res.crate_summaries.len();
    let _ = writeln!(out, "# blaze-bstd-convert Conversion Scoreboard\n");
    let _ = writeln!(out, "- **Crates converted**: {total}");
    let _ = writeln!(
        out,
        "- **Crates at 0 external normal deps**: {} -> **{}** / {total}",
        res.crates_at_zero_before, res.crates_at_zero_after
    );
    let _ = writeln!(
        out,
        "- **Total external normal dependency edges**: {} -> **{}**",
        res.total_external_edges_before, res.total_external_edges_after
    );
    let _ = writeln!(out, "- **Explicit sans-IO state machines compiled**: {}", res.total_lifted_machines);
    let _ = writeln!(out, "- **Mixed-IO / implicit-state structs split**: {}", res.total_refactored_structs);
    let _ = writeln!(
        out,
        "- **Owned-heap structs parameterized with `bstd::mem::Alloc<'a>`**: {}",
        res.total_parameterized_structs
    );
    let _ = writeln!(
        out,
        "- **Cross-crate capability/allocator call sites forwarded**: {}",
        res.total_cross_crate_call_rewrites
    );
    let _ = writeln!(out, "- **Custom proc-macro `comptime` scaffolds emitted**: {}", res.total_comptime_scaffolds);
    let _ = writeln!(
        out,
        "- **C/C++ `blaze::c::import!` / `blaze::cpp::import!` scaffolds emitted**: {}",
        res.total_c_interop_scaffolds
    );
    let _ = writeln!(
        out,
        "- **Explicit `bstd::newtype!` modules emitted**: {}\n",
        res.total_newtype_scaffolds
    );

    let changed: Vec<_> = res
        .crate_summaries
        .iter()
        .filter(|s| {
            !s.removed_deps.is_empty()
                || !s.lifted_machines.is_empty()
                || !s.refactored_structs.is_empty()
                || s.comptime_scaffold_generated
                || s.c_interop_scaffold_generated
                || s.ast_edits_applied > 0
        })
        .collect();

    if !changed.is_empty() {
        let _ = writeln!(out, "## Per-Crate Results ({} touched)\n", changed.len());
        let _ = writeln!(out, "| Crate | Ext Deps | Removed | Kept | AST Edits | Sans-IO SMs | Struct Splits | `no_std` |");
        let _ = writeln!(out, "|---|---|---|---|---:|---:|---:|---|");
        for s in changed.into_iter().take(60) {
            let _ = writeln!(
                out,
                "| `{}` | {} -> {} | {} | {} | {} | {} | {} | {} |",
                s.crate_name,
                s.external_deps_before,
                s.external_deps_after,
                s.removed_deps.join(", "),
                s.kept_deps.join(", "),
                s.ast_edits_applied,
                s.lifted_machines.len(),
                s.refactored_structs.len(),
                if s.graduated_no_std { "yes" } else { "-" }
            );
        }
    }
    out
}

/// Serialize [`WorkspaceAnalysis`] summary to strict JSON via `blaze-json`.
pub fn render_analysis_json(analysis: &WorkspaceAnalysis) -> Result<String, crate::ConvertError> {
    let alloc = Alloc::global();
    let mut root = Map::new_in(alloc);
    root.insert(
        OwnString::from_str_in("crates", alloc),
        Value::Int(Int::from_u64(analysis.crates.len() as u64)),
    );
    root.insert(
        OwnString::from_str_in("rs_files", alloc),
        Value::Int(Int::from_u64(analysis.total_rs_files as u64)),
    );
    root.insert(
        OwnString::from_str_in("functions", alloc),
        Value::Int(Int::from_u64(analysis.total_functions as u64)),
    );
    root.insert(
        OwnString::from_str_in("effect_sites", alloc),
        Value::Int(Int::from_u64(analysis.total_effect_sites as u64)),
    );
    root.insert(
        OwnString::from_str_in("sm_fn_candidates", alloc),
        Value::Int(Int::from_u64(analysis.total_sm_fn_candidates as u64)),
    );
    root.insert(
        OwnString::from_str_in("sm_struct_candidates", alloc),
        Value::Int(Int::from_u64(analysis.total_sm_struct_candidates as u64)),
    );
    root.insert(
        OwnString::from_str_in("proc_macro_crates", alloc),
        Value::Int(Int::from_u64(analysis.total_proc_macro_crates as u64)),
    );
    root.insert(
        OwnString::from_str_in("c_interop_crates", alloc),
        Value::Int(Int::from_u64(analysis.total_c_interop_crates as u64)),
    );

    let mut deps_arr = OwnVec::new_in(alloc);
    for (pkg, users) in &analysis.external_dep_users {
        let mut entry = Map::new_in(alloc);
        entry.insert(
            OwnString::from_str_in("package", alloc),
            Value::Text(OwnString::from_str_in(pkg, alloc)),
        );
        entry.insert(
            OwnString::from_str_in("users", alloc),
            Value::Int(Int::from_u64(users.len() as u64)),
        );
        let disp = lookup_crate(pkg)
            .map(|m| disposition_label(m.disposition))
            .unwrap_or("unknown");
        entry.insert(
            OwnString::from_str_in("disposition", alloc),
            Value::Text(OwnString::from_str_in(disp, alloc)),
        );
        deps_arr.push(Value::Map(entry));
    }
    root.insert(OwnString::from_str_in("external_deps", alloc), Value::List(deps_arr));

    let opts = WriteOptions {
        keys: KeyOrder::Sorted,
        style: Style::Pretty { indent: 2 },
        bytes: BytesAs::Refuse,
    };
    let out = to_string_in(&Value::Map(root), opts, alloc)
        .map_err(|e| crate::ConvertError::JsonWrite { detail: format!("{e}") })?;
    Ok(out.as_str().to_owned())
}

fn disposition_label(d: DepDisposition) -> &'static str {
    match d {
        DepDisposition::DirectBstd => "direct-bstd",
        DepDisposition::NeedsDecision => "needs-decision",
        DepDisposition::KeepExternal => "keep-external",
        DepDisposition::Unknown => "unknown",
    }
}
