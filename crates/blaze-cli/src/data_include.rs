//! A file read as DATA by `include_str!` or `include_bytes!`: listed by the outline of the file
//! whose text writes the call, so the drive opens it and a `CrateScope` that read it holds its
//! bytes' digest in its read list (rule 21: keys cover every input), and NEVER a module's
//! `Surface` (it is no Rust).
//!
//! The rules. The pinned library, core `macros/mod.rs`, `include_str!`: "Includes a UTF-8
//! encoded file as a string. The file is located relative to the current file (similarly to how
//! modules are found)."; `include_bytes!`: "Includes a file as a reference to a byte array. The
//! file is located relative to the current file (similarly to how modules are found)." So the
//! file a literal call names is known from the calling file's path and the literal alone: the
//! outline lists it as it lists a `mod x;` file, before any name is resolved.
//!
//! An `include!` in a body is data too. core `include!`: "Parses a file as an expression or an
//! item according to the context"; in a body (expression or statement position) the file is
//! one expression, a block whose items are the body's, so its whole text is what a reader
//! (the names' block module, the body's lowering) reads: no module `Surface`, and an outline
//! of it as items would report its expression as syntax errors. Both readers read it as data
//! (blaze-scope resolve.rs `Host::include`, blaze-ladder body_host.rs). An item-position
//! `include!` in the file's modules is a module file, listed by the outline (work.rs
//! `included_file`) before this list, which then skips it (one part a file).
//!
//! What is listed: each `include_str!` / `include_bytes!` / `include!` (any path before it:
//! `core::`, `::std::`) whose one argument is a plain or raw string literal with no suffix,
//! anywhere in the file's text (an item, an attribute, a body), with `(..)`, `[..]` or `{..}`
//! and an optional trailing comma. A call whose path is made by another macro
//! (`include_str!(concat!(env!("CARGO_MANIFEST_DIR"), ".."))`) or written by a `macro_rules!`
//! expansion is not listed. In a BODY its read is the file's checkout-read `Given` cell, the one
//! a build script's `file:` read of it has (engine_script_reads.rs `file_read`): kept as a leaf
//! row the sweep follows and planned as its `Given` task from the next request on, so an edit of
//! the file enters the body (engine_bodies.rs `data_read`). At item level (a crate's names) it is
//! a part no record lists, and the drive refuses it by name ("a part its file's record does not
//! list", rule 3), never quietly.
//!
//! A listed file is opened as the one path it names (`module: false`): its `FileItems` task
//! reads its bytes and its out is their content id ([`data_out`]). A file that is not there is
//! an ANSWER, not a stop of the plan: its record's out is [`absent_out`], a digest of its own,
//! with no finding ([`DATA_FILE`]). The call that reads it is the one that reports it, as the
//! expansion's own diagnostic ("including `gen.in`: no such file", blaze-expand failed.rs), and
//! everything else is still checked, as rustc's one error at the call ("couldn't read
//! `src/gen.in`: No such file or directory") leaves the rest of the crate checked. Creating
//! the file moves the sweep's slot, so its task's key, so its out from [`absent_out`] to
//! [`data_out`], and every reader of that cell is entered again. A call in code the cfg turns
//! off reads nothing and reports nothing, as rustc's. The same seam as a module found at both
//! its places (work.rs `MODULE_AT_BOTH`): the file's situation is its record.

use blaze_drive::record::DATA_FILE;
use blaze_ids::ContentId;
use blaze_lex::{Cpu, FrontmatterAllowed, LiteralKind, TokenKind, tokenize};
use blaze_own::Vec;
use bstd::mem::Frame;

/// One literal data include of a file's text: the literal as written (`"../a.json"`,
/// `r#"x.bin"#`) and the byte where the call's macro name starts (the part's place).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct DataInclude<'t> {
    pub literal: &'t str,
    pub place: u32,
}

/// Where a scan is in one call: after its name, after its `!`, after its opener, after its
/// literal, after the comma that may follow the literal.
#[derive(Clone, Copy)]
enum At<'t> {
    Idle,
    Name { place: u32 },
    Bang { place: u32 },
    Open { place: u32, close: TokenKind },
    Literal { place: u32, close: TokenKind, literal: &'t str },
    Comma { place: u32, close: TokenKind, literal: &'t str },
}

/// Each literal `include_str!` / `include_bytes!` / `include!` call of `text`, in text order
/// (an item-position `include!` too: the caller skips a file it already lists as a module).
/// Comments and string contents are tokens of their own (blaze-lex, rustc_lexer's tokens), so
/// a call written inside a comment or a string is not listed. One pass, no recursion.
pub(crate) fn data_includes<'t, 'f>(text: &'t str, frame: &'f Frame) -> Vec<DataInclude<'t>, &'f Frame> {
    let mut found = Vec::new_in(frame);
    let mut at = At::Idle;
    let mut end = 0usize;
    for token in tokenize(text, FrontmatterAllowed::No, Cpu::BASELINE) {
        let start = end;
        end += token.len as usize;
        let kind = token.kind;
        if matches!(kind, TokenKind::Whitespace | TokenKind::LineComment { .. } | TokenKind::BlockComment { .. }) {
            continue;
        }
        let word = &text[start..end];
        at = match (at, kind) {
            (At::Name { place }, TokenKind::Bang) => At::Bang { place },
            (At::Bang { place }, TokenKind::OpenParen) => At::Open { place, close: TokenKind::CloseParen },
            (At::Bang { place }, TokenKind::OpenBracket) => At::Open { place, close: TokenKind::CloseBracket },
            (At::Bang { place }, TokenKind::OpenBrace) => At::Open { place, close: TokenKind::CloseBrace },
            (At::Open { place, close }, TokenKind::Literal { kind: lit, suffix_start }) if path_literal(lit) && suffix_start as usize == word.len() => {
                At::Literal { place, close, literal: word }
            }
            (At::Literal { place, close, literal }, TokenKind::Comma) => At::Comma { place, close, literal },
            (At::Literal { place, close, literal } | At::Comma { place, close, literal }, closing) if closing == close => {
                found.push(DataInclude { literal, place });
                At::Idle
            }
            // Anything else ends the call being read; the token may start the next one.
            _ => match kind {
                TokenKind::Ident if matches!(word, "include_str" | "include_bytes" | "include") => At::Name { place: start as u32 },
                _ => At::Idle,
            },
        };
    }
    found
}

/// A literal a path is written as: a terminated plain string or a raw string (what work.rs
/// `path_literal` reads); a byte, C or unterminated string is none.
fn path_literal(kind: LiteralKind) -> bool {
    matches!(kind, LiteralKind::Str { terminated: true } | LiteralKind::RawStr { n_hashes: Some(_) })
}

/// The out of a data file's `FileItems` record: its bytes' content id, in the sweep's domain
/// (the id the drive opened it by), so the record moves exactly when the bytes do and every
/// `CrateScope` that read it is asked again.
pub(crate) fn data_out(bytes: &[u8]) -> ContentId {
    ContentId::of(crate::sweep::FILE_DOMAIN, bytes)
}

/// The domain of [`absent_out`]: no file's bytes are hashed in it.
const ABSENT_DOMAIN: &str = "blaze-cli an included data file that is not there 1";

/// The out of a listed data file that is not there: an answer of its own. It is no
/// [`data_out`] (another domain, so an empty file is not an absent one) and not
/// `blaze_drive::key::ABSENT` (a cell no record defines, which a reader is refused on). The
/// task is named by the file's path, so the path is not in it.
pub(crate) fn absent_out() -> ContentId {
    ContentId::of(ABSENT_DOMAIN, b"")
}

/// Whether a `Locate::File` part's byte says its file is read as data ([`DATA_FILE`]), not as
/// Rust in an edition.
pub(crate) fn is_data(edition: u8) -> bool {
    edition == DATA_FILE
}

#[cfg(test)]
mod tests {
    use bstd::io::{Io, Limits, SimWorld};
    use bstd::mem::TASK_FRAME;
    use bstd::test::BUDGET;

    use super::*;

    fn listed(text: &str) -> std::vec::Vec<(std::string::String, u32)> {
        let io = Io::simulated(SimWorld::new(1, Limits::USUAL));
        let cx = io.cx(BUDGET, TASK_FRAME).expect("a root context");
        data_includes(text, cx.frame()).iter().map(|found| (std::string::String::from(found.literal), found.place)).collect()
    }

    /// core `macros/mod.rs`: `include_str!` and `include_bytes!` take one string literal
    /// (`($file:expr $(,)?)`), "located relative to the current file". Each literal call is
    /// listed at its name's byte, in an item, an attribute and a body alike, with any of the
    /// three delimiters and a trailing comma; a call in a comment or a string, a computed path
    /// and a `macro_rules!` metavariable are not.
    ///
    /// Prints nothing; passes as `data_include::tests::each_literal_data_include_is_listed ... ok`.
    #[test]
    fn each_literal_data_include_is_listed() {
        let text = concat!(
            "#![doc = include_str!(\"../README.md\")]\n",
            "pub const G: &str = core::include_str!(\"../tests/assets/test-genesis.json\");\n",
            "fn f() { let b = include_bytes![r#\"blob.bin\"#,]; }\n",
            "// include_str!(\"commented.json\")\n",
            "const S: &str = \"include_str!(\\\"quoted.json\\\")\";\n",
            "const C: &str = include_str!(concat!(env!(\"OUT_DIR\"), \"/x\"));\n",
            "macro_rules! m { ($p:expr) => { include_str!($p) }; }\n",
            "const D: &[u8] = include_bytes!{\"d.bin\"};\n",
        );
        let at = |needle: &str| u32::try_from(text.find(needle).expect("in the text")).expect("small");
        assert_eq!(
            listed(text),
            [
                ("\"../README.md\"".into(), at("include_str!(\"../README.md")),
                ("\"../tests/assets/test-genesis.json\"".into(), at("include_str!(\"../tests")),
                ("r#\"blob.bin\"#".into(), at("include_bytes![")),
                ("\"d.bin\"".into(), at("include_bytes!{")),
            ]
        );
    }

    /// core `include!`: in a body its file "is parsed as an expression": a literal call there is
    /// listed as data (the names' block module and the body's lowering read it whole), as
    /// `include_str!`'s is; an item-position one is listed here too and skipped by work.rs,
    /// which lists it as a module first. `include!(concat!(..))` is no literal and not listed.
    ///
    /// Prints nothing; passes as `data_include::tests::a_literal_include_in_a_body_is_listed ... ok`.
    #[test]
    fn a_literal_include_in_a_body_is_listed() {
        let text = concat!(
            "mod gen { include!(\"rules_gen.rs\"); }\n",
            "fn f() -> u32 { let v = include!(\"gen.in\"); std::include!(\"more.in\"); v }\n",
            "fn g() { include!(concat!(env!(\"OUT_DIR\"), \"/x.rs\")); }\n",
            "fn h() { my_include!(\"not.in\"); }\n",
        );
        let at = |needle: &str| u32::try_from(text.find(needle).expect("in the text")).expect("small");
        assert_eq!(
            listed(text),
            [
                ("\"rules_gen.rs\"".into(), at("include!(\"rules_gen")),
                ("\"gen.in\"".into(), at("include!(\"gen.in")),
                ("\"more.in\"".into(), at("include!(\"more.in")),
            ]
        );
    }

    /// [`DATA_FILE`] is no edition byte (blaze-surface `Edition`: 0 to 3), so a data part is
    /// never outlined as Rust and a Rust file never read as data.
    ///
    /// Prints nothing; passes as `data_include::tests::the_data_byte_is_no_edition ... ok`.
    #[test]
    fn the_data_byte_is_no_edition() {
        assert!(blaze_surface::Edition::from_byte(DATA_FILE).is_none());
        assert!(is_data(DATA_FILE) && !is_data(blaze_surface::Edition::E2024.byte()));
    }

    /// A data file that is not there answers [`absent_out`]: not an empty file's out (creating
    /// an empty file moves the cell) and not the drive's ABSENT (a cell no record defines, on
    /// which a body's read is refused: engine_bodies.rs `missing_data`).
    ///
    /// Prints nothing; passes as `data_include::tests::an_absent_file_has_an_out_of_its_own ... ok`.
    #[test]
    fn an_absent_file_has_an_out_of_its_own() {
        assert_ne!(absent_out(), data_out(b""));
        assert_ne!(absent_out(), blaze_drive::key::ABSENT);
    }
}
