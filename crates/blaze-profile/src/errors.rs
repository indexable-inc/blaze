//! Errors as rows: each error a build said, one row of the `errors` table (schema.rs), so
//! "derive panics per build" is a query. The daemon gives each error diagnostic it says to
//! [`rows`]; a build's log, as the client printed it, is read the same way by [`from_log`],
//! for builds the history has no rows of.
//!
//! An error's `kind` is a stable code: rustc's own (`E0277`) or a deny lint's name when the
//! error has one, else one of blaze's ([`Kind`]), from what its message says. An error that
//! names functions blaze could not compile ([`REFUSED_SAYS`]) is a row a function named.

use arrow_array::{
    ArrayRef, DictionaryArray, Int64Array, RecordBatch, StringArray, UInt32Array, UInt64Array, types::UInt32Type,
};
use arrow_schema::ArrowError;

use crate::{batches, schema};

/// What the first line of an error naming the functions blaze could not compile says
/// (blaze-cargo's `REFUSED_SAYS`, the same words).
pub const REFUSED_SAYS: &str = "blaze could not compile";

/// The kind of an error without a code of rustc's.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// A derive's proc macro panicked.
    DerivePanic,
    /// An attribute's proc macro panicked.
    AttrPanic,
    /// A function-like proc macro panicked.
    MacroPanic,
    /// blaze could not expand a macro.
    ExpandFailed,
    /// A link found no file of a native library it was given.
    LinkNativeMissing,
    /// A link failed otherwise.
    LinkFailed,
    /// A function blaze could not compile because the references of a template it reaches
    /// could not be found: an impl, a method, an instance.
    TemplateRefs,
    /// A function blaze could not compile for another reason.
    Refused,
    /// The daemon panicked.
    DaemonPanic,
    /// An error none of these is.
    Uncoded,
}

impl Kind {
    pub const ALL: [Kind; 10] = [
        Kind::DerivePanic,
        Kind::AttrPanic,
        Kind::MacroPanic,
        Kind::ExpandFailed,
        Kind::LinkNativeMissing,
        Kind::LinkFailed,
        Kind::TemplateRefs,
        Kind::Refused,
        Kind::DaemonPanic,
        Kind::Uncoded,
    ];

    pub const fn name(self) -> &'static str {
        match self {
            Kind::DerivePanic => "derive-panic",
            Kind::AttrPanic => "attr-panic",
            Kind::MacroPanic => "macro-panic",
            Kind::ExpandFailed => "expand-failed",
            Kind::LinkNativeMissing => "link-native-missing",
            Kind::LinkFailed => "link-failed",
            Kind::TemplateRefs => "template-refs",
            Kind::Refused => "refused",
            Kind::DaemonPanic => "daemon-panic",
            Kind::Uncoded => "uncoded",
        }
    }

    /// The kind of the error whose first line is `message`, which has no code. The refused
    /// functions' error is [`Kind::Refused`]; [`rows`] gives each function named its own.
    pub fn of(message: &str) -> Kind {
        match message {
            "proc-macro derive panicked" => Kind::DerivePanic,
            "custom attribute panicked" => Kind::AttrPanic,
            "proc macro panicked" => Kind::MacroPanic,
            _ if message.starts_with("blaze can't expand ") => Kind::ExpandFailed,
            _ if message.contains("the native library `") && message.contains("` is not found") => {
                Kind::LinkNativeMissing
            }
            _ if message.starts_with("linking `") => Kind::LinkFailed,
            _ if message.starts_with("the daemon panicked at ") => Kind::DaemonPanic,
            _ if refused(message) => Kind::Refused,
            _ => Kind::Uncoded,
        }
    }
}

/// Whether `first`, an error's first line, is that of the refused functions' error:
/// "`<program>` reaches N function(s) blaze could not compile, ...".
fn refused(first: &str) -> bool {
    first.starts_with('`') && first.contains(" reaches ") && first.contains(REFUSED_SAYS)
}

/// A row of the schema's `errors` table.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ErrorRow {
    pub build_id: u64,
    /// When the daemon said it, on the schema's clock ([`crate::clock::mono_ns`]).
    pub t_ns: Option<i64>,
    /// The package of the unit it is of.
    pub krate: Option<String>,
    /// The unit as cargo names it: `serde (build script)`.
    pub unit: Option<String>,
    pub kind: String,
    /// The function or instance it is of: a refused function, a panicking proc macro.
    pub function: Option<String>,
    /// Its first line.
    pub message: String,
    /// Why: its first help or note, or a refused function's reason.
    pub detail: Option<String>,
    pub file: Option<String>,
    pub line: Option<u32>,
}

/// One error a build said, as [`rows`] takes it.
#[derive(Clone, Copy, Debug, Default)]
pub struct Said<'a> {
    /// rustc's code or a lint's name; `None` for blaze's own errors.
    pub code: Option<&'a str>,
    /// Its whole message: a refused functions' error has a line a function after the first.
    pub message: &'a str,
    /// Its first help or note.
    pub detail: Option<&'a str>,
    pub file: Option<&'a str>,
    pub line: Option<u32>,
}

/// What an error is of: the unit, and when it was said.
#[derive(Clone, Copy, Debug, Default)]
pub struct Of<'a> {
    pub build_id: u64,
    pub t_ns: Option<i64>,
    pub krate: Option<&'a str>,
    pub unit: Option<&'a str>,
}

/// The rows of the error `said`, of `of`, added to `out`: one, or one a function a refused
/// functions' error names.
pub fn rows(said: Said<'_>, of: Of<'_>, out: &mut Vec<ErrorRow>) {
    let (first, rest) = said.message.split_once('\n').unwrap_or((said.message, ""));
    let row = |kind: &str, function: Option<&str>, detail: Option<&str>| ErrorRow {
        build_id: of.build_id,
        t_ns: of.t_ns,
        // alloc: a row an error a build said
        krate: of.krate.map(str::to_owned),
        // alloc: a row an error a build said
        unit: of.unit.map(str::to_owned),
        // alloc: a row an error a build said
        kind: kind.to_owned(),
        // alloc: a row an error a build said
        function: function.map(str::to_owned),
        // alloc: a row an error a build said
        message: first.to_owned(),
        // alloc: a row an error a build said
        detail: detail.map(str::to_owned),
        // alloc: a row an error a build said
        file: said.file.map(str::to_owned),
        line: said.line,
    };
    if let Some(code) = said.code.filter(|it| !it.is_empty()) {
        out.push(row(code, None, said.detail));
        return;
    }
    let kind = Kind::of(first);
    if kind != Kind::Refused {
        let function = match kind {
            Kind::DerivePanic | Kind::AttrPanic | Kind::MacroPanic => said.detail.and_then(proc_macro),
            _ => None,
        };
        out.push(row(kind.name(), function.as_deref(), said.detail));
        return;
    }
    let before = out.len();
    for line in rest.lines() {
        let Some(named) = line.strip_prefix("  ") else { continue };
        if named.starts_with("and ") && named.ends_with("names every one)") {
            continue;
        }
        let Some((function, why)) = split_name(named) else { continue };
        let (kind, why) = match innermost(why) {
            Some(reason) => (Kind::TemplateRefs, reason),
            None => (Kind::Refused, why),
        };
        out.push(row(kind.name(), Some(function), Some(why)));
    }
    if out.len() == before {
        out.push(row(Kind::Refused.name(), None, said.detail));
    }
}

/// `serde_derive::Deserialize` of a proc macro's panic's note, "the proc macro `Deserialize`
/// of `serde_derive` stopped ...".
fn proc_macro(detail: &str) -> Option<String> {
    let detail = detail.strip_prefix("message: ").unwrap_or(detail);
    let rest = detail.strip_prefix("the proc macro `")?;
    let (name, rest) = rest.split_once('`')?;
    let rest = rest.strip_prefix(" of `")?;
    let (krate, _) = rest.split_once('`')?;
    // alloc: a row an error a build said
    Some(format!("{krate}::{name}"))
}

/// A refused function's line, `<function>: <why>`, split at the first `: ` outside brackets:
/// an instance's name holds `: ` of its own (`Binder { value: ... }`).
fn split_name(line: &str) -> Option<(&str, &str)> {
    let mut depth = 0i32;
    let bytes = line.as_bytes();
    for (at, byte) in bytes.iter().enumerate() {
        match byte {
            b'(' | b'[' | b'{' | b'<' => depth += 1,
            b')' | b']' | b'}' => depth -= 1,
            b'>' if at > 0 && bytes[at - 1] != b'-' => depth -= 1,
            b':' if depth == 0 && bytes.get(at + 1) == Some(&b' ') => {
                return Some((&line[..at], &line[at + 2..]));
            }
            _ => {}
        }
    }
    None
}

/// The reason under a chain of "the references of <instance>: ..." the template's references
/// failed for; `None` when `why` is not one.
fn innermost(why: &str) -> Option<&str> {
    let mut why = why.strip_prefix("the references of ")?;
    loop {
        let (_, rest) = split_name(why)?;
        match rest.strip_prefix("the references of ") {
            Some(deeper) => why = deeper,
            None => return Some(rest),
        }
    }
}

/// The errors of a build's log as the client printed it: cargo's lines (`error[E0277]: ...`,
/// `  --> file:line:col`, `  = help: ...`), each error given the unit of the
/// "could not compile `<crate>` (<target>)" after it. Its rows are of build 0.
pub fn from_log(log: &str) -> Vec<ErrorRow> {
    // alloc: once a log read
    let mut out = Vec::new();
    // The rows said since the last "could not compile", given its unit there.
    let mut unowned = 0;
    let mut open: Option<Open<'_>> = None;
    let mut at = 0;
    for chunk in log.split_inclusive('\n') {
        let start = at;
        at += chunk.len();
        let line = chunk.trim_end_matches(['\n', '\r']);
        if let Some((code, message)) = header(line, "error") {
            if let Some(it) = open.take() {
                it.close(log, &mut out);
            }
            if let Some(unit) = message.strip_prefix("could not compile `") {
                let (krate, rest) = unit.split_once('`').unwrap_or((unit, ""));
                let target = rest.trim_start().strip_prefix('(').and_then(|it| it.split_once(')'));
                // alloc: a row an error a build said
                let unit = target.map_or_else(|| krate.to_owned(), |(target, _)| format!("{krate} ({target})"));
                for row in &mut out[unowned..] {
                    // alloc: a row an error a build said
                    row.krate = Some(krate.to_owned());
                    // clone: a row an error a build said
                    row.unit = Some(unit.clone());
                }
                unowned = out.len();
                continue;
            }
            let from = start + (line.len() - message.len());
            open = Some(Open { said: Said { code, message, ..Said::default() }, from, end: start + line.len() });
            continue;
        }
        if line.is_empty() || !(line.starts_with(' ') || gutter(line)) {
            if let Some(it) = open.take() {
                it.close(log, &mut out);
            }
            continue;
        }
        let Some(it) = open.as_mut() else { continue };
        let trimmed = line.trim_start();
        if refused(it.said.message) {
            it.end = start + line.len();
        } else if let Some(place) = trimmed.strip_prefix("--> ") {
            if it.said.file.is_none() {
                let mut parts = place.rsplitn(3, ':');
                let (_, line, file) = (parts.next(), parts.next(), parts.next());
                it.said.file = file.or(Some(place));
                it.said.line = line.and_then(|it| it.parse().ok());
            }
        } else if let Some(note) = trimmed.strip_prefix("= help: ").or_else(|| trimmed.strip_prefix("= note: ")) {
            it.said.detail.get_or_insert(note.strip_prefix("message: ").unwrap_or(note));
        }
    }
    if let Some(it) = open.take() {
        it.close(log, &mut out);
    }
    out
}

/// An error being read from a log: what it said so far, and where in the log its message
/// begins and its last line ends (a refused functions' error's message is every line of it).
struct Open<'a> {
    said: Said<'a>,
    from: usize,
    end: usize,
}

impl Open<'_> {
    fn close(self, log: &str, out: &mut Vec<ErrorRow>) {
        let message = match refused(self.said.message) {
            true => &log[self.from..self.end],
            false => self.said.message,
        };
        rows(Said { message, ..self.said }, Of::default(), out);
    }
}

/// Whether `line` is a source line of a diagnostic: `49 |     #[derive(...)]`.
fn gutter(line: &str) -> bool {
    let rest = line.trim_start_matches(|it: char| it.is_ascii_digit());
    rest.len() < line.len() && rest.trim_start().starts_with('|')
}

/// `(code, message)` of a line `<level>: <message>` or `<level>[<code>]: <message>`.
fn header<'a>(line: &'a str, level: &str) -> Option<(Option<&'a str>, &'a str)> {
    let rest = line.strip_prefix(level)?;
    if let Some(message) = rest.strip_prefix(": ") {
        return Some((None, message));
    }
    let (code, message) = rest.strip_prefix('[')?.split_once("]: ")?;
    Some((Some(code), message))
}

fn column(array: impl arrow_array::Array + 'static) -> ArrayRef {
    // alloc: a batch's column, once a flush
    std::sync::Arc::new(array)
}

fn dict<'a>(values: impl Iterator<Item = Option<&'a str>>) -> ArrayRef {
    column(values.collect::<DictionaryArray<UInt32Type>>())
}

/// The `errors` rows of `rows`.
pub fn batch(rows: &[ErrorRow]) -> Result<RecordBatch, ArrowError> {
    // alloc: a batch's columns, once a flush
    let columns: Vec<ArrayRef> = vec![
        column(UInt64Array::from_iter_values(rows.iter().map(|it| it.build_id))),
        column(Int64Array::from_iter(rows.iter().map(|it| it.t_ns))),
        dict(rows.iter().map(|it| it.krate.as_deref())),
        column(StringArray::from_iter(rows.iter().map(|it| it.unit.as_deref()))),
        dict(rows.iter().map(|it| Some(it.kind.as_str()))),
        column(StringArray::from_iter(rows.iter().map(|it| it.function.as_deref()))),
        column(StringArray::from_iter_values(rows.iter().map(|it| it.message.as_str()))),
        column(StringArray::from_iter(rows.iter().map(|it| it.detail.as_deref()))),
        column(StringArray::from_iter(rows.iter().map(|it| it.file.as_deref()))),
        column(UInt32Array::from_iter(rows.iter().map(|it| it.line))),
    ];
    RecordBatch::try_new(batches::schema_of(&schema::ERRORS), columns)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A log with each kind of error the self-build logs of 2026-11-01 have, in their words.
    const LOG: &str = "\
   Compiling stacker v0.1.21
error: linking `/t/debug/build/stacker/5d3b/out/build_script_build`: finding the native inputs: linking: the native library `iconv` is not found: no libiconv.tbd in /sdk/usr/lib
error: could not compile `stacker` (build script) due to 1 previous error
   Compiling libm v0.2.16
error: `build-script-build` reaches 3 functions blaze could not compile, and a build with nothing of rustc's has nothing else to compile them, so it is not built:
  HirInstance(Code(Instance { def: Fn(FunctionId(1d3eb)), args: [f16] })): the references of HirInstance(Code(Instance { def: Fn(FunctionId(1d3eb)), args: [f16] })): trait Float method is_sign_positive for [f16]: the impl found has no method
  Fn(f2): a body blaze does not lower
  and 1 more (`--refused all` names every one)
warning: `libm` (build script) generated 1 warning
error: could not compile `libm` (build script) due to 1 previous error; 1 warning emitted
error: proc-macro derive panicked
  --> crates/blaze-protocol/src/lib.rs:49:14
   |
49 |     #[derive(Deserialize, Debug)]
   |              ^^^^^^^^^^^
   |
   = help: message: the proc macro `Deserialize` of `serde_derive` stopped in the evaluator: NotSupported(\"unsizing\")
             in `std::panicking::take_hook`

error[E0277]: the trait bound `u8: Foo` is not satisfied
   --> crates/blaze-core/src/threads.rs:396:20
    |
396 |     spare: Vec<u8>,
    |
    = note: required by a bound

error: could not compile `blaze-protocol` (lib) due to 2 previous errors
error: the daemon panicked at crates/blaze-diagnostics/src/api.rs:120:13 on thread blaze-work-0: a span at byte 12114
blaze time: 279376.8 ms: client 279376.7 (100%), unaccounted 0.0 (0%)
";

    fn kinds(rows: &[ErrorRow]) -> Vec<(&str, Option<&str>, Option<&str>)> {
        rows.iter().map(|it| (it.kind.as_str(), it.unit.as_deref(), it.function.as_deref())).collect()
    }

    #[test]
    fn a_log_is_a_row_an_error_each_of_its_unit() {
        let rows = from_log(LOG);
        let instance = "HirInstance(Code(Instance { def: Fn(FunctionId(1d3eb)), args: [f16] }))";
        assert_eq!(
            kinds(&rows),
            [
                ("link-native-missing", Some("stacker (build script)"), None),
                ("template-refs", Some("libm (build script)"), Some(instance)),
                ("refused", Some("libm (build script)"), Some("Fn(f2)")),
                ("derive-panic", Some("blaze-protocol (lib)"), Some("serde_derive::Deserialize")),
                ("E0277", Some("blaze-protocol (lib)"), None),
                ("daemon-panic", None, None),
            ]
        );
        assert_eq!(rows[1].detail.as_deref(), Some("trait Float method is_sign_positive for [f16]: the impl found has no method"));
        assert_eq!(rows[1].krate.as_deref(), Some("libm"));
        assert!(rows[1].message.starts_with("`build-script-build` reaches 3 functions"), "{}", rows[1].message);
        assert_eq!(rows[2].detail.as_deref(), Some("a body blaze does not lower"));
        let derive = &rows[3];
        assert_eq!((derive.file.as_deref(), derive.line), (Some("crates/blaze-protocol/src/lib.rs"), Some(49)));
        assert!(derive.detail.as_deref().is_some_and(|it| it.starts_with("the proc macro `Deserialize`")));
        assert_eq!((rows[4].file.as_deref(), rows[4].line), (Some("crates/blaze-core/src/threads.rs"), Some(396)));
        assert_eq!(rows[4].detail.as_deref(), Some("required by a bound"));
    }

    #[test]
    fn a_said_error_with_a_code_is_of_that_code() {
        let mut out = Vec::new();
        let of = Of { build_id: 7, t_ns: Some(5), krate: Some("app"), unit: Some("app (lib)") };
        rows(Said { code: Some("E0411"), message: "cannot find type `Self`", ..Said::default() }, of, &mut out);
        rows(Said { code: Some("unused_must_use"), message: "unused result", ..Said::default() }, of, &mut out);
        rows(Said { message: "something new", ..Said::default() }, of, &mut out);
        assert_eq!(
            out.iter().map(|it| (it.build_id, it.kind.as_str(), it.krate.as_deref())).collect::<Vec<_>>(),
            [(7, "E0411", Some("app")), (7, "unused_must_use", Some("app")), (7, "uncoded", Some("app"))]
        );
    }

    #[test]
    fn every_kind_has_its_own_name() {
        let names: std::collections::BTreeSet<&str> = Kind::ALL.iter().map(|it| it.name()).collect();
        assert_eq!(names.len(), Kind::ALL.len());
        for kind in Kind::ALL {
            assert!(schema::ERRORS.columns.iter().any(|it| it.name == "kind" && it.doc.contains(kind.name())), "{kind:?}");
        }
    }

    #[test]
    fn the_batch_is_of_the_errors_table() {
        let rows = from_log(LOG);
        let batch = batch(&rows).unwrap();
        assert_eq!(batch.num_rows(), rows.len());
        assert_eq!(batch.schema(), batches::schema_of(&schema::ERRORS));
    }
}
