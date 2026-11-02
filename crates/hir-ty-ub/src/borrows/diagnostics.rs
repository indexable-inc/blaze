//! What a report says: each tag's history of transitions, the error of a refused access in
//! Miri's words, and the tree printer (`miri_print_borrow_state`).
//!
//! Miri: `tree_borrows/diagnostics.rs`, and the helps of `TreeBorrowsUb` in
//! `src/tools/miri/src/diagnostics.rs`. Strings are only built once the run stops (or when the
//! tree is printed); the per-access records are plain data.

use std::{fmt, ops::Range};

use blaze_ids::radix;

use super::{
    ProtectorKind,
    perms::{PermTransition, Permission},
    tree::{LocationState, Tree},
    unimap::UniIndex,
};
use crate::ids::{AccessKind, AllocId, BorTag, SiteId, Span};

fn access_kind_str(kind: AccessKind) -> &'static str {
    match kind {
        AccessKind::Read => "read access",
        AccessKind::Write => "write access",
    }
}

/// Why an access happened: the program's, or one Tree Borrows inserts. Miri: `AccessCause`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum AccessCause {
    Explicit(AccessKind),
    Reborrow(AccessKind),
    Dealloc,
    FnExit(AccessKind),
}

impl fmt::Display for AccessCause {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AccessCause::Explicit(kind) => f.write_str(access_kind_str(*kind)),
            AccessCause::Reborrow(_) => f.write_str("reborrow"),
            AccessCause::Dealloc => f.write_str("deallocation"),
            // A protector's end can never cause UB: what it would invalidate was UB already.
            AccessCause::FnExit(_) => unreachable!("protector accesses can never be the source of UB"),
        }
    }
}

impl AccessCause {
    fn print_as_access(self, is_foreign: bool) -> String {
        let rel = if is_foreign { "foreign" } else { "child" };
        match self {
            AccessCause::Explicit(kind) => format!("{rel} {}", access_kind_str(kind)),
            AccessCause::Reborrow(kind) => {
                format!("reborrow (acting as a {rel} {})", access_kind_str(kind))
            }
            AccessCause::Dealloc => format!("deallocation (acting as a {rel} write access)"),
            AccessCause::FnExit(kind) => {
                format!("protector release (acting as a {rel} {})", access_kind_str(kind))
            }
        }
    }
}

/// One transition of a tag. Miri: `diagnostics.rs`, `Event`.
#[derive(Clone, Debug)]
pub(super) struct TagEvent {
    pub(super) transition: PermTransition,
    pub(super) access_cause: AccessCause,
    /// Whether the access was foreign to the tag.
    pub(super) is_foreign: bool,
    /// The access's range; `None` for a protector's end (every accessed location).
    pub(super) access_range: Option<Span>,
    /// The part of `access_range` this transition happened on. Depends on how the range map
    /// split, so never shown; only used to pick the events that touch the error's offset.
    pub(super) transition_range: Range<u64>,
    pub(super) span: SiteId,
}

/// The access being performed, at one range of locations. Miri: `DiagnosticInfo`.
#[derive(Clone, Debug)]
pub(super) struct DiagnosticInfo {
    pub(super) alloc_id: AllocId,
    pub(super) span: SiteId,
    pub(super) transition_range: Range<u64>,
    pub(super) access_range: Option<Span>,
    pub(super) access_cause: AccessCause,
}

impl DiagnosticInfo {
    pub(super) fn create_event(&self, transition: PermTransition, is_foreign: bool) -> TagEvent {
        TagEvent {
            transition,
            is_foreign,
            access_cause: self.access_cause,
            access_range: self.access_range,
            transition_range: self.transition_range.clone(),
            span: self.span,
        }
    }
}

/// Every transition of a tag, filtered for a report. Miri: `History`.
#[derive(Clone, Debug)]
pub(super) struct History {
    tag: BorTag,
    created: (SiteId, Permission),
    events: Vec<TagEvent>,
}

impl History {
    pub(super) fn push(&mut self, event: TagEvent) {
        self.events.push(event);
    }

    /// Only the creation. Miri: `History::forget`.
    fn forget(&self) -> History {
        History { events: Vec::new(), created: self.created, tag: self.tag }
    }

    /// The events at `error_offset` that matter for `error_kind`. Miri:
    /// `History::extract_relevant`.
    fn extract_relevant(&self, error_offset: u64, error_kind: TransitionError) -> History {
        History {
            events: self
                .events
                .iter()
                .filter(|e| e.transition_range.contains(&error_offset))
                .filter(|e| e.transition.is_relevant(error_kind))
                .cloned()
                .collect(),
            created: self.created,
            tag: self.tag,
        }
    }

    #[cfg(test)]
    pub(super) fn events(&self) -> &[TagEvent] {
        &self.events
    }
}

/// One line of an aliasing error's history: the tag it is about, where (`None` for a note on
/// the line before) and what, in Miri's words ("the accessed tag <3> was created here, in the
/// initial state Reserved").
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Event {
    pub tag: BorTag,
    pub site: Option<SiteId>,
    pub note: String,
}

/// Miri: `HistoryData::extend`.
fn extend_history(out: &mut Vec<Event>, history: History, tag_name: &str, show_initial_state: bool) {
    let History { tag, created, events } = history;
    let this = format!("the {tag_name} tag {tag:?}");
    let note = if show_initial_state {
        format!("{this} was created here, in the initial state {}", created.1)
    } else {
        format!("{this} was created here")
    };
    out.push(Event { tag, site: Some(created.0), note });
    for event in &events {
        let access = event.access_cause.print_as_access(event.is_foreign);
        let access_range_text = match event.access_range {
            Some(r) => format!("at offsets [{:#x}..{:#x}]", r.start, r.end()),
            None => "on every location previously accessed by this tag".to_owned(),
        };
        out.push(Event {
            tag,
            site: Some(event.span),
            note: format!(
                "{this} later transitioned to {} due to a {access} {access_range_text}",
                event.transition.endpoint()
            ),
        });
        out.push(Event {
            tag,
            site: None,
            note: format!("this transition corresponds to {}", event.transition.summary()),
        });
    }
}

/// What a report needs to know about a tag. Miri: `NodeDebugInfo`.
#[derive(Clone, Debug)]
pub(super) struct NodeDebugInfo {
    pub(super) tag: BorTag,
    /// Names given to it (`miri_pointer_name`), comma-separated.
    pub(super) name: Option<String>,
    pub(super) history: History,
}

impl NodeDebugInfo {
    pub(super) fn new(tag: BorTag, initial: Permission, span: SiteId) -> NodeDebugInfo {
        NodeDebugInfo {
            tag,
            name: None,
            history: History { tag, created: (span, initial), events: Vec::new() },
        }
    }

    pub(super) fn vacant() -> NodeDebugInfo {
        NodeDebugInfo::new(BorTag::new(0), Permission::new_disabled(), SiteId::START)
    }

    pub(super) fn add_name(&mut self, name: &str) {
        match &mut self.name {
            Some(prev_name) => {
                prev_name.push_str(", ");
                prev_name.push_str(name);
            }
            None => self.name = Some(name.to_owned()),
        }
    }
}

impl fmt::Display for NodeDebugInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.name {
            Some(name) => write!(f, "{:?} ({name})", self.tag),
            None => write!(f, "{:?}", self.tag),
        }
    }
}

/// Why an access is refused. Miri: `TransitionError`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum TransitionError {
    /// A child access to a tag (or a parent) whose permission forbids it.
    ChildAccessForbidden(Permission),
    /// A foreign access would disable a protected accessed tag in this state.
    ProtectedDisabled(Permission),
    /// A deallocation of memory a strongly protected tag accessed.
    ProtectedDealloc,
}

/// Which rule refused an access, a retag or a free.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum AliasErrorKind {
    /// The accessed tag, or a parent of it, has a permission that forbids the access.
    ChildAccessForbidden,
    /// The access would disable a protected tag.
    ProtectedDisabled,
    /// The deallocation would free memory a strongly protected tag uses.
    ProtectedDealloc,
    /// A wildcard pointer, and no exposed tag may perform the access.
    NoExposedTag,
}

/// Tree Borrows refused an access, a retag or a deallocation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AliasError {
    kind: AliasErrorKind,
    alloc: AllocId,
    offset: u64,
    title: String,
    details: Vec<String>,
    history: Vec<Event>,
}

const HELP_EXPERIMENTAL: &str = "this indicates a potential bug in the program: it performed an invalid operation, but the Tree Borrows rules it violated are still experimental";
const HELP_SEE: &str = "see https://github.com/rust-lang/unsafe-code-guidelines/blob/master/wip/tree-borrows.md for further information";

impl AliasError {
    pub fn kind(&self) -> AliasErrorKind {
        self.kind
    }

    pub fn alloc(&self) -> AllocId {
        self.alloc
    }

    /// The first byte of the access the error is at.
    pub fn offset(&self) -> u64 {
        self.offset
    }

    /// "write access through <2> at alloc1[0x0] is forbidden".
    pub fn title(&self) -> &str {
        &self.title
    }

    /// The helps that say why ("the accessed tag <2> has state Frozen which forbids this
    /// child write access").
    pub fn details(&self) -> &[String] {
        &self.details
    }

    /// Where each tag involved was created and how it changed, for the report.
    pub fn history(&self) -> &[Event] {
        &self.history
    }
}

impl fmt::Display for AliasError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.title)?;
        for help in [HELP_EXPERIMENTAL, HELP_SEE].into_iter().chain(self.details.iter().map(String::as_str)) {
            write!(f, "\n  = help: {help}")?;
        }
        Ok(())
    }
}

/// The error of a refused transition. `accessed` is the tag the access went through (`None`
/// for a wildcard access); `conflicting` the one that refused it. Miri: `TbError::build`.
pub(super) fn build_error(
    error_kind: TransitionError,
    access_info: &DiagnosticInfo,
    conflicting: &NodeDebugInfo,
    accessed: Option<&NodeDebugInfo>,
) -> AliasError {
    let cause = access_info.access_cause;
    let error_offset = access_info.transition_range.start;
    let accessed_str = accessed.map_or_else(|| "<wildcard>".to_owned(), |v| v.to_string());
    // A wildcard access can't have come through the conflicting tag: the exposure cache
    // already rules out every tag through which the access would be UB.
    let accessed_is_conflicting = accessed.map(|a| a.tag) == Some(conflicting.tag);
    let title = format!(
        "{cause} through {accessed_str} at {:?}[{error_offset:#x}] is forbidden",
        access_info.alloc_id
    );
    let (kind, details, conflicting_tag_name) = match error_kind {
        TransitionError::ChildAccessForbidden(perm) => {
            let conflicting_tag_name = if accessed_is_conflicting { "accessed" } else { "conflicting" };
            let mut details = Vec::new();
            if !accessed_is_conflicting {
                details.push(format!(
                    "the accessed tag {accessed_str} is a child of the conflicting tag {conflicting}"
                ));
            }
            let access = cause.print_as_access(false);
            details.push(format!(
                "the {conflicting_tag_name} tag {conflicting} has state {perm} which forbids this {access}"
            ));
            (AliasErrorKind::ChildAccessForbidden, details, conflicting_tag_name)
        }
        TransitionError::ProtectedDisabled(before_disabled) => {
            let conflicting_tag_name = "protected";
            let access = cause.print_as_access(true);
            let details = vec![
                format!(
                    "the accessed tag {accessed_str} is foreign to the {conflicting_tag_name} tag {conflicting} (i.e., it is not a child)"
                ),
                format!(
                    "this {access} would cause the {conflicting_tag_name} tag {conflicting} (currently {before_disabled}) to become Disabled"
                ),
                "protected tags must never be Disabled".to_owned(),
            ];
            (AliasErrorKind::ProtectedDisabled, details, conflicting_tag_name)
        }
        TransitionError::ProtectedDealloc => {
            let conflicting_tag_name = "strongly protected";
            let details = vec![
                format!(
                    "the allocation of the accessed tag {accessed_str} also contains the {conflicting_tag_name} tag {conflicting}"
                ),
                format!("the {conflicting_tag_name} tag {conflicting} disallows deallocations"),
            ];
            (AliasErrorKind::ProtectedDealloc, details, conflicting_tag_name)
        }
    };
    let mut history = Vec::new();
    if let Some(accessed_info) = accessed
        && !accessed_is_conflicting
    {
        extend_history(&mut history, accessed_info.history.forget(), "accessed", false);
    }
    extend_history(
        &mut history,
        conflicting.history.extract_relevant(error_offset, error_kind),
        conflicting_tag_name,
        true,
    );
    AliasError { kind, alloc: access_info.alloc_id, offset: error_offset, title, details, history }
}

/// A wildcard access no exposed tag may perform. Miri: `no_valid_exposed_references_error`.
pub(super) fn no_valid_exposed_references_error(info: &DiagnosticInfo) -> AliasError {
    let offset = info.transition_range.start;
    AliasError {
        kind: AliasErrorKind::NoExposedTag,
        alloc: info.alloc_id,
        offset,
        title: format!(
            "{} through <wildcard> at {:?}[{offset:#x}] is forbidden",
            info.access_cause, info.alloc_id
        ),
        details: vec!["there are no exposed tags which may perform this access here".to_owned()],
        history: Vec::new(),
    }
}

impl Tree {
    /// The `nth_parent` of `tag` (0: itself). Miri: `Tree::nth_parent`.
    fn nth_parent(&self, tag: BorTag, nth_parent: u8) -> Option<BorTag> {
        let mut idx = self.index_of(tag);
        for _ in 0..nth_parent {
            idx = self.nodes.parent(idx)?;
        }
        Some(self.nodes.tag(idx))
    }

    /// Names a tag for reports and the tree printer. Miri: `Tree::give_pointer_debug_name`.
    pub(super) fn give_pointer_debug_name(&mut self, tag: BorTag, nth_parent: u8, name: &str) -> bool {
        let Some(tag) = self.nth_parent(tag, nth_parent) else { return false };
        let idx = self.index_of(tag);
        self.nodes.debug[idx.index()].add_name(name);
        true
    }

    /// The tree as Miri's `miri_print_borrow_state` prints it. Miri: `Tree::print_tree`.
    pub(super) fn print_tree(&self, show_unnamed: bool) -> String {
        let mut out = String::new();
        let ranges = self.locations.iter_all().map(|(range, _)| range).collect::<Vec<_>>();
        let main_tree = DisplayRepr::from(self, self.roots[0], show_unnamed);
        let wildcard_subtrees = self.roots[1..]
            .iter()
            .filter_map(|&root| DisplayRepr::from(self, root, show_unnamed))
            .collect::<Vec<_>>();
        if main_tree.is_none() && wildcard_subtrees.is_empty() {
            out.push_str(
                "This allocation does not contain named tags. Use `miri_print_borrow_state(_, true)` to also print unnamed tags.\n",
            );
        }
        DisplayRepr::print(&main_tree, &wildcard_subtrees, &DEFAULT_FORMATTER, ranges, !show_unnamed, &mut out);
        out
    }
}

/// The tree resolved for printing. Miri: `DisplayRepr`.
struct DisplayRepr {
    tag: BorTag,
    name: Option<String>,
    exposed: bool,
    protector: Option<ProtectorKind>,
    rperm: Vec<Option<LocationState>>,
    children: Vec<DisplayRepr>,
}

struct DisplayFmt {
    top: char,
    bot: char,
    warning_text: &'static str,
    open: &'static str,
    sep: &'static str,
    close: &'static str,
    uninit: &'static str,
    range_sep: &'static str,
    join_middle: &'static str,
    join_last: &'static str,
    join_haschild: &'static str,
    join_default: &'static str,
    indent_middle: &'static str,
    indent_last: &'static str,
    wildcard_root: &'static str,
    accessed_yes: &'static str,
    accessed_no: &'static str,
    accessed_meh: &'static str,
}

const DEFAULT_FORMATTER: DisplayFmt = DisplayFmt {
    top: '─',
    bot: '─',
    warning_text: "Warning: this tree is indicative only. Some tags may have been hidden.",
    open: "|",
    sep: "|",
    close: "|",
    uninit: "----",
    range_sep: "..",
    join_middle: "├",
    join_last: "└",
    indent_middle: "│ ",
    indent_last: "  ",
    join_haschild: "┬",
    join_default: "─",
    wildcard_root: "*",
    accessed_yes: " ",
    accessed_no: "?",
    accessed_meh: "-",
};

impl DisplayFmt {
    fn print_perm(&self, perm: Option<LocationState>) -> String {
        match perm {
            Some(perm) => format!(
                "{}{}",
                if perm.accessed() { self.accessed_yes } else { self.accessed_no },
                perm.permission().short_name()
            ),
            None => format!("{}{}", self.accessed_meh, self.uninit),
        }
    }

    fn print_tag(&self, tag: BorTag, name: &Option<String>) -> String {
        match name {
            Some(name) => format!("<{}={name}>", tag.get()),
            None => format!("<{}>", tag.get()),
        }
    }

    fn print_protector(&self, protector: Option<ProtectorKind>) -> &'static str {
        match protector {
            Some(ProtectorKind::WeakProtector) => " Weakly protected",
            Some(ProtectorKind::StrongProtector) => " Strongly protected",
            None => "",
        }
    }

    fn print_exposed(&self, exposed: bool) -> &'static str {
        if exposed { " (exposed)" } else { "" }
    }
}

fn char_repeat(c: char, n: usize) -> String {
    std::iter::repeat_n(c, n).collect()
}

impl DisplayRepr {
    fn from(tree: &Tree, root: UniIndex, show_unnamed: bool) -> Option<DisplayRepr> {
        let mut v = Vec::new();
        extraction_aux(tree, root, show_unnamed, &mut v);
        let root = v.pop()?;
        assert!(v.is_empty());
        return Some(root);

        fn extraction_aux(tree: &Tree, idx: UniIndex, show_unnamed: bool, acc: &mut Vec<DisplayRepr>) {
            let name = tree.nodes.debug[idx.index()].name.clone();
            let mut children_sorted = tree.nodes.children(idx).to_vec();
            radix::sort_by_key(&mut children_sorted, |&idx| tree.nodes.tag(idx).get());
            if !show_unnamed && name.is_none() {
                for child_idx in children_sorted {
                    extraction_aux(tree, child_idx, show_unnamed, acc);
                }
            } else {
                let rperm = tree.locations.iter_all().map(|(_, loc)| loc.perms.get(idx).copied()).collect();
                let mut children = Vec::new();
                for child_idx in children_sorted {
                    extraction_aux(tree, child_idx, show_unnamed, &mut children);
                }
                acc.push(DisplayRepr {
                    tag: tree.nodes.tag(idx),
                    name,
                    exposed: tree.nodes.is_exposed(idx),
                    protector: tree.nodes.protector[idx.index()],
                    rperm,
                    children,
                });
            }
        }
    }

    fn print(
        main_root: &Option<DisplayRepr>,
        wildcard_subtrees: &[DisplayRepr],
        fmt: &DisplayFmt,
        ranges: Vec<Range<u64>>,
        print_warning: bool,
        out: &mut String,
    ) {
        let mut block = Vec::new();
        let mut indent = String::from("    ");
        // The header `0.. 1.. 2.. 3`, aligned with the `|` of the body.
        let mut header_top = String::from("0..");
        let mut padding = Vec::new();
        for (i, range) in ranges.iter().enumerate() {
            if i > 0 {
                header_top.push_str(fmt.range_sep);
            }
            let s = range.end.to_string();
            let l = s.chars().count() + fmt.range_sep.chars().count();
            let target_len = fmt.uninit.chars().count() + fmt.accessed_yes.chars().count() + 1;
            let tot_len = target_len.max(l);
            let header_top_pad_len = target_len.saturating_sub(l);
            let body_pad_len = tot_len.saturating_sub(target_len);
            header_top.push_str(&char_repeat(' ', header_top_pad_len));
            header_top.push_str(&s);
            padding.push(body_pad_len);
        }
        block.push(header_top);
        if let Some(root) = main_root {
            print_aux(root, &padding, fmt, &mut indent, true, false, &mut block);
        }
        for tree in wildcard_subtrees {
            let mut gap_line = String::from(fmt.open);
            for (i, &pad) in padding.iter().enumerate() {
                if i > 0 {
                    gap_line.push_str(fmt.sep);
                }
                gap_line.push_str(&char_repeat(' ', pad));
                gap_line.push_str("     ");
            }
            gap_line.push_str(fmt.close);
            block.push(gap_line);
            print_aux(tree, &padding, fmt, &mut indent, true, true, &mut block);
        }
        let block_width = block.iter().map(|s| s.chars().count()).max().unwrap_or(0);
        let max_width = if print_warning {
            block_width.max(fmt.warning_text.chars().count())
        } else {
            block_width
        };
        out.push_str(&char_repeat(fmt.top, max_width));
        out.push('\n');
        if print_warning {
            out.push_str(fmt.warning_text);
            out.push('\n');
        }
        for line in block {
            out.push_str(&line);
            out.push('\n');
        }
        out.push_str(&char_repeat(fmt.bot, max_width));
        out.push('\n');

        fn print_aux(
            tree: &DisplayRepr,
            padding: &[usize],
            fmt: &DisplayFmt,
            indent: &mut String,
            is_last_child: bool,
            is_wildcard_root: bool,
            acc: &mut Vec<String>,
        ) {
            let mut line = String::from(fmt.open);
            for (i, (perm, &pad)) in tree.rperm.iter().zip(padding.iter()).enumerate() {
                if i > 0 {
                    line.push_str(fmt.sep);
                }
                line.push_str(&char_repeat(' ', pad));
                line.push_str(&fmt.print_perm(*perm));
            }
            line.push_str(fmt.close);
            line.push_str(indent);
            line.push_str(if is_wildcard_root {
                fmt.wildcard_root
            } else if is_last_child {
                fmt.join_last
            } else {
                fmt.join_middle
            });
            line.push_str(fmt.join_default);
            line.push_str(if tree.children.is_empty() { fmt.join_default } else { fmt.join_haschild });
            line.push_str(fmt.join_default);
            line.push_str(fmt.join_default);
            line.push_str(&fmt.print_tag(tree.tag, &tree.name));
            line.push_str(fmt.print_protector(tree.protector));
            line.push_str(fmt.print_exposed(tree.exposed));
            acc.push(line);
            let nb_children = tree.children.len();
            for (i, child) in tree.children.iter().enumerate() {
                indent.push_str(if is_last_child { fmt.indent_last } else { fmt.indent_middle });
                print_aux(child, padding, fmt, indent, i + 1 == nb_children, false, acc);
                // Miri's `DisplayIndent::decrement` pops as many chars as `indent_last` has
                // bytes; both indents are two chars.
                for _ in 0..fmt.indent_last.len() {
                    indent.pop();
                }
            }
        }
    }
}
