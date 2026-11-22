//! The three paths against each other and against the references: `rustc_lexer::tokenize` for
//! the lexer and the skipper (tokenize and count braces), `core::str::from_utf8` for UTF-8, and a
//! byte predicate per class for the classifier. Over every `.rs` file of `library/` and
//! `crates/`, a hand-written corpus of every string and comment form at every offset around the
//! 16-, 32- and 64-byte boundaries, and random text and token soups from `blaze_sim`; with a
//! planted-bug control per oracle that must catch the plant (rule 53).

use std::borrow::ToOwned;
use std::path::{Path, PathBuf};
use std::string::String;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::vec::Vec;
use std::{format, vec};

use blaze_int::Int;
use blaze_sim::{Broke, Config, Tape};

use crate::lexer::{Plant, tokenize_planted};
use crate::skip::skip_body_in;
use crate::{
    Base, Cpu, DocStyle, FrontmatterAllowed, LiteralKind, Masks, SkipError, Token, TokenKind, classify, skip_body,
    tokenize, validate,
};

/// The widest path this machine runs, detected once here (the crate itself detects nothing).
fn widest() -> Cpu {
    #[cfg(target_arch = "x86_64")]
    {
        if std::is_x86_feature_detected!("avx2") {
            // SAFETY: the CPU said it has AVX2 just above.
            return unsafe { Cpu::avx2_unchecked() };
        }
    }
    Cpu::BASELINE
}

fn cpus() -> Vec<Cpu> {
    widest().and_narrower().collect()
}

// ---- The reference lexer, in this crate's types.

fn doc(style: rustc_lexer::DocStyle) -> DocStyle {
    match style {
        rustc_lexer::DocStyle::Outer => DocStyle::Outer,
        rustc_lexer::DocStyle::Inner => DocStyle::Inner,
    }
}

fn base(base: rustc_lexer::Base) -> Base {
    match base {
        rustc_lexer::Base::Binary => Base::Binary,
        rustc_lexer::Base::Octal => Base::Octal,
        rustc_lexer::Base::Decimal => Base::Decimal,
        rustc_lexer::Base::Hexadecimal => Base::Hexadecimal,
    }
}

fn literal(kind: rustc_lexer::LiteralKind) -> LiteralKind {
    use rustc_lexer::LiteralKind as R;
    match kind {
        R::Int { base: b, empty_int } => LiteralKind::Int { base: base(b), empty_int },
        R::Float { base: b, empty_exponent } => LiteralKind::Float { base: base(b), empty_exponent },
        R::Char { terminated } => LiteralKind::Char { terminated },
        R::Byte { terminated } => LiteralKind::Byte { terminated },
        R::Str { terminated } => LiteralKind::Str { terminated },
        R::ByteStr { terminated } => LiteralKind::ByteStr { terminated },
        R::CStr { terminated } => LiteralKind::CStr { terminated },
        R::RawStr { n_hashes } => LiteralKind::RawStr { n_hashes },
        R::RawByteStr { n_hashes } => LiteralKind::RawByteStr { n_hashes },
        R::RawCStr { n_hashes } => LiteralKind::RawCStr { n_hashes },
    }
}

fn kind(kind: rustc_lexer::TokenKind) -> TokenKind {
    use rustc_lexer::TokenKind as R;
    match kind {
        R::LineComment { doc_style } => TokenKind::LineComment { doc_style: doc_style.map(doc) },
        R::BlockComment { doc_style, terminated } => {
            TokenKind::BlockComment { doc_style: doc_style.map(doc), terminated }
        }
        R::Whitespace => TokenKind::Whitespace,
        R::Frontmatter { has_invalid_preceding_whitespace, invalid_infostring } => {
            TokenKind::Frontmatter { has_invalid_preceding_whitespace, invalid_infostring }
        }
        R::Ident => TokenKind::Ident,
        R::InvalidIdent => TokenKind::InvalidIdent,
        R::RawIdent => TokenKind::RawIdent,
        R::ForcedKeywordIdent => TokenKind::ForcedKeywordIdent,
        R::UnknownPrefix => TokenKind::UnknownPrefix,
        R::UnknownPrefixLifetime => TokenKind::UnknownPrefixLifetime,
        R::RawLifetime => TokenKind::RawLifetime,
        R::GuardedStrPrefix => TokenKind::GuardedStrPrefix,
        R::Literal { kind, suffix_start } => TokenKind::Literal { kind: literal(kind), suffix_start },
        R::Lifetime { starts_with_number } => TokenKind::Lifetime { starts_with_number },
        R::Semi => TokenKind::Semi,
        R::Comma => TokenKind::Comma,
        R::Dot => TokenKind::Dot,
        R::OpenParen => TokenKind::OpenParen,
        R::CloseParen => TokenKind::CloseParen,
        R::OpenBrace => TokenKind::OpenBrace,
        R::CloseBrace => TokenKind::CloseBrace,
        R::OpenBracket => TokenKind::OpenBracket,
        R::CloseBracket => TokenKind::CloseBracket,
        R::At => TokenKind::At,
        R::Pound => TokenKind::Pound,
        R::Tilde => TokenKind::Tilde,
        R::Question => TokenKind::Question,
        R::Colon => TokenKind::Colon,
        R::Dollar => TokenKind::Dollar,
        R::Eq => TokenKind::Eq,
        R::Bang => TokenKind::Bang,
        R::Lt => TokenKind::Lt,
        R::Gt => TokenKind::Gt,
        R::Minus => TokenKind::Minus,
        R::And => TokenKind::And,
        R::Or => TokenKind::Or,
        R::Plus => TokenKind::Plus,
        R::Star => TokenKind::Star,
        R::Slash => TokenKind::Slash,
        R::Caret => TokenKind::Caret,
        R::Percent => TokenKind::Percent,
        R::Unknown => TokenKind::Unknown,
        R::Eof => TokenKind::Eof,
    }
}

fn reference(text: &str, frontmatter: FrontmatterAllowed) -> Vec<Token> {
    let allowed = match frontmatter {
        FrontmatterAllowed::Yes => rustc_lexer::FrontmatterAllowed::Yes,
        FrontmatterAllowed::No => rustc_lexer::FrontmatterAllowed::No,
    };
    rustc_lexer::tokenize(text, allowed).map(|token| Token { kind: kind(token.kind), len: token.len }).collect()
}

/// The lexer on `cpu` (with `plant`) equals rustc_lexer on `text`, or the first difference.
fn same_tokens(what: &str, text: &str, frontmatter: FrontmatterAllowed, cpu: Cpu, plant: Plant) -> Result<(), Broke> {
    let want = reference(text, frontmatter);
    let got: Vec<Token> = tokenize_planted(text, frontmatter, cpu, plant).collect();
    let mut offset = 0usize;
    for index in 0..want.len().max(got.len()) {
        let (ours, theirs) = (got.get(index), want.get(index));
        if ours != theirs {
            let end = text.len().min(offset + 24);
            let around = text.get(offset..end).unwrap_or("<not at a character boundary>");
            return Err(Broke::Oracle(format!(
                "{what} ({frontmatter:?}, {}): token {index} at byte {offset} differs from rustc_lexer: ours {ours:?}, rustc_lexer's {theirs:?}; the text there: {around:?}",
                cpu.name()
            )));
        }
        offset += theirs.map_or(0, |token| token.len.widen_to::<usize>());
    }
    Ok(())
}

/// Tokenize and count braces: the skipper's reference.
fn counted(text: &str) -> Result<usize, SkipError> {
    if !text.starts_with('{') {
        return Err(SkipError::NotAtBrace);
    }
    let mut depth = 0usize;
    let mut at = 0usize;
    for token in rustc_lexer::tokenize(text, rustc_lexer::FrontmatterAllowed::No) {
        match token.kind {
            rustc_lexer::TokenKind::OpenBrace => depth += 1,
            rustc_lexer::TokenKind::CloseBrace => {
                depth -= 1;
                if depth == 0 {
                    return Ok(at);
                }
            }
            _ => {}
        }
        at += token.len.widen_to::<usize>();
    }
    Err(SkipError::Unclosed)
}

fn same_skip(what: &str, text: &str, cpu: Cpu, plant: Plant) -> Result<(), Broke> {
    let want = counted(text);
    let got = skip_body_in(cpu, text, plant);
    if got == want {
        Ok(())
    } else {
        let shown: String = text.chars().take(160).collect();
        Err(Broke::Oracle(format!(
            "{what} ({}): the skipper says {got:?}, tokenizing and counting braces says {want:?}; the text: {shown:?}",
            cpu.name()
        )))
    }
}

fn same_utf8(what: &str, bytes: &[u8], cpu: Cpu) -> Result<(), Broke> {
    let want = core::str::from_utf8(bytes).map(|_| ()).map_err(|e| e.valid_up_to());
    let got = validate(cpu, bytes).map(|_| ()).map_err(|e| e.valid_up_to);
    if got == want {
        Ok(())
    } else {
        Err(Broke::Oracle(format!(
            "{what} ({}): validate says {got:?}, core::str::from_utf8 says {want:?}, of {} bytes: {:02x?}",
            cpu.name(),
            bytes.len(),
            &bytes[..bytes.len().min(200)]
        )))
    }
}

// ---- The trees.

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(kind) = entry.file_type() else { continue };
        if kind.is_dir() {
            if path.file_name().is_some_and(|name| name == "target" || name == ".git") {
                continue;
            }
            rust_files(&path, out);
        } else if kind.is_file() && path.extension().is_some_and(|ext| ext == "rs") {
            out.push(path);
        }
    }
}

/// Every `.rs` file under `library/` and `crates/`, sorted, with its bytes.
fn tree_sources() -> Vec<(PathBuf, Vec<u8>)> {
    let mut paths = Vec::new();
    for dir in ["library", "crates"] {
        rust_files(&root().join(dir), &mut paths);
    }
    paths.sort();
    paths.into_iter().filter_map(|path| std::fs::read(&path).ok().map(|bytes| (path, bytes))).collect()
}

/// `check` on every file, on all cores; the first failure, if any.
fn over_files(files: &[(PathBuf, Vec<u8>)], check: impl Fn(&Path, &[u8]) -> Result<(), String> + Sync) -> Result<(), String> {
    let next = AtomicUsize::new(0);
    let threads = std::thread::available_parallelism().map_or(4, usize::from);
    std::thread::scope(|scope| {
        let handles: Vec<_> = (0..threads)
            .map(|_| {
                scope.spawn(|| {
                    loop {
                        let index = next.fetch_add(1, Ordering::Relaxed);
                        let Some((path, bytes)) = files.get(index) else { return Ok(()) };
                        check(path, bytes)?;
                    }
                })
            })
            .collect();
        handles.into_iter().map(|handle| handle.join().expect("a checking thread panicked")).collect()
    })
}

fn broke(e: Broke) -> String {
    match e {
        Broke::Oracle(why) | Broke::Harness(why) => why,
    }
}

/// The `{` offsets of every fn body in `text`: the first `{` or `;` token after each `fn`.
fn fn_bodies(text: &str) -> Vec<usize> {
    let mut bodies = Vec::new();
    let mut at = 0usize;
    let mut after_fn = false;
    for token in rustc_lexer::tokenize(text, rustc_lexer::FrontmatterAllowed::No) {
        let len = token.len.widen_to::<usize>();
        match token.kind {
            rustc_lexer::TokenKind::Ident if &text[at..at + len] == "fn" => after_fn = true,
            rustc_lexer::TokenKind::OpenBrace if after_fn => {
                bodies.push(at);
                after_fn = false;
            }
            rustc_lexer::TokenKind::Semi => after_fn = false,
            _ => {}
        }
        at += len;
    }
    bodies
}

/// The refinement: every lexer, skipper, UTF-8 validator, and byte classifier SIMD/scalar path agrees with `rustc_lexer::tokenize`, `core::str::from_utf8`, and the scalar predicates across all boundaries.
#[test]
fn every_file_of_the_trees_lexes_validates_and_skips_as_the_references_do() {
    let files = tree_sources();
    assert!(files.len() > 1000, "the trees under {} hold only {} .rs files", root().display(), files.len());
    let cpu = widest();
    let checked = over_files(&files, |path, bytes| {
        let what = format!("{}", path.display());
        same_utf8(&what, bytes, cpu).map_err(broke)?;
        let Ok(text) = core::str::from_utf8(bytes) else { return Ok(()) };
        same_tokens(&what, text, FrontmatterAllowed::Yes, cpu, Plant::Clean).map_err(broke)?;
        for body in fn_bodies(text) {
            same_skip(&format!("{what}, the fn body at byte {body}"), &text[body..], cpu, Plant::Clean).map_err(broke)?;
        }
        Ok(())
    });
    if let Err(why) = checked {
        panic!("{why}");
    }
}

#[test]
fn every_path_is_the_same_on_a_sample_of_the_trees() {
    let files = tree_sources();
    let sample: Vec<_> = files.into_iter().step_by(23).collect();
    let checked = over_files(&sample, |path, bytes| {
        let what = format!("{}", path.display());
        for cpu in cpus() {
            same_utf8(&what, bytes, cpu).map_err(broke)?;
            let Ok(text) = core::str::from_utf8(bytes) else { continue };
            same_tokens(&what, text, FrontmatterAllowed::No, cpu, Plant::Clean).map_err(broke)?;
            for block in bytes.chunks(64) {
                let mut padded = [0u8; 64];
                padded[..block.len()].copy_from_slice(block);
                let (got, want) = (classify(cpu, &padded), oracle_masks(&padded));
                if got != want {
                    return Err(format!("{what}: classify on {} gives {got:?}, the byte predicates {want:?}", cpu.name()));
                }
            }
        }
        Ok(())
    });
    if let Err(why) = checked {
        panic!("{why}");
    }
}

// ---- The corpus: every string and comment form, around every block boundary.

const PIECES: &[&str] = &[
    "r", "b", "c", "k", "br", "cr", "r#", "k#", "r#a", "k#fn", "r#_", "#", "##", "#\"", "##\"a\"##", "\"", "\"a\"",
    "\"\\\"\"", "\"\\\\\"", "\"\\\\\\\"\"", "\"a\\\nb\"", "\"{\"", "\"}\"", "\"//\"", "\"/*\"", "'", "'a", "'a'", "'\\''",
    "'\\\\'", "'{'", "'}'", "'\"'", "'ab", "'ab'", "'r#a", "'r#", "'1", "'1a'", "'a#", "'1#", "b'a'", "b'\\''", "b'{'",
    "b\"x\"", "c\"x\"", "b\"{\"", "br\"x\"", "br#\"x\"#", "cr#\"a\"b\"#", "r\"a\"", "r\"{\"", "r#\"a\"#", "r##\"a\"#\"##",
    "r#\"unterminated", "r#x", "r#{", "br#{", "cr#}", "r###", "0x", "0b", "0o", "0x_", "0b_1", "1e", "1e+", "1e+r",
    "1e-5", "1E5", "1.", "1.0", "1.0e5", "1..2", "1.a", "1._", "12u8", "0b102", "0xFFg", "1e5f32", "1.e3", "0.0.0",
    "0e", "00", "0_0", "//", "///", "////", "//!", "// {\n", "/// }\n", "/*", "*/", "/**", "/*!", "/**/", "/***/",
    "/* { */", "/* /* */ */", "/* /* */", "/*/", "/*/*/", "/*/ */", "\n", "\r\n", "\t", " ", "\x0b", "\x0c", "{",
    "}", "{}", "[", "]", "(", ")", ";", ".", "..", "...", "-", "--", "---", "----", "+", "=>", "::", "é", "éa", "aé",
    "🦀", "a🦀", "🦀a", "\u{200d}", "a\u{200d}🦀", "\u{2028}", "\u{85}", "\u{200E}", "\u{200F}", "\u{a0}", "\0", "№",
    "'é'", "'é", "'🦀'", "_", "__", "a_b", "fn", "x\"y\"", "r\"\"r\"\"", "\"a\"suffix", "\"a\"r\"b\"", "\"a\"é",
    "'a'b\"x\"", "#[cfg(x)]", "#!", "\\", "\\\"", "'\\n'", "'\n'", "'\n", "'\n'", "' '", "'/", "'a/", "b'", "b'ab'",
    "'''", "''", "'\\", "\"\\", "b\"\\", "c'", "k'a", "abc'", "abc#", "abc\"", "---\n", "---cargo\n---\n",
    "---x\nuse y;\n", "  ---\n---\n", "----\n---\n", "---\n//!\n", "--- a b\n---", "--- -\n --- \n",
];

/// The fillers put before a piece, so that every byte of it falls at every lane.
fn filler(kind: usize, len: usize) -> String {
    match kind {
        0 => " ".repeat(len),
        1 => "\n".repeat(len),
        2 => "x;".repeat(len / 2) + &" ".repeat(len % 2),
        _ => "{".repeat(len),
    }
}

/// Every token, skip and validation result on `text`, on every path.
fn all_paths(what: &str, text: &str) -> Result<(), Broke> {
    for cpu in cpus() {
        same_tokens(what, text, FrontmatterAllowed::Yes, cpu, Plant::Clean)?;
        same_tokens(what, text, FrontmatterAllowed::No, cpu, Plant::Clean)?;
        same_utf8(what, text.as_bytes(), cpu)?;
        if text.starts_with('{') {
            same_skip(what, text, cpu, Plant::Clean)?;
        }
        let braced = format!("{{{text}}}");
        same_skip(what, &braced, cpu, Plant::Clean)?;
    }
    Ok(())
}

#[test]
fn every_piece_at_every_offset_is_rustc_lexers() {
    for piece in PIECES {
        for kind in 0..4 {
            for len in 0..=72 {
                let text = filler(kind, len) + piece;
                if let Err(e) = all_paths(&format!("{piece:?} after {len} of filler {kind}"), &text) {
                    panic!("{}", broke(e));
                }
            }
        }
    }
}

#[test]
fn every_pair_of_pieces_near_a_boundary_is_rustc_lexers() {
    const OFFSETS: &[usize] = &[0, 15, 31, 47, 63];
    for first in PIECES {
        for second in PIECES {
            for &len in OFFSETS {
                let text = filler(0, len) + first + second + "}";
                let what = format!("{first:?} then {second:?} after {len} spaces");
                let checked = same_tokens(&what, &text, FrontmatterAllowed::No, widest(), Plant::Clean)
                    .and_then(|()| same_skip(&what, &format!("{{{text}"), widest(), Plant::Clean));
                if let Err(e) = checked {
                    panic!("{}", broke(e));
                }
            }
        }
    }
}

// ---- Random text and token soups.

fn soup(tape: &Tape) -> String {
    let parts = tape.vec(0..=24, "pieces", |tape| {
        let piece = *tape.one_of(PIECES, "piece");
        let gap = *tape.one_of(&["", " ", "\n", "a", "1", "{", "}", "\\", "\"", "'"], "gap");
        piece.to_owned() + gap
    });
    let pad = filler(tape.index(4, "filler"), tape.index(70, "filler length"));
    pad + &parts.concat()
}

#[test]
fn random_soups_are_rustc_lexers_on_every_path() {
    blaze_sim::check(Config::cases(600), |tape: &Tape| all_paths("a soup", &soup(tape)));
}

#[test]
fn random_text_is_rustc_lexers_on_every_path() {
    blaze_sim::check(Config::cases(600), |tape: &Tape| all_paths("random text", &tape.text(0..=200, "text")));
}

#[test]
fn random_bytes_are_rustc_lexers_on_every_path() {
    blaze_sim::check(Config::cases(600), |tape: &Tape| {
        let bytes = tape.bytes(0..=200, "bytes");
        for cpu in cpus() {
            same_utf8("random bytes", &bytes, cpu)?;
        }
        all_paths("random bytes, lossy", &String::from_utf8_lossy(&bytes))
    });
}

// ---- UTF-8: every boundary form at every alignment.

const SEQUENCES: &[&[u8]] = &[
    b"a",
    b"\x7f",
    b"\xc2\x80",
    b"\xdf\xbf",
    b"\xe0\xa0\x80",
    b"\xed\x9f\xbf",
    b"\xee\x80\x80",
    b"\xef\xbf\xbf",
    b"\xf0\x90\x80\x80",
    b"\xf4\x8f\xbf\xbf",
    // Overlong forms.
    b"\xc0\x80",
    b"\xc1\xbf",
    b"\xe0\x80\x80",
    b"\xe0\x9f\xbf",
    b"\xf0\x80\x80\x80",
    b"\xf0\x8f\xbf\xbf",
    // Surrogates.
    b"\xed\xa0\x80",
    b"\xed\xbf\xbf",
    // Past U+10FFFF.
    b"\xf4\x90\x80\x80",
    b"\xf5\x80\x80\x80",
    b"\xff",
    b"\xfe",
    // Lone continuations and truncations.
    b"\x80",
    b"\xbf",
    b"\xc2",
    b"\xe2\x82",
    b"\xf0\x9f\x98",
    b"\xe2",
    b"\xf0",
    b"\xf0\x9f",
];

fn utf8_case(tape: &Tape) -> Vec<u8> {
    let mut bytes = vec![b'a'; tape.index(80, "alignment")];
    let pieces = tape.vec(0..=12, "sequences", |tape| {
        let sequence = *tape.one_of(SEQUENCES, "sequence");
        let ascii = if tape.coin("an ASCII block after") { 64 } else { tape.index(3, "ASCII after") };
        (sequence, ascii)
    });
    for (sequence, ascii) in pieces {
        bytes.extend_from_slice(sequence);
        bytes.extend(std::iter::repeat_n(b'x', ascii));
    }
    bytes
}

#[test]
fn utf8_is_core_on_every_boundary_form_at_every_alignment() {
    blaze_sim::check(Config::cases(3000), |tape: &Tape| {
        let bytes = utf8_case(tape);
        for cpu in cpus() {
            same_utf8("a mix of sequences", &bytes, cpu)?;
        }
        Ok::<(), Broke>(())
    });
}

#[test]
fn utf8_is_core_on_every_two_byte_sequence_at_the_block_edges() {
    for &at in &[0usize, 15, 31, 62, 63, 64, 127] {
        for first in 0..=255u8 {
            for second in 0..=255u8 {
                let mut bytes = vec![b'a'; at];
                bytes.push(first);
                bytes.push(second);
                for cpu in cpus() {
                    if let Err(e) = same_utf8("a pair", &bytes, cpu) {
                        panic!("{}", broke(e));
                    }
                }
            }
        }
    }
}

// ---- The classifier against a byte predicate per class.

fn oracle_masks(block: &[u8; 64]) -> Masks {
    let mut masks = Masks::default();
    for (at, &byte) in block.iter().enumerate() {
        let bit = 1u64 << at;
        let classes: [(&mut u64, bool); 18] = [
            (&mut masks.ident, byte.is_ascii_alphanumeric() || byte == b'_'),
            (&mut masks.whitespace, matches!(byte, b'\t' | b'\n' | 0x0B | 0x0C | b'\r' | b' ')),
            (&mut masks.digit, byte.is_ascii_digit()),
            (&mut masks.underscore, byte == b'_'),
            (&mut masks.double_quote, byte == b'"'),
            (&mut masks.single_quote, byte == b'\''),
            (&mut masks.slash, byte == b'/'),
            (&mut masks.star, byte == b'*'),
            (&mut masks.hash, byte == b'#'),
            (&mut masks.backslash, byte == b'\\'),
            (&mut masks.open_paren, byte == b'('),
            (&mut masks.close_paren, byte == b')'),
            (&mut masks.open_brace, byte == b'{'),
            (&mut masks.close_brace, byte == b'}'),
            (&mut masks.open_bracket, byte == b'['),
            (&mut masks.close_bracket, byte == b']'),
            (&mut masks.newline, byte == b'\n'),
            (&mut masks.non_ascii, byte >= 0x80),
        ];
        for (mask, holds) in classes {
            if holds {
                *mask |= bit;
            }
        }
    }
    masks
}

#[test]
fn every_class_of_every_byte_on_every_path_is_its_predicate() {
    // Every byte value in every lane: block `k` holds byte `(k + lane) % 256` in each lane.
    for start in 0..256usize {
        let block: [u8; 64] = core::array::from_fn(|lane| ((start + lane) % 256).truncate_to::<u8>());
        for cpu in cpus() {
            let (got, want) = (classify(cpu, &block), oracle_masks(&block));
            assert_eq!(got, want, "classify on {} of the block from byte {start}", cpu.name());
        }
    }
    blaze_sim::check(Config::cases(500), |tape: &Tape| {
        let block: [u8; 64] = tape.array("block", |tape| tape.below(256, "byte").truncate_to::<u8>());
        for cpu in cpus() {
            let (got, want) = (classify(cpu, &block), oracle_masks(&block));
            if got != want {
                return Err(Broke::Oracle(format!("classify on {}: {got:?}, the predicates {want:?}", cpu.name())));
            }
        }
        Ok(())
    });
}

#[test]
fn the_skipper_refuses_a_text_not_at_a_brace() {
    assert_eq!(skip_body(widest(), "fn f() {}"), Err(SkipError::NotAtBrace));
    assert_eq!(skip_body(widest(), ""), Err(SkipError::NotAtBrace));
    assert_eq!(skip_body(widest(), "{ \"}\" '}' /* } */ // }\n }"), Ok(24));
    assert_eq!(tokenize("", FrontmatterAllowed::No, widest()).count(), 0);
    for edge in [
        "{ #[inline] fn g<'a, 'static>() { let c = '{'; let b = b'}'; } }",
        "{ r#r\"\\\"}\" }",
        "{ k#r\"\\\"}\" }",
        "{ b'a }",
        "{ 'a'b\"}\" }",
        "{ r#{ } }",
        "{ \"a\"suffix } ",
    ] {
        for cpu in cpus() {
            same_skip("skip edge", edge, cpu, Plant::Clean).expect("matches rustc_lexer");
        }
    }
}

// ---- Planted-bug controls (rule 53).

#[derive(Clone, Copy, Debug)]
enum Planted {
    Clean,
    Planted,
}

/// A string whose escaped quote falls at every lane: `n` bytes in, a `\"` and then a brace.
fn escaped_quote_case(tape: &Tape) -> String {
    let pad = " ".repeat(tape.index(64, "pad"));
    let inside = "x".repeat(tape.index(64, "inside"));
    format!("{{{pad}\"{inside}\\\"}}\" }}")
}

#[test]
fn the_oracle_catches_an_escaped_quote_at_a_block_boundary_taken_as_a_string_end() {
    let run = |tape: &Tape, planted: Planted| {
        let plant = match planted {
            Planted::Clean => Plant::Clean,
            Planted::Planted => Plant::EscapedQuoteAtBlockEnd,
        };
        let text = escaped_quote_case(tape);
        same_tokens("an escaped quote", &text, FrontmatterAllowed::No, widest(), plant)
    };
    blaze_sim::control(Config::cases(3000), Planted::Clean, Planted::Planted, "rustc_lexer", run);
}

#[test]
fn the_oracle_catches_an_escaped_quote_at_a_block_boundary_taken_as_a_string_end_by_the_skipper() {
    let run = |tape: &Tape, planted: Planted| {
        let plant = match planted {
            Planted::Clean => Plant::Clean,
            Planted::Planted => Plant::EscapedQuoteAtBlockEnd,
        };
        same_skip("an escaped quote", &escaped_quote_case(tape), widest(), plant)
    };
    blaze_sim::control(Config::cases(3000), Planted::Clean, Planted::Planted, "tokenizing and counting", run);
}

#[test]
fn the_oracle_catches_a_lifetime_taken_as_a_char_literal_start() {
    let run = |tape: &Tape, planted: Planted| {
        let plant = match planted {
            Planted::Clean => Plant::Clean,
            Planted::Planted => Plant::LifetimeAsChar,
        };
        same_tokens("lifetimes", &soup(tape), FrontmatterAllowed::No, widest(), plant)?;
        same_tokens("a lifetime", "fn f<'a>(x: &'a u8) {}", FrontmatterAllowed::No, widest(), plant)
    };
    blaze_sim::control(Config::cases(300), Planted::Clean, Planted::Planted, "rustc_lexer", run);
}

// ---- The first-byte table against the `char` match it replaced.

/// Forty inputs where a first byte alone could mislead: prefixes against identifiers, a
/// lifetime against a character, numbers against ranges and fields, doc comments against plain
/// ones, and every non-ASCII class the Reference names ("Whitespace": U+0085, U+200E, U+200F,
/// U+2028, U+2029; "Identifiers": XID_Start beyond ASCII) beside one it does not (U+FEFF, an
/// unknown token; an emoji, an invalid identifier).
const FIRST_BYTE_TRICKS: [&str; 40] = [
    "r#\"", "r#\"a\"#", "r#a", "r#é", "r#", "k#fn", "k#é", "k#", "b'", "b'a'", "b\"", "br#\"x\"#", "c\"", "cr\"x\"",
    "c'a'", "'a", "'a'", "'é", "'é'", "'r#a", "0x", "0xé", "1..2", "1.e3", "1.é", "/**/", "/***/", "//!", "///",
    "////", "/*!*/", "#\"", "##", "\u{FEFF}", "\u{2028}x", "\u{85}\u{200E}\u{200F}\u{2029}", "éa_ü", "🦀x", "a🦀",
    "---\n---\n",
];

/// Operator prefixes, non-ASCII identifiers and whitespace, unterminated forms and raw strings,
/// drawn between the corpus's pieces.
const FIRST_BYTE_PARTS: &[&str] = &[
    "!", "!=", "%=", "&&", "&=", "*=", "+=", "-=", "->", "-", "--", "---", "..=", "...", "::", "<<=", ">>=", "<-", "=>",
    "==", "^=", "|=", "||", "@", "~", "?", "$", ";", ",", "`", "\\", "\0", "\x7f", "é", "ñame", "日本", "a\u{300}",
    "\u{2028}", "\u{200E}", "\u{85}", "\u{a0}", "\u{FEFF}", "🦀", "\u{200d}", "\"abc", "\"a\\\"", "/* open", "/* /* */",
    "'a", "'ab", "b'x", "c\"x", "r#\"x\"", "r##\"x\"#", "br\"", "cr#", "0b", "0o7", "0x_", "1e+", "1.0f32", "_",
];

fn first_byte_soup(tape: &Tape) -> String {
    let parts = tape.vec(0..=32, "parts", |tape| {
        let part = if tape.index(3, "from the corpus") == 0 {
            *tape.one_of(PIECES, "piece")
        } else {
            *tape.one_of(FIRST_BYTE_PARTS, "part")
        };
        let gap = *tape.one_of(&["", " ", "\n", "a", "é", "1", "'", "\"", "#", "/"], "gap");
        part.to_owned() + gap
    });
    parts.concat()
}

/// The table's tokens and the old `char` match's are the same, or the first difference.
fn same_as_the_char_match(what: &str, text: &str) -> Result<(), Broke> {
    for cpu in cpus() {
        for frontmatter in [FrontmatterAllowed::Yes, FrontmatterAllowed::No] {
            let want: Vec<Token> = crate::lexer::first::tokenize_reference(text, frontmatter, cpu).collect();
            let got: Vec<Token> = tokenize(text, frontmatter, cpu).collect();
            if got != want {
                let at = got.iter().zip(&want).position(|(ours, theirs)| ours != theirs).unwrap_or(got.len().min(want.len()));
                return Err(Broke::Oracle(format!(
                    "{what} ({frontmatter:?}, {}): token {at} differs: the table's {:?}, the char match's {:?}; the text: {text:?}",
                    cpu.name(),
                    got.get(at),
                    want.get(at)
                )));
            }
        }
    }
    Ok(())
}

/// `advance_token` by first byte gives the old `char` match's tokens, kind and length, on the
/// forty tricky inputs (alone, after an identifier and after a space) and on 2,000 drawn soups.
/// A wrong arm in the table (say `#` before `"` taken as `Pound`, or `b'` as an identifier)
/// fails here with the text and the token index. Prints `test
/// tests::advance_token_by_first_byte_is_the_char_match ... ok`.
#[test]
fn advance_token_by_first_byte_is_the_char_match() {
    for trick in FIRST_BYTE_TRICKS {
        for text in [trick.to_owned(), format!("x{trick}"), format!(" {trick}"), format!("{trick}é")] {
            if let Err(e) = same_as_the_char_match("a tricky input", &text) {
                panic!("{}", broke(e));
            }
        }
    }
    blaze_sim::check(Config::cases(2000), |tape: &Tape| same_as_the_char_match("a first-byte soup", &first_byte_soup(tape)));
}
