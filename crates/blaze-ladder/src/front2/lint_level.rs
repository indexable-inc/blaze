//! Lexical rustc typed-lint level resolution (`deprecated_safe_2024` and
//! `unsafe_op_in_unsafe_fn`) inside `front2`.
//!
//! Runs ONLY when a checked body actually emits a `TypedFinding::DeprecatedSafe2024` or
//! `TypedFinding::UnsafeOpInUnsafeFn`, using the exact unit's `NamesSource`:
//! - Exact post-build-script and target/test `cfg` atoms (`r.names.cfg_atoms(krate)`).
//! - Exact module ancestor chain (`r.names.module_info(m)`) across external `mod foo;`
//!   declarations and inline `mod` blocks.
//! - Real `parser::TopEntryPoint::SourceFile` CST (flat iterative intrusive arena; zero
//!   brace-counting heuristics, handles `#[deny(..)] fn f() -> Ty!{} { .. }` and
//!   `fn f() -> Foo<{ 1 }> { .. }` and arbitrary nesting depths).
//! - Real `blaze_expand::cfg_attr_metas` and `blaze_expand::cfg_predicate_at` (iterative
//!   explicit-allocator stack; zero depth cap).
//! - File reads go through `r.names.file_text` (recording `NameCell::Whole { section: Items }`
//!   on `Tables`), so editing lint attributes in `src/lib.rs` or any ancestor module file
//!   invalidates affected `BodyCheck` rows and standing replay automatically (Rule 21).

use blaze_expand::{Delim, TokenKind, TokenStream};
use blaze_id::ItemKey as LibKey;
use blaze_own::{AString, AVec, Alloc};
use blaze_scope::resolve::At;
use blaze_surface::Edition;
use parser::{LexedStr, Step, SyntaxKind, TopEntryPoint};

use super::check::Records;
use super::source::NamesSource;

/// Which rustc typed lint a body finding represents.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum RustcLint {
    /// `deprecated_safe_2024` (RFC 3458). When `in_unsafe_fn` is true, the call is inside an
    /// `unsafe fn` body outside `unsafe {}`, so rustc fires it only when `unsafe_op_in_unsafe_fn`
    /// is not `Allow` (`Off`).
    DeprecatedSafe2024 { in_unsafe_fn: bool },
    /// `unsafe_op_in_unsafe_fn` (default `Allow` in editions < 2024, `Warn` from 2024).
    UnsafeOpInUnsafeFn,
}

/// Final resolved diagnostic level for a rustc typed lint finding.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResolvedLintLevel {
    Off,
    Warn,
    Error,
}

const IDX_DEPRECATED_SAFE: usize = 0;
const IDX_UNSAFE_OP: usize = 1;
const BIT_DEPRECATED_SAFE: u8 = 1 << 0;
const BIT_UNSAFE_OP: u8 = 1 << 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RawLevel {
    Allow,
    Warn,
    Deny,
    Forbid,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WarningsLevel {
    Default,
    Allow,
    Warn,
    Deny,
    Forbid,
}

/// Outer-to-inner lint state for `deprecated_safe_2024`, `unsafe_op_in_unsafe_fn`, and the
/// `warnings` meta-lint.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct LintState {
    level: [RawLevel; 2],
    warnings: WarningsLevel,
}

impl LintState {
    const fn default_for(edition: Edition) -> Self {
        let unsafe_op = if matches!(edition, Edition::E2024) {
            RawLevel::Warn
        } else {
            RawLevel::Allow
        };
        LintState {
            level: [RawLevel::Allow, unsafe_op],
            warnings: WarningsLevel::Default,
        }
    }

    fn apply(&mut self, action: RawLevel, targets: u8, touches_warnings: bool) {
        for (idx, bit) in [(IDX_DEPRECATED_SAFE, BIT_DEPRECATED_SAFE), (IDX_UNSAFE_OP, BIT_UNSAFE_OP)] {
            if targets & bit == 0 {
                continue;
            }
            if self.level[idx] == RawLevel::Forbid {
                continue;
            }
            if action == RawLevel::Warn && self.warnings == WarningsLevel::Forbid {
                self.level[idx] = RawLevel::Forbid;
            } else {
                self.level[idx] = action;
            }
        }
        if touches_warnings {
            if self.warnings != WarningsLevel::Forbid {
                self.warnings = match action {
                    RawLevel::Allow => WarningsLevel::Allow,
                    RawLevel::Warn => WarningsLevel::Warn,
                    RawLevel::Deny => WarningsLevel::Deny,
                    RawLevel::Forbid => WarningsLevel::Forbid,
                };
            }
            if self.warnings == WarningsLevel::Forbid {
                for slot in &mut self.level {
                    if *slot == RawLevel::Warn {
                        *slot = RawLevel::Forbid;
                    }
                }
            }
        }
    }

    const fn resolve_idx(self, idx: usize) -> ResolvedLintLevel {
        match self.level[idx] {
            RawLevel::Allow => ResolvedLintLevel::Off,
            RawLevel::Deny | RawLevel::Forbid => ResolvedLintLevel::Error,
            RawLevel::Warn => match self.warnings {
                WarningsLevel::Allow => ResolvedLintLevel::Off,
                WarningsLevel::Deny | WarningsLevel::Forbid => ResolvedLintLevel::Error,
                WarningsLevel::Default | WarningsLevel::Warn => ResolvedLintLevel::Warn,
            },
        }
    }

    const fn resolve(self, which: RustcLint) -> ResolvedLintLevel {
        match which {
            RustcLint::UnsafeOpInUnsafeFn => self.resolve_idx(IDX_UNSAFE_OP),
            RustcLint::DeprecatedSafe2024 { in_unsafe_fn: false } => self.resolve_idx(IDX_DEPRECATED_SAFE),
            RustcLint::DeprecatedSafe2024 { in_unsafe_fn: true } => {
                if matches!(self.resolve_idx(IDX_UNSAFE_OP), ResolvedLintLevel::Off) {
                    ResolvedLintLevel::Off
                } else {
                    self.resolve_idx(IDX_DEPRECATED_SAFE)
                }
            }
        }
    }
}

/// Resolves the effective lint level for a rustc typed finding emitted while checking `lowered`
/// at item-relative byte offset `finding_at`.
pub(super) fn resolve_rustc_lint<'n, S: NamesSource<'n>>(
    r: &Records<'_, 'n, S>,
    lowered: &super::body::Body<'n>,
    finding_at: u32,
    which: RustcLint,
    alloc: Alloc<'_>,
) -> ResolvedLintLevel {
    let Some(krate) = r.names.crate_of(lowered.module) else {
        return LintState::default_for(Edition::E2024).resolve(which);
    };
    let edition = r.names.edition_of(krate).unwrap_or(Edition::E2024);
    let atoms = r.names.cfg_atoms(krate).unwrap_or(&[]);
    let mut state = LintState::default_for(edition);

    let item_text = lowered.lowered.view().item();
    let body_at = super::body::body_at(r.names, r.keys, r.store, lowered.item, lowered.owner, item_text);
    let placed = body_at.and_then(|at| super::body_host::Placed::of(r.names, krate, at, alloc));
    let leaf_file = body_at
        .and_then(|at| file_of_lib(r.names, krate, at.lib))
        .or_else(|| r.names.module_info(lowered.module).and_then(|(_, f, _)| f));

    // Walk ancestor modules from crate root down to `lowered.module`.
    let mut chain: AVec<'_, (LibKey, Option<u32>, Option<&'n str>)> = AVec::new_in(alloc);
    let mut cur = lowered.module;
    while let Some((parent, file, path)) = r.names.module_info(cur) {
        if chain.iter().any(|entry| entry.0 == cur) {
            break;
        }
        chain.push((cur, file, path));
        let Some(p) = parent else { break };
        cur = p;
    }
    chain.reverse();

    // Apply lint attributes across ancestor files and external `mod foo;` declarations until we
    // reach `leaf_file`.
    let mut cur_file: Option<u32> = None;
    let mut cur_text: Option<&'n str> = None;
    let mut cur_cst: Option<Cst<'_>> = None;
    let mut cur_node: u32 = NO_NODE;

    for &(_, mod_file, mod_path) in chain.iter() {
        let Some(file_idx) = mod_file else {
            continue;
        };
        let mod_name = mod_path.and_then(|p| p.rsplit("::").next());

        match cur_file {
            None => {
                if leaf_file == Some(file_idx) {
                    break;
                }
                let Some(text) = r.names.file_text(krate, file_idx) else {
                    break;
                };
                let cst = Cst::parse(text, edition, alloc);
                apply_node_attrs(&cst, cst.root(), true, text, edition, atoms, &mut state, alloc);
                cur_file = Some(file_idx);
                cur_text = Some(text);
                cur_node = cst.root();
                cur_cst = Some(cst);
            }
            Some(prev_file) if prev_file == file_idx => {
                // Inline module inside the same ancestor file.
                if let (Some(cst), Some(text), Some(name)) = (cur_cst.as_ref(), cur_text, mod_name)
                    && let Some(mod_node) = find_active_module(cst, cur_node, name, true, text, edition, atoms, alloc)
                {
                    apply_node_attrs(cst, mod_node, false, text, edition, atoms, &mut state, alloc);
                    if let Some(list) = cst.child_of_kind(mod_node, SyntaxKind::ITEM_LIST) {
                        apply_node_attrs(cst, list, true, text, edition, atoms, &mut state, alloc);
                        cur_node = list;
                    }
                }
            }
            Some(_) => {
                // Out-of-line `mod <name>;` declared in `cur_node` of `cur_file`, whose body is
                // in `file_idx`.
                if let (Some(cst), Some(text), Some(name)) = (cur_cst.as_ref(), cur_text, mod_name)
                    && let Some(mod_node) = find_active_module(cst, cur_node, name, false, text, edition, atoms, alloc)
                {
                    apply_node_attrs(cst, mod_node, false, text, edition, atoms, &mut state, alloc);
                }
                if leaf_file == Some(file_idx) {
                    break;
                }
                let Some(text) = r.names.file_text(krate, file_idx) else {
                    break;
                };
                let cst = Cst::parse(text, edition, alloc);
                apply_node_attrs(&cst, cst.root(), true, text, edition, atoms, &mut state, alloc);
                cur_file = Some(file_idx);
                cur_text = Some(text);
                cur_node = cst.root();
                cur_cst = Some(cst);
            }
        }
    }

    // Now walk the leaf file's CST from `SOURCE_FILE` down to `abs_at`, applying every ancestor
    // node's lint attributes (file-top `#![...]`, inline `mod`, `impl`, `trait`, `fn`, blocks,
    // statements, expressions) in exact outer-to-inner order.
    if let Some(placed) = placed {
        let abs_at = if placed.fixed {
            placed.start
        } else {
            placed.start.saturating_add(finding_at)
        };
        let leaf_cst = Cst::parse(placed.text, edition, alloc);
        apply_path_to_offset(&leaf_cst, abs_at, placed.text, edition, atoms, &mut state, alloc);
    } else if let Some(file_idx) = leaf_file
        && let Some(text) = r.names.file_text(krate, file_idx)
    {
        let leaf_cst = Cst::parse(text, edition, alloc);
        apply_node_attrs(&leaf_cst, leaf_cst.root(), true, text, edition, atoms, &mut state, alloc);
    }

    state.resolve(which)
}

/// Resolves the physical `file` index of `lib` (handling members via `member_owner` and
/// macro-expanded items via `call_text` up to the outermost file).
fn file_of_lib<'n, S: NamesSource<'n>>(names: S, krate: usize, lib: LibKey) -> Option<u32> {
    let place = match names.place_of(lib) {
        Some(p) => p,
        None => names.place_of(names.member_owner(lib)?.0)?,
    };
    if place.krate != krate {
        return None;
    }
    let mut at = place.at;
    for _ in 0..64 {
        match at {
            At::File(file) => return Some(file),
            At::Expansion(call) => at = names.call_text(krate, call)?,
        }
    }
    None
}

// ---------------------------------------------------------------------------------------------
// Flat iterative CST built directly from `parser::TopEntryPoint::SourceFile`.

const NO_NODE: u32 = u32::MAX;

#[derive(Clone, Copy)]
struct Node {
    kind: SyntaxKind,
    start: u32,
    end: u32,
    has_span: bool,
    first_child: u32,
    last_child: u32,
    next_sibling: u32,
}

struct Cst<'a> {
    nodes: AVec<'a, Node>,
}

const fn parser_edition(edition: Edition) -> parser::Edition {
    match edition {
        Edition::E2015 => parser::Edition::Edition2015,
        Edition::E2018 => parser::Edition::Edition2018,
        Edition::E2021 => parser::Edition::Edition2021,
        Edition::E2024 => parser::Edition::Edition2024,
    }
}

impl<'a> Cst<'a> {
    fn parse(text: &str, edition: Edition, alloc: Alloc<'a>) -> Self {
        let pe = parser_edition(edition);
        let lexed = LexedStr::new(pe, text);
        let input = lexed.to_input(pe);
        let output = TopEntryPoint::SourceFile.parse(input.tokens());

        // Precompute byte ranges of all non-trivia tokens in `lexed`.
        let mut tok_ranges: AVec<'_, (u32, u32)> = AVec::with_capacity_in(lexed.len(), alloc);
        for i in 0..lexed.len() {
            if !lexed.kind(i).is_trivia() {
                let r = lexed.text_range(i);
                let s = u32::try_from(r.start).unwrap_or(u32::MAX);
                let e = u32::try_from(r.end).unwrap_or(u32::MAX);
                tok_ranges.push((s, e));
            }
        }

        let mut nodes: AVec<'a, Node> = AVec::with_capacity_in(256, alloc);
        let mut stack: AVec<'_, u32> = AVec::with_capacity_in(32, alloc);
        let mut cursor = 0usize;

        let enter = |kind: SyntaxKind, nodes: &mut AVec<'a, Node>, stack: &mut AVec<'_, u32>| {
            let idx = u32::try_from(nodes.len()).unwrap_or(NO_NODE);
            if idx == NO_NODE {
                return;
            }
            nodes.push(Node {
                kind,
                start: 0,
                end: 0,
                has_span: false,
                first_child: NO_NODE,
                last_child: NO_NODE,
                next_sibling: NO_NODE,
            });
            if let Some(&parent_idx) = stack.last()
                && let Ok(p) = usize::try_from(parent_idx)
                && let Some(parent) = nodes.get_mut(p)
            {
                let prev_last = parent.last_child;
                if prev_last == NO_NODE {
                    parent.first_child = idx;
                } else if let Ok(pl) = usize::try_from(prev_last)
                    && let Some(sib) = nodes.get_mut(pl)
                {
                    sib.next_sibling = idx;
                }
                if let Some(parent) = nodes.get_mut(p) {
                    parent.last_child = idx;
                }
            }
            stack.push(idx);
        };

        let extend_top = |s: u32, e: u32, nodes: &mut AVec<'a, Node>, stack: &AVec<'_, u32>| {
            if let Some(&top_idx) = stack.last()
                && let Ok(t) = usize::try_from(top_idx)
                && let Some(node) = nodes.get_mut(t)
            {
                if !node.has_span {
                    node.start = s;
                    node.end = e;
                    node.has_span = true;
                } else {
                    if s < node.start {
                        node.start = s;
                    }
                    if e > node.end {
                        node.end = e;
                    }
                }
            }
        };

        let exit = |nodes: &mut AVec<'a, Node>, stack: &mut AVec<'_, u32>| {
            let Some(done_idx) = stack.pop() else { return };
            let span = usize::try_from(done_idx)
                .ok()
                .and_then(|i| nodes.get(i))
                .and_then(|n| n.has_span.then_some((n.start, n.end)));
            if let Some((s, e)) = span {
                extend_top(s, e, nodes, stack);
            }
        };

        for step in output.iter() {
            match step {
                Step::Enter { kind } => enter(kind, &mut nodes, &mut stack),
                Step::Exit => exit(&mut nodes, &mut stack),
                Step::Token { n_input_tokens, .. } => {
                    let count = usize::from(n_input_tokens);
                    if count > 0 {
                        let first = tok_ranges.get(cursor).copied();
                        let last = tok_ranges.get(cursor.saturating_add(count).saturating_sub(1)).copied();
                        cursor = cursor.saturating_add(count);
                        if let (Some((s, _)), Some((_, e))) = (first, last) {
                            extend_top(s, e, &mut nodes, &stack);
                        }
                    }
                }
                // `x.0.1`: matches `blaze-lower/src/feed.rs` `FloatSplit` stack protocol.
                Step::FloatSplit { ends_in_dot } => {
                    let whole = tok_ranges.get(cursor).copied();
                    cursor = cursor.saturating_add(1);
                    if let Some((ws, we)) = whole {
                        let slice = usize::try_from(ws)
                            .ok()
                            .zip(usize::try_from(we).ok())
                            .and_then(|(s, e)| text.get(s..e))
                            .unwrap_or("");
                        let dot_off = u32::try_from(slice.find('.').unwrap_or(slice.len())).unwrap_or(0);
                        let dot_start = ws.saturating_add(dot_off);
                        let dot_end = dot_start.saturating_add(1).min(we);
                        enter(SyntaxKind::NAME_REF, &mut nodes, &mut stack);
                        extend_top(ws, dot_start, &mut nodes, &stack);
                        exit(&mut nodes, &mut stack);
                        exit(&mut nodes, &mut stack);
                        extend_top(dot_start, dot_end, &mut nodes, &stack);
                        if !ends_in_dot {
                            enter(SyntaxKind::NAME_REF, &mut nodes, &mut stack);
                            extend_top(dot_end, we, &mut nodes, &stack);
                            exit(&mut nodes, &mut stack);
                            exit(&mut nodes, &mut stack);
                        }
                    } else {
                        exit(&mut nodes, &mut stack);
                        if !ends_in_dot {
                            exit(&mut nodes, &mut stack);
                        }
                    }
                }
                Step::Error { .. } => {}
            }
        }

        Cst { nodes }
    }

    fn root(&self) -> u32 {
        if self.nodes.is_empty() { NO_NODE } else { 0 }
    }

    fn get(&self, idx: u32) -> Option<&Node> {
        usize::try_from(idx).ok().and_then(|i| self.nodes.get(i))
    }

    fn child_of_kind(&self, parent: u32, kind: SyntaxKind) -> Option<u32> {
        let mut c = self.get(parent)?.first_child;
        while c != NO_NODE {
            let node = self.get(c)?;
            if node.kind == kind {
                return Some(c);
            }
            c = node.next_sibling;
        }
        None
    }

    fn text_of<'t>(&self, idx: u32, text: &'t str) -> Option<&'t str> {
        let node = self.get(idx)?;
        if !node.has_span {
            return None;
        }
        let s = usize::try_from(node.start).ok()?;
        let e = usize::try_from(node.end).ok()?;
        text.get(s..e)
    }
}

/// Finds an active `MODULE` child of `container` (`SOURCE_FILE` or `ITEM_LIST`) named `name`
/// whose `#[cfg(...)]` attributes all evaluate to true under `atoms`. `want_inline` selects
/// inline `mod name { .. }` (`true`) vs out-of-line `mod name;` (`false`).
#[expect(clippy::too_many_arguments, reason = "CST lookup inputs, each named")]
fn find_active_module(
    cst: &Cst<'_>,
    container: u32,
    name: &str,
    want_inline: bool,
    text: &str,
    edition: Edition,
    atoms: &[(&str, Option<&str>)],
    alloc: Alloc<'_>,
) -> Option<u32> {
    let mut c = cst.get(container)?.first_child;
    while c != NO_NODE {
        let node = cst.get(c)?;
        if node.kind == SyntaxKind::MODULE {
            let has_body = cst.child_of_kind(c, SyntaxKind::ITEM_LIST).is_some();
            if has_body == want_inline
                && let Some(name_node) = cst.child_of_kind(c, SyntaxKind::NAME)
                && let Some(raw_name) = cst.text_of(name_node, text)
            {
                let ident = raw_name.trim().strip_prefix("r#").unwrap_or(raw_name.trim());
                if ident == name && node_cfg_active(cst, c, text, edition, atoms, alloc) {
                    return Some(c);
                }
            }
        }
        c = node.next_sibling;
    }
    None
}

/// Walks from `cst.root()` down to the deepest node containing byte offset `at`, applying every
/// node's direct `ATTR` children in outer-to-inner order.
fn apply_path_to_offset(
    cst: &Cst<'_>,
    at: u32,
    text: &str,
    edition: Edition,
    atoms: &[(&str, Option<&str>)],
    state: &mut LintState,
    alloc: Alloc<'_>,
) {
    let mut cur = cst.root();
    while cur != NO_NODE {
        let Some(node) = cst.get(cur) else { break };
        let inner_only = matches!(
            node.kind,
            SyntaxKind::SOURCE_FILE
                | SyntaxKind::ITEM_LIST
                | SyntaxKind::STMT_LIST
                | SyntaxKind::ASSOC_ITEM_LIST
                | SyntaxKind::EXTERN_ITEM_LIST
        );
        apply_node_attrs(cst, cur, inner_only, text, edition, atoms, state, alloc);

        // Descend into the child whose span contains `at` (prefer strict `start <= at < end`,
        // falling back to inclusive `start <= at <= end` for zero-width/EOF spans).
        let mut next = NO_NODE;
        let mut fallback = NO_NODE;
        let mut c = node.first_child;
        while c != NO_NODE {
            let Some(child) = cst.get(c) else { break };
            if child.has_span && child.kind != SyntaxKind::ATTR {
                if child.start <= at && at < child.end {
                    next = c;
                    break;
                }
                if fallback == NO_NODE && child.start <= at && at <= child.end {
                    fallback = c;
                }
            }
            c = child.next_sibling;
        }
        cur = if next != NO_NODE { next } else { fallback };
    }
}

/// Checks whether all direct `#[cfg(...)]` (and active `cfg_attr(..., cfg(...))`) attributes on
/// `node_idx` hold under `atoms`.
fn node_cfg_active(
    cst: &Cst<'_>,
    node_idx: u32,
    text: &str,
    edition: Edition,
    atoms: &[(&str, Option<&str>)],
    alloc: Alloc<'_>,
) -> bool {
    let Some(node) = cst.get(node_idx) else { return true };
    let mut c = node.first_child;
    let mut buf = AString::new_in(alloc);
    while c != NO_NODE {
        let Some(child) = cst.get(c) else { break };
        if child.kind == SyntaxKind::ATTR
            && let Some(raw) = cst.text_of(c, text)
            && !is_inner_attr(raw)
            && let Some(inside) = attr_brackets_inner(raw)
        {
            buf.clear();
            buf.push_str("#[");
            buf.push_str(inside);
            buf.push(']');
            if let Ok(stream) = blaze_expand::lex(buf.as_str(), edition, alloc) {
                let mut active = true;
                let mut eval_atom = |name: &str, value: Option<&str>| -> Result<bool, core::convert::Infallible> {
                    Ok(atom_holds(atoms, name, value))
                };
                match blaze_expand::cfg_attr_metas(&stream, alloc, &mut eval_atom) {
                    Ok(metas) => {
                        for range in metas.iter() {
                            if let (Ok(s), Ok(e)) = (usize::try_from(range.start), usize::try_from(range.end))
                                && !meta_cfg_holds(&stream, s..e, atoms, alloc)
                            {
                                active = false;
                                break;
                            }
                        }
                    }
                    Err(blaze_expand::CfgAttrError::NotCfgAttr) => {
                        let end = stream.tokens().len().saturating_sub(1);
                        if end > 2 && !meta_cfg_holds(&stream, 2..end, atoms, alloc) {
                            active = false;
                        }
                    }
                    Err(_) => {}
                }
                if !active {
                    return false;
                }
            }
        }
        c = child.next_sibling;
    }
    true
}

/// Evaluates a single attribute meta; if it is `cfg(<pred>)`, returns whether `<pred>` holds,
/// otherwise returns `true`.
fn meta_cfg_holds(
    stream: &TokenStream<'_>,
    range: core::ops::Range<usize>,
    atoms: &[(&str, Option<&str>)],
    alloc: Alloc<'_>,
) -> bool {
    let Some(inside) = meta_call_args(stream, range, "cfg") else {
        return true;
    };
    let mut eval_atom = |name: &str, value: Option<&str>| -> Result<bool, core::convert::Infallible> {
        Ok(atom_holds(atoms, name, value))
    };
    blaze_expand::cfg_predicate_at(stream, inside, alloc, &mut eval_atom).unwrap_or(true)
}

/// Applies all direct `ATTR` children of `node_idx` to `state`. When `inner_only` is true, only
/// `#![...]` attributes are applied (for list containers like `SOURCE_FILE` and `STMT_LIST`).
#[expect(clippy::too_many_arguments, reason = "CST attribute walker inputs")]
fn apply_node_attrs(
    cst: &Cst<'_>,
    node_idx: u32,
    inner_only: bool,
    text: &str,
    edition: Edition,
    atoms: &[(&str, Option<&str>)],
    state: &mut LintState,
    alloc: Alloc<'_>,
) {
    let Some(node) = cst.get(node_idx) else { return };
    let mut c = node.first_child;
    let mut buf = AString::new_in(alloc);
    while c != NO_NODE {
        let Some(child) = cst.get(c) else { break };
        if child.kind == SyntaxKind::ATTR
            && let Some(raw) = cst.text_of(c, text)
            && (!inner_only || is_inner_attr(raw))
            && let Some(inside) = attr_brackets_inner(raw)
        {
            buf.clear();
            buf.push_str("#[");
            buf.push_str(inside);
            buf.push(']');
            if let Ok(stream) = blaze_expand::lex(buf.as_str(), edition, alloc) {
                let mut eval_atom = |name: &str, value: Option<&str>| -> Result<bool, core::convert::Infallible> {
                    Ok(atom_holds(atoms, name, value))
                };
                match blaze_expand::cfg_attr_metas(&stream, alloc, &mut eval_atom) {
                    Ok(metas) => {
                        for range in metas.iter() {
                            if let (Ok(s), Ok(e)) = (usize::try_from(range.start), usize::try_from(range.end)) {
                                apply_meta_tokens(&stream, s..e, state);
                            }
                        }
                    }
                    Err(blaze_expand::CfgAttrError::NotCfgAttr) => {
                        let end = stream.tokens().len().saturating_sub(1);
                        if end > 2 {
                            apply_meta_tokens(&stream, 2..end, state);
                        }
                    }
                    Err(_) => {}
                }
            }
        }
        c = child.next_sibling;
    }
}

fn atom_holds(atoms: &[(&str, Option<&str>)], name: &str, value: Option<&str>) -> bool {
    if value.is_none() {
        if name == "true" {
            return true;
        }
        if name == "false" {
            return false;
        }
    }
    atoms.iter().any(|&(n, v)| n == name && v == value)
}

fn is_inner_attr(raw: &str) -> bool {
    raw.trim_start()
        .strip_prefix('#')
        .is_some_and(|rest| rest.trim_start().starts_with('!'))
}

fn attr_brackets_inner(raw: &str) -> Option<&str> {
    let rest = raw.trim().strip_prefix('#')?.trim_start();
    let rest = rest.strip_prefix('!').unwrap_or(rest).trim_start();
    rest.strip_prefix('[')?.strip_suffix(']')
}

/// If `range` in `stream` is `<expected>(<args>)`, returns the token range of `<args>`.
fn meta_call_args(
    stream: &TokenStream<'_>,
    mut range: core::ops::Range<usize>,
    expected: &str,
) -> Option<core::ops::Range<usize>> {
    let list = stream.tokens();
    while range.start < range.end
        && matches!(list.get(range.start).map(|t| t.kind), Some(TokenKind::Open(Delim::Invisible)))
        && stream.close_of(range.start) == Some(range.end.saturating_sub(1))
    {
        range = range.start + 1..range.end.saturating_sub(1);
    }
    let first = list.get(range.start)?;
    let TokenKind::Ident { .. } = first.kind else { return None };
    if stream.text_at(range.start) != expected {
        return None;
    }
    let open = range.start + 1;
    let TokenKind::Open(Delim::Paren) = list.get(open)?.kind else { return None };
    if stream.close_of(open) != Some(range.end.saturating_sub(1)) {
        return None;
    }
    Some(open + 1..range.end.saturating_sub(1))
}

/// Parses one attribute meta (`allow(..)`, `expect(..)`, `warn(..)`, `deny(..)`, `forbid(..)`)
/// and updates `state`.
fn apply_meta_tokens(stream: &TokenStream<'_>, mut range: core::ops::Range<usize>, state: &mut LintState) {
    let list = stream.tokens();
    while range.start < range.end
        && matches!(list.get(range.start).map(|t| t.kind), Some(TokenKind::Open(Delim::Invisible)))
        && stream.close_of(range.start) == Some(range.end.saturating_sub(1))
    {
        range = range.start + 1..range.end.saturating_sub(1);
    }
    let Some(first) = list.get(range.start) else { return };
    let TokenKind::Ident { .. } = first.kind else { return };
    let action = match stream.text_at(range.start) {
        "allow" | "expect" => RawLevel::Allow,
        "warn" => RawLevel::Warn,
        "deny" => RawLevel::Deny,
        "forbid" => RawLevel::Forbid,
        _ => return,
    };
    let open = range.start + 1;
    if !matches!(list.get(open).map(|t| t.kind), Some(TokenKind::Open(Delim::Paren))) {
        return;
    }
    if stream.close_of(open) != Some(range.end.saturating_sub(1)) {
        return;
    }
    let inside = open + 1..range.end.saturating_sub(1);
    let mut targets = 0u8;
    let mut touches_warnings = false;

    let mut pos = inside.start;
    while pos < inside.end {
        // Find end of this comma-separated item at paren depth 0.
        let item_start = pos;
        let mut depth = 0u32;
        while pos < inside.end {
            match list.get(pos).map(|t| t.kind) {
                Some(TokenKind::Open(d)) if d != Delim::Invisible => depth = depth.saturating_add(1),
                Some(TokenKind::Close(d)) if d != Delim::Invisible => depth = depth.saturating_sub(1),
                Some(TokenKind::Punct { char: ',', .. }) if depth == 0 => break,
                _ => {}
            }
            pos += 1;
        }
        let item_end = pos;
        if pos < inside.end {
            pos += 1; // skip ','
        }

        // Inspect tokens of this item: skip `reason = "..."` or nested groups, collect path idents.
        let mut segs: [&str; 3] = [""; 3];
        let mut seg_count = 0usize;
        let mut valid_path = true;
        let mut i = item_start;
        while i < item_end {
            let Some(tok) = list.get(i) else { break };
            match tok.kind {
                TokenKind::Open(Delim::Invisible) | TokenKind::Close(Delim::Invisible) => {}
                TokenKind::Ident { .. } => {
                    if let Some(slot) = segs.get_mut(seg_count) {
                        let raw = stream.text_at(i);
                        *slot = raw.strip_prefix("r#").unwrap_or(raw);
                        seg_count += 1;
                    } else {
                        valid_path = false;
                        break;
                    }
                }
                TokenKind::Punct { char: ':', .. } => {}
                _ => {
                    valid_path = false;
                    break;
                }
            }
            i += 1;
        }
        if !valid_path || seg_count == 0 {
            continue;
        }
        let name = match seg_count {
            1 => Some(segs[0]),
            2 if segs[0] == "rustc" => Some(segs[1]),
            _ => None,
        };
        if let Some(lint) = name {
            match lint {
                "deprecated_safe_2024" | "deprecated_safe" => targets |= BIT_DEPRECATED_SAFE,
                "unsafe_op_in_unsafe_fn" => targets |= BIT_UNSAFE_OP,
                "rust_2024_compatibility" => targets |= BIT_DEPRECATED_SAFE | BIT_UNSAFE_OP,
                "warnings" => touches_warnings = true,
                _ => {}
            }
        }
    }

    if targets != 0 || touches_warnings {
        state.apply(action, targets, touches_warnings);
    }
}

#[cfg(test)]
mod tests {
    use blaze_own::Alloc;
    use blaze_surface::Edition;

    use super::*;

    fn level_in_src(src: &str, needle: &str, edition: Edition, atoms: &[(&str, Option<&str>)], which: RustcLint) -> ResolvedLintLevel {
        let alloc = Alloc::global();
        let at = u32::try_from(src.find(needle).expect("needle in src")).expect("offset fits u32");
        let cst = Cst::parse(src, edition, alloc);
        let mut state = LintState::default_for(edition);
        apply_path_to_offset(&cst, at, src, edition, atoms, &mut state, alloc);
        state.resolve(which)
    }

    #[test]
    fn cst_resolves_braced_signature_macro_and_const_block_and_deep_nesting() {
        let dep = RustcLint::DeprecatedSafe2024 { in_unsafe_fn: false };
        let uop = RustcLint::UnsafeOpInUnsafeFn;

        // 1. Braced signature macro `fn f() -> Ty!{} { call(); }` preserves item `#[deny]`.
        let macro_sig = "#[deny(deprecated_safe_2024)]\npub fn f() -> Ty!{} { call_macro(); }\n";
        assert_eq!(level_in_src(macro_sig, "call_macro", Edition::E2021, &[], dep), ResolvedLintLevel::Error);

        // 2. Signature const block `fn f() -> Foo<{ 1 }> { call(); }` preserves item `#[deny]`.
        let const_sig = "#[deny(deprecated_safe_2024)]\npub fn f() -> Foo<{ 1 }> { call_const(); Foo }\n";
        assert_eq!(level_in_src(const_sig, "call_const", Edition::E2021, &[], dep), ResolvedLintLevel::Error);

        // 3. Deep `cfg_attr` predicate (>32 nested `all(...)`, zero depth cap).
        let mut deep_cfg = std::string::String::from("#![cfg_attr(");
        for _ in 0..48 {
            deep_cfg.push_str("all(");
        }
        deep_cfg.push_str("has_foo");
        for _ in 0..48 {
            deep_cfg.push(')');
        }
        deep_cfg.push_str(", deny(deprecated_safe_2024))]\npub fn f() { call_deep_cfg(); }\n");
        assert_eq!(level_in_src(&deep_cfg, "call_deep_cfg", Edition::E2021, &[("has_foo", None)], dep), ResolvedLintLevel::Error);
        assert_eq!(level_in_src(&deep_cfg, "call_deep_cfg", Edition::E2021, &[], dep), ResolvedLintLevel::Off);

        // 4. Deep block nesting (>128 nested blocks, zero depth cap).
        let mut deep_blk = std::string::String::from("#[deny(deprecated_safe_2024)]\npub fn f() {\n");
        for _ in 0..160 {
            deep_blk.push('{');
        }
        deep_blk.push_str(" call_deep_blk(); ");
        for _ in 0..160 {
            deep_blk.push('}');
        }
        deep_blk.push_str("\n}\n");
        assert_eq!(level_in_src(&deep_blk, "call_deep_blk", Edition::E2021, &[], dep), ResolvedLintLevel::Error);

        // 5. `forbid(warnings)` blocks inner specific `#[allow(..)]` for both warned and
        //    warn-by-default lints, while preserving `#[allow(..)]` written before `forbid`.
        let forbid_src = r#"
#![forbid(warnings)]
#![warn(deprecated_safe_2024)]
#[allow(deprecated_safe_2024)]
pub fn f() { call_forbid(); }
#[allow(unsafe_op_in_unsafe_fn)]
pub unsafe fn g() { call_uop(); }
"#;
        assert_eq!(level_in_src(forbid_src, "call_forbid", Edition::E2021, &[], dep), ResolvedLintLevel::Error);
        assert_eq!(level_in_src(forbid_src, "call_uop", Edition::E2024, &[], uop), ResolvedLintLevel::Error);

        let allow_before = "#![allow(unsafe_op_in_unsafe_fn)]\n#![forbid(warnings)]\npub unsafe fn g() { call_ok(); }\n";
        assert_eq!(level_in_src(allow_before, "call_ok", Edition::E2024, &[], uop), ResolvedLintLevel::Off);

        // 6. Edition 2021 `unsafe fn` with `deprecated_safe_2024`:
        //    - `#![deny(deprecated_safe_2024)]` alone -> Off (unsafe_op_in_unsafe_fn is Allow)
        //    - `#![deny(rust_2024_compatibility)]` -> Error (both enabled)
        let dep_u = RustcLint::DeprecatedSafe2024 { in_unsafe_fn: true };
        assert_eq!(level_in_src("#![deny(deprecated_safe_2024)]\npub unsafe fn f() { call(); }\n", "call", Edition::E2021, &[], dep_u), ResolvedLintLevel::Off);
        assert_eq!(level_in_src("#![deny(rust_2024_compatibility)]\npub unsafe fn f() { call(); }\n", "call", Edition::E2021, &[], dep_u), ResolvedLintLevel::Error);
        assert_eq!(level_in_src("#![deny(unsafe_op_in_unsafe_fn)]\npub unsafe fn f() { call(); }\n", "call", Edition::E2021, &[], uop), ResolvedLintLevel::Error);
        assert_eq!(level_in_src("pub unsafe fn f() { call(); }\n", "call", Edition::E2021, &[], uop), ResolvedLintLevel::Off);
    }
}
