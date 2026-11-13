//! `blaze diagnostics [--since TOKEN] [-p PKG]... [--file FILE] [--client NAME] [--wait S]
//! [--json]`: what a check of the checkout says that is new since the check `TOKEN` names
//! ([`Request::Diagnostics`], docs/agents.md), with what was fixed and what is unchanged, and
//! the token of this check to pass next. The base each token names is the daemon's, kept in
//! its session ([`blaze_api::diagnostics::Bases`]): a token the daemon does not hold (it
//! restarted, or the base was evicted) is said as unknown, and everything as new.
//!
//! Each diagnostic has an id equal across an edit that only moves it: today the hash of its
//! level, code, file, message with line numbers taken out, the text of its first line and its
//! label ([`blaze_api::diagnostics::id`]). The end state is the id relative to the item it is
//! in (the item's path and the span's offset in it), so an edit of the line itself keeps it
//! too; that needs the front end to say the item of a span, which diagnostics do not carry yet.
//!
//! Errors of every package are said, warnings only of the packages asked for (a dependency's
//! warnings are not the edit's). `blaze agents hook` is this command's other client.

use std::{
    ffi::OsString,
    fmt::Write as _,
    path::{Component, Path, PathBuf},
};

use blaze_api::{Base, Found, Level, Request, ResponseRef, Seen};
use blaze_core::exit::Exit;
use blaze_ids::ContentId;

use super::{
    args::{self, Asked},
    daemon::Start,
    observe, reach, say,
};

const USAGE: &str = "blaze diagnostics [--since TOKEN] [-p PKG]... [--workspace] [--manifest-path PATH] [--file FILE] [--client NAME] [--wait SECONDS] [--json]";

/// How long a check is waited for when `--wait` says nothing: the hook's deadline.
pub(super) const WAIT_MS: u64 = 20_000;

/// `blaze diagnostics`: its exit code: 0 when the check ended and says no error now, else
/// [`Exit::Failed`] (an error said, the check still running, or blaze failing in it).
pub fn main(words: Vec<OsString>, color: bool) -> i32 {
    match diagnostics(words, color) {
        Ok(code) => code,
        Err((what, code)) => {
            blaze_report::err!("{}", say::error(&what, color));
            code
        }
    }
}

/// The command line `blaze diagnostics` was given, read.
#[derive(Debug, PartialEq, Default)]
struct Line {
    since: Option<ContentId>,
    /// The words of the `blaze check` whose targets are checked: `check` and the cargo flags.
    check: Vec<OsString>,
    file: Option<String>,
    client: Option<String>,
    wait_ms: Option<u64>,
    json: bool,
}

fn read(words: Vec<OsString>) -> Result<Line, String> {
    let mut line = Line { check: vec![OsString::from("check")], ..Line::default() };
    let mut words = words.into_iter();
    while let Some(word) = words.next() {
        let Some(word) = word.to_str() else {
            return Err(format!("{} is not UTF-8", word.to_string_lossy()));
        };
        let mut value = |flag: &str| -> Result<String, String> {
            let next = words.next().ok_or_else(|| format!("`{flag}` takes a value"))?;
            next.into_string().map_err(|it| format!("`{flag}`'s {} is not UTF-8", it.to_string_lossy()))
        };
        match word {
            "--json" => line.json = true,
            "--since" => {
                let token = value("--since")?;
                line.since = Some(
                    reach::root_of(&token)
                        .ok_or_else(|| format!("`{token}` is no token: 64 hex digits"))?,
                );
            }
            "--file" => line.file = Some(value("--file")?),
            "--client" => line.client = Some(value("--client")?),
            "--wait" => {
                let seconds = value("--wait")?;
                let seconds: u64 =
                    seconds.parse().map_err(|_| format!("`--wait {seconds}`: whole seconds"))?;
                line.wait_ms = Some(seconds.saturating_mul(1000));
            }
            "-p" | "--package" | "--manifest-path" | "--features" | "-F" | "--profile"
            | "--target" => {
                let it = value(word)?;
                line.check.push(word.into());
                line.check.push(it.into());
            }
            "--workspace" | "--all-targets" | "--lib" | "--bins" | "--tests" | "--examples"
            | "--benches" | "--release" | "--all-features" | "--no-default-features" => {
                line.check.push(word.into())
            }
            _ => return Err(format!("not `{word}`")),
        }
    }
    Ok(line)
}

fn diagnostics(words: Vec<OsString>, color: bool) -> Result<i32, (String, i32)> {
    let line = read(words).map_err(|what| (format!("{USAGE}: {what}"), Exit::Usage.code()))?;
    let failed = |what: String| (what, Exit::Failed.code());
    let client = line.client.as_deref().unwrap_or("cli");
    let (seen, head) =
        ask(line.check, line.since, client, line.wait_ms.unwrap_or(WAIT_MS), color).map_err(failed)?;
    let file = match &line.file {
        None => None,
        Some(file) => {
            let cwd = std::env::current_dir()
                .map_err(|error| failed(format!("the current directory: {error}")))?;
            Some(in_checkout(&cwd, Path::new(file), Path::new(&head)).ok_or_else(|| {
                failed(format!("--file {file} is not in the checkout at {head}"))
            })?)
        }
    };
    let text = match line.json {
        true => format!("{}\n", json(&seen, file.as_deref())),
        false => text(&seen, file.as_deref()),
    };
    blaze_report::out!("{text}");
    let now_errors = seen
        .new
        .iter()
        .chain(&seen.unchanged)
        .filter(|it| it.level == Level::Error && file.as_deref().is_none_or(|f| of_file(it, f)))
        .count();
    Ok(match seen.failed.is_none() && seen.ended && now_errors == 0 {
        true => 0,
        false => Exit::Failed.code(),
    })
}

/// The daemon's answer to a check of `check`'s targets since `since` for `client`, waited for
/// `wait_ms`, with the checkout's head: the daemon started when none runs.
pub(super) fn ask(
    check: Vec<OsString>,
    since: Option<ContentId>,
    client: &str,
    wait_ms: u64,
    color: bool,
) -> Result<(Seen, String), String> {
    let asked = match args::parse(check) {
        Ok(Asked::Cargo(asked)) => asked,
        Ok(_) => return Err("`blaze diagnostics` could not read itself as `blaze check`".to_owned()),
        Err(usage) => return Err(args::refusal(&usage, false)),
    };
    let (_, cargo) = super::prepared_cargo(&asked, color)?;
    let head = cargo.root.clone(); // clone: the head's name, beside the request that owns it
    let Request::Check { view, targets } = super::request(&asked, cargo)? else {
        return Err("`blaze diagnostics` made no check's targets of `blaze check`".to_owned());
    };
    crate::toolchain::ensure()?;
    // alloc: CLI edge, the request, once
    let request = Request::Diagnostics { view, since, client: client.to_owned(), targets, wait_ms };
    let mut seen = None;
    let asked = observe::ask(request, Start::IfMissing, |response| {
        if let ResponseRef::Diagnostics(it) = response {
            seen = Some(it);
        }
        Ok(())
    });
    // A check blaze failed in is answered with its `failed` and a `Done` not ok: the answer
    // says it, and is what is shown. No answer at all is the request's own failure.
    match (seen, asked) {
        (Some(seen), _) => Ok((seen, head)),
        (None, Err(error)) => Err(error),
        (None, Ok(())) => Err("the daemon answered `blaze diagnostics` with no answer".to_owned()),
    }
}

/// Whether `found` is said: every error, and a warning only of a package asked for.
pub(super) fn said(found: &Found) -> bool {
    found.level == Level::Error || found.primary
}

/// Whether `found`'s span is in `file` (relative to the checkout's root).
pub(super) fn of_file(found: &Found, file: &str) -> bool {
    found.span.as_ref().is_some_and(|it| it.file == file)
}

/// `file` (relative to `cwd`, or absolute) relative to the checkout `head`, made lexically
/// (no file is read); `None` outside it.
pub(super) fn in_checkout(cwd: &Path, file: &Path, head: &Path) -> Option<String> {
    let mut whole = PathBuf::new();
    for part in cwd.join(file).components() {
        match part {
            Component::ParentDir => {
                whole.pop();
            }
            Component::CurDir => {}
            part => whole.push(part),
        }
    }
    let rest = whole.strip_prefix(head).ok()?;
    Some(rest.to_str()?.to_owned()) // alloc: CLI edge, a name, once
}

/// `found` as one line and its children's under it: `file:line:col: level[code]: message`.
pub(super) fn draw(found: &Found, out: &mut String) {
    match &found.span {
        Some(span) => {
            let _ = write!(out, "{}:{}:{}: ", span.file, span.line, span.column);
        }
        None => {}
    }
    out.push_str(found.level.name());
    if let Some(code) = &found.code {
        let _ = write!(out, "[{code}]");
    }
    let _ = write!(out, ": {}", found.message);
    for help in &found.help {
        let _ = write!(out, "\n  {help}");
    }
}

/// The base a token named, as a word.
pub(super) fn base_name(base: Base) -> &'static str {
    match base {
        Base::First => "first",
        Base::Held => "held",
        Base::Unknown => "unknown",
    }
}

/// `seen` as lines: the new, each drawn; the counts; what to know about its base; the token.
fn text(seen: &Seen, file: Option<&str>) -> String {
    let keep = |it: &&Found| said(it) && file.is_none_or(|f| of_file(it, f));
    let mut out = String::new(); // alloc: CLI edge, the answer's text, once
    if let Some(failed) = &seen.failed {
        let _ = writeln!(out, "blaze itself failed in this check, not the code: {}", failed.what);
    }
    for found in seen.new.iter().filter(keep) {
        draw(found, &mut out);
        out.push('\n');
    }
    let new = seen.new.iter().filter(keep).count();
    let fixed = seen.fixed.iter().filter(keep).count();
    let unchanged = seen.unchanged.iter().filter(keep).count();
    let _ = writeln!(out, "{new} new, {fixed} fixed, {unchanged} unchanged");
    match seen.base {
        Base::First => {}
        Base::Held => {}
        Base::Unknown => {
            out.push_str(
                "the token is unknown to this daemon (it restarted, or the base was evicted): everything is said as new\n",
            );
        }
    }
    if !seen.ended {
        out.push_str("the check is still running in the daemon: ask again with the same --since\n");
    }
    match seen.token {
        Some(token) => {
            let _ = writeln!(out, "token {token:x}");
        }
        None => out.push_str("token none: pass the same --since again\n"),
    }
    out
}

/// `found` as JSON: its id in hex, where it is, what it says.
fn found_json(found: &Found) -> serde_json::Value {
    let span = found.span.as_ref();
    serde_json::json!({
        "id": format!("{:x}", found.id),
        "level": found.level.name(),
        "code": found.code,
        "message": found.message,
        "file": span.map(|it| it.file.as_str()),
        "line": span.map(|it| it.line),
        "column": span.map(|it| it.column),
        "help": found.help,
        "package": found.package,
        "primary": found.primary,
        "rendered": found.rendered,
    })
}

/// `seen` as one JSON object (docs/agents.md says its fields).
fn json(seen: &Seen, file: Option<&str>) -> serde_json::Value {
    let keep = |it: &&Found| said(it) && file.is_none_or(|f| of_file(it, f));
    let list = |found: &[Found]| -> Vec<serde_json::Value> {
        found.iter().filter(keep).map(found_json).collect() // alloc: CLI edge, once
    };
    serde_json::json!({
        "token": seen.token.map(|it| format!("{it:x}")),
        "base": base_name(seen.base),
        "root": format!("{:x}", seen.root),
        "build": seen.build,
        "ended": seen.ended,
        "failed": seen.failed.as_ref().map(|it| serde_json::json!({ "what": it.what, "at": it.at })),
        "new": list(&seen.new),
        "fixed": list(&seen.fixed),
        "unchanged": list(&seen.unchanged),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn its_flags_are_read_and_the_check_takes_cargos() {
        let token = "ab".repeat(32);
        let words = ["--since", &token, "-p", "demo", "--file", "src/lib.rs", "--wait", "3", "--json"];
        let line = read(words.iter().map(OsString::from).collect()).unwrap();
        assert_eq!(line.since, Some(ContentId([0xab; 32])));
        assert_eq!(line.check, ["check", "-p", "demo"].map(OsString::from));
        assert_eq!(line.file.as_deref(), Some("src/lib.rs"));
        assert_eq!(line.wait_ms, Some(3000));
        assert!(line.json);
        assert!(read(vec!["--since".into(), "zz".into()]).unwrap_err().contains("no token"));
        assert!(read(vec!["--frobnicate".into()]).unwrap_err().contains("--frobnicate"));
    }

    #[test]
    fn a_file_is_named_relative_to_the_checkout() {
        let head = Path::new("/w");
        assert_eq!(in_checkout(Path::new("/w/a"), Path::new("src/lib.rs"), head).as_deref(), Some("a/src/lib.rs"));
        assert_eq!(in_checkout(Path::new("/w/a"), Path::new("../b/x.rs"), head).as_deref(), Some("b/x.rs"));
        assert_eq!(in_checkout(Path::new("/x"), Path::new("/w/c.rs"), head).as_deref(), Some("c.rs"));
        assert_eq!(in_checkout(Path::new("/w"), Path::new("../v/c.rs"), head), None);
    }
}
