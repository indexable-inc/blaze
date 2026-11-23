//! Unused-dependency detection for `blaze check` (and strict enforcement in `blaze ci`).
//!
//! For each workspace member whose targets in the unit graph were checked completely (no refused
//! targets, no skipped `required-features` targets, no failed `CrateScope`/`Script`/`MacroModule`
//! or `FileItems` tasks, and no syntax errors in its outlined files), verifies that every
//! non-optional, non-feature-referenced, non-target-specific, non-`links` dependency declared in
//! that member's `Cargo.toml` (`[dependencies]`, `[dev-dependencies]`, `[build-dependencies]`) is
//! referenced in the member's source files.
//!
//! Soundness (zero false positives):
//! - Skips any member with incomplete resolution, failed scripts/macros, refused targets, skipped
//!   targets, unreadable files, or syntax errors.
//! - Skips optional dependencies, dependencies named by `[features]` (`dep:foo`, `foo/bar`,
//!   `foo?/bar`, `foo`), target-specific `[target.'cfg(..)'.dependencies]`, `links` / `*-sys`
//!   native link crates, and `bstd`/`blaze-std` in blaze editions (already reported by
//!   `TOOLCHAIN_NAMED`).
//! - Uses `blaze_lex::tokenize` so comments and string literals never mask unused deps, while
//!   recognizing `extern crate <ident>`, `<ident>::`, `::<ident>`, `use <ident>`, `use {<ident>}`,
//!   macro calls `<ident>::mac!()`, attributes `#[<ident>::attr]`, and derives
//!   `#[derive(<ident>::Trait)]` (including `r#<ident>` raw identifiers).

use blaze_drive::{Kind, Name};
use blaze_lex::{Cpu, FrontmatterAllowed, TokenKind, tokenize};
use blaze_manifest::layout;
use blaze_own::Vec;
use bstd::Cx;
use bstd::fs::{Dir, ReadWrite};
use bstd::mem::{Bytes, Frame};
use bstd::path::Path;

use crate::CliError;
use crate::engine_lines::Lines;
use crate::engine_names::NamesStage;
use crate::engine_tasks::Outlined;

/// Finding rule number in `blaze_report::frame::words::WORDINGS` (`Area::Cli`, `Kind::Warning`).
pub(crate) const UNUSED_DEPENDENCY: u16 = 48;

/// Scratch frame reserve for reading one member source file at a time (reset per file).
const SCAN_FRAME: usize = 64 << 20;

/// One candidate dependency declared in a workspace member's `Cargo.toml`.
pub(crate) struct DeclaredDep<'f> {
    /// The TOML key as written (e.g. `"my-dep"`).
    pub(crate) key: &'f str,
    /// Normalized Rust crate identifier (`'-'` replaced with `'_'`, e.g. `"my_dep"`).
    pub(crate) crate_ident: &'f str,
    /// Dependency table kind (`Normal`, `Dev`, `Build`).
    pub(crate) kind: layout::DepKind,
    /// Exact byte span of the dependency entry in `Cargo.toml`.
    pub(crate) span: (u32, u32),
}

impl DeclaredDep<'_> {
    pub(crate) fn section(&self) -> &'static str {
        match self.kind {
            layout::DepKind::Normal => "[dependencies]",
            layout::DepKind::Dev => "[dev-dependencies]",
            layout::DepKind::Build => "[build-dependencies]",
        }
    }
}

/// Summary of one workspace member's declared candidate dependencies, extracted once during
/// `units::lay`.
pub(crate) struct MemberDeps<'f> {
    pub(crate) manifest_path: &'f str,
    pub(crate) manifest_name: Name,
    pub(crate) dir: &'f str,
    pub(crate) package: &'f str,
    pub(crate) has_build_script: bool,
    pub(crate) build_script_path: Option<&'f str>,
    pub(crate) has_refused_targets: bool,
    pub(crate) dev_roots: Vec<&'f str, &'f Frame>,
    pub(crate) deps: Vec<DeclaredDep<'f>, &'f Frame>,
}

/// Normalize a Cargo dependency key to its Rust crate identifier (`-` -> `_`) in `frame`.
pub(crate) fn crate_ident_in<'f>(key: &str, frame: &'f Frame) -> &'f str {
    if !key.as_bytes().contains(&b'-') {
        return frame.alloc_str(key);
    }
    let mut out = blaze_own::String::with_capacity_in(key.len(), frame);
    for ch in key.chars() {
        out.push(if ch == '-' { '_' } else { ch });
    }
    frame.alloc_str(out.as_str())
}

/// Whether `key` is referenced by any entry of `[features]` in `member`.
fn is_feature_referenced(member: &layout::Member<'_>, key: &str) -> bool {
    for feat in member.features.iter() {
        for item in feat.enables.iter() {
            let s = item.as_str();
            if s == key {
                return true;
            }
            if let Some(rest) = s.strip_prefix("dep:")
                && rest == key
            {
                return true;
            }
            if let Some(rest) = s.strip_prefix(key)
                && (rest.starts_with('/') || rest.starts_with("?/"))
            {
                return true;
            }
        }
    }
    false
}

/// Whether `dep` looks like a link-only native sys crate (`links = ...` or `*-sys` / `*_sys`).
fn is_link_only(dep: &layout::Dependency<'_>, members: &[layout::Member<'_>]) -> bool {
    if let layout::Source::Member(idx) = dep.source
        && members.get(idx).is_some_and(|m| m.links.is_some())
    {
        return true;
    }
    let pkg = dep.package.as_str();
    pkg.ends_with("-sys") || pkg.ends_with("_sys")
}

/// Extract candidate declared dependencies for every workspace member from `units`.
pub(crate) fn extract_from_layout<'f>(
    units: &layout::Units<'_>,
    paths: &blaze_own::Texts<&Frame>,
    frame: &'f Frame,
) -> Vec<MemberDeps<'f>, &'f Frame> {
    let mut out = Vec::with_capacity_in(units.members.len(), frame);
    for member in units.members.iter() {
        let id = usize::try_from(member.manifest.0).unwrap_or(usize::MAX);
        if id >= paths.len() {
            continue;
        }
        let manifest_path = frame.alloc_str(paths.text(id));
        let manifest_name = Name::manifest(manifest_path.as_bytes());
        let dir = frame.alloc_str(member.dir.as_str());
        let package = frame.alloc_str(member.name.as_str());
        let build_script_path: Option<&'f str> = member.build.as_ref().map(|t| &*frame.alloc_str(t.root.as_str()));
        let mut dev_roots: Vec<&'f str, &'f Frame> = Vec::with_capacity_in(member.extras.len(), frame);
        for extra in member.extras.iter() {
            dev_roots.push(&*frame.alloc_str(extra.root.as_str()));
        }
        let mut deps = Vec::new_in(frame);
        let blaze_ed = member.edition.blaze.is_some();
        for dep in member.dependencies.iter() {
            // Skip toolchain lines (`bstd` / `blaze-std` in blaze editions): `TOOLCHAIN_NAMED` owns those.
            if !matches!(dep.source, layout::Source::Member(_))
                && crate::graph::toolchain_line(blaze_ed, dep.name.as_str(), dep.package.as_str())
            {
                continue;
            }
            // Soundness: skip optional deps, target-specific deps, feature-referenced deps, and link-only deps.
            if dep.optional || dep.target.is_some() || is_feature_referenced(member, dep.name.as_str()) || is_link_only(dep, &units.members) {
                continue;
            }
            let key = frame.alloc_str(dep.name.as_str());
            let crate_ident = crate_ident_in(key, frame);
            deps.push(DeclaredDep {
                key,
                crate_ident,
                kind: dep.kind,
                span: (dep.span.start, dep.span.end),
            });
        }
        out.push(MemberDeps {
            manifest_path,
            manifest_name,
            dir,
            package,
            has_build_script: member.build.is_some(),
            build_script_path,
            has_refused_targets: !member.refused_targets.is_empty(),
            dev_roots,
            deps,
        });
    }
    out
}

/// Whether `path` lies inside directory `dir` (`""` matches every relative path).
fn path_in_dir(path: &str, dir: &str) -> bool {
    if dir.is_empty() {
        return true;
    }
    match path.strip_prefix(dir) {
        Some(rest) => rest.starts_with('/'),
        None => false,
    }
}

/// Return the index of the most specific workspace member owning `path` (longest matching `dir`).
fn owner_member(path: &str, members: &[MemberDeps<'_>]) -> Option<usize> {
    let mut best: Option<(usize, usize)> = None;
    for (idx, member) in members.iter().enumerate() {
        if path_in_dir(path, member.dir) {
            let len = member.dir.len();
            let take = match best {
                None => true,
                Some((_, best_len)) => len > best_len,
            };
            if take {
                best = Some((idx, len));
            }
        }
    }
    best.map(|(idx, _)| idx)
}

/// Strip raw identifier prefix `r#` if present.
fn strip_raw(word: &str) -> &str {
    word.strip_prefix("r#").unwrap_or(word)
}

/// Check if `ident` matches any candidate in `targets` and mark its `used` slot.
fn mark_if_target(ident: &str, targets: &[&str], used: &mut [bool]) {
    for (&target, slot) in targets.iter().zip(used.iter_mut()) {
        if !*slot && ident == target {
            *slot = true;
        }
    }
}

/// Whether all entries in `used` are already `true`.
fn all_used(used: &[bool]) -> bool {
    used.iter().all(|&u| u)
}

/// Scan one Rust source file `text` with `blaze_lex` and mark every extern crate identifier from
/// `targets` that is referenced in `text`.
///
/// Recognizes:
/// - `extern crate <ident>` (including `extern crate <ident> as _;` and `#[macro_use] extern crate <ident>;`)
/// - `<ident>::` (covers `use`, `pub use`, paths, calls, macros `<ident>::m!()`, derives, attributes)
/// - `::<ident>` (covers absolute paths `::<ident>`, `use ::<ident>;`, etc.)
/// - `use <ident>` / `use { <ident>, ... }` (bare crate imports without `::`)
pub(crate) fn scan_used_crates(text: &str, targets: &[&str], used: &mut [bool]) {
    if targets.is_empty() || all_used(used) {
        return;
    }

    // Track the two previous non-trivia tokens.
    #[derive(Clone, Copy)]
    struct Prev<'t> {
        kind: TokenKind,
        word: &'t str,
        end: usize,
    }

    let mut prev2: Option<Prev<'_>> = None;
    let mut prev1: Option<Prev<'_>> = None;
    let mut in_use = false;
    let mut use_brace_depth: u32 = 0;
    let mut use_expect_root = false;
    let mut use_root_group = false;

    let mut offset: usize = 0;
    for token in tokenize(text, FrontmatterAllowed::No, Cpu::BASELINE) {
        let start = offset;
        let len = usize::try_from(token.len).unwrap_or(0);
        offset = offset.saturating_add(len);
        let kind = token.kind;
        if matches!(
            kind,
            TokenKind::Whitespace | TokenKind::LineComment { .. } | TokenKind::BlockComment { .. }
        ) {
            continue;
        }
        let Some(word) = text.get(start..offset) else {
            break;
        };

        let is_ident = matches!(kind, TokenKind::Ident | TokenKind::RawIdent);
        let ident = if is_ident { strip_raw(word) } else { "" };

        // 1. `<ident>::` (two adjacent `Colon` tokens immediately after an identifier).
        if kind == TokenKind::Colon
            && let (Some(p2), Some(p1)) = (prev2, prev1)
            && p1.kind == TokenKind::Colon
            && p1.end == start
            && matches!(p2.kind, TokenKind::Ident | TokenKind::RawIdent)
        {
            mark_if_target(strip_raw(p2.word), targets, used);
            if all_used(used) {
                return;
            }
        }

        // 2. `extern crate <ident>`
        if is_ident
            && let (Some(p2), Some(p1)) = (prev2, prev1)
            && p2.kind == TokenKind::Ident
            && p2.word == "extern"
            && p1.kind == TokenKind::Ident
            && p1.word == "crate"
        {
            mark_if_target(ident, targets, used);
            if all_used(used) {
                return;
            }
        }

        // 3. `::<ident>` (absolute crate path: two adjacent colons followed by ident).
        if is_ident
            && let (Some(p2), Some(p1)) = (prev2, prev1)
            && p2.kind == TokenKind::Colon
            && p1.kind == TokenKind::Colon
            && p2.end == p1.end.saturating_sub(1)
        {
            mark_if_target(ident, targets, used);
            if all_used(used) {
                return;
            }
        }

        // 4. Bare `use <ident>;`, `use <ident> as ...;`, `use { <ident>, ... };`
        if in_use {
            match kind {
                TokenKind::Semi if use_brace_depth == 0 => {
                    in_use = false;
                    use_expect_root = false;
                    use_root_group = false;
                }
                TokenKind::OpenBrace => {
                    use_brace_depth = use_brace_depth.saturating_add(1);
                    if use_brace_depth == 1
                        && prev1.is_some_and(|p| p.kind == TokenKind::Ident && p.word == "use")
                    {
                        use_root_group = true;
                        use_expect_root = true;
                    } else {
                        use_expect_root = false;
                    }
                }
                TokenKind::CloseBrace => {
                    use_brace_depth = use_brace_depth.saturating_sub(1);
                    if use_brace_depth == 0 {
                        use_root_group = false;
                    }
                    use_expect_root = false;
                }
                TokenKind::Comma if use_root_group && use_brace_depth == 1 => {
                    use_expect_root = true;
                }
                _ if is_ident && use_expect_root => {
                    mark_if_target(ident, targets, used);
                    use_expect_root = false;
                    if all_used(used) {
                        return;
                    }
                }
                _ => {
                    use_expect_root = false;
                }
            }
        } else if kind == TokenKind::Ident && word == "use" {
            in_use = true;
            use_brace_depth = 0;
            use_expect_root = true;
            use_root_group = false;
        }

        prev2 = prev1;
        prev1 = Some(Prev { kind, word, end: offset });
    }
}

/// Check whether a member's targets in the engine answer completed cleanly enough to report
/// unused dependencies without false positives.
fn member_checked_completely(
    member_idx: usize,
    members: &[MemberDeps<'_>],
    names: &NamesStage<'_>,
    finished: &blaze_engine::Finished<'_>,
    outlined: &[Outlined<'_>],
    skipped_roots: &[&str],
) -> bool {
    let Some(member) = members.get(member_idx) else {
        return false;
    };
    if member.has_refused_targets {
        return false;
    }
    // If any target of this member was skipped (e.g., required-features off), skip the member.
    for &skipped_path in skipped_roots {
        if owner_member(skipped_path, members) == Some(member_idx) {
            return false;
        }
    }
    // If any outlined file owned by this member failed or had syntax errors, skip the member.
    for one in outlined.iter() {
        let Ok(path) = core::str::from_utf8(&one.path) else {
            continue;
        };
        if owner_member(path, members) != Some(member_idx) {
            continue;
        }
        match &one.made {
            Err(_) => return false,
            Ok(rec) => {
                if rec.findings.iter().any(|&(rule, ..)| rule == crate::work::SYNTAX || rule == crate::work::FILE_NOT_TEXT) {
                    return false;
                }
            }
        }
    }
    // Check that every CrateScope (and any Script / MacroModule) for this member's crates succeeded.
    let Some(world) = names.names.as_ref() else {
        return false;
    };
    let list = world.list();
    let count = blaze_world::crate_list::CrateGraph::len(list);
    let mut saw_member_crate = false;
    for r in 0..count {
        if !list.member(r) || list.package(r) != member.package {
            continue;
        }
        saw_member_crate = true;
        let scope_name = blaze_drive::key::crate_scope_name(&list.key(r));
        match finished.answer.row_facts(scope_name) {
            Ok(Some(row)) if !row.failed => {}
            _ => return false,
        }
        // If the crate reads a build script, ensure that script succeeded.
        if let Some(script_bytes) = list.script(r)
            && let Some(raw_subject) = script_bytes.get(1..33)
        {
            let mut id = [0u8; 32];
            id.copy_from_slice(raw_subject);
            let script_name = Name::new(Kind::Script, blaze_ids::ContentId(id));
            match finished.answer.row_facts(script_name) {
                Ok(Some(row)) if !row.failed => {}
                _ => return false,
            }
        }
    }
    // Also ensure no MacroModule task failed across the workspace (an unexpanded proc macro
    // could generate references to declared dependencies).
    for &with_f in finished.answer.with_findings.iter() {
        if with_f.kind == Kind::MacroModule {
            if let Ok(Some(row)) = finished.answer.row_facts(with_f)
                && row.failed
            {
                return false;
            }
        }
    }
    saw_member_crate
}

/// Format the 3-argument finding message for `UNUSED_DEPENDENCY`:
/// `<key>\x1F<section>\x1F<package>`.
fn format_finding_words<'f>(key: &str, section: &str, package: &str, frame: &'f Frame) -> blaze_own::String<&'f Frame> {
    let cap = key.len() + section.len() + package.len() + 2;
    let mut words = blaze_own::String::with_capacity_in(cap, frame);
    words.push_str(key);
    words.push('\x1f');
    words.push_str(section);
    words.push('\x1f');
    words.push_str(package);
    words
}

/// Collect unused-dependency warnings for all completely-checked workspace members on the Phases
/// path and append them into `lines` under each member's manifest `Kind::Units` block.
#[expect(clippy::too_many_arguments, reason = "check-end analysis: each argument one engine stage input")]
pub(crate) fn collect(
    cx: &Cx,
    root: &Dir<ReadWrite>,
    laid: &crate::units::Laid<'_>,
    names: Option<&NamesStage<'_>>,
    finished: &blaze_engine::Finished<'_>,
    outlined: &[Outlined<'_>],
    skipped_roots: &[&str],
    lines: &mut Lines<'_>,
    frame: &Frame,
) -> Result<(), CliError> {
    let Some(names) = names else {
        return Ok(());
    };
    let members = laid.member_deps.as_slice();
    if members.is_empty() {
        return Ok(());
    }
    let has_any_candidates = members.iter().any(|m| !m.deps.is_empty());
    if !has_any_candidates {
        return Ok(());
    }

    let mut scratch = Frame::with_reserve(SCAN_FRAME, bstd::mem::Tag::Other)
        .map_err(|_| CliError::Text { what: "unused_deps: could not reserve scan frame" })?;

    for (member_idx, member) in members.iter().enumerate() {
        if member.deps.is_empty() {
            continue;
        }
        if !member_checked_completely(member_idx, members, names, finished, outlined, skipped_roots) {
            continue;
        }

        // Partition candidate deps into non-build (normal + dev) and build.
        let mut code_idents: Vec<&str, &Frame> = Vec::new_in(frame);
        let mut build_idents: Vec<&str, &Frame> = Vec::new_in(frame);
        for dep in member.deps.iter() {
            match dep.kind {
                layout::DepKind::Normal | layout::DepKind::Dev => {
                    if !code_idents.iter().any(|&id| id == dep.crate_ident) {
                        code_idents.push(dep.crate_ident);
                    }
                }
                layout::DepKind::Build => {
                    if !build_idents.iter().any(|&id| id == dep.crate_ident) {
                        build_idents.push(dep.crate_ident);
                    }
                }
            }
        }

        let mut code_used: Vec<bool, &Frame> = Vec::with_capacity_in(code_idents.len(), frame);
        code_used.extend(code_idents.iter().map(|_| false));
        let mut build_used: Vec<bool, &Frame> = Vec::with_capacity_in(build_idents.len(), frame);
        build_used.extend(build_idents.iter().map(|_| false));

        // 1. Scan all non-build `.rs` source files owned by this member (outlined files + dev roots).
        let mut code_scan_complete = true;
        let mut dev_scan_complete = true;
        if !code_idents.is_empty() {
            let mut scanned: Vec<&str, &Frame> = Vec::new_in(frame);
            for one in outlined.iter() {
                if all_used(&code_used) {
                    break;
                }
                let Ok(path) = core::str::from_utf8(&one.path) else {
                    continue;
                };
                if !path.ends_with(".rs") || owner_member(path, members) != Some(member_idx) {
                    continue;
                }
                if member.build_script_path == Some(path) {
                    continue;
                }
                scanned.push(path);
                scratch.reset();
                let mut bytes = Bytes::new_in(&scratch, "unused_deps scan");
                let Ok(path_obj) = Path::new(path) else {
                    code_scan_complete = false;
                    dev_scan_complete = false;
                    continue;
                };
                if root.read_all(cx, &path_obj, &mut bytes).is_err() {
                    code_scan_complete = false;
                    dev_scan_complete = false;
                    continue;
                }
                if let Ok(text) = core::str::from_utf8(bytes.as_slice()) {
                    scan_used_crates(text, &code_idents, &mut code_used);
                } else {
                    code_scan_complete = false;
                    dev_scan_complete = false;
                }
            }
            // Also scan any dev roots (tests/examples/benches) that weren't in `outlined`.
            for &dev_path in member.dev_roots.iter() {
                if all_used(&code_used) {
                    break;
                }
                if scanned.iter().any(|&p| p == dev_path) {
                    continue;
                }
                scratch.reset();
                let mut bytes = Bytes::new_in(&scratch, "unused_deps dev scan");
                let Ok(path_obj) = Path::new(dev_path) else {
                    dev_scan_complete = false;
                    continue;
                };
                if root.read_all(cx, &path_obj, &mut bytes).is_err() {
                    dev_scan_complete = false;
                    continue;
                }
                if let Ok(text) = core::str::from_utf8(bytes.as_slice()) {
                    scan_used_crates(text, &code_idents, &mut code_used);
                } else {
                    dev_scan_complete = false;
                }
            }
        }

        // 2. Scan `build.rs` (if present) for `[build-dependencies]`.
        let mut build_scan_complete = true;
        if !build_idents.is_empty() && member.has_build_script {
            if let Some(build_path) = member.build_script_path {
                scratch.reset();
                let mut bytes = Bytes::new_in(&scratch, "unused_deps build.rs scan");
                let read_ok = Path::new(build_path)
                    .ok()
                    .and_then(|p| root.read_all(cx, &p, &mut bytes).ok())
                    .is_some();
                if read_ok {
                    if let Ok(text) = core::str::from_utf8(bytes.as_slice()) {
                        scan_used_crates(text, &build_idents, &mut build_used);
                    } else {
                        build_scan_complete = false;
                    }
                } else {
                    build_scan_complete = false;
                }
            } else {
                build_scan_complete = false;
            }
        }

        // 3. Emit `UNUSED_DEPENDENCY` findings in deterministic manifest order.
        for dep in member.deps.iter() {
            let is_used = match dep.kind {
                layout::DepKind::Normal => {
                    if !code_scan_complete {
                        continue;
                    }
                    code_idents
                        .iter()
                        .position(|&id| id == dep.crate_ident)
                        .is_some_and(|i| code_used.get(i).copied().unwrap_or(false))
                }
                layout::DepKind::Dev => {
                    if !code_scan_complete || !dev_scan_complete {
                        continue;
                    }
                    code_idents
                        .iter()
                        .position(|&id| id == dep.crate_ident)
                        .is_some_and(|i| code_used.get(i).copied().unwrap_or(false))
                }
                layout::DepKind::Build => {
                    if !build_scan_complete {
                        continue;
                    }
                    build_idents
                        .iter()
                        .position(|&id| id == dep.crate_ident)
                        .is_some_and(|i| build_used.get(i).copied().unwrap_or(false))
                }
            };
            if !is_used {
                let words = format_finding_words(dep.key, dep.section(), member.package, frame);
                lines.finding(
                    member.manifest_name,
                    UNUSED_DEPENDENCY,
                    dep.span.0,
                    dep.span.1,
                    Some(member.manifest_path.as_bytes()),
                    words.as_bytes(),
                );
            }
        }
    }
    Ok(())
}

/// Compute a safe single-line deletion edit for an `UNUSED_DEPENDENCY` finding in `Cargo.toml`.
///
/// Returns `Some((line_start, line_end, dep_key))` when the span sits on a single key-value line
/// (`foo = ...`) rather than a multi-line `[dependencies.foo]` table header.
pub(crate) fn line_deletion_fix<'a>(text: &str, start: u32, end: u32, words: &'a [u8]) -> Option<(u32, u32, &'a str)> {
    let words_str = core::str::from_utf8(words).ok()?;
    let dep_key = words_str.split('\x1f').next()?;
    let bytes = text.as_bytes();
    let s = usize::try_from(start).ok()?.min(bytes.len());
    let e = usize::try_from(end).ok()?.clamp(s, bytes.len());
    let line_start = bytes.get(..s)?.iter().rposition(|&b| b == b'\n').map_or(0, |at| at + 1);
    let line_end_no_nl = bytes.get(e..)?.iter().position(|&b| b == b'\n').map_or(bytes.len(), |at| e + at);
    let snippet = text.get(line_start..line_end_no_nl)?.trim();
    // Do not blindly delete a `[dependencies.foo]` table header; its sub-keys live on later lines.
    if snippet.starts_with('[') {
        return None;
    }
    // Only auto-delete when the entry spans a single line (inline table or version string).
    if bytes.get(s..e).is_some_and(|slice| slice.contains(&b'\n')) {
        return None;
    }
    let line_end = if bytes.get(line_end_no_nl) == Some(&b'\n') {
        line_end_no_nl + 1
    } else {
        line_end_no_nl
    };
    Some((u32::try_from(line_start).ok()?, u32::try_from(line_end).ok()?, dep_key))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scanner_recognizes_all_rust_crate_reference_forms() {
        let targets = ["alpha", "beta", "gamma", "delta", "eps", "zeta", "unused_dep"];
        let mut used = [false; 7];
        let src = r#"
            // unused_dep::should_not_count();
            /* extern crate unused_dep; */
            const S: &str = "unused_dep::in_string()";

            use alpha::Thing;
            extern crate beta as _;
            use gamma;

            #[derive(delta::MyDerive)]
            #[eps::my_attr]
            pub struct Foo;

            pub fn run() {
                let _ = ::zeta::call();
            }
        "#;
        scan_used_crates(src, &targets, &mut used);
        assert!(used[0], "use alpha::Thing");
        assert!(used[1], "extern crate beta as _");
        assert!(used[2], "bare use gamma");
        assert!(used[3], "derive(delta::MyDerive)");
        assert!(used[4], "attribute #[eps::my_attr]");
        assert!(used[5], "absolute ::zeta::call()");
        assert!(!used[6], "unused_dep in comments and strings must not count");
    }

    #[test]
    fn scanner_recognizes_macro_and_raw_ident() {
        let targets = ["my_mac", "r_dep", "nope"];
        let mut used = [false; 3];
        let src = r#"
            pub fn f() {
                my_mac::hello!();
                let _ = r#r_dep::val();
            }
        "#;
        scan_used_crates(src, &targets, &mut used);
        assert!(used[0], "macro call my_mac::hello!()");
        assert!(used[1], "raw ident r#r_dep::val()");
        assert!(!used[2], "nope is unused");
    }

    #[test]
    fn line_deletion_handles_inline_and_skips_subtable_header() {
        let toml = "[package]\nname = \"a\"\n\n[dependencies]\nfoo = \"1.0\"\n[dependencies.bar]\nversion = \"1\"\n";
        let foo_start = toml.find("foo = ").map_or(0, |i| u32::try_from(i).unwrap_or(0));
        let foo_end = foo_start + 11;
        let words = b"foo\x1f[dependencies]\x1fa";
        let fix = line_deletion_fix(toml, foo_start, foo_end, words);
        assert!(fix.is_some());
        if let Some((s, e, key)) = fix {
            assert_eq!(key, "foo");
            let s_idx = usize::try_from(s).unwrap_or(0);
            let e_idx = usize::try_from(e).unwrap_or(0);
            assert_eq!(&toml[s_idx..e_idx], "foo = \"1.0\"\n");
        }

        let bar_start = toml.find("[dependencies.bar]").map_or(0, |i| u32::try_from(i).unwrap_or(0));
        let bar_end = bar_start + 18;
        let bar_words = b"bar\x1f[dependencies]\x1fa";
        assert!(line_deletion_fix(toml, bar_start, bar_end, bar_words).is_none());
    }
}
