//! The compiler probes a wasm build script makes, answered from the pinned table
//! pins/answers/clang.json (c.md D174, design A; rules 1, 3, 21, 25), sans-IO.
//!
//! A script that builds C with the `cc` crate asks its compiler questions before it compiles
//! anything: which family it is (`cc -E <OUT_DIR>/<n>detect_compiler_family.c`, whose stdout
//! names `clang`), whether it is MSVC (`cc -?`) and, for each `flag_if_supported`, whether a
//! flag compiles cleanly (`-o <OUT_DIR>/flag_check -c <OUT_DIR>/flag_check.c`, supported when
//! the exit is 0 and stderr empty). A check runs no compiler, and a deferred request answered
//! exit 0 is a lie the script acts on (every flag supported, `-?` taken for MSVC). So each probe
//! is answered from the table, which holds what the shelf's clang printed for it
//! (scripts/src/bin/clang_answers.rs runs the clang the table names and writes the answers).
//!
//! A row is keyed by what is asked, all of it (rule 21): the clang archive's sha256 (the
//! shelf's name), the program's name, the arguments, the spawn's variables clang reads
//! ([`CLANG_ENV`]) and the content of each file the probe reads. Where the script put a probe's
//! files and what it named them is not asked: cc's `NamedTempfile` puts per-run digits before a
//! name (`563577319973579455detect_compiler_family.c`), and a script may give a build its own
//! out folder below `OUT_DIR` (aws-lc-sys's `out-<check>/`). So an argument that is a path
//! under the guest `OUT_DIR`, at any depth, is spelled by a placeholder: [`INPUT`]`<n><ext>` when
//! a file is there (its sha256 in the row's `files`), [`OUTPUT`]`<n><ext>` when none is (what the
//! probe writes), `n` counting each kind in argument order and `<ext>` the name's extension (clang
//! takes the language from it). Any other `OUT_DIR` or SDK folder in an argument is spelled
//! `$OUT_DIR` or `$SDKROOT`. Each placeholder is put back as the script's path in the answer. A
//! probe with no row, or a row still PENDING, is a [`ProbeRefused`] that prints the row to add
//! (rule 3: never an answer made up).
//!
//! The table is in every script's base key ([`table_id`], `key::base`), so a changed answer
//! re-runs the scripts and no read is recorded per probe.

#![blaze::sans_io]

use core::fmt;

use blaze_ids::{ContentId, Hasher};
use blaze_json::{Event, Limits, Reader, Str};
use blaze_own::{AString, AVec, Alloc, String, Vec};

/// The table's path in a checkout (blaze-pins includes it).
pub const PATH: &str = "pins/answers/clang.json";
/// The one form of the table read here (its `form`).
pub const FORM: u64 = 1;
const ID_DOMAIN: &str = "blaze-build-script clang answers table 1";

/// The guest `OUT_DIR`'s spelling in a row.
pub const OUT_DIR: &str = "$OUT_DIR";
/// The guest SDK folder's spelling in a row.
pub const SDKROOT: &str = "$SDKROOT";
/// The head of a file a probe reads under `OUT_DIR`, as a row spells it (`$IN0.c`).
pub const INPUT: &str = "$IN";
/// The head of a path under `OUT_DIR` a probe names where no file is (`-o $OUT0`).
pub const OUTPUT: &str = "$OUT";

/// The environment variables clang's driver reads that can change a probe's answer, sorted. A
/// spawn's value of each is part of its row; every other variable is not read by clang.
pub const CLANG_ENV: [&str; 11] = [
    "CCC_OVERRIDE_OPTIONS",
    "COMPILER_PATH",
    "CPATH",
    "CPLUS_INCLUDE_PATH",
    "C_INCLUDE_PATH",
    "MACOSX_DEPLOYMENT_TARGET",
    "OBJCPLUS_INCLUDE_PATH",
    "OBJC_INCLUDE_PATH",
    "SDKROOT",
    "SOURCE_DATE_EPOCH",
    "TMPDIR",
];

/// The C and C++ compiler names a probe is spawned as.
const COMPILERS: &[&str] = &["c++", "cc", "clang", "clang++", "g++", "gcc"];

fn name_of(program: &str) -> &str {
    program.rsplit('/').next().unwrap_or(program)
}

/// Whether the spawn of `program` with `args` is a compiler probe, from its text alone: a C
/// compiler asked to preprocess (`-E`), asked `-?`, or writing `-o …/flag_check` (cc's
/// `is_flag_supported`). Any other compile is a C request (`c_tasks`).
pub fn is_probe(program: &str, args: &[&str]) -> bool {
    COMPILERS.contains(&name_of(program))
        && (args.iter().any(|arg| matches!(*arg, "-E" | "-?")) || args.windows(2).any(|pair| pair[0] == "-o" && name_of(pair[1]) == "flag_check"))
}

/// The table's identity for the base key: its bytes, under this form.
pub fn table_id(bytes: &[u8]) -> ContentId {
    let mut hasher = Hasher::new(ID_DOMAIN);
    hasher.update(&FORM.to_le_bytes()).part(bytes);
    hasher.finish()
}

/// What the table says a probe printed.
#[derive(Debug, PartialEq, Eq)]
pub enum Answer<'a> {
    /// Not yet run by clang_answers.rs.
    Pending,
    Known { code: i32, stdout: AString<'a>, stderr: AString<'a> },
}

/// One row: a probe's whole input and its answer.
#[derive(Debug)]
pub struct Row<'a> {
    pub clang: AString<'a>,
    pub program: AString<'a>,
    pub args: AVec<'a, AString<'a>>,
    /// `(name, value)`, the names from [`CLANG_ENV`].
    pub env: AVec<'a, (AString<'a>, AString<'a>)>,
    /// `($IN<n><ext>, sha256 hex)` of each file the probe reads under `OUT_DIR`.
    pub files: AVec<'a, (AString<'a>, AString<'a>)>,
    pub answer: Answer<'a>,
}

/// The table, parsed.
#[derive(Debug)]
pub struct Table<'a> {
    id: ContentId,
    rows: AVec<'a, Row<'a>>,
}

/// Why the table was refused: the byte offset in it and what was wrong there.
#[derive(Debug)]
pub struct TableError {
    pub at: usize,
    pub why: TableWhy,
}

#[derive(Debug)]
pub enum TableWhy {
    Json(blaze_json::What),
    /// `form` is not [`FORM`].
    Form,
    /// The text is not the shape a table has: what was wanted.
    Shape(&'static str),
    /// A key a map already had.
    TwoKeys,
    /// A row whose key an earlier row has (rule 21): its index.
    TwoRows { row: usize },
}

impl fmt::Display for TableError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{PATH} at byte {}: ", self.at)?;
        match &self.why {
            TableWhy::Json(what) => write!(f, "{what}"),
            TableWhy::Form => write!(f, "`form` is not {FORM}"),
            TableWhy::Shape(wanted) => write!(f, "{wanted} was wanted"),
            TableWhy::TwoKeys => f.write_str("a key the object already has"),
            TableWhy::TwoRows { row } => write!(f, "rows[{row}] has the key of an earlier row: two answers to one probe (rule 21)"),
        }
    }
}

/// A probe the table does not answer.
#[derive(Debug, PartialEq, Eq)]
pub struct ProbeRefused<'a> {
    pub why: ProbeWhy,
    /// The row to add to the table, as one line of JSON with its answer PENDING.
    pub row: AString<'a>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProbeWhy {
    /// No row has this probe's key.
    NoRow,
    /// Its row has not been answered by clang_answers.rs.
    Pending,
    /// The probe was given standard input, which no row holds.
    Stdin,
}

impl fmt::Display for ProbeRefused<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.why {
            ProbeWhy::NoRow => write!(f, "a compiler probe {PATH} has no row for (rule 3: a probe is answered only from the pinned table)")?,
            ProbeWhy::Pending => write!(f, "a compiler probe whose row in {PATH} is still PENDING")?,
            ProbeWhy::Stdin => {
                return write!(f, "a compiler probe given standard input: {PATH} keys no input but the arguments, the environment and the files (rule 21): {}", self.row.as_str());
            }
        }
        write!(f, "; add or keep the row and run scripts/src/bin/clang_answers.rs: {}", self.row.as_str())
    }
}

/// A spawn, as the script made it.
#[derive(Clone, Copy, Debug)]
pub struct Spawn<'s> {
    pub program: &'s str,
    pub args: &'s [&'s str],
    /// The child's environment (any order; only [`CLANG_ENV`] is read).
    pub env: &'s [(&'s str, &'s str)],
    /// The guest `OUT_DIR`, without a trailing `/`.
    pub out_dir: &'s str,
    /// The guest SDK folder; empty where the target has none.
    pub sdk: &'s str,
    /// What the spawn was given on standard input: a probe answers only when it is empty.
    pub stdin: &'s [u8],
}

/// The table's answer to a probe.
#[derive(Debug, PartialEq, Eq)]
pub struct Answered<'a> {
    pub code: i32,
    pub stdout: AString<'a>,
    pub stderr: AString<'a>,
}

impl<'a> Table<'a> {
    /// The table in `bytes`, every row checked: its shape, `form`, and no two rows with one key.
    pub fn parse(bytes: &[u8], alloc: Alloc<'a>) -> Result<Table<'a>, TableError> {
        let reader = Reader::new_in(bytes, Limits::SHIPPED, alloc).map_err(|e| TableError { at: e.at, why: TableWhy::Json(e.what) })?;
        let mut parse = Parse { reader, alloc, at: 0 };
        let mut rows: AVec<'a, Row<'a>> = Vec::new_in(alloc);
        parse.want_map()?;
        let mut seen = [false; 5];
        while let Some(key) = parse.key()? {
            let slot = match key {
                "comment" => 0,
                "form" => 1,
                "made_by" => 2,
                "sources" => 3,
                "rows" => 4,
                _ => return Err(parse.shape("a key of comment, form, made_by, sources, rows")),
            };
            if core::mem::replace(&mut seen[slot], true) {
                return Err(parse.error(TableWhy::TwoKeys));
            }
            match slot {
                1 => match parse.next()? {
                    Event::Int(n) if n.as_u64() == Some(FORM) => {}
                    _ => return Err(parse.error(TableWhy::Form)),
                },
                3 => {
                    parse.want_map()?;
                    while parse.key()?.is_some() {
                        parse.text()?;
                    }
                }
                4 => {
                    parse.want(Event::StartList, "a list of rows")?;
                    while let Some(row) = parse.row()? {
                        if rows.iter().any(|it| same_key(it, &row)) {
                            return Err(parse.error(TableWhy::TwoRows { row: rows.len() }));
                        }
                        rows.push(row);
                    }
                }
                _ => {
                    parse.text()?;
                }
            }
        }
        if !seen[1] || !seen[4] {
            return Err(parse.shape("`form` and `rows`"));
        }
        if parse.reader.next().map_err(|e| TableError { at: e.at, why: TableWhy::Json(e.what) })?.is_some() {
            return Err(parse.shape("the end of the text"));
        }
        Ok(Table { id: table_id(bytes), rows })
    }

    pub fn id(&self) -> ContentId {
        self.id
    }

    pub fn rows(&self) -> &[Row<'a>] {
        &self.rows
    }

    /// The answer to `spawn` (a probe: [`is_probe`]) from the rows of `clang` (this host's clang
    /// archive sha256, `toolchain::clang_pin`'s). `file` gives the sha256 of a file under
    /// `OUT_DIR` by its `OUT_DIR`-relative path (`out-x/1a.c`), or `None` when there is none.
    /// Sans-IO.
    pub fn answer<'b>(
        &self,
        clang: &[u8; 32],
        spawn: &Spawn<'_>,
        mut file: impl FnMut(&str) -> Option<[u8; 32]>,
        alloc: Alloc<'b>,
    ) -> Result<Answered<'b>, ProbeRefused<'b>> {
        let clang = hex(clang, alloc);
        let asked = asked(clang.as_str(), spawn, &mut file, alloc);
        if !spawn.stdin.is_empty() {
            return Err(ProbeRefused { why: ProbeWhy::Stdin, row: row_json(&asked.row, alloc) });
        }
        let Some(row) = self.rows.iter().find(|row| same_key(row, &asked.row)) else {
            return Err(ProbeRefused { why: ProbeWhy::NoRow, row: row_json(&asked.row, alloc) });
        };
        let Answer::Known { code, stdout, stderr } = &row.answer else {
            return Err(ProbeRefused { why: ProbeWhy::Pending, row: row_json(&asked.row, alloc) });
        };
        let back = |text: &str| asked.unspell(text, spawn, alloc);
        Ok(Answered { code: *code, stdout: back(stdout.as_str()), stderr: back(stderr.as_str()) })
    }
}

/// A probe's key as a row, and its path placeholders.
struct Asked<'b, 's> {
    row: Row<'b>,
    /// `($IN<n><ext> or $OUT<n><ext>, the argument as spawned)`.
    paths: AVec<'b, (AString<'b>, &'s str)>,
}

impl<'b> Asked<'b, '_> {
    /// `text` (an answer as the table spells it) as the script's guest paths spell it: each
    /// placeholder, longest first (`$IN10` before `$IN1`), then the folders.
    fn unspell(&self, text: &str, spawn: &Spawn<'_>, alloc: Alloc<'b>) -> AString<'b> {
        let mut pairs: AVec<'b, (&str, &str)> = Vec::new_in(alloc);
        for (spelled, actual) in self.paths.iter() {
            pairs.push((spelled.as_str(), *actual));
        }
        pairs.sort_unstable_by(|a, b| b.0.len().cmp(&a.0.len()).then_with(|| a.0.cmp(b.0)));
        pairs.push((OUT_DIR, spawn.out_dir));
        if !spawn.sdk.is_empty() {
            pairs.push((SDKROOT, spawn.sdk));
        }
        replaced(text, &pairs, alloc)
    }
}

fn asked<'b, 's>(clang: &str, spawn: &Spawn<'s>, file: &mut impl FnMut(&str) -> Option<[u8; 32]>, alloc: Alloc<'b>) -> Asked<'b, 's> {
    let own = |text: &str| String::from_str_in(text, alloc);
    let mut roots: AVec<'b, (&str, &str)> = Vec::new_in(alloc);
    roots.push((spawn.out_dir, OUT_DIR));
    if !spawn.sdk.is_empty() {
        roots.push((spawn.sdk, SDKROOT));
    }
    let mut args: AVec<'b, AString<'b>> = Vec::new_in(alloc);
    let mut files: AVec<'b, (AString<'b>, AString<'b>)> = Vec::new_in(alloc);
    let mut paths: AVec<'b, (AString<'b>, &'s str)> = Vec::new_in(alloc);
    let (mut inputs, mut outputs) = (0u32, 0u32);
    for arg in spawn.args.iter().copied() {
        let rel = arg.strip_prefix(spawn.out_dir).and_then(|rest| rest.strip_prefix('/')).filter(|rel| !rel.is_empty() && !rel.ends_with('/'));
        let Some(rel) = rel else {
            args.push(spelled_roots(arg, &roots, alloc));
            continue;
        };
        // One path, one placeholder, however often it is named.
        if let Some((spelled, _)) = paths.iter().find(|(_, actual)| *actual == arg) {
            args.push(spelled.clone());
            continue;
        }
        let digest = file(rel);
        let (head, count) = match digest {
            Some(_) => (INPUT, &mut inputs),
            None => (OUTPUT, &mut outputs),
        };
        let mut spelled = own(head);
        push_decimal(&mut spelled, *count);
        *count += 1;
        spelled.push_str(extension(rel));
        if let Some(digest) = digest {
            files.push((spelled.clone(), hex(&digest, alloc)));
        }
        paths.push((spelled.clone(), arg));
        args.push(spelled);
    }
    // Each placeholder once (above), so the unstable sort is the one order.
    files.sort_unstable_by(|a, b| a.0.as_str().cmp(b.0.as_str()));
    let mut env: AVec<'b, (AString<'b>, AString<'b>)> = Vec::new_in(alloc);
    for name in CLANG_ENV {
        if let Some((_, value)) = spawn.env.iter().rev().find(|(have, _)| *have == name) {
            env.push((own(name), spelled_roots(value, &roots, alloc)));
        }
    }
    let row = Row { clang: own(clang), program: own(name_of(spawn.program)), args, env, files, answer: Answer::Pending };
    Asked { row, paths }
}

/// The extension of the last name of `path` with its dot (`.c`, `.cpp`), or "" where it has
/// none a placeholder may carry (no dot after the name's first byte, or a byte past
/// `[A-Za-z0-9+_-]`).
fn extension(path: &str) -> &str {
    let name = path.rsplit('/').next().unwrap_or(path);
    match name.rfind('.') {
        Some(dot) if dot > 0 && dot + 1 < name.len() && name[dot + 1..].bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'_' | b'-')) => &name[dot..],
        _ => "",
    }
}

fn push_decimal(out: &mut AString<'_>, value: u32) {
    let mut digits = [0u8; 10];
    let mut at = digits.len();
    let mut left = value;
    loop {
        at -= 1;
        digits[at] = b'0' + (left % 10) as u8;
        left /= 10;
        if left == 0 {
            break;
        }
    }
    for digit in &digits[at..] {
        out.push(char::from(*digit));
    }
}

/// Whether two rows have one key: everything but the answer, `env` and `files` as sets.
fn same_key(a: &Row<'_>, b: &Row<'_>) -> bool {
    let same_set = |x: &[(AString<'_>, AString<'_>)], y: &[(AString<'_>, AString<'_>)]| {
        x.len() == y.len() && x.iter().all(|(k, v)| y.iter().any(|(k2, v2)| k.as_str() == k2.as_str() && v.as_str() == v2.as_str()))
    };
    a.clang.as_str() == b.clang.as_str()
        && a.program.as_str() == b.program.as_str()
        && a.args.len() == b.args.len()
        && a.args.iter().zip(b.args.iter()).all(|(x, y)| x.as_str() == y.as_str())
        && same_set(&a.env, &b.env)
        && same_set(&a.files, &b.files)
}

/// Whether `byte` may continue a path component (so a root followed by it is another name).
fn continues_name(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.')
}

/// `text` with each root (a guest folder) spelled as its placeholder where it stands whole.
fn spelled_roots<'b>(text: &str, roots: &[(&str, &str)], alloc: Alloc<'b>) -> AString<'b> {
    let mut out = String::new_in(alloc);
    let bytes = text.as_bytes();
    let mut at = 0;
    'scan: while at < text.len() {
        for (root, spelling) in roots.iter().copied() {
            if !root.is_empty() && text[at..].starts_with(root) && !bytes.get(at + root.len()).is_some_and(|b| continues_name(*b)) {
                out.push_str(spelling);
                at += root.len();
                continue 'scan;
            }
        }
        let width = text[at..].chars().next().map_or(1, char::len_utf8);
        out.push_str(&text[at..at + width]);
        at += width;
    }
    out
}

/// `text` with every `from` of `pairs` put as its `to`, the first pair that matches at a place
/// winning.
fn replaced<'b>(text: &str, pairs: &[(&str, &str)], alloc: Alloc<'b>) -> AString<'b> {
    let mut out = String::new_in(alloc);
    let mut at = 0;
    'scan: while at < text.len() {
        for (from, to) in pairs.iter().copied() {
            if !from.is_empty() && text[at..].starts_with(from) {
                out.push_str(to);
                at += from.len();
                continue 'scan;
            }
        }
        let width = text[at..].chars().next().map_or(1, char::len_utf8);
        out.push_str(&text[at..at + width]);
        at += width;
    }
    out
}

fn hex<'b>(bytes: &[u8], alloc: Alloc<'b>) -> AString<'b> {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::new_in(alloc);
    for byte in bytes {
        out.push(char::from(DIGITS[usize::from(byte >> 4)]));
        out.push(char::from(DIGITS[usize::from(byte & 15)]));
    }
    out
}

/// `text` as a JSON string, quoted.
fn push_json(out: &mut AString<'_>, text: &str) {
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            c if u32::from(c) < 0x20 => {
                const DIGITS: &[u8; 16] = b"0123456789abcdef";
                let n = u32::from(c) as usize;
                out.push_str("\\u00");
                out.push(char::from(DIGITS[n >> 4]));
                out.push(char::from(DIGITS[n & 15]));
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

/// The row as one line of the table's JSON, its answer PENDING.
fn row_json<'b>(row: &Row<'_>, alloc: Alloc<'b>) -> AString<'b> {
    let mut out = String::new_in(alloc);
    out.push_str("{\"clang\": ");
    push_json(&mut out, row.clang.as_str());
    out.push_str(", \"program\": ");
    push_json(&mut out, row.program.as_str());
    out.push_str(", \"args\": [");
    for (at, arg) in row.args.iter().enumerate() {
        if at > 0 {
            out.push_str(", ");
        }
        push_json(&mut out, arg.as_str());
    }
    for (field, pairs) in [("env", &row.env), ("files", &row.files)] {
        out.push_str(if field == "env" { "], \"env\": {" } else { "}, \"files\": {" });
        for (at, (key, value)) in pairs.iter().enumerate() {
            if at > 0 {
                out.push_str(", ");
            }
            push_json(&mut out, key.as_str());
            out.push_str(": ");
            push_json(&mut out, value.as_str());
        }
    }
    out.push_str("}, \"answer\": \"PENDING\"}");
    out
}

/// The table's events, read with the offset of the last one kept for errors.
struct Parse<'t, 'a> {
    reader: Reader<'t, Alloc<'a>>,
    alloc: Alloc<'a>,
    at: usize,
}

impl<'t, 'a> Parse<'t, 'a> {
    fn error(&self, why: TableWhy) -> TableError {
        TableError { at: self.at, why }
    }

    fn shape(&self, wanted: &'static str) -> TableError {
        self.error(TableWhy::Shape(wanted))
    }

    fn next(&mut self) -> Result<Event<'t>, TableError> {
        match self.reader.next() {
            Ok(Some(item)) => {
                self.at = item.at;
                Ok(item.event)
            }
            Ok(None) => Err(self.shape("more text")),
            Err(e) => Err(TableError { at: e.at, why: TableWhy::Json(e.what) }),
        }
    }

    fn want(&mut self, event: Event<'t>, wanted: &'static str) -> Result<(), TableError> {
        if self.next()? == event { Ok(()) } else { Err(self.shape(wanted)) }
    }

    fn want_map(&mut self) -> Result<(), TableError> {
        self.want(Event::StartMap, "an object")
    }

    /// The next key of the open map, or `None` at its end. Keys are plain words.
    fn key(&mut self) -> Result<Option<&'t str>, TableError> {
        match self.next()? {
            Event::Key(key) => key.as_plain().map(Some).ok_or_else(|| self.shape("a key without escapes")),
            Event::EndMap => Ok(None),
            _ => Err(self.shape("a key")),
        }
    }

    fn own(&self, text: Str<'t>) -> AString<'a> {
        let mut out = String::new_in(self.alloc);
        text.decode_into(&mut out);
        out
    }

    fn text(&mut self) -> Result<AString<'a>, TableError> {
        match self.next()? {
            Event::Text(text) => Ok(self.own(text)),
            _ => Err(self.shape("a string")),
        }
    }

    /// An object of strings to strings, its keys once each.
    fn pairs(&mut self) -> Result<AVec<'a, (AString<'a>, AString<'a>)>, TableError> {
        self.want_map()?;
        let mut pairs: AVec<'a, (AString<'a>, AString<'a>)> = Vec::new_in(self.alloc);
        loop {
            let key = match self.next()? {
                Event::Key(key) => self.own(key),
                Event::EndMap => return Ok(pairs),
                _ => return Err(self.shape("a key")),
            };
            if pairs.iter().any(|(have, _)| have.as_str() == key.as_str()) {
                return Err(self.error(TableWhy::TwoKeys));
            }
            let value = self.text()?;
            pairs.push((key, value));
        }
    }

    /// The next row of the open list, or `None` at its end.
    fn row(&mut self) -> Result<Option<Row<'a>>, TableError> {
        match self.next()? {
            Event::StartMap => {}
            Event::EndList => return Ok(None),
            _ => return Err(self.shape("a row object")),
        }
        let (mut clang, mut program, mut args, mut env, mut files, mut answer) = (None, None, None, None, None, None);
        while let Some(key) = self.key()? {
            let fresh = match key {
                "clang" => clang.replace(self.text()?).is_none(),
                "program" => program.replace(self.text()?).is_none(),
                "args" => {
                    self.want(Event::StartList, "a list of arguments")?;
                    let mut list: AVec<'a, AString<'a>> = Vec::new_in(self.alloc);
                    loop {
                        match self.next()? {
                            Event::Text(text) => list.push(self.own(text)),
                            Event::EndList => break,
                            _ => return Err(self.shape("an argument string")),
                        }
                    }
                    args.replace(list).is_none()
                }
                "env" => env.replace(self.pairs()?).is_none(),
                "files" => files.replace(self.pairs()?).is_none(),
                "answer" => answer.replace(self.answer()?).is_none(),
                _ => return Err(self.shape("a row key of clang, program, args, env, files, answer")),
            };
            if !fresh {
                return Err(self.error(TableWhy::TwoKeys));
            }
        }
        match (clang, program, args, env, files, answer) {
            (Some(clang), Some(program), Some(args), Some(env), Some(files), Some(answer)) => {
                Ok(Some(Row { clang, program, args, env, files, answer }))
            }
            _ => Err(self.shape("a row with clang, program, args, env, files and answer")),
        }
    }

    /// `"PENDING"` or `{code, stdout, stderr}`.
    fn answer(&mut self) -> Result<Answer<'a>, TableError> {
        match self.next()? {
            Event::Text(text) if text.as_plain() == Some("PENDING") => return Ok(Answer::Pending),
            Event::StartMap => {}
            _ => return Err(self.shape("\"PENDING\" or {code, stdout, stderr}")),
        }
        let (mut code, mut stdout, mut stderr) = (None, None, None);
        while let Some(key) = self.key()? {
            let fresh = match key {
                "code" => match self.next()? {
                    Event::Int(n) => match n.as_i64().and_then(|n| i32::try_from(n).ok()) {
                        Some(n) => code.replace(n).is_none(),
                        None => return Err(self.shape("an exit status in i32")),
                    },
                    _ => return Err(self.shape("an exit status")),
                },
                "stdout" => stdout.replace(self.text()?).is_none(),
                "stderr" => stderr.replace(self.text()?).is_none(),
                _ => return Err(self.shape("an answer key of code, stdout, stderr")),
            };
            if !fresh {
                return Err(self.error(TableWhy::TwoKeys));
            }
        }
        match (code, stdout, stderr) {
            (Some(code), Some(stdout), Some(stderr)) => Ok(Answer::Known { code, stdout, stderr }),
            _ => Err(self.shape("an answer with code, stdout and stderr")),
        }
    }
}
