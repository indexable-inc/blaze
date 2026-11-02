//! The agent API's responses ([`blaze_api::Response`]) as text for people: what a client of
//! the API (the CLI, a CI log) prints. Diagnostics as rustc writes them, with the facts they
//! read; tests as libtest writes them. JSON for programs is [`blaze_api::json`]'s, but for the
//! forms lane proof's harness reads: [`work_json`], [`served_json`], [`codegen_inputs_json`] and
//! [`trace_cbor`].
//!
//! Every line is written into the one `String` it ends in, with no `String` of its own.

use std::fmt;

use blaze_api::{
    Diagnostic, FetchVerb, Hot, HotFunction, HotSince, HotWaitKind, How, Level, LinkHow, LinkKind, Linked, NodeKind,
    Ranked, Response, ServedCrate, TestOutcome, TestResult, Traced, Verb, Worked,
};
use blaze_ids::ContentId;
use blaze_query::QueryKey;
use blaze_v2_proof::TraceBuilder;
use blaze_vfs::ChangeKind;

use crate::{CYAN, DIM, GREEN, Painted, RED, YELLOW};

/// The most bytes of a test's or a run's output shown.
const OUTPUT: usize = 64 << 10;

/// The text of one response, each line ending in a newline; empty for one that says nothing
/// to a person (a request's successful end).
pub fn lines(response: &Response, color: bool) -> String {
    // alloc: the one-call form; a printer of many writes `lines_into` one buffer
    let mut out = String::new();
    lines_into(&mut out, response, color);
    out
}

/// [`lines`], written at the end of `out`.
pub fn lines_into(out: &mut String, response: &Response, color: bool) {
    let paint = |code, text| Painted(code, text, color);
    // A status word as cargo writes it: right-aligned in 12 columns.
    let status = |word: &'static str| Painted(GREEN, Status(word), color);
    match response {
        Response::Committed { root, rebased } => {
            let rebased = if *rebased { " (rebased onto the head)" } else { "" };
            put!(out, "{} {root}{rebased}\n", paint(GREEN, "committed"));
        }
        Response::Diagnostic(diagnostic) => diagnostic_lines(out, diagnostic, 0, color),
        Response::Built { artifacts, profile, .. } => {
            for it in artifacts {
                let (package, kind, path, content) = (&it.package, it.kind.name(), &it.path, &it.content);
                put!(out, "{} {package} {kind} at {path} ({content})\n", status("Built"));
            }
            put!(out, "{} {profile}\n", status("Finished"));
        }
        Response::Unit(unit) => {
            let verb = match unit.verb {
                Verb::Compiling => "Compiling",
                Verb::Checking => "Checking",
                Verb::Fresh => "Fresh",
            };
            put!(out, "{} {} v{}", status(verb), unit.name, unit.version);
            if let Some(source) = &unit.source {
                put!(out, " {source}");
            }
            out.push('\n');
        }
        Response::Fetched { verb, what } => {
            let verb = match verb {
                FetchVerb::Updating => "Updating",
                FetchVerb::Downloaded => "Downloaded",
            };
            put!(out, "{} {what}\n", status(verb));
        }
        Response::BuildScript(_) | Response::UnitEnded(_) => {}
        Response::Work(worked) => {
            let link = match worked.link {
                LinkKind::Patched => "patched in place",
                LinkKind::Placed => "placed whole",
            };
            put!(
                out,
                "{} {} queries ran, {} green; {} functions compiled, {} found; {link}; \
                 critical path {} us\n",
                paint(DIM, "work:"),
                worked.reran,
                worked.green,
                worked.compiled,
                worked.found,
                worked.critical_path_us
            );
            link_lines(out, &worked.links);
        }
        Response::CodegenInputs(inputs) => {
            for (instance, code) in inputs {
                put!(out, "{instance:x} {code:x}\n");
            }
        }
        Response::Trace(traced) => {
            for result in traced {
                let ran = if result.ran { "ran" } else { "kept" };
                let (edges, fingerprint) = (result.edges.len(), result.fingerprint);
                put!(out, "{} {ran}, {edges} edges, {fingerprint:?}\n", Key(&result.key));
            }
        }
        Response::Tables(tables) => {
            for table in tables {
                let (name, elements, bytes) = (&table.name, table.elements, table.bytes);
                put!(out, "{name}: {elements} elements, {bytes} bytes\n");
            }
        }
        Response::Memory(memory) => {
            for it in &memory.tags {
                put!(
                    out,
                    "{}: {} bytes live, {} at most, {} bytes/s, task peak {}, budget ",
                    it.tag,
                    it.live,
                    it.high_water,
                    it.rate,
                    it.task_peak
                );
                match it.budget {
                    u64::MAX => put!(out, "none\n"),
                    bytes => put!(out, "{bytes}\n"),
                }
            }
            put!(
                out,
                "committed: {} bytes, {} at most\n",
                memory.committed, memory.committed_high_water
            );
        }
        Response::Edited(edited) => {
            let (items, ops) = (edited.items.len(), edited.ops.len());
            put!(out, "{} {items} items, {ops} ops\n", status("Edited"));
            for item in &edited.items {
                put!(out, "  {} {}: {}\n", how_word(item.how), item.file, item.path);
            }
        }
        Response::Announced { root } => put!(out, "{} {root}\n", status("Announced")),
        Response::Packed { pack } => put!(out, "{} {} bytes\n", status("Packed"), pack.len()),
        Response::DocPage { path, page } => put!(out, "{} {path}, {} bytes\n", status("Page"), page.len()),
        Response::Documented(documented) => {
            let (dir, crates) = (&documented.dir, documented.crates.len());
            put!(out, "{} {crates} crates, {} pages in {dir}\n", status("Documented"), documented.pages);
        }
        Response::TestResult(result) => test_lines(out, result, color),
        Response::Ran { code, stdout, stderr } => {
            bytes(out, stdout);
            bytes(out, stderr);
            match code {
                Some(0) => {}
                Some(code) => put!(out, "{} exited with {code}\n", paint(RED, "error")),
                None => put!(out, "{} ended by a signal\n", paint(RED, "error")),
            }
        }
        Response::Diff { changes } => {
            for change in changes {
                let mark = match change.kind {
                    ChangeKind::Added => paint(GREEN, "A"),
                    ChangeKind::Removed => paint(RED, "D"),
                    ChangeKind::Modified => paint(YELLOW, "M"),
                };
                put!(out, "{mark} {}\n", change.path);
            }
        }
        Response::Why(why) => {
            put!(out, "{} reran because:\n", Key(&why.query));
            for changed in &why.chain {
                let (was, now) = (&changed.was, &changed.now);
                put!(out, "  {} changed ({was:?} -> {now:?})\n", Key(&changed.key));
            }
            if let Some(input) = &why.input {
                put!(out, "  from the input {input:?}\n");
            }
        }
        Response::Status(status) => {
            let requests = status.requests.len();
            put!(
                out,
                "{requests} {} under way, {} {} open, {} of {} bytes of memory\n",
                if requests == 1 { "request" } else { "requests" },
                status.views,
                if status.views == 1 { "view" } else { "views" },
                status.memory_bytes,
                status.budget_bytes
            );
            if let Some(why) = &status.history {
                put!(out, "{}: the profile history: {why}\n", paint(RED, "error"));
            }
        }
        Response::Done { ok: true } => {}
        Response::Done { ok: false } => put!(out, "{}\n", paint(RED, "failed")),
        Response::Error(error) => {
            put!(out, "{}: {}", paint(RED, "error"), error.what);
            if let Some(at) = &error.at {
                put!(out, " (at {at})");
            }
            out.push('\n');
        }
        Response::Opened { root } => put!(out, "{} {root}\n", paint(GREEN, "opened")),
        Response::Reached(reach) => {
            put!(
                out,
                "{} files changed; {} items, {} functions to compile, {} tests reached\n",
                reach.changes.len(),
                reach.items.len(),
                reach.compiled.len(),
                reach.tests.len()
            );
            for item in &reach.items {
                put!(out, "  {}\n", item.path);
                for fact in &item.facts {
                    put!(out, "    {} {}\n", paint(DIM, "changed:"), fact.what);
                }
            }
            for function in &reach.compiled {
                put!(out, "  {} {function}\n", paint(CYAN, "compile"));
            }
            for test in &reach.tests {
                put!(out, "  {} {test}\n", paint(CYAN, "test"));
            }
        }
        Response::Of { candidate, response } => {
            let from = out.len();
            lines_into(out, response, color);
            let inner = out.split_off(from);
            for said in inner.lines() {
                put!(out, "[{candidate}] {said}\n");
            }
        }
        Response::Ranked(ranked) => {
            for (place, it) in ranked.iter().enumerate() {
                ranked_line(out, place, it, color);
            }
        }
        Response::Verified { mismatches } if mismatches.is_empty() => {
            put!(out, "{} every result matched a build from nothing\n", paint(GREEN, "verified"));
        }
        Response::Verified { mismatches } => {
            let (error, count) = (paint(RED, "error"), mismatches.len());
            put!(out, "{error}: {count} results differ from a build from nothing\n");
            for mismatch in mismatches {
                let (kept, scratch) = (&mismatch.incremental, &mismatch.scratch);
                put!(out, "  {}: {kept:?} kept, {scratch:?} from nothing", Key(&mismatch.query));
                if let Some(fact) = &mismatch.fact {
                    put!(out, ", the fact that missed: {}", fact.what);
                }
                out.push('\n');
            }
        }
        Response::Snapshotted { heads } => {
            put!(out, "{} {} heads durable\n", paint(GREEN, "snapshotted"), heads.len());
            for (head, root) in heads {
                put!(out, "  {head} {root}\n");
            }
        }
        Response::Explored { link } => put!(out, "{link}\n"),
        Response::Profiled { trace } => put!(out, "{} {} bytes of Chrome trace\n", status("Profiled"), trace.len()),
        Response::Live(frame) => {
            let tables = frame.spans.len() + frame.counters.len() + frame.open.len();
            put!(out, "{} {tables} bytes of Arrow IPC", status("Live"));
            if frame.skipped > 0 {
                put!(out, ", {} rows skipped", frame.skipped);
            }
            out.push('\n');
        }
        Response::Queried { rows, ipc } => put!(out, "{} {rows} rows, {} bytes of Arrow IPC\n", status("Queried"), ipc.len()),
        Response::Hot(hot) => hot_lines(out, hot),
    }
}

/// `blaze debug hot`'s table: the keys that took most, each with the bracket the sketches
/// put its time in, then each kind of wait's quantiles.
pub fn hot_lines(out: &mut String, hot: &Hot) {
    use std::fmt::Write as _;
    let ms = |ns: u64| ns as f64 / 1e6;
    let since = match hot.since {
        HotSince::Start => "since the daemon began",
        HotSince::LastBuild => "over the last build",
    };
    _ = writeln!(
        out,
        "blaze hot, {since}: {} spans, {:.1} ms in spans (nested ones again); a key's time is at most \
         its first column, over the truth by at most {:.1} ms but for a 1.8% chance, and at least its second",
        hot.spans,
        ms(hot.total_ns),
        ms(hot.error_ns),
    );
    _ = writeln!(out, "{:>12} {:>12}  span  of", "at most ms", "at least ms");
    for key in &hot.keys {
        _ = write!(out, "{:>12.1} {:>12.1}  {}", ms(key.upper_ns), ms(key.lower_ns), key.span);
        _ = match &key.function {
            HotFunction::None => writeln!(out),
            HotFunction::Named(name) => writeln!(out, "  {name}"),
            HotFunction::Forgotten(sym) => writeln!(out, "  (a name the daemon let go, #{sym})"),
        };
    }
    if hot.torn > 0 {
        _ = writeln!(out, "{} entries were read as a thread replaced them: left out of `at least`", hot.torn);
    }
    _ = writeln!(out, "{:<8} {:>9} {:>12} {:>12} {:>12} {:>12}", "wait", "count", "p50 ms", "p90 ms", "p99 ms", "max ms");
    for wait in &hot.waits {
        let kind = match wait.kind {
            HotWaitKind::Key => "key",
            HotWaitKind::Lock => "lock",
            HotWaitKind::Channel => "channel",
            HotWaitKind::Io => "io",
            HotWaitKind::Child => "child",
            HotWaitKind::Scope => "scope",
        };
        _ = writeln!(
            out,
            "{kind:<8} {:>9} {:>12.3} {:>12.3} {:>12.3} {:>12.3}",
            wait.count,
            ms(wait.p50_ns),
            ms(wait.p90_ns),
            ms(wait.p99_ns),
            ms(wait.max_ns),
        );
    }
}

fn how_word(how: How) -> &'static str {
    match how {
        How::Body => "body",
        How::Signature => "signature",
        How::Fields => "fields",
        How::Whole => "whole",
        How::Added => "added",
        How::Removed => "removed",
        How::Renamed => "renamed",
        How::Scope => "scope",
    }
}

/// A word right-aligned in 12 columns, as cargo's status lines start.
struct Status(&'static str);

impl fmt::Display for Status {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:>12}", self.0)
    }
}

/// rustc's own text when the diagnostic carries it, else made from its parts; then the facts.
fn diagnostic_lines(out: &mut String, diagnostic: &Diagnostic, depth: usize, color: bool) {
    if depth == 0 && !diagnostic.rendered.is_empty() {
        out.push_str(&diagnostic.rendered);
        if !diagnostic.rendered.ends_with('\n') {
            out.push('\n');
        }
        facts_lines(out, diagnostic, color);
        return;
    }
    let (word, color_of) = match diagnostic.level {
        Level::Error => ("error", RED),
        Level::Warning => ("warning", YELLOW),
        Level::Note => ("note", GREEN),
        Level::Help => ("help", CYAN),
        Level::FailureNote => {
            out.push_str(&diagnostic.message);
            out.push('\n');
            return;
        }
    };
    let message = &diagnostic.message;
    match (depth, &diagnostic.code) {
        (0, Some(code)) => {
            let word = Painted(color_of, format_args!("{word}[{code}]"), color);
            put!(out, "{word}: {message}\n");
        }
        (0, None) => put!(out, "{}: {message}\n", Painted(color_of, word, color)),
        _ => put!(out, "   = {}: {message}\n", Painted(color_of, word, color)),
    }
    for span in &diagnostic.spans {
        let arrow = Painted(CYAN, "-->", color);
        put!(out, "  {arrow} {}:{}:{}", span.file, span.line, span.column);
        if let Some(label) = &span.label {
            put!(out, ": {label}");
        }
        out.push('\n');
    }
    for child in &diagnostic.children {
        diagnostic_lines(out, child, depth + 1, color);
    }
    facts_lines(out, diagnostic, color);
}

fn facts_lines(out: &mut String, diagnostic: &Diagnostic, color: bool) {
    for fact in &diagnostic.facts {
        put!(out, "   = {} {}\n", Painted(DIM, "read:", color), fact.what);
    }
}

fn test_lines(out: &mut String, result: &TestResult, color: bool) {
    let outcome = result.cached.as_ref().map_or(result.outcome, |cached| cached.outcome);
    let (code, verdict) = match outcome {
        TestOutcome::Passed => (GREEN, "ok"),
        TestOutcome::Failed => (RED, "FAILED"),
        TestOutcome::Ignored => (YELLOW, "ignored"),
        TestOutcome::Unreached => (DIM, "unreached"),
    };
    let verdict = Painted(code, verdict, color);
    match &result.cached {
        None => {
            put!(out, "test {} ... {verdict} ({} ms)\n", result.name, result.millis);
        }
        Some(cached) => {
            put!(out, "test {} ... {verdict} (cached, {} ms)\n", result.name, cached.millis);
        }
    }
    if result.outcome == TestOutcome::Failed {
        bytes(out, &result.output);
    }
}

fn ranked_line(out: &mut String, place: usize, ranked: &Ranked, color: bool) {
    let verdict = match ranked.ok {
        true => Painted(GREEN, "ok", color),
        false => Painted(RED, "failed", color),
    };
    put!(
        out,
        "{}. candidate {} ({}): {verdict}, {} errors, {} warnings, {} passed, {} failed\n",
        place + 1,
        ranked.candidate,
        ranked.root,
        ranked.errors,
        ranked.warnings,
        ranked.passed,
        ranked.failed
    );
}

/// A query as the lines name it: `query 0x1000:<input>`.
struct Key<'a>(&'a QueryKey);

impl fmt::Display for Key<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "query {:#06x}:{}", self.0.kind.0, self.0.input)
    }
}

/// What a request did, as the CLI prints it under `--message-format=json`: one line, newline
/// ended, in cargo's `reason` style.
pub fn work_json(worked: &Worked) -> String {
    let link = match worked.link {
        LinkKind::Patched => "patched",
        LinkKind::Placed => "placed",
    };
    // alloc: the CLI's, once per request
    let mut out = format!(
        "{{\"reason\": \"blaze-work\", \"reran\": {}, \"green\": {}, \"compiled\": {}, \
         \"found\": {}, \"link\": \"{link}\", \"critical_path_us\": {}, \"links\": [",
        worked.reran, worked.green, worked.compiled, worked.found, worked.critical_path_us
    );
    for (at, it) in worked.links.iter().enumerate() {
        out.push_str(if at == 0 { "{\"path\": " } else { ", {\"path\": " });
        quoted(&mut out, &it.path);
        let (how, why) = match &it.how {
            LinkHow::First => ("first", &[][..]),
            LinkHow::Spliced => ("spliced", &[][..]),
            LinkHow::Full { why } => ("full", &why[..]),
        };
        put!(&mut out, ", \"how\": \"{how}\", \"why\": [");
        for (at, it) in why.iter().enumerate() {
            if at > 0 {
                out.push_str(", ");
            }
            quoted(&mut out, it);
        }
        out.push_str("]}");
    }
    out.push_str("], \"steps_us\": {");
    for (at, (kind, us)) in worked.steps_us.iter().enumerate() {
        put!(&mut out, "{}\"{}\": {us}", if at == 0 { "" } else { ", " }, kind.name());
    }
    out.push_str("}}\n");
    out
}

/// One `link: <path> (<how>)` line an image the request linked, in its order: what
/// `harness/bench.py`'s `link_kind` reads, so it is written in every format.
pub fn link_lines(out: &mut String, links: &[Linked]) {
    for it in links {
        put!(out, "link: {} ({})\n", it.path, it.how);
    }
}

/// `timeline-us: <kind>=<micros> ...`, the request's run time by kind ([`Worked::steps_us`]):
/// what `harness/bench.py`'s `daemon_steps` reads. Nothing for a request that ran nothing.
pub fn timeline_line(out: &mut String, steps: &[(NodeKind, u64)]) {
    if steps.is_empty() {
        return;
    }
    out.push_str("timeline-us:");
    for (kind, us) in steps {
        put!(out, " {}={us}", kind.name());
    }
    out.push('\n');
}

/// What became of each dependency crate's check ([`Worked::served`]), as the CLI prints it under
/// `--message-format=json` after [`work_json`]'s line: one `blaze-served` line a crate, in its
/// order, each newline ended; empty for none.
pub fn served_json(served: &[ServedCrate]) -> String {
    // alloc: the CLI's, once per request
    let mut out = String::with_capacity(served.len() * 96);
    for it in served {
        out.push_str("{\"reason\": \"blaze-served\", \"package\": ");
        quoted(&mut out, &it.package);
        out.push_str(", \"outcome\": \"");
        out.push_str(it.outcome.name());
        out.push_str("\", \"bodies\": \"");
        out.push_str(it.bodies.name());
        out.push_str("\", \"why\": ");
        match &it.why {
            Some(why) => quoted(&mut out, why),
            None => out.push_str("null"),
        }
        out.push_str("}\n");
    }
    out
}

/// `text` as a JSON string (RFC 8259): `"` and `\` escaped, and every control character.
fn quoted(out: &mut String, text: &str) {
    out.push('"');
    for it in text.chars() {
        match it {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            it if it < ' ' => {
                const HEX: &[u8; 16] = b"0123456789abcdef";
                out.push_str("\\u00");
                out.push(char::from(HEX[usize::from(it as u8 >> 4)]));
                out.push(char::from(HEX[usize::from(it as u8 & 15)]));
            }
            it => out.push(it),
        }
    }
    out.push('"');
}

/// `blaze codegen-inputs --format=json`: one object from each instance key's hex to its code
/// hash's hex, in the response's order (sorted by key), newline ended.
pub fn codegen_inputs_json(inputs: &[(ContentId, ContentId)]) -> String {
    // `"<64 hex>": "<64 hex>", ` is 136 bytes a pair.
    let mut out = String::with_capacity(3 + inputs.len() * 136);
    out.push('{');
    for (at, (instance, code)) in inputs.iter().enumerate() {
        let comma = if at > 0 { ", " } else { "" };
        put!(&mut out, "{comma}\"{instance:x}\": \"{code:x}\"");
    }
    out.push_str("}\n");
    out
}

/// `blaze trace --out PATH`: the deterministic CBOR of the [`blaze_v2_proof::Trace`] the
/// results make, what the `blaze-v2-proof` binary compares.
pub fn trace_cbor(traced: &[Traced]) -> Result<Vec<u8>, blaze_protocol::cbor::Error> {
    let mut trace = TraceBuilder::default();
    for result in traced {
        trace.insert(result.key, result.fingerprint, &result.edges, result.ran);
    }
    blaze_protocol::cbor::to_vec(&trace.finish())
}

/// Output of user code: lossily as text, its last [`OUTPUT`] bytes, each line ended.
fn bytes(out: &mut String, bytes: &[u8]) {
    let start = bytes.len().saturating_sub(OUTPUT);
    // alloc: a `Cow`, owned only for output that is not UTF-8
    let text = String::from_utf8_lossy(&bytes[start..]);
    if start > 0 {
        put!(out, "({start} bytes cut)\n");
    }
    for said in text.lines() {
        out.push_str(said);
        out.push('\n');
    }
}

#[cfg(test)]
mod tests {
    use blaze_api::{ApiError, Basis, Cached, FactRead, Span};
    use blaze_ids::ContentId;
    use blaze_query::QueryKind;

    use super::*;

    fn fact(what: &str) -> FactRead {
        let key = QueryKey { kind: QueryKind(0x1000), input: ContentId([1; 32]) };
        FactRead { key, what: what.to_owned() }
    }

    fn diagnostic() -> Diagnostic {
        Diagnostic {
            level: Level::Error,
            code: Some("E0308".to_owned()),
            message: "mismatched types".to_owned(),
            spans: vec![Span {
                file: "src/main.rs".to_owned(),
                line: 3,
                column: 17,
                end_line: 3,
                end_column: 19,
                label: Some("expected `u32`, found `&str`".to_owned()),
                byte_start: 40,
                byte_end: 42,
                primary: true,
                replacement: None,
                applicability: None,
            }],
            children: vec![Diagnostic {
                level: Level::Note,
                code: None,
                message: "expected due to this".to_owned(),
                spans: Vec::new(),
                children: Vec::new(),
                facts: Vec::new(),
                rendered: String::new(),
                unit: None,
                explanation: None,
            }],
            facts: vec![fact("the signature of `main::parse`")],
            rendered: String::new(),
            unit: None,
            explanation: None,
        }
    }

    #[test]
    fn a_diagnostic_reads_as_rustc_writes_it_with_its_facts() {
        assert_eq!(
            lines(&Response::Diagnostic(diagnostic()), false),
            "error[E0308]: mismatched types\n\
             \x20 --> src/main.rs:3:17: expected `u32`, found `&str`\n\
             \x20  = note: expected due to this\n\
             \x20  = read: the signature of `main::parse`\n"
        );
    }

    /// In color, each word is painted as `paint` painted it: the code, the word, the reset.
    #[test]
    fn a_diagnostic_in_color_paints_its_words() {
        let (red, green, cyan, dim) = (RED, GREEN, CYAN, DIM);
        let reset = crate::RESET;
        assert_eq!(
            lines(&Response::Diagnostic(diagnostic()), true),
            format!(
                "{red}error[E0308]{reset}: mismatched types\n\
                 \x20 {cyan}-->{reset} src/main.rs:3:17: expected `u32`, found `&str`\n\
                 \x20  = {green}note{reset}: expected due to this\n\
                 \x20  = {dim}read:{reset} the signature of `main::parse`\n"
            )
        );
        let failed = Response::Done { ok: false };
        assert_eq!(lines(&failed, true), format!("{red}failed{reset}\n"));
    }

    #[test]
    fn work_is_one_json_line_in_the_form_proof_reads() {
        let worked = Worked {
            reran: 12,
            green: 300,
            served: Vec::new(),
            compiled: 2,
            found: 40,
            link: LinkKind::Patched,
            links: Vec::new(),
            critical_path_us: 1500,
            steps_us: vec![(NodeKind::Bodies, 900), (NodeKind::Codegen, 600)],
        };
        let line = work_json(&worked);
        assert_eq!(
            line,
            "{\"reason\": \"blaze-work\", \"reran\": 12, \"green\": 300, \"compiled\": 2, \
             \"found\": 40, \"link\": \"patched\", \"critical_path_us\": 1500, \"links\": [], \
             \"steps_us\": {\"bodies\": 900, \"codegen\": 600}}\n"
        );
        let value: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(value["link"], "patched");
        let mut timeline = String::new();
        timeline_line(&mut timeline, &worked.steps_us);
        assert_eq!(timeline, "timeline-us: bodies=900 codegen=600\n");
        timeline.clear();
        timeline_line(&mut timeline, &[]);
        assert_eq!(timeline, "");
    }

    /// Each link is a `link:` line in the form `harness/bench.py`'s `link_kind` reads, and an
    /// entry of the `blaze-work` line's `links`.
    #[test]
    fn each_link_says_how_it_went() {
        let links = vec![
            Linked { how: LinkHow::Spliced, path: "target/debug/app".to_owned() },
            Linked { how: LinkHow::First, path: "target/debug/deps/m.so".to_owned() },
            Linked {
                how: LinkHow::Full { why: vec!["exports".to_owned(), "debug".to_owned()] },
                path: "target/debug/deps/p.so".to_owned(),
            },
        ];
        let mut text = String::new();
        link_lines(&mut text, &links);
        assert_eq!(
            text,
            "link: target/debug/app (spliced)\n\
             link: target/debug/deps/m.so (first link)\n\
             link: target/debug/deps/p.so (not spliced: exports, debug)\n"
        );
        let worked = Worked {
            reran: 0,
            green: 0,
            served: Vec::new(),
            compiled: 0,
            found: 0,
            link: LinkKind::Placed,
            links,
            critical_path_us: 0,
            steps_us: Vec::new(),
        };
        assert!(lines(&Response::Work(worked.clone()), false).ends_with(&text));
        let value: serde_json::Value = serde_json::from_str(&work_json(&worked)).unwrap();
        let spliced = serde_json::json!({"path": "target/debug/app", "how": "spliced", "why": []});
        assert_eq!(value["links"][0], spliced);
        assert_eq!(value["links"][2]["how"], "full");
        assert_eq!(value["links"][2]["why"], serde_json::json!(["exports", "debug"]));
    }

    /// One line a crate in the shape `harness/v2/frontend/cases.py` reads, its strings as
    /// serde_json writes them.
    #[test]
    fn each_dependency_is_one_served_line() {
        use blaze_api::ServedPart;
        let served = [
            ServedCrate {
                why: None,
                bodies: ServedPart::Served,
                outcome: ServedPart::Served,
                package: "u0_1_arith".to_owned(),
            },
            ServedCrate {
                why: Some("a note in \"src/lib.rs\" of another package\\\n\u{1}\u{1f}é".to_owned()),
                bodies: ServedPart::Checked,
                outcome: ServedPart::Refused,
                package: "bad_cross".to_owned(),
            },
        ];
        let text = served_json(&served);
        assert_eq!(
            text.lines().next(),
            Some(
                "{\"reason\": \"blaze-served\", \"package\": \"u0_1_arith\", \"outcome\": \"served\", \
                 \"bodies\": \"served\", \"why\": null}"
            )
        );
        assert!(text.ends_with('\n'));
        let lines: Vec<serde_json::Value> = text.lines().map(|it| serde_json::from_str(it).unwrap()).collect();
        assert_eq!(lines.len(), 2);
        for (line, it) in lines.iter().zip(&served) {
            assert_eq!(line["reason"], "blaze-served");
            assert_eq!(line["package"], it.package.as_str());
            assert_eq!(line["outcome"], it.outcome.name());
            assert_eq!(line["bodies"], it.bodies.name());
            assert_eq!(line["why"].as_str(), it.why.as_deref());
        }
        let why = served[1].why.as_deref().unwrap();
        assert!(text.contains(&serde_json::to_string(why).unwrap()), "{text}");
        assert_eq!(served_json(&[]), "");
    }

    #[test]
    fn codegen_inputs_are_one_object_of_hex() {
        let inputs =
            [(ContentId([1; 32]), ContentId([2; 32])), (ContentId([3; 32]), ContentId([4; 32]))];
        let text = codegen_inputs_json(&inputs);
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(value[format!("{:x}", ContentId([3; 32]))], format!("{:x}", ContentId([4; 32])));
        assert_eq!(value.as_object().unwrap().len(), 2);
        assert_eq!(codegen_inputs_json(&[]), "{}\n");
        let pairs: Vec<String> = inputs
            .iter()
            .map(|(instance, code)| format!("\"{instance:x}\": \"{code:x}\""))
            .collect();
        assert_eq!(text, format!("{{{}}}\n", pairs.join(", ")));
    }

    #[test]
    fn a_trace_is_written_as_proofs_trace() {
        let key = |n: u8| QueryKey { kind: QueryKind(0x2000), input: ContentId([n; 32]) };
        let traced = [
            Traced {
                key: key(2),
                fingerprint: blaze_ids::Fingerprint(7),
                edges: vec![blaze_query::Edge { read: key(1), saw: blaze_ids::Fingerprint(5) }],
                ran: true,
            },
            Traced {
                key: key(1),
                fingerprint: blaze_ids::Fingerprint(5),
                edges: Vec::new(),
                ran: false,
            },
        ];
        let bytes = trace_cbor(&traced).unwrap();
        blaze_protocol::cbor::check(&bytes).unwrap();
        let trace: blaze_v2_proof::Trace = blaze_protocol::cbor::from_slice(&bytes).unwrap();
        assert_eq!(trace.keys, [key(1), key(2)]);
        assert!(trace.ran.contains(1) && !trace.ran.contains(0));
        assert_eq!(trace.reads, [0]);
        assert_eq!(trace.edges_from, [0, 0, 1]);
    }

    #[test]
    fn rustcs_own_text_is_printed_as_it_is_then_the_facts() {
        let diagnostic = Diagnostic {
            level: Level::Warning,
            code: None,
            message: "unused variable: `x`".to_owned(),
            spans: Vec::new(),
            children: Vec::new(),
            facts: vec![fact("what `x` in `main` resolves to")],
            rendered: "warning: unused variable: `x`\n --> src/main.rs:2:9\n".to_owned(),
            unit: None,
            explanation: None,
        };
        assert_eq!(
            lines(&Response::Diagnostic(diagnostic), false),
            "warning: unused variable: `x`\n --> src/main.rs:2:9\n\
             \x20  = read: what `x` in `main` resolves to\n"
        );
    }

    #[test]
    fn a_failed_test_shows_its_output_and_a_passed_one_does_not() {
        let mut result = TestResult {
            binary: "t".to_owned(),
            name: "parses".to_owned(),
            cached: None,
            outcome: TestOutcome::Passed,
            output: b"noise\n".to_vec(),
            millis: 4,
            outside: false,
            executable: 0,
        };
        let passed = lines(&Response::TestResult(result.clone()), false);
        assert_eq!(passed, "test parses ... ok (4 ms)\n");
        result.outcome = TestOutcome::Failed;
        assert_eq!(
            lines(&Response::TestResult(result.clone()), false),
            "test parses ... FAILED (4 ms)\nnoise\n"
        );
        let cached = Cached { basis: Basis::Executed, millis: 9, outcome: TestOutcome::Passed };
        let kept = TestResult {
            cached: Some(cached),
            millis: 0,
            output: Vec::new(),
            outcome: TestOutcome::Unreached,
            ..result
        };
        assert_eq!(lines(&Response::TestResult(kept), false), "test parses ... ok (cached, 9 ms)\n");
    }

    #[test]
    fn a_candidates_lines_are_marked_with_it() {
        let inner = Response::Error(ApiError::new("no such file", Some("src/a.rs".to_owned())));
        let of = Response::Of { candidate: 2, response: Box::new(inner) };
        assert_eq!(lines(&of, false), "[2] error: no such file (at src/a.rs)\n");
        // After what `out` had, which stays as it was.
        let mut out = "before\n".to_owned();
        lines_into(&mut out, &of, false);
        assert_eq!(out, "before\n[2] error: no such file (at src/a.rs)\n");
    }

    #[test]
    fn a_successful_end_says_nothing() {
        assert_eq!(lines(&Response::Done { ok: true }, false), "");
        assert_eq!(lines(&Response::Done { ok: false }, false), "failed\n");
    }

    #[test]
    fn an_edit_lists_what_it_changed_and_an_intent_its_root() {
        let item = |how, path: &str| blaze_api::EditedItem {
            how,
            key: None,
            file: "src/lib.rs".to_owned(),
            path: path.to_owned(),
        };
        let edited = blaze_api::Edited {
            ops: Vec::new(),
            items: vec![item(How::Body, "fn parse"), item(How::Added, "struct Token")],
        };
        assert_eq!(
            lines(&Response::Edited(edited), false),
            "      Edited 2 items, 0 ops\n  body src/lib.rs: fn parse\n  added src/lib.rs: struct Token\n"
        );
        let root = ContentId([2; 32]);
        assert_eq!(lines(&Response::Announced { root }, false), format!("   Announced {root}\n"));
    }

    #[test]
    fn long_output_keeps_its_tail() {
        let output = vec![b'x'; OUTPUT + 10];
        let ran = Response::Ran { code: Some(0), stdout: output, stderr: Vec::new() };
        assert!(lines(&ran, false).starts_with("(10 bytes cut)\n"));
    }
}
