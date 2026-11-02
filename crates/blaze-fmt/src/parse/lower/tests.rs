//! The lowering read through `syntax::view`: every source file of this crate lowers with nothing
//! left unsupported, to the `Debug` recorded in `lowered.txt` (an FNV-1a hash per file, so a
//! node, span, symbol or token that changes shows), and a qualified path's segments come out
//! innermost qualifier first.

use std::fmt::{self, Write as _};
use std::path::{Path, PathBuf};

use expect_test::expect_file;
use syntax::view::{self as ra, AstNode};

use super::{DelimIndex, Lower};
use crate::ast;
use crate::parse::lexer::{self, StripTokens};
use crate::parse::stream::parse_ra;
use crate::span::BytePos;
use crate::span::edition::Edition;

/// The `.rs` files under `dir`, sorted.
fn sources(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).expect("the crate's sources are readable") {
        let path = entry.expect("a directory entry").path();
        if path.is_dir() {
            sources(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
    out.sort();
}

/// `src` lexed, parsed and lowered as a whole file of edition 2024, its spans from 0.
fn lowered(src: &str) -> Result<ast::Crate, String> {
    let edition = Edition::Edition2024;
    let stream =
        lexer::lex_token_trees(src, BytePos(0), edition, StripTokens::ShebangAndFrontmatter)
            .map_err(|e| format!("lex: {} at {:?}", e.message, e.span))?;
    let delims = DelimIndex::new(&stream);
    let parse = parse_ra(src, edition.to_ra(), parser::TopEntryPoint::SourceFile)
        .map_err(|(at, message)| format!("parse: {message} at {at}"))?;
    let file = ra::SourceFile::cast(parse.root()).expect("a source file parses to one");
    let lower = Lower::new(BytePos(0), edition, &delims);
    let krate = lower.krate(file);
    lower
        .finish()
        .map_err(|u| format!("lower: {} at {:?}", u.message, u.span))?;
    Ok(krate)
}

/// FNV-1a over what is written to it: a hash that no toolchain bump changes.
struct Fnv(u64);

impl fmt::Write for Fnv {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        for b in s.bytes() {
            self.0 = (self.0 ^ u64::from(b)).wrapping_mul(0x0000_0100_0000_01b3);
        }
        Ok(())
    }
}

#[test]
fn every_source_file_lowers_as_recorded() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut files = Vec::new();
    sources(&root.join("src"), &mut files);
    let mut record = String::new();
    let mut failed = String::new();
    for path in &files {
        let rel = path.strip_prefix(root).expect("under the crate").display();
        let src = std::fs::read_to_string(path).expect("a source file is UTF-8");
        match lowered(&src) {
            Ok(krate) => {
                let mut hash = Fnv(0xcbf2_9ce4_8422_2325);
                write!(hash, "{krate:?}").expect("hashing does not fail");
                writeln!(record, "{rel} {} {:016x}", krate.items.len(), hash.0).unwrap();
            }
            Err(why) => writeln!(failed, "{rel}: {why}").unwrap(),
        }
    }
    assert!(failed.is_empty(), "files that did not lower:\n{failed}");
    expect_file!["lowered.txt"].assert_eq(&record);
}

/// Each path expression of `src`'s first function as its qualified self's position and its
/// segments' names.
fn paths(src: &str) -> Vec<(Option<usize>, Vec<String>)> {
    let edition = Edition::Edition2024;
    let stream = lexer::lex_token_trees(src, BytePos(0), edition, StripTokens::Nothing)
        .expect("the test source lexes");
    let delims = DelimIndex::new(&stream);
    let parse = parse_ra(src, edition.to_ra(), parser::TopEntryPoint::SourceFile)
        .expect("the test source parses");
    let lower = Lower::new(BytePos(0), edition, &delims);
    let found = parse
        .root()
        .descendants()
        .filter_map(ra::PathExpr::cast)
        .filter_map(|e| e.path())
        .map(|p| {
            let (qself, path) = lower.path(p);
            let names = path
                .segments
                .iter()
                .map(|s| s.ident.name.as_str().to_owned())
                .collect();
            (qself.map(|q| q.position), names)
        })
        .collect();
    assert!(lower.finish().is_ok(), "the test source lowers");
    found
}

#[test]
fn qualified_paths_run_from_the_innermost_qualifier() {
    let got = paths("fn f() { a; a::b::c; ::d::e; <T as Tr<u8>>::x::y; <T>::z; }");
    let want: Vec<(Option<usize>, Vec<String>)> = [
        (None, &["a"][..]),
        (None, &["a", "b", "c"]),
        (None, &["{{root}}", "d", "e"]),
        (Some(1), &["Tr", "x", "y"]),
        (Some(0), &["z"]),
    ]
    .into_iter()
    .map(|(q, names)| (q, names.iter().map(|n| (*n).to_owned()).collect()))
    .collect();
    assert_eq!(got, want);
}
