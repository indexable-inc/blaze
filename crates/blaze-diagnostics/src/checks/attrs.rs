//! The errors of builtin attributes: what rustc's attribute parser gives as it lowers the AST
//! to HIR (`parse_attribute_list`: an attribute's safety, its arguments, what its parser says
//! of them, a second one where one is allowed, its target, a crate-level attribute elsewhere,
//! the attributes of a `where` predicate), what expansion's `check_attr` gives of the attributes
//! it leaves unparsed, and what `check_mod_attrs` gives of the parsed attributes.
//!
//! - `args`: an attribute's arguments (rustc's `ArgParser`) and the errors of reading them.
//! - `accept`: what a parser is given, and its errors of its input and its target.
//! - `cfg`: the errors of a `cfg` predicate a parser reads.
//! - `parsed`: what the parsers make of the attributes, as the checks after them read it.
//! - `parsers`: the parsers, one module per group of rustc's files.
//! - `check`: `check_mod_attrs`' errors.
//!
//! The lints of all of this are `lints::syntax::attrs`'; the feature gates of attributes
//! (E0658) are `checks::features::attributes`', which parsing calls where rustc gives them.
//!
//! Follows rustc's `compiler/rustc_attr_parsing/src/interface.rs` (`parse_attribute_list`),
//! `compiler/rustc_attr_parsing/src/safety.rs` (`check_attribute_safety`),
//! `compiler/rustc_attr_parsing/src/target_checking.rs` (`check_invalid_crate_level_attr_item`,
//! `first_line_of_next_item`, `check_invalid_where_predicate_attrs`),
//! `compiler/rustc_attr_parsing/src/attributes/mod.rs` (`Single`, `OnDuplicate`),
//! `compiler/rustc_attr_parsing/src/attributes/transparency.rs` and the texts of
//! `compiler/rustc_attr_parsing/src/session_diagnostics.rs` and
//! `compiler/rustc_attr_parsing/src/diagnostics.rs` (MIT or Apache-2.0).

mod accept;
pub(in crate::checks) mod args;
mod cfg;
mod check;
pub(in crate::checks) mod parsed;
mod parsers;

pub(in crate::checks) use self::accept::Session;

use blaze_items::Fact;
use blaze_mem::Frame;
use hir::InFile;
use smallvec::SmallVec;
use syntax::{TextRange, TextSize, view::AstNode as _};

use self::{
    accept::Accept,
    args::ArgParser,
    parsed::ParsedAttr,
    parsers::{Accepted, Finalize, GroupId, Module},
};
use crate::{
    checks::features,
    cx::Cx,
    lints::syntax::{
        attrs::{
            Attr, AttrKind, Owner, Target,
            table::{self, Kind, OnDuplicate, Safety, Targets},
        },
        source_fact,
    },
    lower::{Found, Phase},
    model::{Applicability, Diag},
};

/// The errors attribute parsing gives of `owners`' attributes: as the AST is lowered, and for
/// a macro call's attributes, at expansion; with those of expansion's `check_attr`. Each
/// owner's attributes are lowered once, here: what they are parsed to is returned for
/// [`checked`] (nothing for a macro call's, which `check_mod_attrs` does not see).
pub(crate) fn parsed<'f, 'db>(
    cx: &Cx<'f, 'db>,
    session: &Session,
    owners: &[Owner<'db>],
    found: &mut Vec<Found>,
) -> Lowered<'f> {
    let mut lowered = Lowered::with_capacity(cx.frame(), owners.len());
    // alloc: setup, one buffer reused for every owner, once per check
    let mut errors = Vec::new();
    for owner in owners {
        for attr in &owner.attrs {
            parsers::misc::validate(cx, attr, &mut errors);
        }
        push(cx, owner, Phase::Expand, &mut errors, found);
        let start = lowered.attrs.len();
        match owner.target {
            // Expansion parses a macro call's attributes one at a time (`check_attributes` of
            // `collect_bang`): none is checked against another.
            Target::MacroCall => {
                for &attr in &owner.attrs {
                    let alone = Owner {
                        node: owner.node,
                        target: owner.target,
                        // alloc: an owner of the one attribute, for a macro call's attributes only
                        attrs: vec![attr],
                        // clone: a pointer, copied with it
                        buffered_at: owner.buffered_at.clone(),
                    };
                    lower(cx, session, &alone, &mut errors, &mut lowered.attrs);
                }
            }
            _ => lower(cx, session, owner, &mut errors, &mut lowered.attrs),
        }
        let phase = match owner.target {
            Target::MacroCall => {
                lowered.attrs.truncate(start);
                Phase::Expand
            }
            _ => Phase::Lower,
        };
        lowered.close();
        push(cx, owner, phase, &mut errors, found);
    }
    lowered
}

/// The errors expansion gives of the attributes that invoke attribute macros, `invoking`
/// ([`attrs::invoking_attrs`](crate::lints::syntax::attrs::invoking_attrs)): read once each,
/// as the macro is called.
pub(crate) fn invoked<'db>(cx: &Cx<'_, 'db>, invoking: &[Owner<'db>], found: &mut Vec<Found>) {
    // alloc: setup, one buffer reused for every attribute macro call, once per check
    let mut errors = Vec::new();
    for owner in invoking {
        for attr in &owner.attrs {
            parsers::misc::validate(cx, attr, &mut errors);
        }
        push(cx, owner, Phase::Expand, &mut errors, found);
    }
}

/// The errors `check_mod_attrs` gives of `owners`' attributes, parsed by [`parsed`] to
/// `lowered`, then those of the stability pass, which reads what each definition inherits
/// from its parents.
pub(crate) fn checked<'db>(
    cx: &Cx<'_, 'db>,
    session: &Session,
    owners: &[Owner<'db>],
    lowered: &Lowered<'_>,
    found: &mut Vec<Found>,
) {
    // alloc: setup, one buffer reused for every owner, once per check
    let mut errors = Vec::new();
    for (at, owner) in owners.iter().enumerate() {
        let parsed = lowered.of(at);
        if parsed.is_empty() {
            continue;
        }
        check::check(cx, session, owner, parsed, &mut errors);
        push(cx, owner, Phase::Items, &mut errors, found);
    }
    let stability = parsers::stability::crate_checks(cx, session, owners, lowered);
    if stability.is_empty() {
        return;
    }
    let root = cx.module_key(cx.krate.root_module(cx.db));
    for (at, mut diag) in stability {
        source_fact(cx, &mut diag, at);
        diag.fact(Fact::Signature(root), "the `#![feature]`s of the crate root");
        found.push(Found::new(diag, Phase::Items));
    }
}

/// What every owner's attributes are parsed to, as one CSR list: owner `i`'s are
/// `attrs[ends[i - 1]..ends[i]]`, in the check's frame. Two allocations per crate, not one
/// per owner.
pub(crate) struct Lowered<'f> {
    attrs: Vec<ParsedAttr, &'f Frame>,
    ends: Vec<u32, &'f Frame>,
}

impl<'f> Lowered<'f> {
    /// `ends` is exact; `attrs` is allocated last, so it is the frame's top block and grows in
    /// place past its estimate (most owners have only doc comments and lint attributes, which
    /// lower to nothing).
    fn with_capacity(frame: &'f Frame, owners: usize) -> Lowered<'f> {
        let ends = Vec::with_capacity_in(owners, frame);
        let attrs = Vec::with_capacity_in(owners / 4, frame);
        Lowered { attrs, ends }
    }

    /// Ends the current owner's run.
    fn close(&mut self) {
        let end = u32::try_from(self.attrs.len()).expect("a crate has fewer than 2^32 parsed attributes");
        self.ends.push(end);
    }

    /// What owner `at` (its index in the owners) is parsed to; nothing for one not lowered.
    pub(crate) fn of(&self, at: usize) -> &[ParsedAttr] {
        let Some(&end) = self.ends.get(at) else { return &[] };
        let start = match at {
            0 => 0,
            _ => self.ends[at - 1],
        };
        &self.attrs[start as usize..end as usize]
    }
}

/// Pushes `owner`'s errors, draining `errors`. Besides its source, each reads the
/// `#![feature]`s of the crate root, which the gates, the parsers and their targets look at.
fn push(cx: &Cx<'_, '_>, owner: &Owner<'_>, phase: Phase, errors: &mut Vec<Diag>, found: &mut Vec<Found>) {
    if errors.is_empty() {
        return;
    }
    let root = cx.module_key(cx.krate.root_module(cx.db));
    for mut diag in errors.drain(..) {
        source_fact(cx, &mut diag, owner.at());
        diag.fact(Fact::Signature(root), "the `#![feature]`s of the crate root");
        let phase = match (diag.fatal, owner.target) {
            (true, Target::Crate) => Phase::CrateAttrs,
            (true, _) => Phase::Expand,
            (false, _) => phase,
        };
        found.push(Found::new(diag, phase));
    }
}

/// A parser group of a node, in the order its first attribute comes.
struct Group {
    module: Module,
    id: GroupId,
    /// Its first attribute; for a single parser, the one it uses once it has one.
    first: usize,
    kind: GroupKind,
}

enum GroupKind {
    /// A single parser, with the conversion it uses.
    Single(Option<parsed::Parsed>),
    /// A parser that keeps its attributes in its module's state.
    State,
}

/// rustc's `parse_attribute_list` of `owner`'s attributes: what they are, pushed to `parsed` in
/// the order rustc keeps them, with the errors in `out`.
fn lower<'db>(
    cx: &Cx<'_, 'db>,
    session: &Session,
    owner: &Owner<'db>,
    out: &mut Vec<Diag>,
    parsed: &mut Vec<ParsedAttr, &Frame>,
) {
    let mut node = parsers::Node::default();
    let mut groups: SmallVec<[Group; 4]> = SmallVec::new();
    // The doc comments, `#[doc = ".."]` and the attributes no parser takes, which rustc keeps in
    // place: `where` predicates may have none of them.
    let mut in_place: SmallVec<[usize; 8]> = SmallVec::new();
    for (index, attr) in owner.attrs.iter().enumerate() {
        let AttrKind::Meta { .. } = &attr.kind else {
            in_place.push(index);
            continue;
        };
        let Some(builtin) = with_path(attr, table::builtin) else {
            if attr.is_unsafe() {
                out.push(not_unsafe(cx, attr));
            }
            if owner.target == Target::Crate {
                out.extend(at_crate_level(cx, attr));
            }
            in_place.push(index);
            continue;
        };
        out.extend(safety(cx, attr, builtin.safety));
        if owner.target != Target::MacroCall {
            out.extend(features::attributes::gate(cx, &session.features, attr));
        }
        let Some(args) = args::parse(cx, attr, builtin.path, out) else { continue };
        if builtin.path == ["doc"]
            && let ArgParser::NameValue(nv) = &args
            && nv.value_as_str().is_some()
        {
            in_place.push(index);
            continue;
        }
        let Some(module) = parsers::module(builtin.path) else {
            panic!("no parser module takes the builtin attribute `{}`", attr.name())
        };
        let mut accept = Accept::new(cx, session, owner, index, builtin, out);
        match parsers::accept(&mut node, module, &mut accept, &args) {
            Accepted::Single(converted) => {
                let id = GroupId::Single(builtin.path);
                let at = match groups.iter().position(|it| it.id == id) {
                    Some(at) => at,
                    None => {
                        groups.push(Group { module, id, first: index, kind: GroupKind::Single(None) });
                        groups.len() - 1
                    }
                };
                let group = &mut groups[at];
                if let Some(converted) = converted {
                    let GroupKind::Single(used) = &mut group.kind else { unreachable!("{id:?} is a single parser") };
                    if used.is_some() {
                        if let Kind::Single(on_duplicate) = builtin.kind
                            && let Some(diag) = duplicate(cx, owner, builtin.path, on_duplicate, group.first, index)
                        {
                            out.push(diag);
                        }
                    } else {
                        *used = Some(converted);
                        group.first = index;
                    }
                }
            }
            Accepted::Group(name) => {
                let id = GroupId::Named(name);
                if !groups.iter().any(|it| it.id == id) {
                    groups.push(Group { module, id, first: index, kind: GroupKind::State });
                }
            }
        }
        if !matches!(builtin.targets, Targets::ManuallyChecked)
            && let Some(diag) = accept::target_error(cx, &session.krate, owner, attr, builtin.targets, "")
        {
            out.push(diag);
        }
    }
    if groups.is_empty() && owner.target != Target::WherePredicate {
        return;
    }
    let start = parsed.len();
    let mut deferred: SmallVec<[(Module, GroupId, usize); 4]> = SmallVec::new();
    let mut fx = Finalize::new(cx, session, owner, out);
    for group in &mut groups {
        match &mut group.kind {
            GroupKind::Single(used) => {
                if let Some(made) = used.take() {
                    parsed.push(ParsedAttr { parsed: made, attr: group.first });
                    deferred.push((group.module, group.id, group.first));
                }
            }
            GroupKind::State => {
                let GroupId::Named(name) = group.id else { unreachable!("{:?} keeps a state", group.id) };
                if let Some(made) = parsers::finalize(&mut node, group.module, name, &mut fx) {
                    parsed.push(ParsedAttr { parsed: made, attr: group.first });
                }
                deferred.push((group.module, group.id, group.first));
            }
        }
    }
    let parsed = &parsed[start..];
    for (module, id, first) in deferred {
        parsers::check(module, id, first, &mut fx, parsed);
    }
    drop(fx);
    if owner.target == Target::WherePredicate {
        let mut spans = in_place;
        if let Some(doc) = parsed.iter().find(|it| matches!(it.parsed, parsed::Parsed::Doc(_))) {
            spans.push(doc.attr);
        }
        if !spans.is_empty() {
            out.push(in_where(cx, owner, &spans));
        }
    }
}

/// `attr`'s path as rustc's tables key it, given to `f` with nothing allocated: `["inline"]`,
/// `["diagnostic", "on_unimplemented"]`, a doc comment's `["doc"]`. A segment that is not a
/// name (`<T>`) is given as `"<>"`, which no table has.
pub(in crate::checks) fn with_path<R>(attr: &Attr<'_>, f: impl FnOnce(&[&str]) -> R) -> R {
    let AttrKind::Meta { meta, .. } = attr.kind else { return f(&["doc"]) };
    let Some(path) = meta.path() else { return f(&[]) };
    // alloc: inline up to four segments
    let segments: SmallVec<[&str; 4]> = path.segments().map(|it| it.name_ref().map_or("<>", |it| it.text())).collect();
    f(&segments)
}

/// Whether `attr`'s path is the one segment `name`, read with nothing allocated (what
/// [`Attr::is`] answers by building the path).
pub(in crate::checks) fn is(attr: &Attr<'_>, name: &str) -> bool {
    match &attr.kind {
        AttrKind::Doc(_) => name == "doc",
        AttrKind::Meta { meta, .. } => {
            meta.path().and_then(|it| it.as_single_name_ref()).is_some_and(|it| it.text() == name)
        }
    }
}

/// What a second attribute of a single parser is, where the first is used: `UnusedMultiple`,
/// or `transparency.rs`'s own error (the lints are `lints::syntax::attrs`').
fn duplicate(
    cx: &Cx<'_, '_>,
    owner: &Owner<'_>,
    path: &[&str],
    on_duplicate: OnDuplicate,
    used: usize,
    unused: usize,
) -> Option<Diag> {
    let used_attr = &owner.attrs[used];
    let unused_attr = &owner.attrs[unused];
    let used_span = cx.span(used_attr.in_file(used_attr.range()));
    let unused_span = cx.span(unused_attr.in_file(unused_attr.range()));
    if path == ["rustc_macro_transparency"] {
        let mut diag = Diag::error(None, "multiple macro transparency attributes", used_span);
        diag.primary(unused_span);
        return Some(diag);
    }
    if on_duplicate != OnDuplicate::Error {
        return None;
    }
    // alloc: clone: the error's message and span
    let mut diag = Diag::error(None, format!("multiple `{}` attributes", path.join("..")), unused_span.clone());
    diag.suggestion(unused_span, "remove this attribute", "", Applicability::MachineApplicable);
    diag.span_note(used_span, "attribute also specified here");
    Some(diag)
}

/// The errors of rustc's `check_attribute_safety`: an unsafe attribute without `unsafe(..)`
/// from the edition it must have one, `unsafe(..)` around one that is not unsafe.
fn safety(cx: &Cx<'_, '_>, attr: &Attr<'_>, expected: Safety) -> Option<Diag> {
    match expected {
        Safety::Unsafe { unsafe_since, .. } if !attr.is_unsafe() => {
            if unsafe_since.is_some_and(|since| attr.file.edition(cx.db) < since) {
                return None;
            }
            let path = cx.span(attr.in_file(attr.path_range()));
            // clone: the error's span
            let mut diag = Diag::error(None, "unsafe attribute used without unsafe", path.clone());
            diag.span_label(path, "usage of unsafe attribute");
            if !hides_wrapping(cx, attr) {
                let inner = cx.span(attr.in_file(attr.inner_range()));
                diag.multipart_suggestion(
                    "wrap the attribute in `unsafe(...)`",
                    // alloc: the error's suggestion
                    vec![(inner.shrink_to_lo(), "unsafe(".to_owned()), (inner.shrink_to_hi(), ")".to_owned())],
                    Applicability::MachineApplicable,
                );
            }
            Some(diag)
        }
        Safety::Normal if attr.is_unsafe() => Some(not_unsafe(cx, attr)),
        _ => None,
    }
}

/// `InvalidAttrUnsafe`: `unsafe(..)` around an attribute that is not unsafe.
fn not_unsafe(cx: &Cx<'_, '_>, attr: &Attr<'_>) -> Diag {
    let AttrKind::Meta { unsafety: Some(unsafety), .. } = attr.kind else {
        unreachable!("`{}` is written `unsafe(..)`", attr.name())
    };
    let keyword = unsafety
        .unsafe_token()
        .map(|it| it.text_range())
        .unwrap_or_else(|| TextRange::at(unsafety.syntax().text_range().start(), TextSize::from(6)));
    let span = cx.span(attr.in_file(keyword));
    // alloc: clone: the error's message and span
    let mut diag = Diag::error(None, format!("`{}` is not an unsafe attribute", attr.name()), span.clone());
    diag.span_label(span, "this is not an unsafe attribute");
    diag.note("extraneous unsafe is not allowed in attributes");
    diag
}

/// Whether rustc leaves out the suggestion to wrap the attribute in `unsafe(..)`: it is from an
/// expansion and its text is a macro call or a whole attribute (`not_from_proc_macro`).
fn hides_wrapping(cx: &Cx<'_, '_>, attr: &Attr<'_>) -> bool {
    let AttrKind::Meta { written, .. } = attr.kind else { return false };
    if !crate::lints::syntax::from_expansion(cx, InFile::new(attr.file, written.syntax())) {
        return false;
    }
    let span = cx.span(attr.in_file(attr.inner_range()));
    let Some(snippet) = cx.snippet(&span) else { return false };
    // alloc: on the error path (whether an unsafe attribute's error suggests `unsafe(..)`)
    let snippet: String = snippet.chars().filter(|it| !it.is_whitespace()).collect();
    snippet.contains("!(") || snippet.starts_with("#[") && snippet.ends_with(']')
}

/// `InvalidAttrAtCrateLevel`: `#![derive]`, `#![test]`, `#![test_case]`,
/// `#![global_allocator]` or `#![bench]`, which name attribute macros that apply to items.
fn at_crate_level(cx: &Cx<'_, '_>, attr: &Attr<'_>) -> Option<Diag> {
    const NAMES: &[&str] = &["derive", "test", "test_case", "global_allocator", "bench"];
    let name = NAMES.iter().find(|it| is(attr, it))?;
    let range = attr.range();
    let span = cx.span(attr.in_file(range));
    // alloc: clone: the error's message and span
    let mut diag = Diag::error(None, format!("`{name}` attribute cannot be used at crate level"), span.clone());
    let pound = TextRange::new(range.start(), attr.inner_range().start());
    diag.suggestion_verbose(cx.span(attr.in_file(pound)), "perhaps you meant to use an outer attribute", "#[", Applicability::MachineApplicable);
    if let Some(file) = cx.file_of(&span)
        && let Some(next) = first_line_of_next_item(cx.file_text(file), range.end())
    {
        diag.span_label(cx.span_in_file(file, next), "the inner attribute doesn't annotate this item");
    }
    Some(diag)
}

/// rustc's `first_line_of_next_item`: the first line of what follows `end` that is not blank,
/// a comment or an attribute (up to its first `]`).
fn first_line_of_next_item(text: &str, end: TextSize) -> Option<TextRange> {
    let start = usize::from(end);
    let mut source = text.get(start..)?;
    loop {
        let first = source.chars().next()?;
        if first.is_whitespace() {
            let at = source.find(|c: char| !c.is_whitespace())?;
            source = &source[at..];
        } else if source.starts_with("//") {
            let at = source.find('\n')?;
            source = &source[at + 1..];
        } else if source.starts_with("/*") {
            let at = source.find("*/")?;
            source = &source[at + 2..];
        } else if first == '#' {
            let at = source.find(']')?;
            source = &source[at + 1..];
        } else {
            let lo = text.len() - source.len();
            let line = source.split('\n').next().map(str::trim_end)?;
            let lo = TextSize::try_from(lo).ok()?;
            return Some(TextRange::at(lo, TextSize::try_from(line.len()).ok()?));
        }
    }
}

/// `UnsupportedAttributesInWhere`, at the attributes `indices` of a `where` predicate.
fn in_where(cx: &Cx<'_, '_>, owner: &Owner<'_>, indices: &[usize]) -> Diag {
    let mut spans = indices.iter().map(|&it| {
        let attr = &owner.attrs[it];
        cx.span(attr.in_file(attr.range()))
    });
    let Some(first) = spans.next() else { unreachable!("a `where` predicate's attributes are given") };
    let mut diag = Diag::error(None, "most attributes are not supported in `where` clauses", first);
    for span in spans {
        diag.primary(span);
    }
    diag.help("only `#[cfg]` and `#[cfg_attr]` are supported");
    diag
}

#[cfg(test)]
mod tests {
    use syntax::TextSize;

    use super::first_line_of_next_item;

    #[test]
    fn the_next_item_is_after_comments_and_attributes() {
        let text = "#![derive(Copy)]\n// a\n/* b */\n#[inline]\nfn main() {}   \nfn g() {}\n";
        let end = TextSize::from(16);
        let found = first_line_of_next_item(text, end).map(|it| &text[it]);
        assert_eq!(found, Some("fn main() {}"));
        assert_eq!(first_line_of_next_item("#![test]", TextSize::from(8)), None);
    }
}
