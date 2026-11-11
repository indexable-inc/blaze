//! The compile-time checks: what makes an extension safe to load into the daemon.
//!
//! An extension runs natively in blaze's own process with no OS sandbox. What it can do is
//! bounded by what its source can say, so the checks are over the source: a crate that
//! passes can name nothing but `core` and `blaze-ext`, and can reach nothing either of them
//! does not hand a safe caller. The rules, each a [`Rule`]:
//!
//! | rule | refuses |
//! |---|---|
//! | `ext::deps` | a dependency other than `blaze-ext`; a build script; a proc-macro crate |
//! | `ext::api` | a `blaze-ext` requirement this blaze's surface does not meet |
//! | `ext::no-std` | a crate root without `#![no_std]`; a path through `std` |
//! | `ext::unsafe` | the keyword `unsafe`, anywhere |
//! | `ext::extern` | the keyword `extern`: a foreign block, a foreign ABI, `extern crate` |
//! | `ext::asm` | `asm!`, `global_asm!`, `naked_asm!` |
//! | `ext::global-state` | a `static`; `thread_local!` |
//! | `ext::attribute` | an attribute that names a symbol, a section or a runtime hook |
//! | `ext::feature` | a `#![feature(..)]` outside [`FEATURES`] |
//! | `ext::include` | `include!`, `include_str!`, `include_bytes!`, `#[path]` |
//! | `ext::entry` | a crate root without `pub fn extension` |
//!
//! They read tokens, not the tree after expansion, on purpose. A keyword is refused wherever
//! it is written: in an item, in a macro's definition, in a macro call's arguments, under a
//! `cfg` that is off. Rust has no way to make the token `unsafe` out of other tokens without
//! a proc macro, and an extension has none (`ext::deps`), so what no file writes no expansion
//! holds; the macros of `core` and of `blaze-ext` are blaze's own to vouch for. The price is
//! that an identifier spelled like a refused macro (`asm!` of the crate's own) is refused
//! too.
//!
//! When bstd lands the allowed dependencies gain it ([`ALLOWED`]) and a granted capability
//! becomes a field of the `Cx`; the rules themselves do not change.
//!
//! What this rests on: these checks and blaze's own front end being right. A hole in either
//! is arbitrary code in the daemon; there is no second wall.

use std::fmt;

use rustc_lexer::{FrontmatterAllowed, TokenKind};

use crate::{manifest::Declared, source::Sources};

/// The dependencies an extension may name. bstd joins it when it lands.
pub const ALLOWED: &[&str] = &["blaze-ext"];

/// The unstable features an extension may turn on: `allocator_api`, for `Vec::new_in` on its
/// arena. Every other gate is refused, since a gate can open what the other rules assume
/// shut (`rustc_attrs`, `linkage`, `core_intrinsics`).
pub const FEATURES: &[&str] = &["allocator_api"];

/// The attributes refused wherever they are written, `cfg_attr` included: each names a
/// symbol or a section the linker sees, or a hook the runtime calls.
const ATTRIBUTES: &[&str] = &[
    "no_mangle",
    "export_name",
    "link_name",
    "link_section",
    "link",
    "linkage",
    "used",
    "global_allocator",
    "panic_handler",
    "alloc_error_handler",
    "start",
    "no_main",
    "naked",
    "target_feature",
    "instruction_set",
    "proc_macro",
    "proc_macro_derive",
    "proc_macro_attribute",
];

/// Which check refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Rule {
    Deps,
    Api,
    NoStd,
    Unsafe,
    Extern,
    Asm,
    GlobalState,
    Attribute,
    Feature,
    Include,
    Entry,
    Grant,
}

impl Rule {
    /// As a refusal names it.
    pub const fn name(self) -> &'static str {
        match self {
            Rule::Deps => "ext::deps",
            Rule::Api => "ext::api",
            Rule::NoStd => "ext::no-std",
            Rule::Unsafe => "ext::unsafe",
            Rule::Extern => "ext::extern",
            Rule::Asm => "ext::asm",
            Rule::GlobalState => "ext::global-state",
            Rule::Attribute => "ext::attribute",
            Rule::Feature => "ext::feature",
            Rule::Include => "ext::include",
            Rule::Entry => "ext::entry",
            Rule::Grant => "ext::grant",
        }
    }
}

/// One thing of an extension a check refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Refusal {
    pub extension: String,
    /// The file, from the extension's directory.
    pub file: String,
    /// The line and column, from 1, when it is a place in a file.
    pub at: Option<(u32, u32)>,
    pub rule: Rule,
    /// What is there and why it is refused: it names the item.
    pub what: String,
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "extension `{}`: {}", self.extension, self.file)?;
        if let Some((line, column)) = self.at {
            write!(f, ":{line}:{column}")?;
        }
        write!(f, ": {} (rule `{}`)", self.what, self.rule.name())
    }
}

impl std::error::Error for Refusal {}

/// Every refusal of `sources`, in the order of its files and of each file's text; empty for
/// an extension that may be built.
pub fn check(sources: &Sources) -> Vec<Refusal> {
    let mut found = Vec::new(); // alloc: an extension's refusals, the error path
    manifest(sources, &mut found);
    match sources.file(ROOT) {
        Some(_) => {}
        None => found.push(Refusal {
            extension: sources.name.clone(), // clone: error path
            file: ROOT.to_owned(),           // alloc: error path
            at: None,
            rule: Rule::Entry,
            what: "the extension has no `src/lib.rs`: its crate root is there".to_owned(), // alloc: error path
        }),
    }
    for (path, text) in &sources.files {
        file(sources, path, text, &mut found);
    }
    found
}

/// The refusal of grants this blaze cannot hand out: every one, until bstd's capability types
/// exist to hand. The grant is in the build's key already, so nothing changes shape when one
/// can be granted.
pub fn grants(declared: &Declared) -> Vec<Refusal> {
    declared
        .grants
        .iter()
        .map(|grant| Refusal {
            extension: declared.name.clone(), // clone: error path
            file: "the workspace's Cargo.toml".to_owned(), // alloc: error path
            at: None,
            rule: Rule::Grant,
            what: format!(
                "it is granted `{grant}`, a capability this blaze cannot hand out yet: the capability types are \
                 bstd's (docs/v2/plan/design/std.md), and until they land an extension has compute, its arena \
                 and the diagnostics sink only"
            ),
        })
        .collect()
}

const ROOT: &str = "src/lib.rs";

fn manifest(sources: &Sources, found: &mut Vec<Refusal>) {
    let mut refuse = |rule: Rule, what: String| {
        found.push(Refusal {
            extension: sources.name.clone(), // clone: error path
            file: "Cargo.toml".to_owned(),   // alloc: error path
            at: None,
            rule,
            what,
        });
    };
    let table = match sources.manifest.parse::<toml::Table>() {
        Ok(it) => it,
        Err(error) => {
            refuse(Rule::Deps, format!("the manifest does not read: {}", error.message()));
            return;
        }
    };
    let mut requirement = None;
    for kind in ["dependencies", "dev-dependencies", "build-dependencies", "target"] {
        let Some(deps) = table.get(kind).and_then(|it| it.as_table()) else { continue };
        if kind != "dependencies" {
            if !deps.is_empty() {
                refuse(
                    Rule::Deps,
                    format!(
                        "`[{kind}]` is not empty: an extension's only dependency is `blaze-ext`, under `[dependencies]`"
                    ),
                );
            }
            continue;
        }
        for (name, of) in deps {
            if !ALLOWED.contains(&name.as_str()) {
                refuse(
                    Rule::Deps,
                    format!(
                        "the dependency `{name}`: an extension may depend only on {}, which blaze itself vouches for; \
                         code of another crate would run in the daemon unchecked",
                        ALLOWED.join(", ")
                    ),
                );
                continue;
            }
            requirement = match of {
                toml::Value::String(it) => Some(it.clone()), // clone: the requirement, once
                toml::Value::Table(it) => {
                    // clone: the requirement, once
                    it.get("version").and_then(|it| it.as_str()).map(str::to_owned)
                }
                _ => None,
            };
            if requirement.is_none() {
                refuse(
                    Rule::Api,
                    format!(
                        "the dependency `blaze-ext` states no version: say which surface the source was written for, \
                         `blaze-ext = \"{}\"`",
                        minor(blaze_ext::VERSION)
                    ),
                );
            }
        }
    }
    let package = table.get("package").and_then(|it| it.as_table());
    if package.is_some_and(|it| it.get("build").is_some_and(|it| it.as_bool() != Some(false))) {
        refuse(
            Rule::Deps,
            "`package.build` names a build script, which an extension may not have".to_owned(),
        ); // alloc: error path
    }
    let lib = table.get("lib").and_then(|it| it.as_table());
    if lib.is_some_and(|it| it.get("proc-macro").and_then(|it| it.as_bool()) == Some(true)) {
        refuse(Rule::Deps, "`lib.proc-macro` is set: an extension is not a proc macro".to_owned()); // alloc: error path
    }
    if lib.is_some_and(|it| it.get("path").is_some()) {
        refuse(
            Rule::Entry,
            "`lib.path` is set: an extension's crate root is `src/lib.rs`".to_owned(),
        ); // alloc: error path
    }
    match requirement {
        Some(written) if !meets(&written, blaze_ext::VERSION) => {
            let changed = changes_since(&written);
            refuse(
                Rule::Api,
                format!(
                    "it is written for blaze-ext {written}, and this blaze has blaze-ext {}: the API is not stable \
                     across blaze's versions, so its source may need edits{changed}",
                    blaze_ext::VERSION
                ),
            );
        }
        Some(_) => {}
        None if !found_requirement_refusal(&table) => refuse(
            Rule::Api,
            format!(
                "it does not depend on `blaze-ext`: an extension states the surface it was written for, \
                 `blaze-ext = \"{}\"`",
                minor(blaze_ext::VERSION)
            ),
        ),
        None => {}
    }
}

/// Whether the manifest names `blaze-ext` at all: one without a version was refused where it
/// was read, and is not refused again for being missing.
fn found_requirement_refusal(table: &toml::Table) -> bool {
    table
        .get("dependencies")
        .and_then(|it| it.as_table())
        .is_some_and(|it| it.contains_key("blaze-ext"))
}

/// `major.minor` of a version.
fn minor(version: &str) -> String {
    let (major, minor, _) = numbers(version);
    format!("{major}.{minor}")
}

/// The numbers of a version or of a requirement (`0.1`, `=0.1.0`, `^0.1`), absent parts 0.
fn numbers(version: &str) -> (u64, u64, u64) {
    let mut parts = version
        .trim()
        .trim_start_matches(['=', '^', '~', ' '])
        .split('.')
        .map(|it| it.trim().parse::<u64>().unwrap_or(0));
    (parts.next().unwrap_or(0), parts.next().unwrap_or(0), parts.next().unwrap_or(0))
}

/// Whether the surface `have` is the one `written` asks for: the same major and minor. The
/// surface is unstable by design, so nothing newer is taken as compatible.
fn meets(written: &str, have: &str) -> bool {
    let (written, have) = (numbers(written), numbers(have));
    (written.0, written.1) == (have.0, have.1)
}

/// What the surface changed after the version `written` asks for, as a refusal's tail.
fn changes_since(written: &str) -> String {
    let from = numbers(written);
    let mut out = String::new(); // alloc: error path
    for (version, items) in blaze_ext::CHANGES {
        if numbers(version) <= from {
            continue;
        }
        for item in *items {
            out.push_str(if out.is_empty() { "; changed since: " } else { "; " });
            out.push_str(version);
            out.push_str(": ");
            out.push_str(item);
        }
    }
    out
}

/// One token that is not space or a comment.
#[derive(Clone, Copy)]
struct Token<'t> {
    kind: TokenKind,
    text: &'t str,
    at: u32,
}

impl Token<'_> {
    /// The word it is, when it is an identifier or a keyword: a raw identifier (`r#unsafe`)
    /// is a name, never the keyword, and a forced keyword (`k#unsafe`) is the keyword.
    fn word(&self) -> Option<&str> {
        match self.kind {
            TokenKind::Ident => Some(self.text),
            TokenKind::ForcedKeywordIdent => Some(self.text.trim_start_matches("k#")),
            _ => None,
        }
    }
}

fn tokens(text: &str) -> Vec<Token<'_>> {
    let mut at = 0u32;
    let mut out = Vec::new(); // alloc: one file's tokens, once per check of an extension
    for token in rustc_lexer::tokenize(text, FrontmatterAllowed::No) {
        let start = at;
        at += token.len;
        let skipped = matches!(
            token.kind,
            TokenKind::Whitespace | TokenKind::LineComment { .. } | TokenKind::BlockComment { .. }
        );
        if !skipped {
            out.push(Token {
                kind: token.kind,
                text: &text[start as usize..at as usize],
                at: start,
            });
        }
    }
    out
}

/// The line and column, from 1, of the byte `at` of `text`.
fn place(text: &str, at: u32) -> (u32, u32) {
    let before = &text[..at as usize];
    let line = before.bytes().filter(|it| *it == b'\n').count() as u32 + 1;
    let column = before.rfind('\n').map_or(before.len(), |it| before.len() - it - 1) as u32 + 1;
    (line, column)
}

fn file(sources: &Sources, path: &str, text: &str, found: &mut Vec<Refusal>) {
    let tokens = tokens(text);
    let mut refuse = |at: u32, rule: Rule, what: String| {
        found.push(Refusal {
            extension: sources.name.clone(), // clone: error path
            file: path.to_owned(),           // alloc: error path
            at: Some(place(text, at)),
            rule,
            what,
        });
    };
    let is = |at: usize, kind: TokenKind| tokens.get(at).is_some_and(|it| it.kind == kind);
    let root = path == ROOT;
    let mut no_std = false;
    let mut entry = false;
    let mut depth = 0u32;
    let mut at = 0;
    while at < tokens.len() {
        let token = tokens[at];
        match token.kind {
            TokenKind::OpenBrace => depth += 1,
            TokenKind::CloseBrace => depth = depth.saturating_sub(1),
            TokenKind::Pound => {
                let inner = is(at + 1, TokenKind::Bang);
                let open = at + 1 + usize::from(inner);
                if is(open, TokenKind::OpenBracket) {
                    let close = closing(&tokens, open);
                    let attribute = &tokens[open + 1..close.min(tokens.len())];
                    if root
                        && inner
                        && depth == 0
                        && matches!(attribute, [it] if it.word() == Some("no_std"))
                    {
                        no_std = true;
                    }
                    self::attribute(attribute, &mut refuse);
                    // The attribute's own tokens are read below too: a keyword in one is a
                    // keyword.
                }
            }
            _ => {}
        }
        let Some(word) = token.word() else {
            at += 1;
            continue;
        };
        let bang = is(at + 1, TokenKind::Bang);
        match word {
            "unsafe" => refuse(
                token.at,
                Rule::Unsafe,
                "`unsafe`: an extension is safe Rust only; what needs `unsafe` belongs behind `blaze-ext`".to_owned(), // alloc: error path
            ),
            "extern" => {
                let next = tokens.get(at + 1).and_then(Token::word);
                let what = match next {
                    Some("crate") => {
                        let named = tokens.get(at + 2).map_or("", |it| it.text);
                        format!(
                            "`extern crate {named}`: an extension names no crate but `core` and `blaze_ext`, which need no \
                             `extern crate`; collections come from its arena (`cx.vec()`), never from `alloc`'s global heap"
                        )
                    }
                    _ => "`extern`: an extension declares no foreign function, block or ABI".to_owned(), // alloc: error path
                };
                refuse(token.at, Rule::Extern, what);
            }
            "static" => refuse(
                token.at,
                Rule::GlobalState,
                "`static`: an extension keeps no state between jobs, so two runs give one result; use a `const`".to_owned(), // alloc: error path
            ),
            "thread_local" if bang => refuse(
                token.at,
                Rule::GlobalState,
                "`thread_local!`: an extension keeps no state between jobs".to_owned(), // alloc: error path
            ),
            "asm" | "global_asm" | "naked_asm" if bang => {
                refuse(token.at, Rule::Asm, format!("`{word}!`: an extension has no inline assembly"));
            }
            "include" | "include_str" | "include_bytes" if bang => refuse(
                token.at,
                Rule::Include,
                format!("`{word}!`: an extension reads no file, when compiled or when run; its source is its `src`"),
            ),
            "std" if is(at + 1, TokenKind::Colon) && is(at + 2, TokenKind::Colon) => refuse(
                token.at,
                Rule::NoStd,
                "a path through `std`: an extension may not use `std`, whose functions reach files, sockets, the clock \
                 and the environment with no capability"
                    .to_owned(), // alloc: error path
            ),
            "extension"
                if root
                    && depth == 0
                    && at >= 2
                    && tokens[at - 1].word() == Some("fn")
                    && tokens[at - 2].word() == Some("pub") =>
            {
                entry = true;
            }
            _ => {}
        }
        at += 1;
    }
    if root && !no_std {
        refuse(
            0,
            Rule::NoStd,
            "the crate root has no `#![no_std]`: an extension may not use `std`".to_owned(), // alloc: error path
        );
    }
    if root && !entry {
        refuse(
            0,
            Rule::Entry,
            "the crate root has no `pub fn extension() -> blaze_ext::Extension`: what blaze calls to learn its lints and facts"
                .to_owned(), // alloc: error path
        );
    }
}

/// The index of the bracket that closes the one at `open`, or the end.
fn closing(tokens: &[Token<'_>], open: usize) -> usize {
    let mut depth = 0u32;
    for (at, token) in tokens.iter().enumerate().skip(open) {
        match token.kind {
            TokenKind::OpenBracket => depth += 1,
            TokenKind::CloseBracket => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return at;
                }
            }
            _ => {}
        }
    }
    tokens.len()
}

/// The refusals of one attribute's tokens, between its brackets.
fn attribute(tokens: &[Token<'_>], refuse: &mut impl FnMut(u32, Rule, String)) {
    for (at, token) in tokens.iter().enumerate() {
        let Some(word) = token.word() else { continue };
        let next = tokens.get(at + 1).map(|it| it.kind);
        if ATTRIBUTES.contains(&word) {
            refuse(
                token.at,
                Rule::Attribute,
                format!(
                    "the attribute `{word}`: it names a symbol, a section or a runtime hook, which an extension has none of"
                ),
            );
        }
        if word == "path" && at == 0 && next == Some(TokenKind::Eq) {
            refuse(
                token.at,
                Rule::Include,
                "`#[path]`: an extension's modules are the files of its `src`, where the checks and the key read them".to_owned(), // alloc: error path
            );
        }
        if word == "feature" && next == Some(TokenKind::OpenParen) {
            for feature in tokens[at + 2..].iter().take_while(|it| it.kind != TokenKind::CloseParen)
            {
                let Some(name) = feature.word() else { continue };
                if !FEATURES.contains(&name) {
                    refuse(
                        feature.at,
                        Rule::Feature,
                        format!(
                            "the feature `{name}`: an extension may turn on only {}; a gate can open what the other \
                             rules keep shut",
                            FEATURES.join(", ")
                        ),
                    );
                }
            }
        }
    }
}
