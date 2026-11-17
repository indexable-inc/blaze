//! What came of a test, decided from what blaze and the program did. Pure: the plan and what
//! was observed in, a [`Verdict`] out. What is compared is what is right whoever compiles it:
//! accepted or refused, an error at the line the test marks (with the code where the test
//! gives one), how the program ended and what it printed. rustc's wording is never compared.

use super::directives::{Annotation, Level, cut};
use super::plan::{Ends, Kind, Plan};
use blaze_script::regex::Regex;

/// One diagnostic blaze reported.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Diagnostic {
    /// `error`, `warning`, ..., as the message says.
    pub level: String,
    pub code: Option<String>,
    pub message: String,
    /// The lines of the test's own file its primary spans are on, and the lines of the macro
    /// calls those spans were expanded from.
    pub lines: Vec<usize>,
}

/// One `blaze check` or `blaze build`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Compiled {
    pub accepted: bool,
    pub diagnostics: Vec<Diagnostic>,
    /// What it wrote to standard error, and each message as blaze renders it.
    pub said: String,
    /// The first line that says blaze itself panicked or its daemon went away.
    pub panic: Option<String>,
    pub timed_out: bool,
}

/// The program, run.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Ran {
    /// Its exit status; `None` when a signal ended it.
    pub status: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub timed_out: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Verdict {
    Pass,
    /// `cause` is short and the same for every test that fails the same way; `detail` is the
    /// first difference of this one.
    Fail { cause: String, detail: String },
    Panic { cause: String, detail: String },
    Timeout { step: String },
}

/// `text` as the name of a group: what it quotes and its numbers taken out, ASCII, cut short.
pub(crate) fn group(text: &str) -> String {
    let mut out = String::new();
    let mut quoted = false;
    for c in text.chars() {
        match c {
            '`' => {
                quoted = !quoted;
                if quoted {
                    out.push_str("`..`");
                }
            }
            _ if quoted => {}
            c if c.is_ascii_digit() => {
                if !out.ends_with('N') {
                    out.push('N');
                }
            }
            c if c.is_ascii_graphic() || c == ' ' => out.push(c),
            _ => out.push('?'),
        }
    }
    cut(out.trim(), 110).to_owned()
}

fn shown(it: &Diagnostic) -> String {
    let lines: Vec<String> = it.lines.iter().map(usize::to_string).collect();
    format!("{}{} at line {}: {}", it.level, it.code.as_ref().map(|it| format!("[{it}]")).unwrap_or_default(), lines.join(","), cut(&it.message, 160))
}

/// The first error of `compiled` as a group's name.
fn first_error(compiled: &Compiled) -> String {
    match compiled.diagnostics.iter().find(|it| it.level == "error") {
        Some(it) => match &it.code {
            Some(code) => format!("{code}: {}", group(&it.message)),
            None => group(&it.message),
        },
        None => {
            let line = compiled.said.lines().map(str::trim).find(|it| it.starts_with("error")).unwrap_or("no error line");
            group(line)
        }
    }
}

/// A panic's line as a group's name: where it panicked, without the numbers of the place.
fn panic_group(line: &str) -> String {
    let at = Regex::new(r"panicked at ([^:\s]+):\d+:\d+").expect("a regex that compiles");
    match at.captures(line) {
        Some(found) => format!("panic at {}", &found[1]),
        None => group(line),
    }
}

/// What every step that compiles may end in before its own expectation is read: a panic or a
/// timeout.
fn broke(step: &str, compiled: &Compiled) -> Option<Verdict> {
    if let Some(line) = &compiled.panic {
        return Some(Verdict::Panic { cause: panic_group(line), detail: format!("{step}: {}", cut(line, 400)) });
    }
    compiled.timed_out.then(|| Verdict::Timeout { step: step.to_owned() })
}

fn level_word(level: Level) -> &'static str {
    match level {
        Level::Error => "error",
        _ => "warning",
    }
}

/// The first annotation no diagnostic of `compiled` answers: one of its level on its line,
/// with its code where the annotation gives one.
fn unanswered<'a>(annotations: &'a [Annotation], compiled: &Compiled) -> Option<&'a Annotation> {
    annotations.iter().find(|wanted| {
        !compiled.diagnostics.iter().any(|it| {
            it.level == level_word(wanted.level)
                && wanted.line.is_none_or(|line| it.lines.contains(&line))
                && wanted.code.as_ref().is_none_or(|code| it.code.as_ref() == Some(code))
        })
    })
}

fn missing(step: &str, wanted: &Annotation, compiled: &Compiled) -> Verdict {
    let same_level: Vec<&Diagnostic> = compiled.diagnostics.iter().filter(|it| it.level == level_word(wanted.level)).collect();
    let said: Vec<String> = same_level.iter().take(4).map(|it| shown(it)).collect();
    let place = match wanted.line {
        Some(line) => format!("line {line}"),
        None => "any line".to_owned(),
    };
    let cause = match (wanted.level, same_level.first()) {
        (Level::Error, Some(first)) if wanted.line.is_some_and(|line| same_level.iter().all(|it| !it.lines.contains(&line))) => {
            format!("no error at the marked line; blaze's first: {}", first.code.clone().unwrap_or_else(|| group(&first.message)))
        }
        (Level::Error, Some(_)) => format!("an error at the marked line has another code than {}", wanted.code.as_deref().unwrap_or("?")),
        (Level::Error, None) => "refused with no error diagnostic".to_owned(),
        (_, _) => "no warning at the marked line".to_owned(),
    };
    Verdict::Fail {
        cause,
        detail: format!("{step}: {place} expects {} `{}`; blaze reported {} of that level: {}", wanted.level.word(), cut(&wanted.text, 120), same_level.len(), said.join(" | ")),
    }
}

/// A step that must be accepted.
pub(crate) fn accepted(step: &str, plan: &Plan, compiled: &Compiled) -> Option<Verdict> {
    if let Some(it) = broke(step, compiled) {
        return Some(it);
    }
    if !compiled.accepted {
        let said: Vec<String> = compiled.diagnostics.iter().filter(|it| it.level == "error").take(3).map(shown).collect();
        let detail = match said.is_empty() {
            true => format!("{step}: {}", cut(compiled.said.trim(), 400)),
            false => format!("{step}: {}", said.join(" | ")),
        };
        return Some(Verdict::Fail { cause: format!("refuses: {}", first_error(compiled)), detail });
    }
    let warnings: Vec<Annotation> = plan.annotations.iter().filter(|it| it.level == Level::Warning).cloned().collect();
    unanswered(&warnings, compiled).map(|wanted| missing(step, wanted, compiled))
}

/// A step of a test that must be refused: `None` when this step accepted it (the caller goes
/// on to a build, or fails it), else the verdict.
pub(crate) fn refused(step: &str, plan: &Plan, compiled: &Compiled) -> Option<Verdict> {
    if let Some(it) = broke(step, compiled) {
        return Some(it);
    }
    if compiled.accepted {
        return None;
    }
    Some(match unanswered(&plan.annotations, compiled) {
        Some(wanted) => missing(step, wanted, compiled),
        None => Verdict::Pass,
    })
}

/// A test that must be refused and was accepted by every step tried.
pub(crate) fn accepts(plan: &Plan) -> Verdict {
    let first = plan.annotations.iter().find(|it| it.level == Level::Error);
    let cause = match first.and_then(|it| it.code.as_ref()) {
        Some(code) => format!("accepts what must be refused ({code})"),
        None => "accepts what must be refused".to_owned(),
    };
    let detail = match first {
        Some(it) => format!("expects ERROR `{}` at {}", cut(&it.text, 160), it.line.map_or("any line".to_owned(), |line| format!("line {line}"))),
        None => "the test marks no line".to_owned(),
    };
    Verdict::Fail { cause, detail }
}

/// A step of a test that only has to be survived: `None` when it was.
pub(crate) fn survived(step: &str, compiled: &Compiled) -> Option<Verdict> {
    broke(step, compiled)
}

/// The program's run against what the plan says of it; `expected_stdout` is the text of the
/// plan's `run.stdout` file (empty where the file is not there).
pub(crate) fn ran(plan: &Plan, ran: &Ran, expected_stdout: Option<&str>) -> Verdict {
    if ran.timed_out {
        return Verdict::Timeout { step: "run".to_owned() };
    }
    let ended = match ran.status {
        Some(0) => "exit 0".to_owned(),
        Some(status) => format!("exit {status}"),
        None => "a signal".to_owned(),
    };
    let tail = || {
        let text = ran.stderr.trim();
        let from = text.char_indices().rev().nth(399).map_or(0, |it| it.0);
        text[from..].to_owned()
    };
    let wrong = |wanted: &str| Verdict::Fail { cause: format!("run: ends with {ended}, not {wanted}"), detail: tail() };
    match plan.kind {
        Kind::RunPass if ran.status != Some(0) => return wrong("success"),
        Kind::RunFail(ends) => {
            let failed = ran.status.is_some_and(|it| (1..=127).contains(&it));
            let crashed = ran.status != Some(0) && !failed;
            match ends {
                Ends::Fail if !failed => return wrong("a failure status"),
                Ends::Crash if !crashed => return wrong("a crash"),
                Ends::Either if !failed && !crashed => return wrong("a failure or a crash"),
                _ => {}
            }
        }
        _ => {}
    }
    let output = format!("{}{}", ran.stdout, ran.stderr);
    for pattern in &plan.run.patterns {
        if !output.contains(pattern.as_str()) {
            return Verdict::Fail { cause: "run: its output lacks the test's error-pattern".to_owned(), detail: format!("`{pattern}` is not in: {}", tail()) };
        }
    }
    for pattern in &plan.run.regex_patterns {
        match Regex::new(pattern) {
            Ok(regex) if regex.is_match(&output) => {}
            Ok(_) => return Verdict::Fail { cause: "run: its output lacks the test's error-pattern".to_owned(), detail: format!("`{pattern}` matches nothing in: {}", tail()) },
            Err(why) => return Verdict::Fail { cause: "run: the test's regex-error-pattern is no regex here".to_owned(), detail: why.to_string() },
        }
    }
    if let Some(expected) = expected_stdout
        && expected != ran.stdout
    {
        let at = expected.lines().zip(ran.stdout.lines()).position(|(a, b)| a != b).unwrap_or(expected.lines().count().min(ran.stdout.lines().count()));
        return Verdict::Fail {
            cause: "run: stdout is not the test's .run.stdout".to_owned(),
            detail: format!("line {}: expected `{}`, printed `{}`", at + 1, cut(expected.lines().nth(at).unwrap_or(""), 120), cut(ran.stdout.lines().nth(at).unwrap_or(""), 120)),
        };
    }
    Verdict::Pass
}

#[cfg(test)]
mod tests {
    use super::super::directives::parse;
    use super::super::plan::{Host, Planned, Suite, plan};
    use super::*;

    fn planned(source: &str) -> Plan {
        let host = Host { arch: "aarch64".to_owned(), os: "linux".to_owned() };
        match plan(Suite::Ui, "t.rs", source, &parse(source), None, &host, &|_| None) {
            Planned::Run(plan) => *plan,
            other => panic!("{other:?}"),
        }
    }

    fn error(line: usize, code: Option<&str>) -> Diagnostic {
        Diagnostic { level: "error".to_owned(), code: code.map(str::to_owned), message: "mismatched `u8` and 16 things".to_owned(), lines: vec![line] }
    }

    fn refusal(diagnostics: Vec<Diagnostic>) -> Compiled {
        Compiled { accepted: false, diagnostics, ..Compiled::default() }
    }

    /// The invariant: a test that must be refused passes only when blaze refuses it with an
    /// error on every line the test marks, with the code where the test gives one.
    #[test]
    fn a_refusal_answers_every_marked_line() {
        let plan = planned("fn main() {\n    let x: u8 = 1u16; //~ ERROR E0308\n    f(); //~ ERROR cannot find\n}\n");
        assert_eq!(refused("check", &plan, &refusal(vec![error(2, Some("E0308")), error(3, Some("E0425"))])), Some(Verdict::Pass));
        assert_eq!(refused("check", &plan, &Compiled { accepted: true, ..Compiled::default() }), None);
        let other_line = refused("check", &plan, &refusal(vec![error(2, Some("E0308")), error(4, Some("E0425"))]));
        assert!(matches!(&other_line, Some(Verdict::Fail { cause, .. }) if cause == "no error at the marked line; blaze's first: E0308"), "{other_line:?}");
        let other_code = refused("check", &plan, &refusal(vec![error(2, Some("E0277")), error(3, None)]));
        assert!(matches!(&other_code, Some(Verdict::Fail { cause, .. }) if cause == "an error at the marked line has another code than E0308"), "{other_code:?}");
        let none = refused("check", &plan, &refusal(Vec::new()));
        assert!(matches!(&none, Some(Verdict::Fail { cause, .. }) if cause == "refused with no error diagnostic"), "{none:?}");
        assert!(matches!(accepts(&plan), Verdict::Fail { cause, .. } if cause == "accepts what must be refused (E0308)"));
    }

    /// The control of `a_refusal_answers_every_marked_line`: the reading that looks only at
    /// whether blaze refused (any error anywhere passes) is the bug the annotations are for.
    #[test]
    fn the_oracle_catches_a_refusal_for_another_reason() {
        let plan = planned("fn main() {\n    let x: u8 = 1u16; //~ ERROR E0308\n}\n");
        let elsewhere = refusal(vec![Diagnostic { level: "error".to_owned(), code: None, message: "blaze cannot do this yet".to_owned(), lines: vec![1] }]);
        assert!(!elsewhere.accepted, "the planted reading would pass here");
        assert!(matches!(refused("check", &plan, &elsewhere), Some(Verdict::Fail { .. })));
    }

    #[test]
    fn a_panic_or_a_timeout_is_never_a_refusal() {
        let plan = planned("fn main() {} //~ ERROR x\n");
        let panicked = Compiled { panic: Some("thread 'w' panicked at crates/hir-ty/src/x.rs:10:5: boom".to_owned()), ..Compiled::default() };
        assert_eq!(
            refused("check", &plan, &panicked),
            Some(Verdict::Panic { cause: "panic at crates/hir-ty/src/x.rs".to_owned(), detail: "check: thread 'w' panicked at crates/hir-ty/src/x.rs:10:5: boom".to_owned() })
        );
        assert_eq!(survived("build", &Compiled { timed_out: true, ..Compiled::default() }), Some(Verdict::Timeout { step: "build".to_owned() }));
        assert_eq!(survived("check", &refusal(vec![error(1, None)])), None);
    }

    #[test]
    fn a_test_that_must_pass_names_what_refused_it() {
        let plan = planned("//@ check-pass\nfn main() {} //~ WARN unused\n");
        let verdict = accepted("check", &plan, &refusal(vec![error(2, Some("E0308"))]));
        assert!(matches!(&verdict, Some(Verdict::Fail { cause, .. }) if cause == "refuses: E0308: mismatched `..` and N things"), "{verdict:?}");
        let quiet = accepted("check", &plan, &Compiled { accepted: true, ..Compiled::default() });
        assert!(matches!(&quiet, Some(Verdict::Fail { cause, .. }) if cause == "no warning at the marked line"), "{quiet:?}");
        let warned = Compiled { accepted: true, diagnostics: vec![Diagnostic { level: "warning".to_owned(), code: None, message: "unused".to_owned(), lines: vec![2] }], ..Compiled::default() };
        assert_eq!(accepted("check", &plan, &warned), None);
    }

    #[test]
    fn a_run_is_held_to_how_it_ends_and_what_it_prints() {
        let pass = planned("//@ run-pass\n//@ check-run-results\nfn main() {}\n");
        let ok = Ran { status: Some(0), stdout: "a\nb\n".to_owned(), ..Ran::default() };
        assert_eq!(ran(&pass, &ok, Some("a\nb\n")), Verdict::Pass);
        assert!(matches!(ran(&pass, &ok, Some("a\nc\n")), Verdict::Fail { cause, detail } if cause == "run: stdout is not the test's .run.stdout" && detail.starts_with("line 2:")));
        assert!(matches!(ran(&pass, &Ran { status: Some(101), ..Ran::default() }, None), Verdict::Fail { cause, .. } if cause == "run: ends with exit 101, not success"));
        assert_eq!(ran(&pass, &Ran { timed_out: true, ..Ran::default() }, None), Verdict::Timeout { step: "run".to_owned() });
        let fail = planned("//@ run-fail\n//@ error-pattern: explicit panic\nfn main() {}\n");
        let panicked = Ran { status: Some(101), stderr: "thread 'main' panicked: explicit panic".to_owned(), ..Ran::default() };
        assert_eq!(ran(&fail, &panicked, None), Verdict::Pass);
        assert!(matches!(ran(&fail, &Ran { status: None, ..panicked.clone() }, None), Verdict::Fail { cause, .. } if cause == "run: ends with a signal, not a failure status"));
        assert!(matches!(ran(&fail, &Ran { stderr: String::new(), ..panicked }, None), Verdict::Fail { cause, .. } if cause.contains("error-pattern")));
        let crash = planned("//@ run-crash\nfn main() {}\n");
        assert_eq!(ran(&crash, &Ran { status: None, ..Ran::default() }, None), Verdict::Pass);
        assert!(matches!(ran(&crash, &Ran { status: Some(1), ..Ran::default() }, None), Verdict::Fail { .. }));
    }

    #[test]
    fn groups_drop_what_differs_between_tests() {
        assert_eq!(group("the trait bound `Foo<u8>: Bar` is not satisfied in 12 places (émigré)"), "the trait bound `..` is not satisfied in N places (?migr?)");
    }
}
