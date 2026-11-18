//! A census of every `extern` block's fn and static declarations in the pinned library's `core`,
//! `alloc` and `std`, by where the block stands, so the next stop of the kind "a fn with no body
//! whose `extern` block the names source did not answer" starts from the list of positions the
//! real library writes (rule 82), not from one run's first stop.
//!
//! The library read is the checkout's `library/` (as tests/intrinsics_of_core.rs locates it). The
//! scan is plain text, line by line, with each `{` classed by the line that opens it:
//! - MACRO: a `macro_rules!` or `macro` definition: a block inside it is written by the
//!   definition's expansions wherever they are called (std's `thread_local_inner!`);
//! - EXTERN: an `extern "abi" {` or `extern {` block (with `unsafe`, `pub` and attributes
//!   before it): the census's subject;
//! - MODULE: a `mod name {`, or a macro CALL's braces (`cfg_select! {`, `cfg_if! {`) and the `=>
//!   {` arms inside one, which stand where the call stands;
//! - BODY: every other brace (a fn's or closure's body, a block, a `const`'s or `static`'s
//!   initializer, an `impl`, a `trait`, a struct's fields).
//!
//! A block's position class is MACRO when a MACRO brace encloses it, else BODY when a BODY
//! brace does (an `impl` counts as one: no `extern` block stands directly in an impl), else
//! MODULE LEVEL. Its declarations are the `fn` and `static` words of the lines inside it. A
//! string's or comment's braces are skipped (line comments, block comments that open and close
//! on one line, `"..."` and `'{'` literals); a raw string spanning lines would be miscounted, and
//! none holds a brace in these crates' item positions.
//!
//! Prints, per crate: `foreign census <crate>: module level <b> blocks (<f> fns, <s> statics);
//! inside a body <b> (<f>, <s>); inside a macro definition <b> (<f>, <s>)`, then each block
//! inside a body or a macro definition by `file:line` and its ABI.

use std::path::{Path, PathBuf};

fn library() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../library")
}

/// Every `.rs` file under `dir`, sorted by path.
fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let entries = std::fs::read_dir(dir).unwrap_or_else(|e| panic!("listing {}: {e}", dir.display()));
    for entry in entries {
        let path = entry.unwrap_or_else(|e| panic!("an entry of {}: {e}", dir.display())).path();
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().is_some_and(|x| x == "rs") {
            out.push(path);
        }
    }
}

/// What a `{` opened.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Frame {
    Macro,
    /// An `extern` block: its census row's index.
    Extern(usize),
    Module,
    Body,
}

/// Where an `extern` block stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Class {
    ModuleLevel,
    InBody,
    InMacro,
}

/// One `extern` block the scan found.
struct Block {
    at: String,
    abi: String,
    class: Class,
    fns: u32,
    statics: u32,
}

/// `line` with its comment, its strings' and its char literals' contents removed: what its
/// braces and words are read from. `in_comment` carries a `/* .. */` across lines.
fn code_of(line: &str, in_comment: &mut bool) -> String {
    let b = line.as_bytes();
    let mut out = String::new();
    let mut i = 0;
    while i < b.len() {
        if *in_comment {
            if b[i] == b'*' && b.get(i + 1) == Some(&b'/') {
                *in_comment = false;
                i += 2;
            } else {
                i += 1;
            }
            continue;
        }
        match b[i] {
            b'/' if b.get(i + 1) == Some(&b'/') => break,
            b'/' if b.get(i + 1) == Some(&b'*') => {
                *in_comment = true;
                i += 2;
            }
            b'"' => {
                // The string's contents dropped, its quotes kept (an ABI is read from them).
                out.push('"');
                let start = i + 1;
                let mut j = start;
                while j < b.len() && b[j] != b'"' {
                    j += if b[j] == b'\\' { 2 } else { 1 };
                }
                let inner = line.get(start..j.min(b.len())).unwrap_or("");
                // An ABI string is short and plain: kept, so the census can print it.
                if inner.len() <= 16 && !inner.contains(['{', '}']) {
                    out.push_str(inner);
                }
                out.push('"');
                i = j + 1;
            }
            // `'{'`, `'}'`, `'\''`: a char literal (a lifetime `'a` has no closing quote two or
            // three bytes on).
            b'\'' if b.get(i + 2) == Some(&b'\'') => i += 3,
            b'\'' if b.get(i + 1) == Some(&b'\\') && b.get(i + 3) == Some(&b'\'') => i += 4,
            c => {
                out.push(c as char);
                i += 1;
            }
        }
    }
    out
}

/// The words of `code` (identifier runs).
fn words(code: &str) -> impl Iterator<Item = &str> {
    code.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_')).filter(|w| !w.is_empty())
}

/// The declarations `code` (a line, or the part of one, inside an `extern` block) starts: each
/// `;`-separated piece whose first word, after its outer attributes and its visibility and
/// qualifiers, is `fn` or `static` (a parameter's `unsafe extern "C" fn(..)` type is not one).
/// Returns (fns, statics).
fn decls(code: &str) -> (u32, u32) {
    let (mut fns, mut statics) = (0, 0);
    for piece in code.split(';') {
        let mut p = piece.trim_start();
        // `#[..]` attributes before the declaration on the same line.
        while let Some(attr) = p.strip_prefix("#[") {
            match attr.find(']') {
                Some(end) => p = attr[end + 1..].trim_start(),
                None => {
                    p = "";
                    break;
                }
            }
        }
        match words(p).find(|w| !matches!(*w, "pub" | "crate" | "super" | "in" | "self" | "safe" | "unsafe")) {
            Some("fn") => fns += 1,
            Some("static") => statics += 1,
            _ => {}
        }
    }
    (fns, statics)
}

/// The ABI of an `extern` block opened on `code`, or `None` when the line opens no such block:
/// its first `{` follows `extern` and at most an ABI string, with no `fn` or `crate` between.
fn extern_block_abi(code: &str) -> Option<String> {
    let brace = code.find('{')?;
    let head = code[..brace].trim_end();
    let at = head.rfind("extern")?;
    let before = &head[..at];
    if before.chars().last().is_some_and(|c| c.is_ascii_alphanumeric() || c == '_') {
        return None;
    }
    let after = head[at + "extern".len()..].trim();
    if after.is_empty() {
        return Some(String::from("C (bare)"));
    }
    let abi = after.strip_prefix('"')?.strip_suffix('"')?;
    Some(abi.to_string())
}

/// What the first `{` of `code` opens, the frame around it being `around`.
fn frame_of(code: &str, around: Option<Frame>, blocks: &[Block]) -> Frame {
    let trimmed = code.trim_start();
    if trimmed.contains("macro_rules!") || words(trimmed).take_while(|w| matches!(*w, "pub" | "crate" | "super" | "in" | "self" | "macro")).any(|w| w == "macro") {
        return Frame::Macro;
    }
    if extern_block_abi(code).is_some() {
        return Frame::Extern(blocks.len());
    }
    let before = &code[..code.find('{').unwrap_or(code.len())];
    let first_words: Vec<&str> = words(before).collect();
    if first_words.iter().take_while(|w| matches!(**w, "pub" | "crate" | "super" | "in" | "self" | "unsafe")).count() < first_words.len()
        && first_words.iter().find(|w| !matches!(**w, "pub" | "crate" | "super" | "in" | "self" | "unsafe")) == Some(&"mod")
    {
        return Frame::Module;
    }
    // A macro call's braces (`name! {`) and an arm of one (`=> {` directly in it).
    if before.trim_end().ends_with('!') {
        return Frame::Module;
    }
    if before.trim_end().ends_with("=>") && around == Some(Frame::Module) {
        return Frame::Module;
    }
    Frame::Body
}

/// The census of one file's text, its blocks pushed onto `blocks` with `shown` and their line.
fn scan(text: &str, shown: &str, blocks: &mut Vec<Block>) {
    let mut stack: Vec<Frame> = Vec::new();
    let mut in_comment = false;
    for (n, line) in text.lines().enumerate() {
        let code = code_of(line, &mut in_comment);
        // Declarations of the innermost frame when it is an `extern` block, before this line's
        // braces move it (a one-line block counts below, on its own line).
        if let Some(Frame::Extern(i)) = stack.last().copied() {
            // Up to the line's first `}` (the block's close, when it closes here).
            let inside = &code[..code.find('}').unwrap_or(code.len())];
            let (f, s) = decls(inside);
            blocks[i].fns += f;
            blocks[i].statics += s;
        }
        let mut rest = code.as_str();
        while let Some(at) = rest.find(['{', '}']) {
            if rest.as_bytes()[at] == b'}' {
                stack.pop();
                rest = &rest[at + 1..];
                continue;
            }
            let opened = frame_of(rest, stack.last().copied(), blocks);
            if let Frame::Extern(i) = opened {
                let class = if stack.contains(&Frame::Macro) {
                    Class::InMacro
                } else if stack.contains(&Frame::Body) {
                    Class::InBody
                } else {
                    Class::ModuleLevel
                };
                let abi = extern_block_abi(rest).unwrap_or_default();
                blocks.push(Block { at: format!("{shown}:{}", n + 1), abi, class, fns: 0, statics: 0 });
                // A one-line block: its declarations are on this line, after its `{`.
                let inner = &rest[at + 1..];
                let end = inner.find('}').unwrap_or(inner.len());
                let (f, s) = decls(&inner[..end]);
                blocks[i].fns += f;
                blocks[i].statics += s;
            }
            stack.push(opened);
            rest = &rest[at + 1..];
        }
    }
}

/// The census of crate `name`'s `src/`.
fn census(name: &str) -> Vec<Block> {
    let root = library().join(name).join("src");
    let mut files = Vec::new();
    rust_files(&root, &mut files);
    files.sort();
    let mut blocks = Vec::new();
    for file in files.iter() {
        let text = std::fs::read_to_string(file).unwrap_or_else(|e| panic!("reading {}: {e}", file.display()));
        let shown = file.strip_prefix(library()).map_or_else(|_| file.display().to_string(), |p| p.display().to_string());
        scan(&text, &shown, &mut blocks);
    }
    blocks
}

/// The census's sums of `blocks` of `class`: (blocks, fns, statics).
fn sums(blocks: &[Block], class: Class) -> (usize, u32, u32) {
    let of: Vec<&Block> = blocks.iter().filter(|b| b.class == class).collect();
    (of.len(), of.iter().map(|b| b.fns).sum(), of.iter().map(|b| b.statics).sum())
}

/// The scanner reads each position of a small text as the module doc says: a module-level
/// block, one in an inline module, one in a fn's body, one in a const's initializer, one inside
/// a `macro_rules!` definition, one in a `cfg_select!` arm at module level, and a one-line
/// block. A scanner that classed every block as module level fails the body and macro counts.
#[test]
fn the_scanner_classes_each_position() {
    let text = "unsafe extern \"C\" {
    fn a();
    static B: u8;
}
mod m {
    unsafe extern \"C-unwind\" {
        pub fn c();
    }
}
fn f() {
    let s = \"{ not a brace\";
    unsafe extern \"system\" {
        fn d();
    }
}
const K: u8 = {
    unsafe extern {
        static E: u8;
    }
    1
};
macro_rules! decl {
    () => {
        unsafe extern \"C\" {
            fn g();
        }
    };
}
cfg_select! {
    unix => {
        unsafe extern \"C\" {
            fn h();
        }
    }
    _ => {}
}
unsafe extern \"C\" { fn i(); }
";
    let mut blocks = Vec::new();
    scan(text, "t.rs", &mut blocks);
    let classes: Vec<(Class, &str, u32, u32)> = blocks.iter().map(|b| (b.class, b.abi.as_str(), b.fns, b.statics)).collect();
    assert_eq!(
        classes,
        [
            (Class::ModuleLevel, "C", 1, 1),
            (Class::ModuleLevel, "C-unwind", 1, 0),
            (Class::InBody, "system", 1, 0),
            (Class::InBody, "C (bare)", 0, 1),
            (Class::InMacro, "C", 1, 0),
            (Class::ModuleLevel, "C", 1, 0),
            (Class::ModuleLevel, "C", 1, 0),
        ],
        "each block's class, ABI and declarations"
    );
    println!("foreign census scanner: {} blocks classed", blocks.len());
}

/// The census of the pinned `core`, `alloc` and `std`, printed. `std` declares foreign fns at
/// module level and inside bodies (`guard::apple::enable`'s `_tlv_atexit`); the counts are the
/// census, not a bound.
#[test]
fn the_census_of_the_pinned_library() {
    let mut total = 0usize;
    for name in ["core", "alloc", "std"] {
        let blocks = census(name);
        let (mb, mf, ms) = sums(&blocks, Class::ModuleLevel);
        let (bb, bf, bs) = sums(&blocks, Class::InBody);
        let (xb, xf, xs) = sums(&blocks, Class::InMacro);
        println!("foreign census {name}: module level {mb} blocks ({mf} fns, {ms} statics); inside a body {bb} ({bf}, {bs}); inside a macro definition {xb} ({xf}, {xs})");
        for b in blocks.iter().filter(|b| b.class != Class::ModuleLevel) {
            println!("  {:?} {} extern \"{}\": {} fns, {} statics", b.class, b.at, b.abi, b.fns, b.statics);
        }
        total += blocks.len();
        if name == "std" {
            assert!(mf > 0, "std declares foreign fns at module level");
            assert!(bb > 0, "std declares foreign fns inside bodies (`guard::apple::enable`)");
            assert!(blocks.iter().any(|b| b.at.ends_with("sys/thread_local/guard/apple.rs:13") && b.class == Class::InBody), "apple.rs's `_tlv_atexit` block is inside `enable`'s body");
        }
    }
    assert!(total > 0, "the library declares `extern` blocks");
}
