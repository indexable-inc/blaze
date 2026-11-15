//! lint_table: blaze's registry of rustc's lints (crates/blaze-lint-defs/src/rustc.rs), written
//! from rustc's own source at the pinned commit, and held to it.
//!
//!     blaze run -q -p scripts --bin lint_table -- '{"command": "write", "rust": "<checkout>"}'
//!     blaze run -q -p scripts --bin lint_table -- '{"command": "check"}'
//!     blaze run -q -p scripts --bin lint_table -- '{"command": "check", "rust": "<checkout>"}'
//!     blaze run -q -p scripts --bin lint_table -- selftest
//!
//! `rust` is a checkout of rust-lang/rust at the commit library/UPSTREAM names, with at least
//! compiler/rustc_lint, compiler/rustc_lint_defs and compiler/rustc_feature (the toolchain
//! bump's sparse clone has them: harness/toolchain_bump.py, `vendor`). `write` refuses a
//! checkout at another commit.
//!
//! What it reads, as rustc's `register_builtins` and `register_internals` do
//! (compiler/rustc_lint/src/lib.rs): each `declare_lint!` and `declare_tool_lint!` (name, level,
//! text, future-incompatibility, edition level, feature gate with its issue from
//! rustc_feature's unstable.rs, the flags), in the order the lint passes register them (the
//! combined pre-expansion, early and late module passes, `foreign_modules`, the hardwired
//! ones, then the internal passes), the lints declared and never registered, the groups, the
//! alias, the renamed, removed and ignored names, and the `rustc::internal` group.
//!
//! `check` with no `rust` needs no network and is the gate's: the file says which commit it
//! was written from, which must be library/UPSTREAM's (so a toolchain bump that does not
//! write it again fails), and carries a digest of its own text (so a hand edit fails). With
//! `rust` it writes the table again in memory and compares every byte.
//!
//! The registry was a hand copy of rustc 1.99's for two weeks after the pin moved to a 1.101
//! nightly, with nine lints unknown by name (agents row `lang1100-2`).

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt::Write as _,
    path::{Path, PathBuf},
};

use blaze_script::{
    fail,
    input::{self, Fields, Input, InputError},
};

const SCRIPT: &str = "lint_table";
const OUT: &str = "crates/blaze-lint-defs/src/rustc.rs";
const UPSTREAM: &str = "library/UPSTREAM";

struct Args {
    command: String,
    rust: Option<String>,
}

impl Input for Args {
    const FIELDS: &'static [&'static str] = &["command", "rust"];

    fn parse(f: &Fields<'_>) -> Result<Args, InputError> {
        Ok(Args { command: f.one_of("command", &["write", "check"], None)?, rust: f.str_or_null("rust")? })
    }
}

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).parent().map_or_else(|| PathBuf::from("."), Path::to_path_buf)
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|e| fail(SCRIPT, &format!("{}: {e}", path.display())))
}

// Tokens of Rust source, as far as the lint declarations need them.

#[derive(Clone, Debug, PartialEq, Eq)]
enum Tok {
    Ident(String),
    /// A string literal's value.
    Str(String),
    Num(String),
    Punct(char),
    Open(char),
    Close(char),
}

/// The tokens of `text`, comments (doc comments too) left out.
fn tokens(text: &str) -> Vec<Tok> {
    let s: Vec<char> = text.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < s.len() {
        let c = s[i];
        if c.is_whitespace() {
            i += 1;
        } else if c == '/' && s.get(i + 1) == Some(&'/') {
            while i < s.len() && s[i] != '\n' {
                i += 1;
            }
        } else if c == '/' && s.get(i + 1) == Some(&'*') {
            let mut depth = 1;
            i += 2;
            while i < s.len() && depth > 0 {
                if s[i] == '/' && s.get(i + 1) == Some(&'*') {
                    depth += 1;
                    i += 2;
                } else if s[i] == '*' && s.get(i + 1) == Some(&'/') {
                    depth -= 1;
                    i += 2;
                } else {
                    i += 1;
                }
            }
        } else if c == '"' {
            let (value, end) = cooked(&s, i + 1);
            out.push(Tok::Str(value));
            i = end;
        } else if (c == 'r' || c == 'b') && raw_start(&s, i).is_some() {
            let (hashes, start) = raw_start(&s, i).unwrap_or((0, i));
            let mut end = start;
            'find: while end < s.len() {
                if s[end] == '"' && (0..hashes).all(|k| s.get(end + 1 + k) == Some(&'#')) {
                    break 'find;
                }
                end += 1;
            }
            out.push(Tok::Str(s[start..end.min(s.len())].iter().collect()));
            i = end + 1 + hashes;
        } else if c == 'b' && s.get(i + 1) == Some(&'"') {
            let (value, end) = cooked(&s, i + 2);
            out.push(Tok::Str(value));
            i = end;
        } else if c == '\'' {
            // A char literal or a lifetime: neither is read, both are stepped over.
            if s.get(i + 1) == Some(&'\\') {
                i += 2;
                while i < s.len() && s[i] != '\'' {
                    i += 1;
                }
                i += 1;
            } else if s.get(i + 2) == Some(&'\'') {
                i += 3;
            } else {
                i += 1;
                while i < s.len() && (s[i].is_alphanumeric() || s[i] == '_') {
                    i += 1;
                }
            }
        } else if c.is_alphabetic() || c == '_' {
            let start = i;
            while i < s.len() && (s[i].is_alphanumeric() || s[i] == '_') {
                i += 1;
            }
            out.push(Tok::Ident(s[start..i].iter().collect()));
        } else if c.is_ascii_digit() {
            let start = i;
            while i < s.len() && (s[i].is_alphanumeric() || s[i] == '_') {
                i += 1;
            }
            out.push(Tok::Num(s[start..i].iter().collect()));
        } else if matches!(c, '(' | '[' | '{') {
            out.push(Tok::Open(c));
            i += 1;
        } else if matches!(c, ')' | ']' | '}') {
            out.push(Tok::Close(c));
            i += 1;
        } else {
            out.push(Tok::Punct(c));
            i += 1;
        }
    }
    out
}

/// `r"`, `r#"`, `br"`...: the number of `#`s and where the text starts.
fn raw_start(s: &[char], at: usize) -> Option<(usize, usize)> {
    let mut i = at;
    if s.get(i) == Some(&'b') {
        i += 1;
    }
    if s.get(i) != Some(&'r') {
        return None;
    }
    i += 1;
    let mut hashes = 0;
    while s.get(i) == Some(&'#') {
        hashes += 1;
        i += 1;
    }
    (s.get(i) == Some(&'"')).then_some((hashes, i + 1))
}

/// The value of the string literal whose text starts at `at`, and the index after its `"`.
fn cooked(s: &[char], mut at: usize) -> (String, usize) {
    let mut out = String::new();
    while at < s.len() && s[at] != '"' {
        if s[at] != '\\' {
            out.push(s[at]);
            at += 1;
            continue;
        }
        at += 1;
        match s.get(at) {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('\n') => {
                // A line continuation: the white space that follows is not of the string.
                while at < s.len() && s[at].is_whitespace() {
                    at += 1;
                }
                continue;
            }
            Some(&other) => out.push(other),
            None => {}
        }
        at += 1;
    }
    (out, at + 1)
}

/// The index of the token that closes the group opened at `open`.
fn closing(toks: &[Tok], open: usize) -> usize {
    let mut depth = 0usize;
    for (i, tok) in toks.iter().enumerate().skip(open) {
        match tok {
            Tok::Open(_) => depth += 1,
            Tok::Close(_) => {
                depth -= 1;
                if depth == 0 {
                    return i;
                }
            }
            _ => {}
        }
    }
    toks.len()
}

/// The token groups of every `name!( .. )`, `name![ .. ]` and `name!{ .. }` in `toks`.
fn macro_calls<'t>(toks: &'t [Tok], name: &str) -> Vec<&'t [Tok]> {
    let mut out = Vec::new();
    let mut i = 0;
    while i + 2 < toks.len() {
        if toks[i] == Tok::Ident(name.to_owned())
            && toks[i + 1] == Tok::Punct('!')
            && matches!(toks[i + 2], Tok::Open(_))
        {
            let end = closing(toks, i + 2);
            out.push(&toks[i + 3..end]);
            i = end;
        }
        i += 1;
    }
    out
}

/// `toks` without its attributes (`#[..]`).
fn without_attrs(toks: &[Tok]) -> Vec<Tok> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < toks.len() {
        if toks[i] == Tok::Punct('#') && toks.get(i + 1) == Some(&Tok::Open('[')) {
            i = closing(toks, i + 1) + 1;
        } else {
            out.push(toks[i].clone());
            i += 1;
        }
    }
    out
}

// The lints.

#[derive(Clone, Debug, PartialEq, Eq)]
struct Lint {
    /// As `declare_lint!` names it: `DEAD_CODE`.
    ident: String,
    /// As an attribute names it: `dead_code`, `rustc::default_hash_types`.
    name: String,
    level: String,
    desc: String,
    /// `Reason::..(..)` as blaze writes it.
    reason: Option<String>,
    report_in_deps: bool,
    explain_reason: bool,
    edition: Option<(String, String)>,
    gate: Option<String>,
    external: bool,
    crate_level: bool,
    eval_always: bool,
    ignore_deny_warnings: bool,
}

fn ident(tok: Option<&Tok>) -> Option<&str> {
    match tok {
        Some(Tok::Ident(it)) => Some(it),
        _ => None,
    }
}

/// `fcw!(..)`'s reason, as a `Reason` of blaze-lint-defs.
fn reason(toks: &[Tok], lint: &str) -> String {
    let bad = || -> ! { fail(SCRIPT, &format!("{lint}: a `fcw!` this script does not read: {toks:?}")) };
    let edition = |at: usize| match toks.get(at) {
        Some(Tok::Num(year)) => format!("Edition::Edition{year}"),
        _ => bad(),
    };
    let page = |at: usize| match toks.get(at) {
        Some(Tok::Str(it)) => format!("{it:?}"),
        _ => bad(),
    };
    let issue = |at: usize| match (toks.get(at), toks.get(at + 1)) {
        (Some(Tok::Punct('#')), Some(Tok::Num(n))) => n.replace('_', ""),
        _ => bad(),
    };
    match toks.first() {
        Some(Tok::Str(text)) => format!("Reason::Custom({text:?}, {})", issue(1)),
        Some(Tok::Ident(kind)) => match kind.as_str() {
            "FutureReleaseError" | "FutureReleaseSemanticsChange" => format!("Reason::{kind}({})", issue(1)),
            "EditionError"
            | "EditionSemanticsChange"
            | "EditionAndFutureReleaseError"
            | "EditionAndFutureReleaseSemanticsChange" => format!("Reason::{kind}({}, {})", edition(1), page(2)),
            _ => bad(),
        },
        _ => bad(),
    }
}

/// One `declare_lint!` or `declare_tool_lint!`.
fn lint(body: &[Tok]) -> Option<Lint> {
    let toks = without_attrs(body);
    let mut i = 0;
    if ident(toks.get(i)) == Some("pub") {
        i += 1;
        if toks.get(i) == Some(&Tok::Open('(')) {
            i = closing(&toks, i) + 1;
        }
    }
    let first = ident(toks.get(i))?.to_owned();
    i += 1;
    let (tool, name_ident) = if toks.get(i) == Some(&Tok::Punct(':')) && toks.get(i + 1) == Some(&Tok::Punct(':')) {
        let name = ident(toks.get(i + 2))?.to_owned();
        i += 3;
        (Some(first), name)
    } else {
        (None, first)
    };
    // The macro's own definition (`$NAME`) is no lint.
    if !name_ident.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_') {
        return None;
    }
    if toks.get(i) != Some(&Tok::Punct(',')) {
        return None;
    }
    let level = ident(toks.get(i + 1))?.to_owned();
    if !matches!(level.as_str(), "Allow" | "Warn" | "Deny" | "Forbid") {
        return None;
    }
    let Some(Tok::Str(desc)) = toks.get(i + 3) else { return None };
    let name = match &tool {
        Some(tool) => format!("{tool}::{}", name_ident.to_lowercase()),
        None => name_ident.to_lowercase(),
    };
    let mut lint = Lint {
        ident: name_ident,
        name,
        level,
        desc: desc.clone(),
        reason: None,
        report_in_deps: false,
        explain_reason: true,
        edition: None,
        gate: None,
        external: false,
        crate_level: false,
        eval_always: false,
        ignore_deny_warnings: false,
    };
    let rest = &toks[i + 4..];
    let truth = |at: usize| ident(rest.get(at)) == Some("true");
    let mut j = 0;
    while j < rest.len() {
        match &rest[j] {
            Tok::Ident(word) => match word.as_str() {
                // `declare_lint!`'s bare flag, `declare_tool_lint!`'s `report_in_external_macro: true`.
                "report_in_external_macro" => {
                    lint.external = rest.get(j + 1) != Some(&Tok::Punct(':')) || truth(j + 2);
                }
                "crate_level_only" => lint.crate_level = true,
                "ignore_deny_warnings" => lint.ignore_deny_warnings = true,
                // `declare_tool_lint!`'s bare `true`.
                "true" if tool.is_some() && rest.get(j.wrapping_sub(1)) == Some(&Tok::Punct(',')) => {
                    lint.external = true;
                }
                "eval_always" => lint.eval_always = truth(j + 2),
                "feature_gate" => lint.gate = ident(rest.get(j + 2)).map(str::to_owned),
                "edition" if j > 0 && rest[j - 1] == Tok::Punct('@') => {
                    let edition = ident(rest.get(j + 1)).unwrap_or_default().to_owned();
                    // `=>` is two tokens.
                    let level = ident(rest.get(j + 4)).unwrap_or_default().to_owned();
                    lint.edition = Some((edition, level));
                }
                "future_incompatible" => {
                    let Some(open) = (j..rest.len()).find(|&k| rest[k] == Tok::Open('{')) else {
                        fail(SCRIPT, &format!("{}: `@future_incompatible` with no braces", lint.name))
                    };
                    let end = closing(rest, open);
                    let info = &rest[open + 1..end];
                    let fcw = macro_calls(info, "fcw");
                    let Some(fcw) = fcw.first() else {
                        fail(SCRIPT, &format!("{}: a reason that is not `fcw!(..)`", lint.name))
                    };
                    lint.reason = Some(reason(fcw, &lint.name));
                    for (k, tok) in info.iter().enumerate() {
                        match ident(Some(tok)) {
                            Some("report_in_deps") => lint.report_in_deps = ident(info.get(k + 2)) == Some("true"),
                            Some("explain_reason") => lint.explain_reason = ident(info.get(k + 2)) != Some("false"),
                            _ => {}
                        }
                    }
                    j = end;
                }
                _ => {}
            },
            _ => {}
        }
        j += 1;
    }
    Some(lint)
}

/// The `.rs` files under `dir`, sorted.
fn sources(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        fail(SCRIPT, &format!("{} is not a directory: is `rust` a checkout with compiler/?", dir.display()))
    };
    let mut entries: Vec<PathBuf> = entries.filter_map(|it| it.ok().map(|it| it.path())).collect();
    entries.sort();
    for path in entries {
        if path.is_dir() {
            sources(&path, out);
        } else if path.extension().is_some_and(|it| it == "rs") {
            out.push(path);
        }
    }
}

/// What rustc's source says of its lints.
#[derive(Default)]
struct Rustc {
    /// By `declare_lint!`'s identifier.
    lints: BTreeMap<String, Lint>,
    /// `declare_lint_pass!` and `impl_lint_pass!`: each pass's lints, in order.
    passes: BTreeMap<String, Vec<String>>,
    /// lib.rs's tokens.
    lib: Vec<Tok>,
    /// Feature gates' issues, by name (`None` for a feature with no issue).
    issues: BTreeMap<String, Option<String>>,
}

impl Rustc {
    fn add(&mut self, text: &str) {
        let toks = tokens(text);
        for name in ["declare_lint", "declare_tool_lint"] {
            for body in macro_calls(&toks, name) {
                if let Some(lint) = lint(body) {
                    self.lints.insert(lint.ident.clone(), lint);
                }
            }
        }
        for name in ["declare_lint_pass", "impl_lint_pass"] {
            for body in macro_calls(&toks, name) {
                let body = without_attrs(body);
                let Some(pass) = ident(body.first()) else { continue };
                let Some(open) = body.iter().position(|it| *it == Tok::Open('[')) else { continue };
                let end = closing(&body, open);
                let lints = body[open + 1..end]
                    .split(|it| *it == Tok::Punct(','))
                    .filter_map(|it| ident(it.last()).map(str::to_owned))
                    .collect();
                self.passes.insert(pass.to_owned(), lints);
            }
        }
    }

    /// The lints of a module's own `fn lint_vec() -> LintVec { vec![A, B] }` (`foreign_modules`,
    /// `hardwired`), kept as the pass `name`.
    fn lint_vec(&mut self, name: &str, text: &str) {
        let toks = tokens(text);
        let Some(at) = toks.iter().position(|it| *it == Tok::Ident("lint_vec".to_owned())) else {
            fail(SCRIPT, &format!("`{name}` has no `fn lint_vec`"))
        };
        let Some(list) = macro_calls(&toks[at..], "vec").into_iter().next() else {
            fail(SCRIPT, &format!("`{name}::lint_vec` is not a `vec![..]`"))
        };
        let lints =
            list.split(|it| *it == Tok::Punct(',')).filter_map(|it| ident(it.last()).map(str::to_owned)).collect();
        self.passes.insert(name.to_owned(), lints);
    }

    fn features(&mut self, text: &str) {
        let toks = tokens(text);
        for i in 0..toks.len() {
            if toks[i] != Tok::Open('(') || !matches!(ident(toks.get(i + 1)), Some("unstable" | "internal" | "incomplete")) {
                continue;
            }
            let Some(name) = ident(toks.get(i + 3)) else { continue };
            let end = closing(&toks, i);
            let issue = toks[i..end].iter().rev().find_map(|it| match it {
                Tok::Num(n) => Some(n.clone()),
                _ => None,
            });
            let none = toks[i..end].iter().any(|it| ident(Some(it)) == Some("None"));
            self.issues.insert(name.to_owned(), if none { None } else { issue });
        }
    }

    /// The passes of the combined pass `name`, as its macro lists them.
    fn combined(&self, name: &str) -> Vec<String> {
        // `[pub Name, [Pass: expr, ..]]`: the name, a comma, the list.
        let declared = |i: &usize| {
            self.lib[*i] == Tok::Ident(name.to_owned())
                && self.lib.get(i + 1) == Some(&Tok::Punct(','))
                && self.lib.get(i + 2) == Some(&Tok::Open('['))
        };
        let Some(at) = (0..self.lib.len()).find(declared) else {
            fail(SCRIPT, &format!("compiler/rustc_lint/src/lib.rs declares no combined pass `{name}`"))
        };
        let open = at + 2;
        let end = closing(&self.lib, open);
        let mut out = Vec::new();
        let mut depth = 0usize;
        let mut start = true;
        for i in open + 1..end {
            match &self.lib[i] {
                Tok::Open(_) => depth += 1,
                Tok::Close(_) => depth -= 1,
                Tok::Punct(',') if depth == 0 => start = true,
                Tok::Ident(pass) if depth == 0 && start => {
                    out.push(pass.clone());
                    start = false;
                }
                _ => {}
            }
        }
        out
    }

    /// The lints of `passes`, in order, each once.
    fn of_passes(&self, passes: &[String], seen: &mut BTreeSet<String>, out: &mut Vec<Lint>) {
        for pass in passes {
            let Some(lints) = self.passes.get(pass) else {
                fail(SCRIPT, &format!("the pass `{pass}` has no `declare_lint_pass!` or `impl_lint_pass!`"))
            };
            for ident in lints {
                let Some(lint) = self.lints.get(ident) else {
                    fail(SCRIPT, &format!("the pass `{pass}` lists `{ident}`, which no `declare_lint!` declares"))
                };
                if seen.insert(ident.clone()) {
                    out.push(lint.clone());
                }
            }
        }
    }

    fn name_of(&self, ident: &str) -> String {
        match self.lints.get(ident) {
            Some(lint) => lint.name.clone(),
            None => fail(SCRIPT, &format!("a group names `{ident}`, which no `declare_lint!` declares")),
        }
    }

    /// The string arguments of every `store.<method>(..)` in lib.rs.
    fn calls(&self, method: &str) -> Vec<Vec<String>> {
        let mut out = Vec::new();
        for i in 0..self.lib.len().saturating_sub(1) {
            if self.lib[i] == Tok::Ident(method.to_owned()) && self.lib[i + 1] == Tok::Open('(') {
                let end = closing(&self.lib, i + 1);
                let strs: Vec<String> = self.lib[i + 2..end]
                    .iter()
                    .filter_map(|it| match it {
                        Tok::Str(s) => Some(s.clone()),
                        _ => None,
                    })
                    .collect();
                if !strs.is_empty() {
                    out.push(strs);
                }
            }
        }
        out
    }
}

/// The table's text.
fn table(rustc: &Rustc, commit: &str) -> String {
    let mut seen = BTreeSet::new();
    let mut builtin = Vec::new();
    for combined in
        ["BuiltinCombinedPreExpansionLintPass", "BuiltinCombinedEarlyLintPass", "BuiltinCombinedLateLintModPass"]
    {
        rustc.of_passes(&rustc.combined(combined), &mut seen, &mut builtin);
    }
    // `foreign_modules::lint_vec()` and `hardwired::lint_vec()`.
    rustc.of_passes(&["foreign_modules".to_owned(), "hardwired".to_owned()], &mut seen, &mut builtin);
    let mut internal = Vec::new();
    for combined in ["InternalCombinedEarlyLintPass", "InternalCombinedLateLintModPass"] {
        rustc.of_passes(&rustc.combined(combined), &mut seen, &mut internal);
    }
    let unregistered: Vec<Lint> = rustc.lints.values().filter(|it| !seen.contains(&it.ident)).cloned().collect();

    let mut body = String::new();
    let lints = |body: &mut String, doc: &str, name: &str, lints: &[Lint]| {
        _ = writeln!(body, "\n{doc}\n#[rustfmt::skip]\npub(super) const {name}: &[Lint] = &[");
        for lint in lints {
            // blaze's own lint of the word in a string: say whose text it is.
            if lint.desc.contains("dyn ") {
                body.push_str("    // dyn: rustc's text of the lint, no trait object\n");
            }
            _ = write!(body, "    Lint::new({:?}, Level::{}, {:?})", lint.name, lint.level, lint.desc);
            if let Some(reason) = &lint.reason {
                let deps = if lint.report_in_deps { "fcw_deps" } else { "fcw" };
                _ = write!(body, ".{deps}({reason})");
                if !lint.explain_reason {
                    body.push_str(".unexplained()");
                }
            }
            if let Some((edition, level)) = &lint.edition {
                _ = write!(body, ".edition(Edition::{edition}, Level::{level})");
            }
            if let Some(gate) = &lint.gate {
                let issue = match rustc.issues.get(gate) {
                    Some(Some(issue)) => format!("Some({issue})"),
                    Some(None) => "None".to_owned(),
                    None => fail(SCRIPT, &format!("{}: no feature `{gate}` in rustc_feature's unstable.rs", lint.name)),
                };
                _ = write!(body, ".gate({gate:?}, {issue})");
            }
            for (flag, method) in [
                (lint.external, "external"),
                (lint.crate_level, "crate_level"),
                (lint.eval_always, "eval_always"),
                (lint.ignore_deny_warnings, "ignore_deny_warnings"),
            ] {
                if flag {
                    _ = write!(body, ".{method}()");
                }
            }
            body.push_str(",\n");
        }
        body.push_str("];\n");
    };
    lints(
        &mut body,
        "/// The lints `register_builtins` registers, in its order: the combined pre-expansion, early\n/// and late module passes' (each pass's in the order its macro lists them), `foreign_modules`',\n/// then the hardwired ones.",
        "BUILTIN",
        &builtin,
    );
    lints(
        &mut body,
        "/// Lints rustc declares and never registers: an attribute can't name them, and some are\n/// still emitted at their default level.",
        "UNREGISTERED",
        &unregistered,
    );
    lints(&mut body, "/// `register_internals`: the internal early pass's, then the internal late pass's.", "INTERNAL", &internal);

    // The groups: `add_lint_group!("name", A, B, ..)`.
    _ = writeln!(body, "\n/// `add_lint_group!`, in order.\n#[rustfmt::skip]\npub(super) const GROUPS: &[(&str, &[&str])] = &[");
    for group in macro_calls(&rustc.lib, "add_lint_group") {
        let Some(Tok::Str(name)) = group.first() else { continue };
        let members: Vec<String> =
            group[1..].iter().filter_map(|it| ident(Some(it))).map(|it| format!("{:?}", rustc.name_of(it))).collect();
        _ = writeln!(body, "    ({name:?}, &[{}]),", members.join(", "));
    }
    body.push_str("];\n");
    let pairs = |body: &mut String, doc: &str, name: &str, method: &str, swap: bool| {
        _ = writeln!(body, "\n{doc}\n#[rustfmt::skip]\npub(super) const {name}: &[(&str, &str)] = &[");
        for call in rustc.calls(method) {
            let [a, b] = call.as_slice() else {
                fail(SCRIPT, &format!("`{method}` with other than two strings: {call:?}"))
            };
            let (a, b) = if swap { (b, a) } else { (a, b) };
            _ = writeln!(body, "    ({a:?}, {b:?}),");
        }
        body.push_str("];\n");
    };
    pairs(&mut body, "/// `register_group_alias`: the alias and the group, a silent one.", "ALIASES", "register_group_alias", true);
    pairs(&mut body, "/// `register_renamed`: the old name and the new.", "RENAMED", "register_renamed", false);
    pairs(&mut body, "/// `register_removed`: the name and why.", "REMOVED", "register_removed", false);

    // `register_ignored`: the names of the `RUSTDOC_LINTS` list.
    let ignored: Vec<String> = match rustc.lib.iter().position(|it| *it == Tok::Ident("RUSTDOC_LINTS".to_owned())) {
        Some(at) => match (at..rustc.lib.len()).find(|&i| rustc.lib[i] == Tok::Punct('=')) {
            Some(eq) => {
                let open = (eq..rustc.lib.len()).find(|&i| rustc.lib[i] == Tok::Open('[')).unwrap_or(eq);
                rustc.lib[open..closing(&rustc.lib, open)]
                    .iter()
                    .filter_map(|it| match it {
                        Tok::Str(s) => Some(format!("{s:?}")),
                        _ => None,
                    })
                    .collect()
            }
            None => Vec::new(),
        },
        None => Vec::new(),
    };
    _ = writeln!(
        body,
        "\n/// `register_ignored`: rustdoc's lints of old, before they were `rustdoc::`.\n#[rustfmt::skip]\npub(super) const IGNORED: &[&str] = &[{}];",
        ignored.join(", ")
    );

    // `rustc::internal`: `store.register_group(false, \"rustc::internal\", None, vec![LintId::of(X), ..])`.
    let mut members = Vec::new();
    for i in 0..rustc.lib.len() {
        if rustc.lib[i] == Tok::Str("rustc::internal".to_owned()) {
            let Some(open) = (i..rustc.lib.len()).find(|&k| rustc.lib[k] == Tok::Open('[')) else { break };
            let end = closing(&rustc.lib, open);
            for k in open..end {
                if ident(rustc.lib.get(k)) == Some("of")
                    && let Some(lint) = ident(rustc.lib.get(k + 2))
                {
                    members.push(format!("{:?}", rustc.name_of(lint)));
                }
            }
            break;
        }
    }
    _ = writeln!(
        body,
        "\n/// The lints of the group `rustc::internal`, as `register_internals` lists them.\n#[rustfmt::skip]\npub(super) static INTERNAL_GROUP: &[&str] = &[{}];",
        members.join(", ")
    );

    let head = format!(
        "//! rustc's lints, groups and the renamed, removed and ignored names of its lint store, as\n\
         //! `register_builtins` and `register_internals` register them.\n\
         //!\n\
         //! Generated by scripts/src/bin/lint_table.rs from rust-lang/rust's compiler/rustc_lint,\n\
         //! compiler/rustc_lint_defs and compiler/rustc_feature (MIT or Apache-2.0) at the commit\n\
         //! library/UPSTREAM names. Do not edit: move the pin, then write it again\n\
         //! (`blaze run -q -p scripts --bin lint_table -- '{{\"command\": \"write\", \"rust\": \"<checkout>\"}}'`).\n\
         //! The gate holds the two lines below to the pin and to this file's text.\n\
         //!\n\
         //! commit {commit}\n"
    );
    let uses = "\nuse edition::Edition;\n\nuse super::{Level, Lint, Reason};\n";
    let rest = format!("{uses}{body}");
    format!("{head}//! digest {:016x}\n{rest}", fnv(&format!("{commit}\n{rest}")))
}

/// FNV-1a, 64 bits: the digest of the file's own text.
fn fnv(text: &str) -> u64 {
    text.bytes().fold(0xcbf2_9ce4_8422_2325, |hash, byte| (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3))
}

/// library/UPSTREAM's `commit` line.
fn pinned(root: &Path) -> String {
    let text = read(&root.join(UPSTREAM));
    match text.lines().find_map(|it| it.strip_prefix("commit ")) {
        Some(commit) => commit.trim().to_owned(),
        None => fail(SCRIPT, &format!("{UPSTREAM} has no `commit` line")),
    }
}

/// The table written from the checkout `rust`, which must be at the pinned commit.
fn generated(root: &Path, rust: &str) -> String {
    let rust = Path::new(rust);
    let commit = pinned(root);
    let head = std::process::Command::new("git").arg("-C").arg(rust).args(["rev-parse", "HEAD"]).output();
    let head = match head {
        Ok(out) if out.status.success() => String::from_utf8_lossy(&out.stdout).trim().to_owned(),
        _ => fail(SCRIPT, &format!("{} is not a git checkout (`git rev-parse HEAD` failed)", rust.display())),
    };
    if head != commit {
        fail(
            SCRIPT,
            &format!("{} is at {head}, and {UPSTREAM} pins {commit}: check that commit out", rust.display()),
        );
    }
    let mut rustc = Rustc::default();
    let mut files = Vec::new();
    sources(&rust.join("compiler/rustc_lint/src"), &mut files);
    sources(&rust.join("compiler/rustc_lint_defs/src"), &mut files);
    for file in &files {
        rustc.add(&read(file));
    }
    rustc.lib = tokens(&read(&rust.join("compiler/rustc_lint/src/lib.rs")));
    rustc.lint_vec("foreign_modules", &read(&rust.join("compiler/rustc_lint/src/foreign_modules.rs")));
    rustc.lint_vec("hardwired", &read(&rust.join("compiler/rustc_lint_defs/src/builtin.rs")));
    rustc.features(&read(&rust.join("compiler/rustc_feature/src/unstable.rs")));
    table(&rustc, &commit)
}

/// What is wrong with the file as it is, with no checkout: its commit and its digest.
fn problems(root: &Path) -> Vec<String> {
    let text = read(&root.join(OUT));
    let line = |key: &str| text.lines().find_map(|it| it.strip_prefix(&format!("//! {key} "))).map(str::to_owned);
    let mut out = Vec::new();
    let pin = pinned(root);
    let again = "write it again: `blaze run -q -p scripts --bin lint_table -- '{\"command\": \"write\", \"rust\": \"<a checkout of rust-lang/rust at that commit>\"}'`";
    match line("commit") {
        Some(commit) if commit == pin => {}
        Some(commit) => out.push(format!(
            "{OUT} was written from rustc {commit}, and {UPSTREAM} pins {pin}: the lints of the pin may differ; {again}"
        )),
        None => out.push(format!("{OUT} has no `//! commit` line; {again}")),
    }
    let rest = text.split_once("//! digest ").and_then(|(_, after)| after.split_once('\n'));
    match (line("commit"), line("digest"), rest) {
        (Some(commit), Some(digest), Some((_, rest))) => {
            if format!("{:016x}", fnv(&format!("{commit}\n{rest}"))) != digest {
                out.push(format!("{OUT} was edited by hand (its digest is not its text's); {again}"));
            }
        }
        _ => out.push(format!("{OUT} has no `//! digest` line; {again}")),
    }
    out
}

fn main_with(args: &Args) {
    let root = root();
    match (args.command.as_str(), &args.rust) {
        ("write", Some(rust)) => {
            let text = generated(&root, rust);
            if let Err(e) = std::fs::write(root.join(OUT), &text) {
                fail(SCRIPT, &format!("{OUT}: {e}"));
            }
            println!("{OUT}: written from rustc {}", pinned(&root));
        }
        ("write", None) => fail(SCRIPT, "`write` needs `rust`, a checkout of rust-lang/rust at the pinned commit"),
        (_, rust) => {
            let mut said = problems(&root);
            if let Some(rust) = rust
                && said.is_empty()
                && generated(&root, rust) != read(&root.join(OUT))
            {
                said.push(format!("{OUT} is not what rustc's source at {} gives: write it again", pinned(&root)));
            }
            if !said.is_empty() {
                for line in &said {
                    eprintln!("{SCRIPT}: {line}");
                }
                blaze_script::exit(1);
            }
            println!("{OUT}: rustc {}'s lints", pinned(&root));
        }
    }
}

const SAMPLE: &str = r###"
declare_lint! {
    /// The `a_lint` lint: "quoted", and a `declare_lint!` in a comment.
    pub A_LINT,
    Warn,
    "first \
     and second with `dyn Trait`",
    @future_incompatible = FutureIncompatibleInfo {
        reason: fcw!(EditionError 2024 "a-page"),
        report_in_deps: true,
        explain_reason: false,
    };
    @edition Edition2024 => Deny;
    report_in_external_macro
}
declare_lint! {
    pub(crate) B_LINT,
    Deny,
    r#"raw "text""#,
    @eval_always = true
    @feature_gate = some_gate;
    @future_incompatible = FutureIncompatibleInfo {
        reason: fcw!("in its own words" #12_345),
    };
    crate_level_only
}
declare_lint! { pub NEVER_REGISTERED, Allow, "declared only" }
declare_tool_lint! {
    pub rustc::A_TOOL_LINT,
    Allow,
    "of a tool",
    report_in_external_macro: true
}
declare_lint_pass!(First => [A_LINT]);
impl_lint_pass!(Second => [B_LINT, A_LINT]);
declare_lint_pass! {
    /// Documented.
    Third => [B_LINT]
}
declare_lint_pass!(Tool => [A_TOOL_LINT]);
early_lint_methods!(declare_combined_early_lint_pass, [pub BuiltinCombinedPreExpansionLintPass, []]);
early_lint_methods!(
    declare_combined_early_lint_pass,
    [pub BuiltinCombinedEarlyLintPass, [Second: Second::default(), First: First,]]
);
late_lint_methods!(declare_combined_late_lint_pass, [BuiltinCombinedLateLintModPass, []]);
early_lint_methods!(declare_combined_early_lint_pass, [InternalCombinedEarlyLintPass, [Tool: Tool]]);
late_lint_methods!(declare_combined_late_lint_pass, [InternalCombinedLateLintModPass, []]);
fn register_builtins(store: &mut LintStore) {
    macro_rules! add_lint_group {
        ($name:expr, $($lint:ident),*) => (store.register_group(false, $name, None, vec![$(LintId::of($lint)),*]);)
    }
    add_lint_group!("a_group", A_LINT, B_LINT);
    store.register_renamed("old", "a_lint");
    const RUSTDOC_LINTS: &[&str] = &["of_rustdoc"];
    store.register_group_alias("a_group", "an_alias");
    store.register_removed("gone", "converted into hard error, \
         see elsewhere");
    store.register_group(false, "rustc::internal", None, vec![LintId::of(A_TOOL_LINT)]);
}
"###;

fn selftest() {
    let mut rustc = Rustc::default();
    rustc.add(SAMPLE);
    rustc.lib = tokens(SAMPLE);
    rustc.lint_vec("foreign_modules", "pub(crate) fn lint_vec() -> LintVec { vec![] }");
    rustc.lint_vec("hardwired", "pub mod hardwired { pub fn lint_vec() -> crate::LintVec { vec![\n// a comment\nA_LINT,\n] } }");
    rustc.features("declare_features! ( (unstable, some_gate, \"1.0.0\", Some(77)), (internal, other, \"1.0.0\", None), );");
    let text = table(&rustc, "0000");
    let expected = [
        "//! commit 0000\n",
        "pub(super) const BUILTIN: &[Lint] = &[\n    Lint::new(\"b_lint\", Level::Deny, \"raw \\\"text\\\"\").fcw(Reason::Custom(\"in its own words\", 12345)).gate(\"some_gate\", Some(77)).crate_level().eval_always(),\n    // dyn: rustc's text of the lint, no trait object\n    Lint::new(\"a_lint\", Level::Warn, \"first and second with `dyn Trait`\").fcw_deps(Reason::EditionError(Edition::Edition2024, \"a-page\")).unexplained().edition(Edition::Edition2024, Level::Deny).external(),\n];\n",
        "pub(super) const UNREGISTERED: &[Lint] = &[\n    Lint::new(\"never_registered\", Level::Allow, \"declared only\"),\n];\n",
        "pub(super) const INTERNAL: &[Lint] = &[\n    Lint::new(\"rustc::a_tool_lint\", Level::Allow, \"of a tool\").external(),\n];\n",
        "pub(super) const GROUPS: &[(&str, &[&str])] = &[\n    (\"a_group\", &[\"a_lint\", \"b_lint\"]),\n];\n",
        "pub(super) const ALIASES: &[(&str, &str)] = &[\n    (\"an_alias\", \"a_group\"),\n];\n",
        "pub(super) const RENAMED: &[(&str, &str)] = &[\n    (\"old\", \"a_lint\"),\n];\n",
        "pub(super) const REMOVED: &[(&str, &str)] = &[\n    (\"gone\", \"converted into hard error, see elsewhere\"),\n];\n",
        "pub(super) const IGNORED: &[&str] = &[\"of_rustdoc\"];\n",
        "pub(super) static INTERNAL_GROUP: &[&str] = &[\"rustc::a_tool_lint\"];\n",
    ];
    for part in expected {
        if !text.contains(part) {
            fail(SCRIPT, &format!("selftest: the table of the sample has no\n{part}\nin\n{text}"));
        }
    }
    // The digest is of the text: one changed byte is another digest.
    if fnv("a\nb") == fnv("a\nc") {
        fail(SCRIPT, "selftest: two texts of one digest");
    }
    println!("{SCRIPT}: selftest passed");
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.as_slice() {
        [one] if one == "selftest" => selftest(),
        [one] => main_with(&input::parse::<Args>(one, SCRIPT)),
        _ => {
            eprintln!("usage: {SCRIPT} '<json>' | selftest");
            blaze_script::exit(1);
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn selftest_passes() {
        super::selftest();
    }
}
