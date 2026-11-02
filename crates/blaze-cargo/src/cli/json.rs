//! cargo's JSON messages (`util/machine_message.rs`) and the rustc diagnostic each
//! `compiler-message` carries (`rustc_errors/src/json.rs`), byte for byte: fields in the order
//! cargo and rustc write them, `null` where they write `null`, strings escaped as serde_json
//! escapes them. Each message is written straight into the caller's buffer, which it reuses:
//! no tree, no copy of a field, no allocation per message once the buffer has grown. The
//! messages a build streams are read where they lie in the daemon's frame
//! ([`blaze_api::borrowed`]): each string is copied once, into the buffer, from the frame.

use std::collections::hash_map::Entry;
use std::hash::{BuildHasher as _, BuildHasherDefault};
use std::io::Read as _;
use std::path::{Path, PathBuf};

use blaze_api::{
    Applicability, Artifact, BuildScriptRanRef, CrateType, DiagnosticRef, SpanRef, UnitOfRef, UnitTarget,
    UnitTargetRef,
};
use blaze_hash::FixedState;

use super::print::ByHash;

const HEX: &[u8; 16] = b"0123456789abcdef";

/// serde_json's escape table: 0 for a byte written as it is, else the letter after the `\`
/// (`u` for `\u00XX`). ESC is among the escaped, which is where cargo's stripping of colors
/// joins the escape.
const ESCAPE: [u8; 256] = {
    let mut table = [0u8; 256];
    let mut byte = 0;
    while byte < 0x20 {
        table[byte] = b'u';
        byte += 1;
    }
    table[0x08] = b'b';
    table[0x09] = b't';
    table[0x0a] = b'n';
    table[0x0c] = b'f';
    table[0x0d] = b'r';
    table[b'"' as usize] = b'"';
    table[b'\\' as usize] = b'\\';
    table
};

/// Whether none of the 8 bytes of `word` is escaped ([`ESCAPE`]: below 0x20, `"` or `\`), by
/// SWAR: each test is exact for whether any byte has it, which is all that is asked.
fn plain(word: u64) -> bool {
    const ONES: u64 = 0x0101_0101_0101_0101;
    const HIGH: u64 = 0x8080_8080_8080_8080;
    let below = word.wrapping_sub(ONES * 0x20) & !word;
    let quote = word ^ (ONES * u64::from(b'"'));
    let backslash = word ^ (ONES * u64::from(b'\\'));
    let zero = |it: u64| it.wrapping_sub(ONES) & !it;
    (below | zero(quote) | zero(backslash)) & HIGH == 0
}

/// `text` as serde_json writes a string: runs of bytes that need no escape are copied whole,
/// found 8 bytes at a time.
pub fn quote(out: &mut String, text: &str) {
    quote_with(out, text, false);
}

/// [`quote`], with the escapes that color a terminal taken out when `strip`, as
/// `anstream::adapter::strip_str` takes them out ([`super::say::strip_ansi`]): CSI sequences to
/// their final byte, OSC sequences to BEL or ST, any other escape with the character after it.
fn quote_with(out: &mut String, text: &str, strip: bool) {
    let bytes = text.as_bytes();
    out.reserve(bytes.len() + 2);
    out.push('"');
    let (mut run, mut at) = (0, 0);
    while at < bytes.len() {
        if let Some(chunk) = bytes.get(at..at + 8)
            && let Ok(word) = <[u8; 8]>::try_from(chunk)
            && plain(u64::from_le_bytes(word))
        {
            at += 8;
            continue;
        }
        let escape = ESCAPE[usize::from(bytes[at])];
        if escape == 0 {
            at += 1;
            continue;
        }
        out.push_str(&text[run..at]);
        if strip && bytes[at] == 0x1b {
            at = after_escape(text, at);
        } else {
            match escape {
                b'u' => {
                    out.push_str("\\u00");
                    out.push(char::from(HEX[usize::from(bytes[at] >> 4)]));
                    out.push(char::from(HEX[usize::from(bytes[at] & 0xf)]));
                }
                letter => {
                    out.push('\\');
                    out.push(char::from(letter));
                }
            }
            at += 1;
        }
        run = at;
    }
    out.push_str(&text[run..]);
    out.push('"');
}

/// Where the terminal escape at `at` ends; always on a character boundary, since every byte
/// that ends one is ASCII.
fn after_escape(text: &str, at: usize) -> usize {
    let bytes = text.as_bytes();
    match bytes.get(at + 1) {
        Some(b'[') => bytes[at + 2..]
            .iter()
            .position(|it| (0x40..=0x7e).contains(it))
            .map_or(bytes.len(), |end| at + 2 + end + 1),
        Some(b']') => {
            let mut next = at + 2;
            while next < bytes.len() {
                match bytes[next] {
                    0x07 => return next + 1,
                    0x1b if bytes.get(next + 1) == Some(&b'\\') => return next + 2,
                    _ => next += 1,
                }
            }
            bytes.len()
        }
        Some(_) => at + 1 + text[at + 1..].chars().next().map_or(0, char::len_utf8),
        None => bytes.len(),
    }
}

fn num(out: &mut String, mut n: u64) {
    let mut digits = [0u8; 20];
    let mut at = digits.len();
    loop {
        at -= 1;
        digits[at] = b'0' + (n % 10) as u8;
        n /= 10;
        if n == 0 {
            break;
        }
    }
    out.extend(digits[at..].iter().map(|&it| char::from(it)));
}

fn boolean(out: &mut String, value: bool) {
    out.push_str(if value { "true" } else { "false" });
}

fn opt(out: &mut String, text: Option<&str>) {
    match text {
        Some(text) => quote(out, text),
        None => out.push_str("null"),
    }
}

fn strs<'s>(out: &mut String, texts: impl IntoIterator<Item = &'s str>) {
    out.push('[');
    for (at, text) in texts.into_iter().enumerate() {
        if at > 0 {
            out.push(',');
        }
        quote(out, text);
    }
    out.push(']');
}

/// The fields of a target, owned or read in place, as [`target_with`] writes them.
struct TargetFields<'s, K, C> {
    kinds: K,
    crate_types: C,
    name: &'s str,
    src_path: &'s str,
    edition: &'s str,
    doc: bool,
    doctest: bool,
    test: bool,
}

/// A target as cargo's `SerializedTarget` writes it. `required-features` is written only for a
/// target that has some, which [`UnitTarget`] does not say.
pub fn target(out: &mut String, target: &UnitTarget) {
    target_with(out, TargetFields {
        kinds: target.kinds(),
        crate_types: target.crate_types.iter().copied(),
        name: &target.name,
        src_path: &target.src_path,
        edition: &target.edition,
        doc: target.doc,
        doctest: target.doctest,
        test: target.test,
    });
}

/// [`target`] of one read in place.
pub fn target_ref(out: &mut String, target: UnitTargetRef<'_>) {
    target_with(out, TargetFields {
        kinds: target.kinds(),
        crate_types: target.crate_types.iter(),
        name: target.name,
        src_path: target.src_path,
        edition: target.edition,
        doc: target.doc,
        doctest: target.doctest,
        test: target.test,
    });
}

fn target_with<'s>(
    out: &mut String,
    target: TargetFields<'s, impl Iterator<Item = &'static str>, impl Iterator<Item = CrateType>>,
) {
    out.push_str(r#"{"kind":"#);
    strs(out, target.kinds);
    out.push_str(r#","crate_types":"#);
    strs(out, target.crate_types.map(CrateType::name));
    out.push_str(r#","name":"#);
    quote(out, target.name);
    out.push_str(r#","src_path":"#);
    quote(out, target.src_path);
    out.push_str(r#","edition":"#);
    quote(out, target.edition);
    out.push_str(r#","doc":"#);
    boolean(out, target.doc);
    out.push_str(r#","doctest":"#);
    boolean(out, target.doctest);
    out.push_str(r#","test":"#);
    boolean(out, target.test);
    out.push('}');
}

/// `compiler-artifact`, without its newline.
pub fn artifact(out: &mut String, artifact: &Artifact) {
    let profile = &artifact.profile;
    out.push_str(r#"{"reason":"compiler-artifact","package_id":"#);
    quote(out, &artifact.package);
    out.push_str(r#","manifest_path":"#);
    quote(out, &artifact.manifest_path);
    out.push_str(r#","target":"#);
    target(out, &artifact.target);
    out.push_str(r#","profile":{"opt_level":"#);
    quote(out, &profile.opt_level);
    out.push_str(r#","debuginfo":"#);
    num(out, u64::from(profile.debuginfo));
    out.push_str(r#","debug_assertions":"#);
    boolean(out, profile.debug_assertions);
    out.push_str(r#","overflow_checks":"#);
    boolean(out, profile.overflow_checks);
    out.push_str(r#","test":"#);
    boolean(out, profile.test);
    out.push_str(r#"},"features":"#);
    strs(out, artifact.features.iter().map(String::as_str));
    out.push_str(r#","filenames":"#);
    strs(out, artifact.filenames.iter().map(String::as_str));
    out.push_str(r#","executable":"#);
    opt(out, artifact.executable.as_deref());
    out.push_str(r#","fresh":"#);
    boolean(out, artifact.fresh);
    out.push('}');
}

/// `build-script-executed`, without its newline.
pub fn build_script(out: &mut String, ran: BuildScriptRanRef<'_>) {
    out.push_str(r#"{"reason":"build-script-executed","package_id":"#);
    quote(out, ran.package_id);
    out.push_str(r#","linked_libs":"#);
    strs(out, ran.linked_libs);
    out.push_str(r#","linked_paths":"#);
    strs(out, ran.linked_paths);
    out.push_str(r#","cfgs":"#);
    strs(out, ran.cfgs);
    out.push_str(r#","env":["#);
    for (at, (name, value)) in ran.env.iter().enumerate() {
        if at > 0 {
            out.push(',');
        }
        out.push('[');
        quote(out, name);
        out.push(',');
        quote(out, value);
        out.push(']');
    }
    out.push_str(r#"],"out_dir":"#);
    quote(out, ran.out_dir);
    out.push('}');
}

/// `build-finished`, without its newline.
pub fn build_finished(out: &mut String, success: bool) {
    out.push_str(r#"{"reason":"build-finished","success":"#);
    boolean(out, success);
    out.push('}');
}

/// `compiler-message` of `unit`, without its newline. With `ansi`
/// (`json-diagnostic-rendered-ansi`) rustc's message passes as rustc wrote it; without, cargo
/// takes the colors out of `rendered` and writes the message again through
/// `CompilerMessage { rendered, #[serde(flatten)] other: BTreeMap, code: Option<DiagnosticCode> }`:
/// `rendered` first, then every other field sorted by key (at every depth, as `serde_json::Value`
/// maps are), then the outermost `code` last.
pub fn compiler_message(out: &mut String, unit: UnitOfRef<'_>, diagnostic: DiagnosticRef<'_>, ansi: bool, sources: &mut Sources) {
    let dir = unit.manifest_path.rsplit_once(['/', '\\']).map_or("", |(dir, _)| dir);
    out.push_str(r#"{"reason":"compiler-message","package_id":"#);
    quote(out, unit.package_id);
    out.push_str(r#","manifest_path":"#);
    quote(out, unit.manifest_path);
    out.push_str(r#","target":"#);
    target_ref(out, unit.target);
    out.push_str(r#","message":"#);
    let order = if ansi { Order::Rustc } else { Order::Sorted };
    diagnostic_json(out, diagnostic, true, order, sources, dir);
    out.push('}');
}

/// The field order a diagnostic is written in.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Order {
    /// rustc's, as its `Serialize` impls declare the fields.
    Rustc,
    /// serde_json's `Map` without `preserve_order`: by key.
    Sorted,
}

/// rustc's `DiagnosticCode`: the code and its long explanation (`Diagnostic::explanation`, rustc's
/// markdown), `null` without one.
fn code(out: &mut String, diagnostic: DiagnosticRef<'_>) {
    match diagnostic.code {
        Some(code) => {
            out.push_str(r#"{"code":"#);
            quote(out, code);
            out.push_str(r#","explanation":"#);
            opt(out, diagnostic.explanation);
            out.push('}');
        }
        None => out.push_str("null"),
    }
}

/// rustc's `Diagnostic`: `$message_type` on the outermost only, `rendered` `null` on children.
fn diagnostic_json(out: &mut String, diagnostic: DiagnosticRef<'_>, outermost: bool, order: Order, sources: &mut Sources, dir: &str) {
    let spans = |out: &mut String, sources: &mut Sources| {
        out.push('[');
        for (at, span) in diagnostic.spans.iter().enumerate() {
            if at > 0 {
                out.push(',');
            }
            span_json(out, span, order, sources, dir);
        }
        out.push(']');
    };
    let children = |out: &mut String, sources: &mut Sources| {
        out.push('[');
        for (at, child) in diagnostic.children.iter().enumerate() {
            if at > 0 {
                out.push(',');
            }
            diagnostic_json(out, child, false, order, sources, dir);
        }
        out.push(']');
    };
    match (order, outermost) {
        (Order::Rustc, _) => {
            out.push('{');
            if outermost {
                out.push_str(r#""$message_type":"diagnostic","#);
            }
            out.push_str(r#""message":"#);
            quote(out, diagnostic.message);
            out.push_str(r#","code":"#);
            code(out, diagnostic);
            out.push_str(r#","level":""#);
            out.push_str(diagnostic.level.name());
            out.push_str(r#"","spans":"#);
            spans(out, sources);
            out.push_str(r#","children":"#);
            children(out, sources);
            out.push_str(r#","rendered":"#);
            match outermost {
                true => quote(out, diagnostic.rendered),
                false => out.push_str("null"),
            }
            out.push('}');
        }
        (Order::Sorted, true) => {
            out.push_str(r#"{"rendered":"#);
            quote_with(out, diagnostic.rendered, true);
            out.push_str(r#","$message_type":"diagnostic","children":"#);
            children(out, sources);
            out.push_str(r#","level":""#);
            out.push_str(diagnostic.level.name());
            out.push_str(r#"","message":"#);
            quote(out, diagnostic.message);
            out.push_str(r#","spans":"#);
            spans(out, sources);
            out.push_str(r#","code":"#);
            code(out, diagnostic);
            out.push('}');
        }
        (Order::Sorted, false) => {
            out.push_str(r#"{"children":"#);
            children(out, sources);
            out.push_str(r#","code":"#);
            code(out, diagnostic);
            out.push_str(r#","level":""#);
            out.push_str(diagnostic.level.name());
            out.push_str(r#"","message":"#);
            quote(out, diagnostic.message);
            out.push_str(r#","rendered":null,"spans":"#);
            spans(out, sources);
            out.push('}');
        }
    }
}

/// rustc's `DiagnosticSpan`. `expansion` is `null`: [`blaze_api::Span`] carries no macro
/// backtrace.
fn span_json(out: &mut String, span: SpanRef<'_>, order: Order, sources: &mut Sources, dir: &str) {
    let file = sources.file(span.file, dir);
    match order {
        Order::Rustc => {
            out.push_str(r#"{"file_name":"#);
            quote(out, span.file);
            out.push_str(r#","byte_start":"#);
            num(out, u64::from(span.byte_start));
            out.push_str(r#","byte_end":"#);
            num(out, u64::from(span.byte_end));
            out.push_str(r#","line_start":"#);
            num(out, u64::from(span.line));
            out.push_str(r#","line_end":"#);
            num(out, u64::from(span.end_line));
            out.push_str(r#","column_start":"#);
            num(out, u64::from(span.column));
            out.push_str(r#","column_end":"#);
            num(out, u64::from(span.end_column));
            out.push_str(r#","is_primary":"#);
            boolean(out, span.primary);
            out.push_str(r#","text":"#);
            span_text(out, span, order, sources, file);
            out.push_str(r#","label":"#);
            opt(out, span.label);
            out.push_str(r#","suggested_replacement":"#);
            opt(out, span.replacement);
            out.push_str(r#","suggestion_applicability":"#);
            opt(out, span.applicability.map(Applicability::name));
            out.push_str(r#","expansion":null}"#);
        }
        Order::Sorted => {
            out.push_str(r#"{"byte_end":"#);
            num(out, u64::from(span.byte_end));
            out.push_str(r#","byte_start":"#);
            num(out, u64::from(span.byte_start));
            out.push_str(r#","column_end":"#);
            num(out, u64::from(span.end_column));
            out.push_str(r#","column_start":"#);
            num(out, u64::from(span.column));
            out.push_str(r#","expansion":null,"file_name":"#);
            quote(out, span.file);
            out.push_str(r#","is_primary":"#);
            boolean(out, span.primary);
            out.push_str(r#","label":"#);
            opt(out, span.label);
            out.push_str(r#","line_end":"#);
            num(out, u64::from(span.end_line));
            out.push_str(r#","line_start":"#);
            num(out, u64::from(span.line));
            out.push_str(r#","suggested_replacement":"#);
            opt(out, span.replacement);
            out.push_str(r#","suggestion_applicability":"#);
            opt(out, span.applicability.map(Applicability::name));
            out.push_str(r#","text":"#);
            span_text(out, span, order, sources, file);
            out.push('}');
        }
    }
}

/// rustc's `DiagnosticSpanLine`s of `span` (`SourceMap::span_to_lines`): each line it covers,
/// highlighted from its start column on the first and to its end column on the last, in
/// 1-based columns of characters. `[]` when the span is outside the file as it is on disk.
fn span_text(out: &mut String, span: SpanRef<'_>, order: Order, sources: &Sources, file: Option<u32>) {
    let (first, last) = (span.line as usize, span.end_line as usize);
    let lines = file.map_or(0, |file| sources.line_count(file));
    out.push('[');
    if let Some(file) = file
        && first != 0
        && last >= first
        && last <= lines
    {
        for number in first..=last {
            let text = sources.line(file, number);
            let start = if number == first { u64::from(span.column) } else { 1 };
            let end = match number == last {
                true => u64::from(span.end_column),
                false => text.chars().count() as u64 + 1,
            };
            if number > first {
                out.push(',');
            }
            match order {
                Order::Rustc => {
                    out.push_str(r#"{"text":"#);
                    quote(out, text);
                    out.push_str(r#","highlight_start":"#);
                    num(out, start);
                    out.push_str(r#","highlight_end":"#);
                    num(out, end);
                }
                Order::Sorted => {
                    out.push_str(r#"{"highlight_end":"#);
                    num(out, end);
                    out.push_str(r#","highlight_start":"#);
                    num(out, start);
                    out.push_str(r#","text":"#);
                    quote(out, text);
                }
            }
            out.push('}');
        }
    }
    out.push(']');
}

/// One source file as on disk, read once, as a range of [`Sources`]' text and line starts.
/// Lines as rustc keeps them: no BOM, no `\r` before a newline.
struct Source {
    /// Its first line's index in `Sources::starts`.
    first: u32,
    /// As many as `split('\n')` has lines.
    lines: u32,
    /// Its text's end in `Sources::text`.
    end: u32,
}

/// The source files spans point into, as on disk, each read once: rustc's JSON carries the
/// text of the lines a span covers. A name is found by the hash of the directory of its
/// unit's manifest and the name as the span has it, with one probe; every file's text goes into
/// one buffer and every line start into another, 4 bytes a line, so that once the buffers have
/// grown a lookup or a read allocates nothing.
pub struct Sources {
    /// Each (manifest directory, name)'s file, by hash; `None` for a name that is no file.
    named: ByHash<Option<u32>>,
    texts: Texts,
}

/// The files read, their text and their line starts.
struct Texts {
    root: PathBuf,
    /// Each file read, by the hash of its path, so that two names of one file read it once.
    at: ByHash<u32>,
    files: Vec<Source>,
    text: String,
    starts: Vec<u32>,
    /// The path being looked for, and the bytes of the file being read.
    path: PathBuf,
    bytes: Vec<u8>,
}

const FILES: usize = 256;
const TEXT: usize = 1 << 20;

impl Sources {
    /// `root` is the checkout's: a relative file name is looked for no higher.
    pub fn new(root: &Path) -> Sources {
        Sources {
            named: ByHash::with_capacity_and_hasher(FILES, BuildHasherDefault::default()),
            texts: Texts {
                // alloc: once a request.
                root: root.to_owned(),
                at: ByHash::with_capacity_and_hasher(FILES, BuildHasherDefault::default()),
                files: Vec::with_capacity(FILES),
                text: String::new(), // alloc: none until the first file is read.
                starts: Vec::new(),  // alloc: none until the first file is read.
                path: PathBuf::with_capacity(1024),
                bytes: Vec::new(), // alloc: none until the first file is read.
            },
        }
    }

    /// The file `name` is, as rustc names it: absolute, or relative to the directory rustc ran
    /// in, which is the workspace root, an ancestor of the unit's manifest directory `dir`: the
    /// nearest one that has it.
    pub fn file(&mut self, name: &str, dir: &str) -> Option<u32> {
        match self.named.entry(FixedState.hash_one((dir, name))) {
            Entry::Occupied(found) => *found.get(),
            Entry::Vacant(slot) => *slot.insert(self.texts.file(Path::new(name), Path::new(dir))),
        }
    }

    pub fn line_count(&self, file: u32) -> usize {
        self.texts.files[file as usize].lines as usize
    }

    /// Line `number` (1-based, at most [`Sources::line_count`]) of `file`.
    pub fn line(&self, file: u32, number: usize) -> &str {
        let texts = &self.texts;
        let source = &texts.files[file as usize];
        let at = source.first as usize + number - 1;
        let start = texts.starts[at] as usize;
        let end = match number < source.lines as usize {
            true => texts.starts[at + 1] as usize - 1,
            false => source.end as usize,
        };
        let line = &texts.text[start..end];
        line.strip_suffix('\r').unwrap_or(line)
    }
}

impl Texts {
    /// The file `file` names from `dir` (as [`Sources::file`]), read the first time: each place
    /// it may be is opened, with no `stat` before.
    fn file(&mut self, file: &Path, dir: &Path) -> Option<u32> {
        self.path.clear();
        if file.is_absolute() {
            self.path.push(file);
            return self.read();
        }
        for it in dir.ancestors() {
            if !it.starts_with(&self.root) {
                break;
            }
            self.path.clear();
            self.path.push(it);
            self.path.push(file);
            if let Some(at) = self.read() {
                return Some(at);
            }
        }
        None
    }

    /// The file at `path`, read the first time; `None` for no file there.
    fn read(&mut self) -> Option<u32> {
        let Texts { at, files, text, starts, path, bytes, .. } = self;
        let slot = match at.entry(FixedState.hash_one(path.as_os_str().as_encoded_bytes())) {
            Entry::Occupied(found) => return Some(*found.get()),
            Entry::Vacant(slot) => slot,
        };
        // Grown at the first file, so that a build without a span into one maps no buffer.
        if text.capacity() == 0 {
            text.reserve(TEXT);
            starts.reserve(TEXT / 32);
            bytes.reserve(TEXT / 4);
        }
        bytes.clear();
        // A directory opens and fails to read: no file there either.
        std::fs::File::open(&*path).and_then(|mut file| file.read_to_end(bytes)).ok()?;
        let begin = text.len();
        match std::str::from_utf8(bytes) {
            Ok(it) => text.push_str(it),
            // alloc: a file that is not UTF-8, as rustc reads it.
            Err(_) => text.push_str(&String::from_utf8_lossy(bytes)),
        }
        let first = starts.len();
        let start = match text[begin..].starts_with('\u{feff}') {
            true => begin + '\u{feff}'.len_utf8(),
            false => begin,
        };
        starts.push(start as u32);
        starts.extend(text[start..].match_indices('\n').map(|(at, _)| (start + at + 1) as u32));
        let id = files.len() as u32;
        let lines = (starts.len() - first) as u32;
        files.push(Source { first: first as u32, lines, end: text.len() as u32 });
        Some(*slot.insert(id))
    }
}

#[cfg(test)]
mod tests;
