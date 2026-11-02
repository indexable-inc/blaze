//! The lints of `#[doc]` and doc comments: `invalid_doc_attributes` and `unused_attributes`
//! from parsing `#[doc(..)]`, `invalid_doc_attributes` from `check_mod_attrs`' checks of what
//! it parsed, and `unused_doc_comments` ([`comments`]).
//!
//! rustc parses all the `#[doc]` attributes of a node with one `DocParser`, so a duplicate
//! alias or `test(no_crate_inject)` is found across them, and checks what they give when one
//! of them had a list item. A macro call's attributes are parsed one at a time at expansion,
//! never checked, and their lints are buffered at the node around the call.
//!
//! `#[doc = ".."]` is a doc comment to rustc's attribute parser and gives no lint here; the
//! `cfg` of `#[doc(cfg(..))]` is a `cfg` predicate, linted with the others.
//!
//! Follows rustc's `compiler/rustc_attr_parsing/src/attributes/doc.rs` (`DocParser`),
//! `compiler/rustc_attr_parsing/src/diagnostics.rs`, `compiler/rustc_attr_parsing/src/
//! session_diagnostics.rs` (`UnusedDuplicate`), `compiler/rustc_attr_parsing/src/template.rs`
//! (`AttributeTemplate::suggestions`), `compiler/rustc_passes/src/check_attr.rs`
//! (`check_doc_attrs`, `check_doc_inline`, `check_doc_masked`), `compiler/rustc_passes/src/
//! diagnostics.rs`, `compiler/rustc_middle/src/hir/map.rs` (`hir_span`) and `compiler/
//! rustc_expand/src/expand.rs` (`InvocationCollector::check_attributes`) (MIT or Apache-2.0).

pub(super) mod comments;
mod meta;

use std::borrow::Cow;

use blaze_hash::HashSet;
use hir::InFile;
use syntax::{
    SyntaxKind, SyntaxNodePtr, TextRange, TextSize,
    view::{self as ast, AstNode},
};

use self::meta::{Item, List, Lit, MetaArgs, MetaItem};
use super::super::{Emitted, Pass, Stage, Walk, source_fact};
use super::{Args, Attr, Owner, Target, checked, parsed};
use crate::{
    cx::{Cx, def_range_view, item_range_view, signature_range_view},
    lower::Phase,
    model::{Applicability, Diag, SuggestionStyle},
};
use stdx::list::Join;

const INVALID: &str = "invalid_doc_attributes";

const PHASED_OUT: &str =
    "this was previously accepted by the compiler but is being phased out; it will become a hard error in a future release!";

/// The list forms of `DocParser`'s template, in its order.
const FORMS: [&str; 25] = [
    "alias",
    "attribute",
    "hidden",
    "html_favicon_url",
    "html_logo_url",
    "html_no_source",
    "html_playground_url",
    "html_root_url",
    "issue_tracker_base_url",
    "inline",
    "no_inline",
    "masked",
    "cfg",
    "notable_trait",
    "keyword",
    "fake_variadic",
    "search_unbox",
    "rust_logo",
    "auto_cfg",
    "test",
    "spotlight",
    "include",
    "no_default_passes",
    "passes",
    "plugins",
];

/// The lints of the `#[doc]` attributes and doc comments of `owners`.
pub(crate) fn lints(cx: &Cx<'_, '_>, _walk: &Walk<'_>, owners: &[Owner<'_>]) -> Vec<Emitted> {
    // alloc: empty, allocates nothing until a lint
    let mut emitted = Vec::new();
    let mut modules = comments::Modules::default();
    for owner in owners {
        if owner.target == Target::MacroCall {
            macro_call(cx, owner, &mut modules, &mut emitted);
            continue;
        }
        let mut parser = Parser { cx, owner, buffered_at: None, state: State::default(), emitted: &mut emitted };
        for attr in &owner.attrs {
            parser.attr(attr);
        }
        let state = parser.state;
        if state.items > 0 {
            check(cx, owner, &state, &mut emitted);
        }
        comments::unused(cx, owner, &mut emitted);
    }
    emitted
}

/// rustc's `InvocationCollector::check_attributes`, for what it says of `#[doc]`: each
/// attribute of the macro call parsed alone, and `unused_doc_comments` for its doc strings,
/// a run of them as one, from the call's first attribute on.
fn macro_call(cx: &Cx<'_, '_>, owner: &Owner<'_>, modules: &mut comments::Modules, emitted: &mut Vec<Emitted>) {
    let at = comments::lint_node(cx, owner.node, modules);
    let mut span: Option<InFile<TextRange>> = None;
    for (i, attr) in owner.attrs.iter().enumerate() {
        let mut parser =
            // clone: a pointer, copied; once per attribute of a macro call
            Parser { cx, owner, buffered_at: Some(at.clone()), state: State::default(), emitted: &mut *emitted };
        parser.attr(attr);
        let range = attr.in_file(attr.range());
        let current = match span {
            Some(it) if it.file_id == range.file_id => it.map(|it| it.cover(range.value)),
            _ => range,
        };
        span = Some(current);
        if owner.attrs.get(i + 1).is_some_and(is_doc_str) {
            continue;
        }
        if is_doc_str(attr) {
            let mut diag = comments::macro_call(cx, current, range);
            // clone: a pointer, copied for a lint emitted
            source_fact(cx, &mut diag, at.clone());
            // clone: as above
            emitted.push(Emitted::new(diag, Phase::Resolve, Pass::Buffered, at.clone()).buffered(Stage::Collected));
        }
    }
}

/// rustc's `Attribute::doc_str`: a doc comment, or `#[doc = "string"]`.
fn is_doc_str(attr: &Attr<'_>) -> bool {
    attr.is_doc_comment()
        || (is_doc(attr)
            && matches!(attr.args(), Args::NameValue(Some(ast::Expr::Literal(it))) if (Lit { token: it.token() }).str().is_some()))
}

/// Whether `attr` is `#[doc..]`, not a doc comment.
fn is_doc(attr: &Attr<'_>) -> bool {
    let Some(path) = attr.meta().and_then(|it| it.path()) else { return false };
    path.qualifier().is_none()
        && path.segment().is_some_and(|it| {
            it.coloncolon_token().is_none() && it.name_ref().is_some_and(|it| it.text() == "doc")
        })
}

/// What `DocParser` keeps of a node's `#[doc]` attributes that its lints read.
#[derive(Default)]
struct State {
    /// `alias`es, each where it was last given.
    aliases: Vec<(String, InFile<TextRange>)>,
    no_crate_inject: Option<InFile<TextRange>>,
    /// `inline` (`true`) and `no_inline`, in order.
    inline: Vec<(bool, InFile<TextRange>)>,
    /// The last `masked`.
    masked: Option<InFile<TextRange>>,
    /// How many meta items the lists had: rustc's `nb_doc_attrs`.
    items: usize,
}

/// rustc's `DocParser` over a node's `#[doc]` attributes.
struct Parser<'p, 'a, 'b> {
    cx: &'p Cx<'a, 'b>,
    owner: &'p Owner<'p>,
    /// For a macro call's attributes, the node expansion buffers their lints at.
    buffered_at: Option<InFile<SyntaxNodePtr>>,
    state: State,
    emitted: &'p mut Vec<Emitted>,
}

impl Parser<'_, '_, '_> {
    fn diag(&self, lint: &str, message: impl Into<String>, at: InFile<TextRange>) -> Diag {
        Diag::lint(lint, message, self.cx.span(at))
    }

    fn emit(&mut self, mut diag: Diag) {
        match &self.buffered_at {
            Some(at) => {
                // clone: a pointer, copied for a lint emitted
                source_fact(self.cx, &mut diag, at.clone());
                // clone: as above
                self.emitted.push(Emitted::new(diag, Phase::Resolve, Pass::Buffered, at.clone()).buffered(Stage::Collected));
            }
            None => {
                source_fact(self.cx, &mut diag, self.owner.at());
                self.emitted.push(parsed(diag, self.owner));
            }
        }
    }

    /// A lint with rustc's "phased out" warning: `expected_name_value`, `expected_no_args`,
    /// `expected_string_literal`.
    fn phased_out(&mut self, message: &str, at: InFile<TextRange>) {
        let mut diag = self.diag(INVALID, message, at);
        diag.warn(PHASED_OUT);
        self.emit(diag);
    }

    fn expected_name_value(&mut self, at: InFile<TextRange>) {
        self.phased_out("expected this to be of the form `... = \"...\"`", at);
    }

    fn expected_no_args(&mut self, at: InFile<TextRange>) {
        self.phased_out("didn't expect any arguments here", at);
    }

    fn malformed(&mut self, at: InFile<TextRange>) {
        self.phased_out("malformed `doc` attribute input", at);
    }

    /// `as_no_args`: whether `meta` has no arguments, else the lint at them.
    fn no_args(&mut self, attr: &Attr<'_>, meta: &MetaItem<'_>) -> bool {
        match meta.args.range() {
            None => true,
            Some(range) => {
                self.expected_no_args(attr.in_file(range));
                false
            }
        }
    }

    /// `check_attr_crate_level`: whether it is on the crate, else the lint at `path`.
    fn crate_level(&mut self, path: InFile<TextRange>) -> bool {
        if self.owner.target == Target::Crate {
            return true;
        }
        let mut diag = self.diag(INVALID, "this attribute can only be applied at the crate level", path);
        diag.note("read <https://doc.rust-lang.org/nightly/rustdoc/the-doc-attribute.html#at-the-crate-level> for more information");
        self.emit(diag);
        false
    }

    /// `DocParser`'s `accept_single_doc_attr`.
    fn attr(&mut self, attr: &Attr<'_>) {
        if !is_doc(attr) {
            return;
        }
        match attr.args() {
            Args::Word => self.ill_formed(attr),
            Args::List(tree) => {
                // A list rustc's parser refuses is an error, and the attribute is not parsed.
                let Some(list) = meta::list(tree) else { return };
                for item in &list.items {
                    match item {
                        Item::Meta(meta) => {
                            self.state.items += 1;
                            self.item(attr, meta);
                        }
                        Item::Lit(lit) => self.expected_name_value(attr.in_file(lit.range())),
                    }
                }
            }
            Args::NameValue(Some(ast::Expr::Literal(literal))) => {
                let lit = Lit { token: literal.token() };
                // A string is a doc comment; a suffix, or a string that does not unescape, is
                // an error.
                if lit.suffixed() || lit.token.kind() == SyntaxKind::STRING {
                    return;
                }
                self.malformed(attr.in_file(lit.range()));
            }
            // Any other value is an error, or a macro call that expands to the string.
            Args::NameValue(_) => {}
        }
    }

    /// The `IllFormedAttributeInput` of a bare `#[doc]`: the template's forms, sorted.
    fn ill_formed(&mut self, attr: &Attr<'_>) {
        let (open, close) = if attr.is_unsafe() { ("unsafe(", ")") } else { ("", "") };
        // alloc: the diagnostic's text, as is all of `ill_formed`'s
        let mut forms: Vec<String> = FORMS.iter().map(|it| format!("`{open}doc({it}){close}`")).collect();
        // alloc: as above
        forms.push(format!("`{open}doc = \"string\"{close}`"));
        forms.sort_unstable();
        // alloc: as above
        let message = format!("valid forms for the attribute are {}", stdx::list::list(forms.iter(), Join::And, false));
        let diag = self.diag(INVALID, message, attr.in_file(attr.inner_range()));
        self.emit(diag);
    }

    /// `parse_single_doc_attr_item`.
    fn item(&mut self, attr: &Attr<'_>, meta: &MetaItem<'_>) {
        let path = attr.in_file(meta.path.range);
        let Some(word) = meta.path.word() else {
            // alloc: the diagnostic's text
            let diag = self.diag(INVALID, format!("unknown `doc` attribute `{}`", meta.path.name()), path);
            self.emit(diag);
            return;
        };
        match word {
            "alias" => self.alias(attr, meta),
            "hidden" | "notable_trait" | "fake_variadic" | "search_unbox" => {
                // The last two are errors at the crate root.
                self.no_args(attr, meta);
            }
            "masked" => {
                if self.no_args(attr, meta) {
                    self.state.masked = Some(path);
                }
            }
            "html_favicon_url" | "html_logo_url" | "html_playground_url" | "html_root_url"
            | "issue_tracker_base_url" => {
                if self.string_value(attr, meta).is_some() {
                    self.crate_level(path);
                }
            }
            "html_no_source" | "rust_logo" => {
                if self.no_args(attr, meta) {
                    self.crate_level(path);
                }
            }
            "inline" | "no_inline" => {
                if self.no_args(attr, meta) {
                    self.state.inline.push((word == "inline", path));
                }
            }
            // `keyword` and `attribute`'s other checks are errors.
            "keyword" | "attribute" => {
                self.string_value(attr, meta);
            }
            // The predicate's lints are those of every `cfg`.
            "cfg" => {}
            "auto_cfg" => self.auto_cfg(attr, meta),
            "test" => {
                let MetaArgs::List(list) = &meta.args else {
                    let at = meta.args.range().unwrap_or(meta.path.range);
                    let diag = self.diag(INVALID, "`#[doc(test(...)]` takes a list of attributes", attr.in_file(at));
                    self.emit(diag);
                    return;
                };
                for item in &list.items {
                    match item {
                        Item::Meta(meta) => self.test(attr, meta),
                        Item::Lit(lit) => self.malformed(attr.in_file(lit.range())),
                    }
                }
            }
            "spotlight" => {
                let mut diag = self.diag(INVALID, "unknown `doc` attribute `spotlight`", path);
                diag.note("`doc(spotlight)` was renamed to `doc(notable_trait)`");
                diag.note("`doc(spotlight)` is now a no-op");
                diag.suggestion_with_style(
                    // alloc: the diagnostic's text
                    vec![(self.cx.span(path), "notable_trait".to_owned())],
                    "use `notable_trait` instead",
                    Applicability::MachineApplicable,
                    SuggestionStyle::HideCodeInline,
                );
                self.emit(diag);
            }
            "include" if matches!(meta.args, MetaArgs::NameValue { .. }) => {
                let MetaArgs::NameValue { value, .. } = &meta.args else { return };
                let inner = if attr.inner { "!" } else { "" };
                let mut diag = self.diag(INVALID, "unknown `doc` attribute `include`", path);
                diag.suggestion(
                    self.cx.span(path),
                    "use `doc = include_str!` instead",
                    // alloc: the diagnostic's text
                    format!("#{inner}[doc = include_str!(\"{}\")]", value.symbol()),
                    Applicability::MaybeIncorrect,
                );
                self.emit(diag);
            }
            "passes" | "no_default_passes" => {
                // alloc: the diagnostic's text, as each note below
                let mut diag = self.diag(INVALID, format!("unknown `doc` attribute `{word}`"), path);
                diag.span_label(self.cx.span(path), "no longer functions");
                // alloc: as above
                diag.note(format!(
                    "`doc` attribute `{word}` no longer functions; see issue #44136 <https://github.com/rust-lang/rust/issues/44136>"
                ));
                // alloc: as above
                diag.note(format!("`doc({word})` is now a no-op"));
                self.emit(diag);
            }
            "plugins" => {
                let mut diag = self.diag(INVALID, "unknown `doc` attribute `plugins`", path);
                diag.span_label(self.cx.span(path), "no longer functions");
                diag.note(
                    "`doc` attribute `plugins` no longer functions; see issue #44136 <https://github.com/rust-lang/rust/issues/44136> and CVE-2018-1000622 <https://nvd.nist.gov/vuln/detail/CVE-2018-1000622>",
                );
                diag.note("`doc(plugins)` is now a no-op");
                self.emit(diag);
            }
            _ => {
                // alloc: the diagnostic's text
                let diag = self.diag(INVALID, format!("unknown `doc` attribute `{word}`"), path);
                self.emit(diag);
            }
        }
    }

    /// `string_arg_and_crate_level`'s and `parse_keyword_and_attribute`'s reading of
    /// `name = "value"`: the value, else the lint.
    fn string_value<'t>(&mut self, attr: &Attr<'_>, meta: &MetaItem<'t>) -> Option<Cow<'t, str>> {
        let MetaArgs::NameValue { value, .. } = &meta.args else {
            self.expected_name_value(attr.in_file(meta.args.range().unwrap_or(meta.path.range)));
            return None;
        };
        let string = value.str();
        if string.is_none() {
            self.malformed(attr.in_file(value.range()));
        }
        string
    }

    /// `parse_alias`, `add_alias`: what is not a string, is empty, has a quote or a whitespace
    /// but a space, starts or ends with a space, or is on the crate is an error.
    fn alias(&mut self, attr: &Attr<'_>, meta: &MetaItem<'_>) {
        match &meta.args {
            MetaArgs::None => {}
            MetaArgs::List(list) => {
                for item in &list.items {
                    if let Item::Lit(lit) = item
                        && let Some(alias) = lit.str()
                    {
                        self.add_alias(alias, attr.in_file(lit.range()));
                    }
                }
            }
            MetaArgs::NameValue { value, .. } => {
                if let Some(alias) = value.str() {
                    self.add_alias(alias, attr.in_file(value.range()));
                }
            }
        }
    }

    fn add_alias(&mut self, alias: Cow<'_, str>, at: InFile<TextRange>) {
        let bad = |c: char| c == '"' || c == '\'' || (c.is_whitespace() && c != ' ');
        if alias.is_empty()
            || alias.chars().any(bad)
            || alias.starts_with(' ')
            || alias.ends_with(' ')
            || self.owner.target == Target::Crate
        {
            return;
        }
        let Some(index) = self.state.aliases.iter().position(|(it, _)| *it == alias) else {
            self.state.aliases.push((alias.into_owned(), at));
            return;
        };
        let first = std::mem::replace(&mut self.state.aliases[index].1, at);
        let mut diag = self.diag("unused_attributes", "doc alias is duplicated", at);
        diag.span_label(self.cx.span(first), "first defined here");
        self.emit(diag);
    }

    /// `parse_single_test_doc_attr_item`: an item of `test(..)`.
    fn test(&mut self, attr: &Attr<'_>, meta: &MetaItem<'_>) {
        let path = attr.in_file(meta.path.range);
        match meta.path.word() {
            Some("no_crate_inject") => {
                if !self.no_args(attr, meta) {
                    return;
                }
                if let Some(used) = self.state.no_crate_inject {
                    let mut diag = self.diag(INVALID, "unused attribute", path);
                    diag.suggestion(self.cx.span(path), "remove this attribute", "", Applicability::MachineApplicable);
                    diag.span_note(self.cx.span(used), "attribute also specified here");
                    diag.warn(PHASED_OUT);
                    self.emit(diag);
                    return;
                }
                if self.crate_level(path) {
                    self.state.no_crate_inject = Some(path);
                }
            }
            Some("attr") => {
                if !matches!(meta.args, MetaArgs::List(_)) {
                    self.malformed(attr.in_file(attr.range()));
                }
            }
            Some(name) => {
                // alloc: the diagnostic's text
                let diag = self.diag(INVALID, format!("unknown `doc(test)` attribute `{name}`"), path);
                self.emit(diag);
            }
            None => {
                let diag = self.diag(INVALID, "`#![doc(test(...)]` does not take a literal", path);
                self.emit(diag);
            }
        }
    }

    /// `parse_auto_cfg`.
    fn auto_cfg(&mut self, attr: &Attr<'_>, meta: &MetaItem<'_>) {
        let expects_hide_or_show = |this: &mut Self, at: TextRange| {
            let diag = this.diag(INVALID, "only `hide` or `show` are allowed in `#[doc(auto_cfg(...))]`", attr.in_file(at));
            this.emit(diag);
        };
        let unexpected = |this: &mut Self, name: &str, at: TextRange| {
            // alloc: the diagnostic's text
            let message = format!("`#![doc(auto_cfg({name}(...)))]` only accepts identifiers or `values(...)`");
            let diag = this.diag(INVALID, message, attr.in_file(at));
            this.emit(diag);
        };
        let list = match &meta.args {
            MetaArgs::None => return,
            MetaArgs::NameValue { value, .. } => {
                if !value.is_bool() {
                    let diag = self.diag(INVALID, "expected boolean for `#[doc(auto_cfg = ...)]`", attr.in_file(value.range()));
                    self.emit(diag);
                }
                return;
            }
            MetaArgs::List(list) => list,
        };
        'main: for item in &list.items {
            let Item::Meta(item) = item else {
                expects_hide_or_show(self, item.range());
                continue;
            };
            let name = match item.path.word() {
                Some(name @ ("hide" | "show")) => name,
                _ => {
                    expects_hide_or_show(self, item.range());
                    continue;
                }
            };
            let MetaArgs::List(names) = &item.args else {
                // alloc: the diagnostic's text
                let message = format!("`#![doc(auto_cfg({name}(...)))]` expects a list of items");
                let diag = self.diag(INVALID, message, attr.in_file(item.range()));
                self.emit(diag);
                continue;
            };
            let mut has_names = false;
            let mut has_values = false;
            for sub in &names.items {
                let Item::Meta(sub) = sub else {
                    unexpected(self, name, sub.range());
                    continue 'main;
                };
                match &sub.args {
                    MetaArgs::None if !has_values => {
                        // Not an identifier: an error.
                        if sub.path.word().is_none() {
                            continue 'main;
                        }
                        has_names = true;
                    }
                    MetaArgs::List(values) if !has_values => {
                        if sub.path.word() != Some("values") {
                            continue 'main;
                        }
                        if !has_names {
                            let message = "there must be at least one identifier before `values(...)`";
                            let diag = self.diag(INVALID, message, attr.in_file(sub.range()));
                            self.emit(diag);
                            continue 'main;
                        }
                        self.auto_cfg_values(attr, values);
                        has_values = true;
                    }
                    MetaArgs::NameValue { .. } => unexpected(self, name, sub.range()),
                    _ => {
                        let diag = self.diag(INVALID, "unexpected item after `values()`", attr.in_file(sub.range()));
                        self.emit(diag);
                    }
                }
            }
        }
    }

    /// `parse_auto_cfg_values`: the items of `values(..)`, as `DocCfgHideShow` gathers them.
    fn auto_cfg_values(&mut self, attr: &Attr<'_>, list: &List<'_>) {
        let mix = |this: &mut Self, at: TextRange, value: TextRange| {
            let mut diag = this.diag(INVALID, "`any()` was used when other values were provided", attr.in_file(at));
            diag.span_label(this.cx.span(attr.in_file(value)), "value declared here");
            this.emit(diag);
        };
        let unexpected = |this: &mut Self, name: &str, at: TextRange| {
            // alloc: the diagnostic's text
            let message = format!("`#![doc(auto_cfg({name}(...)))]` only accepts identifiers or `values(...)`");
            let diag = this.diag(INVALID, message, attr.in_file(at));
            this.emit(diag);
        };
        // `DocCfgHideShow::Any`'s span, or the first of the `List`'s values and whether one
        // of them is `none()`.
        let mut any: Option<TextRange> = None;
        let mut first: Option<TextRange> = None;
        let mut has_none = false;
        let mut seen: HashSet<Cow<'_, str>> = HashSet::default();
        for item in &list.items {
            match item {
                Item::Lit(lit) => match lit.str() {
                    Some(value) => match any {
                        Some(any) => mix(self, any, lit.range()),
                        None => {
                            if seen.insert(value) {
                                first.get_or_insert(lit.range());
                            }
                        }
                    },
                    None => unexpected(self, &lit.symbol(), lit.range()),
                },
                Item::Meta(sub) => {
                    // Not an identifier: an error.
                    let Some(ident) = sub.path.word() else { continue };
                    let empty = matches!(&sub.args, MetaArgs::List(it) if it.items.is_empty());
                    match ident {
                        "any" if empty => match (any, first) {
                            (None, Some(value)) => mix(self, sub.range(), value),
                            (None, None) => any = Some(sub.range()),
                            (Some(_), _) => {}
                        },
                        "none" if empty => {
                            if any.is_none() && !has_none {
                                has_none = true;
                                first.get_or_insert(sub.range());
                            }
                        }
                        _ => unexpected(self, ident, sub.range()),
                    }
                }
            }
        }
    }
}

/// `check_doc_attrs`' lints: `check_doc_inline` and `check_doc_masked`; the rest of what it
/// checks are errors.
fn check(cx: &Cx<'_, '_>, owner: &Owner<'_>, state: &State, emitted: &mut Vec<Emitted>) {
    let mut emit = |mut diag: Diag| {
        source_fact(cx, &mut diag, owner.at());
        emitted.push(checked(diag, owner));
    };
    if let [(inline, span), rest @ ..] = state.inline.as_slice()
        && rest.iter().all(|(it, _)| it == inline)
        && !matches!(owner.target, Target::Use | Target::ExternCrate)
    {
        let span = cx.span(*span);
        // clone: the diagnostic's span, as below
        let mut diag = Diag::lint(INVALID, "this attribute can only be applied to a `use` item", span.clone());
        diag.span_label(span, "only applicable on `use` items");
        diag.span_label(cx.span(hir_span(owner)), "not a `use` item");
        diag.note("read <https://doc.rust-lang.org/nightly/rustdoc/the-doc-attribute.html#inline-and-no_inline> for more information");
        emit(diag);
    }
    if let Some(span) = state.masked {
        let span = cx.span(span);
        if owner.target != Target::ExternCrate {
            // clone: as above
            let mut diag = Diag::lint(INVALID, "this attribute can only be applied to an `extern crate` item", span.clone());
            diag.span_label(span, "only applicable on `extern crate` items");
            diag.span_label(cx.span(hir_span(owner)), "not an `extern crate` item");
            diag.note("read <https://doc.rust-lang.org/unstable-book/language-features/doc-masked.html> for more information");
            emit(diag);
        } else if ast::ExternCrate::cast(owner.node.value)
            .and_then(|it| it.name_ref())
            .is_some_and(|it| it.self_token().is_some())
        {
            // clone: as above
            let mut diag = Diag::lint(INVALID, "this attribute cannot be applied to an `extern crate self` item", span.clone());
            diag.span_label(span, "not applicable on `extern crate self` items");
            diag.span_label(cx.span(hir_span(owner)), "`extern crate self` defined here");
            emit(diag);
        }
    }
}

/// rustc's `hir_span`: what a lint labels as the node `owner` is.
fn hir_span(owner: &Owner<'_>) -> InFile<TextRange> {
    let node = owner.node.value;
    let range = item_range_view(node);
    let until = |end: Option<TextSize>| match end {
        Some(end) if end > range.start() => TextRange::new(range.start(), end),
        _ => range,
    };
    let end_of = |kind: SyntaxKind| node.children().find(|it| it.kind() == kind).map(|it| it.text_range().end());
    // `named_span`: to the end of the name, or of the generics when they are after it.
    let named = || until(end_of(SyntaxKind::NAME).max(end_of(SyntaxKind::GENERIC_PARAM_LIST)));
    let value = match node.kind() {
        SyntaxKind::SOURCE_FILE => inner_span(node),
        SyntaxKind::FN => ast::Fn::cast(node).map_or(range, signature_range_view),
        SyntaxKind::IMPL => def_range_view(node),
        SyntaxKind::CONST | SyntaxKind::STATIC => {
            until(node.children().find(|it| ast::Type::can_cast(it.kind())).map(|it| it.text_range().end()))
        }
        SyntaxKind::TRAIT if ast::Trait::cast(node).is_some_and(|it| it.eq_token().is_some()) => named(),
        SyntaxKind::TRAIT => until(end_of(SyntaxKind::TYPE_BOUND_LIST).or_else(|| Some(named().end()))),
        SyntaxKind::TYPE_ALIAS if is_trait_item(node) => {
            until(end_of(SyntaxKind::TYPE_BOUND_LIST).or_else(|| Some(named().end())))
        }
        SyntaxKind::USE => {
            let tree = node.children().find(|it| it.kind() == SyntaxKind::USE_TREE);
            let path = tree.and_then(|it| it.children().find(|it| it.kind() == SyntaxKind::PATH));
            path.or(tree).map_or(range, |it| it.text_range())
        }
        SyntaxKind::EXTERN_CRATE => {
            let rename = node.children().find(|it| it.kind() == SyntaxKind::RENAME).map(|rename| {
                rename
                    .children_with_tokens()
                    .find(|it| matches!(it.kind(), SyntaxKind::NAME | SyntaxKind::UNDERSCORE))
                    .map_or(rename.text_range().end(), |it| it.text_range().end())
            });
            until(rename.or_else(|| end_of(SyntaxKind::NAME_REF)))
        }
        SyntaxKind::VARIANT => until(end_of(SyntaxKind::NAME)),
        SyntaxKind::STRUCT
        | SyntaxKind::ENUM
        | SyntaxKind::UNION
        | SyntaxKind::TYPE_ALIAS
        | SyntaxKind::MODULE
        | SyntaxKind::MACRO_RULES
        | SyntaxKind::MACRO_DEF => named(),
        SyntaxKind::CLOSURE_EXPR => until(end_of(SyntaxKind::RET_TYPE).or_else(|| end_of(SyntaxKind::PARAM_LIST))),
        _ => range,
    };
    InFile::new(owner.node.file_id, value)
}

/// Whether `node` is an item of a trait.
fn is_trait_item(node: ast::Node<'_>) -> bool {
    node.parent()
        .filter(|it| it.kind() == SyntaxKind::ASSOC_ITEM_LIST)
        .and_then(|it| it.parent())
        .is_some_and(|it| it.kind() == SyntaxKind::TRAIT)
}

/// The crate's `inner_span`: from its first token to its last, comments left out.
fn inner_span(node: ast::Node<'_>) -> TextRange {
    let mut elements = node.children_with_tokens().filter(|it| !it.kind().is_trivia());
    let Some(first) = elements.next() else { return TextRange::empty(node.text_range().start()) };
    let last = elements.last().unwrap_or(first);
    first.text_range().cover(last.text_range())
}

