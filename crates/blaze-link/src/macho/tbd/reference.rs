//! The reader as it was before the lexer: a tree of `String`s and `Vec`s per node, kept as
//! the model the lexer is checked against.

#![forbid(unsafe_code)]

use blaze_codegen::Arch;
use blaze_hash::HashSet;

use super::{PLATFORMS, arch_name, arch_rank, platform_name, v3_platforms};
use crate::model::{Shared, SymbolKind};

/// A symbol as the reader held it before names moved into the arena: the form the
/// reference model gives and the tests compare in.
#[derive(PartialEq, Eq, Debug)]
pub(super) struct Owned {
    pub name: String,
    pub version: Option<String>,
    pub kind: SymbolKind,
    pub weak: bool,
}

/// `shared`'s symbols in the reference's form; every name must be in its arena.
pub(super) fn owned(shared: &Shared) -> Vec<Owned> {
    shared
        .symbols
        .iter()
        .map(|it| Owned {
            name: shared.symbol_name(it).to_owned(),
            version: shared.version(it).map(str::to_owned),
            kind: it.kind,
            weak: it.weak,
        })
        .collect()
}

/// A library as the reference reads it: every name its own `String`.
pub(super) struct Library {
    pub name: String,
    pub symbols: Vec<Owned>,
    pub inputs: Vec<String>,
}

pub(super) fn read(name: &str, text: &str, arch: Arch, platform: u32) -> Result<Library, String> {
    read_documents(text, arch, platform).map_err(|what| format!("{name}: {what}"))
}

fn read_documents(text: &str, arch: Arch, platform: u32) -> Result<Library, String> {
    if matches!(arch, Arch::Wasm32) {
        return Err("a `.tbd` describes a Mach-O library; this output is wasm32".to_owned());
    }
    if text.trim_start().starts_with('{') {
        return Err("a JSON `.tbd` (version 5) is not read: only versions 1 to 4".to_owned());
    }
    let raw = documents(text)?;
    if raw.is_empty() {
        return Err("no document".to_owned());
    }
    let mut docs = Vec::with_capacity(raw.len());
    for (tag, lines) in &raw {
        docs.push(Doc::new(tag.as_deref(), lines)?);
    }
    let mut out = Exports::default();
    let mut seen = HashSet::default();
    seen.insert(docs[0].install_name.clone());
    fold(&docs, 0, arch, platform, &mut seen, &mut out)?;
    Ok(Library { name: docs[0].install_name.clone(), symbols: out.symbols, inputs: out.inputs })
}

fn fold(
    docs: &[Doc],
    at: usize,
    arch: Arch,
    platform: u32,
    seen: &mut HashSet<String>,
    out: &mut Exports,
) -> Result<(), String> {
    let doc = &docs[at];
    let chosen = doc.choose(arch, platform)?;
    let reexported = doc.exports(&chosen, platform, out)?;
    for library in reexported {
        if !seen.insert(library.clone()) {
            continue;
        }
        match docs.iter().position(|it| it.install_name == library) {
            Some(index) => fold(docs, index, arch, platform, seen, out)?,
            None => out.inputs.push(library),
        }
    }
    Ok(())
}

#[derive(Default)]
struct Exports {
    symbols: Vec<Owned>,
    names: HashSet<String>,
    inputs: Vec<String>,
}

impl Exports {
    fn add(&mut self, raw: &str, kind: SymbolKind, weak: bool) -> Result<(), String> {
        let name = crate::macho::core_name(raw.as_bytes())?;
        if self.names.insert(name.clone()) {
            self.symbols.push(Owned { name, version: None, kind, weak });
        }
        Ok(())
    }
}

struct Target {
    arch: String,
    platform: u32,
}

struct Doc {
    version: u8,
    install_name: String,
    listed: Vec<String>,
    targets: Vec<Target>,
    map: Vec<(String, Value)>,
}

impl Doc {
    fn new(tag: Option<&str>, lines: &[(usize, &str)]) -> Result<Doc, String> {
        let first = lines.first().map_or(0, |it| it.0);
        let map = match parse_block(lines)? {
            Value::Map(map) => map,
            Value::Scalar(it) if it.is_empty() => Vec::new(),
            _ => return Err(format!("line {first}: a document must be a mapping")),
        };
        let version = match tag {
            None => 1,
            Some("!tapi-tbd-v2") => 2,
            Some("!tapi-tbd-v3") => 3,
            Some("!tapi-tbd") => {
                let version = get(&map, "tbd-version")
                    .ok_or_else(|| format!("line {first}: `!tapi-tbd` without `tbd-version`"))?;
                match scalar(version, "tbd-version")? {
                    "4" => 4,
                    other => {
                        return Err(format!(
                            "line {first}: tbd-version {other} is not read: only versions 1 to 4"
                        ));
                    }
                }
            }
            Some(other) => return Err(format!("line {first}: unknown document tag `{other}`")),
        };
        let install_name = get(&map, "install-name")
            .ok_or_else(|| format!("line {first}: a document without `install-name`"))
            .and_then(|it| scalar(it, "install-name"))?
            .to_owned();
        let mut listed = Vec::new();
        let mut targets = Vec::new();
        if version == 4 {
            let list = get(&map, "targets").ok_or_else(|| format!("`{install_name}` has no `targets`"))?;
            for target in scalars(list, "targets")? {
                targets.push(v4_target(target)?);
                listed.push(target.to_owned());
            }
        } else {
            let archs = get(&map, "archs").ok_or_else(|| format!("`{install_name}` has no `archs`"))?;
            let platform = get(&map, "platform")
                .ok_or_else(|| format!("`{install_name}` has no `platform`"))
                .and_then(|it| scalar(it, "platform"))?;
            for arch in scalars(archs, "archs")? {
                for &number in v3_platforms(platform, arch) {
                    targets.push(Target { arch: arch.to_owned(), platform: number });
                }
                listed.push(format!("{arch} ({platform})"));
            }
        }
        Ok(Doc { version, install_name, listed, targets, map })
    }

    fn choose(&self, arch: Arch, platform: u32) -> Result<String, String> {
        self.targets
            .iter()
            .filter(|it| it.platform == platform)
            .filter_map(|it| arch_rank(arch, &it.arch).map(|rank| (rank, &it.arch)))
            .min_by_key(|(rank, _)| *rank)
            .map(|(_, it)| it.clone())
            .ok_or_else(|| {
                format!(
                    "`{}` has no {}-{}; it has {}",
                    self.install_name,
                    arch_name(arch),
                    platform_name(platform),
                    self.listed.join(", ")
                )
            })
    }

    fn exports(&self, chosen: &str, platform: u32, out: &mut Exports) -> Result<Vec<String>, String> {
        let mut libraries = Vec::new();
        if self.version == 4 {
            for key in ["exports", "reexports"] {
                for section in self.sections(key)? {
                    if self.selects(section, "targets", chosen, platform)? {
                        self.section_exports(key, section, &mut libraries, out)?;
                    }
                }
            }
            for section in self.sections("reexported-libraries")? {
                if self.selects(section, "targets", chosen, platform)? {
                    for (key, value) in section {
                        match key.as_str() {
                            "targets" => {}
                            "libraries" => libraries
                                .extend(scalars(value, "libraries")?.into_iter().map(str::to_owned)),
                            other => return Err(self.unknown("reexported-libraries", other)),
                        }
                    }
                }
            }
        } else {
            for section in self.sections("exports")? {
                if self.selects(section, "archs", chosen, platform)? {
                    self.section_exports("exports", section, &mut libraries, out)?;
                }
            }
        }
        Ok(libraries)
    }

    fn section_exports(
        &self,
        list: &str,
        section: &[(String, Value)],
        libraries: &mut Vec<String>,
        out: &mut Exports,
    ) -> Result<(), String> {
        let v4 = self.version == 4;
        let objc = |name: &str| -> String {
            match self.version {
                1 | 2 => name.strip_prefix('_').unwrap_or(name).to_owned(),
                _ => name.to_owned(),
            }
        };
        for (key, value) in section {
            match key.as_str() {
                "targets" if v4 => {}
                "archs" if !v4 => {}
                "allowable-clients" | "allowed-clients" => {}
                "re-exports" if !v4 => {
                    libraries.extend(scalars(value, key)?.into_iter().map(str::to_owned));
                }
                "symbols" => {
                    for name in scalars(value, key)? {
                        out.add(name, SymbolKind::Other, false)?;
                    }
                }
                "weak-symbols" if v4 => {
                    for name in scalars(value, key)? {
                        out.add(name, SymbolKind::Other, true)?;
                    }
                }
                "weak-def-symbols" if !v4 => {
                    for name in scalars(value, key)? {
                        out.add(name, SymbolKind::Other, true)?;
                    }
                }
                "thread-local-symbols" => {
                    for name in scalars(value, key)? {
                        out.add(name, SymbolKind::Tls, false)?;
                    }
                }
                "objc-classes" => {
                    for name in scalars(value, key)? {
                        let name = objc(name);
                        out.add(&format!("_OBJC_CLASS_$_{name}"), SymbolKind::Other, false)?;
                        out.add(&format!("_OBJC_METACLASS_$_{name}"), SymbolKind::Other, false)?;
                    }
                }
                "objc-eh-types" => {
                    for name in scalars(value, key)? {
                        out.add(&format!("_OBJC_EHTYPE_$_{name}"), SymbolKind::Other, false)?;
                    }
                }
                "objc-ivars" => {
                    for name in scalars(value, key)? {
                        let name = objc(name);
                        out.add(&format!("_OBJC_IVAR_$_{name}"), SymbolKind::Other, false)?;
                    }
                }
                other => return Err(self.unknown(list, other)),
            }
        }
        Ok(())
    }

    fn sections(&self, key: &str) -> Result<Vec<&[(String, Value)]>, String> {
        let Some(value) = get(&self.map, key) else { return Ok(Vec::new()) };
        match value {
            Value::Seq(items) => items
                .iter()
                .map(|item| match item {
                    Value::Map(map) => Ok(map.as_slice()),
                    _ => Err(format!("`{}`: an entry of `{key}` is not a mapping", self.install_name)),
                })
                .collect(),
            Value::Scalar(it) if it.is_empty() => Ok(Vec::new()),
            _ => Err(format!("`{}`: `{key}` is not a sequence", self.install_name)),
        }
    }

    fn selects(
        &self,
        section: &[(String, Value)],
        selector: &str,
        chosen: &str,
        platform: u32,
    ) -> Result<bool, String> {
        let value = get(section, selector)
            .ok_or_else(|| format!("`{}`: a list without `{selector}`", self.install_name))?;
        let names = scalars(value, selector)?;
        if self.version == 4 {
            for name in names {
                let target = v4_target(name)?;
                if target.arch == chosen && target.platform == platform {
                    return Ok(true);
                }
            }
            Ok(false)
        } else {
            Ok(names.contains(&chosen))
        }
    }

    fn unknown(&self, list: &str, key: &str) -> String {
        format!("`{}`: unknown key `{key}` in `{list}`", self.install_name)
    }
}

fn v4_target(text: &str) -> Result<Target, String> {
    let (arch, platform) =
        text.split_once('-').ok_or_else(|| format!("target `{text}` is not `<arch>-<platform>`"))?;
    let platform = PLATFORMS.iter().find(|it| it.0 == platform).map_or(0, |it| it.1);
    Ok(Target { arch: arch.to_owned(), platform })
}

#[derive(PartialEq, Debug)]
pub(super) enum Value {
    Scalar(String),
    Seq(Vec<Value>),
    Map(Vec<(String, Value)>),
}

fn get<'v>(map: &'v [(String, Value)], key: &str) -> Option<&'v Value> {
    map.iter().find(|it| it.0 == key).map(|it| &it.1)
}

fn scalar<'v>(value: &'v Value, key: &str) -> Result<&'v str, String> {
    match value {
        Value::Scalar(it) => Ok(it),
        _ => Err(format!("`{key}` is not a scalar")),
    }
}

fn scalars<'v>(value: &'v Value, key: &str) -> Result<Vec<&'v str>, String> {
    match value {
        Value::Scalar(it) if it.is_empty() => Ok(Vec::new()),
        Value::Scalar(it) => Ok(vec![it.as_str()]),
        Value::Seq(items) => items.iter().map(|it| scalar(it, key)).collect(),
        Value::Map(_) => Err(format!("`{key}` is not a sequence")),
    }
}

pub(super) fn documents(text: &str) -> Result<Vec<(Option<String>, Vec<(usize, &str)>)>, String> {
    let mut docs = Vec::new();
    let mut open: Option<(Option<String>, Vec<(usize, &str)>)> = None;
    for (index, line) in text.lines().enumerate() {
        let number = index + 1;
        if line == "---" || line.starts_with("--- ") || line.starts_with("---\t") {
            if let Some(doc) = open.take() {
                docs.push(doc);
            }
            let tag = strip_comment(&line[3..])?.trim().to_owned();
            open = Some(((!tag.is_empty()).then_some(tag), Vec::new()));
        } else if line.trim_end() == "..." {
            if let Some(doc) = open.take() {
                docs.push(doc);
            }
        } else if let Some((_, lines)) = &mut open {
            lines.push((number, line));
        } else if !line.starts_with('%') && !strip_comment(line)?.trim().is_empty() {
            open = Some((None, vec![(number, line)]));
        }
    }
    if let Some(doc) = open {
        docs.push(doc);
    }
    Ok(docs)
}

#[derive(Clone, Debug)] // clone: the test and fuzz reference parser only, once per block
struct Line {
    number: usize,
    indent: usize,
    text: String,
}

fn logical_lines(lines: &[(usize, &str)]) -> Result<Vec<Line>, String> {
    let mut out: Vec<Line> = Vec::new();
    let mut depth = 0i32;
    for &(number, raw) in lines {
        let text = strip_comment(raw).map_err(|what| format!("line {number}: {what}"))?;
        let trimmed = text.trim();
        if trimmed.is_empty() {
            continue;
        }
        let delta = flow_depth(trimmed);
        if depth > 0 {
            let last = out.last_mut().expect("an open flow collection has a line");
            last.text.push(' ');
            last.text.push_str(trimmed);
        } else {
            let indent = text.len() - text.trim_start_matches(' ').len();
            if text[indent..].starts_with('\t') {
                return Err(format!("line {number}: a tab in the indentation"));
            }
            out.push(Line { number, indent, text: trimmed.to_owned() });
        }
        depth += delta;
        if depth < 0 {
            return Err(format!("line {number}: an unopened `]` or `}}`"));
        }
    }
    if depth > 0 {
        let number = out.last().map_or(0, |it| it.number);
        return Err(format!("line {number}: an unclosed `[` or `{{`"));
    }
    Ok(out)
}

fn opens_quote(text: &str, at: usize) -> bool {
    text[..at].chars().next_back().is_none_or(|it| it.is_whitespace() || "[{,:-".contains(it))
}

fn strip_comment(text: &str) -> Result<&str, String> {
    let mut quote = None;
    let mut chars = text.char_indices().peekable();
    while let Some((at, c)) = chars.next() {
        match quote {
            Some('\'') => {
                if c == '\'' {
                    if chars.peek().is_some_and(|it| it.1 == '\'') {
                        chars.next();
                    } else {
                        quote = None;
                    }
                }
            }
            Some(_) => match c {
                '\\' => {
                    chars.next();
                }
                '"' => quote = None,
                _ => {}
            },
            None => match c {
                '\'' | '"' if opens_quote(text, at) => quote = Some(c),
                '#' if text[..at].chars().next_back().is_none_or(char::is_whitespace) => {
                    return Ok(&text[..at]);
                }
                _ => {}
            },
        }
    }
    if quote.is_some() {
        return Err("an unterminated quoted scalar".to_owned());
    }
    Ok(text)
}

fn flow_depth(text: &str) -> i32 {
    let mut depth = 0;
    let mut quote = None;
    let mut chars = text.char_indices().peekable();
    while let Some((at, c)) = chars.next() {
        match quote {
            Some('\'') => {
                if c == '\'' {
                    if chars.peek().is_some_and(|it| it.1 == '\'') {
                        chars.next();
                    } else {
                        quote = None;
                    }
                }
            }
            Some(_) => match c {
                '\\' => {
                    chars.next();
                }
                '"' => quote = None,
                _ => {}
            },
            None => match c {
                '\'' | '"' if opens_quote(text, at) => quote = Some(c),
                '[' | '{' => depth += 1,
                ']' | '}' => depth -= 1,
                _ => {}
            },
        }
    }
    depth
}

pub(super) fn parse_block(lines: &[(usize, &str)]) -> Result<Value, String> {
    let lines = logical_lines(lines)?;
    if lines.is_empty() {
        return Ok(Value::Scalar(String::new()));
    }
    let mut parser = Block { lines, at: 0 };
    let value = parser.block()?;
    if let Some(line) = parser.lines.get(parser.at) {
        return Err(format!("line {}: unexpected indentation", line.number));
    }
    Ok(value)
}

struct Block {
    lines: Vec<Line>,
    at: usize,
}

fn is_item(text: &str) -> bool {
    text == "-" || text.starts_with("- ")
}

impl Block {
    fn block(&mut self) -> Result<Value, String> {
        let line = self.lines[self.at].clone();
        if is_item(&line.text) {
            self.sequence(line.indent)
        } else if split_key(&line.text, line.number)?.is_some() {
            self.mapping(line.indent)
        } else {
            self.at += 1;
            flow(&line.text, line.number)
        }
    }

    fn sequence(&mut self, indent: usize) -> Result<Value, String> {
        let mut items = Vec::new();
        while let Some(line) = self.lines.get(self.at) {
            if line.indent != indent || !is_item(&line.text) {
                break;
            }
            let rest = line.text[1..].trim_start().to_owned();
            if rest.is_empty() {
                self.at += 1;
                items.push(self.nested(indent)?);
            } else {
                let line = &mut self.lines[self.at];
                line.indent += line.text.len() - rest.len();
                line.text = rest;
                items.push(self.block()?);
            }
        }
        Ok(Value::Seq(items))
    }

    fn mapping(&mut self, indent: usize) -> Result<Value, String> {
        let mut entries = Vec::new();
        while let Some(line) = self.lines.get(self.at) {
            if line.indent != indent || is_item(&line.text) {
                break;
            }
            let number = line.number;
            let (key, value) = split_key(&line.text, number)?
                .ok_or_else(|| format!("line {number}: expected `key: value`"))?;
            let value = value.to_owned();
            self.at += 1;
            let value = if value.is_empty() {
                match self.lines.get(self.at) {
                    Some(next) if next.indent == indent && is_item(&next.text) => self.sequence(indent)?,
                    _ => self.nested(indent)?,
                }
            } else {
                flow(&value, number)?
            };
            if entries.iter().any(|(it, _): &(String, Value)| *it == key) {
                return Err(format!("line {number}: `{key}` given twice"));
            }
            entries.push((key, value));
        }
        Ok(Value::Map(entries))
    }

    fn nested(&mut self, indent: usize) -> Result<Value, String> {
        match self.lines.get(self.at) {
            Some(next) if next.indent > indent => self.block(),
            _ => Ok(Value::Scalar(String::new())),
        }
    }
}

fn split_key(text: &str, number: usize) -> Result<Option<(String, &str)>, String> {
    if text.starts_with(['[', '{']) {
        return Ok(None);
    }
    let (key, rest) = if text.starts_with(['\'', '"']) {
        let mut parser = Flow { text, at: 0, number };
        let key = parser.quoted()?;
        (key, text[parser.at..].trim_start())
    } else {
        let bytes = text.as_bytes();
        let Some(colon) = (0..bytes.len())
            .find(|&at| bytes[at] == b':' && bytes.get(at + 1).is_none_or(u8::is_ascii_whitespace))
        else {
            return Ok(None);
        };
        (text[..colon].trim_end().to_owned(), &text[colon..])
    };
    match rest.strip_prefix(':') {
        Some(value) if value.is_empty() || value.starts_with(char::is_whitespace) => {
            Ok(Some((key, value.trim())))
        }
        _ => Ok(None),
    }
}

fn flow(text: &str, number: usize) -> Result<Value, String> {
    let mut parser = Flow { text, at: 0, number };
    let value = parser.value(false)?;
    parser.skip_spaces();
    if parser.at != text.len() {
        return Err(format!("line {number}: unexpected `{}`", &text[parser.at..]));
    }
    Ok(value)
}

struct Flow<'a> {
    text: &'a str,
    at: usize,
    number: usize,
}

impl Flow<'_> {
    fn peek(&self) -> Option<char> {
        self.text[self.at..].chars().next()
    }

    fn skip_spaces(&mut self) {
        while self.peek().is_some_and(char::is_whitespace) {
            self.at += 1;
        }
    }

    fn error(&self, what: &str) -> String {
        format!("line {}: {what} at `{}`", self.number, &self.text[self.at..])
    }

    fn value(&mut self, in_flow: bool) -> Result<Value, String> {
        self.skip_spaces();
        match self.peek() {
            Some('[') => {
                self.at += 1;
                let mut items = Vec::new();
                loop {
                    self.skip_spaces();
                    if self.peek() == Some(']') {
                        self.at += 1;
                        break;
                    }
                    items.push(self.value(true)?);
                    self.skip_spaces();
                    match self.peek() {
                        Some(',') => self.at += 1,
                        Some(']') => {
                            self.at += 1;
                            break;
                        }
                        _ => return Err(self.error("expected `,` or `]`")),
                    }
                }
                Ok(Value::Seq(items))
            }
            Some('{') => {
                self.at += 1;
                let mut entries = Vec::new();
                loop {
                    self.skip_spaces();
                    if self.peek() == Some('}') {
                        self.at += 1;
                        break;
                    }
                    let key = match self.peek() {
                        Some('\'' | '"') => self.quoted()?,
                        _ => self.plain(true, true),
                    };
                    self.skip_spaces();
                    if self.peek() != Some(':') {
                        return Err(self.error("expected `:`"));
                    }
                    self.at += 1;
                    self.skip_spaces();
                    let value = match self.peek() {
                        Some(',' | '}') => Value::Scalar(String::new()),
                        _ => self.value(true)?,
                    };
                    entries.push((key, value));
                    self.skip_spaces();
                    match self.peek() {
                        Some(',') => self.at += 1,
                        Some('}') => {
                            self.at += 1;
                            break;
                        }
                        _ => return Err(self.error("expected `,` or `}`")),
                    }
                }
                Ok(Value::Map(entries))
            }
            Some('\'' | '"') => Ok(Value::Scalar(self.quoted()?)),
            _ => Ok(Value::Scalar(self.plain(in_flow, false))),
        }
    }

    fn plain(&mut self, in_flow: bool, key: bool) -> String {
        let start = self.at;
        while let Some(c) = self.peek() {
            if (in_flow && matches!(c, ',' | ']' | '}')) || (key && c == ':') {
                break;
            }
            self.at += c.len_utf8();
        }
        self.text[start..self.at].trim().to_owned()
    }

    fn quoted(&mut self) -> Result<String, String> {
        let quote = self.peek().expect("a quote");
        self.at += 1;
        let mut out = String::new();
        loop {
            let Some(c) = self.peek() else {
                return Err(self.error("an unterminated quoted scalar"));
            };
            self.at += c.len_utf8();
            match (quote, c) {
                ('\'', '\'') => {
                    if self.peek() == Some('\'') {
                        self.at += 1;
                        out.push('\'');
                    } else {
                        return Ok(out);
                    }
                }
                ('"', '"') => return Ok(out),
                ('"', '\\') => out.push(self.escape()?),
                _ => out.push(c),
            }
        }
    }

    fn escape(&mut self) -> Result<char, String> {
        let Some(c) = self.peek() else { return Err(self.error("an unterminated escape")) };
        self.at += c.len_utf8();
        let digits = match c {
            '0' => return Ok('\0'),
            't' => return Ok('\t'),
            'n' => return Ok('\n'),
            'r' => return Ok('\r'),
            '"' | '\\' | '/' | ' ' => return Ok(c),
            'x' => 2,
            'u' => 4,
            'U' => 8,
            _ => return Err(self.error("an unknown escape")),
        };
        let hex =
            self.text.get(self.at..self.at + digits).ok_or_else(|| self.error("a short escape"))?;
        let code = u32::from_str_radix(hex, 16).map_err(|_| self.error("a bad escape"))?;
        self.at += digits;
        char::from_u32(code).ok_or_else(|| self.error("an escape of no character"))
    }
}
