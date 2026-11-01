//! `--check-cfg` and what rustc lints with it: the names and values it expects ([`CheckCfg`]),
//! `unexpected_cfgs` on every cfg rustc reads, and `explicit_builtin_cfgs_in_flags` on a
//! `--cfg` that sets one of rustc's own.
//!
//! - `meta`: rustc's meta items, read from a command line's spec or a `cfg_select!`'s tokens.
//! - `unexpected`: where rustc reads a cfg, and what `unexpected_cfgs` says of one.
//! - `well_known`: the names and values rustc expects whatever the command line says.
//!
//! Follows rustc's `compiler/rustc_interface/src/interface.rs` (`parse_cfg`,
//! `parse_check_cfg`), `compiler/rustc_session/src/config/cfg.rs` (`CheckCfg`,
//! `ExpectedValues`, `disallow_cfgs`) and `compiler/rustc_session/src/diagnostics.rs`
//! (`UnexpectedBuiltinCfg`) (MIT or Apache-2.0). The nightly has no unstable form of
//! `--check-cfg` left: every one `add` takes is stable.

mod meta;
mod unexpected;
mod well_known;

use std::borrow::Cow;

use syntax::Edition;

use super::{Emitted, Pass, Stage, Walk};
use crate::{
    cx::Cx,
    emit::{Sources, Style},
    lints::CommandLine,
    lower::Phase,
    model::{Code, Diag, Level},
};

/// What `--check-cfg` says the expected cfg names and values are.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CheckCfg {
    /// Whether any `--check-cfg` was given: only then are cfgs checked, against what is given
    /// and the names and values rustc knows.
    given: bool,
    /// Whether a name not expected is linted: `cfg(any())` was not given.
    exhaustive_names: bool,
    /// The names given, in the order first given, with their values.
    expecteds: Vec<(String, Values)>,
    /// rustc is run by cargo (`CARGO_CRATE_NAME` is set), as it is under blaze: the lint's help
    /// says what to write in `Cargo.toml` and `build.rs` rather than on the command line.
    pub cargo: bool,
}

/// rustc's `ExpectedValues`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Values {
    /// `values(any())`.
    Any,
    /// Each value expected, `None` for the name alone; each once.
    Some(Vec<Option<String>>),
}

const VISIT: &str = "visit <https://doc.rust-lang.org/nightly/rustc/check-cfg.html> for more details";

impl CheckCfg {
    /// One `--check-cfg` value, as rustc parses it in the default edition (2015); the error
    /// is rustc's, as it writes it.
    pub fn add(&mut self, spec: &str) -> Result<(), String> {
        self.add_in(spec, Edition::Edition2015)
    }

    /// One `--check-cfg` value, as rustc parses it for a crate of `edition` (which decides
    /// which names are keywords).
    pub fn add_in(&mut self, spec: &str, edition: Edition) -> Result<(), String> {
        self.add_diag(spec, edition).map_err(|it| rendered(&it))
    }

    /// Whether any `--check-cfg` was given.
    pub fn is_given(&self) -> bool {
        self.given
    }

    /// The names given, with their values, in the order first given.
    pub(crate) fn expecteds(&self) -> &[(String, Values)] {
        &self.expecteds
    }

    pub(crate) fn exhaustive_names(&self) -> bool {
        self.exhaustive_names
    }

    /// [`CheckCfg::add_in`], its error rustc's fatal error. Nothing is added when it fails.
    pub(crate) fn add_diag(&mut self, spec: &str, edition: Edition) -> Result<(), Diag> {
        // alloc: the error's text; `add_diag` is setup, once per `--check-cfg`
        let error = |reason: &str| invalid(spec, vec![reason.to_owned()]);
        let in_arg = |arg: &meta::Inner<'_>, reason: &str| {
            let shown = arg.to_text();
            let what = match arg.lit() {
                Some(lit) => {
                    let (article, descr) = lit.kind.described();
                    // alloc: the error's text
                    format!("`{shown}` is {article} {descr} literal")
                }
                // alloc: the error's text
                None => format!("`{shown}` is invalid"),
            };
            // alloc: the error's text
            invalid(spec, vec![what, reason.to_owned()])
        };
        let expected = || error("expected `cfg(name, values(\"value1\", \"value2\", ... \"valueN\"))`");

        let tokens = meta::lex(spec, edition).ok_or_else(expected)?;
        let mut parser = meta::Parser::new(&tokens, edition);
        let item = parser.meta_item().ok_or_else(expected)?;
        if !parser.at_end() {
            return Err(expected());
        }
        let Some(args) = item.list() else { return Err(expected()) };
        if !item.has_name("cfg") {
            return Err(expected());
        }

        // alloc: setup, once per `--check-cfg`
        let mut names: Vec<&str> = Vec::new();
        // alloc: as above
        let mut values: Vec<Option<Cow<'_, str>>> = Vec::new();
        fn insert<'s>(values: &mut Vec<Option<Cow<'s, str>>>, value: Option<Cow<'s, str>>) {
            if !values.contains(&value) {
                values.push(value);
            }
        }
        let (mut any_specified, mut values_specified, mut values_any_specified) = (false, false, false);
        for arg in args {
            if arg.is_word()
                && let Some(ident) = arg.ident()
            {
                if values_specified {
                    return Err(error("`cfg()` names cannot be after values"));
                }
                if meta::is_path_segment_keyword(ident.name) {
                    return Err(error("malformed `cfg` input, expected a valid identifier"));
                }
                names.push(ident.name);
            } else if let Some(boolean) = arg.boolean_literal() {
                if values_specified {
                    return Err(error("`cfg()` names cannot be after values"));
                }
                names.push(if boolean { "true" } else { "false" });
            } else if arg.has_name("any")
                && let Some(list) = arg.list()
            {
                if any_specified {
                    return Err(error("`any()` cannot be specified multiple times"));
                }
                any_specified = true;
                if !list.is_empty() {
                    return Err(in_arg(arg, "`any()` takes no argument"));
                }
            } else if arg.has_name("values")
                && let Some(list) = arg.list()
            {
                if names.is_empty() {
                    return Err(error("`values()` cannot be specified before the names"));
                } else if values_specified {
                    return Err(error("`values()` cannot be specified multiple times"));
                }
                values_specified = true;
                for arg in list {
                    if let Some(value) = arg.lit().and_then(|it| it.value_str()) {
                        insert(&mut values, Some(value));
                    } else if arg.has_name("any")
                        && let Some(list) = arg.list()
                    {
                        if values_any_specified {
                            return Err(in_arg(arg, "`any()` in `values()` cannot be specified multiple times"));
                        }
                        values_any_specified = true;
                        if !list.is_empty() {
                            return Err(in_arg(arg, "`any()` in `values()` takes no argument"));
                        }
                    } else if arg.has_name("none")
                        && let Some(list) = arg.list()
                    {
                        insert(&mut values, None);
                        if !list.is_empty() {
                            return Err(in_arg(arg, "`none()` in `values()` takes no argument"));
                        }
                    } else {
                        return Err(in_arg(arg, "`values()` arguments must be string literals, `none()` or `any()`"));
                    }
                }
            } else {
                return Err(in_arg(arg, "`cfg()` arguments must be simple identifiers, `any()` or `values(...)`"));
            }
        }

        if !values_specified && !any_specified {
            insert(&mut values, None);
        } else if !values.is_empty() && values_any_specified {
            return Err(error("`values()` arguments cannot specify string literals and `any()` at the same time"));
        }
        if any_specified && !(names.is_empty() && values.is_empty() && !values_specified && !values_any_specified) {
            return Err(error("`cfg(any())` can only be provided in isolation"));
        }

        if !self.given {
            self.given = true;
            self.exhaustive_names = true;
        }
        if any_specified {
            self.exhaustive_names = false;
            return Ok(());
        }
        for name in names {
            let given = || match values_any_specified {
                true => Values::Any,
                // alloc: setup, the expected values kept for the session
                false => Values::Some(values.iter().map(|it| it.as_deref().map(str::to_owned)).collect()),
            };
            match self.expecteds.iter_mut().find(|(it, _)| it == name) {
                // alloc: as above
                None => self.expecteds.push((name.to_owned(), given())),
                Some((_, expected)) => {
                    if values_any_specified {
                        if matches!(expected, Values::Some(_)) {
                            *expected = Values::Any;
                        }
                    } else if let Values::Some(old) = expected {
                        for value in &values {
                            let value = value.as_deref().map(str::to_owned);
                            if !old.contains(&value) {
                                old.push(value);
                            }
                        }
                    }
                }
            }
        }
        Ok(())
    }
}

/// rustc's fatal error on a `--check-cfg` it cannot read.
fn invalid(spec: &str, notes: Vec<String>) -> Diag {
    // alloc: the error's text
    let mut diag = Diag::new(Level::Error, None, format!("invalid `--check-cfg` argument: `{spec}`"));
    for note in notes {
        diag.note(note);
    }
    diag.note(VISIT);
    diag
}

/// An error of the command line as rustc writes it: it points into no file, so no span of it
/// is refused; one that were would be the error's text instead.
fn rendered(diag: &Diag) -> String {
    crate::emit::rendered(diag, &Sources::of(std::iter::empty()), Style::default())
        .unwrap_or_else(|why| why.to_string())
}

/// One `--cfg` value as rustc's `parse_cfg` reads it for a crate of `edition`: its name, and
/// its value when it has one; the error is rustc's fatal error.
pub(crate) fn parse_cfg_diag(spec: &str, edition: Edition) -> Result<(String, Option<String>), Diag> {
    // alloc: the error's text; `parse_cfg_diag` is setup, once per `--cfg`
    let error = |reason: &str| Diag::new(Level::Error, None, format!("invalid `--cfg` argument: `{spec}` ({reason})"));
    if let Some(tokens) = meta::lex(spec, edition) {
        let mut parser = meta::Parser::new(&tokens, edition);
        if let Some(item) = parser.meta_item()
            && parser.at_end()
        {
            if item.global || item.path.len() != 1 {
                return Err(error("argument key must be an identifier"));
            }
            match &item.kind {
                meta::Kind::List(_) => {}
                meta::Kind::NameValue(lit) if !matches!(lit.kind, meta::LitKind::Str | meta::LitKind::StrRaw) => {
                    return Err(error("argument value must be a string"));
                }
                meta::Kind::NameValue(_) | meta::Kind::Word => {
                    let name = item.path[0].name;
                    if meta::is_path_segment_keyword(name) {
                        return Err(error("malformed `cfg` input, expected a valid identifier"));
                    }
                    let value = match &item.kind {
                        meta::Kind::NameValue(lit) => lit.value_str().map(Cow::into_owned),
                        _ => None,
                    };
                    // alloc: setup, as above
                    return Ok((name.to_owned(), value));
                }
            }
        }
    }
    if spec.contains('=') && !spec.contains("=\"") && !spec.ends_with('"') {
        return Err(error(concat!(
            r#"expected `key` or `key="value"`, ensure escaping is appropriate"#,
            r#" for your shell, try 'key="value"' or key=\"value\""#
        )));
    }
    Err(error(r#"expected `key` or `key="value"`"#))
}

/// What this module lints: `explicit_builtin_cfgs_in_flags` of each `--cfg`, as the session is
/// made, then `unexpected_cfgs` as expansion reads each cfg, when a `--check-cfg` was given.
/// A `--cfg` rustc cannot parse is its fatal error, and nothing else is said.
pub(crate) fn lints<'db>(cx: &Cx<'_, 'db>, cmd: &CommandLine, walk: &Walk<'db>) -> Vec<Emitted> {
    let mut emitted = match builtin_cfgs(cx, &cmd.cfg, walk) {
        Ok(emitted) => emitted,
        // alloc: the fatal error
        Err(fatal) => return vec![fatal],
    };
    if cmd.check_cfg.is_given() {
        emitted.extend(unexpected::lints(cx, &cmd.check_cfg, walk));
    }
    emitted
}

/// rustc's `parse_cfg` of each `--cfg` (an `IndexSet`: each once, in the order first given),
/// then its `disallow_cfgs`: a `--cfg` setting a cfg rustc sets itself, buffered for the crate.
fn builtin_cfgs(cx: &Cx<'_, '_>, specs: &[String], walk: &Walk<'_>) -> Result<Vec<Emitted>, Emitted> {
    // alloc: empty, allocates nothing until a lint
    let mut emitted = Vec::new();
    let mut cfgs: Vec<(String, Option<String>)> = Vec::with_capacity(specs.len());
    for spec in specs {
        let cfg = parse_cfg_diag(spec, cx.edition)
            .map_err(|diag| Emitted::unplaced(diag, Phase::Resolve, Pass::Buffered).buffered(Stage::Configured))?;
        if !cfgs.contains(&cfg) {
            cfgs.push(cfg);
        }
    }
    if cfgs.is_empty() {
        return Ok(emitted);
    }
    let root = cx.krate.root_module(cx.db).definition_source_file_id(cx.db);
    let Some(tree) = walk.tree(root) else { return Ok(emitted) };
    let at = tree.ptr(tree.root);
    for (name, value) in &cfgs {
        let Some(controlled_by) = controlled_by(name, value.as_deref()) else { continue };
        let shown = match &value {
            // alloc: the diagnostic's text, as is all below
            Some(value) => format!(r#"{name}="{value}""#),
            // clone: as above
            None => name.clone(),
        };
        let mut diag = Diag::new(
            Level::Warning,
            // alloc: as above
            Some(Code::Lint("explicit_builtin_cfgs_in_flags".to_owned())),
            // alloc: as above
            format!("unexpected `--cfg {shown}` flag"),
        );
        // alloc: as above
        diag.note(format!("config `{name}` is only supposed to be controlled by `{controlled_by}`"));
        diag.note("manually setting a built-in cfg can and does create incoherent behaviors");
        emitted.push(Emitted::new(diag, Phase::Resolve, Pass::Buffered, at).buffered(Stage::Configured));
    }
    Ok(emitted)
}

/// The flag that sets the builtin cfg `name` (with `value`), if rustc refuses it on the
/// command line.
fn controlled_by(name: &str, value: Option<&str>) -> Option<&'static str> {
    if name == "panic" {
        return value.is_some_and(|it| well_known::DISALLOWED_PANIC.contains(&it)).then_some("-C panic");
    }
    let &(_, with_value, flag) = well_known::DISALLOWED.iter().find(|(it, ..)| *it == name)?;
    match with_value {
        Some(with_value) if with_value != value.is_some() => None,
        _ => Some(flag),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_builtin_cfg_is_refused_as_rustc_refuses_it() {
        assert_eq!(controlled_by("unix", None), Some("--target"));
        assert_eq!(controlled_by("unix", Some("x")), None);
        assert_eq!(controlled_by("target_os", Some("linux")), Some("--target"));
        assert_eq!(controlled_by("target_os", None), None);
        assert_eq!(controlled_by("target_env", None), Some("--target"));
        assert_eq!(controlled_by("panic", Some("abort")), Some("-C panic"));
        assert_eq!(controlled_by("panic", Some("other")), None);
        assert_eq!(controlled_by("panic", None), None);
        assert_eq!(controlled_by("test", None), None);
        assert_eq!(controlled_by("feature", Some("std")), None);
    }

    #[test]
    fn cfg_values_parse_as_rustc_parses_them() {
        let parse = |it: &str| parse_cfg_diag(it, Edition::Edition2021).map_err(|it| it.message);
        assert_eq!(parse("unix"), Ok(("unix".to_owned(), None)));
        assert_eq!(parse(r#"feature="std""#), Ok(("feature".to_owned(), Some("std".to_owned()))));
        assert_eq!(parse(r#"x = "a\"b""#), Ok(("x".to_owned(), Some("a\"b".to_owned()))));
        assert_eq!(parse("r#async"), Ok(("async".to_owned(), None)));
        assert_eq!(parse("a::b"), Err("invalid `--cfg` argument: `a::b` (argument key must be an identifier)".to_owned()));
        assert_eq!(parse("a = 1"), Err("invalid `--cfg` argument: `a = 1` (argument value must be a string)".to_owned()));
        assert_eq!(
            parse("self"),
            Err("invalid `--cfg` argument: `self` (malformed `cfg` input, expected a valid identifier)".to_owned())
        );
        assert_eq!(parse("a(b)"), Err(r#"invalid `--cfg` argument: `a(b)` (expected `key` or `key="value"`)"#.to_owned()));
        assert_eq!(
            parse("a=b"),
            Err(concat!(
                r#"invalid `--cfg` argument: `a=b` (expected `key` or `key="value"`, ensure escaping is appropriate"#,
                r#" for your shell, try 'key="value"' or key=\"value\")"#
            )
            .to_owned())
        );
        assert!(parse("async").is_err());
        assert_eq!(parse_cfg_diag("async", Edition::Edition2015).map_err(|it| it.message), Ok(("async".to_owned(), None)));
    }
}
