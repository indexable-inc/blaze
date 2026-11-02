//! What an attribute's parser is given (rustc's `AcceptContext`): the crate, the node and the
//! attribute, and the errors it says of its input (`AttributeDiagnosticContext`,
//! `AttributeParseError`: "malformed `x` attribute input" with what was expected and the
//! attribute's valid forms), and of its target (`InvalidTarget`, `InvalidAttrStyle`).
//!
//! Follows rustc's `compiler/rustc_attr_parsing/src/context.rs` (`AcceptContext`,
//! `ExpectNameValue`, `ExpectStringLiteral`, `AttributeDiagnosticContext`),
//! `compiler/rustc_attr_parsing/src/session_diagnostics.rs` (`AttributeParseError`,
//! `InvalidTarget`), `compiler/rustc_attr_parsing/src/target_checking.rs` (`check_target`,
//! `check_crate_level`) and `compiler/rustc_attr_parsing/src/template.rs` (MIT or Apache-2.0).

use hir::InFile;
use span::Edition;
use syntax::{TextRange, TextSize};

use super::args::{ArgParser, Ident, Lit, MetaItem, MetaItemOrLit, MetaList, NameValue};
use stdx::list::Join;
use crate::{
    checks::features::Features,
    cx::{Cx, item_range_view as item_range},
    lints::syntax::attrs::{
        Attr, Owner, Target,
        parse::{Krate, applied, crate_level},
        table::{Allowed, Builtin, Targets},
    },
    model::{Applicability, Diag, Span, SuggestionStyle},
};

/// What the crate says that the parsers read: rustc's `Session` and `Features`, of what they
/// look at.
pub(crate) struct Session {
    /// The crate root and the features the targets an attribute can be applied to depend on.
    pub(crate) krate: Krate,
    pub(crate) features: Features,
    pub(crate) target: TargetInfo,
    /// rustc's `registered_tools`, besides the builtin ones: those of `#![register_tool(..)]`.
    tools: Vec<String>,
}

/// What the parsers read of the target (rustc's `sess.target`), from the crate's cfg.
pub(crate) struct TargetInfo {
    pub(crate) arch: String,
    pub(crate) os: String,
    pub(crate) vendor: String,
    pub(crate) families: Vec<String>,
    /// `target_pointer_width`; every target sets it.
    pub(crate) pointer_width: Option<u32>,
}

/// rustc's `BinaryFormat`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BinaryFormat {
    Coff,
    Elf,
    MachO,
    Wasm,
    Xcoff,
}

impl TargetInfo {
    fn of(cx: &Cx<'_, '_>) -> TargetInfo {
        let cfg = cx.krate.cfg(cx.db);
        // alloc: setup, once per check: the target's cfg values the parsers read
        let one = |key: &str| cfg.get_cfg_values(key).next().map(|it| it.to_string()).unwrap_or_default();
        TargetInfo {
            arch: one("target_arch"),
            os: one("target_os"),
            vendor: one("target_vendor"),
            // alloc: setup, once per check: the target's families
            families: cfg.get_cfg_values("target_family").map(|it| it.to_string()).collect(),
            pointer_width: one("target_pointer_width").parse().ok(),
        }
    }

    /// rustc's `is_like_darwin`: an Apple target.
    pub(crate) fn is_like_darwin(&self) -> bool {
        self.vendor == "apple"
    }

    /// rustc's `is_like_windows`: Windows and UEFI.
    pub(crate) fn is_like_windows(&self) -> bool {
        matches!(self.os.as_str(), "windows" | "uefi")
    }

    pub(crate) fn is_like_wasm(&self) -> bool {
        self.families.iter().any(|it| it == "wasm")
    }

    pub(crate) fn binary_format(&self) -> BinaryFormat {
        if self.is_like_darwin() {
            BinaryFormat::MachO
        } else if self.is_like_windows() {
            BinaryFormat::Coff
        } else if self.is_like_wasm() {
            BinaryFormat::Wasm
        } else if self.os == "aix" {
            BinaryFormat::Xcoff
        } else {
            BinaryFormat::Elf
        }
    }
}

/// rustc's builtin tools, which `#![register_tool(..)]` adds to.
const BUILTIN_TOOLS: [&str; 5] = ["clippy", "rustfmt", "diagnostic", "miri", "rust_analyzer"];

impl Session {
    /// Built once per check, by `checks::check`, from the crate's attributes and `features`
    /// (what `checks::features::configured` found).
    pub(crate) fn of<'db>(cx: &Cx<'_, 'db>, owners: &[Owner<'db>], features: Features) -> Session {
        // alloc: setup, once per check: the tools the crate registers
        let mut tools = Vec::new();
        let crate_attrs = owners.iter().filter(|it| it.target == Target::Crate).flat_map(|it| &it.attrs);
        for attr in crate_attrs.filter(|it| super::is(it, "register_tool")) {
            // alloc: setup, once per `#![register_tool]`: its parse errors, dropped here
            let mut quiet = Vec::new();
            let Some(ArgParser::List(list)) = super::args::parse(cx, attr, &["register_tool"], &mut quiet) else {
                continue;
            };
            for item in list.mixed() {
                if let Some(ident) = item.meta_item_no_args().and_then(MetaItem::ident) {
                    // alloc: once per tool a crate registers, at setup.
                    tools.push(ident.name.to_string());
                }
            }
        }
        Session { krate: Krate::of(cx, owners), features, target: TargetInfo::of(cx), tools }
    }

    pub(crate) fn is_tool(&self, name: &str) -> bool {
        BUILTIN_TOOLS.contains(&name) || self.tools.iter().any(|it| it == name)
    }

    /// rustc's `Features::enabled`.
    pub(crate) fn feature(&self, name: &str) -> bool {
        self.features.enabled(name)
    }

    /// rustc's `feature_err` of the language feature `feature` at `span`.
    pub(crate) fn feature_err(&self, feature: &str, span: Span, explain: impl Into<String>) -> Diag {
        crate::checks::features::feature_err(&self.features, feature, span, explain)
    }
}

/// A suggestion a parser gives with the error it says next, in place of the attribute's valid
/// forms: rustc's `Suggestion`.
struct Custom {
    message: String,
    range: TextRange,
    code: String,
}

/// What one attribute's parser is given: rustc's `AcceptContext`, and its
/// `AttributeDiagnosticContext` (the suggestions it pushes go with the next error).
pub(crate) struct Accept<'a, 'c, 'db> {
    pub(crate) cx: &'a Cx<'c, 'db>,
    pub(crate) session: &'a Session,
    pub(crate) owner: &'a Owner<'db>,
    pub(crate) attr: &'a Attr<'db>,
    /// Its index in the owner's attributes.
    pub(crate) index: usize,
    pub(crate) builtin: &'static Builtin,
    errors: &'a mut Vec<Diag>,
    /// Rarely more than one; empty until a parser pushes a suggestion.
    custom: Vec<Custom>,
}

impl<'a, 'c, 'db> Accept<'a, 'c, 'db> {
    pub(crate) fn new(
        cx: &'a Cx<'c, 'db>,
        session: &'a Session,
        owner: &'a Owner<'db>,
        index: usize,
        builtin: &'static Builtin,
        errors: &'a mut Vec<Diag>,
    ) -> Accept<'a, 'c, 'db> {
        let attr = &owner.attrs[index];
        // alloc: empty, allocates nothing
        Accept { cx, session, owner, attr, index, builtin, errors, custom: Vec::new() }
    }

    /// A range of the attribute's file.
    pub(crate) fn span(&self, range: TextRange) -> Span {
        self.cx.span(self.attr.in_file(range))
    }

    /// rustc's `attr_span`.
    pub(crate) fn attr_range(&self) -> TextRange {
        self.attr.range()
    }

    /// rustc's `inner_span`.
    pub(crate) fn inner_range(&self) -> TextRange {
        self.attr.inner_range()
    }

    /// rustc's `attr_path.span`.
    pub(crate) fn path_range(&self) -> TextRange {
        self.attr.path_range()
    }

    pub(crate) fn attr_span(&self) -> Span {
        self.span(self.attr_range())
    }

    pub(crate) fn inner_span(&self) -> Span {
        self.span(self.inner_range())
    }

    pub(crate) fn path_span(&self) -> Span {
        self.span(self.path_range())
    }

    /// rustc's `target_span`: what the attribute is applied to, without its attributes.
    pub(crate) fn target_span(&self) -> Span {
        let node = self.owner.node;
        self.cx.span(InFile::new(node.file_id, item_range(node.value)))
    }

    pub(crate) fn target(&self) -> Target {
        self.owner.target
    }

    /// rustc's `attr_path`, as it prints it.
    pub(crate) fn name(&self) -> String {
        self.attr.name()
    }

    /// The edition of the attribute's path.
    pub(crate) fn edition(&self) -> Edition {
        self.attr.file.edition(self.cx.db)
    }

    pub(crate) fn feature(&self, name: &str) -> bool {
        self.session.feature(name)
    }

    /// rustc's `emit_err`.
    pub(crate) fn emit_err(&mut self, diag: Diag) {
        self.errors.push(diag);
    }

    /// `parse`, the errors it gives fatal (rustc's `ShouldEmit::EarlyFatal`).
    pub(crate) fn fatal<T>(&mut self, parse: impl FnOnce(&mut Self) -> T) -> T {
        let from = self.errors.len();
        let parsed = parse(self);
        for diag in &mut self.errors[from..] {
            if diag.is_error() && !diag.is_lint() {
                diag.stop();
            }
        }
        parsed
    }

    /// An error with code `code` at `range` of the attribute's file.
    pub(crate) fn err(&self, code: Option<&str>, message: impl Into<String>, range: TextRange) -> Diag {
        Diag::error(code, message, self.span(range))
    }

    /// The error of one of `checks::features::attributes`' gates, when it gives one.
    pub(crate) fn emit_gate(&mut self, diag: Option<Diag>) {
        if let Some(diag) = diag {
            self.emit_err(diag);
        }
    }

    /// `range` of the file the attribute is written in.
    pub(crate) fn in_file(&self, range: TextRange) -> InFile<TextRange> {
        self.attr.in_file(range)
    }

    /// rustc's `expect_single_element_list`; `range` is of the node `args` is of.
    pub(crate) fn expect_single_element_list<'p>(
        &mut self,
        args: &'p ArgParser,
        range: TextRange,
    ) -> Option<&'p MetaItemOrLit> {
        let ArgParser::List(list) = args else {
            self.expected_list(range, args);
            return None;
        };
        let Some(single) = list.as_single() else {
            self.expected_single_argument(list.range, list.len());
            return None;
        };
        Some(single)
    }

    /// rustc's `expect_list`.
    pub(crate) fn expect_list<'p>(&mut self, args: &'p ArgParser, range: TextRange) -> Option<&'p MetaList> {
        let list = args.as_list();
        if list.is_none() {
            self.expected_list(range, args);
        }
        list
    }

    /// rustc's `expect_single`.
    pub(crate) fn expect_single<'p>(&mut self, list: &'p MetaList) -> Option<&'p MetaItemOrLit> {
        let single = list.as_single();
        if single.is_none() {
            self.expected_single_argument(list.range, list.len());
        }
        single
    }

    /// rustc's `expect_name_value` of an `ArgParser`.
    pub(crate) fn expect_name_value<'p>(
        &mut self,
        args: &'p ArgParser,
        range: TextRange,
        name: Option<&str>,
    ) -> Option<&'p NameValue> {
        let nv = args.as_name_value();
        if nv.is_none() {
            self.expected_name_value(range, name);
        }
        nv
    }

    /// rustc's `expect_name_value` of a `MetaItemOrLitParser`.
    pub(crate) fn expect_name_value_item<'p>(
        &mut self,
        item: &'p MetaItemOrLit,
        name: Option<&str>,
    ) -> Option<(&'p Ident, &'p NameValue)> {
        let Some(meta) = item.meta_item() else {
            self.expected_name_value(item.range(), name);
            return None;
        };
        self.expect_name_value_meta(meta, name)
    }

    /// rustc's `expect_name_value` of a `MetaItemParser`.
    pub(crate) fn expect_name_value_meta<'p>(
        &mut self,
        meta: &'p MetaItem,
        name: Option<&str>,
    ) -> Option<(&'p Ident, &'p NameValue)> {
        let word = meta.path.word();
        let nv = meta.args.as_name_value();
        if word.is_none() {
            self.expected_identifier(meta.path.range);
        }
        if nv.is_none() {
            self.expected_name_value(meta.range(), name);
        }
        word.zip(nv)
    }

    /// rustc's `expect_no_args`.
    pub(crate) fn expect_no_args(&mut self, args: &ArgParser) -> Option<()> {
        if let Err(range) = args.as_no_args() {
            self.expected_no_args(range);
            return None;
        }
        Some(())
    }

    /// rustc's `expect_string_literal` of a `NameValueParser`.
    pub(crate) fn expect_string_literal(&mut self, nv: &NameValue) -> Option<String> {
        // alloc: the value the parsed attribute keeps
        self.expect_str(nv).map(str::to_owned)
    }

    /// [`Accept::expect_string_literal`], the value borrowed from the attribute.
    pub(crate) fn expect_str<'p>(&mut self, nv: &'p NameValue) -> Option<&'p str> {
        let value = nv.value_as_str();
        if value.is_none() {
            self.expected_string_literal(nv.value_range(), Some(nv.value_as_lit()));
        }
        value
    }

    /// rustc's `expect_string_literal` of a `MetaItemOrLitParser`.
    pub(crate) fn expect_string_literal_item(&mut self, item: &MetaItemOrLit) -> Option<String> {
        let Some(lit) = item.as_lit() else {
            self.expected_string_literal(item.range(), None);
            return None;
        };
        let value = lit.str().map(str::to_owned);
        if value.is_none() {
            self.expected_string_literal(item.range(), Some(lit));
        }
        value
    }

    /// rustc's `push_suggestion`: given with the next error, in place of the valid forms.
    pub(crate) fn push_suggestion(&mut self, message: impl Into<String>, range: TextRange, code: impl Into<String>) -> &mut Self {
        self.custom.push(Custom { message: message.into(), range, code: code.into() });
        self
    }

    /// The attribute's valid forms: rustc's `suggestions`.
    pub(crate) fn suggestions(&self) -> Vec<String> {
        self.builtin.template.suggestions(&self.name(), self.attr.is_unsafe())
    }

    pub(crate) fn expected_integer_literal_in_range(&mut self, range: TextRange, lower: i64, upper: i64) {
        // alloc: the error's label
        self.parse_error(range, Reason::Label(format!("expected an integer literal in the range of {lower}..={upper}")));
    }

    /// rustc's `expected_list`; `range` is used when `args` has none.
    pub(crate) fn expected_list(&mut self, range: TextRange, args: &ArgParser) {
        let range = args.range().unwrap_or(range);
        // alloc: the error's label
        self.parse_error(range, Reason::Label("expected this to be a list".to_owned()));
    }

    pub(crate) fn expected_list_with_num_args_or_more(&mut self, args: usize, range: TextRange) {
        // alloc: the error's label
        self.parse_error(range, Reason::Label(format!("expected {args} or more items")));
    }

    pub(crate) fn expected_list_or_no_args(&mut self, range: TextRange) {
        // alloc: the error's label
        self.parse_error(range, Reason::Label("expected a list or no arguments here".to_owned()));
    }

    pub(crate) fn expected_nv_or_no_args(&mut self, range: TextRange) {
        // alloc: the error's label
        self.parse_error(range, Reason::Label("didn't expect a list here".to_owned()));
    }

    pub(crate) fn expected_non_empty_string_literal(&mut self, range: TextRange) {
        // alloc: the error's label
        self.parse_error(range, Reason::Label("string is not allowed to be empty".to_owned()));
    }

    pub(crate) fn expected_no_args(&mut self, range: TextRange) {
        // alloc: the error's label
        self.parse_error(range, Reason::Coded("didn't expect any arguments here".to_owned(), "E0565"));
    }

    pub(crate) fn expected_identifier(&mut self, range: TextRange) {
        // alloc: the error's label
        self.parse_error(range, Reason::Coded("expected a valid identifier here".to_owned(), "E0565"));
    }

    /// rustc's `expected_name_value`: `name = "..."` was expected at `range`.
    pub(crate) fn expected_name_value(&mut self, range: TextRange, name: Option<&str>) {
        let reason = match name {
            // alloc: the error's label
            Some(name) => Reason::Label(format!("expected this to be of the form `{name} = \"...\"`")),
            None => Reason::NameValue,
        };
        self.parse_error(range, reason);
    }

    pub(crate) fn missing_name_value(&mut self, range: TextRange, name: &str) {
        // alloc: the error's label
        self.parse_error(range, Reason::Label(format!("missing argument `{name} = \"...\"`")));
    }

    pub(crate) fn duplicate_key(&mut self, range: TextRange, key: &str) {
        // alloc: the error's label
        self.parse_error(range, Reason::Coded(format!("found `{key}` used as a key more than once"), "E0538"));
    }

    pub(crate) fn expected_not_literal(&mut self, range: TextRange) {
        // alloc: the error's label
        self.parse_error(range, Reason::Coded("didn't expect a literal here".to_owned(), "E0565"));
    }

    /// rustc's `expected_single_argument`: none or more than one were given.
    pub(crate) fn expected_single_argument(&mut self, range: TextRange, provided: usize) {
        let label = match provided {
            0 => "expected an argument here",
            _ => "expected a single argument here",
        };
        // alloc: the error's label
        self.parse_error(range, Reason::Coded(label.to_owned(), "E0805"));
    }

    pub(crate) fn expected_at_least_one_argument(&mut self, range: TextRange) {
        // alloc: the error's label
        self.parse_error(range, Reason::Label("expected at least 1 argument here".to_owned()));
    }

    /// "valid arguments are `a` or `b`".
    pub(crate) fn expected_specific_argument(&mut self, range: TextRange, possibilities: &[&str]) {
        let label = specific(possibilities, '`').map(|it| match possibilities.len() {
            // alloc: the error's label
            1 => format!("the only valid argument here is {it}"),
            // alloc: the error's label
            _ => format!("valid arguments are {it}"),
        });
        self.parse_error(range, Reason::Specific(label));
    }

    /// "this attribute is only valid with `a` as an argument".
    pub(crate) fn expected_specific_argument_and_list(&mut self, range: TextRange, possibilities: &[&str]) {
        let label = specific(possibilities, '`').map(|it| match possibilities.len() {
            // alloc: the error's label
            1 => format!("this attribute is only valid with {it} as an argument"),
            // alloc: the error's label
            2 => format!("this attribute is only valid with either {it} as an argument"),
            // alloc: the error's label
            _ => format!("this attribute is only valid with one of the following arguments: {it}"),
        });
        self.parse_error(range, Reason::Specific(label));
    }

    /// "valid arguments are \"a\" or \"b\"".
    pub(crate) fn expected_specific_argument_strings(&mut self, range: TextRange, possibilities: &[&str]) {
        let label = specific(possibilities, '"').map(|it| match possibilities.len() {
            // alloc: the error's label
            1 => format!("the only valid argument here is {it}"),
            // alloc: the error's label
            _ => format!("valid arguments are {it}"),
        });
        self.parse_error(range, Reason::Specific(label));
    }

    /// rustc's `expected_string_literal`, with the literal found, if one was.
    pub(crate) fn expected_string_literal(&mut self, range: TextRange, found: Option<&Lit>) {
        let reason = match found {
            Some(lit) if lit.is_bytestr() => Reason::ByteString(TextRange::at(lit.range.start(), TextSize::from(1))),
            // alloc: the error's label
            _ => Reason::Label("expected a string literal here".to_owned()),
        };
        self.parse_error(range, reason);
    }

    pub(crate) fn expected_filename_literal(&mut self, range: TextRange) {
        // alloc: the error's label
        self.parse_error(range, Reason::Label("expected a filename string literal here".to_owned()));
    }

    pub(crate) fn expected_integer_literal(&mut self, range: TextRange) {
        // alloc: the error's label
        self.parse_error(range, Reason::Label("expected an integer literal here".to_owned()));
    }

    /// rustc's `emit_parse_error` and `AttributeParseError::into_diag`.
    fn parse_error(&mut self, range: TextRange, reason: Reason) {
        let custom = std::mem::take(&mut self.custom);
        let inner = self.inner_range();
        let range = if range == self.attr_range() { inner } else { range };
        let span = self.span(range);
        let inner_span = self.span(inner);
        // alloc: the error's message; clone: its span, used again for the suggestions
        let mut diag = Diag::error(Some("E0539"), format!("malformed `{}` attribute input", self.name()), inner_span.clone());
        let mut code = None;
        match reason {
            Reason::ByteString(prefix) => {
                diag.suggestion(self.span(prefix), "consider removing the prefix", "", Applicability::MaybeIncorrect);
                diag.note("expected a normal string literal, not a byte string literal");
                self.errors.push(diag);
                return;
            }
            Reason::Label(label) => {
                diag.span_label(span, label);
            }
            Reason::Coded(label, it) => {
                diag.span_label(span, label);
                code = Some(it);
            }
            Reason::NameValue => {
                if range != inner {
                    diag.span_label(span, "expected this to be of the form `... = \"...\"`");
                }
            }
            Reason::Specific(label) => {
                if let Some(label) = label {
                    diag.span_label(span, label);
                }
            }
        }
        if let Some(code) = code {
            // alloc: the error's code
            diag.code = Some(crate::model::Code::Error(code.to_owned()));
        }
        if let Some(docs) = self.builtin.template.docs {
            // alloc: the error's note
            diag.note(format!("for more information, visit <{docs}>"));
        }
        if custom.is_empty() {
            let suggestions = self.suggestions();
            if suggestions.len() < 4 {
                let message = match suggestions.len() {
                    // alloc: the error's suggestion message
                    1 => "must be of the form".to_owned(),
                    // alloc: the error's suggestion message
                    _ => "try changing it to one of the following valid forms of the attribute".to_owned(),
                };
                diag.suggestions(inner_span, message, suggestions, Applicability::HasPlaceholders, SuggestionStyle::ShowAlways);
            }
        } else if custom.len() < 4 {
            for it in custom {
                diag.suggestion_verbose(self.span(it.range), it.message, it.code, Applicability::MaybeIncorrect);
            }
        }
        self.errors.push(diag);
    }

    /// rustc's `check_target` of an attribute whose parser checks its own targets
    /// (`AllowedTargets::ManuallyChecked`), with its arguments as the message names them
    /// (`"(C)"`): the errors; the lints are `lints::syntax::attrs`'.
    pub(crate) fn check_target(&mut self, args: &str, targets: Targets) {
        if let Some(diag) = target_error(self.cx, &self.session.krate, self.owner, self.attr, targets, args) {
            self.errors.push(diag);
        }
    }

    /// [`Accept::check_target`] of `#[name = "value"]`, its arguments written out only for
    /// the error.
    pub(crate) fn check_target_value(&mut self, value: &str, targets: Targets) {
        if target_allowed(self.owner, targets) {
            return;
        }
        // alloc: the error's arguments
        self.check_target(&format!(" = \"{value}\""), targets);
    }
}

/// What a parse error says at its span.
enum Reason {
    Label(String),
    /// A label, and the error's code in place of E0539.
    Coded(String, &'static str),
    /// `ExpectedNameValue(None)`: labelled when it is not the whole attribute.
    NameValue,
    /// `ExpectedSpecificArgument`: nothing when there are no possibilities.
    Specific(Option<String>),
    /// A byte string where a string was expected: the `b` to remove.
    ByteString(TextRange),
}

/// `a`, `a` or `b`, `a`, `b` or `c`, each in `quote`s; `None` for none.
fn specific(possibilities: &[&str], quote: char) -> Option<String> {
    match possibilities {
        [] => None,
        // alloc: an error's label
        [x] => Some(format!("{quote}{x}{quote}")),
        [first @ .., second_to_last, last] => {
            // alloc: an error's label
            let mut out = String::new();
            for it in first {
                // alloc: an error's label
                out.push_str(&format!("{quote}{it}{quote}, "));
            }
            // alloc: an error's label
            out.push_str(&format!("{quote}{second_to_last}{quote} or {quote}{last}{quote}"));
            Some(out)
        }
    }
}

/// Whether `check_target` gives no error on `owner` of an attribute allowed on `targets`.
fn target_allowed(owner: &Owner<'_>, targets: Targets) -> bool {
    match targets.crate_level() {
        Some(warn) => warn || owner.target == Target::Crate,
        None => targets.is_allowed(owner.target) != Allowed::Error,
    }
}

/// The error rustc's `check_target` gives of `attr` (with arguments `args`, empty for most) on
/// `owner`: `InvalidTarget` where it is not allowed, `InvalidAttrStyle` for a crate-level one
/// elsewhere.
pub(crate) fn target_error(
    cx: &Cx<'_, '_>,
    krate: &Krate,
    owner: &Owner<'_>,
    attr: &Attr<'_>,
    targets: Targets,
    args: &str,
) -> Option<Diag> {
    if target_allowed(owner, targets) {
        return None;
    }
    if targets.crate_level().is_some() {
        let mut diag = crate_level(cx, krate, owner, attr);
        diag.level = crate::model::Level::Error;
        diag.code = None;
        return Some(diag);
    }
    let path = attr.path();
    let name = attr.name();
    let range = if args.is_empty() { attr.path_range() } else { attr.inner_range() };
    // alloc: the error's message
    let message = format!("the `{name}{args}` attribute cannot be used on {}", owner.target.plural_name());
    let mut diag = Diag::error(None, message, cx.span(attr.in_file(range)));
    diag.suggestion_with_style(
        // alloc: the error's suggestion
        vec![(cx.span(attr.in_file(attr.range())), String::new())],
        "remove the attribute",
        Applicability::MachineApplicable,
        SuggestionStyle::CompletelyHidden,
    );
    // alloc: the targets the error's help names
    let (applied, only) = applied(krate, targets.allowed().collect(), owner.target);
    let only = if only { "only " } else { "" };
    // alloc: the error's help
    diag.help(format!("the `{name}{args}` attribute can {only}be applied to {}", stdx::list::list(applied.iter(), Join::And, false)));
    match (path.as_slice(), owner.target) {
        ([it], Target::Static) if it.as_str() == "link_name" => {
            let inner = cx.span(attr.in_file(attr.inner_range()));
            // alloc: the error's suggestion
            let mut parts = Vec::new();
            if !attr.is_unsafe() {
                // alloc: the error's suggestion
                parts.push((inner.shrink_to_lo(), "unsafe(".to_owned()));
            }
            // alloc: the error's suggestion
            parts.push((cx.span(attr.in_file(attr.path_range())), "export_name".to_owned()));
            if !attr.is_unsafe() {
                // alloc: the error's suggestion
                parts.push((inner.shrink_to_hi(), ")".to_owned()));
            }
            diag.multipart_suggestion("did you mean to use `#[export_name]`?", parts, Applicability::MaybeIncorrect);
        }
        ([it], Target::Fn | Target::Method(_)) if it.as_str() == "repr" && args == "(align(...))" && krate.fn_align => {
            diag.help("use `#[rustc_align(...)]` instead");
        }
        ([it], Target::Static) if it.as_str() == "repr" && args == "(align(...))" && krate.static_align => {
            diag.help("use `#[rustc_align_static(...)]` instead");
        }
        _ => {}
    }
    if owner.target == Target::MacroCall {
        diag.note(
            "placing this attribute on a macro invocation does nothing even if the macro expands to what would be a valid target for the attribute",
        );
    }
    Some(diag)
}

#[cfg(test)]
mod tests {
    use super::specific;

    #[test]
    fn possibilities_are_listed_with_or() {
        assert_eq!(specific(&[], '`'), None);
        assert_eq!(specific(&["a"], '`').as_deref(), Some("`a`"));
        assert_eq!(specific(&["a", "b"], '"').as_deref(), Some("\"a\" or \"b\""));
        assert_eq!(specific(&["a", "b", "c"], '`').as_deref(), Some("`a`, `b` or `c`"));
    }
}
