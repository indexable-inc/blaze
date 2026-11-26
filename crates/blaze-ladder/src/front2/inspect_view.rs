//! LLM-first source & slice inspection view over `blaze-file-items` + `front2::inspect`:
//!
//! Renders a file, a 1-based line slice (`file.rs:100-180`), or a named item (`file.rs::Item` /
//! `file.rs::Type::method`) with:
//! 1. **Header & module doc** (`//!` / `#![doc = "..."]`), total line count, and explicit
//!    `front2` semantic inlay-hint status (never silently hidden).
//! 2. **Item outline** for items overlapping the slice (structs, enums, traits, impls, methods,
//!    functions, type aliases, consts, statics, modules, macros) with signatures and 1-line doc
//!    summaries.
//! 3. **Annotated source** splicing both `front2` semantic inlay hints (`/*: Ty*/`, `/* ? Err */`,
//!    coercions, defaulted fields, inferred returns) AND inline LSP-like `@doc` hover comments on
//!    the first reference of each documented local or cross-file workspace symbol.
//! 4. **Referenced Docs appendix** (deduplicated hover cards with exact signature, `path:line`,
//!    referencing lines in the slice, and cleaned doc comment).
//!
//! Strictly sans-IO (all external workspace files are handed in as [`ContextFile`] slices),
//! iterative (zero recursion), zero panics, zero integer `as` casts, and allocator-parametric
//! (`blaze_own` + `blaze_hash`).

use core::alloc::Allocator;
use core::fmt::Write as _;

use blaze_file_items::{Bodies, Extensions, Outline, detect_blaze_extensions, outline_with_extensions};
use blaze_hash::HashMap;
use blaze_own::{Alloc, String as OwnString, Vec as OwnVec};
use blaze_surface::{Attr, AttrKind, Item, ItemDetail, ItemIndex, ItemKind};
use edition::PackageEdition;

use super::inspect::{
    Hint, InspectError, InspectOpts, LineIndex, clamp_char_boundary, decimal_digits,
    inspect_source_detailed, to_surface_edition, write_inline_hint, write_json_str,
};

/// Default character cap for single-line inline `@doc` hover comments.
pub const DEFAULT_MAX_INLINE_DOC_CHARS: usize = 120;

/// Character cap for full doc comments stored in the Referenced Docs appendix.
const MAX_APPENDIX_DOC_CHARS: usize = 600;

/// Maximum inline `@doc` lines attached directly beneath a single source line.
const MAX_INLINE_DOCS_PER_LINE: usize = 4;

/// Maximum referenced symbols collected for a single view.
const MAX_REFERENCED_SYMBOLS: usize = 128;

/// Maximum iterative worklist steps for use-tree and token scans.
const MAX_SCAN_STEPS: usize = 262_144;

/// Inclusive 1-based line range within a source file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LineRange {
    pub start: u32,
    pub end: u32,
}

impl LineRange {
    /// Parses `"42"`, `"10-80"`, `"10..=80"`, `"10..80"`, or `"10-"` (open end -> `u32::MAX`).
    pub fn parse(raw: &str) -> Option<Self> {
        let s = raw.trim();
        if s.is_empty() {
            return None;
        }
        if let Some((a, b)) = s.split_once("..=") {
            let start = parse_line_num(a)?;
            let end = parse_line_num(b)?;
            return (start <= end).then_some(LineRange { start, end });
        }
        if let Some((a, b)) = s.split_once("..") {
            let start = parse_line_num(a)?;
            let end_excl = parse_line_num(b)?;
            let end = end_excl.checked_sub(1)?;
            return (start <= end).then_some(LineRange { start, end });
        }
        if let Some((a, b)) = s.split_once('-') {
            let start = parse_line_num(a)?;
            let end = if b.trim().is_empty() { u32::MAX } else { parse_line_num(b)? };
            return (start <= end).then_some(LineRange { start, end });
        }
        let line = parse_line_num(s)?;
        Some(LineRange { start: line, end: line })
    }

    /// Clamps this range to `1..=total_lines`.
    pub fn clamp(self, total_lines: u32) -> Self {
        let max = total_lines.max(1);
        let start = self.start.clamp(1, max);
        let end = self.end.clamp(start, max);
        LineRange { start, end }
    }

    /// Returns `true` when `[a_start, a_end]` overlaps this range.
    pub const fn overlaps(self, a_start: u32, a_end: u32) -> bool {
        a_start <= self.end && a_end >= self.start
    }
}

fn parse_line_num(s: &str) -> Option<u32> {
    let trimmed = s.trim();
    if trimmed.is_empty() || !trimmed.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let n: u32 = trimmed.parse().ok()?;
    (n >= 1).then_some(n)
}

/// Which slice of a target file [`inspect_file_view`] renders.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InspectTarget<'a> {
    WholeFile,
    Lines(LineRange),
    Item(&'a str),
}

/// One additional workspace/crate file supplied to the sans-IO view builder for cross-file hover
/// documentation resolution.
#[derive(Clone, Copy, Debug)]
pub struct ContextFile<'a> {
    pub path: &'a str,
    pub crate_name: &'a str,
    pub text: &'a str,
}

/// Options governing [`inspect_file_view`] and its LLM/JSON renderers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ViewOpts {
    pub inspect: InspectOpts,
    pub show_outline: bool,
    pub show_docs: bool,
    pub show_types: bool,
    pub max_inline_doc_chars: usize,
}

impl Default for ViewOpts {
    fn default() -> Self {
        Self {
            inspect: InspectOpts::default(),
            show_outline: true,
            show_docs: true,
            show_types: true,
            max_inline_doc_chars: DEFAULT_MAX_INLINE_DOC_CHARS,
        }
    }
}

/// Explicit report of whether `front2` semantic typeck inlay hints ran for the inspected file.
#[derive(Debug, PartialEq, Eq)]
pub enum SemanticStatus<A: Allocator> {
    Complete { checked: u32 },
    Partial { checked: u32, skipped: u32, reason: OwnString<A> },
    Unavailable { reason: OwnString<A> },
    Disabled,
}

/// One outline entry (top-level item or impl/trait/module member) overlapping the viewed slice.
#[derive(Debug)]
pub struct OutlineEntry<A: Allocator> {
    pub kind: &'static str,
    pub name: OwnString<A>,
    pub signature: OwnString<A>,
    pub doc_summary: Option<OwnString<A>>,
    pub line: u32,
    pub end_line: u32,
    pub depth: u8,
}

/// Resolved hover documentation for a local or cross-file symbol referenced in the viewed slice.
#[derive(Debug)]
pub struct SymbolDoc<A: Allocator> {
    pub name: OwnString<A>,
    pub kind: &'static str,
    pub signature: OwnString<A>,
    pub doc: OwnString<A>,
    pub summary: OwnString<A>,
    pub defined_file: OwnString<A>,
    pub defined_line: u32,
    pub ref_lines: OwnVec<u32, A>,
}

/// First-reference inline `@doc` attachment for one source line in the viewed slice.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InlineDoc {
    pub line: u32,
    pub symbol_index: usize,
}

/// Import & module hints extracted from a source file so a thin `bstd::fs` driver knows which
/// sibling modules and workspace crates to read as [`ContextFile`]s.
#[derive(Debug)]
pub struct ImportHints<A: Allocator> {
    /// Non-inline `mod foo;` declarations (`(mod_name, optional_explicit_path)`).
    pub mods: OwnVec<(OwnString<A>, Option<OwnString<A>>), A>,
    /// Expanded `use` path segments (e.g. `["crate", "foo", "Bar"]` or `["blaze_core", "Driver"]`).
    pub use_paths: OwnVec<OwnVec<OwnString<A>, A>, A>,
}

/// Complete LLM-ready view of a file or file slice.
#[derive(Debug)]
pub struct FileView<A: Allocator> {
    pub path: OwnString<A>,
    pub edition: PackageEdition,
    pub total_lines: u32,
    pub view_range: LineRange,
    pub module_doc: Option<OwnString<A>>,
    pub semantic_status: SemanticStatus<A>,
    pub outline: OwnVec<OutlineEntry<A>, A>,
    pub source: OwnString<A>,
    pub hints: OwnVec<Hint<A>, A>,
    pub inline_docs: OwnVec<InlineDoc, A>,
    pub referenced_docs: OwnVec<SymbolDoc<A>, A>,
    pub show_outline: bool,
    pub show_docs: bool,
}

/// Extracts `mod` declarations and expanded `use` paths from `source` so the CLI driver can load
/// referenced workspace/sibling files through `bstd::fs`.
pub fn collect_import_hints<A: Allocator + Copy>(
    source: &str,
    edition: PackageEdition,
    alloc: A,
) -> ImportHints<A> {
    let mut mods = OwnVec::new_in(alloc);
    let mut use_paths = OwnVec::new_in(alloc);

    let Ok(frame) = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer) else {
        return ImportHints { mods, use_paths };
    };
    let erased = blaze_own::Erased::new(&frame);
    let outline = outline_for_edition(source, edition, erased.handle());

    // Iterative stack over outline children (`None` = top-level, plus inline `mod` bodies).
    let mut parents: OwnVec<Option<ItemIndex>, A> = OwnVec::new_in(alloc);
    parents.push(None);
    let mut steps = 0usize;
    while let Some(parent) = parents.pop() {
        if steps >= MAX_SCAN_STEPS {
            break;
        }
        for (idx, item) in outline.children(parent) {
            steps += 1;
            if steps >= MAX_SCAN_STEPS {
                break;
            }
            match item.detail {
                ItemDetail::Mod { inline: true, .. } => {
                    parents.push(Some(idx));
                }
                ItemDetail::Mod { inline: false, path } => {
                    if let Some(name_r) = item.name {
                        let name = item.text(source, name_r).trim();
                        let explicit = path.map(|pr| {
                            let raw = item.text(source, pr).trim();
                            OwnString::from_str_in(unquote_basic_str(raw), alloc)
                        });
                        mods.push((OwnString::from_str_in(name, alloc), explicit));
                    }
                }
                ItemDetail::Use { tree: Some(tree_r) } => {
                    let tree_text = item.text(source, tree_r);
                    let mut expanded = OwnVec::new_in(alloc);
                    expand_use_tree(tree_text, &mut expanded, alloc);
                    for entry in expanded.iter() {
                        if !entry.path.is_empty() {
                            let mut segs = OwnVec::with_capacity_in(entry.path.len(), alloc);
                            for seg in entry.path.iter() {
                                segs.push(OwnString::from_str_in(seg.as_str(), alloc));
                            }
                            use_paths.push(segs);
                        }
                    }
                }
                _ => {}
            }
        }
    }

    ImportHints { mods, use_paths }
}

/// Builds a complete [`FileView`] for `source` (and optional cross-file `context_files`) with
/// zero filesystem I/O.
pub fn inspect_file_view<A: Allocator + Copy>(
    path: &str,
    source: &str,
    edition: PackageEdition,
    target: InspectTarget<'_>,
    context_files: &[ContextFile<'_>],
    opts: ViewOpts,
    store_alloc: Alloc<'static>,
    alloc: A,
) -> Result<FileView<A>, InspectError> {
    let parse_frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).map_err(InspectError::Frame)?;
    let parse_erased = blaze_own::Erased::new(&parse_frame);
    let parse_alloc = parse_erased.handle();

    let line_index = LineIndex::new(source, alloc);
    let total_lines = line_index.line_count(source);
    let outline = outline_for_edition(source, edition, parse_alloc);

    let module_doc = extract_doc_from_slice(source, None, outline.file_attrs(), MAX_APPENDIX_DOC_CHARS, alloc);

    // 1. Collect all outline items & local symbols from the target file.
    let mut all_outline: OwnVec<OutlineEntry<A>, A> = OwnVec::new_in(alloc);
    let mut use_ranges: OwnVec<(usize, usize), A> = OwnVec::new_in(alloc);
    let mut local_defs: OwnVec<DefRecord<A>, A> = OwnVec::new_in(alloc);
    let mut imports: OwnVec<UseBinding<A>, A> = OwnVec::new_in(alloc);

    collect_file_items(
        path,
        source,
        &outline,
        &line_index,
        true,
        opts.max_inline_doc_chars,
        &mut all_outline,
        &mut local_defs,
        &mut imports,
        &mut use_ranges,
        alloc,
    );

    // 2. Resolve target slice (`WholeFile`, `Lines`, or `Item` matching ANY item kind).
    let (view_range, item_filter_for_typeck) = match target {
        InspectTarget::WholeFile => (LineRange { start: 1, end: total_lines }, None),
        InspectTarget::Lines(r) => (r.clamp(total_lines), None),
        InspectTarget::Item(wanted) => {
            let found = find_outline_item(&all_outline, wanted).ok_or(InspectError::ItemNotFound)?;
            let range = LineRange { start: found.line, end: found.end_line }.clamp(total_lines);
            let fn_filter = if matches!(found.kind, "fn" | "method") { Some(wanted) } else { None };
            (range, fn_filter)
        }
    };

    // Keep outline entries overlapping the viewed slice.
    let mut filtered_outline: OwnVec<OutlineEntry<A>, A> = OwnVec::new_in(alloc);
    for entry in all_outline.into_iter() {
        if view_range.overlaps(entry.line, entry.end_line) {
            filtered_outline.push(entry);
        }
    }

    // 3. Collect `front2` semantic inlay hints (explicit status on partial/unavailable, never hidden).
    let mut hints: OwnVec<Hint<A>, A> = OwnVec::new_in(alloc);
    let semantic_status = if !opts.show_types {
        SemanticStatus::Disabled
    } else {
        match inspect_source_detailed(source, edition, item_filter_for_typeck, opts.inspect, store_alloc, alloc) {
            Ok(detailed) => {
                let checked = u32::try_from(detailed.output.items.len()).unwrap_or(u32::MAX);
                for item in detailed.output.items.into_iter() {
                    for h in item.hints.into_iter() {
                        if h.line >= view_range.start && h.line <= view_range.end {
                            hints.push(h);
                        }
                    }
                }
                hints.sort_by_key(|h| (h.byte_offset, h.kind));
                if detailed.skipped == 0 {
                    SemanticStatus::Complete { checked }
                } else if checked > 0 {
                    let reason = detailed
                        .first_failure
                        .map(|m| shorten_reason(m.as_str(), alloc))
                        .unwrap_or_else(|| OwnString::from_str_in("unresolved dependency", alloc));
                    SemanticStatus::Partial { checked, skipped: detailed.skipped, reason }
                } else {
                    let reason = detailed
                        .first_failure
                        .map(|m| shorten_reason(m.as_str(), alloc))
                        .unwrap_or_else(|| OwnString::from_str_in("no checkable functions in single-file scope", alloc));
                    SemanticStatus::Unavailable { reason }
                }
            }
            Err(InspectError::ItemNotFound) => {
                // Non-function item targeted (struct/enum/trait/const): typeck has no body to check.
                SemanticStatus::Complete { checked: 0 }
            }
            Err(err) => {
                let mut raw = OwnString::new_in(alloc);
                let _ = write!(raw, "{err}");
                let reason = shorten_reason(raw.as_str(), alloc);
                SemanticStatus::Unavailable { reason }
            }
        }
    };

    // 4. Build workspace-aware DocIndex and resolve LSP-like inline + appendix hover docs.
    let mut inline_docs: OwnVec<InlineDoc, A> = OwnVec::new_in(alloc);
    let mut referenced_docs: OwnVec<SymbolDoc<A>, A> = OwnVec::new_in(alloc);

    if opts.show_docs {
        let mut all_defs = local_defs;
        for ctx in context_files {
            let ctx_outline = outline_for_edition(ctx.text, edition, parse_alloc);
            let ctx_lines = LineIndex::new(ctx.text, alloc);
            let mut unused_outline = OwnVec::new_in(alloc);
            let mut unused_imports = OwnVec::new_in(alloc);
            let mut unused_uses = OwnVec::new_in(alloc);
            collect_file_items(
                ctx.path,
                ctx.text,
                &ctx_outline,
                &ctx_lines,
                false,
                opts.max_inline_doc_chars,
                &mut unused_outline,
                &mut all_defs,
                &mut unused_imports,
                &mut unused_uses,
                alloc,
            );
        }

        resolve_slice_docs(
            path,
            source,
            &line_index,
            view_range,
            &use_ranges,
            &imports,
            &all_defs,
            &mut inline_docs,
            &mut referenced_docs,
            alloc,
        );
    }

    Ok(FileView {
        path: OwnString::from_str_in(path, alloc),
        edition,
        total_lines,
        view_range,
        module_doc,
        semantic_status,
        outline: filtered_outline,
        source: OwnString::from_str_in(source, alloc),
        hints,
        inline_docs,
        referenced_docs,
        show_outline: opts.show_outline,
        show_docs: opts.show_docs,
    })
}

/// Renders [`FileView`] into a clean, token-efficient Markdown/text view designed for LLMs and
/// humans.
pub fn render_llm_view<A: Allocator + Copy>(view: &FileView<A>, alloc: A) -> OwnString<A> {
    let source = view.source.as_str();
    let lines = LineIndex::new(source, alloc);
    let mut out = OwnString::with_capacity_in(source.len().saturating_mul(2).max(1024), alloc);

    // 1. Compact file & slice header.
    let _ = writeln!(
        out,
        "# {} (lines {}-{} of {}, edition {})",
        view.path.as_str(),
        view.view_range.start,
        view.view_range.end,
        view.total_lines,
        edition_label(view.edition),
    );
    if let Some(mod_doc) = &view.module_doc {
        let first = first_summary_line(mod_doc.as_str(), 180);
        if !first.is_empty() {
            let _ = writeln!(out, "> doc: {first}");
        }
    }
    match &view.semantic_status {
        SemanticStatus::Complete { checked } => {
            if *checked > 0 {
                let _ = writeln!(out, "> semantic-hints: complete ({checked} functions checked)");
            } else {
                let _ = writeln!(out, "> semantic-hints: complete (no function bodies in view)");
            }
        }
        SemanticStatus::Partial { checked, skipped, reason } => {
            let _ = writeln!(
                out,
                "> semantic-hints: partial ({checked} checked, {skipped} skipped: {})",
                reason.as_str()
            );
        }
        SemanticStatus::Unavailable { reason } => {
            let _ = writeln!(out, "> semantic-hints: unavailable ({})", reason.as_str());
        }
        SemanticStatus::Disabled => {}
    }

    // 2. Compact Outline section.
    if view.show_outline && !view.outline.is_empty() {
        out.push_str("\n## Outline\n");
        for entry in view.outline.iter() {
            if entry.depth > 0 {
                out.push_str("  ");
            }
            if entry.line == entry.end_line {
                let _ = write!(out, "- L{} `{}`", entry.line, entry.signature.as_str());
            } else {
                let _ = write!(out, "- L{}-{} `{}`", entry.line, entry.end_line, entry.signature.as_str());
            }
            if let Some(summary) = &entry.doc_summary
                && !summary.is_empty()
            {
                out.push_str(" — ");
                out.push_str(summary.as_str());
            }
            out.push('\n');
        }
    }

    // 3. Annotated source with spliced inlay hints and first-reference inline `@doc` comments.
    out.push_str("\n## Source\n");
    let gutter_width = decimal_digits(view.view_range.end).max(2);
    let mut hint_cursor = 0usize;
    let mut doc_cursor = 0usize;

    for line_num in view.view_range.start..=view.view_range.end {
        let Some((line_start, line_end)) = lines.line_bounds(source, line_num) else { break };
        let _ = write!(out, "{line_num:>gutter_width$} | ");

        while let Some(h) = view.hints.get(hint_cursor) {
            if h.byte_offset < line_start {
                hint_cursor += 1;
            } else {
                break;
            }
        }
        let mut cursor = line_start;
        while let Some(h) = view.hints.get(hint_cursor) {
            if h.byte_offset > line_end {
                break;
            }
            let at = clamp_char_boundary(source, h.byte_offset).max(cursor).min(line_end);
            if let Some(chunk) = source.get(cursor..at) {
                out.push_str(chunk);
            }
            write_inline_hint(h, source, at, &mut out);
            cursor = at;
            hint_cursor += 1;
        }
        if let Some(tail) = source.get(cursor..line_end) {
            out.push_str(tail);
        }
        out.push('\n');

        // Emit inline `@doc` hover lines attached to this line, aligned to the line's indentation.
        while let Some(idoc) = view.inline_docs.get(doc_cursor) {
            if idoc.line < line_num {
                doc_cursor += 1;
            } else {
                break;
            }
        }
        let indent = leading_indent(source.get(line_start..line_end).unwrap_or(""));
        let mut emitted = 0usize;
        while let Some(idoc) = view.inline_docs.get(doc_cursor) {
            if idoc.line > line_num {
                break;
            }
            if emitted < MAX_INLINE_DOCS_PER_LINE
                && let Some(sym) = view.referenced_docs.get(idoc.symbol_index)
            {
                for _ in 0..gutter_width {
                    out.push(' ');
                }
                out.push_str(" | ");
                out.push_str(indent);
                let _ = write!(
                    out,
                    "// @doc {} ({}:{}): ",
                    sym.name.as_str(),
                    sym.defined_file.as_str(),
                    sym.defined_line
                );
                if !sym.summary.is_empty() {
                    out.push_str(sym.summary.as_str());
                } else {
                    out.push_str(sym.signature.as_str());
                }
                out.push('\n');
                emitted += 1;
            }
            doc_cursor += 1;
        }
    }

    // 4. Deduplicated Referenced Docs appendix.
    if view.show_docs && !view.referenced_docs.is_empty() {
        out.push_str("\n## Referenced Docs\n");
        for sym in view.referenced_docs.iter() {
            let _ = write!(
                out,
                "- **{}** (`{}:{}`",
                sym.name.as_str(),
                sym.defined_file.as_str(),
                sym.defined_line
            );
            if !sym.ref_lines.is_empty() {
                out.push_str(", refs: ");
                for (i, &rl) in sym.ref_lines.iter().enumerate() {
                    if i > 0 {
                        out.push_str(", ");
                    }
                    let _ = write!(out, "L{rl}");
                }
            }
            out.push_str(")\n");
            if !sym.signature.is_empty() {
                let _ = writeln!(out, "  `{}`", sym.signature.as_str());
            }
            if !sym.doc.is_empty() {
                for doc_line in sym.doc.as_str().lines() {
                    let trimmed = doc_line.trim_end();
                    if trimmed.is_empty() {
                        out.push('\n');
                    } else {
                        out.push_str("  ");
                        out.push_str(trimmed);
                        out.push('\n');
                    }
                }
            }
        }
    }

    out
}

/// Renders [`FileView`] as deterministic, properly escaped JSON.
pub fn render_llm_json<A: Allocator + Copy>(view: &FileView<A>, alloc: A) -> OwnString<A> {
    let mut out = OwnString::with_capacity_in(1024, alloc);
    out.push_str("{\n  \"file\": ");
    write_json_str(view.path.as_str(), &mut out);
    let _ = write!(
        out,
        ",\n  \"edition\": \"{}\",\n  \"total_lines\": {},\n  \"range\": {{\"start\": {}, \"end\": {}}}",
        edition_label(view.edition),
        view.total_lines,
        view.view_range.start,
        view.view_range.end
    );
    out.push_str(",\n  \"module_doc\": ");
    match &view.module_doc {
        Some(d) => write_json_str(d.as_str(), &mut out),
        None => out.push_str("null"),
    }
    out.push_str(",\n  \"semantic_status\": ");
    match &view.semantic_status {
        SemanticStatus::Complete { checked } => {
            let _ = write!(out, "{{\"state\": \"complete\", \"checked\": {checked}}}");
        }
        SemanticStatus::Partial { checked, skipped, reason } => {
            let _ = write!(out, "{{\"state\": \"partial\", \"checked\": {checked}, \"skipped\": {skipped}, \"reason\": ");
            write_json_str(reason.as_str(), &mut out);
            out.push('}');
        }
        SemanticStatus::Unavailable { reason } => {
            out.push_str("{\"state\": \"unavailable\", \"reason\": ");
            write_json_str(reason.as_str(), &mut out);
            out.push('}');
        }
        SemanticStatus::Disabled => out.push_str("{\"state\": \"disabled\"}"),
    }

    out.push_str(",\n  \"outline\": [");
    for (i, entry) in view.outline.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str("\n    {\"kind\": ");
        write_json_str(entry.kind, &mut out);
        out.push_str(", \"name\": ");
        write_json_str(entry.name.as_str(), &mut out);
        out.push_str(", \"signature\": ");
        write_json_str(entry.signature.as_str(), &mut out);
        out.push_str(", \"doc_summary\": ");
        match &entry.doc_summary {
            Some(s) => write_json_str(s.as_str(), &mut out),
            None => out.push_str("null"),
        }
        let _ = write!(out, ", \"line\": {}, \"end_line\": {}, \"depth\": {}}}", entry.line, entry.end_line, entry.depth);
    }
    if !view.outline.is_empty() {
        out.push_str("\n  ");
    }
    out.push_str("],\n  \"hints\": [");
    for (i, h) in view.hints.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        let _ = write!(
            out,
            "\n    {{\"line\": {}, \"col\": {}, \"byte_offset\": {}, \"kind\": ",
            h.line, h.col, h.byte_offset
        );
        write_json_str(h.kind.as_str(), &mut out);
        out.push_str(", \"label\": ");
        write_json_str(h.label.as_str(), &mut out);
        out.push('}');
    }
    if !view.hints.is_empty() {
        out.push_str("\n  ");
    }
    out.push_str("],\n  \"docs\": [");
    for (i, sym) in view.referenced_docs.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str("\n    {\"name\": ");
        write_json_str(sym.name.as_str(), &mut out);
        out.push_str(", \"kind\": ");
        write_json_str(sym.kind, &mut out);
        out.push_str(", \"signature\": ");
        write_json_str(sym.signature.as_str(), &mut out);
        out.push_str(", \"summary\": ");
        write_json_str(sym.summary.as_str(), &mut out);
        out.push_str(", \"doc\": ");
        write_json_str(sym.doc.as_str(), &mut out);
        out.push_str(", \"defined_file\": ");
        write_json_str(sym.defined_file.as_str(), &mut out);
        let _ = write!(out, ", \"defined_line\": {}, \"ref_lines\": [", sym.defined_line);
        for (j, &rl) in sym.ref_lines.iter().enumerate() {
            if j > 0 {
                out.push_str(", ");
            }
            let _ = write!(out, "{rl}");
        }
        out.push_str("]}");
    }
    if !view.referenced_docs.is_empty() {
        out.push_str("\n  ");
    }
    out.push_str("]\n}\n");
    out
}

// ============================================================================
// Internal outline, doc extraction, use-tree expansion, and symbol resolution
// ============================================================================

struct DefRecord<A: Allocator> {
    short_name: OwnString<A>,
    qualified_name: OwnString<A>,
    kind: &'static str,
    signature: OwnString<A>,
    doc: OwnString<A>,
    summary: OwnString<A>,
    defined_file: OwnString<A>,
    defined_line: u32,
    def_byte_start: usize,
    def_byte_end: usize,
    is_local: bool,
    is_method: bool,
}

struct UseBinding<A: Allocator> {
    path: OwnVec<OwnString<A>, A>,
    alias: OwnString<A>,
}

fn outline_for_edition<'a>(source: &str, edition: PackageEdition, alloc: Alloc<'a>) -> Outline<'a> {
    let surf_ed = to_surface_edition(edition.syntax);
    let mut ext = detect_blaze_extensions(source);
    if edition.blaze.is_some() {
        ext = ext
            .with(Extensions::UNIQ)
            .with(Extensions::RECORDS)
            .with(Extensions::ERROR_UNIONS);
    }
    outline_with_extensions(source, surf_ed, Bodies::Skipped, ext, alloc)
}

#[expect(clippy::too_many_arguments, reason = "internal collector threading output buffers")]
fn collect_file_items<A: Allocator + Copy>(
    file_path: &str,
    source: &str,
    outline: &Outline<'_>,
    line_index: &LineIndex<A>,
    is_local: bool,
    max_inline_doc_chars: usize,
    out_outline: &mut OwnVec<OutlineEntry<A>, A>,
    out_defs: &mut OwnVec<DefRecord<A>, A>,
    out_imports: &mut OwnVec<UseBinding<A>, A>,
    out_use_ranges: &mut OwnVec<(usize, usize), A>,
    alloc: A,
) {
    struct FrameItem<A: Allocator> {
        parent: Option<ItemIndex>,
        container: Option<OwnString<A>>,
        container_kind: Option<ItemKind>,
        depth: u8,
    }

    let mut work: OwnVec<FrameItem<A>, A> = OwnVec::new_in(alloc);
    work.push(FrameItem { parent: None, container: None, container_kind: None, depth: 0 });
    let mut steps = 0usize;

    while let Some(frame) = work.pop() {
        if steps >= MAX_SCAN_STEPS {
            break;
        }
        // Collect children in order so pushing nested containers preserves source order.
        let mut children: OwnVec<(ItemIndex, Item), A> = OwnVec::new_in(alloc);
        for (idx, item) in outline.children(frame.parent) {
            children.push((idx, *item));
        }
        let mut next_frames: OwnVec<FrameItem<A>, A> = OwnVec::new_in(alloc);

        for &(idx, item) in children.iter() {
            steps += 1;
            if steps >= MAX_SCAN_STEPS {
                break;
            }
            let kind = item.kind();
            let byte_start = usize::try_from(item.range.start).unwrap_or(0).min(source.len());
            let byte_end = usize::try_from(item.range.end).unwrap_or(byte_start).min(source.len());
            let (line, _) = line_index.line_col(source, byte_start);
            let (end_line, _) = line_index.line_col(source, byte_end.saturating_sub(1).max(byte_start));
            let decl_byte = item
                .name
                .map(|nr| usize::try_from(item.in_file(nr).start).unwrap_or(byte_start))
                .unwrap_or(byte_start)
                .min(source.len());
            let (decl_line, _) = line_index.line_col(source, decl_byte);

            if kind == ItemKind::Use {
                if is_local {
                    out_use_ranges.push((byte_start, byte_end));
                    if let ItemDetail::Use { tree: Some(tree_r) } = item.detail {
                        expand_use_tree(item.text(source, tree_r), out_imports, alloc);
                    }
                }
                continue;
            }

            if matches!(kind, ItemKind::MacroCall | ItemKind::Error | ItemKind::Unhandled | ItemKind::ExternCrate) {
                continue;
            }

            let sig = extract_clean_signature(source, &item, outline.attrs(&item), alloc);
            let doc_opt = extract_doc_from_slice(source, Some(&item), outline.attrs(&item), MAX_APPENDIX_DOC_CHARS, alloc);
            let summary_opt = doc_opt
                .as_ref()
                .map(|d| OwnString::from_str_in(first_summary_line(d.as_str(), max_inline_doc_chars), alloc));

            let short_name = match kind {
                ItemKind::Impl => OwnString::from_str_in(short_impl_target_type(sig.as_str()), alloc),
                ItemKind::ExternBlock => OwnString::from_str_in("extern", alloc),
                _ => match item.name {
                    Some(nr) => OwnString::from_str_in(item.text(source, nr).trim(), alloc),
                    None => continue,
                },
            };

            let is_member = matches!(
                frame.container_kind,
                Some(ItemKind::Impl | ItemKind::Trait | ItemKind::Enum | ItemKind::Struct | ItemKind::Union)
            );
            let qualified = if let Some(cont) = &frame.container
                && !cont.is_empty()
                && kind != ItemKind::Impl
            {
                let mut q = OwnString::with_capacity_in(cont.len() + 2 + short_name.len(), alloc);
                q.push_str(cont.as_str());
                q.push_str("::");
                q.push_str(short_name.as_str());
                q
            } else {
                OwnString::from_str_in(short_name.as_str(), alloc)
            };

            let kind_str = display_kind(kind, is_member);

            // Outline includes top-level items plus methods/associated items inside impl/trait/mod (not struct fields).
            if is_local && !matches!(kind, ItemKind::Field | ItemKind::Variant) && frame.depth <= 1 {
                out_outline.push(OutlineEntry {
                    kind: kind_str,
                    name: OwnString::from_str_in(qualified.as_str(), alloc),
                    signature: OwnString::from_str_in(sig.as_str(), alloc),
                    doc_summary: summary_opt.as_ref().map(|s| OwnString::from_str_in(s.as_str(), alloc)),
                    line,
                    end_line,
                    depth: frame.depth,
                });
            }

            // Index into DocIndex when it has a doc comment OR is a named cross-file type/fn/trait/method.
            let has_doc = doc_opt.as_ref().is_some_and(|d| !d.is_empty());
            let indexable_kind = !matches!(kind, ItemKind::Impl | ItemKind::ExternBlock | ItemKind::Field);
            if indexable_kind && (has_doc || !is_local) && !short_name.is_empty() {
                let doc = doc_opt.unwrap_or_else(|| OwnString::new_in(alloc));
                let summary = summary_opt.unwrap_or_else(|| OwnString::new_in(alloc));
                out_defs.push(DefRecord {
                    short_name: OwnString::from_str_in(short_name.as_str(), alloc),
                    qualified_name: OwnString::from_str_in(qualified.as_str(), alloc),
                    kind: kind_str,
                    signature: OwnString::from_str_in(sig.as_str(), alloc),
                    doc,
                    summary,
                    defined_file: OwnString::from_str_in(file_path, alloc),
                    defined_line: decl_line,
                    def_byte_start: byte_start,
                    def_byte_end: byte_end,
                    is_local,
                    is_method: kind == ItemKind::Function && matches!(frame.container_kind, Some(ItemKind::Impl | ItemKind::Trait)),
                });
            }

            if matches!(kind, ItemKind::Impl | ItemKind::Trait | ItemKind::Enum | ItemKind::Module)
                && frame.depth < 2
            {
                let next_container = if kind == ItemKind::Module {
                    None
                } else {
                    Some(OwnString::from_str_in(short_name.as_str(), alloc))
                };
                next_frames.push(FrameItem {
                    parent: Some(idx),
                    container: next_container,
                    container_kind: Some(kind),
                    depth: frame.depth.saturating_add(1),
                });
            }
        }

        // Push nested frames in reverse so popping visits them in source order.
        while let Some(nf) = next_frames.pop() {
            work.push(nf);
        }
    }

    if is_local {
        out_outline.sort_by_key(|e| (e.line, e.depth));
    }
}

fn find_outline_item<'a, A: Allocator>(
    outline: &'a [OutlineEntry<A>],
    wanted: &str,
) -> Option<&'a OutlineEntry<A>> {
    // 1. Exact match on qualified or short name (`Type::method` or `foo`).
    for entry in outline {
        if entry.name.as_str() == wanted {
            return Some(entry);
        }
    }
    // 2. Suffix match (`::wanted` for a method name when container wasn't given).
    for entry in outline {
        if let Some((_, tail)) = entry.name.as_str().rsplit_once("::")
            && tail == wanted
        {
            return Some(entry);
        }
    }
    None
}

#[expect(clippy::too_many_arguments, reason = "internal doc resolver over pre-built tables")]
fn resolve_slice_docs<A: Allocator + Copy>(
    target_path: &str,
    source: &str,
    line_index: &LineIndex<A>,
    view_range: LineRange,
    use_ranges: &[(usize, usize)],
    imports: &[UseBinding<A>],
    defs: &[DefRecord<A>],
    out_inline: &mut OwnVec<InlineDoc, A>,
    out_symbols: &mut OwnVec<SymbolDoc<A>, A>,
    alloc: A,
) {
    if defs.is_empty() {
        return;
    }

    // Build fast lookup tables over `defs`:
    // - `by_qualified`: exact `"Type::method"` or `"Enum::Variant"` or top-level name -> def index
    // - `by_short`: non-method top-level item name -> best def index (local beats external)
    // - `method_by_name`: method name -> Some(idx) if unambiguous across indexed defs, None if ambiguous
    let mut by_qualified: HashMap<&str, usize, A> = HashMap::new_in(alloc);
    let mut by_short: HashMap<&str, usize, A> = HashMap::new_in(alloc);
    let mut method_by_name: HashMap<&str, Option<usize>, A> = HashMap::new_in(alloc);

    for (idx, def) in defs.iter().enumerate() {
        let q = def.qualified_name.as_str();
        match by_qualified.get(q).copied() {
            Some(prev) if !should_prefer_def(def, &defs[prev]) => {}
            _ => {
                by_qualified.insert(q, idx);
            }
        }
        if !def.is_method && def.kind != "variant" {
            let s = def.short_name.as_str();
            match by_short.get(s).copied() {
                Some(prev) if !should_prefer_def(def, &defs[prev]) => {}
                _ => {
                    by_short.insert(s, idx);
                }
            }
        } else if def.is_method {
            let m = def.short_name.as_str();
            match method_by_name.get(m).copied() {
                None => {
                    method_by_name.insert(m, Some(idx));
                }
                Some(Some(prev)) => {
                    // Two methods with the same name: prefer local if only one is local, else mark ambiguous.
                    let prev_def = &defs[prev];
                    if def.is_local && !prev_def.is_local {
                        method_by_name.insert(m, Some(idx));
                    } else if !def.is_local && prev_def.is_local {
                        // keep local
                    } else if def.defined_file.as_str() == prev_def.defined_file.as_str()
                        && def.defined_line == prev_def.defined_line
                    {
                        // same definition
                    } else {
                        method_by_name.insert(m, None);
                    }
                }
                Some(None) => {}
            }
        }
    }

    // Map explicit `use ... as Alias` bindings to their resolved `DefRecord` index.
    let mut import_alias: HashMap<&str, usize, A> = HashMap::new_in(alloc);
    for imp in imports {
        let Some(last) = imp.path.last().map(OwnString::as_str) else { continue };
        let candidate = imp
            .path
            .len()
            .checked_sub(2)
            .and_then(|i| imp.path.get(i))
            .and_then(|prev| {
                let mut pair = OwnString::with_capacity_in(prev.len() + 2 + last.len(), alloc);
                pair.push_str(prev.as_str());
                pair.push_str("::");
                pair.push_str(last);
                by_qualified.get(pair.as_str()).copied()
            })
            .or_else(|| by_short.get(last).copied());
        if let Some(def_idx) = candidate {
            import_alias.insert(imp.alias.as_str(), def_idx);
        }
    }

    // Track `def_idx -> output symbol_index` so each symbol is deduplicated.
    let mut emitted_map: HashMap<usize, usize, A> = HashMap::new_in(alloc);

    let Some((slice_start, _)) = line_index.line_bounds(source, view_range.start) else { return };
    let Some((_, slice_end)) = line_index.line_bounds(source, view_range.end) else { return };

    // Lexically scan tokens inside `slice_start..slice_end` skipping comments and literals.
    let bytes = source.as_bytes();
    let mut i = slice_start;
    let mut prev_was_dot = false;
    let mut prev_was_decl_kw = false;
    let mut steps = 0usize;

    while i < slice_end && steps < MAX_SCAN_STEPS {
        steps += 1;
        let b = bytes[i];
        // Line comment `// ...`
        if b == b'/' && bytes.get(i + 1) == Some(&b'/') {
            i += 2;
            while i < slice_end && bytes[i] != b'\n' {
                i += 1;
            }
            prev_was_dot = false;
            prev_was_decl_kw = false;
            continue;
        }
        // Block comment `/* ... */` (nested)
        if b == b'/' && bytes.get(i + 1) == Some(&b'*') {
            i = skip_block_comment(bytes, i, slice_end);
            prev_was_dot = false;
            prev_was_decl_kw = false;
            continue;
        }
        // Raw string `r#"..."#` / `br#"..."#`
        if (b == b'r' || (b == b'b' && bytes.get(i + 1) == Some(&b'r')))
            && let Some(after) = try_skip_raw_string(bytes, i, slice_end)
        {
            i = after;
            prev_was_dot = false;
            prev_was_decl_kw = false;
            continue;
        }
        // String literal `"..."`
        if b == b'"' {
            i = skip_quoted(bytes, i, slice_end, b'"');
            prev_was_dot = false;
            prev_was_decl_kw = false;
            continue;
        }
        // Char literal `'x'` or lifetime `'a`
        if b == b'\'' {
            i = skip_char_or_lifetime(bytes, i, slice_end);
            prev_was_dot = false;
            prev_was_decl_kw = false;
            continue;
        }
        if b == b'.' {
            prev_was_dot = bytes.get(i + 1) != Some(&b'.');
            prev_was_decl_kw = false;
            i += 1;
            continue;
        }
        if is_ident_start(b) {
            let tok_start = i;
            let mut segs: OwnVec<(usize, usize), A> = OwnVec::new_in(alloc);
            loop {
                let s_start = i;
                while i < slice_end && is_ident_cont(bytes[i]) {
                    i += 1;
                }
                segs.push((s_start, i));
                // Check for `::` chaining (ignoring spaces only? standard Rust paths rarely split `::`, check exact `::`).
                if bytes.get(i) == Some(&b':')
                    && bytes.get(i + 1) == Some(&b':')
                    && bytes.get(i + 2).is_some_and(|&c| is_ident_start(c))
                {
                    i += 2;
                    continue;
                }
                break;
            }
            let tok_end = i;
            let followed_by_paren = skip_spaces(bytes, tok_end, slice_end)
                .is_some_and(|p| bytes.get(p) == Some(&b'(') || (bytes.get(p) == Some(&b':') && bytes.get(p + 1) == Some(&b':') && bytes.get(p + 2) == Some(&b'<')));

            // Skip tokens inside `use ...;` statements and declared item names right after `fn`/`struct`/etc.
            let in_use = use_ranges.iter().any(|&(us, ue)| tok_start >= us && tok_start < ue);
            if !in_use && !prev_was_decl_kw {
                let resolved = resolve_token_path(
                    source,
                    &segs,
                    prev_was_dot,
                    followed_by_paren,
                    &import_alias,
                    &by_qualified,
                    &by_short,
                    &method_by_name,
                    alloc,
                );
                if let Some(def_idx) = resolved {
                    let def = &defs[def_idx];
                    // Never show `@doc` for a symbol inside its own definition span in the same file.
                    let is_self_def = def.defined_file.as_str() == target_path
                        && tok_start >= def.def_byte_start
                        && tok_start < def.def_byte_end;
                    if !is_self_def && (out_symbols.len() < MAX_REFERENCED_SYMBOLS || emitted_map.contains_key(&def_idx)) {
                        let (line_num, _) = line_index.line_col(source, tok_start);
                        if let Some(&sym_idx) = emitted_map.get(&def_idx) {
                            if let Some(sym) = out_symbols.get_mut(sym_idx)
                                && sym.ref_lines.last().copied() != Some(line_num)
                            {
                                sym.ref_lines.push(line_num);
                            }
                        } else {
                            let sym_idx = out_symbols.len();
                            emitted_map.insert(def_idx, sym_idx);
                            let mut ref_lines = OwnVec::new_in(alloc);
                            ref_lines.push(line_num);
                            out_symbols.push(SymbolDoc {
                                name: OwnString::from_str_in(def.qualified_name.as_str(), alloc),
                                kind: def.kind,
                                signature: OwnString::from_str_in(def.signature.as_str(), alloc),
                                doc: OwnString::from_str_in(def.doc.as_str(), alloc),
                                summary: OwnString::from_str_in(def.summary.as_str(), alloc),
                                defined_file: OwnString::from_str_in(def.defined_file.as_str(), alloc),
                                defined_line: def.defined_line,
                                ref_lines,
                            });
                            out_inline.push(InlineDoc { line: line_num, symbol_index: sym_idx });
                        }
                    }
                }
            }
            prev_was_dot = false;
            prev_was_decl_kw = segs.len() == 1
                && source.get(tok_start..tok_end).is_some_and(|w| {
                    matches!(w, "fn" | "struct" | "enum" | "union" | "trait" | "type" | "const" | "static" | "mod")
                });
            continue;
        }
        if !b.is_ascii_whitespace() {
            prev_was_dot = false;
            prev_was_decl_kw = false;
        }
        i += 1;
    }
}

#[expect(clippy::too_many_arguments, reason = "internal lookup over pre-built tables")]
fn resolve_token_path<A: Allocator + Copy>(
    source: &str,
    segs: &[(usize, usize)],
    prev_was_dot: bool,
    followed_by_paren: bool,
    import_alias: &HashMap<&str, usize, A>,
    by_qualified: &HashMap<&str, usize, A>,
    by_short: &HashMap<&str, usize, A>,
    method_by_name: &HashMap<&str, Option<usize>, A>,
    alloc: A,
) -> Option<usize> {
    let first_pair = *segs.first()?;
    let first = source.get(first_pair.0..first_pair.1)?;

    if segs.len() == 1 {
        if is_ignored_ident(first) {
            return None;
        }
        if prev_was_dot {
            if followed_by_paren && !is_generic_method_name(first) {
                return method_by_name.get(first).copied().flatten();
            }
            return None;
        }
        if let Some(&idx) = import_alias.get(first) {
            return Some(idx);
        }
        if let Some(&idx) = by_short.get(first) {
            // Avoid spurious matches for common lowercase variable names unless called as `foo(`.
            let starts_upper = first.bytes().next().is_some_and(|b| b.is_ascii_uppercase());
            if starts_upper || followed_by_paren {
                return Some(idx);
            }
        }
        return None;
    }

    // Qualified path (`a::b` or `a::b::C`): check last two segments as `Container::Item` first.
    let last_pair = *segs.last()?;
    let last = source.get(last_pair.0..last_pair.1)?;
    if is_ignored_ident(last) {
        return None;
    }
    if let Some(prev_pair) = segs.len().checked_sub(2).and_then(|k| segs.get(k))
        && let Some(prev) = source.get(prev_pair.0..prev_pair.1)
    {
        let mut pair = OwnString::with_capacity_in(prev.len() + 2 + last.len(), alloc);
        pair.push_str(prev);
        pair.push_str("::");
        pair.push_str(last);
        if let Some(&idx) = by_qualified.get(pair.as_str()) {
            return Some(idx);
        }
    }
    by_short.get(last).copied()
}

fn should_prefer_def<A: Allocator>(cand: &DefRecord<A>, existing: &DefRecord<A>) -> bool {
    if cand.is_local != existing.is_local {
        return cand.is_local;
    }
    !cand.doc.is_empty() && existing.doc.is_empty()
}

fn display_kind(kind: ItemKind, is_member: bool) -> &'static str {
    match kind {
        ItemKind::Function if is_member => "method",
        ItemKind::Function => "fn",
        ItemKind::Struct => "struct",
        ItemKind::Enum => "enum",
        ItemKind::Union => "union",
        ItemKind::Trait => "trait",
        ItemKind::TraitAlias => "trait alias",
        ItemKind::Impl => "impl",
        ItemKind::TypeAlias => "type",
        ItemKind::Const => "const",
        ItemKind::Static => "static",
        ItemKind::Module => "mod",
        ItemKind::MacroRules | ItemKind::Macro2 => "macro",
        ItemKind::Variant => "variant",
        ItemKind::Field => "field",
        ItemKind::ExternBlock => "extern",
        _ => "item",
    }
}

fn extract_clean_signature<A: Allocator>(
    source: &str,
    item: &Item,
    attrs: &[Attr],
    alloc: A,
) -> OwnString<A> {
    let sig_file = item.in_file(item.signature);
    let sig_start = usize::try_from(sig_file.start).unwrap_or(0).min(source.len());
    let sig_end = usize::try_from(sig_file.end).unwrap_or(sig_start).min(source.len());
    let outer_end_rel = attrs
        .iter()
        .filter(|a| !a.inner)
        .map(|a| a.range.end)
        .max()
        .unwrap_or(0);
    let body_start = usize::try_from(item.range.start.saturating_add(outer_end_rel))
        .unwrap_or(sig_start)
        .max(sig_start)
        .min(sig_end);
    let mut raw = source.get(body_start..sig_end).unwrap_or("").trim();
    // Strip any plain leading comments (`// ...` or `/* ... */`) that sit between outer attrs and the keyword.
    loop {
        raw = raw.trim_start();
        if let Some(after) = raw.strip_prefix("//") {
            raw = match after.split_once('\n') {
                Some((_, tail)) => tail,
                None => "",
            };
        } else if raw.starts_with("/*") {
            let end = skip_block_comment(raw.as_bytes(), 0, raw.len());
            raw = raw.get(end..).unwrap_or("");
        } else {
            break;
        }
    }
    let trimmed = raw
        .trim_end_matches('{')
        .trim_end_matches(';')
        .trim_end_matches('=')
        .trim();

    // Collapse multi-line signatures into one clean readable line.
    let mut out = OwnString::with_capacity_in(trimmed.len().min(160), alloc);
    let mut prev_ws = false;
    for ch in trimmed.chars() {
        if ch.is_whitespace() {
            if !prev_ws && !out.is_empty() {
                out.push(' ');
                prev_ws = true;
            }
        } else {
            out.push(ch);
            prev_ws = false;
        }
        if out.len() >= 180 {
            out.push_str("...");
            break;
        }
    }
    out
}

fn extract_doc_from_slice<A: Allocator>(
    source: &str,
    item: Option<&Item>,
    attrs: &[Attr],
    max_chars: usize,
    alloc: A,
) -> Option<OwnString<A>> {
    let mut out = OwnString::new_in(alloc);
    let mut chars = 0usize;
    let mut truncated = false;

    for attr in attrs {
        if attr.kind != AttrKind::Doc {
            continue;
        }
        let raw = match item {
            Some(it) => it.text(source, attr.range),
            None => attr.range.of(source),
        }
        .trim();
        append_cleaned_doc_attr(raw, &mut out, &mut chars, max_chars, &mut truncated);
        if truncated {
            break;
        }
    }

    let trimmed_len = out.as_str().trim_end().len();
    out.truncate(trimmed_len);
    if out.is_empty() {
        None
    } else {
        if truncated {
            out.push_str("...");
        }
        Some(out)
    }
}

fn append_cleaned_doc_attr<A: Allocator>(
    raw: &str,
    out: &mut OwnString<A>,
    chars: &mut usize,
    max_chars: usize,
    truncated: &mut bool,
) {
    if let Some(rest) = raw.strip_prefix("///").or_else(|| raw.strip_prefix("//!")) {
        let line = rest.strip_prefix(' ').unwrap_or(rest).trim_end();
        push_doc_line(line, out, chars, max_chars, truncated);
        return;
    }
    if let Some(rest) = raw.strip_prefix("/**").or_else(|| raw.strip_prefix("/*!")) {
        let inner = rest.strip_suffix("*/").unwrap_or(rest);
        for raw_line in inner.lines() {
            let t = raw_line.trim_start();
            let stripped = t.strip_prefix('*').map_or(t, |s| s.strip_prefix(' ').unwrap_or(s)).trim_end();
            push_doc_line(stripped, out, chars, max_chars, truncated);
            if *truncated {
                return;
            }
        }
        return;
    }
    // `#[doc = "..."]` / `#![doc = "..."]`
    if let Some(eq) = raw.find('=') {
        let rhs = raw[eq + 1..].trim().trim_end_matches(']').trim();
        let unquoted = unquote_basic_str(rhs);
        for line in unquoted.lines() {
            push_doc_line(line.trim_end(), out, chars, max_chars, truncated);
            if *truncated {
                return;
            }
        }
    }
}

fn push_doc_line<A: Allocator>(
    line: &str,
    out: &mut OwnString<A>,
    chars: &mut usize,
    max_chars: usize,
    truncated: &mut bool,
) {
    if line.is_empty() && out.is_empty() {
        return;
    }
    if !out.is_empty() {
        if *chars + 1 >= max_chars {
            *truncated = true;
            return;
        }
        out.push('\n');
        *chars += 1;
    }
    for ch in line.chars() {
        if *chars >= max_chars {
            *truncated = true;
            return;
        }
        out.push(ch);
        *chars += 1;
    }
}

/// Extracts a concise single-line summary from a cleaned doc comment.
fn first_summary_line(doc: &str, max_chars: usize) -> &str {
    // Take text up to first blank line or first heading/fence.
    let first_para_end = doc
        .find("\n\n")
        .or_else(|| doc.find("\n```"))
        .or_else(|| doc.find("\n#"))
        .unwrap_or(doc.len());
    let first_line = doc[..first_para_end].lines().next().unwrap_or("").trim();
    if first_line.chars().count() <= max_chars {
        return first_line;
    }
    let mut byte_end = 0usize;
    for (idx, (b, _)) in first_line.char_indices().enumerate() {
        if idx >= max_chars {
            break;
        }
        byte_end = b;
    }
    first_line.get(..byte_end).unwrap_or(first_line).trim_end()
}

/// Iteratively expands a Rust `use` tree (`foo::{a, b::{c as d, self}}`) into `(path, alias)`.
fn expand_use_tree<A: Allocator + Copy>(
    tree: &str,
    out: &mut OwnVec<UseBinding<A>, A>,
    alloc: A,
) {
    struct Entry<A: Allocator> {
        prefix: OwnVec<OwnString<A>, A>,
        rest_start: usize,
        rest_end: usize,
    }

    let mut stack: OwnVec<Entry<A>, A> = OwnVec::new_in(alloc);
    stack.push(Entry { prefix: OwnVec::new_in(alloc), rest_start: 0, rest_end: tree.len() });
    let mut steps = 0usize;

    while let Some(entry) = stack.pop() {
        steps += 1;
        if steps > 4096 {
            break;
        }
        let s = tree.get(entry.rest_start..entry.rest_end).unwrap_or("").trim();
        if s.is_empty() || s == "*" {
            if !entry.prefix.is_empty() {
                let alias = OwnString::from_str_in("*", alloc);
                out.push(UseBinding { path: entry.prefix, alias });
            }
            continue;
        }
        // Check for a top-level `{ ... }` group.
        if let Some(open) = find_top_level_byte(s, b'{')
            && let Some(close) = find_matching_brace_rel(s, open)
        {
            let head = s[..open].trim().trim_end_matches("::").trim();
            let mut base = OwnVec::with_capacity_in(entry.prefix.len() + 4, alloc);
            for p in entry.prefix.iter() {
                base.push(OwnString::from_str_in(p.as_str(), alloc));
            }
            for seg in head.split("::").map(str::trim).filter(|p| !p.is_empty()) {
                base.push(OwnString::from_str_in(seg, alloc));
            }
            let inner = &s[open + 1..close];
            let inner_base_off = entry.rest_start
                + usize::try_from(
                    s.as_ptr().addr().saturating_sub(tree.as_ptr().addr()),
                )
                .unwrap_or(0)
                + open
                + 1;
            let mut part_start = 0usize;
            let mut depth = 0u32;
            let mut parts: OwnVec<(usize, usize), A> = OwnVec::new_in(alloc);
            for (idx, b) in inner.bytes().enumerate() {
                match b {
                    b'{' => depth = depth.saturating_add(1),
                    b'}' => depth = depth.saturating_sub(1),
                    b',' if depth == 0 => {
                        parts.push((part_start, idx));
                        part_start = idx + 1;
                    }
                    _ => {}
                }
            }
            parts.push((part_start, inner.len()));
            for &(ps, pe) in parts.iter().rev() {
                let mut cloned = OwnVec::with_capacity_in(base.len(), alloc);
                for p in base.iter() {
                    cloned.push(OwnString::from_str_in(p.as_str(), alloc));
                }
                stack.push(Entry {
                    prefix: cloned,
                    rest_start: inner_base_off + ps,
                    rest_end: inner_base_off + pe,
                });
            }
            continue;
        }

        // Leaf: `a::b::C` or `a::b::C as Alias` or `self` or `self as Alias`.
        let (path_part, explicit_alias) = match s.split_once(" as ") {
            Some((p, a)) => (p.trim(), Some(a.trim())),
            None => (s, None),
        };
        let mut full = entry.prefix;
        for seg in path_part.split("::").map(str::trim).filter(|p| !p.is_empty()) {
            if seg != "self" {
                full.push(OwnString::from_str_in(seg, alloc));
            }
        }
        let alias_str = explicit_alias.or_else(|| full.last().map(OwnString::as_str)).unwrap_or("");
        if !full.is_empty() && !alias_str.is_empty() && alias_str != "_" {
            let alias = OwnString::from_str_in(alias_str, alloc);
            out.push(UseBinding { path: full, alias });
        }
    }
}

fn find_top_level_byte(s: &str, needle: u8) -> Option<usize> {
    s.bytes().position(|b| b == needle)
}

fn find_matching_brace_rel(s: &str, open: usize) -> Option<usize> {
    let mut depth = 0u32;
    for (idx, b) in s.bytes().enumerate().skip(open) {
        match b {
            b'{' => depth = depth.saturating_add(1),
            b'}' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return Some(idx);
                }
            }
            _ => {}
        }
    }
    None
}

fn short_impl_target_type(sig: &str) -> &str {
    // Strip `where ...` clause, then take the type after ` for ` if trait impl, else after `impl<...>`.
    let before_where = sig.split(" where ").next().unwrap_or(sig).trim();
    let target = if let Some((_, rhs)) = before_where.rsplit_once(" for ") {
        rhs.trim()
    } else {
        let rest = before_where.strip_prefix("unsafe ").unwrap_or(before_where).trim();
        let rest = rest.strip_prefix("impl").unwrap_or(rest).trim();
        skip_angle_generics(rest).trim()
    };
    // Strip leading `&` / `mut` and trailing `<...>`.
    let target = target.trim_start_matches('&').trim_start_matches("mut ").trim();
    let head = target.split('<').next().unwrap_or(target).trim();
    head.rsplit("::").next().unwrap_or(head).trim()
}

fn skip_angle_generics(s: &str) -> &str {
    if !s.starts_with('<') {
        return s;
    }
    let mut depth = 0u32;
    for (idx, b) in s.bytes().enumerate() {
        match b {
            b'<' => depth = depth.saturating_add(1),
            b'>' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return s.get(idx + 1..).unwrap_or("");
                }
            }
            _ => {}
        }
    }
    s
}

fn unquote_basic_str(raw: &str) -> &str {
    if let Some(inner) = raw.strip_prefix('"').and_then(|s| s.strip_suffix('"')) {
        return inner;
    }
    if let Some(rest) = raw.strip_prefix('r') {
        let hashes = rest.bytes().take_while(|&b| b == b'#').count();
        if rest.as_bytes().get(hashes) == Some(&b'"') {
            let after_open = &rest[hashes + 1..];
            if let Some(end_quote) = after_open.len().checked_sub(hashes + 1)
                && after_open.as_bytes().get(end_quote) == Some(&b'"')
            {
                return &after_open[..end_quote];
            }
        }
    }
    raw
}

fn shorten_reason<A: Allocator>(msg: &str, alloc: A) -> OwnString<A> {
    let first_line = msg.lines().next().unwrap_or(msg).trim();
    let trimmed = first_line.strip_prefix("inspect: ").unwrap_or(first_line);
    let mut out = OwnString::new_in(alloc);
    for (n, ch) in trimmed.chars().enumerate() {
        if n >= 140 {
            out.push_str("...");
            break;
        }
        out.push(ch);
    }
    out
}

fn edition_label(ed: PackageEdition) -> &'static str {
    if ed.blaze.is_some() {
        return "blaze-2026.11";
    }
    match ed.syntax {
        edition::Edition::Edition2015 => "2015",
        edition::Edition::Edition2018 => "2018",
        edition::Edition::Edition2021 => "2021",
        edition::Edition::Edition2024 => "2024",
    }
}

fn leading_indent(line: &str) -> &str {
    let count = line.bytes().take_while(|&b| b == b' ' || b == b'\t').count();
    line.get(..count).unwrap_or("")
}

fn is_ident_start(b: u8) -> bool {
    b.is_ascii_alphabetic() || b == b'_'
}

fn is_ident_cont(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

fn skip_spaces(bytes: &[u8], mut i: usize, end: usize) -> Option<usize> {
    while i < end && bytes[i].is_ascii_whitespace() {
        i += 1;
    }
    (i < end).then_some(i)
}

fn skip_block_comment(bytes: &[u8], mut i: usize, end: usize) -> usize {
    let mut depth = 0u32;
    while i + 1 < end {
        if bytes[i] == b'/' && bytes[i + 1] == b'*' {
            depth = depth.saturating_add(1);
            i += 2;
        } else if bytes[i] == b'*' && bytes[i + 1] == b'/' {
            depth = depth.saturating_sub(1);
            i += 2;
            if depth == 0 {
                return i;
            }
        } else {
            i += 1;
        }
    }
    end
}

fn skip_quoted(bytes: &[u8], mut i: usize, end: usize, quote: u8) -> usize {
    i += 1;
    while i < end {
        if bytes[i] == b'\\' {
            i = (i + 2).min(end);
            continue;
        }
        if bytes[i] == quote {
            return i + 1;
        }
        i += 1;
    }
    end
}

fn try_skip_raw_string(bytes: &[u8], start: usize, end: usize) -> Option<usize> {
    let mut i = start;
    if bytes.get(i) == Some(&b'b') {
        i += 1;
    }
    if bytes.get(i) != Some(&b'r') {
        return None;
    }
    i += 1;
    let mut hashes = 0usize;
    while i < end && bytes[i] == b'#' {
        hashes += 1;
        i += 1;
    }
    if bytes.get(i) != Some(&b'"') {
        return None;
    }
    i += 1;
    while i < end {
        if bytes[i] == b'"' {
            let mut matched = 0usize;
            while matched < hashes && bytes.get(i + 1 + matched) == Some(&b'#') {
                matched += 1;
            }
            if matched == hashes {
                return Some(i + 1 + hashes);
            }
        }
        i += 1;
    }
    Some(end)
}

fn skip_char_or_lifetime(bytes: &[u8], start: usize, end: usize) -> usize {
    // Distinguish `'x'` / `'\n'` from lifetime `'a`.
    if bytes.get(start + 1) == Some(&b'\\') {
        return skip_quoted(bytes, start, end, b'\'');
    }
    if bytes.get(start + 2) == Some(&b'\'') {
        return (start + 3).min(end);
    }
    // Lifetime `'ident`
    let mut i = start + 1;
    while i < end && is_ident_cont(bytes[i]) {
        i += 1;
    }
    i
}

fn is_ignored_ident(s: &str) -> bool {
    matches!(
        s,
        "as" | "async" | "await" | "break" | "const" | "continue" | "crate" | "dyn" | "else"
            | "enum" | "extern" | "false" | "fn" | "for" | "if" | "impl" | "in" | "let" | "loop"
            | "match" | "mod" | "move" | "mut" | "pub" | "ref" | "return" | "self" | "Self"
            | "static" | "struct" | "super" | "trait" | "true" | "type" | "unsafe" | "use"
            | "where" | "while" | "bool" | "char" | "str" | "u8" | "u16" | "u32" | "u64"
            | "u128" | "usize" | "i8" | "i16" | "i32" | "i64" | "i128" | "isize" | "f32"
            | "f64" | "Ok" | "Err" | "Some" | "None" | "Option" | "Result" | "Vec" | "String"
            | "Box" | "Arc" | "Copy" | "Clone" | "Debug" | "Default" | "PartialEq" | "Eq"
            | "PartialOrd" | "Ord" | "Hash" | "Send" | "Sync" | "Sized" | "Drop" | "From"
            | "Into" | "Iterator" | "IntoIterator" | "Fn" | "FnMut" | "FnOnce"
    )
}

fn is_generic_method_name(s: &str) -> bool {
    matches!(
        s,
        "new" | "len" | "is_empty" | "push" | "pop" | "get" | "get_mut" | "iter" | "iter_mut"
            | "into_iter" | "clone" | "as_str" | "as_bytes" | "as_slice" | "unwrap" | "expect"
            | "map" | "and_then" | "or_else" | "ok" | "err" | "copied" | "cloned" | "clear"
            | "truncate" | "drain" | "extend" | "insert" | "remove" | "contains" | "first"
            | "last" | "next" | "collect" | "count" | "min" | "max" | "trim" | "split"
            | "starts_with" | "ends_with" | "fmt" | "write" | "read" | "to_string" | "into"
            | "from" | "default" | "eq" | "cmp" | "hash"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::front2::inspect::RUST_2024;

    fn test_alloc() -> Alloc<'static> {
        blaze_own::testing::heap().alloc()
    }

    #[test]
    fn line_range_parses_all_forms() {
        assert_eq!(LineRange::parse("42"), Some(LineRange { start: 42, end: 42 }));
        assert_eq!(LineRange::parse("10-80"), Some(LineRange { start: 10, end: 80 }));
        assert_eq!(LineRange::parse("10..=80"), Some(LineRange { start: 10, end: 80 }));
        assert_eq!(LineRange::parse("10..80"), Some(LineRange { start: 10, end: 79 }));
        assert_eq!(LineRange::parse("10-"), Some(LineRange { start: 10, end: u32::MAX }));
        assert_eq!(LineRange::parse("0-5"), None);
        assert_eq!(LineRange::parse("20-10"), None);
    }

    #[test]
    fn inspect_file_view_emits_outline_types_and_cross_file_inline_docs() {
        let alloc = test_alloc();
        let dep_src = "\
/// A monotonic deadline on the driver's clock.
pub struct Deadline {
    pub nanos: u64,
}

impl Deadline {
    /// Checks whether this deadline has already elapsed.
    pub fn has_passed(&self, now: u64) -> bool {
        now >= self.nanos
    }
}
";
        let main_src = "\
//! Driver scheduler module.
use my_core::Deadline;

/// Runs one tick of the scheduler against `limit`.
pub fn step_once(limit: Deadline, now: u64) -> bool {
    let expired = limit.has_passed(now);
    expired
}
";
        let ctx = [ContextFile {
            path: "crates/my-core/src/wait.rs",
            crate_name: "my_core",
            text: dep_src,
        }];
        let view = inspect_file_view(
            "crates/app/src/sched.rs",
            main_src,
            RUST_2024,
            InspectTarget::Lines(LineRange { start: 4, end: 8 }),
            &ctx,
            ViewOpts::default(),
            alloc,
            alloc,
        )
        .expect("inspect_file_view");

        assert_eq!(view.view_range, LineRange { start: 4, end: 8 });
        assert_eq!(view.module_doc.as_ref().map(OwnString::as_str), Some("Driver scheduler module."));
        assert_eq!(view.outline.len(), 1);
        assert_eq!(view.outline[0].name.as_str(), "step_once");

        let rendered = render_llm_view(&view, alloc);
        let text = rendered.as_str();
        assert!(text.contains("# crates/app/src/sched.rs (lines 4-8 of 8"), "text:\n{text}");
        assert!(text.contains("> doc: Driver scheduler module."), "text:\n{text}");
        assert!(text.contains("## Outline"), "text:\n{text}");
        assert!(text.contains("// @doc Deadline (crates/my-core/src/wait.rs:2): A monotonic deadline on the driver's clock."), "text:\n{text}");
        assert!(text.contains("// @doc Deadline::has_passed (crates/my-core/src/wait.rs:8): Checks whether this deadline has already elapsed."), "text:\n{text}");
        assert!(text.contains("## Referenced Docs"), "text:\n{text}");

        let json = render_llm_json(&view, alloc);
        assert!(json.as_str().contains("\"defined_file\": \"crates/my-core/src/wait.rs\""));
        assert!(json.as_str().contains("\"name\": \"Deadline::has_passed\""));
    }

    #[test]
    fn inspect_file_view_combines_front2_type_hints_with_local_inline_docs() {
        let alloc = test_alloc();
        let src = "\
/// Computes twice the input value.
fn double(x: u32) -> _ {
    let doubled = x + x;
    doubled
}

/// Calls double and adds one.
fn bump(v: u32) -> u32 {
    let twice = double(v);
    twice + 1
}
";
        let view = inspect_file_view(
            "src/lib.rs",
            src,
            RUST_2024,
            InspectTarget::Item("bump"),
            &[],
            ViewOpts::default(),
            alloc,
            alloc,
        )
        .expect("inspect bump");

        assert!(matches!(view.semantic_status, SemanticStatus::Complete { checked: 1 }));
        assert!(view.hints.iter().any(|h| h.label.as_str() == ": u32"), "hints: {:?}", view.hints);

        let rendered = render_llm_view(&view, alloc);
        let text = rendered.as_str();
        assert!(text.contains("let twice/*: u32*/ = double(v);"), "text:\n{text}");
        assert!(text.contains("// @doc double (src/lib.rs:2): Computes twice the input value."), "text:\n{text}");
    }

    #[test]
    fn collect_import_hints_expands_nested_use_trees_and_mods() {
        let alloc = test_alloc();
        let src = "\
mod wait;
#[path = \"custom/io_mod.rs\"]
mod io;
use crate::fs::{Dir, File as MyFile};
use blaze_core::{Driver, sim::Sim};
";
        let hints = collect_import_hints(src, RUST_2024, alloc);
        assert_eq!(hints.mods.len(), 2);
        assert_eq!(hints.mods[0].0.as_str(), "wait");
        assert_eq!(hints.mods[0].1, None);
        assert_eq!(hints.mods[1].0.as_str(), "io");
        assert_eq!(hints.mods[1].1.as_ref().map(OwnString::as_str), Some("custom/io_mod.rs"));
        assert_eq!(hints.use_paths.len(), 4);
    }
}
