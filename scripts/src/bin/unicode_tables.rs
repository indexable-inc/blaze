//! unicode_tables: writes crates/blaze-unicode/src/tables.rs, the Unicode properties blaze reads,
//! from the Unicode Character Database. `DOC` below says what it reads and writes.
//!
//!     blaze run -q -p scripts --bin unicode_tables -- '{"ucd": "/tmp/UCD", "version": "17.0.0"}'
//!     blaze run -q -p scripts --bin unicode_tables -- selftest

use std::{collections::BTreeMap, fmt, fs, path::Path};

use blaze_script::{
    fail,
    input::{self, Fields, Input, InputError},
};

const SCRIPT: &str = "unicode_tables";

const DOC: &str = r#"unicode_tables: crates/blaze-unicode/src/tables.rs from the Unicode Character Database.

Input: {"ucd": <dir>, "version": "<major>.<minor>.<update>", "out": <file>}. `ucd` is UCD.zip of
that version unzipped (https://www.unicode.org/Public/<version>/ucd/UCD.zip, under the Unicode
License v3, https://www.unicode.org/license.txt); `out` is crates/blaze-unicode/src/tables.rs
unless given. It reads UnicodeData.txt, DerivedCoreProperties.txt, PropList.txt,
EastAsianWidth.txt, HangulSyllableType.txt, extracted/DerivedGeneralCategory.txt,
auxiliary/GraphemeBreakProperty.txt, emoji/emoji-data.txt and emoji/emoji-variation-sequences.txt,
and refuses a file whose header names another version.

The same input writes the same bytes: every table is a sorted list of disjoint ranges, each
range as long as it can be."#;

struct Args {
    ucd: String,
    version: String,
    out: String,
}

impl Input for Args {
    const FIELDS: &'static [&'static str] = &["ucd", "version", "out"];

    fn parse(f: &Fields<'_>) -> Result<Args, InputError> {
        Ok(Args {
            ucd: f.str("ucd", None)?,
            version: f.str("version", None)?,
            out: f.str("out", Some("crates/blaze-unicode/src/tables.rs"))?,
        })
    }
}

/// Why the tables could not be written, naming the file and line.
#[derive(Debug, PartialEq, Eq)]
enum TableError {
    /// A data file could not be read.
    Read { path: String, why: String },
    /// The version is not `<major>.<minor>.<update>`.
    Version { given: String },
    /// A data file's first line does not name the version asked for.
    OtherVersion { path: String, first: String },
    /// A line that is not `<code points> ; <fields>`.
    Line { path: String, line: usize, text: String },
    /// The table could not be written.
    Write { path: String, why: String },
}

impl fmt::Display for TableError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TableError::Read { path, why } => write!(f, "{path}: cannot be read: {why}"),
            TableError::Version { given } => write!(f, "version {given:?} is not <major>.<minor>.<update>"),
            TableError::OtherVersion { path, first } => write!(f, "{path}: its first line {first:?} names another version"),
            TableError::Line { path, line, text } => write!(f, "{path}:{line}: not a data line: {text:?}"),
            TableError::Write { path, why } => write!(f, "{path}: cannot be written: {why}"),
        }
    }
}

const MAX: u32 = 0x11_0000;

fn is_scalar(cp: u32) -> bool {
    cp < MAX && !(0xd800..=0xdfff).contains(&cp)
}

/// One data line: its code points (`0041` or `0041..005A`, or a sequence `0023 FE0F`) and the
/// fields after them, trimmed, the comment taken off.
struct Row {
    points: Vec<u32>,
    last: u32,
    fields: Vec<String>,
}

/// The data lines of `text` (the file at `path`).
fn rows(path: &str, text: &str) -> Result<Vec<Row>, TableError> {
    let mut out = Vec::new();
    for (at, line) in text.lines().enumerate() {
        let data = line.split('#').next().unwrap_or("").trim();
        if data.is_empty() {
            continue;
        }
        let bad = || TableError::Line { path: path.to_owned(), line: at + 1, text: line.to_owned() };
        let mut parts = data.split(';').map(str::trim);
        let points = parts.next().ok_or_else(bad)?;
        let hex = |it: &str| u32::from_str_radix(it, 16).ok().filter(|cp| *cp < MAX);
        let (points, last) = match points.split_once("..") {
            Some((first, last)) => {
                let (first, last) = (hex(first).ok_or_else(bad)?, hex(last).ok_or_else(bad)?);
                (vec![first], last)
            }
            None => {
                let seq: Option<Vec<u32>> = points.split_whitespace().map(hex).collect();
                let seq = seq.filter(|it| !it.is_empty()).ok_or_else(bad)?;
                let last = seq[0];
                (seq, last)
            }
        };
        out.push(Row { points, last, fields: parts.map(str::to_owned).collect() });
    }
    Ok(out)
}

/// The UCD at `dir` of one version: each file read once, its version checked.
struct Ucd<'a> {
    dir: &'a Path,
    version: &'a str,
    major_minor: (u8, u8),
}

impl Ucd<'_> {
    fn text(&self, name: &str) -> Result<String, TableError> {
        let path = self.dir.join(name);
        let shown = path.display().to_string();
        let text = fs::read_to_string(&path).map_err(|e| TableError::Read { path: shown.clone(), why: e.to_string() })?;
        // UnicodeData.txt has no header. The others begin `# Name-<version>.txt`, or (the emoji
        // files) name `Version <major>.<minor>` or `Version: <major>.<minor>`.
        if name != "UnicodeData.txt" {
            let head: String = text.lines().take(12).collect::<Vec<_>>().join("\n");
            let (major, minor) = (self.major_minor.0, self.major_minor.1);
            let named = [format!("-{}.txt", self.version), format!("Version {major}.{minor}"), format!("Version: {major}.{minor}")];
            if !named.iter().any(|it| head.contains(it.as_str())) {
                let first = text.lines().next().unwrap_or("").to_owned();
                return Err(TableError::OtherVersion { path: shown, first });
            }
        }
        Ok(text)
    }

    /// The code points each value of the first field takes in `name`: `value -> points`.
    fn property(&self, name: &str) -> Result<BTreeMap<String, Points>, TableError> {
        let text = self.text(name)?;
        let mut out: BTreeMap<String, Points> = BTreeMap::new();
        for row in rows(name, &text)? {
            let Some(value) = row.fields.first() else { continue };
            out.entry(value.clone()).or_default().add(row.points[0], row.last);
        }
        Ok(out)
    }
}

/// A set of code points, one flag each.
#[derive(Clone)]
struct Points(Vec<bool>);

impl Default for Points {
    fn default() -> Points {
        Points(vec![false; MAX as usize])
    }
}

impl Points {
    fn add(&mut self, first: u32, last: u32) {
        for cp in first..=last {
            self.0[cp as usize] = true;
        }
    }

    fn has(&self, cp: u32) -> bool {
        self.0[cp as usize]
    }
}

fn get<'a>(map: &'a BTreeMap<String, Points>, key: &str, empty: &'a Points) -> &'a Points {
    map.get(key).unwrap_or(empty)
}

/// The ranges of the scalar values `has` holds, each as long as it can be (a range does not
/// cross the surrogates).
fn set_ranges(has: impl Fn(u32) -> bool) -> Vec<(u32, u32)> {
    valued_ranges(|cp| u8::from(has(cp)), 0).into_iter().map(|(a, b, _)| (a, b)).collect()
}

/// The ranges of the scalar values whose value is not `default`.
fn valued_ranges(value: impl Fn(u32) -> u8, default: u8) -> Vec<(u32, u32, u8)> {
    let mut out: Vec<(u32, u32, u8)> = Vec::new();
    for cp in (0..MAX).filter(|it| is_scalar(*it)) {
        let v = value(cp);
        if v == default {
            continue;
        }
        match out.last_mut() {
            Some((_, last, said)) if *said == v && (*last + 1 == cp || (*last == 0xd7ff && cp == 0xe000)) => *last = cp,
            _ => out.push((cp, cp, v)),
        }
    }
    out
}

/// The text of tables.rs, in the format blaze-unicode reads.
struct Emit(String);

impl Emit {
    fn lines(&mut self, entries: &[String], per: usize) {
        for chunk in entries.chunks(per) {
            self.0.push_str(&format!("    {},\n", chunk.join(", ")));
        }
    }

    fn set(&mut self, name: &str, source: &str, ranges: &[(u32, u32)]) {
        let n = ranges.len();
        self.0.push_str(&format!(
            "\n// {source}: {n} ranges, {} bytes.\n#[rustfmt::skip]\npub(crate) static {name}: &[(u32, u32)] = &[\n",
            n * 8
        ));
        self.lines(&ranges.iter().map(|(a, b)| format!("({a:#x}, {b:#x})")).collect::<Vec<_>>(), 8);
        self.0.push_str("];\n");
    }

    fn valued(&mut self, name: &str, source: &str, ranges: &[(u32, u32, u8)]) {
        let n = ranges.len();
        self.0.push_str(&format!(
            "\n// {source}: {n} ranges, {} bytes.\n#[rustfmt::skip]\npub(crate) static {name}: &[(u32, u32, u8)] = &[\n",
            n * 12
        ));
        self.lines(&ranges.iter().map(|(a, b, v)| format!("({a:#x}, {b:#x}, {v})")).collect::<Vec<_>>(), 6);
        self.0.push_str("];\n");
    }
}

/// The version as `(major, minor, update)`.
fn version_of(given: &str) -> Result<(u8, u8, u8), TableError> {
    let bad = || TableError::Version { given: given.to_owned() };
    let parts: Option<Vec<u8>> = given.split('.').map(|it| it.parse().ok()).collect();
    match parts.ok_or_else(bad)?.as_slice() {
        [a, b, c] => Ok((*a, *b, *c)),
        _ => Err(bad()),
    }
}

/// The tables' text from the UCD at `dir` of `version`.
fn tables(dir: &Path, version: &str) -> Result<String, TableError> {
    let (major, minor, update) = version_of(version)?;
    let ucd = Ucd { dir, version, major_minor: (major, minor) };
    let empty = Points::default();
    let core = ucd.property("DerivedCoreProperties.txt")?;
    let general = ucd.property("extracted/DerivedGeneralCategory.txt")?;
    let emoji = ucd.property("emoji/emoji-data.txt")?;
    let wide = ucd.property("EastAsianWidth.txt")?;
    let hangul = ucd.property("HangulSyllableType.txt")?;
    let props = ucd.property("PropList.txt")?;
    let grapheme = ucd.property("auxiliary/GraphemeBreakProperty.txt")?;

    // emoji-variation-sequences.txt: `<base> FE0F ; emoji style;` and `<base> FE0E ; text style;`.
    let name = "emoji/emoji-variation-sequences.txt";
    let (mut emoji_style, mut text_style) = (Points::default(), Points::default());
    for row in rows(name, &ucd.text(name)?)? {
        match row.points.as_slice() {
            [base, 0xfe0f] => emoji_style.add(*base, *base),
            [base, 0xfe0e] => text_style.add(*base, *base),
            _ => {}
        }
    }

    // UnicodeData.txt: field 3 the combining class, field 5 the decomposition (a canonical one
    // has no `<tag>`). A `First>`/`Last>` range has class 0 and no decomposition.
    let name = "UnicodeData.txt";
    let text = ucd.text(name)?;
    let mut class = vec![0u8; MAX as usize];
    let mut canonical: BTreeMap<u32, Vec<u32>> = BTreeMap::new();
    for (at, line) in text.lines().enumerate() {
        let fields: Vec<&str> = line.split(';').collect();
        let bad = || TableError::Line { path: name.to_owned(), line: at + 1, text: line.to_owned() };
        let [code, _, _, ccc, _, decomposition, ..] = fields.as_slice() else { return Err(bad()) };
        let cp = u32::from_str_radix(code, 16).map_err(|_| bad())?;
        class[cp as usize] = ccc.parse().map_err(|_| bad())?;
        if !decomposition.is_empty() && !decomposition.starts_with('<') {
            let parts: Option<Vec<u32>> = decomposition.split_whitespace().map(|it| u32::from_str_radix(it, 16).ok()).collect();
            canonical.insert(cp, parts.ok_or_else(bad)?);
        }
    }
    fn full(cp: u32, canonical: &BTreeMap<u32, Vec<u32>>, out: &mut Vec<u32>) {
        match canonical.get(&cp) {
            Some(parts) => parts.iter().for_each(|it| full(*it, canonical, out)),
            None => out.push(cp),
        }
    }

    let has = |map: &BTreeMap<String, Points>, key: &str, cp: u32| get(map, key, &empty).has(cp);
    let pcm_zero = [0x605, 0x70f, 0x890, 0x891, 0x8e2];
    let regional = |cp: u32| (0x1f1e6..=0x1f1ff).contains(&cp);
    let modifier = |cp: u32| (0x1f3fb..=0x1f3ff).contains(&cp);
    // unicode-width 0.2's rules for one character, outside an East Asian context (its crate
    // documentation, "Rules for determining width", 2.1 to 2.7); 4 is 2 and Emoji_Presentation.
    let width = |cp: u32| -> u8 {
        if cp < 0xa0 {
            return 1;
        }
        if matches!(cp, 0x2d7f | 0xff9e | 0xff9f) {
            return 1;
        }
        if matches!(cp, 0x115f | 0x17a4) {
            return 2;
        }
        if cp == 0x17d8 {
            return 3;
        }
        let pcm = has(&props, "Prepended_Concatenation_Mark", cp);
        if has(&core, "Default_Ignorable_Code_Point", cp)
            || has(&core, "Grapheme_Extend", cp)
            || has(&hangul, "V", cp)
            || has(&hangul, "T", cp)
            || pcm_zero.contains(&cp)
            || (has(&grapheme, "Prepend", cp) && !pcm)
            || cp == 0xa8fa
        {
            return 0;
        }
        if has(&wide, "W", cp) || has(&wide, "F", cp) {
            let presentation = has(&emoji, "Emoji_Presentation", cp) && !regional(cp) && !modifier(cp);
            return if presentation { 4 } else { 2 };
        }
        1
    };
    // Legacy clusters' values; Prepend, SpacingMark and the Hangul syllables (computed) are Any.
    let breaks = [
        ("CR", 1),
        ("LF", 2),
        ("Control", 3),
        ("Extend", 4),
        ("ZWJ", 5),
        ("Regional_Indicator", 6),
        ("L", 7),
        ("V", 8),
        ("T", 9),
        ("LV", 10),
        ("LVT", 11),
    ];
    let grapheme_value = |cp: u32| -> u8 {
        if (0xac00..=0xd7a3).contains(&cp) {
            return 0;
        }
        for (name, value) in breaks {
            if has(&grapheme, name, cp) {
                return value;
            }
        }
        if has(&emoji, "Extended_Pictographic", cp) { 12 } else { 0 }
    };

    let mut e = Emit(format!(
        "//! blaze-unicode's tables; do not edit. Each set is a sorted list of disjoint inclusive ranges\n\
         //! of code points, each valued table a sorted list of disjoint `(first, last, value)` ranges\n\
         //! whose code points outside every range have the default its reader names. Generated by\n\
         //! scripts/src/bin/unicode_tables.rs from the Unicode Character Database {version}\n\
         //! (https://www.unicode.org/Public/{version}/ucd/UCD.zip, the Unicode License v3).\n\
         \n\
         pub(crate) const UNICODE_VERSION: (u8, u8, u8) = ({major}, {minor}, {update});\n"
    ));
    e.set("XID_START", "XID_Start (DerivedCoreProperties.txt)", &set_ranges(|cp| has(&core, "XID_Start", cp)));
    e.set("XID_CONTINUE", "XID_Continue (DerivedCoreProperties.txt)", &set_ranges(|cp| has(&core, "XID_Continue", cp)));
    e.set("EMOJI", "Emoji=Yes (emoji/emoji-data.txt)", &set_ranges(|cp| has(&emoji, "Emoji", cp)));
    e.set("OTHER_PUNCTUATION", "General_Category=Po (extracted/DerivedGeneralCategory.txt)", &set_ranges(|cp| has(&general, "Po", cp)));
    e.set("DECIMAL_NUMBER", "General_Category=Nd (extracted/DerivedGeneralCategory.txt)", &set_ranges(|cp| has(&general, "Nd", cp)));
    e.valued(
        "WIDTH",
        "Display width from U+00A0, default 1; 4 is 2 and Emoji_Presentation (EastAsianWidth.txt and the zero-width properties)",
        &valued_ranges(width, 1),
    );
    e.set("EMOJI_STYLE_BASE", "Bases of an emoji presentation sequence (emoji/emoji-variation-sequences.txt)", &set_ranges(|cp| emoji_style.has(cp)));
    e.set(
        "TEXT_STYLE_BASE",
        "Emoji_Presentation bases of a text presentation sequence, outside U+1F200..U+1F2FF (emoji/emoji-variation-sequences.txt)",
        &set_ranges(|cp| text_style.has(cp) && has(&emoji, "Emoji_Presentation", cp) && !(0x1f200..=0x1f2ff).contains(&cp)),
    );
    e.set("EMOJI_MODIFIER_BASE", "Emoji_Modifier_Base (emoji/emoji-data.txt)", &set_ranges(|cp| has(&emoji, "Emoji_Modifier_Base", cp)));
    e.valued(
        "GRAPHEME_BREAK",
        "Grapheme_Cluster_Break (Hangul syllables computed) and Extended_Pictographic, default Any (auxiliary/GraphemeBreakProperty.txt, emoji/emoji-data.txt)",
        &valued_ranges(grapheme_value, 0),
    );
    e.valued("COMBINING_CLASS", "Canonical_Combining_Class, default 0 (UnicodeData.txt)", &valued_ranges(|cp| class[cp as usize], 0));

    let (mut nfd, mut chars) = (Vec::new(), Vec::new());
    for cp in canonical.keys() {
        let mut parts = Vec::new();
        full(*cp, &canonical, &mut parts);
        nfd.push(format!("({cp:#x}, {}, {})", chars.len(), parts.len()));
        chars.extend(parts.iter().map(|it| format!("{it:#x}")));
    }
    e.0.push_str(&format!(
        "\n// Full canonical decompositions but Hangul's (UnicodeData.txt): {} rows, {} bytes; each is (code point, start, length) into NFD_CHARS.\n#[rustfmt::skip]\npub(crate) static NFD: &[(u32, u16, u16)] = &[\n",
        nfd.len(),
        nfd.len() * 8
    ));
    e.lines(&nfd, 6);
    e.0.push_str("];\n");
    e.0.push_str(&format!(
        "\n// The characters NFD points into: {} code points, {} bytes.\n#[rustfmt::skip]\npub(crate) static NFD_CHARS: &[u32] = &[\n",
        chars.len(),
        chars.len() * 4
    ));
    e.lines(&chars, 12);
    e.0.push_str("];\n");
    Ok(e.0)
}

fn main_with(args: &Args) -> Result<(), TableError> {
    let text = tables(Path::new(&args.ucd), &args.version)?;
    fs::write(&args.out, text).map_err(|e| TableError::Write { path: args.out.clone(), why: e.to_string() })?;
    println!("{SCRIPT}: wrote {} from UCD {}", args.out, args.version);
    Ok(())
}

fn selftest() {
    let check = |ok: bool, what: &str| {
        if !ok {
            fail(SCRIPT, &format!("selftest: {what}"));
        }
    };
    let text = "# Test-17.0.0.txt\n0041..005A ; XID_Start # Lu\n00AA ; XID_Start\n\n0023 FE0F ; emoji style; # x\n";
    let read = rows("t", text).unwrap_or_default();
    check(read.len() == 3, "three data lines");
    check(read[0].points == [0x41] && read[0].last == 0x5a && read[0].fields == ["XID_Start"], "a range");
    check(read[1].points == [0xaa] && read[1].last == 0xaa, "one code point");
    check(read[2].points == [0x23, 0xfe0f], "a sequence");
    check(matches!(rows("t", "zz ; X\n"), Err(TableError::Line { line: 1, .. })), "a bad line is refused naming it");
    check(set_ranges(|cp| (0x41..=0x43).contains(&cp) || cp == 0x45) == [(0x41, 0x43), (0x45, 0x45)], "ranges join");
    check(set_ranges(|cp| (0xd7fe..=0xe001).contains(&cp)) == [(0xd7fe, 0xe001)], "a range runs over the surrogates");
    check(valued_ranges(|cp| if cp < 3 { u8::from(cp == 1) + 1 } else { 0 }, 0) == [(0, 0, 1), (1, 1, 2), (2, 2, 1)], "values split ranges");
    check(version_of("17.0.0") == Ok((17, 0, 0)) && version_of("17.0").is_err() && version_of("x").is_err(), "the version");
    println!("{SCRIPT} selftest: ok");
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.as_slice() {
        [one] if one == "selftest" => selftest(),
        [one] => {
            if let Err(e) = main_with(&input::parse::<Args>(one, SCRIPT)) {
                fail(SCRIPT, &e.to_string());
            }
        }
        _ => {
            eprintln!("usage: {SCRIPT} '<json>' | selftest\n\n{DOC}");
            std::process::exit(1);
        }
    }
}
