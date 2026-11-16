//! tobstd: moves one crate from Rust's `std` onto bstd and blaze-own by the rows of
//! tools/std-map.json, editing Rust only through ast-grep with rules it writes from the table
//! (AGENTS.md rule 72, edit-rust-by-syntax). It is dumb on purpose: it resolves no names and
//! checks no types; where a step needs either it stops and names the place (a wall). `DOC`
//! below is what its usage error prints.
//!
//!     blaze run -q -p scripts --bin tobstd -- '{"crate": "crates/x", "mode": "rename"}'
//!     blaze run -q -p scripts --bin tobstd -- '{"crate": "crates/x", "mode": "add-arg"}'
//!     blaze run -q -p scripts --bin tobstd -- '{"crate": "crates/x", "mode": "settle", "steps": 12}'
//!     blaze run -q -p scripts --bin tobstd -- selftest
//!
//! Why (2026-11-15): blaze's own code is to use no `std` and no `alloc`
//! (docs/v2/plan/design/bottom.md), and most of the move is mechanical: tools/std-map.json
//! counts what the tree takes from `std` by class, and the `rename` and `add-arg` rows are a
//! rewrite plus an allocator threaded down the calls. The threading needs no type checker of
//! ours: the compiler's E0061 ("this function takes N arguments but N-1 were supplied") names
//! each call that lacks the new argument, so `settle` adds it there and to the caller's
//! signature, and repeats. The same loop threads a capability (`fs`, `net`, `clock`) when the
//! input names it in place of `alloc`.

/// What the usage error prints.
const DOC: &str = r#"tobstd: moves one crate from `std` onto bstd and blaze-own by tools/std-map.json, editing only through ast-grep (AGENTS.md rule 72).

    blaze run -q -p scripts --bin tobstd -- '{"crate": "<dir>", "mode": "rename"|"add-arg"|"settle"|"share-clone", ...}'
    blaze run -q -p scripts --bin tobstd -- selftest

    crate     the crate's directory, from `root` (its Cargo.toml and every `.rs` under it but
              `target`)
    mode      rename   every `rename` row whose replacement's crate is in `roots`, written as a
                       path (inline and in a `use` whose last segment stays the same); a path in
                       a `use` group (`use std::{fmt, mem}`) is not one ast-grep sees, and stays
              add-arg  every `add-arg` row whose shape needs no type: a call `T::f(..)` of a
                       prelude type, a macro (`format!`, `vec!`), a method whose target the text
                       pins (`.collect::<Vec<_>>()`, a string literal's `.to_string()`), and,
                       with value "alloc", the prelude collection types (`Vec<T>` to
                       `blaze_own::Vec<T, blaze_own::Alloc<'_>>`) in the functions that gain
                       the parameter. Each rewritten site takes `value`; a function that has no
                       parameter of that name gains `<value>: <type>` (and `generic`, if one is
                       given). A struct, enum or union whose fields name such a type is a wall
                       `needs-lifetime`: type definitions are not rewritten
              settle   `cargo check --message-format=json --all-targets` on the crate (through
                       `cargo`); each E0061 at a call of a function with a `value` parameter gets
                       `value` added (the call's function gaining the parameter as above), and
                       again, until no such error is left or `steps` checks have run
              share-clone
                       the `clone` split, with the compiler as the classifier: every
                       `$R.clone()` of a file that has a top-level `use` becomes `$R.share()`
                       (the file gaining `use blaze_own::Share as _;`), then settle's loop,
                       which also reads each E0599 at a `.share()`: a receiver that is a
                       blaze-own collection over `Alloc` (its name the last segment of a type
                       the table moves to blaze_own, or that with `A` before it) becomes
                       `$R.clone_in(<value>)`, threading `value` as add-arg does; any other is
                       `$R.clone()` again and a wall `kept-clone` naming its type
    steps     settle's most checks (8)
    value     the name threaded ("alloc"; "fs", "net", "clock" for a capability)
    type      the parameter's type ("blaze_own::Alloc<'_>", a one-word `Copy` handle; "&bstd::fs::Fs"
              for a capability)
    generic   a generic a function gains with the parameter (null: none, for a concrete `type`;
              "A: core::alloc::Allocator + Copy" with type "A" for a generic allocator)
    entry     what `main` and a test pass in place of gaining the parameter
              ("blaze_own::Alloc::global()"), or null: then each is a wall `entry`
    roots     the crates a replacement may name (["core", "blaze_own"]): a row whose
              replacement is elsewhere is skipped, and the summary says so
    cargo     the cargo to run ("/Volumes/Projects/blaze-int-1112-notes/wtcargo.sh")
    root      the checkout (".")

Where a site's value would come from:
  - a parameter of its function named `value`: that one;
  - `main`, or a function after `#[test]` (or an attribute whose path ends in `test`): `entry`;
  - a method with `self` of an inherent impl whose type has a field of type `Alloc<..>` or of a
    type parameter bounded by `Allocator`: `self.<field>` (with value "alloc" only);
  - else its function gains the parameter.
A wall is listed with its file and line, and nothing there is changed:
  trait-method  the site is in a method of a trait impl, whose signature the trait fixes
                (`Default::default`, `From::from`, `FromStr::from_str`, `FromIterator`,
                any foreign trait); its trait is named
  drop          the same, in `Drop::drop`
  static-init   in a `static` or `const` initializer
  outside       in no function (a closure in a `static`, an array length)
  ambiguous     its function's name names two functions of its file, which ast-grep's rules,
                selecting by name, cannot tell apart
  name-taken    its function already has a generic named as `generic` (only with a `generic`)
  needs-lifetime
                a type definition's field names a collection that moves to blaze-own: the type
                gains a lifetime `'a` and its fields `Alloc<'a>`, by a person or a lane
  fn-pointer    a function that gained the parameter is named as a value (an argument: a
                function pointer, or a closure's stand-in). With a concrete `type` the pointer
                can take the argument too when every site that builds its type is in the crate;
                that is listed, not attempted
  entry         `main` or a test, with `entry` null
  error         (settle) a compile error that is not an E0061 (or, in share-clone, an E0599 at
                a `.share()`) this tool can act on
  unmatched     (settle) an E0061 or `.share()` E0599 that no rule matched (inside a macro's
                tokens, or a shape the rules do not know); settle stops when a pass changes
                nothing
  no-use        (share-clone) a file with `.clone()` calls and no top-level `use`, where the
                trait's import has no place: its calls stay
  kept-clone    (share-clone) a `.share()` whose receiver is not a blaze-own collection (a
                foreign type, a type parameter, a reference to one): `.clone()` again
  not-in-scope  (share-clone) a `.share()` in a module that does not see the file's import (an
                inline `mod` with no `use super::*`): `.clone()` again
  steps         (settle) the step limit was reached with E0061s left
The summary (standard output, JSON): sites converted per row, rows skipped and why, the
functions that gained the parameter, the walls by kind, and settle's checks run.

It needs `ast-grep` on PATH (0.45 or later), and for `settle` the cargo given.
"#;

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    path::{Path, PathBuf},
    time::Duration,
};

use blaze_script::{
    TempDir,
    child::{Ended, run_logged},
    fail,
    input::{self, Fields, Input, InputError},
    pyjson,
};

const SCRIPT: &str = "tobstd";
/// The table, from the root.
const TABLE: &str = "tools/std-map.json";
/// How long one ast-grep run may take.
const AST_GREP_LIMIT: Duration = Duration::from_secs(300);
/// How long one `cargo check` may take.
const CARGO_LIMIT: Duration = Duration::from_secs(1800);
/// How many times one rule file is applied until it changes nothing: nested sites
/// (`format!("{}", format!(..))`, `Vec<Vec<u8>>`) take one run per level.
const PASSES: usize = 6;

struct Args {
    krate: String,
    mode: Mode,
    steps: usize,
    value: String,
    ty: String,
    generic: Option<String>,
    entry: Option<String>,
    roots: Vec<String>,
    cargo: String,
    root: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Rename,
    AddArg,
    Settle,
    ShareClone,
}

impl Mode {
    const fn name(self) -> &'static str {
        match self {
            Mode::Rename => "rename",
            Mode::AddArg => "add-arg",
            Mode::Settle => "settle",
            Mode::ShareClone => "share-clone",
        }
    }
}

/// The allocator's parameter type: one concrete `Copy` handle, so a function gains no generic.
const ALLOC_TYPE: &str = "blaze_own::Alloc<'_>";
/// What `main` and a test pass.
const ALLOC_ENTRY: &str = "blaze_own::Alloc::global()";
/// The trait whose `share` a `.clone()` becomes, imported by share-clone.
const SHARE_USE: &str = "use blaze_own::Share as _;";

impl Input for Args {
    const FIELDS: &'static [&'static str] =
        &["crate", "mode", "steps", "value", "type", "generic", "entry", "roots", "cargo", "root"];

    fn parse(f: &Fields<'_>) -> Result<Args, InputError> {
        let mode = match f.one_of("mode", &["rename", "add-arg", "settle", "share-clone"], None)?.as_str() {
            "rename" => Mode::Rename,
            "add-arg" => Mode::AddArg,
            "settle" => Mode::Settle,
            _ => Mode::ShareClone,
        };
        let steps = f.int("steps", Some(8))?;
        let steps = usize::try_from(steps).map_err(|_| InputError(format!("{} is {steps}: a count of checks", f.at("steps"))))?;
        let value = f.str("value", Some("alloc"))?;
        if ident_len(value.as_bytes()) != value.len() || value.is_empty() {
            return Err(InputError(format!("{} is {value:?}: a parameter's name", f.at("value"))));
        }
        // `null` and absent differ for `entry`: absent is the allocator's default.
        let entry = match f.get("entry") {
            None => Some(ALLOC_ENTRY.to_owned()),
            Some(_) => f.str_or_null("entry")?,
        };
        let generic = match f.get("generic") {
            None => None,
            Some(_) => f.str_or_null("generic")?,
        };
        let roots = f.strs("roots")?;
        Ok(Args {
            krate: f.str("crate", None)?,
            mode,
            steps,
            value,
            ty: f.str("type", Some(ALLOC_TYPE))?,
            generic,
            entry,
            roots: if roots.is_empty() { vec!["core".to_owned(), "blaze_own".to_owned()] } else { roots },
            cargo: f.str("cargo", Some("/Volumes/Projects/blaze-int-1112-notes/wtcargo.sh"))?,
            root: f.str("root", Some("."))?,
        })
    }
}

/// Why the tool stopped before it could say what it did.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ToolError {
    /// The table at its path does not read: what of it.
    Table { what: String },
    /// The crate's directory, or its manifest, is not there.
    NoCrate { path: String },
    /// A file could not be read or written: which, and the system's message.
    File { path: String, why: String },
    /// A program could not be run: its name and why.
    Run { program: String, why: String },
    /// A program ran past its limit.
    TimedOut { program: String, after: Duration },
    /// A program ended with a status that says it failed: its name, the status and its error
    /// output's last line.
    Failed { program: String, status: i32, last: String },
    /// A line of a program's JSON output is not what this tool reads: the program and the line.
    Output { program: String, line: String },
}

impl fmt::Display for ToolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ToolError::Table { what } => write!(f, "{TABLE}: {what}"),
            ToolError::NoCrate { path } => write!(f, "{path} is no crate (no Cargo.toml with a `[package]` name there)"),
            ToolError::File { path, why } => write!(f, "{path}: {why}"),
            ToolError::Run { program, why } => write!(f, "{program} could not be run: {why}"),
            ToolError::TimedOut { program, after } => write!(f, "{program} was still running after {after:?}: a hang is a bug to report"),
            ToolError::Failed { program, status, last } => write!(f, "{program} failed with status {status}: {last}"),
            ToolError::Output { program, line } => write!(f, "{program} wrote a line this tool does not read: {line:.200}"),
        }
    }
}

impl std::error::Error for ToolError {}

/// One row of the table, the fields this tool reads.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Row {
    path: String,
    written: String,
    form: String,
    kind: String,
    replacement: Option<String>,
    class: String,
}

fn read_table(text: &str) -> Result<Vec<Row>, ToolError> {
    let bad = |what: &str| ToolError::Table { what: what.to_owned() };
    let value = input::read(text).map_err(|e| bad(&e.0))?;
    let rows = value.get("rows").and_then(|it| it.as_array()).ok_or_else(|| bad("no `rows` array"))?;
    let mut out = Vec::new();
    for (at, row) in rows.iter().enumerate() {
        let field = |name: &str| -> Result<String, ToolError> {
            row.get(name).and_then(|it| it.as_str()).map(str::to_owned).ok_or_else(|| bad(&format!("row {at} has no string `{name}`")))
        };
        out.push(Row {
            path: field("path")?,
            written: field("written")?,
            form: field("form")?,
            kind: field("kind")?,
            replacement: row.get("replacement").and_then(|it| it.as_str()).map(str::to_owned),
            class: field("class")?,
        });
    }
    Ok(out)
}

/// A JSON value this tool writes: ast-grep's rules (YAML reads JSON) and the summary.
#[derive(Debug, Clone)]
enum J {
    S(String),
    N(usize),
    I(i64),
    A(Vec<J>),
    O(Vec<(String, J)>),
}

fn s(text: &str) -> J {
    J::S(text.to_owned())
}

fn o(fields: &[(&str, J)]) -> J {
    J::O(fields.iter().map(|(k, v)| ((*k).to_owned(), v.clone())).collect())
}

impl J {
    fn write(&self, out: &mut String) {
        match self {
            J::S(text) => pyjson::string(out, text, false),
            J::N(n) => out.push_str(&n.to_string()),
            J::I(n) => out.push_str(&n.to_string()),
            J::A(items) => {
                out.push('[');
                for (at, it) in items.iter().enumerate() {
                    if at > 0 {
                        out.push_str(", ");
                    }
                    it.write(out);
                }
                out.push(']');
            }
            J::O(fields) => {
                out.push('{');
                for (at, (k, v)) in fields.iter().enumerate() {
                    if at > 0 {
                        out.push_str(", ");
                    }
                    pyjson::string(out, k, false);
                    out.push_str(": ");
                    v.write(out);
                }
                out.push('}');
            }
        }
    }

    fn text(&self) -> String {
        let mut out = String::new();
        self.write(&mut out);
        out
    }
}

/// One ast-grep rule: its id, its `rule` object, its `constraints`, its `fix` (none for a
/// query) and its `transform` (metavariables the fix reads, made from others).
#[derive(Debug, Clone)]
struct Rule {
    id: String,
    rule: J,
    constraints: Option<J>,
    fix: Option<String>,
    transform: Option<J>,
}

/// A rule file: documents separated by `---`, each a JSON object.
fn rule_file(rules: &[Rule]) -> String {
    let docs: Vec<String> = rules
        .iter()
        .map(|r| {
            let mut fields = vec![("id".to_owned(), s(&r.id)), ("language".to_owned(), s("rust")), ("rule".to_owned(), r.rule.clone())];
            if let Some(c) = &r.constraints {
                fields.push(("constraints".to_owned(), c.clone()));
            }
            if let Some(t) = &r.transform {
                fields.push(("transform".to_owned(), t.clone()));
            }
            if let Some(fix) = &r.fix {
                fields.push(("fix".to_owned(), s(fix)));
            }
            J::O(fields).text()
        })
        .collect();
    docs.join("\n---\n") + "\n"
}

/// `^(a|b)$` for names that are identifiers.
fn names_regex(names: &BTreeSet<String>) -> String {
    let names: Vec<&str> = names.iter().map(String::as_str).collect();
    format!("^({})$", names.join("|"))
}

/// The condition that a node is inside a function named one of `names`.
/// Not inside a `static` or `const` in it, whose initializer is a wall.
fn inside_fns(names: &BTreeSet<String>) -> J {
    let in_fn = o(&[("inside", o(&[("stopBy", s("end")), ("kind", s("function_item")), ("has", o(&[("field", s("name")), ("regex", s(&names_regex(names)))]))]))]);
    let item = o(&[("stopBy", s("end")), ("any", J::A(vec![o(&[("kind", s("static_item"))]), o(&[("kind", s("const_item"))])]))]);
    all(in_fn, o(&[("not", o(&[("inside", item)]))]))
}

/// `rule` and `extra` both.
fn all(rule: J, extra: J) -> J {
    o(&[("all", J::A(vec![rule, extra]))])
}

/// The length of the identifier `bytes` starts with.
fn ident_len(bytes: &[u8]) -> usize {
    bytes.iter().take_while(|b| b.is_ascii_alphanumeric() || **b == b'_').count()
}

/// Whether `text` is a path of identifiers (`core::mem::swap`).
fn plain_path(text: &str) -> bool {
    !text.is_empty() && text.split("::").all(|seg| !seg.is_empty() && ident_len(seg.as_bytes()) == seg.len())
}

/// The crate a path names first.
fn root_of(path: &str) -> &str {
    path.split("::").next().unwrap_or(path)
}

/// A rewrite of one shape, written from a row: an ast-grep pattern (or rule), its constraints,
/// and its fix with `{V}` where the threaded value goes and `{T}` where the parameter's type goes.
#[derive(Debug, Clone)]
struct Shape {
    /// The row's `written`, which names it in the summary.
    row: String,
    rule: J,
    constraints: Option<J>,
    fix: String,
    /// A type's rewrite, applied only in functions that gain the parameter.
    is_type: bool,
}

/// The rename rules of `rows`, for replacements whose crate is in `roots`, and the rows skipped
/// with why.
fn rename_shapes(rows: &[Row], roots: &[String]) -> (Vec<Shape>, BTreeMap<String, String>) {
    let mut shapes = Vec::new();
    let mut skipped = BTreeMap::new();
    for row in rows.iter().filter(|r| r.class == "rename") {
        let Some(to) = &row.replacement else { continue };
        if row.form != "path" {
            skipped.insert(row.written.clone(), "a prelude name or a method: the rename is its type's import, which add-arg's types make".to_owned());
            continue;
        }
        if !plain_path(to) || !plain_path(&row.written) {
            skipped.insert(row.written.clone(), format!("`{to}` is not a plain path"));
            continue;
        }
        if !roots.iter().any(|it| it == root_of(to)) {
            skipped.insert(row.written.clone(), format!("`{}` is not in `roots`", root_of(to)));
            continue;
        }
        let from = &row.written;
        // The path node itself, whole (a pattern `std::fmt` matches no `use std::fmt;`): as an
        // expression, a type, a macro's name, or the path of a longer one (`std::fmt` in
        // `std::fmt::Debug`, whose items a rename keeps). A path in a `use` group
        // (`use std::{fmt, mem}`) is written in pieces and is not matched.
        let path = o(&[
            ("any", J::A(vec![o(&[("kind", s("scoped_identifier"))]), o(&[("kind", s("scoped_type_identifier"))])])),
            ("regex", s(&format!("^{from}$"))),
        ]);
        let in_use = o(&[("inside", o(&[("stopBy", s("end")), ("kind", s("use_declaration"))]))]);
        // Inline: everywhere but a `use`.
        shapes.push(Shape { row: from.clone(), rule: all(path.clone(), o(&[("not", in_use.clone())])), constraints: None, fix: to.clone(), is_type: false });
        // In a `use` only when the name it binds stays: `use std::mem::swap;` to `core::mem::swap`.
        if from.rsplit("::").next() == to.rsplit("::").next() {
            shapes.push(Shape { row: from.clone(), rule: all(path, in_use), constraints: None, fix: to.clone(), is_type: false });
        }
    }
    (shapes, skipped)
}

/// The arguments of a call template `p(a, b)`: `p` and `[a, b]`, split at top-level commas.
fn call_parts(text: &str) -> Option<(&str, Vec<&str>)> {
    let open = text.find('(')?;
    let inner = text[open + 1..].strip_suffix(')')?;
    let mut args = Vec::new();
    let (mut depth, mut from) = (0usize, 0);
    for (at, b) in inner.bytes().enumerate() {
        match b {
            b'(' | b'[' | b'{' | b'<' => depth += 1,
            b')' | b']' | b'}' | b'>' => depth = depth.saturating_sub(1),
            b',' if depth == 0 => {
                args.push(inner[from..at].trim());
                from = at + 1;
            }
            _ => {}
        }
    }
    if !inner.trim().is_empty() {
        args.push(inner[from..].trim());
    }
    Some((&text[..open], args))
}

/// The add-arg rules of `rows` (calls, macros, methods whose target the text pins, and with
/// `types`, the collection types), and the rows skipped with why.
fn add_arg_shapes(rows: &[Row], roots: &[String], types: bool) -> (Vec<Shape>, BTreeMap<String, String>) {
    let mut shapes = Vec::new();
    let mut skipped = BTreeMap::new();
    let mut skip = |row: &Row, why: &str| {
        skipped.insert(row.written.clone(), why.to_owned());
    };
    for row in rows.iter().filter(|r| r.class == "add-arg") {
        let Some(to) = row.replacement.as_deref() else {
            skip(row, "no replacement");
            continue;
        };
        if !roots.iter().any(|it| it == root_of(to)) {
            skip(row, &format!("`{}` is not in `roots`", root_of(to)));
            continue;
        }
        let shape = match (row.form.as_str(), row.kind.as_str()) {
            ("prelude", "function") => call_shape(row, to),
            ("macro", _) => macro_shape(row, to),
            ("method", _) => method_shape(row, to),
            (_, "type") if types => type_shape(row, to),
            (_, "type") => Err("a type gains its allocator only with value `alloc`".to_owned()),
            _ => Err(format!("a {} written as a {}: no shape this tool writes", row.kind, row.form)),
        };
        match shape {
            Ok(shape) => shapes.push(shape),
            Err(why) => skip(row, &why),
        }
    }
    (shapes, skipped)
}

/// `T::f(a)` to `P(a, alloc)`, from the template's argument names: each that is not `alloc`
/// is the call's next argument.
fn call_shape(row: &Row, to: &str) -> Result<Shape, String> {
    let (path, args) = call_parts(to).ok_or_else(|| format!("`{to}` is not a call"))?;
    let mut fix_args = Vec::new();
    let mut given = 0;
    for arg in &args {
        if *arg == "alloc" {
            fix_args.push("{V}".to_owned());
        } else {
            fix_args.push(format!("$A{given}"));
            given += 1;
        }
    }
    if !args.contains(&"alloc") {
        return Err(format!("`{to}` takes no `alloc`"));
    }
    let pattern_args: Vec<String> = (0..given).map(|n| format!("$A{n}")).collect();
    Ok(Shape {
        row: row.written.clone(),
        rule: o(&[("pattern", s(&format!("{}({})", row.written, pattern_args.join(", "))))]),
        constraints: None,
        fix: format!("{path}({})", fix_args.join(", ")),
        is_type: false,
    })
}

/// `m!(..)` to `P!(alloc, ..)` or `m![..]` to `P![alloc; ..]`: `..` is the call's tokens.
fn macro_shape(row: &Row, to: &str) -> Result<Shape, String> {
    let name = row.written.strip_suffix('!').ok_or_else(|| format!("`{}` is not a macro", row.written))?;
    let bang = to.find('!').ok_or_else(|| format!("`{to}` is not a macro"))?;
    let (open, close) = match to.as_bytes().get(bang + 1) {
        Some(b'(') => ('(', ')'),
        Some(b'[') => ('[', ']'),
        _ => return Err(format!("`{to}`'s delimiter is not `(` or `[`")),
    };
    let inner = to[bang + 2..].strip_suffix(close).ok_or_else(|| format!("`{to}` does not close its `{open}`"))?;
    if !inner.contains("alloc") || !inner.contains("..") {
        return Err(format!("`{to}` is not `P!(alloc, ..)`"));
    }
    let inner = inner.replacen("alloc", "{V}", 1).replacen("..", "$$$A", 1);
    Ok(Shape {
        row: row.written.clone(),
        rule: o(&[("pattern", s(&format!("{name}!{open}$$$A{close}")))]),
        constraints: None,
        fix: format!("{}!{open}{inner}{close}", &to[..bang]),
        is_type: false,
    })
}

/// A method call the text pins: `$X.collect::<Vec<_>>()` (a turbofish names the target) to
/// `P($X, alloc)`, or `"..".to_string()` (a string literal's) to `P("..", alloc)`. Any other
/// method's target is a type's question.
fn method_shape(row: &Row, to: &str) -> Result<Shape, String> {
    let (path, args) = call_parts(to).ok_or_else(|| format!("`{to}` is not a call"))?;
    if let Some(method) = row.written.strip_prefix("\"..\"") {
        let fix_args: Vec<&str> = args.iter().map(|it| if *it == "\"..\"" { "$X" } else if *it == "alloc" { "{V}" } else { it }).collect();
        return Ok(Shape {
            row: row.written.clone(),
            rule: o(&[("pattern", s(&format!("$X{method}")))]),
            constraints: Some(o(&[("X", o(&[("kind", s("string_literal"))]))])),
            fix: format!("{path}({})", fix_args.join(", ")),
            is_type: false,
        });
    }
    if args != ["alloc"] {
        return Err(format!("`{to}` is not `Trait::method(alloc)`"));
    }
    if !row.written.contains("::<") {
        // Only where the text pins the receiver: a string literal's (`"a".to_owned()`). Any
        // other receiver's type decides the method (the type checker's question).
        return Ok(Shape {
            row: format!("\"..\"{}", row.written),
            rule: o(&[("pattern", s(&format!("$X{}", row.written)))]),
            constraints: Some(o(&[("X", o(&[("kind", s("string_literal"))]))])),
            fix: format!("{path}($X, {{V}})"),
            is_type: false,
        });
    }
    Ok(Shape {
        row: row.written.clone(),
        rule: o(&[("pattern", s(&format!("$X{}", row.written)))]),
        constraints: None,
        fix: format!("{path}($X, {{V}})"),
        is_type: false,
    })
}

/// `Vec<T>` to `P<T, A>`, `String` to `P<A>`: the template's last generic is the parameter's
/// type, the others the written type's own, in order.
fn type_shape(row: &Row, to: &str) -> Result<Shape, String> {
    let open = to.find('<').ok_or_else(|| format!("`{to}` takes no type parameter"))?;
    let inner = to[open + 1..].strip_suffix('>').ok_or_else(|| format!("`{to}` does not close its `<`"))?;
    let params: Vec<&str> = inner.split(',').map(str::trim).collect();
    let own = params.len() - 1;
    let path = &to[..open];
    if own == 0 {
        // A type with no generic of its own: the bare name, not a path's last segment.
        if !plain_path(&row.written) || row.written.contains("::") {
            return Err(format!("`{}` is not a prelude type's name", row.written));
        }
        let rule = o(&[
            ("kind", s("type_identifier")),
            ("regex", s(&format!("^{}$", row.written))),
            ("not", o(&[("inside", o(&[("kind", s("scoped_type_identifier"))]))])),
        ]);
        return Ok(Shape { row: row.written.clone(), rule, constraints: None, fix: format!("{path}<{{T}}>"), is_type: true });
    }
    let vars: Vec<String> = (0..own).map(|n| format!("$T{n}")).collect();
    Ok(Shape {
        row: row.written.clone(),
        rule: o(&[("pattern", o(&[("context", s(&format!("type X = {}<{}>;", row.written, vars.join(", ")))), ("selector", s("generic_type"))]))]),
        constraints: None,
        fix: format!("{path}<{}, {{T}}>", vars.join(", ")),
        is_type: true,
    })
}

/// One match from ast-grep's `--json=stream`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Match {
    rule: String,
    /// From the crate's directory.
    file: String,
    start: usize,
    end: usize,
    /// From 1.
    line: usize,
    /// The metavariables' texts.
    vars: BTreeMap<String, String>,
}

fn read_matches(out: &str) -> Result<Vec<Match>, ToolError> {
    let bad = |line: &str| ToolError::Output { program: "ast-grep".to_owned(), line: line.to_owned() };
    let mut found = Vec::new();
    for line in out.split('\n').filter(|it| it.starts_with('{')) {
        let v = input::read(line).map_err(|_| bad(line))?;
        let range = v.get("range").ok_or_else(|| bad(line))?;
        let byte = |k: &str| range.get("byteOffset").and_then(|b| b.get(k)).and_then(|n| n.as_u64()).and_then(|n| usize::try_from(n).ok());
        let mut vars = BTreeMap::new();
        if let Some(single) = v.get("metaVariables").and_then(|m| m.get("single")).and_then(|m| m.as_object()) {
            for (name, var) in single {
                if let Some(text) = var.get("text").and_then(|t| t.as_str()) {
                    vars.insert(name.clone(), text.to_owned());
                }
            }
        }
        found.push(Match {
            rule: v.get("ruleId").and_then(|r| r.as_str()).ok_or_else(|| bad(line))?.to_owned(),
            file: v.get("file").and_then(|r| r.as_str()).ok_or_else(|| bad(line))?.trim_start_matches("./").to_owned(),
            start: byte("start").ok_or_else(|| bad(line))?,
            end: byte("end").ok_or_else(|| bad(line))?,
            line: range.get("start").and_then(|s| s.get("line")).and_then(|n| n.as_u64()).map_or(0, |n| n as usize) + 1,
            vars,
        });
    }
    Ok(found)
}

/// Runs a program under its limit in `cwd`, with this process's PATH and HOME, and gives its
/// standard output; `status_ok` says which statuses are not a failure.
fn run(argv: &[String], cwd: &Path, limit: Duration, tmp: &Path, status_ok: fn(i32) -> bool) -> Result<String, ToolError> {
    run_both(argv, cwd, limit, tmp, status_ok).map(|(out, _)| out)
}

/// [`run`], giving its standard output and its error output.
fn run_both(argv: &[String], cwd: &Path, limit: Duration, tmp: &Path, status_ok: fn(i32) -> bool) -> Result<(String, String), ToolError> {
    let program = argv.first().cloned().unwrap_or_default();
    let keep = ["PATH", "HOME", "TMPDIR", "USER", "RUSTUP_HOME", "CARGO_HOME"];
    let env: Vec<(&str, String)> = keep.iter().filter_map(|k| std::env::var(k).ok().map(|v| (*k, v))).collect();
    let log = tmp.join("run");
    let ended = run_logged(argv, cwd, &env, &log, limit).map_err(|why| ToolError::Run { program: program.clone(), why })?;
    let read = |ext: &str| std::fs::read_to_string(log.with_extension(ext)).unwrap_or_default();
    match ended {
        Ended::TimedOut { after } => Err(ToolError::TimedOut { program, after }),
        Ended::Exited(m) if status_ok(m.status) => Ok((read("out"), read("err"))),
        Ended::Exited(m) => {
            let err = read("err");
            Err(ToolError::Failed { program, status: m.status, last: err.trim_end().rsplit('\n').next().unwrap_or("").to_owned() })
        }
    }
}

/// ast-grep over the crate with `rules`: the matches, or (with `apply`) the number of changes.
struct Grep<'a> {
    dir: &'a Path,
    tmp: &'a Path,
}

impl Grep<'_> {
    fn write_rules(&self, rules: &[Rule]) -> Result<PathBuf, ToolError> {
        let path = self.tmp.join("rules.yml");
        std::fs::write(&path, rule_file(rules)).map_err(|e| ToolError::File { path: path.display().to_string(), why: e.to_string() })?;
        Ok(path)
    }

    fn query(&self, rules: &[Rule], files: &[String]) -> Result<Vec<Match>, ToolError> {
        if rules.is_empty() {
            return Ok(Vec::new());
        }
        let path = self.write_rules(rules)?;
        let mut argv = vec!["ast-grep".to_owned(), "scan".to_owned(), "--rule".to_owned(), path.display().to_string(), "--json=stream".to_owned()];
        argv.extend(files.iter().cloned());
        // ast-grep's status is 1 when a rule of severity `error` matched; these are hints.
        let out = run(&argv, self.dir, AST_GREP_LIMIT, self.tmp, |s| s == 0)?;
        let mut found = read_matches(&out)?;
        found.sort_by(|a, b| (&a.file, a.start, &a.rule).cmp(&(&b.file, b.start, &b.rule)));
        Ok(found)
    }

    /// Applies `rules` until they change nothing (at most [`PASSES`] runs); the changes made.
    fn apply(&self, rules: &[Rule], files: &[String], passes: usize) -> Result<usize, ToolError> {
        if rules.is_empty() || files.is_empty() {
            return Ok(0);
        }
        let path = self.write_rules(rules)?;
        let mut total = 0;
        for _ in 0..passes {
            let mut argv = vec!["ast-grep".to_owned(), "scan".to_owned(), "--rule".to_owned(), path.display().to_string(), "-U".to_owned()];
            argv.extend(files.iter().cloned());
            // ast-grep says "Applied N changes" on its error output.
            let (_, err) = run_both(&argv, self.dir, AST_GREP_LIMIT, self.tmp, |s| s == 0)?;
            let applied = err
                .rsplit('\n')
                .find_map(|line| line.trim().strip_prefix("Applied ").and_then(|it| it.split(' ').next()).and_then(|n| n.parse::<usize>().ok()))
                .unwrap_or(0);
            if applied == 0 {
                break;
            }
            total += applied;
        }
        Ok(total)
    }
}

/// A function of the crate, as the context query sees it.
#[derive(Debug, Clone)]
struct Func {
    file: String,
    name: String,
    start: usize,
    end: usize,
    line: usize,
    params: String,
    /// The text before its parameters: `fn f<T>`.
    head: String,
    /// `main`, or after a test attribute.
    entry: bool,
    /// The trait of the impl it is a method of, when it is one.
    trait_of: Option<String>,
    /// The type of the inherent impl it is a method of, when it is one.
    impl_of: Option<String>,
}

/// What the crate's text says around its sites.
#[derive(Debug, Default)]
struct Context {
    funcs: Vec<Func>,
    /// `static` and `const` items, by file: their byte ranges.
    statics: Vec<(String, usize, usize)>,
    /// A struct's name and its field whose type is a type parameter bounded by `Allocator`.
    alloc_fields: BTreeMap<String, String>,
}

fn context_rules() -> Vec<Rule> {
    let has = |field: &str, var: &str| o(&[("has", o(&[("field", s(field)), ("pattern", s(var))]))]);
    let q = |id: &str, rule: J| Rule { id: id.to_owned(), rule, constraints: None, fix: None, transform: None };
    vec![
        q("fn", o(&[("all", J::A(vec![o(&[("kind", s("function_item"))]), has("name", "$NAME"), has("parameters", "$PARAMS")]))])),
        q("trait-impl", o(&[("all", J::A(vec![o(&[("kind", s("impl_item"))]), has("trait", "$TRAIT")]))])),
        q("impl", o(&[("all", J::A(vec![o(&[("kind", s("impl_item"))]), has("type", "$TYPE")]))])),
        q("static", o(&[("any", J::A(vec![o(&[("kind", s("static_item"))]), o(&[("kind", s("const_item"))])]))])),
        q("struct", o(&[("all", J::A(vec![o(&[("kind", s("struct_item"))]), has("name", "$NAME")]))])),
        q("test-attr", o(&[("all", J::A(vec![o(&[("kind", s("attribute_item"))]), o(&[("regex", s(r"^#\[\s*(\w+\s*::\s*)*test\b"))])]))])),
    ]
}

/// The struct's field that holds its allocator, or none: one of type `Alloc<..>`
/// (`struct S<'a> { buf: AVec<'a, u8>, alloc: Alloc<'a> }` gives `alloc`), or of a type
/// parameter bounded by `Allocator` (`struct S<T, A: Allocator> { .., alloc: A }`).
fn alloc_field(text: &str) -> Option<String> {
    let body = text.find('{')?;
    let generics = match text.find('<') {
        Some(open) if open < body => &text[open + 1..body],
        _ => "",
    };
    let bounded: Vec<&str> = generics
        .split(',')
        .filter(|it| it.contains("Allocator"))
        .filter_map(|it| it.split(':').next())
        .map(|it| it.trim().trim_start_matches('<'))
        .filter(|it| ident_len(it.as_bytes()) == it.len() && !it.is_empty())
        .collect();
    let fields = text[body + 1..].rsplit_once('}')?.0;
    fields.split(',').find_map(|field| {
        let (name, ty) = field.split_once(':')?;
        let name = name.trim().trim_start_matches("pub(crate)").trim_start_matches("pub").trim();
        let ty = ty.trim();
        let head = ty.split('<').next().unwrap_or(ty).trim();
        let concrete = head.rsplit("::").next() == Some("Alloc");
        (concrete || bounded.contains(&ty)).then(|| name.to_owned())
    })
}

fn read_context(grep: &Grep<'_>, files: &[String], texts: &BTreeMap<String, String>) -> Result<Context, ToolError> {
    let found = grep.query(&context_rules(), files)?;
    let mut ctx = Context::default();
    let mut trait_impls = Vec::new();
    let mut impls = Vec::new();
    let mut tests: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    for m in &found {
        let var = |k: &str| m.vars.get(k).cloned().unwrap_or_default();
        match m.rule.as_str() {
            "fn" => ctx.funcs.push(Func {
                file: m.file.clone(),
                name: var("NAME"),
                start: m.start,
                end: m.end,
                line: m.line,
                params: var("PARAMS"),
                head: texts.get(&m.file).and_then(|t| t.get(m.start..m.end)).and_then(|t| t.split('(').next()).unwrap_or("").to_owned(),
                entry: false,
                trait_of: None,
                impl_of: None,
            }),
            "trait-impl" => trait_impls.push((m.file.clone(), m.start, m.end, var("TRAIT"))),
            "impl" => impls.push((m.file.clone(), m.start, m.end, var("TYPE"))),
            "static" => ctx.statics.push((m.file.clone(), m.start, m.end)),
            "struct" => {
                let text = texts.get(&m.file).and_then(|t| t.get(m.start..m.end)).unwrap_or("");
                if let Some(field) = alloc_field(text) {
                    ctx.alloc_fields.insert(var("NAME"), field);
                }
            }
            "test-attr" => tests.entry(m.file.clone()).or_default().push(m.end),
            _ => {}
        }
    }
    for f in &mut ctx.funcs {
        // The innermost impl around it: its trait, or its type.
        let around = |list: &[(String, usize, usize, String)]| {
            list.iter().filter(|(file, s, e, _)| *file == f.file && *s <= f.start && f.end <= *e).min_by_key(|(_, s, e, _)| e - s).map(|(_, s, e, t)| (*s, *e, t.clone()))
        };
        let tr = around(&trait_impls);
        let im = around(&impls);
        match (tr, im) {
            (Some(t), Some(i)) if t.1 - t.0 <= i.1 - i.0 => f.trait_of = Some(t.2),
            (_, Some(i)) => f.impl_of = Some(i.2.split('<').next().unwrap_or("").trim().to_owned()),
            (Some(t), None) => f.trait_of = Some(t.2),
            (None, None) => {}
        }
        // A test attribute with only attributes, comments and blanks between it and the function.
        let text = texts.get(&f.file).map_or("", String::as_str);
        let after_test = tests.get(&f.file).into_iter().flatten().any(|end| {
            *end <= f.start && text.get(*end..f.start).is_some_and(|gap| gap.split('\n').all(|l| { let l = l.trim(); l.is_empty() || l.starts_with("#[") || l.starts_with("//") || l.starts_with("pub") || l.starts_with("async") || l.starts_with("fn") }))
        });
        f.entry = f.name == "main" || after_test;
    }
    Ok(ctx)
}

/// Where a site's value comes from, or why it cannot.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Source {
    /// An expression already in scope (the parameter, `entry`, `self.<field>`).
    Expr(String),
    /// The function (file, name, start) gains the parameter.
    Gain(String, String, usize),
    /// A wall: its kind and what.
    Wall(&'static str, String),
}

/// `cands` is every function (file, start) that would gain, or none for the first pass that
/// finds them: rules select a function by its name, so a name that also names a function that
/// would not gain is a wall.
fn source_of(ctx: &Context, a: &Args, file: &str, at: usize, cands: Option<&BTreeSet<(String, usize)>>) -> Source {
    let inner = ctx.funcs.iter().filter(|f| f.file == file && f.start <= at && at < f.end).min_by_key(|f| f.end - f.start);
    let in_static = ctx.statics.iter().any(|(f, s, e)| f == file && *s <= at && at < *e);
    let Some(func) = inner else {
        return if in_static { Source::Wall("static-init", String::new()) } else { Source::Wall("outside", String::new()) };
    };
    // A `static` inside the function, around the site: the static is innermost.
    if ctx.statics.iter().any(|(f, s, e)| f == file && func.start <= *s && *s <= at && at < *e) {
        return Source::Wall("static-init", format!("in `{}`", func.name));
    }
    if let Some(tr) = &func.trait_of {
        let kind = if tr.trim() == "Drop" { "drop" } else { "trait-method" };
        return Source::Wall(kind, format!("`{}` of `{tr}`", func.name));
    }
    if has_param(&func.params, &a.value) {
        return Source::Expr(a.value.clone());
    }
    if func.entry {
        return match &a.entry {
            Some(e) => Source::Expr(e.clone()),
            None => Source::Wall("entry", format!("`{}`", func.name)),
        };
    }
    if a.value == "alloc" && func.params.contains("self") {
        if let Some(field) = func.impl_of.as_ref().and_then(|t| ctx.alloc_fields.get(t)) {
            return Source::Expr(format!("self.{field}"));
        }
    }
    let same: Vec<&Func> = ctx.funcs.iter().filter(|f| f.file == file && f.name == func.name).collect();
    if let Some(cands) = cands
        && same.len() > 1
        && !same.iter().all(|f| cands.contains(&(f.file.clone(), f.start)))
    {
        return Source::Wall("ambiguous", format!("`{}` names {} functions of the file", func.name, same.len()));
    }
    if let Some(g) = &a.generic {
        let name = g.split(':').next().unwrap_or(g).trim();
        if func.head.contains('<') && func.head.split(|c: char| !c.is_alphanumeric() && c != '_').any(|w| w == name) {
            return Source::Wall("name-taken", format!("`{}` already has a generic `{name}`", func.name));
        }
    }
    Source::Gain(file.to_owned(), func.name.clone(), func.start)
}

/// The innermost function around byte `at` of `file`.
fn innermost<'c>(ctx: &'c Context, file: &str, at: usize) -> Option<&'c Func> {
    ctx.funcs.iter().filter(|f| f.file == file && f.start <= at && at < f.end).min_by_key(|f| f.end - f.start)
}

/// The names (file, name) that name two functions of a file whose sites would be rewritten
/// differently: a rule selects functions by name, so it would rewrite them all alike.
fn mixed_names(ctx: &Context, ats: &[(String, usize)], sources: &[Source]) -> BTreeSet<(String, String)> {
    let mut outcomes: BTreeMap<(String, String), BTreeSet<String>> = BTreeMap::new();
    for ((file, at), source) in ats.iter().zip(sources) {
        let Some(func) = innermost(ctx, file, *at) else { continue };
        let outcome = match source {
            Source::Expr(e) => e.clone(),
            Source::Gain(..) => "\0gain".to_owned(),
            Source::Wall(..) => "\0wall".to_owned(),
        };
        outcomes.entry((file.clone(), func.name.clone())).or_default().insert(outcome);
    }
    outcomes
        .into_iter()
        .filter(|((file, name), seen)| seen.len() > 1 && ctx.funcs.iter().filter(|f| &f.file == file && &f.name == name).count() > 1)
        .map(|(key, _)| key)
        .collect()
}

/// The functions (file, start) that the sites at `ats` would give the parameter.
fn candidates(ctx: &Context, a: &Args, ats: &[(String, usize)]) -> BTreeSet<(String, usize)> {
    ats.iter()
        .filter_map(|(file, at)| match source_of(ctx, a, file, *at, None) {
            Source::Gain(f, _, start) => Some((f, start)),
            _ => None,
        })
        .collect()
}

/// Whether a parameter list `(..)` has a parameter named `name`.
fn has_param(params: &str, name: &str) -> bool {
    let inner = params.trim().trim_start_matches('(').trim_end_matches(')');
    inner.split(',').any(|p| p.split(':').next().is_some_and(|n| n.trim().trim_start_matches("mut ").trim() == name))
}

/// The rules that give the functions `names` of a file the parameter and the generic.
fn gain_rules(names: &BTreeSet<String>, a: &Args) -> Vec<Rule> {
    let any_param = o(&[("any", J::A(vec![o(&[("kind", s("parameter"))]), o(&[("kind", s("self_parameter"))])]))]);
    let in_named = o(&[("kind", s("function_item")), ("has", o(&[("field", s("name")), ("regex", s(&names_regex(names)))]))]);
    let param = format!("{}: {}", a.value, a.ty);
    let mut rules = vec![
        Rule {
            id: "gain-param".to_owned(),
            rule: o(&[("all", J::A(vec![
                o(&[("pattern", s("$P"))]),
                any_param.clone(),
                o(&[("not", o(&[("precedes", any_param.clone())]))]),
                o(&[("inside", o(&[("kind", s("parameters")), ("inside", in_named.clone())]))]),
            ]))]),
            constraints: None,
            fix: Some(format!("$P, {param}")),
            transform: None,
        },
        Rule {
            id: "gain-param-first".to_owned(),
            rule: o(&[("kind", s("parameters")), ("not", o(&[("has", any_param)])), ("inside", in_named.clone())]),
            constraints: None,
            fix: Some(format!("({param})")),
            transform: None,
        },
    ];
    if let Some(g) = &a.generic {
        // `<T>` to `<T, A: ..>`: the fix takes the generics' inside, which `INNER` cuts out.
        let inner = o(&[("INNER", o(&[("substring", o(&[("source", s("$G")), ("startChar", J::I(1)), ("endChar", J::I(-1))]))]))]);
        rules.push(Rule {
            id: "gain-generic".to_owned(),
            rule: o(&[("all", J::A(vec![
                o(&[("pattern", s("$G"))]),
                o(&[("kind", s("type_parameters"))]),
                o(&[("inside", o(&[("field", s("type_parameters")), ("kind", s("function_item")), ("has", o(&[("field", s("name")), ("regex", s(&names_regex(names)))]))]))]),
            ]))]),
            constraints: None,
            fix: Some(format!("<$INNER, {g}>")),
            transform: Some(inner),
        });
        rules.push(Rule {
            id: "gain-generic-first".to_owned(),
            rule: o(&[("all", J::A(vec![
                o(&[("pattern", s("$N"))]),
                o(&[("kind", s("identifier"))]),
                o(&[("regex", s(&names_regex(names)))]),
                o(&[("inside", o(&[("kind", s("function_item")), ("field", s("name"))]))]),
                o(&[("not", o(&[("inside", o(&[("kind", s("function_item")), ("has", o(&[("field", s("type_parameters")), ("kind", s("type_parameters"))]))]))]))]),
            ]))]),
            constraints: None,
            fix: Some(format!("$N<{g}>")),
            transform: None,
        });
    }
    rules
}


/// What one run did.
#[derive(Debug, Default)]
struct Summary {
    sites: BTreeMap<String, usize>,
    skipped: BTreeMap<String, String>,
    gained: BTreeSet<String>,
    walls: BTreeMap<String, BTreeSet<String>>,
    checks: usize,
}

impl Summary {
    fn wall(&mut self, kind: &str, file: &str, line: usize, what: &str) {
        let at = if what.is_empty() { format!("{file}:{line}") } else { format!("{file}:{line}: {what}") };
        self.walls.entry(kind.to_owned()).or_default().insert(at);
    }

    fn json(&self, a: &Args) -> String {
        let map = |m: &BTreeMap<String, usize>| J::O(m.iter().map(|(k, v)| (k.clone(), J::N(*v))).collect());
        let strs = |it: &BTreeSet<String>| J::A(it.iter().map(|s| J::S(s.clone())).collect());
        let summary = o(&[
            ("crate", s(&a.krate)),
            ("mode", s(a.mode.name())),
            ("value", s(&a.value)),
            ("sites", map(&self.sites)),
            ("skipped", J::O(self.skipped.iter().map(|(k, v)| (k.clone(), J::S(v.clone()))).collect())),
            ("gained", strs(&self.gained)),
            ("walls", J::O(self.walls.iter().map(|(k, v)| (k.clone(), strs(v))).collect())),
            ("checks", J::N(self.checks)),
        ]);
        summary.text()
    }
}

/// The `.rs` files under the crate's directory, from it, sorted; none under `target`.
fn rust_files(dir: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let mut todo = vec![PathBuf::new()];
    // Bounded: each directory is listed once; links are not followed.
    while let Some(rel) = todo.pop() {
        let Ok(listed) = std::fs::read_dir(dir.join(&rel)) else { continue };
        for entry in listed.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let path = rel.join(&name);
            let ty = entry.file_type();
            if ty.as_ref().is_ok_and(|t| t.is_dir()) && name != "target" {
                todo.push(path);
            } else if ty.is_ok_and(|t| t.is_file()) && name.ends_with(".rs") {
                out.push(path.to_string_lossy().into_owned());
            }
        }
    }
    out.sort();
    out
}

fn texts_of(dir: &Path, files: &[String]) -> Result<BTreeMap<String, String>, ToolError> {
    files
        .iter()
        .map(|f| std::fs::read_to_string(dir.join(f)).map(|t| (f.clone(), t)).map_err(|e| ToolError::File { path: f.clone(), why: e.to_string() }))
        .collect()
}

/// Rewrites the sites of `shapes` in the crate, giving their functions the parameter; types
/// follow into the functions that gained it.
fn thread_sites(grep: &Grep<'_>, a: &Args, shapes: &[Shape], files: &[String], out: &mut Summary) -> Result<(), ToolError> {
    let texts = texts_of(grep.dir, files)?;
    let ctx = read_context(grep, files, &texts)?;
    let site_shapes: Vec<&Shape> = shapes.iter().filter(|s| !s.is_type).collect();
    let query: Vec<Rule> = site_shapes
        .iter()
        .enumerate()
        .map(|(n, sh)| Rule { id: format!("site{n}"), rule: sh.rule.clone(), constraints: sh.constraints.clone(), fix: None, transform: None })
        .collect();
    let sites = grep.query(&query, files)?;
    // First every function that would gain, so that a name's other functions are judged with them.
    let ats: Vec<(String, usize)> = sites.iter().map(|m| (m.file.clone(), m.start)).collect();
    let cands = candidates(&ctx, a, &ats);
    let sources: Vec<Source> = sites.iter().map(|m| source_of(&ctx, a, &m.file, m.start, Some(&cands))).collect();
    let mixed = mixed_names(&ctx, &ats, &sources);
    let mut gaining: BTreeSet<(String, String)> = BTreeSet::new();
    // By file, then by the value's expression: the functions whose sites take it.
    let mut plan: BTreeMap<String, BTreeMap<String, BTreeSet<String>>> = BTreeMap::new();
    for (m, source) in sites.iter().zip(sources) {
        let index: usize = m.rule.trim_start_matches("site").parse().unwrap_or(0);
        let shape = site_shapes[index];
        let func = innermost(&ctx, &m.file, m.start);
        let name = func.map(|f| f.name.clone()).unwrap_or_default();
        let source = if mixed.contains(&(m.file.clone(), name.clone())) {
            Source::Wall("ambiguous", format!("`{name}` names functions of the file whose sites differ"))
        } else {
            source
        };
        match source {
            Source::Wall(kind, what) => out.wall(kind, &m.file, m.line, format!("{} {what}", shape.row).trim()),
            Source::Expr(e) => {
                *out.sites.entry(shape.row.clone()).or_default() += 1;
                plan.entry(m.file.clone()).or_default().entry(e).or_default().insert(name);
            }
            Source::Gain(f, n, _) => {
                *out.sites.entry(shape.row.clone()).or_default() += 1;
                gaining.insert((f.clone(), n.clone()));
                plan.entry(f).or_default().entry(a.value.clone()).or_default().insert(n);
            }
        }
    }
    needs_lifetime_walls(grep, shapes, files, out)?;
    apply_plan(grep, a, shapes, &ctx, &plan, &gaining, out)
}

/// A type definition whose fields name a collection that moves to blaze-own: it takes a
/// lifetime and `Alloc<'a>`, which this tool leaves to a person or a lane.
fn needs_lifetime_walls(grep: &Grep<'_>, shapes: &[Shape], files: &[String], out: &mut Summary) -> Result<(), ToolError> {
    let def = o(&[("inside", o(&[("stopBy", s("end")), ("any", J::A(vec![o(&[("kind", s("struct_item"))]), o(&[("kind", s("enum_item"))]), o(&[("kind", s("union_item"))])]))]))]);
    let rules: Vec<Rule> = shapes
        .iter()
        .filter(|sh| sh.is_type)
        .enumerate()
        .map(|(n, sh)| Rule { id: format!("def{n}"), rule: all(sh.rule.clone(), def.clone()), constraints: None, fix: None, transform: None })
        .collect();
    let defs: Vec<&Shape> = shapes.iter().filter(|sh| sh.is_type).collect();
    for m in grep.query(&rules, files)? {
        let n: usize = m.rule.trim_start_matches("def").parse().unwrap_or(0);
        let row = defs.get(n).map_or("", |sh| sh.row.as_str());
        out.wall("needs-lifetime", &m.file, m.line, &format!("`{row}` in a type definition"));
    }
    Ok(())
}

/// Applies the plan: per file, each expression's site rules inside its functions, then the
/// types inside the functions that gain, then their parameters.
fn apply_plan(
    grep: &Grep<'_>,
    a: &Args,
    shapes: &[Shape],
    ctx: &Context,
    plan: &BTreeMap<String, BTreeMap<String, BTreeSet<String>>>,
    gaining: &BTreeSet<(String, String)>,
    out: &mut Summary,
) -> Result<(), ToolError> {
    for (file, by_expr) in plan {
        let one = [file.clone()];
        let mut rules = Vec::new();
        for (n, (expr, fns)) in by_expr.iter().enumerate() {
            for (m, sh) in shapes.iter().filter(|s| !s.is_type).enumerate() {
                rules.push(Rule { id: format!("e{n}s{m}"), rule: all(sh.rule.clone(), inside_fns(fns)), constraints: sh.constraints.clone(), fix: Some(sh.fix.replace("{V}", expr)), transform: None });
            }
        }
        grep.apply(&rules, &one, PASSES)?;
        let gain: BTreeSet<String> = gaining.iter().filter(|(f, _)| f == file).map(|(_, n)| n.clone()).collect();
        if gain.is_empty() {
            continue;
        }
        let types: Vec<Rule> = shapes
            .iter()
            .filter(|s| s.is_type)
            .enumerate()
            .map(|(m, sh)| Rule { id: format!("t{m}"), rule: all(sh.rule.clone(), inside_fns(&gain)), constraints: None, fix: Some(sh.fix.replace("{T}", &a.ty)), transform: None })
            .collect();
        grep.apply(&types, &one, PASSES)?;
        let path = grep.write_rules(&gain_rules(&gain, a))?;
        let argv = vec!["ast-grep".to_owned(), "scan".to_owned(), "--rule".to_owned(), path.display().to_string(), "-U".to_owned(), file.clone()];
        run(&argv, grep.dir, AST_GREP_LIMIT, grep.tmp, |s| s == 0)?;
        for f in ctx.funcs.iter().filter(|f| &f.file == file && gain.contains(&f.name)) {
            out.gained.insert(format!("{}:{} {}", f.file, f.line, f.name));
        }
    }
    fn_pointer_walls(grep, gaining, out)
}

/// A function that gained the parameter, named as a value: an argument that is its path.
fn fn_pointer_walls(grep: &Grep<'_>, gaining: &BTreeSet<(String, String)>, out: &mut Summary) -> Result<(), ToolError> {
    let names: BTreeSet<String> = gaining.iter().map(|(_, n)| n.clone()).collect();
    if names.is_empty() {
        return Ok(());
    }
    let regex = format!("^(\\w+::)*({})$", names.iter().cloned().collect::<Vec<_>>().join("|"));
    let rule = Rule {
        id: "fn-pointer".to_owned(),
        rule: o(&[
            ("any", J::A(vec![o(&[("kind", s("identifier"))]), o(&[("kind", s("scoped_identifier"))])])),
            ("regex", s(&regex)),
            ("inside", o(&[("kind", s("arguments"))])),
        ]),
        constraints: None,
        fix: None,
        transform: None,
    };
    let files: Vec<String> = rust_files(grep.dir);
    for m in grep.query(&[rule], &files)? {
        out.wall("fn-pointer", &m.file, m.line, "a function that gained the parameter, passed as a value");
    }
    Ok(())
}

/// The package's name from its manifest.
fn package_name(manifest: &str) -> Option<String> {
    let mut in_package = false;
    for line in manifest.split('\n').map(str::trim) {
        if line.starts_with('[') {
            in_package = line == "[package]";
            continue;
        }
        if in_package && let Some(value) = line.strip_prefix("name").map(str::trim_start).and_then(|it| it.strip_prefix('=')) {
            return Some(value.trim().trim_matches('"').to_owned());
        }
    }
    None
}

/// One error of `cargo check`'s JSON: its code, its primary span (file from the crate's
/// directory, line, byte range) and message.
#[derive(Debug, Clone, PartialEq, Eq)]
struct CompileError {
    code: String,
    file: String,
    line: usize,
    start: usize,
    end: usize,
    message: String,
    /// Its notes' and helps' messages, joined.
    notes: String,
}

fn read_errors(out: &str, prefix: &str) -> Vec<CompileError> {
    let mut errors = Vec::new();
    for line in out.split('\n').filter(|l| l.starts_with('{')) {
        let Ok(v) = input::read(line) else { continue };
        if v.get("reason").and_then(|r| r.as_str()) != Some("compiler-message") {
            continue;
        }
        let Some(msg) = v.get("message") else { continue };
        if msg.get("level").and_then(|l| l.as_str()) != Some("error") {
            continue;
        }
        let code = msg.get("code").and_then(|c| c.get("code")).and_then(|c| c.as_str()).unwrap_or("").to_owned();
        let text = msg.get("message").and_then(|m| m.as_str()).unwrap_or("").to_owned();
        let notes: Vec<&str> = msg.get("children").and_then(|c| c.as_array()).into_iter().flatten().filter_map(|c| c.get("message").and_then(|m| m.as_str())).collect();
        let notes = notes.join("\n");
        let primary = msg.get("spans").and_then(|s| s.as_array()).and_then(|spans| spans.iter().find(|s| s.get("is_primary").and_then(|p| p.as_bool()) == Some(true)));
        let Some(span) = primary else {
            errors.push(CompileError { code, file: String::new(), line: 0, start: 0, end: 0, message: text, notes });
            continue;
        };
        let num = |k: &str| span.get(k).and_then(|n| n.as_u64()).map_or(0, |n| n as usize);
        let file = span.get("file_name").and_then(|f| f.as_str()).unwrap_or("");
        let file = file.strip_prefix(prefix).unwrap_or(file).trim_start_matches('/').to_owned();
        errors.push(CompileError { code, file, line: num("line_start"), start: num("byte_start"), end: num("byte_end"), message: text, notes });
    }
    errors.sort_by(|a, b| (&a.file, a.start, &a.code).cmp(&(&b.file, b.start, &b.code)));
    errors.dedup();
    errors
}

/// E0061's counts: "this function takes 2 arguments but 1 argument was supplied" gives (2, 1).
fn arity(message: &str) -> Option<(usize, usize)> {
    let after = message.split(" takes ").nth(1)?;
    let wanted: usize = after.split(' ').next()?.parse().ok()?;
    let given_text = after.split(" but ").nth(1)?;
    let given: usize = given_text.split(' ').next()?.parse().ok()?;
    Some((wanted, given))
}

/// The rules that add `expr` as the last argument of the calls of `name` with `given`
/// arguments, inside the functions `fns`.
fn call_rules(name: &str, given: usize, expr: &str, fns: &BTreeSet<String>) -> Vec<Rule> {
    let vars: Vec<String> = (0..given).map(|n| format!("$A{n}")).collect();
    let args = vars.join(", ");
    let fixed = if given == 0 { expr.to_owned() } else { format!("{args}, {expr}") };
    let callee = o(&[("F", o(&[("regex", s(&format!("^(.*::)?{name}$")))]))]);
    vec![
        Rule { id: "call".to_owned(), rule: all(o(&[("pattern", s(&format!("$F({args})")))]), inside_fns(fns)), constraints: Some(callee), fix: Some(format!("$F({fixed})")), transform: None },
        Rule { id: "method".to_owned(), rule: all(o(&[("pattern", s(&format!("$R.{name}({args})")))]), inside_fns(fns)), constraints: None, fix: Some(format!("$R.{name}({fixed})")), transform: None },
    ]
}

/// The names an error prints for blaze-own's collections: the last segment of each type the
/// table moves to `blaze_own`, and its alias with `A` before it (`Vec`, `AVec`).
fn own_collections(rows: &[Row]) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    for to in rows.iter().filter(|r| r.kind == "type").filter_map(|r| r.replacement.as_deref()) {
        if root_of(to) != "blaze_own" {
            continue;
        }
        let head = to.split('<').next().unwrap_or(to);
        let last = head.rsplit("::").next().unwrap_or(head);
        names.insert(format!("A{last}"));
        names.insert(last.to_owned());
    }
    names
}

/// What an E0599 at a `.share()` says its receiver is.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ShareSite {
    /// A blaze-own collection over `Alloc`: `.clone_in(alloc)`.
    Own,
    /// Any other type, named: `.clone()` again.
    Kept(String),
    /// The trait's import is not seen there: `.clone()` again.
    NotInScope,
}

/// The receiver's kind from the error's message ("no method named `share` found for struct
/// `Vec<u8, Alloc<'_>>` in the current scope", "the method `share` exists for reference
/// `&Foo`, but its trait bounds were not satisfied") and notes; none for another error.
fn share_site(message: &str, notes: &str, own: &BTreeSet<String>) -> Option<ShareSite> {
    if !message.contains("`share`") {
        return None;
    }
    if notes.contains("not in scope") || notes.contains("trait is in scope") {
        return Some(ShareSite::NotInScope);
    }
    let ty = message.split(" for ").nth(1).and_then(|it| it.split('`').nth(1)).unwrap_or("").to_owned();
    let mut bare = ty.trim();
    while let Some(rest) = bare.strip_prefix('&') {
        bare = rest.trim_start().strip_prefix("mut ").unwrap_or(rest).trim_start();
    }
    let head = bare.split('<').next().unwrap_or(bare);
    let last = head.rsplit("::").next().unwrap_or(head);
    match own.contains(last) && bare.contains("Alloc") {
        true => Some(ShareSite::Own),
        false => Some(ShareSite::Kept(ty)),
    }
}

/// A rule for the one `$R.share()` whose method name is at bytes `start..end` of `text`
/// (ast-grep's `range` counts lines from 0 and columns in characters), fixed to `fix`.
fn share_at_rule(id: String, text: &str, start: usize, end: usize, fix: String) -> Option<Rule> {
    let point = |at: usize| -> Option<J> {
        let before = text.get(..at)?;
        let line_start = before.rfind('\n').map_or(0, |n| n + 1);
        let line = before.matches('\n').count();
        let column = before[line_start..].chars().count();
        Some(o(&[("line", J::N(line)), ("column", J::N(column))]))
    };
    let name = o(&[("stopBy", s("end")), ("kind", s("field_identifier")), ("regex", s("^share$")), ("range", o(&[("start", point(start)?), ("end", point(end)?)]))]);
    Some(Rule { id, rule: all(o(&[("pattern", s("$R.share()"))]), o(&[("has", name)])), constraints: None, fix: Some(fix), transform: None })
}

/// share-clone's first step: each file with a top-level `use` has its `.clone()` calls made
/// `.share()` and gains the trait's import; then settle's loop sorts the calls out.
fn share_clone(grep: &Grep<'_>, a: &Args, root: &Path, files: &[String], own: &BTreeSet<String>, out: &mut Summary) -> Result<(), ToolError> {
    let first_use = o(&[("all", J::A(vec![
        o(&[("pattern", s("$U"))]),
        o(&[("kind", s("use_declaration"))]),
        o(&[("inside", o(&[("kind", s("source_file"))]))]),
        o(&[("not", o(&[("follows", o(&[("stopBy", s("end")), ("kind", s("use_declaration"))]))]))]),
    ]))]);
    let q = |id: &str, rule: J, fix: Option<String>| Rule { id: id.to_owned(), rule, constraints: None, fix, transform: None };
    let clone = q("clone", o(&[("pattern", s("$R.clone()"))]), Some("$R.share()".to_owned()));
    let found = grep.query(&[q("first-use", first_use.clone(), None), Rule { fix: None, ..clone.clone() }], files)?;
    let with_use: BTreeSet<&str> = found.iter().filter(|m| m.rule == "first-use").map(|m| m.file.as_str()).collect();
    let mut eligible = BTreeSet::new();
    for m in found.iter().filter(|m| m.rule == "clone") {
        match with_use.contains(m.file.as_str()) {
            true => {
                *out.sites.entry(".clone() to .share()".to_owned()).or_default() += 1;
                eligible.insert(m.file.clone());
            }
            false => out.wall("no-use", &m.file, m.line, "a `.clone()` in a file with no top-level `use`"),
        }
    }
    let eligible: Vec<String> = eligible.into_iter().collect();
    grep.apply(&[clone], &eligible, PASSES)?;
    // One pass: the import is then the file's first `use`, which a second pass would precede.
    grep.apply(&[q("share-use", first_use, Some(format!("{SHARE_USE}\n$U")))], &eligible, 1)?;
    settle(grep, a, root, Some(own), out)
}

fn settle(grep: &Grep<'_>, a: &Args, root: &Path, own: Option<&BTreeSet<String>>, out: &mut Summary) -> Result<(), ToolError> {
    let manifest = grep.dir.join("Cargo.toml");
    let prefix = grep.dir.strip_prefix(root).map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
    for step in 0..=a.steps {
        if step == a.steps {
            out.wall("steps", &a.krate, 0, &format!("{} checks run, errors left", a.steps));
            return Ok(());
        }
        let argv = vec![a.cargo.clone(), "check".to_owned(), "--message-format=json".to_owned(), "--all-targets".to_owned(), "--manifest-path".to_owned(), manifest.display().to_string()];
        // cargo's status is 101 when the crate does not compile: that is what is read.
        let json = run(&argv, root, CARGO_LIMIT, grep.tmp, |s| s == 0 || s == 101)?;
        out.checks += 1;
        let errors = read_errors(&json, &prefix);
        if errors.is_empty() {
            return Ok(());
        }
        let files = rust_files(grep.dir);
        let texts = texts_of(grep.dir, &files)?;
        let ctx = read_context(grep, &files, &texts)?;
        let threaded: BTreeSet<String> = ctx.funcs.iter().filter(|f| has_param(&f.params, &a.value)).map(|f| f.name.clone()).collect();
        let mut acted = false;
        let mut gaining: BTreeSet<(String, String)> = BTreeSet::new();
        // By file: (callee, given, the expression) to the functions the calls are in.
        let mut calls: BTreeMap<String, BTreeMap<(String, usize, String), BTreeSet<String>>> = BTreeMap::new();
        let mut left = Vec::new();
        let mut acting = Vec::new();
        let mut sharing = Vec::new();
        for e in &errors {
            if let Some(own) = own
                && e.code == "E0599"
                && let Some(site) = share_site(&e.message, &e.notes, own)
            {
                sharing.push((e, site));
                continue;
            }
            let callee = texts.get(&e.file).and_then(|t| t.get(e.start..e.end)).map(|t| t.rsplit("::").next().unwrap_or(t).rsplit('.').next().unwrap_or(t).trim().to_owned());
            let fits = e.code == "E0061" && arity(&e.message).is_some_and(|(w, g)| w == g + 1);
            match callee.filter(|c| fits && threaded.contains(c)) {
                Some(callee) => acting.push((e, callee)),
                None => left.push(e),
            }
        }
        let ats: Vec<(String, usize)> = acting.iter().map(|(e, _)| (e.file.clone(), e.start)).collect();
        let cands = candidates(&ctx, a, &ats);
        let sources: Vec<Source> = ats.iter().map(|(file, at)| source_of(&ctx, a, file, *at, Some(&cands))).collect();
        let mixed = mixed_names(&ctx, &ats, &sources);
        for ((e, callee), source) in acting.into_iter().zip(sources) {
            let (_, given) = arity(&e.message).unwrap_or((0, 0));
            let name = innermost(&ctx, &e.file, e.start).map(|f| f.name.clone()).unwrap_or_default();
            let source = if mixed.contains(&(e.file.clone(), name.clone())) {
                Source::Wall("ambiguous", format!("`{name}` names functions of the file whose calls differ"))
            } else {
                source
            };
            match source {
                Source::Wall(kind, what) => out.wall(kind, &e.file, e.line, format!("a call of `{callee}` {what}").trim()),
                Source::Expr(x) => {
                    calls.entry(e.file.clone()).or_default().entry((callee, given, x)).or_default().insert(name);
                    acted = true;
                }
                Source::Gain(f, n, _) => {
                    gaining.insert((f.clone(), n.clone()));
                    calls.entry(f).or_default().entry((callee, given, a.value.clone())).or_default().insert(n);
                    acted = true;
                }
            }
        }
        // share-clone: each `.share()` the compiler refused, by file, at its own place.
        let mut share_rules: BTreeMap<String, Vec<(Rule, &'static str, usize)>> = BTreeMap::new();
        let own_ats: Vec<(String, usize)> = sharing.iter().filter(|(_, s)| *s == ShareSite::Own).map(|(e, _)| (e.file.clone(), e.start)).collect();
        let own_cands = candidates(&ctx, a, &own_ats);
        let own_sources: Vec<Source> = own_ats.iter().map(|(file, at)| source_of(&ctx, a, file, *at, Some(&own_cands))).collect();
        let own_mixed = mixed_names(&ctx, &own_ats, &own_sources);
        let mut own_sources = own_sources.into_iter();
        let back = || ("$R.clone()".to_owned(), ".share() back to .clone()");
        for (n, (e, site)) in sharing.iter().enumerate() {
            let (fix, row) = match site {
                ShareSite::Own => {
                    let name = innermost(&ctx, &e.file, e.start).map(|f| f.name.clone()).unwrap_or_default();
                    let source = own_sources.next().unwrap_or(Source::Wall("outside", String::new()));
                    let source = if own_mixed.contains(&(e.file.clone(), name.clone())) {
                        Source::Wall("ambiguous", format!("`{name}` names functions of the file whose sites differ"))
                    } else {
                        source
                    };
                    match source {
                        Source::Expr(x) => (format!("$R.clone_in({x})"), ".share() to .clone_in"),
                        Source::Gain(f, g, _) => {
                            gaining.insert((f, g));
                            (format!("$R.clone_in({})", a.value), ".share() to .clone_in")
                        }
                        Source::Wall(kind, what) => {
                            out.wall(kind, &e.file, e.line, format!("a `.share()` of a blaze-own collection {what}").trim());
                            back()
                        }
                    }
                }
                ShareSite::Kept(ty) => {
                    out.wall("kept-clone", &e.file, e.line, &format!("`{ty}`"));
                    back()
                }
                ShareSite::NotInScope => {
                    out.wall("not-in-scope", &e.file, e.line, "the module does not see the file's `blaze_own::Share` import");
                    back()
                }
            };
            let text = texts.get(&e.file).map_or("", String::as_str);
            match share_at_rule(format!("share{n}"), text, e.start, e.end, fix) {
                Some(rule) => {
                    share_rules.entry(e.file.clone()).or_default().push((rule, row, e.line));
                    acted = true;
                }
                None => out.wall("unmatched", &e.file, e.line, "a `.share()` error whose place is not in its file"),
            }
        }
        if !acted {
            for e in left {
                out.wall("error", &e.file, e.line, &format!("{} {}", e.code, e.message).trim().to_owned());
            }
            return Ok(());
        }
        let mut changed = 0;
        // The `.share()` sites first: their places are the check's, before any other edit.
        for (file, planned) in &share_rules {
            let rules: Vec<Rule> = planned.iter().map(|(r, _, _)| r.clone()).collect();
            let applied = grep.apply(&rules, &[file.clone()], 1)?;
            changed += applied;
            if applied == planned.len() {
                for (_, row, _) in planned {
                    *out.sites.entry((*row).to_owned()).or_default() += 1;
                }
            } else {
                let lines: Vec<String> = planned.iter().map(|(_, _, l)| l.to_string()).collect();
                out.wall("unmatched", file, planned[0].2, &format!("{} of {} `.share()` sites rewritten (lines {})", applied, planned.len(), lines.join(", ")));
            }
        }
        let mut called = 0;
        for (file, by) in &calls {
            let rules: Vec<Rule> = by
                .iter()
                .enumerate()
                .flat_map(|(n, ((callee, given, x), fns))| {
                    call_rules(callee, *given, x, fns).into_iter().map(move |mut r| {
                        r.id = format!("{}{n}", r.id);
                        r
                    })
                })
                .collect();
            called += grep.apply(&rules, &[file.clone()], 1)?;
        }
        if called > 0 {
            *out.sites.entry("E0061".to_owned()).or_default() += called;
        }
        changed += called;
        if changed == 0 {
            // The errors' calls are where ast-grep's rules do not reach: a macro's tokens.
            for e in errors.iter().filter(|e| e.code == "E0061") {
                out.wall("unmatched", &e.file, e.line, "the call is not one ast-grep sees (in a macro's tokens?)");
            }
            return Ok(());
        }
        let files_gaining: BTreeMap<String, BTreeMap<String, BTreeSet<String>>> = gaining.iter().map(|(f, _)| (f.clone(), BTreeMap::new())).collect();
        apply_plan(grep, a, &[], &ctx, &files_gaining, &gaining, out)?;
    }
    Ok(())
}

fn run_mode(a: &Args) -> Result<Summary, ToolError> {
    let root = std::fs::canonicalize(&a.root).map_err(|e| ToolError::File { path: a.root.clone(), why: e.to_string() })?;
    let dir = root.join(&a.krate);
    let manifest = std::fs::read_to_string(dir.join("Cargo.toml")).ok();
    if manifest.as_deref().and_then(package_name).is_none() {
        return Err(ToolError::NoCrate { path: a.krate.clone() });
    }
    let tmp = TempDir::new("tobstd-").map_err(|e| ToolError::File { path: "a temporary directory".to_owned(), why: e.to_string() })?;
    let grep = Grep { dir: &dir, tmp: tmp.path() };
    let mut out = Summary::default();
    let files = rust_files(&dir);
    match a.mode {
        Mode::Rename => {
            let text = std::fs::read_to_string(root.join(TABLE)).map_err(|e| ToolError::File { path: TABLE.to_owned(), why: e.to_string() })?;
            let (shapes, skipped) = rename_shapes(&read_table(&text)?, &a.roots);
            out.skipped = skipped;
            let rules: Vec<Rule> = shapes.iter().enumerate().map(|(n, sh)| Rule { id: format!("r{n}"), rule: sh.rule.clone(), constraints: None, fix: Some(sh.fix.clone()), transform: None }).collect();
            // A site is the outermost path a row names: `std::fmt::Debug` counts for its own
            // row, not also for `std::fmt`'s (the matches come sorted by file and start).
            let found = grep.query(&rules, &files)?;
            let mut outer: Vec<&Match> = Vec::new();
            for m in &found {
                match outer.last_mut() {
                    Some(last) if last.file == m.file && last.start == m.start => {
                        if m.end > last.end {
                            *last = m;
                        }
                    }
                    _ => outer.push(m),
                }
            }
            for m in outer {
                let n: usize = m.rule.trim_start_matches('r').parse().unwrap_or(0);
                *out.sites.entry(shapes[n].row.clone()).or_default() += 1;
            }
            grep.apply(&rules, &files, PASSES)?;
        }
        Mode::AddArg => {
            let text = std::fs::read_to_string(root.join(TABLE)).map_err(|e| ToolError::File { path: TABLE.to_owned(), why: e.to_string() })?;
            let (shapes, skipped) = add_arg_shapes(&read_table(&text)?, &a.roots, a.value == "alloc");
            out.skipped = skipped;
            thread_sites(&grep, a, &shapes, &files, &mut out)?;
        }
        Mode::Settle => settle(&grep, a, &root, None, &mut out)?,
        Mode::ShareClone => {
            let text = std::fs::read_to_string(root.join(TABLE)).map_err(|e| ToolError::File { path: TABLE.to_owned(), why: e.to_string() })?;
            let own = own_collections(&read_table(&text)?);
            share_clone(&grep, a, &root, &files, &own, &mut out)?;
        }
    }
    Ok(out)
}

fn main_with(a: &Args) {
    match run_mode(a) {
        Ok(summary) => println!("{}", summary.json(a)),
        Err(e) => fail(SCRIPT, &e.to_string()),
    }
}

fn expect(holds: bool, what: &str) {
    blaze_script::expect(SCRIPT, holds, what);
}

fn args(krate: &str, mode: Mode, root: &str) -> Args {
    Args {
        krate: krate.to_owned(),
        mode,
        steps: 8,
        value: "alloc".to_owned(),
        ty: ALLOC_TYPE.to_owned(),
        generic: None,
        entry: Some(ALLOC_ENTRY.to_owned()),
        roots: vec!["core".to_owned(), "blaze_own".to_owned()],
        cargo: String::new(),
        root: root.to_owned(),
    }
}

/// The fixture: scripts/tests/tobstd/fixture is copied, each mode run on the copy, and each
/// file compared with scripts/tests/tobstd/expected/<mode>/, the summary with
/// expected/<mode>.json. `settle` needs a cargo and is not run here.
fn selftest() {
    let row = |written: &str, form: &str, kind: &str, to: &str, class: &str| Row {
        path: written.to_owned(),
        written: written.to_owned(),
        form: form.to_owned(),
        kind: kind.to_owned(),
        replacement: Some(to.to_owned()),
        class: class.to_owned(),
    };
    // The shapes from the table's forms.
    let call = call_shape(&row("Vec::with_capacity", "prelude", "function", "blaze_own::Vec::with_capacity_in(n, alloc)", "add-arg"), "blaze_own::Vec::with_capacity_in(n, alloc)");
    expect(call.as_ref().is_ok_and(|c| c.fix == "blaze_own::Vec::with_capacity_in($A0, {V})" && c.rule.text().contains("Vec::with_capacity($A0)")), &format!("a call's shape: {call:?}"));
    let mac = macro_shape(&row("vec!", "macro", "macro", "blaze_own::vec_in![alloc; ..]", "add-arg"), "blaze_own::vec_in![alloc; ..]");
    expect(mac.as_ref().is_ok_and(|m| m.fix == "blaze_own::vec_in![{V}; $$$A]"), &format!("a macro's shape: {mac:?}"));
    let lit = method_shape(&row("\"..\".to_string()", "method", "method", "blaze_own::String::from_str_in(\"..\", alloc)", "add-arg"), "blaze_own::String::from_str_in(\"..\", alloc)");
    expect(lit.as_ref().is_ok_and(|m| m.fix == "blaze_own::String::from_str_in($X, {V})"), &format!("a literal's method: {lit:?}"));
    let untyped = method_shape(&row(".to_owned()", "method", "method", "blaze_own::ToOwnedIn::to_owned_in(alloc)", "add-arg"), "blaze_own::ToOwnedIn::to_owned_in(alloc)");
    expect(
        untyped.as_ref().is_ok_and(|m| m.constraints.as_ref().is_some_and(|c| c.text().contains("string_literal")) && m.fix == "blaze_own::ToOwnedIn::to_owned_in($X, {V})"),
        &format!("a method whose receiver's type decides it is taken on a literal only: {untyped:?}"),
    );
    let ty = type_shape(&row("Vec", "prelude", "type", "blaze_own::Vec<T, A>", "add-arg"), "blaze_own::Vec<T, A>");
    expect(ty.as_ref().is_ok_and(|t| t.fix == "blaze_own::Vec<$T0, {T}>"), &format!("a type's shape: {ty:?}"));
    expect(arity("this function takes 2 arguments but 1 argument was supplied") == Some((2, 1)), "E0061's counts");
    expect(arity("this method takes 1 argument but 0 arguments were supplied") == Some((1, 0)), "E0061's counts for a method");
    expect(has_param("(&self, x: u8, mut alloc: A)", "alloc") && !has_param("(allocs: u8)", "alloc"), "a parameter by name");
    expect(alloc_field("struct S<T, A: Allocator + Copy> { buf: Vec<T, A>, pub alloc: A }").as_deref() == Some("alloc"), "a struct's allocator field");
    expect(alloc_field("struct S<'a> { buf: Vec<u8>, pub(crate) alloc: blaze_own::Alloc<'a> }").as_deref() == Some("alloc"), "a struct's concrete allocator field");
    expect(package_name("[package]\nname = \"x\"\n") == Some("x".to_owned()), "a package's name");
    // share-clone's classifier, from E0599's messages.
    let own: BTreeSet<String> = ["Vec", "AVec", "String"].into_iter().map(str::to_owned).collect();
    let site = |m: &str, notes: &str| share_site(m, notes, &own);
    expect(site("no method named `share` found for struct `Vec<u8, Alloc<'_>>` in the current scope", "") == Some(ShareSite::Own), "a blaze-own collection's share");
    expect(site("the method `share` exists for reference `&blaze_own::Vec<u8, Alloc<'_>>`, but its trait bounds were not satisfied", "") == Some(ShareSite::Own), "a reference to one");
    expect(site("no method named `share` found for struct `Vec<u8>` in the current scope", "") == Some(ShareSite::Kept("Vec<u8>".to_owned())), "std's `Vec` keeps `.clone()`");
    expect(site("no method named `share` found for type parameter `T` in the current scope", "") == Some(ShareSite::Kept("T".to_owned())), "a type parameter keeps `.clone()`");
    expect(site("no method named `share` found for struct `Arc<u8>` in the current scope", "items from traits can only be used if the trait is in scope") == Some(ShareSite::NotInScope), "an import not seen");
    expect(site("no method named `push` found for struct `Vec<u8>`", "").is_none(), "another method's error");
    let text = "fn f() {\n    let s = \"é\"; let t = a.share();\n}\n";
    let at = text.find("share").unwrap_or(0);
    let r = share_at_rule("x".to_owned(), text, at, at + 5, "$R.clone()".to_owned());
    expect(r.is_some_and(|r| r.rule.text().contains("\"line\": 1") && r.rule.text().contains("\"column\": 27")), "a place in lines from 0 and columns in characters");

    // The fixture, through ast-grep.
    let here = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/tobstd");
    let tmp = TempDir::new("tobstd-selftest-").unwrap_or_else(|e| fail(SCRIPT, &e.to_string()));
    let copy = |from: &Path, to: &Path| {
        for f in rust_files(from).into_iter().chain(["Cargo.toml".to_owned()]) {
            let target = to.join(&f);
            let made = target.parent().map_or(Ok(()), std::fs::create_dir_all).and_then(|()| std::fs::copy(from.join(&f), &target).map(|_| ()));
            made.unwrap_or_else(|e| fail(SCRIPT, &format!("{f} is not copied: {e}")));
        }
    };
    let table = Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join(TABLE);
    let place = tmp.path().join("tools");
    std::fs::create_dir_all(&place).and_then(|()| std::fs::copy(&table, place.join("std-map.json")).map(|_| ())).unwrap_or_else(|e| fail(SCRIPT, &format!("{TABLE} is not copied: {e}")));
    copy(&here.join("fixture"), &tmp.path().join("fixture"));
    let root = tmp.path().to_string_lossy().into_owned();
    for mode in [Mode::Rename, Mode::AddArg] {
        let a = args("fixture", mode, &root);
        let summary = run_mode(&a).unwrap_or_else(|e| fail(SCRIPT, &e.to_string()));
        let want_dir = here.join("expected").join(mode.name());
        for f in rust_files(&want_dir) {
            let got = std::fs::read_to_string(tmp.path().join("fixture").join(&f)).unwrap_or_default();
            let want = std::fs::read_to_string(want_dir.join(&f)).unwrap_or_default();
            expect(got == want, &format!("{} of {f} differs from expected/{}/{f}:\n{got}", mode.name(), mode.name()));
        }
        let want = std::fs::read_to_string(here.join("expected").join(format!("{}.json", mode.name()))).unwrap_or_default();
        let got = summary.json(&a);
        expect(got.trim() == want.trim(), &format!("{}'s summary differs from expected/{}.json:\n{got}", mode.name(), mode.name()));
    }
    println!("{SCRIPT} selftest: ok");
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.as_slice() {
        [one] if one == "selftest" => selftest(),
        [one] => main_with(&input::parse::<Args>(one, SCRIPT)),
        _ => {
            eprintln!("usage: {SCRIPT} '<json>' | selftest\n\n{DOC}");
            std::process::exit(1);
        }
    }
}
