//! docs: every document under docs/ says what it is, and the current ones are held to the tree.
//! The port of `tools/docs.py`, deleted once both were run on the same tree and agreed byte for
//! byte (docs/v2/plan/design/scripts.md, "Ports and their parity"): the same commands, text
//! printed, exit status and bytes written.
//!
//!     blaze run -q -p scripts --bin docs -- check      fail naming each problem (CI)
//!     blaze run -q -p scripts --bin docs -- write      regenerate docs/README.md, the index, from
//!                                                      the front matter, and take the files that
//!                                                      are gone out of tools/python-allowed.txt
//!     blaze run -q -p scripts --bin docs -- python '{"left": 0}'   at most `left` Python files
//!                                                      remain (ledger D43)
//!     blaze run -q -p scripts --bin docs -- selftest
//!
//! Each docs/**/*.md but docs/README.md (generated) and docs/upstream/ (others' text, as fetched)
//! opens with front matter, one `key: value` per line between `---` lines:
//!
//!     status: current | plan | record | superseded
//!     summary: one line, for the index
//!     date: YYYY-MM-DD        a plan's, a record's or a superseded doc's: when it was written
//!     superseded_by: path#anchor, relative to the doc as a link is, for a superseded doc alone
//!
//! `current` says how blaze is now and is kept true: its links and anchors resolve, the repository
//! paths it names in backticks exist, and the BLAZE_* settings it names are in the code. A `plan`
//! is a design not (fully) built, dated: its links and anchors resolve, but it may name paths
//! still to come. A `record` was true on its date and is never brought up to date: only the files
//! it links have to exist. A `superseded` doc names what replaced it and is left out of the
//! index. Outside docs/, every doc under docs/ that code or another document names exists.
//!
//! No new Python: tools/python-allowed.txt lists every `.py` file in the tree, a path a line.
//! `check` fails for a `.py` file that is not listed and for a line whose file is gone; `write`
//! takes such lines out and never adds one, so the list only shrinks.
//!
//! Where this differs from the Python, on purpose: a heading's anchor keeps a character when
//! Rust's `char::is_alphanumeric` holds or it is in the combining-mark blocks of `mark`, where
//! Python asked `str.isalnum` and the Unicode category M; no heading of docs/ tells them apart.
//! The words after "the input is not JSON:" are serde_json's, not Python's.

use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};

use blaze_script::{
    Value, fail,
    pyjson::dumps,
    pystr::is_word,
    read_text,
    regex::{Captures, Regex},
    run,
};

/// What the Python called itself in every line it printed; kept so the output stayed the same
/// bytes through the switch.
const SAYS: &str = "tools/docs.py";
const STATUSES: [&str; 4] = ["current", "plan", "record", "superseded"];
const FRONT_FIELDS: [&str; 4] = ["status", "summary", "date", "superseded_by"];
/// Top-level directories whose paths a doc names in backticks; anything else in backticks is a
/// command, a type or a relative path, not checked.
const TREE: [&str; 11] =
    ["crates", "harness", "tools", "docs", "site", "library", "vendor", ".github", "demo", "licenses", "scripts"];
/// Not checked for the docs they name: docs/ itself (its links are), the history, others' code,
/// harness (its own workspace) and the flecs skills, about other projects.
const NOT_OURS: [&str; 6] =
    ["docs/", "library/", "vendor/", "crates/harness/", ".agents/skills/flecs/", ".agents/skills/flecs-ui/"];
const ALLOWED_HEAD: &str = "# The Python files left (docs/v2/plan/design/scripts.md). `blaze run -q -p scripts --bin docs --
# check` fails for a .py file that is not here; `... --bin docs -- write` takes out the lines whose
# file is gone and never adds one. A new script is written in Rust.
";

/// The patterns of tools/docs.py, compiled once. The ones Python wrote with a lookaround or a
/// backreference (`SETTING`, `DOC_NAMED`, `SPAN`) are code here (`settings`, `spans`) or a
/// pattern plus a check in code (`docs_named`).
struct Patterns {
    path: Regex,
    doc_named: Regex,
    link: Regex,
    ref_link: Regex,
    html_id: Regex,
    fence: Regex,
    heading: Regex,
    scheme: Regex,
    date: Regex,
    line_suffix: Regex,
    inline_link: Regex,
}

impl Patterns {
    fn new() -> Patterns {
        let re = |pattern: &str| match Regex::new(pattern) {
            Ok(it) => it,
            Err(e) => fail(SAYS, &format!("a pattern of its own does not compile: {e}")),
        };
        let tree: Vec<String> = TREE.iter().map(|it| blaze_script::regex::escape(it)).collect();
        Patterns {
            path: re(&format!(r"^(?:{})/[\w./@+-]*$", tree.join("|"))),
            doc_named: re(r"docs/[\w./-]+\.md\b"),
            link: re(r#"!?\[(?:[^\[\]]|\[[^\]]*\])*\]\(\s*<?([^)\s>]+)>?(?:\s+"[^"]*")?\s*\)"#),
            ref_link: re(r"(?m)^\s{0,3}\[[^\]]+\]:\s*<?(\S+?)>?(?:\s|$)"),
            html_id: re(r#"<a\s+(?:name|id)="([^"]+)""#),
            fence: re(r"^\s{0,3}(`{3,}|~{3,})"),
            heading: re(r"(?m)^\s{0,3}#{1,6}\s+(.+?)\s*#*\s*$"),
            scheme: re(r"^[a-z][a-z0-9+.-]*:"),
            date: re(r"^\d{4}-\d{2}-\d{2}$"),
            line_suffix: re(r":\d+(-\d+)?$"),
            inline_link: re(r"\[[^\]]*\]\([^)]*\)"),
        }
    }

    /// Python's `SETTING.findall`: `\bBLAZE_[A-Z0-9_]*[A-Z0-9](?![\w*])`. Every character of a
    /// run of `[A-Z0-9_]` is a word character, so only the whole run can be followed by something
    /// that is not: the match is the whole run, when it ends in a letter or a digit and what
    /// follows is neither a word character nor `*`.
    ///
    /// Hand-written, not a regex: it runs over every tracked file, and a substring search for
    /// `BLAZE_` is what makes `check` fast (a Unicode `\b` keeps the regex crate off its DFA).
    fn settings<'t>(&self, text: &'t str) -> Vec<&'t str> {
        let mut out = Vec::new();
        let mut from = 0;
        while let Some(at) = text[from..].find("BLAZE_").map(|at| from + at) {
            // `\b` before `B`: what comes before is no word character.
            let starts = !text[..at].chars().next_back().is_some_and(is_word);
            let name = |b: &u8| b.is_ascii_uppercase() || b.is_ascii_digit() || *b == b'_';
            let end = at + text[at..].bytes().take_while(name).count();
            from = if starts { end } else { at + 1 };
            if !starts {
                continue;
            }
            let ends = text[..end].chars().next_back().is_some_and(|c| c != '_');
            let next = text[end..].chars().next();
            if ends && !next.is_some_and(|c| is_word(c) || c == '*') {
                out.push(&text[at..end]);
            }
        }
        out
    }

    /// Python's `DOC_NAMED.findall`: `(?<![\w./:-])docs/[\w./-]+\.md\b`.
    fn docs_named<'t>(&self, line: &'t str) -> Vec<&'t str> {
        self.doc_named
            .find_iter(line)
            .filter(|m| {
                let before = line[..m.start()].chars().next_back();
                !before.is_some_and(|c| is_word(c) || matches!(c, '.' | '/' | ':' | '-'))
            })
            .map(|m| m.as_str())
            .collect()
    }
}

/// Python's `SPAN.finditer`, ``(`+)(.+?)\1``: what stands between a run of backticks and the
/// next run of as many, on one line. As the backtracking does: at a backtick, the longest run
/// first, then shorter ones; the first closing run wins; a match is passed over whole.
fn spans(body: &str) -> Vec<&str> {
    let bytes = body.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    'starts: while i < bytes.len() {
        if bytes[i] != b'`' {
            i += 1;
            continue;
        }
        let run = bytes[i..].iter().take_while(|&&b| b == b'`').count();
        let line_end = bytes[i..].iter().position(|&b| b == b'\n').map_or(bytes.len(), |n| i + n);
        for ticks in (1..=run).rev() {
            // `.+?` is one character or more, none a newline; `\1` is `ticks` backticks.
            let mut j = i + ticks;
            while j < line_end {
                // One character of `.+?`, whole: a backtick is one byte, so a slice is never cut.
                j += body[j..].chars().next().map_or(1, char::len_utf8);
                if j + ticks <= line_end && bytes[j..j + ticks].iter().all(|&b| b == b'`') {
                    out.push(&body[i + ticks..j]);
                    i = j + ticks;
                    continue 'starts;
                }
            }
        }
        i += 1;
    }
    out
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Front {
    status: String,
    summary: String,
    date: Option<String>,
    superseded_by: Option<String>,
}

struct Doc {
    /// Relative to the root.
    path: String,
    front: Option<Front>,
    /// Without front matter or fenced code.
    body: String,
}

fn strip_fences(p: &Patterns, text: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    let mut fence: Option<String> = None;
    for line in text.split('\n') {
        let opens = p.fence.captures(line).and_then(|c| c.get(1)).map(|m| m.as_str().to_owned());
        match (fence.clone(), opens) {
            (None, Some(opened)) => {
                fence = Some(opened);
                out.push("");
            }
            (Some(open), _) => {
                let mark = open.chars().next().unwrap_or('`');
                let closes = line.trim().starts_with(open.as_str()) && line.trim().trim_matches(mark).is_empty();
                if closes {
                    fence = None;
                }
                out.push("");
            }
            (None, None) => out.push(line),
        }
    }
    out.join("\n")
}

/// The combining marks a heading may hold (Unicode category M), by block: what Python asked
/// `unicodedata.category(c).startswith("M")` for.
fn mark(c: char) -> bool {
    matches!(
        c as u32,
        0x0300..=0x036F | 0x1AB0..=0x1AFF | 0x1DC0..=0x1DFF | 0x20D0..=0x20FF | 0xFE00..=0xFE0F | 0xFE20..=0xFE2F
            | 0xE0100..=0xE01EF
    )
}

fn slug(p: &Patterns, heading: &str, seen: &mut BTreeMap<String, usize>) -> String {
    // A link in a heading counts as its text: `m.group(0)[1:m.group(0).index("](")]`.
    let text = p.link.replace_all(heading, |m: &Captures<'_>| {
        let all = &m[0];
        let end = all.find("](").unwrap_or(all.len());
        all.get(1..end).unwrap_or("").to_owned()
    });
    let text: String = text
        .trim()
        .to_lowercase()
        .chars()
        .filter(|&c| c.is_alphanumeric() || matches!(c, '-' | '_' | ' ') || mark(c))
        .collect();
    let text = text.replace(' ', "-");
    let n = seen.get(&text).copied().unwrap_or(0);
    seen.insert(text.clone(), n + 1);
    if n == 0 { text } else { format!("{text}-{n}") }
}

fn anchors(p: &Patterns, body: &str) -> BTreeSet<String> {
    let mut seen = BTreeMap::new();
    let mut out: BTreeSet<String> = p.heading.captures_iter(body).map(|m| slug(p, &m[1], &mut seen)).collect();
    out.extend(p.html_id.captures_iter(body).map(|m| m[1].to_owned()));
    out
}

/// Python's `repr` of a str, for the one message that quotes a line.
fn py_repr(s: &str) -> String {
    let quote = if s.contains('\'') && !s.contains('"') { '"' } else { '\'' };
    let mut out = String::from(quote);
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c == quote => {
                out.push('\\');
                out.push(c);
            }
            c if (c as u32) < 0x20 || c as u32 == 0x7f => out.push_str(&format!("\\x{:02x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push(quote);
    out
}

fn parse_front(p: &Patterns, path: &str, text: &str, errors: &mut Vec<String>) -> (Option<Front>, String) {
    if !text.starts_with("---\n") {
        errors.push(format!(
            "{path}: no front matter (`---`, then status, summary and the rest; scripts/src/bin/docs.rs)"
        ));
        return (None, text.to_owned());
    }
    let Some(end) = text[4..].find("\n---\n").map(|at| at + 4) else {
        errors.push(format!("{path}: front matter has no closing `---`"));
        return (None, text.to_owned());
    };
    // In the order written, a later value of a key replacing the earlier, as a dict's.
    let mut fields: Vec<(String, String)> = Vec::new();
    for (n, line) in text[4..end].split('\n').enumerate() {
        let n = n + 2;
        let (key, value) = match line.split_once(": ") {
            Some((key, value)) if !value.trim().is_empty() => (key, value.trim()),
            _ => {
                errors.push(format!("{path}:{n}: front matter line is not `key: value`: {}", py_repr(line)));
                continue;
            }
        };
        match fields.iter_mut().find(|(k, _)| k == key) {
            Some(field) => {
                errors.push(format!("{path}:{n}: `{key}` given twice"));
                field.1 = value.to_owned();
            }
            None => fields.push((key.to_owned(), value.to_owned())),
        }
    }
    let body = text[end + 5..].to_owned();
    let unknown: BTreeSet<&str> =
        fields.iter().map(|(k, _)| k.as_str()).filter(|k| !FRONT_FIELDS.contains(k)).collect();
    if !unknown.is_empty() {
        let unknown: Vec<&str> = unknown.into_iter().collect();
        errors.push(format!(
            "{path}: unknown front matter {} (status, summary, date, superseded_by)",
            unknown.join(", ")
        ));
        return (None, body);
    }
    let field = |name: &str| fields.iter().find(|(k, _)| k == name).map(|(_, v)| v.clone());
    let (Some(status), Some(summary)) = (field("status"), field("summary")) else {
        errors.push(format!("{path}: front matter needs `status` and `summary`"));
        return (None, body);
    };
    let front = Front { status, summary, date: field("date"), superseded_by: field("superseded_by") };
    if !STATUSES.contains(&front.status.as_str()) {
        errors.push(format!("{path}: status `{}` is not one of {}", front.status, STATUSES.join(", ")));
    }
    if front.status != "current" && front.date.is_none() {
        errors.push(format!("{path}: a {} doc needs `date: YYYY-MM-DD`", front.status));
    }
    if let Some(date) = &front.date
        && !p.date.is_match(date)
    {
        errors.push(format!("{path}: date `{date}` is not YYYY-MM-DD"));
    }
    if (front.status == "superseded") != front.superseded_by.is_some() {
        errors.push(format!("{path}: `superseded_by` goes with status superseded, and only with it"));
    }
    (Some(front), body)
}

/// A path relative to the root, with `/` between its parts.
fn rel(root: &Path, full: &Path) -> String {
    let parts: Vec<String> = full
        .strip_prefix(root)
        .unwrap_or(full)
        .components()
        .map(|it| it.as_os_str().to_string_lossy().into_owned())
        .collect();
    parts.join("/")
}

/// Python's `os.walk(docs)` with both lists sorted and docs/upstream left out: a directory's
/// files, then each of its directories in turn.
fn walk(root: &Path, dir: &Path, p: &Patterns, docs: &mut Vec<Doc>, errors: &mut Vec<String>) {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) => fail(SAYS, &format!("{} is not read: {e}", dir.display())),
    };
    let (mut dirs, mut files) = (Vec::new(), Vec::new());
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        // A link to a directory is not walked into, as os.walk does not follow one.
        match entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            true => dirs.push(name),
            false => files.push(name),
        }
    }
    dirs.sort();
    files.sort();
    let index = root.join("docs").join("README.md");
    for name in files {
        let full = dir.join(&name);
        if !name.ends_with(".md") || full == index {
            continue;
        }
        let path = rel(root, &full);
        let Some(text) = read_text(&full) else { fail(SAYS, &format!("{path} is not UTF-8 text")) };
        let (front, body) = parse_front(p, &path, &text, errors);
        docs.push(Doc { path, front, body: strip_fences(p, &body) });
    }
    for name in dirs {
        let full = dir.join(&name);
        if full != root.join("docs").join("upstream") {
            walk(root, &full, p, docs, errors);
        }
    }
}

fn load(root: &Path, p: &Patterns) -> (Vec<Doc>, Vec<String>) {
    let (mut docs, mut errors) = (Vec::new(), Vec::new());
    walk(root, &root.join("docs"), p, &mut docs, &mut errors);
    (docs, errors)
}

fn links(p: &Patterns, body: &str) -> Vec<String> {
    let mut out: Vec<String> = p.link.captures_iter(body).map(|m| m[1].to_owned()).collect();
    out.extend(p.ref_link.captures_iter(body).map(|m| m[1].to_owned()));
    out
}

/// Python's `urllib.parse.unquote`: `%XX` as bytes, read as UTF-8 with what is not replaced.
fn unquote(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = |b: u8| (b as char).to_digit(16);
        match (bytes[i], bytes.get(i + 1).copied().and_then(hex), bytes.get(i + 2).copied().and_then(hex)) {
            (b'%', Some(high), Some(low)) => {
                out.push((high * 16 + low) as u8);
                i += 3;
            }
            (b, _, _) => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Python's `os.path.normpath(os.path.join(base, path))`, on `/` paths: `path` absolute replaces
/// `base`; `.` and empty parts go; `..` takes the part before it.
fn join_normal(base: &Path, path: &str) -> PathBuf {
    let joined = if path.starts_with('/') { PathBuf::from(path) } else { base.join(path) };
    let mut out = PathBuf::new();
    for part in joined.components() {
        match part {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                // `/..` is `/`; a relative path that runs out keeps its `..`.
                if !out.pop() && !out.has_root() {
                    out.push("..");
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

fn check_link(
    root: &Path,
    p: &Patterns,
    doc: &Doc,
    target: &str,
    anchored: bool,
    cache: &mut BTreeMap<PathBuf, BTreeSet<String>>,
    errors: &mut Vec<String>,
) {
    if p.scheme.is_match(target) || target.starts_with("//") {
        return;
    }
    let (path, anchor) = target.split_once('#').unwrap_or((target, ""));
    let path = unquote(path.split('?').next().unwrap_or(""));
    let doc_dir = Path::new(&doc.path).parent().unwrap_or(Path::new(""));
    let full = if path.is_empty() { root.join(&doc.path) } else { join_normal(&root.join(doc_dir), &path) };
    if !full.exists() {
        errors.push(format!("{}: links to {target}, which does not exist", doc.path));
        return;
    }
    if anchored && !anchor.is_empty() && full.to_string_lossy().ends_with(".md") {
        if !cache.contains_key(&full) {
            let Some(text) = read_text(&full) else { fail(SAYS, &format!("{} is not UTF-8 text", full.display())) };
            cache.insert(full.clone(), anchors(p, &strip_fences(p, &text)));
        }
        if !cache.get(&full).is_some_and(|found| found.contains(&unquote(anchor).to_lowercase())) {
            errors.push(format!(
                "{}: links to {target}, and {} has no heading #{anchor}",
                doc.path,
                rel(root, &full)
            ));
        }
    }
}

/// `git <args>` in the root, its output split at NULs; the script ends when git fails, as
/// `subprocess.run(check=True)` raised.
fn git_z(root: &Path, args: &[&str]) -> Vec<String> {
    let mut argv = vec!["git"];
    argv.extend_from_slice(args);
    match run(&argv, root) {
        Ok(ran) if ran.status == 0 => {
            ran.out().split('\0').filter(|it| !it.is_empty()).map(str::to_owned).collect()
        }
        Ok(ran) => fail(SAYS, &format!("git {} exited {}: {}", args.join(" "), ran.status, ran.err().trim())),
        Err(e) => fail(SAYS, &e),
    }
}

fn tracked(root: &Path) -> Vec<String> {
    git_z(root, &["ls-files", "-z"])
}

/// Every .py file in the tree that git does not ignore, tracked or not.
fn python_files(root: &Path) -> Vec<String> {
    let found = git_z(root, &["ls-files", "-z", "--cached", "--others", "--exclude-standard", "--", "*.py"]);
    let found: BTreeSet<String> = found.into_iter().filter(|it| root.join(it).exists()).collect();
    found.into_iter().collect()
}

fn allowed(text: &str) -> Vec<String> {
    text.split('\n')
        .filter(|line| !line.trim().is_empty() && !line.starts_with('#'))
        .map(|line| line.trim().to_owned())
        .collect()
}

/// What is wrong between the .py `files` of the tree and the paths `listed` as allowed.
fn python_problems(files: &[String], listed: &[String]) -> Vec<String> {
    let mut errors: Vec<String> = files
        .iter()
        .filter(|path| !listed.contains(path))
        .map(|path| {
            format!(
                "{path}: a new Python file: write it as a Rust script (docs/v2/plan/design/scripts.md); \
                 tools/python-allowed.txt only shrinks"
            )
        })
        .collect();
    errors.extend(listed.iter().filter(|path| !files.contains(path)).map(|path| {
        format!("tools/python-allowed.txt: {path} is gone: run `blaze run -q -p scripts --bin docs -- write` and commit it")
    }));
    let sorted: Vec<&String> = listed.iter().collect::<BTreeSet<_>>().into_iter().collect();
    if errors.is_empty() && listed.iter().collect::<Vec<_>>() != sorted {
        errors.push(
            "tools/python-allowed.txt: not sorted, or a path twice: run `blaze run -q -p scripts --bin docs -- write`".to_owned(),
        );
    }
    errors
}

/// The list `write` leaves: what was listed and is still there. Never a path that was not listed.
fn allowed_after(files: &[String], listed: &[String]) -> String {
    let kept: BTreeSet<&String> = listed.iter().filter(|path| files.contains(path)).collect();
    let mut out = ALLOWED_HEAD.to_owned();
    for path in kept {
        out.push_str(path);
        out.push('\n');
    }
    out
}

/// `python`'s input, `{"left": N}`: a field it does not have is refused.
// when bstd::data is released: derive by reflection.
fn left(text: &str) -> i64 {
    let value: Value = match blaze_script::json::from_str(text) {
        Ok(value) => value,
        Err(e) => fail(SAYS, &format!("the input is not JSON: {e}")),
    };
    let Value::Object(fields) = &value else {
        fail(SAYS, &format!("the input is a JSON object, not {}", dumps(&value)))
    };
    let unknown: BTreeSet<&str> = fields.keys().map(String::as_str).filter(|key| *key != "left").collect();
    if !unknown.is_empty() {
        let unknown: Vec<&str> = unknown.into_iter().collect();
        fail(SAYS, &format!("no field {} (the fields are left)", unknown.join(", ")));
    }
    match fields.get("left") {
        None => fail(SAYS, "field left is required"),
        Some(Value::Number(n)) if n.is_i64() => n.as_i64().unwrap_or(0),
        Some(other) => fail(SAYS, &format!("left is a int, not {}", dumps(other))),
    }
}

fn allowed_now(root: &Path) -> Vec<String> {
    allowed(&read_text(&root.join("tools").join("python-allowed.txt")).unwrap_or_default())
}

fn check(root: &Path, p: &Patterns) -> Vec<String> {
    let (docs, mut errors) = load(root, p);
    errors.extend(python_problems(&python_files(root), &allowed_now(root)));
    let mut settings: BTreeSet<String> = BTreeSet::new();
    for path in tracked(root) {
        if NOT_OURS.iter().any(|ours| path.starts_with(ours)) {
            continue;
        }
        let Some(text) = read_text(&root.join(&path)) else { continue };
        let markdown = path.ends_with(".md");
        if !markdown && text.contains("BLAZE_") {
            settings.extend(p.settings(&text).into_iter().map(str::to_owned));
        }
        // Most files name no doc: the substring search skips them, and every line but the
        // few that hold `docs/`, before any regex runs.
        if !text.contains("docs/") {
            continue;
        }
        let text = if markdown { strip_fences(p, &text) } else { text };
        for (n, line) in text.split('\n').enumerate() {
            if !line.contains("docs/") {
                continue;
            }
            for named in p.docs_named(line) {
                if !root.join(named).exists() {
                    errors.push(format!("{path}:{}: names {named}, which does not exist", n + 1));
                }
            }
        }
    }
    let mut cache = BTreeMap::new();
    for doc in &docs {
        let Some(front) = &doc.front else { continue };
        let status = front.status.as_str();
        if let Some(by) = &front.superseded_by {
            check_link(root, p, doc, by, true, &mut cache, &mut errors);
        }
        for target in links(p, &doc.body) {
            check_link(root, p, doc, &target, matches!(status, "current" | "plan"), &mut cache, &mut errors);
        }
        if status != "current" {
            continue;
        }
        for span in spans(&doc.body) {
            let token = p.line_suffix.replace(span.trim(), "");
            let named = token.trim_end_matches('/');
            if p.path.is_match(&token) && !token.contains("...") && !root.join(named).exists() {
                errors.push(format!("{}: names `{token}`, which does not exist", doc.path));
            }
        }
        let prose = p.inline_link.replace_all(&doc.body, "");
        let named: BTreeSet<&str> = p.settings(&prose).into_iter().collect();
        for name in named {
            if !settings.contains(name) {
                errors.push(format!("{}: names {name}, which no code reads", doc.path));
            }
        }
    }
    if errors.is_empty() && read_text(&root.join("docs").join("README.md")).as_deref() != Some(index(&docs).as_str())
    {
        errors.push("docs/README.md is stale: run `blaze run -q -p scripts --bin docs -- write` and commit it".to_owned());
    }
    errors
}

fn index(docs: &[Doc]) -> String {
    fn line(doc: &Doc, front: &Front) -> String {
        let when = match (front.status.as_str(), &front.date) {
            ("current", _) | (_, None) => String::new(),
            (_, Some(date)) => format!(" ({date})"),
        };
        let path = doc.path.strip_prefix("docs/").unwrap_or(&doc.path);
        format!("- [{path}]({path}){when}: {}", front.summary)
    }
    fn group(docs: &[Doc], status: &str, title: &str, intro: &str) -> Vec<String> {
        let mut chosen: Vec<(&Doc, &Front)> = docs
            .iter()
            .filter_map(|doc| doc.front.as_ref().filter(|front| front.status == status).map(|front| (doc, front)))
            .collect();
        if status != "current" {
            // Newest first, and among one date the later path first.
            chosen.sort_by(|a, b| (&b.1.date, &b.0.path).cmp(&(&a.1.date, &a.0.path)));
        }
        let mut out = vec![format!("## {title}"), String::new(), intro.to_owned(), String::new()];
        out.extend(chosen.into_iter().map(|(doc, front)| line(doc, front)));
        out.push(String::new());
        out
    }
    let mut out: Vec<String> = [
        "<!-- Generated by `blaze run -q -p scripts --bin docs -- write` from each doc's front matter; edit those, not this. -->",
        "# Documents",
        "",
        "Each document under docs/ says in its front matter what it is (scripts/src/bin/docs.rs). Superseded ones",
        "are left out here; each names what replaced it. docs/upstream/ holds others' text, as fetched.",
        "",
    ]
    .iter()
    .map(|it| (*it).to_owned())
    .collect();
    out.extend(group(
        docs,
        "current",
        "Current",
        "How blaze is now. Kept true: CI checks their links, paths and settings against the tree.",
    ));
    out.extend(group(docs, "plan", "Plans", "Designs not (fully) built, newest first."));
    out.extend(group(
        docs,
        "record",
        "Records",
        "Research, reports and measurements, true on their date and not brought up to date. Newest first.",
    ));
    format!("{}\n", out.join("\n").trim_end_matches('\n'))
}

fn write_file(path: &Path, text: &str) {
    if let Err(e) = std::fs::write(path, text) {
        fail(SAYS, &format!("{} is not written: {e}", path.display()));
    }
}

fn selftest(p: &Patterns) {
    let expect = |holds: bool, what: &str| blaze_script::expect(SAYS, holds, what);
    let paths = |names: &[&str]| -> Vec<String> { names.iter().map(|it| (*it).to_owned()).collect() };
    // tools/docs.py's selftest, case for case.
    expect(
        python_problems(&paths(&["a.py", "b/c.py"]), &paths(&["a.py", "b/c.py"])).is_empty(),
        "a tree that is its list holds",
    );
    let added = python_problems(&paths(&["a.py", "new.py"]), &paths(&["a.py"]));
    expect(
        added.len() == 1 && added[0].starts_with("new.py: a new Python file"),
        "a file that is not listed is refused, by name",
    );
    let gone = python_problems(&paths(&["a.py"]), &paths(&["a.py", "gone.py"]));
    expect(
        gone.len() == 1 && gone[0].contains("gone.py is gone"),
        "a line whose file is gone is refused, by name",
    );
    expect(
        !python_problems(&paths(&["a.py", "b.py"]), &paths(&["b.py", "a.py"])).is_empty(),
        "an unsorted list is refused",
    );
    expect(
        allowed(&allowed_after(&paths(&["a.py", "new.py"]), &paths(&["a.py", "gone.py"]))) == paths(&["a.py"]),
        "write drops what is gone and adds nothing",
    );
    expect(
        allowed(&format!("{ALLOWED_HEAD}\n a.py \n# b.py\n")) == paths(&["a.py"]),
        "comments and blank lines are not paths",
    );
    expect(python_problems(&[], &[]).is_empty(), "no Python and an empty list hold");
    // The patterns Python wrote with a lookaround or a backreference, one case each way.
    expect(
        p.settings("BLAZE_HOME, BLAZE_X_ (BLAZE_A_B) BLAZE_*, BLAZE_FOO* xBLAZE_NO BLAZE_lower")
            == ["BLAZE_HOME", "BLAZE_A_B"],
        "a setting is a whole BLAZE_ name, not a prefix of one or a glob",
    );
    // Written with `{d}` so that this file names no doc that `check` would look for.
    let d = "docs";
    expect(
        p.docs_named(&format!("see {d}/a.md, x/{d}/b.md, {d}/c/d.md. and {d}/e.mdx"))
            == [format!("{d}/a.md"), format!("{d}/c/d.md")],
        "a doc is named by its path from the root, not as the tail of another path",
    );
    expect(
        spans("a `b` c ``d`e`` f `g\nh` ```i`` j`") == ["b", "d`e", " ", "i"],
        "a span ends at the next run of as many backticks, on its line",
    );
    expect(
        strip_fences(p, "a\n```rust\n`x`\n```\nb\n~~~\nc\n~~~~\nd") == "a\n\n\n\nb\n\n\n\nd",
        "fenced code is blanked, line for line",
    );
    let mut seen = BTreeMap::new();
    expect(
        slug(p, "The [gate](ci.md)'s `checks`, twice!", &mut seen) == "the-gates-checks-twice"
            && slug(p, "The gates checks twice", &mut seen) == "the-gates-checks-twice-1",
        "a heading's anchor is GitHub's, and a second of the same text is numbered",
    );
    expect(
        anchors(p, "# One\n\n## Two words ##\n<a id=\"three\"></a>\n")
            == ["one", "three", "two-words"].iter().map(|it| (*it).to_owned()).collect::<BTreeSet<String>>(),
        "a doc's anchors are its headings' and its `<a id>`s",
    );
    expect(
        links(p, "[a](b.md#c) ![i](<d e.png> \"t\") [n [m]](f.md)\n[r]: g.md\n") == ["b.md#c", "f.md", "g.md"],
        "inline, image and reference links are found",
    );
    expect(unquote("a%20b%C3%A9%zz") == "a b\u{e9}%zz", "a link's path is unquoted");
    expect(
        join_normal(Path::new("/r/docs/v2"), "../a/./b.md") == Path::new("/r/docs/a/b.md")
            && join_normal(Path::new("/r/docs"), "/abs/x.md") == Path::new("/abs/x.md"),
        "a link is resolved from its doc's directory",
    );
    let mut errors = Vec::new();
    let (front, body) = parse_front(p, "d.md", "---\nstatus: plan\nsummary: s\ndate: 2026-11-11\n---\nbody\n", &mut errors);
    expect(
        errors.is_empty()
            && body == "body\n"
            && front
                == Some(Front {
                    status: "plan".to_owned(),
                    summary: "s".to_owned(),
                    date: Some("2026-11-11".to_owned()),
                    superseded_by: None,
                }),
        "front matter is read",
    );
    let mut errors = Vec::new();
    parse_front(p, "d.md", "---\nstatus: plan\nsummary: s\nbad\n---\n", &mut errors);
    expect(
        errors
            == ["d.md:4: front matter line is not `key: value`: 'bad'", "d.md: a plan doc needs `date: YYYY-MM-DD`"],
        "a bad line and a missing date are refused, naming the doc",
    );
    println!("docs.py selftest: ok");
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    // The checkout this script was built in, as the Python took the one its file was in.
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap_or(Path::new("."));
    let p = Patterns::new();
    let end = |errors: Vec<String>| {
        if !errors.is_empty() {
            eprintln!("{}", errors.join("\n"));
            fail(SAYS, &format!("{} problems", errors.len()));
        }
    };
    match args.as_slice() {
        ["python", input] => {
            let (most, files) = (left(input), python_files(root));
            let mut errors = python_problems(&files, &allowed_now(root));
            if i64::try_from(files.len()).unwrap_or(i64::MAX) > most {
                errors.push(format!(
                    "{} Python files are left (tools/python-allowed.txt), more than {most}",
                    files.len()
                ));
            }
            end(errors);
        }
        ["selftest"] => selftest(&p),
        ["write"] => {
            let (docs, errors) = load(root, &p);
            if !errors.is_empty() {
                eprintln!("{}", errors.join("\n"));
                std::process::exit(1);
            }
            write_file(&root.join("docs").join("README.md"), &index(&docs));
            let kept = allowed_after(&python_files(root), &allowed_now(root));
            write_file(&root.join("tools").join("python-allowed.txt"), &kept);
        }
        ["check"] => end(check(root, &p)),
        _ => fail(SAYS, "one subcommand: check, write, selftest, or python with its input"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every pattern compiles with the regex features the lock builds (a `(?i)` needs
    /// `unicode-case`), so a missing feature fails `blaze test`, not the script's first run.
    #[test]
    fn the_patterns_compile() {
        let _ = Patterns::new();
    }
}
