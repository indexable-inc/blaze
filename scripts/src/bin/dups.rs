//! dups: finds duplicated Rust code across the workspace (exact clones, local-α clones,
//! diverged near-duplicates, sub-function duplicated blocks, and optional embedding-based
//! semantic duplicates), with persistent "not a duplicate" dismissals so you can hill-climb
//! duplicate count down to zero (AGENTS.md one-concept-one-home, rule 30).
//!
//!     blaze run -q -p scripts --bin dups -- '{"command": "scan"}'
//!     blaze run -q -p scripts --bin dups -- '{"command": "next"}'
//!     blaze run -q -p scripts --bin dups -- '{"command": "show", "id": "<id>"}'
//!     blaze run -q -p scripts --bin dups -- '{"command": "dismiss", "id": "<id>", "reason": "..."}'
//!     blaze run -q -p scripts --bin dups -- '{"command": "undismiss", "id": "<id>"}'
//!     blaze run -q -p scripts --bin dups -- '{"command": "prune"}'
//!     blaze run -q -p scripts --bin dups -- '{"command": "write"}'
//!     blaze run -q -p scripts --bin dups -- '{"command": "check", "base": "origin/main"}'
//!     blaze run -q -p scripts --bin dups -- '{"command": "findings"}'
//!     blaze run -q -p scripts --bin dups -- selftest

const DOC: &str = r#"dups: find duplicate Rust code and hill-climb it down with persistent dismissals (AGENTS.md one-concept-one-home, rule 30).

    blaze run -q -p scripts --bin dups -- '{"command": "scan"}'
    blaze run -q -p scripts --bin dups -- '{"command": "next"}'
    blaze run -q -p scripts --bin dups -- '{"command": "show", "id": "<id>"}'
    blaze run -q -p scripts --bin dups -- '{"command": "dismiss", "id": "<id>", "reason": "<why>"}'
    blaze run -q -p scripts --bin dups -- '{"command": "undismiss", "id": "<id>"}'
    blaze run -q -p scripts --bin dups -- '{"command": "prune"}'
    blaze run -q -p scripts --bin dups -- '{"command": "write"}'
    blaze run -q -p scripts --bin dups -- '{"command": "check", "base": "<rev>"}'
    blaze run -q -p scripts --bin dups -- '{"command": "findings"}'
    blaze run -q -p scripts --bin dups -- selftest

    command          scan       find duplicates, exclude dismissed pairs, rank by removable lines
                     next       show the #1 highest-impact unreviewed duplicate with source, diff,
                                and the exact dismiss command for hill-climbing
                     show       inspect one group or pair by `id` with source and diff
                     dismiss    mark a pair or every pair of a group as not a duplicate in
                                tools/ratchets/dups-not.json (requires non-empty `reason`)
                     undismiss  remove a dismissal by `id` from tools/ratchets/dups-not.json
                     prune      remove stale dismissals that no longer match in the current tree
                     write      write the per-crate ratchet list tools/ratchets/dups.txt
                     check      hold each crate's unreviewed duplicate count to tools/ratchets/dups.txt
                     findings   print every unreviewed duplicate as a `blaze-findings/1` document
    root             checkout root (".")
    base             commit to compare ratchet rows against in `check`, or null
    dirs             top-level directories to scan (["crates"]; e.g. ["crates", "scripts"])
    crate            restrict output to groups involving this crate, or null
    skip             crate names to skip ([])
    include_examples include files under `examples/` directories (true)
    skip_generated   skip files under `generated/` directories, `generated.rs`, `*_generated.rs` or `*.gen.rs` (true)
    cross_crate_only only keep duplicates spanning 2+ crates (false)
    kinds            detection tiers (["exact", "alpha", "near", "block"])
    item_kinds       syntax items to compare (["fn", "macro"])
    min_lines        minimum item lines (8)
    min_tokens       minimum normalized tokens per item (45)
    threshold        minimum LCS token similarity for `near` (0.78)
    ident_threshold  minimum domain-identifier Jaccard for `near` (0.40)
    block_tokens     minimum matching tokens for `block` clones (60)
    block_min_lines  minimum matching lines for `block` clones (12)
    top              max groups shown in `scan` table (30; 0 for all)
    format           output format for `scan`: "table" or "json" ("table")
    html_out         optional path to write a side-by-side HTML report, or null
    id               group or pair id for `show`, `dismiss`, or `undismiss`
    ids              multiple group or pair ids for batch `dismiss` or `undismiss` ([])
    reason           required explanation for `dismiss`
    include_dismissed include dismissed pairs in `scan` (false)
    dismissals       path to dismissals file ("tools/ratchets/dups-not.json")
    embed_url        optional OpenAI-compatible `/v1/embeddings` URL for `semantic` tier, or null
    embed_model      model name sent to `embed_url`, or null
    embed_mode       text fed to embeddings: "raw" (default) or "alpha" (local-vars renamed)
    embed_threshold  cosine similarity threshold for `semantic` tier (0.96)
    embed_cache      embedding cache path ("target/dups/embed-cache.bin")

Tiers (no blanket identifier blanking — types, methods, fields, and paths stay intact):
  exact     identical normalized token stream (comments, whitespace, lint attrs, visibility, own name stripped)
  alpha     identical structure after consistent local-variable α-renaming ($0, $1, ...); keeps types/fields/methods
  near      diverged copy-paste found by IDF-weighted 5-gram index + exact token LCS + domain-ident overlap
  block     duplicated 60+ token / 12+ line blocks inside otherwise different functions (Winnowing + maximal extend)
  semantic  optional embedding cosine similarity over raw code via `embed_url`
"#;

use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    fmt::Write as _,
    fs,
    hash::{BuildHasherDefault, Hasher},
    path::{Path, PathBuf},
    time::Duration,
};

/// Fast non-cryptographic hasher for in-run integer maps/sets (never persisted; output is sorted).
#[derive(Default)]
struct FxHasher(u64);

impl Hasher for FxHasher {
    #[inline]
    fn finish(&self) -> u64 {
        self.0
    }
    #[inline]
    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 = (self.0.rotate_left(5) ^ u64::from(b)).wrapping_mul(0x517cc1b727220a95);
        }
    }
    #[inline]
    fn write_u32(&mut self, i: u32) {
        self.0 = (self.0.rotate_left(5) ^ u64::from(i)).wrapping_mul(0x517cc1b727220a95);
    }
    #[inline]
    fn write_u64(&mut self, i: u64) {
        self.0 = (self.0.rotate_left(5) ^ i).wrapping_mul(0x517cc1b727220a95);
    }
    #[inline]
    fn write_usize(&mut self, i: usize) {
        self.0 = (self.0.rotate_left(5) ^ (i as u64)).wrapping_mul(0x517cc1b727220a95);
    }
}

type FxMap<K, V> = HashMap<K, V, BuildHasherDefault<FxHasher>>;
type FxSet<T> = HashSet<T, BuildHasherDefault<FxHasher>>;

use blaze_lex::{Cpu, FrontmatterAllowed, TokenKind, tokenize};
use blaze_script::{
    Map, TempDir, Value, fail, fan_out,
    finding::{self, Finding},
    input::{self, Fields, Input, InputError},
    json,
    ratchet::{self, Policy, Tests},
    run_within, universal_newlines,
};
use blaze_sha256::Sha256;

const SCRIPT: &str = "dups";
const RATCHET_LIST: &str = "tools/ratchets/dups.txt";
const DEFAULT_DISMISSALS: &str = "tools/ratchets/dups-not.json";
const DISMISSALS_SCHEMA: &str = "blaze-dups-not/1";
const REPORT_SCHEMA: &str = "blaze-dups/1";
const TEST_DIRS: [&str; 4] = ["tests", "benches", "test_data", "fixtures"];
const TEST_FILES: [&str; 2] = ["tests.rs", "test.rs"];
const ROOT_FILES: [&str; 4] = ["lib.rs", "main.rs", "mod.rs", "build.rs"];
const RATCHET_HEADER: &str = "# Unreviewed duplicate code items outside test code, by crate (its directory under\n\
                              # crates/), excluding dismissed non-duplicates in tools/ratchets/dups-not.json\n\
                              # (AGENTS.md one-concept-one-home, rule 30).\n\
                              # Written by scripts/src/bin/dups.rs (`{\"command\": \"write\"}`).\n\
                              # crate\titems\n";
const RULE_MSG: &str = "AGENTS.md one-concept-one-home (rule 30): each concept has one home; merge duplicates or dismiss false positives with a reason";
const INSTEAD_MSG: &str = "extract one shared helper/type and delete the copy, or run `dups` with `{\"command\": \"dismiss\", \"id\": \"...\", \"reason\": \"...\"}` if distinct";

/// Rust keywords (never treated as local variables or domain identifiers).
const KEYWORDS: [&str; 53] = [
    "as", "async", "await", "break", "const", "continue", "crate", "dyn", "else", "enum", "extern",
    "false", "fn", "for", "if", "impl", "in", "let", "loop", "match", "mod", "move", "mut", "pub",
    "ref", "return", "self", "Self", "static", "struct", "super", "trait", "true", "type", "union",
    "unsafe", "use", "where", "while", "abstract", "become", "box", "do", "final", "macro",
    "override", "priv", "try", "typeof", "unsized", "virtual", "yield", "raw",
];

/// Very common prelude/builtin names excluded from `domain_idents` so trivial wrappers don't share high ident Jaccard.
const COMMON_PRELUDE: [&str; 24] = [
    "Option", "Some", "None", "Result", "Ok", "Err", "Vec", "String", "str", "bool", "u8", "u16",
    "u32", "u64", "u128", "usize", "i8", "i16", "i32", "i64", "i128", "isize", "f32", "f64",
];

/// Trivial outer/inner attributes stripped before comparing tokens.
const STRIP_ATTRS: [&str; 8] = [
    "inline",
    "allow",
    "expect",
    "warn",
    "deny",
    "must_use",
    "cold",
    "track_caller",
];

struct Args {
    command: String,
    root: String,
    base: Option<String>,
    dirs: Vec<String>,
    krate: Option<String>,
    skip: Vec<String>,
    include_examples: bool,
    skip_generated: bool,
    cross_crate_only: bool,
    kinds: Vec<String>,
    item_kinds: Vec<String>,
    min_lines: usize,
    min_tokens: usize,
    threshold: f64,
    ident_threshold: f64,
    block_tokens: usize,
    block_min_lines: usize,
    top: usize,
    format: String,
    html_out: Option<String>,
    id: Option<String>,
    ids: Vec<String>,
    reason: Option<String>,
    include_dismissed: bool,
    dismissals: String,
    embed_url: Option<String>,
    embed_model: Option<String>,
    embed_mode: String,
    embed_threshold: f64,
    embed_cache: String,
}

impl Input for Args {
    const FIELDS: &'static [&'static str] = &[
        "command",
        "root",
        "base",
        "dirs",
        "crate",
        "skip",
        "include_examples",
        "skip_generated",
        "cross_crate_only",
        "kinds",
        "item_kinds",
        "min_lines",
        "min_tokens",
        "threshold",
        "ident_threshold",
        "block_tokens",
        "block_min_lines",
        "top",
        "format",
        "html_out",
        "id",
        "ids",
        "reason",
        "include_dismissed",
        "dismissals",
        "embed_url",
        "embed_model",
        "embed_mode",
        "embed_threshold",
        "embed_cache",
    ];

    fn parse(f: &Fields<'_>) -> Result<Args, InputError> {
        let non_neg = |name: &str, default: i64| -> Result<usize, InputError> {
            let v = f.int(name, Some(default))?;
            usize::try_from(v).map_err(|_| InputError(format!("{} must be >= 0, not {v}", f.at(name))))
        };
        let dirs = {
            let given = f.strs("dirs")?;
            if given.is_empty() { vec!["crates".to_owned()] } else { given }
        };
        let kinds = f.ones_of(
            "kinds",
            &["exact", "alpha", "near", "block", "semantic"],
            &["exact", "alpha", "near", "block"],
        )?;
        let item_kinds = f.ones_of("item_kinds", &["fn", "macro", "struct", "enum"], &["fn", "macro"])?;
        let threshold = f.float("threshold", Some(0.78))?;
        let ident_threshold = f.float("ident_threshold", Some(0.40))?;
        let embed_threshold = f.float("embed_threshold", Some(0.96))?;
        if !(0.1..=1.0).contains(&threshold) {
            return Err(InputError(format!("{} must be in [0.1, 1.0], not {threshold}", f.at("threshold"))));
        }
        if !(0.0..=1.0).contains(&ident_threshold) {
            return Err(InputError(format!(
                "{} must be in [0.0, 1.0], not {ident_threshold}",
                f.at("ident_threshold")
            )));
        }
        if !(0.1..=1.0).contains(&embed_threshold) {
            return Err(InputError(format!(
                "{} must be in [0.1, 1.0], not {embed_threshold}",
                f.at("embed_threshold")
            )));
        }
        Ok(Args {
            command: f.one_of(
                "command",
                &["scan", "next", "show", "dismiss", "undismiss", "prune", "write", "check", "findings"],
                Some("scan"),
            )?,
            root: f.str("root", Some("."))?,
            base: f.str_or_null("base")?,
            dirs,
            krate: f.str_or_null("crate")?,
            skip: f.strs("skip")?,
            include_examples: f.bool("include_examples", Some(true))?,
            skip_generated: f.bool("skip_generated", Some(true))?,
            cross_crate_only: f.bool("cross_crate_only", Some(false))?,
            kinds,
            item_kinds,
            min_lines: non_neg("min_lines", 8)?,
            min_tokens: non_neg("min_tokens", 45)?,
            threshold,
            ident_threshold,
            block_tokens: non_neg("block_tokens", 60)?,
            block_min_lines: non_neg("block_min_lines", 12)?,
            top: non_neg("top", 30)?,
            format: f.one_of("format", &["table", "json"], Some("table"))?,
            html_out: f.str_or_null("html_out")?,
            id: f.str_or_null("id")?,
            ids: f.strs("ids")?,
            reason: f.str_or_null("reason")?,
            include_dismissed: f.bool("include_dismissed", Some(false))?,
            dismissals: f.str("dismissals", Some(DEFAULT_DISMISSALS))?,
            embed_url: f.str_or_null("embed_url")?,
            embed_model: f.str_or_null("embed_model")?,
            embed_mode: f.one_of("embed_mode", &["raw", "alpha"], Some("raw"))?,
            embed_threshold,
            embed_cache: f.str("embed_cache", Some("target/dups/embed-cache.bin"))?,
        })
    }
}

/// Syntax item kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
enum ItemKind {
    Fn,
    Macro,
    Struct,
    Enum,
}

impl ItemKind {
    fn as_str(self) -> &'static str {
        match self {
            ItemKind::Fn => "fn",
            ItemKind::Macro => "macro",
            ItemKind::Struct => "struct",
            ItemKind::Enum => "enum",
        }
    }
}

/// Detection tier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
enum DupTier {
    Exact,
    Alpha,
    Near,
    Block,
    Semantic,
}

impl DupTier {
    fn as_str(self) -> &'static str {
        match self {
            DupTier::Exact => "exact",
            DupTier::Alpha => "alpha",
            DupTier::Near => "near",
            DupTier::Block => "block",
            DupTier::Semantic => "semantic",
        }
    }

    const fn finding_kind(self) -> finding::Kind {
        const RULE: &str = "30 one-concept-one-home";
        const INSTEAD: &str = "merge into one shared helper/type or dismiss with reason in tools/ratchets/dups-not.json";
        match self {
            DupTier::Exact => finding::Kind {
                name: "exact",
                label: "exact duplicate items",
                rule: RULE,
                instead: INSTEAD,
                mechanical: false,
            },
            DupTier::Alpha => finding::Kind {
                name: "alpha",
                label: "alpha-renamed duplicate items",
                rule: RULE,
                instead: INSTEAD,
                mechanical: false,
            },
            DupTier::Near => finding::Kind {
                name: "near",
                label: "near-duplicate items",
                rule: RULE,
                instead: INSTEAD,
                mechanical: false,
            },
            DupTier::Block => finding::Kind {
                name: "block",
                label: "duplicated code blocks",
                rule: RULE,
                instead: INSTEAD,
                mechanical: false,
            },
            DupTier::Semantic => finding::Kind {
                name: "semantic",
                label: "semantically similar items",
                rule: RULE,
                instead: INSTEAD,
                mechanical: false,
            },
        }
    }
}

/// One scanned source file.
struct SourceFile {
    krate: String,
    rel_path: String,
    text: String,
}

/// Raw non-trivia token from `blaze_lex`.
#[derive(Debug, Clone, Copy)]
struct SrcTok {
    kind: SrcTokKind,
    start: usize,
    end: usize,
    line: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SrcTokKind {
    Ident,
    Literal,
    Lifetime,
    Punct,
}

/// An extracted item before global token interning.
struct RawItem {
    krate: String,
    path: String,
    kind: ItemKind,
    name: String,
    canonical_id: String,
    start_line: usize,
    end_line: usize,
    start_byte: usize,
    end_byte: usize,
    raw_tokens: Vec<String>,
    alpha_tokens: Vec<String>,
    /// Line number of each token in `alpha_tokens` (for mapping block matches back to exact lines).
    alpha_lines: Vec<usize>,
    /// Sorted unique domain identifiers (non-keyword, non-local, non-prelude).
    domain_idents: Vec<String>,
    raw_hash: [u8; 32],
    alpha_hash: [u8; 32],
}

/// An extracted item with interned `u32` token streams for fast comparison.
struct Item {
    krate: String,
    path: String,
    kind: ItemKind,
    name: String,
    canonical_id: String,
    start_line: usize,
    end_line: usize,
    start_byte: usize,
    end_byte: usize,
    raw_ids: Vec<u32>,
    alpha_ids: Vec<u32>,
    /// Like `alpha_ids`, but every local-variable occurrence is the single `$V` sentinel so
    /// sub-function Winnowing is invariant to earlier locals in the enclosing function;
    /// bijective α-equivalence is then verified against `raw_ids` during block extension.
    block_ids: Vec<u32>,
    alpha_lines: Vec<usize>,
    domain_ids: Vec<u32>,
    raw_hash: [u8; 32],
    alpha_hash: [u8; 32],
    /// Stable 64-bit hashes of 5-grams of `alpha` token strings (sorted unique not needed here).
    shingles: Vec<u64>,
    /// Structural diversity: unique 4-grams / total 4-grams.
    diversity: f64,
}

impl Item {
    fn lines(&self) -> usize {
        self.end_line.saturating_sub(self.start_line) + 1
    }
}

/// One matched pair of items (or sub-function blocks).
#[derive(Debug, Clone)]
struct MatchPair {
    pair_id: String,
    tier: DupTier,
    a: usize,
    b: usize,
    score: f64,
    ident_sim: f64,
    /// For `Block` matches: exact line ranges and token count.
    block: Option<BlockSpan>,
}

#[derive(Debug, Clone)]
struct BlockSpan {
    a_start_line: usize,
    a_end_line: usize,
    b_start_line: usize,
    b_end_line: usize,
    tokens: usize,
}

/// One clustered duplicate group.
#[derive(Debug, Clone)]
struct DupGroup {
    group_id: String,
    tier: DupTier,
    /// Indices into `items`, sorted by (crate, path, start_line).
    members: Vec<usize>,
    /// Pairs belonging to this group.
    pairs: Vec<MatchPair>,
    crates: Vec<String>,
    score: f64,
    removable_lines: usize,
    /// For `Block` groups (always 2 members with specific line ranges).
    block: Option<BlockSpan>,
}

/// One dismissed pair recorded in `tools/ratchets/dups-not.json`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Dismissal {
    id: String,
    a: String,
    b: String,
    reason: String,
}

fn is_keyword(s: &str) -> bool {
    KEYWORDS.binary_search(&s).is_ok() || KEYWORDS.contains(&s)
}

fn is_prelude(s: &str) -> bool {
    COMMON_PRELUDE.contains(&s)
}

fn hex_short(digest: &[u8; 32], bytes: usize) -> String {
    let mut out = String::with_capacity(bytes * 2);
    for b in &digest[..bytes] {
        let _ = write!(out, "{b:02x}");
    }
    out
}

fn hash_token_strings(tokens: &[String]) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(b"blaze-dups/tokens/1\0");
    for tok in tokens {
        let len = (tok.len() as u32).to_le_bytes();
        h.update(&len);
        h.update(tok.as_bytes());
    }
    h.finish()
}

fn pair_id_for(a_id: &str, b_id: &str, extra: Option<&str>) -> String {
    let (lo, hi) = if a_id <= b_id { (a_id, b_id) } else { (b_id, a_id) };
    let mut h = Sha256::new();
    h.update(b"blaze-dups/pair/1\0");
    h.update(lo.as_bytes());
    h.update(b"\0");
    h.update(hi.as_bytes());
    if let Some(ex) = extra {
        h.update(b"\0");
        h.update(ex.as_bytes());
    }
    hex_short(&h.finish(), 6)
}

fn group_id_for(member_ids: &[&str], extra: Option<&str>) -> String {
    let mut sorted: Vec<&str> = member_ids.to_vec();
    sorted.sort_unstable();
    let mut h = Sha256::new();
    h.update(b"blaze-dups/group/1\0");
    for m in sorted {
        h.update(m.as_bytes());
        h.update(b"\0");
    }
    if let Some(ex) = extra {
        h.update(ex.as_bytes());
    }
    hex_short(&h.finish(), 6)
}

/// Deterministic 64-bit hash of a slice of interned token IDs (for in-run windowing).
fn hash_u32_slice(slice: &[u32]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for &id in slice {
        h ^= u64::from(id);
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

fn compute_line_starts(text: &str) -> Vec<usize> {
    let mut starts = vec![0usize];
    for (i, &b) in text.as_bytes().iter().enumerate() {
        if b == b'\n' {
            starts.push(i + 1);
        }
    }
    starts
}

fn line_at(line_starts: &[usize], byte: usize) -> usize {
    match line_starts.binary_search(&byte) {
        Ok(i) => i + 1,
        Err(i) => i,
    }
}

/// Tokenize source into non-trivia `SrcTok`s using `blaze-lex`.
fn lex_source(text: &str) -> Vec<SrcTok> {
    let line_starts = compute_line_starts(text);
    let mut out = Vec::with_capacity(text.len() / 6);
    let mut offset = 0usize;
    for tok in tokenize(text, FrontmatterAllowed::No, Cpu::BASELINE) {
        let len = tok.len as usize;
        let start = offset;
        let end = offset + len;
        offset = end;
        let kind = match tok.kind {
            TokenKind::Whitespace
            | TokenKind::LineComment { .. }
            | TokenKind::BlockComment { .. }
            | TokenKind::Frontmatter { .. }
            | TokenKind::Eof => continue,
            TokenKind::Ident | TokenKind::RawIdent | TokenKind::ForcedKeywordIdent => SrcTokKind::Ident,
            TokenKind::Literal { .. } => SrcTokKind::Literal,
            TokenKind::Lifetime { .. } | TokenKind::RawLifetime => SrcTokKind::Lifetime,
            _ => SrcTokKind::Punct,
        };
        let line = line_at(&line_starts, start);
        out.push(SrcTok { kind, start, end, line });
    }
    out
}

fn tok_str<'a>(text: &'a str, tok: SrcTok) -> &'a str {
    &text[tok.start..tok.end]
}

/// Find the closing delimiter matching `tokens[open]` (`(`, `[`, `{`).
fn match_delim(text: &str, tokens: &[SrcTok], open: usize) -> Option<usize> {
    let open_ch = tok_str(text, tokens[open]);
    let close_ch = match open_ch {
        "(" => ")",
        "[" => "]",
        "{" => "}",
        _ => return None,
    };
    let mut depth = 0usize;
    for (i, &tok) in tokens.iter().enumerate().skip(open) {
        let s = tok_str(text, tok);
        if s == open_ch {
            depth += 1;
        } else if s == close_ch {
            depth -= 1;
            if depth == 0 {
                return Some(i);
            }
        }
    }
    None
}

/// Check if an attribute `#[...]` at `pound_idx` is a trivial lint/inline/doc attribute to strip.
fn is_ignorable_attr(text: &str, tokens: &[SrcTok], pound_idx: usize) -> Option<usize> {
    if tok_str(text, tokens[pound_idx]) != "#" {
        return None;
    }
    let mut next = pound_idx + 1;
    if next < tokens.len() && tok_str(text, tokens[next]) == "!" {
        next += 1;
    }
    if next >= tokens.len() || tok_str(text, tokens[next]) != "[" {
        return None;
    }
    let close = match_delim(text, tokens, next)?;
    let inner = next + 1;
    if inner < close && tokens[inner].kind == SrcTokKind::Ident {
        let name = tok_str(text, tokens[inner]);
        if name == "doc" || STRIP_ATTRS.contains(&name) {
            return Some(close + 1);
        }
        // `#[clippy::...]` or `#[blaze::...]`
        if inner + 2 < close && tok_str(text, tokens[inner + 1]) == ":" && tok_str(text, tokens[inner + 2]) == ":" {
            if name == "clippy" || name == "blaze" {
                return Some(close + 1);
            }
        }
    }
    None
}

/// Extract clean header text between `start..end` tokens (e.g. `impl Foo` -> `impl Foo`).
fn compact_header(text: &str, tokens: &[SrcTok], start: usize, end: usize) -> String {
    let mut out = String::new();
    let mut prev_ident = false;
    for &t in &tokens[start..end] {
        let s = tok_str(text, t);
        let cur_ident = t.kind == SrcTokKind::Ident || t.kind == SrcTokKind::Lifetime;
        if prev_ident && cur_ident {
            out.push(' ');
        }
        out.push_str(s);
        prev_ident = cur_ident;
    }
    if out.len() > 64 {
        let mut trunc = 64;
        while !out.is_char_boundary(trunc) && trunc > 0 {
            trunc -= 1;
        }
        out.truncate(trunc);
    }
    out
}

/// Collect local variable bindings declared in a function's parameter list and body.
fn collect_locals(text: &str, tokens: &[SrcTok], param_open: usize, param_close: usize, body_open: usize, body_close: usize) -> HashSet<String> {
    let mut locals = HashSet::new();
    let mut add_if_local = |s: &str| {
        let first = s.as_bytes().first().copied().unwrap_or(0);
        if (first.is_ascii_lowercase() || first == b'_') && s != "_" && s != "self" && !is_keyword(s) {
            locals.insert(s.to_owned());
        }
    };

    // 1. Parameters: top-level `ident :` or `mut ident :` inside `( ... )`.
    let mut depth = 0usize;
    let mut i = param_open + 1;
    while i < param_close {
        let s = tok_str(text, tokens[i]);
        match s {
            "(" | "[" | "{" | "<" => depth += 1,
            ")" | "]" | "}" | ">" => depth = depth.saturating_sub(1),
            _ => {
                if depth == 0 && tokens[i].kind == SrcTokKind::Ident && i + 1 < param_close && tok_str(text, tokens[i + 1]) == ":" {
                    let is_colons = i + 2 < param_close && tok_str(text, tokens[i + 2]) == ":";
                    if !is_colons {
                        add_if_local(s);
                    }
                }
            }
        }
        i += 1;
    }

    // 2. Body bindings: `let [mut] x`, `for x in`, `|x, y|`, `Some(x)` / `Ok(x)` / `Err(x)` in `if let` / `while let` / `match` arms.
    let mut j = body_open + 1;
    while j < body_close {
        let s = tok_str(text, tokens[j]);
        if s == "let" {
            // Scan pattern until `:` or `=` or `;` at depth 0.
            let mut k = j + 1;
            let mut pdepth = 0usize;
            while k < body_close {
                let ks = tok_str(text, tokens[k]);
                if pdepth == 0 && (ks == "=" || ks == ";" || (ks == ":" && tok_str_opt(text, tokens, k + 1) != Some(":"))) {
                    break;
                }
                match ks {
                    "(" | "[" | "{" => pdepth += 1,
                    ")" | "]" | "}" => pdepth = pdepth.saturating_sub(1),
                    _ => {
                        if tokens[k].kind == SrcTokKind::Ident {
                            let prev = tok_str_opt(text, tokens, k.wrapping_sub(1));
                            let next = tok_str_opt(text, tokens, k + 1);
                            let is_path = prev == Some(":") || next == Some(":");
                            // Skip struct field names in `Foo { field: local }` (`field` followed by `:`)
                            let is_field_key = next == Some(":") && tok_str_opt(text, tokens, k + 2) != Some(":");
                            if !is_path && !is_field_key {
                                add_if_local(ks);
                            }
                        }
                    }
                }
                k += 1;
            }
            j = k;
            continue;
        }
        if s == "for" {
            let mut k = j + 1;
            while k < body_close && tok_str(text, tokens[k]) != "in" {
                if tokens[k].kind == SrcTokKind::Ident {
                    add_if_local(tok_str(text, tokens[k]));
                }
                k += 1;
            }
            j = k;
            continue;
        }
        // Simple closure parameters `|a, b|` (heuristic: not preceded by expression-ending ident/literal/`)`/`]`/`}`).
        if s == "|" {
            let prev_is_expr = j > body_open + 1 && matches!(
                tokens[j - 1].kind,
                SrcTokKind::Ident | SrcTokKind::Literal
            ) && !is_keyword(tok_str(text, tokens[j - 1]))
                || tok_str_opt(text, tokens, j - 1).is_some_and(|p| matches!(p, ")" | "]" | "}"));
            if !prev_is_expr {
                let mut k = j + 1;
                let mut valid = true;
                let mut found = Vec::new();
                while k < body_close && tok_str(text, tokens[k]) != "|" {
                    let ks = tok_str(text, tokens[k]);
                    if matches!(ks, ";" | "{" | "}" | "=>" | "=") {
                        valid = false;
                        break;
                    }
                    if tokens[k].kind == SrcTokKind::Ident {
                        let prev = tok_str_opt(text, tokens, k.wrapping_sub(1));
                        let next = tok_str_opt(text, tokens, k + 1);
                        if prev != Some(":") && next != Some(":") {
                            found.push(ks);
                        }
                    }
                    k += 1;
                }
                if valid && k < body_close && tok_str(text, tokens[k]) == "|" && k - j <= 24 {
                    for name in found {
                        add_if_local(name);
                    }
                    j = k + 1;
                    continue;
                }
            }
        }
        j += 1;
    }
    locals
}

fn tok_str_opt<'a>(text: &'a str, tokens: &[SrcTok], idx: usize) -> Option<&'a str> {
    tokens.get(idx).map(|&t| tok_str(text, t))
}

/// Normalize a slice of tokens into `(raw_tokens, alpha_tokens, alpha_lines, domain_idents)`.
/// Strips ignorable attributes and trailing commas before closing delimiters.
fn normalize_tokens(
    text: &str,
    tokens: &[SrcTok],
    range: std::ops::Range<usize>,
    own_name_idx: Option<usize>,
    locals: &HashSet<String>,
    body_start: usize,
) -> (Vec<String>, Vec<String>, Vec<usize>, Vec<String>) {
    let mut raw = Vec::with_capacity(range.len());
    let mut alpha = Vec::with_capacity(range.len());
    let mut lines = Vec::with_capacity(range.len());
    let mut domain = BTreeSet::new();
    let mut rename: HashMap<&str, String> = HashMap::new();

    let mut i = range.start;
    while i < range.end {
        if let Some(after_attr) = is_ignorable_attr(text, tokens, i) {
            if after_attr <= range.end {
                i = after_attr;
                continue;
            }
        }
        let tok = tokens[i];
        let s = tok_str(text, tok);

        // Strip trailing comma immediately before `)`, `]`, `}`.
        if s == "," && i + 1 < range.end && matches!(tok_str(text, tokens[i + 1]), ")" | "]" | "}") {
            i += 1;
            continue;
        }

        // Replace item's own name with `$FN` so renamed copies still match as `exact`.
        if own_name_idx == Some(i) {
            raw.push("$SELF_ITEM".to_owned());
            alpha.push("$SELF_ITEM".to_owned());
            lines.push(tok.line);
            i += 1;
            continue;
        }

        raw.push(s.to_owned());

        let is_local_occ = if tok.kind == SrcTokKind::Ident && locals.contains(s) {
            let prev = if i > range.start { Some(tok_str(text, tokens[i - 1])) } else { None };
            let next = if i + 1 < range.end { Some(tok_str(text, tokens[i + 1])) } else { None };
            let next2 = if i + 2 < range.end { Some(tok_str(text, tokens[i + 2])) } else { None };
            let after_dot_or_path = matches!(prev, Some(".") | Some(":"));
            let before_path_or_macro = next == Some("!") || (next == Some(":") && next2 == Some(":"));
            // In body, `field: expr` (single colon not after `let`) is a struct field name, not a local use.
            let is_struct_field = i >= body_start
                && next == Some(":")
                && next2 != Some(":")
                && matches!(prev, Some("{") | Some(","));
            !after_dot_or_path && !before_path_or_macro && !is_struct_field
        } else {
            false
        };

        if is_local_occ {
            let next_id = rename.len();
            let mapped = rename.entry(s).or_insert_with(|| format!("${next_id}"));
            alpha.push(mapped.clone());
        } else {
            alpha.push(s.to_owned());
            if tok.kind == SrcTokKind::Ident && !is_keyword(s) && !is_prelude(s) && s.len() > 1 {
                domain.insert(s.to_owned());
            }
        }
        lines.push(tok.line);
        i += 1;
    }

    (raw, alpha, lines, domain.into_iter().collect())
}

/// Extract all non-test items (`fn`, `macro`, `struct`, `enum`) from one source file.
fn extract_items(
    file: &SourceFile,
    test_lines: &[bool],
    wanted_kinds: &HashSet<ItemKind>,
    min_lines: usize,
    min_tokens: usize,
) -> Vec<RawItem> {
    let tokens = lex_source(&file.text);
    let mut items = Vec::new();
    let mut name_counts: HashMap<String, usize> = HashMap::new();
    // Scope stack: `(closing_brace_tok_idx, scope_label)`.
    let mut scopes: Vec<(usize, String)> = Vec::new();

    let mut i = 0usize;
    while i < tokens.len() {
        while scopes.last().is_some_and(|&(end_idx, _)| i > end_idx) {
            scopes.pop();
        }
        let s = tok_str(&file.text, tokens[i]);

        // Enter `mod <name> { ... }`
        if s == "mod" && i + 2 < tokens.len() && tokens[i + 1].kind == SrcTokKind::Ident {
            if tok_str(&file.text, tokens[i + 2]) == "{" {
                if let Some(close) = match_delim(&file.text, &tokens, i + 2) {
                    let mod_name = tok_str(&file.text, tokens[i + 1]);
                    scopes.push((close, format!("mod {mod_name}")));
                    i += 3;
                    continue;
                }
            }
        }

        // Enter `impl ... { ... }`
        if s == "impl" {
            let mut k = i + 1;
            let mut angle = 0usize;
            while k < tokens.len() {
                let ks = tok_str(&file.text, tokens[k]);
                if ks == "<" {
                    angle += 1;
                } else if ks == ">" {
                    angle = angle.saturating_sub(1);
                } else if ks == "{" && angle == 0 {
                    if let Some(close) = match_delim(&file.text, &tokens, k) {
                        let label = compact_header(&file.text, &tokens, i, k);
                        scopes.push((close, label));
                        i = k + 1;
                    }
                    break;
                } else if ks == ";" && angle == 0 {
                    break;
                }
                k += 1;
            }
            if k < tokens.len() && i == k + 1 {
                continue;
            }
        }

        // Extract `fn <name>(...) ... { ... }`
        if s == "fn" && i + 2 < tokens.len() && tokens[i + 1].kind == SrcTokKind::Ident {
            // Skip `fn(...)` function pointer types (where `(` immediately follows `fn`).
            let name_idx = i + 1;
            let fn_name = tok_str(&file.text, tokens[name_idx]);
            // Find `(` for params (skipping optional generics `<...>`).
            let mut k = name_idx + 1;
            if k < tokens.len() && tok_str(&file.text, tokens[k]) == "<" {
                let mut angle = 0usize;
                while k < tokens.len() {
                    let ks = tok_str(&file.text, tokens[k]);
                    if ks == "<" {
                        angle += 1;
                    } else if ks == ">" {
                        angle = angle.saturating_sub(1);
                        if angle == 0 {
                            k += 1;
                            break;
                        }
                    }
                    k += 1;
                }
            }
            if k < tokens.len() && tok_str(&file.text, tokens[k]) == "(" {
                if let Some(param_close) = match_delim(&file.text, &tokens, k) {
                    // Find body `{` before `;`.
                    let mut b = param_close + 1;
                    let mut found_body = None;
                    while b < tokens.len() {
                        let bs = tok_str(&file.text, tokens[b]);
                        if bs == "{" {
                            found_body = Some(b);
                            break;
                        }
                        if bs == ";" {
                            break;
                        }
                        b += 1;
                    }
                    if let Some(body_open) = found_body {
                        if let Some(body_close) = match_delim(&file.text, &tokens, body_open) {
                            let start_line = tokens[i].line;
                            let end_line = tokens[body_close].line;
                            let is_test = test_lines.get(start_line.saturating_sub(1)).copied().unwrap_or(false);
                            if !is_test && wanted_kinds.contains(&ItemKind::Fn) {
                                let lines_len = end_line.saturating_sub(start_line) + 1;
                                if lines_len >= min_lines {
                                    let locals = collect_locals(&file.text, &tokens, k, param_close, body_open, body_close);
                                    let (raw_tokens, alpha_tokens, alpha_lines, domain_idents) =
                                        normalize_tokens(&file.text, &tokens, i..body_close + 1, Some(name_idx), &locals, body_open);
                                    if alpha_tokens.len() >= min_tokens {
                                        let qualified = qualify_name(&scopes, fn_name);
                                        let canonical_id = make_canonical_id(&file.rel_path, ItemKind::Fn, &qualified, &mut name_counts);
                                        let raw_hash = hash_token_strings(&raw_tokens);
                                        let alpha_hash = hash_token_strings(&alpha_tokens);
                                        items.push(RawItem {
                                            krate: file.krate.clone(),
                                            path: file.rel_path.clone(),
                                            kind: ItemKind::Fn,
                                            name: qualified,
                                            canonical_id,
                                            start_line,
                                            end_line,
                                            start_byte: tokens[i].start,
                                            end_byte: tokens[body_close].end,
                                            raw_tokens,
                                            alpha_tokens,
                                            alpha_lines,
                                            domain_idents,
                                            raw_hash,
                                            alpha_hash,
                                        });
                                    }
                                }
                            }
                            // Continue scanning inside the body? Skip nested fn bodies only if test;
                            // actually skipping past `body_close` avoids nested helper fns overlapping their parent!
                            i = body_close + 1;
                            continue;
                        }
                    }
                }
            }
        }

        // Extract `macro_rules! <name> { ... }`
        if s == "macro_rules"
            && i + 3 < tokens.len()
            && tok_str(&file.text, tokens[i + 1]) == "!"
            && tokens[i + 2].kind == SrcTokKind::Ident
            && matches!(tok_str(&file.text, tokens[i + 3]), "{" | "(" | "[")
        {
            if let Some(close) = match_delim(&file.text, &tokens, i + 3) {
                let start_line = tokens[i].line;
                let end_line = tokens[close].line;
                let is_test = test_lines.get(start_line.saturating_sub(1)).copied().unwrap_or(false);
                if !is_test && wanted_kinds.contains(&ItemKind::Macro) {
                    let lines_len = end_line.saturating_sub(start_line) + 1;
                    if lines_len >= min_lines {
                        let empty = HashSet::new();
                        let (raw_tokens, alpha_tokens, alpha_lines, domain_idents) =
                            normalize_tokens(&file.text, &tokens, i..close + 1, Some(i + 2), &empty, i + 3);
                        if alpha_tokens.len() >= min_tokens {
                            let mname = tok_str(&file.text, tokens[i + 2]);
                            let qualified = qualify_name(&scopes, mname);
                            let canonical_id = make_canonical_id(&file.rel_path, ItemKind::Macro, &qualified, &mut name_counts);
                            let raw_hash = hash_token_strings(&raw_tokens);
                            let alpha_hash = hash_token_strings(&alpha_tokens);
                            items.push(RawItem {
                                krate: file.krate.clone(),
                                path: file.rel_path.clone(),
                                kind: ItemKind::Macro,
                                name: qualified,
                                canonical_id,
                                start_line,
                                end_line,
                                start_byte: tokens[i].start,
                                end_byte: tokens[close].end,
                                raw_tokens,
                                alpha_tokens,
                                alpha_lines,
                                domain_idents,
                                raw_hash,
                                alpha_hash,
                            });
                        }
                    }
                }
                i = close + 1;
                continue;
            }
        }

        // Optional `struct <Name> { ... }` / `enum <Name> { ... }`
        if (s == "struct" || s == "enum") && i + 2 < tokens.len() && tokens[i + 1].kind == SrcTokKind::Ident {
            let kind = if s == "struct" { ItemKind::Struct } else { ItemKind::Enum };
            if wanted_kinds.contains(&kind) {
                let mut b = i + 2;
                let mut found_brace = None;
                while b < tokens.len() {
                    let bs = tok_str(&file.text, tokens[b]);
                    if bs == "{" {
                        found_brace = Some(b);
                        break;
                    }
                    if bs == ";" {
                        break;
                    }
                    b += 1;
                }
                if let Some(open) = found_brace {
                    if let Some(close) = match_delim(&file.text, &tokens, open) {
                        let start_line = tokens[i].line;
                        let end_line = tokens[close].line;
                        let is_test = test_lines.get(start_line.saturating_sub(1)).copied().unwrap_or(false);
                        if !is_test && end_line.saturating_sub(start_line) + 1 >= min_lines {
                            let empty = HashSet::new();
                            let (raw_tokens, alpha_tokens, alpha_lines, domain_idents) =
                                normalize_tokens(&file.text, &tokens, i..close + 1, Some(i + 1), &empty, open);
                            if alpha_tokens.len() >= min_tokens {
                                let tname = tok_str(&file.text, tokens[i + 1]);
                                let qualified = qualify_name(&scopes, tname);
                                let canonical_id = make_canonical_id(&file.rel_path, kind, &qualified, &mut name_counts);
                                let raw_hash = hash_token_strings(&raw_tokens);
                                let alpha_hash = hash_token_strings(&alpha_tokens);
                                items.push(RawItem {
                                    krate: file.krate.clone(),
                                    path: file.rel_path.clone(),
                                    kind,
                                    name: qualified,
                                    canonical_id,
                                    start_line,
                                    end_line,
                                    start_byte: tokens[i].start,
                                    end_byte: tokens[close].end,
                                    raw_tokens,
                                    alpha_tokens,
                                    alpha_lines,
                                    domain_idents,
                                    raw_hash,
                                    alpha_hash,
                                });
                            }
                        }
                        i = close + 1;
                        continue;
                    }
                }
            }
        }

        i += 1;
    }

    items
}

fn qualify_name(scopes: &[(usize, String)], leaf: &str) -> String {
    if scopes.is_empty() {
        leaf.to_owned()
    } else {
        let mut out = String::new();
        for (_, label) in scopes {
            out.push_str(label);
            out.push_str("::");
        }
        out.push_str(leaf);
        out
    }
}

fn make_canonical_id(path: &str, kind: ItemKind, name: &str, counts: &mut HashMap<String, usize>) -> String {
    let base = format!("{path}::{}::{name}", kind.as_str());
    let entry = counts.entry(base.clone()).or_insert(0);
    *entry += 1;
    if *entry == 1 { base } else { format!("{base}#{}", *entry) }
}

/// Walk `dirs` under `root`, skipping test files & test modules (using `blaze_script::ratchet::Tests`).
fn collect_sources(root: &Path, dirs: &[String], skip: &HashSet<String>, include_examples: bool, skip_generated: bool, tests: &Tests) -> Vec<(SourceFile, Vec<bool>)> {
    let workspace_members = fs::read_to_string(root.join("Cargo.toml"))
        .ok()
        .map(|text| ratchet::Members::of(&text));
    let mut all_paths: Vec<Vec<String>> = Vec::new();
    for dir in dirs {
        let top = root.join(dir);
        if !top.is_dir() {
            fail(SCRIPT, &format!("directory {} does not exist", top.display()));
        }
        walk_rs_files(&top, &mut vec![dir.clone()], include_examples, skip_generated, &mut all_paths);
    }
    all_paths.sort();

    // Filter `crates/` to workspace members when Cargo.toml lists them (honoring globs & exclude).
    if let Some(ref members) = workspace_members {
        if !members.globs.is_empty() {
            all_paths.retain(|parts| {
                if parts.first().map(String::as_str) != Some("crates") || parts.len() < 2 {
                    return true;
                }
                let prefix = format!("{}/{}", parts[0], parts[1]);
                members.holds(&prefix)
            });
        }
    }

    let known: BTreeSet<String> = all_paths.iter().map(|p| p.join("/")).collect();
    let mut manifests: BTreeMap<Vec<String>, bool> = BTreeMap::new();
    let mut work: Vec<(Vec<String>, String, String, PathBuf)> = Vec::with_capacity(all_paths.len());
    for parts in all_paths {
        let rel = parts.join("/");
        let krate = crate_name_of(root, &parts, &mut manifests);
        if skip.contains(&krate) {
            continue;
        }
        let full = parts.iter().fold(root.to_path_buf(), |p, s| p.join(s));
        work.push((parts, rel, krate, full));
    }

    // Parallel file read + test-line scan across cores (`Tests::of` runs regexes per line).
    let jobs = std::thread::available_parallelism().map_or(4, std::num::NonZeroUsize::get).min(16);
    let mut test_files: BTreeSet<String> = BTreeSet::new();
    let mut children: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut loaded: Vec<(Vec<String>, String, String, Vec<bool>)> = Vec::with_capacity(work.len());

    fan_out(
        jobs,
        &work,
        |(parts, rel, krate, full)| {
            let Ok(bytes) = fs::read(full) else {
                fail(SCRIPT, &format!("{} could not be read", full.display()));
            };
            let text = universal_newlines(String::from_utf8_lossy(&bytes).into_owned());
            let found = tests.of(&text);
            (parts.clone(), rel.clone(), krate.clone(), text, found)
        },
        |(parts, rel, krate, text, found)| {
            if is_test_path(&parts) || found.whole {
                test_files.insert(rel.clone());
            }
            for (mod_name, in_test) in &found.mods {
                for target in module_files(&parts, mod_name) {
                    if !known.contains(&target) {
                        continue;
                    }
                    if *in_test {
                        test_files.insert(target.clone());
                    }
                    children.entry(rel.clone()).or_default().push(target);
                }
            }
            loaded.push((parts, krate, text, found.test));
        },
    );

    let mut todo: Vec<String> = test_files.iter().cloned().collect();
    while let Some(path) = todo.pop() {
        for child in children.get(&path).into_iter().flatten() {
            if test_files.insert(child.clone()) {
                todo.push(child.clone());
            }
        }
    }

    loaded
        .into_iter()
        .filter_map(|(parts, krate, text, tlines)| {
            let rel = parts.join("/");
            if test_files.contains(&rel) {
                None
            } else {
                Some((SourceFile { krate, rel_path: rel, text }, tlines))
            }
        })
        .collect()
}

fn walk_rs_files(dir: &Path, prefix: &mut Vec<String>, include_examples: bool, skip_generated: bool, out: &mut Vec<Vec<String>>) {
    let Ok(entries) = fs::read_dir(dir) else { return };
    let mut names: Vec<String> = entries
        .filter_map(Result::ok)
        .filter_map(|e| e.file_name().into_string().ok())
        .collect();
    names.sort();
    for name in names {
        if name == "target" || name == ".git" {
            continue;
        }
        let path = dir.join(&name);
        let Ok(meta) = fs::symlink_metadata(&path) else { continue };
        if meta.file_type().is_symlink() {
            continue;
        }
        if meta.is_dir() {
            if TEST_DIRS.contains(&name.as_str())
                || (!include_examples && name == "examples")
                || (skip_generated && name == "generated")
            {
                continue;
            }
            prefix.push(name);
            walk_rs_files(&path, prefix, include_examples, skip_generated, out);
            prefix.pop();
        } else if name.ends_with(".rs") {
            if skip_generated && (name == "generated.rs" || name.ends_with("_generated.rs") || name.ends_with(".gen.rs")) {
                continue;
            }
            let mut p = prefix.clone();
            p.push(name);
            out.push(p);
        }
    }
}

fn is_test_path(parts: &[String]) -> bool {
    if parts.iter().any(|p| TEST_DIRS.contains(&p.as_str())) {
        return true;
    }
    let Some(file) = parts.last().map(String::as_str) else { return false };
    TEST_FILES.contains(&file) || file.ends_with("_tests.rs") || file.ends_with("_test.rs")
}

fn module_files(parts: &[String], name: &str) -> Vec<String> {
    let Some((file, dir)) = parts.split_last() else { return Vec::new() };
    let is_root = ROOT_FILES.contains(&file.as_str()) || dir.last().map(String::as_str) == Some("bin");
    let mut base = dir.to_vec();
    if !is_root {
        base.push(file.trim_end_matches(".rs").to_owned());
    }
    let prefix = if base.is_empty() { String::new() } else { format!("{}/", base.join("/")) };
    vec![format!("{prefix}{name}.rs"), format!("{prefix}{name}/mod.rs")]
}

fn crate_name_of(root: &Path, parts: &[String], cache: &mut BTreeMap<Vec<String>, bool>) -> String {
    let mut dir = parts[..parts.len().saturating_sub(1)].to_vec();
    while dir.len() >= 2 {
        let has = *cache.entry(dir.clone()).or_insert_with(|| {
            dir.iter().fold(root.to_path_buf(), |p, s| p.join(s)).join("Cargo.toml").is_file()
        });
        if has {
            return if dir[0] == "crates" { dir[1..].join("/") } else { dir.join("/") };
        }
        dir.pop();
    }
    parts.first().cloned().unwrap_or_else(|| "root".to_owned())
}

/// Intern tokens across all extracted items and precompute 5-gram shingles + diversity.
/// Returns `(items, local_sentinel_id, vocab)`.
fn intern_items(raw_items: Vec<RawItem>) -> (Vec<Item>, u32, Vec<String>) {
    let mut dict: HashMap<String, u32> = HashMap::new();
    let mut vocab: Vec<String> = Vec::new();
    let mut intern = |s: &str| -> u32 {
        if let Some(&id) = dict.get(s) {
            id
        } else {
            let id = vocab.len() as u32;
            vocab.push(s.to_owned());
            dict.insert(s.to_owned(), id);
            id
        }
    };
    let local_sentinel = intern("$V");

    let items = raw_items
        .into_iter()
        .map(|r| {
            let raw_ids: Vec<u32> = r.raw_tokens.iter().map(|s| intern(s)).collect();
            let alpha_ids: Vec<u32> = r.alpha_tokens.iter().map(|s| intern(s)).collect();
            let block_ids: Vec<u32> = r
                .alpha_tokens
                .iter()
                .zip(alpha_ids.iter())
                .map(|(s, &aid)| {
                    if s.starts_with('$') && s != "$SELF_ITEM" {
                        local_sentinel
                    } else {
                        aid
                    }
                })
                .collect();
            let mut domain_ids: Vec<u32> = r.domain_idents.iter().map(|s| intern(s)).collect();
            domain_ids.sort_unstable();
            domain_ids.dedup();

            // 5-gram shingles over interned `alpha_ids` (fast u32 mix; only used for in-run indexing).
            let mut shingles = Vec::new();
            if alpha_ids.len() >= 5 {
                let mut seen: FxSet<u64> = FxSet::default();
                seen.reserve(alpha_ids.len());
                for w in alpha_ids.windows(5) {
                    let h = hash_u32_slice(w);
                    if seen.insert(h) {
                        shingles.push(h);
                    }
                }
            }
            // Structural diversity: unique 4-grams / total 4-grams.
            let diversity = token_diversity(&alpha_ids);

            Item {
                krate: r.krate,
                path: r.path,
                kind: r.kind,
                name: r.name,
                canonical_id: r.canonical_id,
                start_line: r.start_line,
                end_line: r.end_line,
                start_byte: r.start_byte,
                end_byte: r.end_byte,
                raw_ids,
                alpha_ids,
                block_ids,
                alpha_lines: r.alpha_lines,
                domain_ids,
                raw_hash: r.raw_hash,
                alpha_hash: r.alpha_hash,
                shingles,
                diversity,
            }
        })
        .collect();
    (items, local_sentinel, vocab)
}

fn token_diversity(tokens: &[u32]) -> f64 {
    if tokens.len() < 4 {
        return 1.0;
    }
    let total = tokens.len() - 3;
    let mut seen: FxSet<u64> = FxSet::default();
    seen.reserve(total);
    for w in tokens.windows(4) {
        seen.insert(hash_u32_slice(w));
    }
    seen.len() as f64 / total as f64
}

fn sorted_jaccard(a: &[u32], b: &[u32]) -> f64 {
    if a.is_empty() && b.is_empty() {
        return 1.0;
    }
    let (mut i, mut j, mut inter) = (0usize, 0usize, 0usize);
    while i < a.len() && j < b.len() {
        match a[i].cmp(&b[j]) {
            std::cmp::Ordering::Less => i += 1,
            std::cmp::Ordering::Greater => j += 1,
            std::cmp::Ordering::Equal => {
                inter += 1;
                i += 1;
                j += 1;
            }
        }
    }
    let union = a.len() + b.len() - inter;
    if union == 0 { 1.0 } else { inter as f64 / union as f64 }
}

/// Exact LCS length between two interned token sequences, bounded in cost (rule 18).
/// Strips common prefix & suffix first; for very long sequences (> 900 tokens after trimming),
/// computes LCS over 3-gram hashes at stride 2 so worst-case work is strictly bounded.
fn lcs_similarity(a: &[u32], b: &[u32]) -> f64 {
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    let total = a.len() + b.len();
    let mut pref = 0usize;
    let min_len = a.len().min(b.len());
    while pref < min_len && a[pref] == b[pref] {
        pref += 1;
    }
    let mut suff = 0usize;
    while suff < min_len - pref && a[a.len() - 1 - suff] == b[b.len() - 1 - suff] {
        suff += 1;
    }
    let ma = &a[pref..a.len() - suff];
    let mb = &b[pref..b.len() - suff];
    if ma.is_empty() || mb.is_empty() {
        return (2 * (pref + suff)) as f64 / total as f64;
    }

    let mid_lcs = if ma.len().saturating_mul(mb.len()) <= 400_000 {
        lcs_dp(ma, mb)
    } else {
        // Bounded fallback: stride-2 3-gram sequence DP (never unbounded O(N^2) on huge functions).
        let ca: Vec<u64> = ma.windows(3).step_by(2).map(hash_u32_slice).collect();
        let cb: Vec<u64> = mb.windows(3).step_by(2).map(hash_u32_slice).collect();
        let lcs_chunks = lcs_dp_u64(&ca, &cb);
        (lcs_chunks * 2).min(ma.len().min(mb.len()))
    };
    (2 * (pref + suff + mid_lcs)) as f64 / total as f64
}

fn lcs_dp(a: &[u32], b: &[u32]) -> usize {
    let (short, long) = if a.len() <= b.len() { (a, b) } else { (b, a) };
    let mut prev = vec![0u32; short.len() + 1];
    let mut curr = vec![0u32; short.len() + 1];
    for &x in long {
        curr[0] = 0;
        for (j, &y) in short.iter().enumerate() {
            curr[j + 1] = if x == y {
                prev[j] + 1
            } else {
                prev[j + 1].max(curr[j])
            };
        }
        std::mem::swap(&mut prev, &mut curr);
    }
    prev[short.len()] as usize
}

fn lcs_dp_u64(a: &[u64], b: &[u64]) -> usize {
    let (short, long) = if a.len() <= b.len() { (a, b) } else { (b, a) };
    let mut prev = vec![0u32; short.len() + 1];
    let mut curr = vec![0u32; short.len() + 1];
    for &x in long {
        curr[0] = 0;
        for (j, &y) in short.iter().enumerate() {
            curr[j + 1] = if x == y {
                prev[j] + 1
            } else {
                prev[j + 1].max(curr[j])
            };
        }
        std::mem::swap(&mut prev, &mut curr);
    }
    prev[short.len()] as usize
}

fn items_overlap(a: &Item, b: &Item) -> bool {
    a.path == b.path && a.start_line <= b.end_line && b.start_line <= a.end_line
}

/// Run all requested detection tiers and return all raw matched pairs.
fn detect_pairs(
    args: &Args,
    root: &Path,
    items: &[Item],
    local_sentinel: u32,
    vocab: &[String],
    sources: &HashMap<String, String>,
) -> Vec<MatchPair> {
    let want = |k: &str| args.kinds.iter().any(|s| s == k);
    let mut pairs = Vec::new();
    let mut matched_item_pairs: HashSet<(usize, usize)> = HashSet::new();

    // 1. Exact clones (Type-1)
    if want("exact") {
        let mut by_hash: BTreeMap<(ItemKind, [u8; 32]), Vec<usize>> = BTreeMap::new();
        for (idx, item) in items.iter().enumerate() {
            by_hash.entry((item.kind, item.raw_hash)).or_default().push(idx);
        }
        for members in by_hash.into_values() {
            if members.len() < 2 {
                continue;
            }
            for x in 0..members.len() {
                for y in x + 1..members.len() {
                    let (a, b) = (members[x], members[y]);
                    if items_overlap(&items[a], &items[b]) {
                        continue;
                    }
                    if matched_item_pairs.insert((a, b)) {
                        pairs.push(MatchPair {
                            pair_id: pair_id_for(&items[a].canonical_id, &items[b].canonical_id, None),
                            tier: DupTier::Exact,
                            a,
                            b,
                            score: 1.0,
                            ident_sim: 1.0,
                            block: None,
                        });
                    }
                }
            }
        }
    }

    // 2. Local-α clones (Type-2)
    if want("alpha") {
        let mut by_hash: BTreeMap<(ItemKind, [u8; 32]), Vec<usize>> = BTreeMap::new();
        for (idx, item) in items.iter().enumerate() {
            by_hash.entry((item.kind, item.alpha_hash)).or_default().push(idx);
        }
        for members in by_hash.into_values() {
            if members.len() < 2 {
                continue;
            }
            for x in 0..members.len() {
                for y in x + 1..members.len() {
                    let (a, b) = (members[x], members[y]);
                    if items_overlap(&items[a], &items[b]) || matched_item_pairs.contains(&(a, b)) {
                        continue;
                    }
                    matched_item_pairs.insert((a, b));
                    let ident_sim = sorted_jaccard(&items[a].domain_ids, &items[b].domain_ids);
                    pairs.push(MatchPair {
                        pair_id: pair_id_for(&items[a].canonical_id, &items[b].canonical_id, None),
                        tier: DupTier::Alpha,
                        a,
                        b,
                        score: 0.98,
                        ident_sim,
                        block: None,
                    });
                }
            }
        }
    }

    // 3. Near duplicates (Type-3): IDF-weighted 5-gram inverted index + exact token LCS + domain-ident overlap
    if want("near") {
        detect_near(args, items, &mut matched_item_pairs, &mut pairs);
    }

    // 4. Sub-function duplicated blocks (Winnowing + maximal bijective α-extension)
    // Skip any pair already in the same item-level duplicate component so block hits don't duplicate near/exact groups.
    if want("block") {
        let mut comp = UnionFind::new(items.len());
        for &(a, b) in &matched_item_pairs {
            comp.union(a, b);
        }
        detect_blocks(args, items, local_sentinel, &mut comp, &mut pairs);
    }

    // 5. Optional semantic duplicates via external OpenAI-compatible embedding server
    if want("semantic") || args.embed_url.is_some() {
        if let Some(ref url) = args.embed_url {
            detect_semantic(args, root, url, items, vocab, sources, &mut matched_item_pairs, &mut pairs);
        }
    }

    pairs
}

fn detect_near(
    args: &Args,
    items: &[Item],
    matched: &mut HashSet<(usize, usize)>,
    out: &mut Vec<MatchPair>,
) {
    // Partition by ItemKind so functions only compare with functions, macros with macros, etc.
    let mut by_kind: BTreeMap<ItemKind, Vec<usize>> = BTreeMap::new();
    for (idx, item) in items.iter().enumerate() {
        // Reject ultra-repetitive lookup/match tables (low 4-gram diversity) from near-dup matching.
        if item.diversity >= 0.35 && !item.shingles.is_empty() {
            by_kind.entry(item.kind).or_default().push(idx);
        }
    }

    for indices in by_kind.into_values() {
        if indices.len() < 2 {
            continue;
        }
        let n = indices.len() as f64;
        // Build inverted index over 5-gram shingles (FxMap for fast integer hashing).
        let mut postings: FxMap<u64, Vec<usize>> = FxMap::default();
        for &idx in &indices {
            for &sh in &items[idx].shingles {
                postings.entry(sh).or_default().push(idx);
            }
        }
        // Drop unique (df < 2) and ubiquitous (df > max_df) shingles to stay O(N) and kill boilerplate.
        let max_df = ((indices.len() / 50).clamp(12, 32)) as usize;
        let mut idf_map: FxMap<u64, f64> = FxMap::default();
        for (&sh, list) in &postings {
            if list.len() >= 2 && list.len() <= max_df {
                idf_map.insert(sh, (n / list.len() as f64).ln().max(0.1));
            }
        }
        let mut total_idf: FxMap<usize, f64> = FxMap::default();
        total_idf.reserve(indices.len());
        for &idx in &indices {
            let sum: f64 = items[idx].shingles.iter().filter_map(|sh| idf_map.get(sh)).sum();
            total_idf.insert(idx, sum);
        }

        // Accumulate candidate pairs.
        let mut cands: FxMap<(usize, usize), (u32, f64)> = FxMap::default();
        for (&sh, list) in &postings {
            let Some(&w) = idf_map.get(&sh) else { continue };
            for x in 0..list.len() {
                for y in x + 1..list.len() {
                    let (a, b) = if list[x] < list[y] { (list[x], list[y]) } else { (list[y], list[x]) };
                    if matched.contains(&(a, b)) {
                        continue;
                    }
                    let la = items[a].alpha_ids.len();
                    let lb = items[b].alpha_ids.len();
                    let ratio = la.min(lb) as f64 / la.max(lb) as f64;
                    if ratio < 0.55 {
                        continue;
                    }
                    let entry = cands.entry((a, b)).or_insert((0, 0.0));
                    entry.0 += 1;
                    entry.1 += w;
                }
            }
        }

        // Verify surviving candidates in parallel chunks while preserving deterministic order.
        let mut cand_list: Vec<(usize, usize)> = cands
            .into_iter()
            .filter_map(|((a, b), (cnt, shared_w))| {
                if cnt < 4 {
                    return None;
                }
                let denom = total_idf[&a] + total_idf[&b] - shared_w;
                if denom <= 0.0 {
                    return None;
                }
                let wj = shared_w / denom;
                if wj < 0.30 {
                    return None;
                }
                Some((a, b))
            })
            .collect();
        cand_list.sort_unstable();

        let chunks: Vec<&[(usize, usize)]> = cand_list.chunks(64).collect();
        let jobs = std::thread::available_parallelism().map_or(4, std::num::NonZeroUsize::get).min(16);
        let threshold = args.threshold;
        let ident_threshold = args.ident_threshold;
        let mut verified: Vec<(usize, usize, f64, f64)> = Vec::new();
        fan_out(
            jobs,
            &chunks,
            |chunk| {
                let mut local = Vec::new();
                for &(a, b) in *chunk {
                    if items_overlap(&items[a], &items[b]) {
                        continue;
                    }
                    let ident_sim = if items[a].domain_ids.len() >= 4 && items[b].domain_ids.len() >= 4 {
                        sorted_jaccard(&items[a].domain_ids, &items[b].domain_ids)
                    } else {
                        1.0
                    };
                    if ident_sim < ident_threshold {
                        continue;
                    }
                    let lcs = lcs_similarity(&items[a].alpha_ids, &items[b].alpha_ids);
                    if lcs >= threshold {
                        local.push((a, b, lcs, ident_sim));
                    }
                }
                local
            },
            |mut part| verified.append(&mut part),
        );

        for (a, b, lcs, ident_sim) in verified {
            if matched.insert((a, b)) {
                out.push(MatchPair {
                    pair_id: pair_id_for(&items[a].canonical_id, &items[b].canonical_id, None),
                    tier: DupTier::Near,
                    a,
                    b,
                    score: lcs,
                    ident_sim,
                    block: None,
                });
            }
        }
    }
}

/// Detect duplicated sub-function blocks via parallel Winnowing over `block_ids` + bijective α-extension.
fn detect_blocks(
    args: &Args,
    items: &[Item],
    local_sentinel: u32,
    item_comp: &mut UnionFind,
    out: &mut Vec<MatchPair>,
) {
    let k = args.block_tokens;
    if k < 12 {
        return;
    }
    let win = 8usize;
    let fn_indices: Vec<usize> = items
        .iter()
        .enumerate()
        .filter_map(|(idx, item)| (item.kind == ItemKind::Fn && item.block_ids.len() >= k + win).then_some(idx))
        .collect();
    let jobs = std::thread::available_parallelism().map_or(4, std::num::NonZeroUsize::get).min(16);
    let idx_chunks: Vec<&[usize]> = fn_indices.chunks(64).collect();
    let mut index: FxMap<u64, Vec<(usize, usize)>> = FxMap::default();
    fan_out(
        jobs,
        &idx_chunks,
        |chunk| {
            let mut local: Vec<(u64, usize, usize)> = Vec::new();
            for &idx in *chunk {
                let bids = &items[idx].block_ids;
                let n_hashes = bids.len() - k + 1;
                let mut hashes = Vec::with_capacity(n_hashes);
                for pos in 0..n_hashes {
                    hashes.push(hash_u32_slice(&bids[pos..pos + k]));
                }
                let mut last_picked = usize::MAX;
                for w_start in 0..=hashes.len() - win {
                    let mut best_pos = w_start;
                    let mut best_h = hashes[w_start];
                    for p in w_start + 1..w_start + win {
                        if hashes[p] <= best_h {
                            best_h = hashes[p];
                            best_pos = p;
                        }
                    }
                    if best_pos != last_picked {
                        local.push((best_h, idx, best_pos));
                        last_picked = best_pos;
                    }
                }
            }
            local
        },
        |part| {
            for (h, idx, pos) in part {
                index.entry(h).or_default().push((idx, pos));
            }
        },
    );

    // Extend matching windows maximally with bijective local-variable check and deduplicate per (a, b).
    let mut seen_runs: HashMap<(usize, usize), Vec<(usize, usize, usize, usize)>> = HashMap::new();
    let mut hits: Vec<((usize, usize), usize, usize)> = Vec::new();
    for list in index.values() {
        if list.len() < 2 || list.len() > 16 {
            continue;
        }
        for x in 0..list.len() {
            for y in x + 1..list.len() {
                let (mut a, mut pa) = list[x];
                let (mut b, mut pb) = list[y];
                if a > b || (a == b && pa > pb) {
                    std::mem::swap(&mut a, &mut b);
                    std::mem::swap(&mut pa, &mut pb);
                }
                if a == b || item_comp.find(a) == item_comp.find(b) || items_overlap(&items[a], &items[b]) {
                    continue;
                }
                hits.push(((a, b), pa, pb));
            }
        }
    }
    hits.sort_unstable();

    for ((a, b), pa, pb) in hits {
        let ba = &items[a].block_ids;
        let bb = &items[b].block_ids;
        let ra_raw = &items[a].raw_ids;
        let rb_raw = &items[b].raw_ids;
        if ba[pa..pa + k] != bb[pb..pb + k] {
            continue;
        }
        // Check if already covered by an extended run for (a, b).
        if let Some(runs) = seen_runs.get(&(a, b)) {
            if runs.iter().any(|&(la, ra, lb, rb)| pa >= la && pa + k <= ra && pb >= lb && pb + k <= rb) {
                continue;
            }
        }
        // Verify local-variable bijection across the seed window [pa..pa+k] vs [pb..pb+k].
        let mut a_to_b: HashMap<u32, u32> = HashMap::new();
        let mut b_to_a: HashMap<u32, u32> = HashMap::new();
        let try_step = |ia: usize, ib: usize, ab: &mut HashMap<u32, u32>, ba_map: &mut HashMap<u32, u32>| -> bool {
            if ba[ia] != bb[ib] {
                return false;
            }
            if ba[ia] != local_sentinel {
                return true;
            }
            let va = ra_raw[ia];
            let vb = rb_raw[ib];
            match (ab.get(&va), ba_map.get(&vb)) {
                (Some(&m), Some(&r)) => m == vb && r == va,
                (None, None) => {
                    ab.insert(va, vb);
                    ba_map.insert(vb, va);
                    true
                }
                _ => false,
            }
        };
        let mut seed_ok = true;
        for off in 0..k {
            if !try_step(pa + off, pb + off, &mut a_to_b, &mut b_to_a) {
                seed_ok = false;
                break;
            }
        }
        if !seed_ok {
            continue;
        }

        let mut la = pa;
        let mut lb = pb;
        while la > 0 && lb > 0 && try_step(la - 1, lb - 1, &mut a_to_b, &mut b_to_a) {
            la -= 1;
            lb -= 1;
        }
        let mut ra = pa + k;
        let mut rb = pb + k;
        while ra < ba.len() && rb < bb.len() && try_step(ra, rb, &mut a_to_b, &mut b_to_a) {
            ra += 1;
            rb += 1;
        }
        let span_toks = ra - la;
        if span_toks < k {
            continue;
        }
        // Reject repetitive boilerplate spans (e.g., macro/enum tables).
        if token_diversity(&items[a].alpha_ids[la..ra]) < 0.40 {
            continue;
        }
        let a_start_line = items[a].alpha_lines[la];
        let a_end_line = items[a].alpha_lines[ra - 1];
        let b_start_line = items[b].alpha_lines[lb];
        let b_end_line = items[b].alpha_lines[rb - 1];
        let a_lines = a_end_line.saturating_sub(a_start_line) + 1;
        let b_lines = b_end_line.saturating_sub(b_start_line) + 1;
        if a_lines < args.block_min_lines || b_lines < args.block_min_lines {
            continue;
        }
        // Skip if block covers > 85% of BOTH functions (that's an item-level near-dup, not a sub-block).
        if span_toks * 100 >= ba.len() * 85 && span_toks * 100 >= bb.len() * 85 {
            continue;
        }
        seen_runs.entry((a, b)).or_default().push((la, ra, lb, rb));

        let mut bh = Sha256::new();
        bh.update(b"blaze-dups/block/1\0");
        for &id in &ba[la..ra] {
            bh.update(&id.to_le_bytes());
        }
        let block_hash = hex_short(&bh.finish(), 6);
        let pid = pair_id_for(&items[a].canonical_id, &items[b].canonical_id, Some(&block_hash));
        out.push(MatchPair {
            pair_id: pid,
            tier: DupTier::Block,
            a,
            b,
            score: 1.0,
            ident_sim: 1.0,
            block: Some(BlockSpan {
                a_start_line,
                a_end_line,
                b_start_line,
                b_end_line,
                tokens: span_toks,
            }),
        });
    }
}

/// Optional semantic duplicate detection via an OpenAI-compatible `/v1/embeddings` HTTP endpoint.
fn detect_semantic(
    args: &Args,
    root: &Path,
    url: &str,
    items: &[Item],
    vocab: &[String],
    sources: &HashMap<String, String>,
    matched: &mut HashSet<(usize, usize)>,
    out: &mut Vec<MatchPair>,
) {
    let model = args.embed_model.as_deref().unwrap_or("default");
    let cache_path = root.join(&args.embed_cache);
    let mut cache: HashMap<[u8; 16], Vec<f32>> = load_embed_cache(&cache_path);

    let mut keys: Vec<[u8; 16]> = Vec::with_capacity(items.len());
    let mut missing: Vec<([u8; 16], String)> = Vec::new();
    let mut seen_missing: HashSet<[u8; 16]> = HashSet::new();

    for item in items {
        let text = if args.embed_mode == "alpha" {
            let mut s = String::with_capacity(item.alpha_ids.len() * 5);
            for (idx, &tid) in item.alpha_ids.iter().enumerate() {
                if idx > 0 {
                    s.push(' ');
                }
                s.push_str(&vocab[tid as usize]);
                if s.len() >= 2400 {
                    break;
                }
            }
            s
        } else {
            let src = sources.get(&item.path).map(String::as_str).unwrap_or("");
            let snippet = &src[item.start_byte.min(src.len())..item.end_byte.min(src.len())];
            if snippet.len() > 2400 {
                let mut end = 2400;
                while !snippet.is_char_boundary(end) && end > 0 {
                    end -= 1;
                }
                snippet[..end].to_owned()
            } else {
                snippet.to_owned()
            }
        };
        let mut h = Sha256::new();
        h.update(b"blaze-dups/embed/1\0");
        h.update(model.as_bytes());
        h.update(b"\0");
        h.update(args.embed_mode.as_bytes());
        h.update(b"\0");
        h.update(text.as_bytes());
        let digest = h.finish();
        let mut key = [0u8; 16];
        key.copy_from_slice(&digest[..16]);
        if !cache.contains_key(&key) && seen_missing.insert(key) {
            missing.push((key, text));
        }
        keys.push(key);
    }

    if !missing.is_empty() {
        // Sort by length so GPU batches have minimal padding waste.
        missing.sort_by_key(|(_, t)| t.len());
        eprintln!(
            "[dups] embedding {} new of {} items via {url} (model={model}, mode={})",
            missing.len(),
            items.len(),
            args.embed_mode
        );
        let mut done = 0usize;
        for chunk in missing.chunks(64) {
            let mut body = Map::new();
            body.insert("model".to_owned(), Value::from(model));
            body.insert(
                "input".to_owned(),
                Value::Array(chunk.iter().map(|(_, t)| Value::from(t.as_str())).collect()),
            );
            let payload = Value::Object(body).to_string();
            let tmp = TempDir::new("dups-embed")
                .unwrap_or_else(|e| fail(SCRIPT, &format!("failed to create temp dir: {e}")));
            let req_path = tmp.path().join("req.json");
            if let Err(e) = fs::write(&req_path, payload.as_bytes()) {
                fail(SCRIPT, &format!("failed to write embed request: {e}"));
            }
            let req_arg = format!("@{}", req_path.display());
            let ran = match run_within(
                &["curl", "-sSf", "-X", "POST", "-H", "Content-Type: application/json", "--data-binary", &req_arg, url],
                root,
                Duration::from_secs(180),
            ) {
                Ok(Some(ran)) => ran,
                Ok(None) => fail(SCRIPT, &format!("embedding request to {url} timed out after 180s")),
                Err(e) => fail(SCRIPT, &format!("embedding request to {url} failed to start: {e}")),
            };
            if ran.status != 0 {
                fail(SCRIPT, &format!("embedding request to {url} exited {}: {}", ran.status, ran.err().trim()));
            }
            let parsed: Value = json::from_str(&ran.out())
                .unwrap_or_else(|e| fail(SCRIPT, &format!("invalid JSON from {url}: {e}")));
            let Some(data) = parsed.get("data").and_then(Value::as_array) else {
                fail(SCRIPT, &format!("embedding response from {url} missing `data` array"));
            };
            if data.len() != chunk.len() {
                fail(SCRIPT, &format!("embedding response returned {} vectors for {} inputs", data.len(), chunk.len()));
            }
            for ((key, _), row) in chunk.iter().zip(data.iter()) {
                let Some(arr) = row.get("embedding").and_then(Value::as_array) else {
                    fail(SCRIPT, "embedding row missing `embedding` array");
                };
                let mut vec: Vec<f32> = arr.iter().filter_map(Value::as_f64).map(|v| v as f32).collect();
                let norm: f32 = vec.iter().map(|x| x * x).sum::<f32>().sqrt();
                if norm > 0.0 {
                    for x in &mut vec {
                        *x /= norm;
                    }
                }
                cache.insert(*key, vec);
            }
            done += chunk.len();
            if done % 512 == 0 || done == missing.len() {
                eprintln!("[dups] embedded {done}/{}", missing.len());
                save_embed_cache(&cache_path, &cache);
            }
        }
    }

    // Parallel cosine similarity over normalized embeddings (same item kind, excluding already-matched).
    let vecs: Vec<&[f32]> = keys.iter().map(|k| cache[k].as_slice()).collect();
    let thresh = args.embed_threshold as f32;
    let cross_only = args.cross_crate_only;
    let indices: Vec<usize> = (0..items.len()).collect();
    let jobs = std::thread::available_parallelism().map_or(4, std::num::NonZeroUsize::get).min(16);
    let mut found: Vec<(usize, usize, f32)> = Vec::new();
    fan_out(
        jobs,
        &indices,
        |&i| {
            let mut local = Vec::new();
            let vi = vecs[i];
            if vi.is_empty() {
                return local;
            }
            let ki = items[i].kind;
            let li = items[i].alpha_ids.len();
            for j in i + 1..items.len() {
                if items[j].kind != ki {
                    continue;
                }
                if cross_only && items[i].krate == items[j].krate {
                    continue;
                }
                let lj = items[j].alpha_ids.len();
                if li.min(lj) * 100 < li.max(lj) * 40 {
                    continue;
                }
                if matched.contains(&(i, j)) || items_overlap(&items[i], &items[j]) {
                    continue;
                }
                let dot = dot_f32(vi, vecs[j]);
                if dot >= thresh {
                    local.push((i, j, dot));
                }
            }
            local
        },
        |mut part| found.append(&mut part),
    );
    for (i, j, dot) in found {
        if matched.insert((i, j)) {
            let ident_sim = sorted_jaccard(&items[i].domain_ids, &items[j].domain_ids);
            out.push(MatchPair {
                pair_id: pair_id_for(&items[i].canonical_id, &items[j].canonical_id, None),
                tier: DupTier::Semantic,
                a: i,
                b: j,
                score: f64::from(dot),
                ident_sim,
                block: None,
            });
        }
    }
}

#[inline]
fn dot_f32(a: &[f32], b: &[f32]) -> f32 {
    let n = a.len().min(b.len());
    let mut s0 = 0.0f32;
    let mut s1 = 0.0f32;
    let mut s2 = 0.0f32;
    let mut s3 = 0.0f32;
    let chunks = n / 4;
    for k in 0..chunks {
        let base = k * 4;
        s0 += a[base] * b[base];
        s1 += a[base + 1] * b[base + 1];
        s2 += a[base + 2] * b[base + 2];
        s3 += a[base + 3] * b[base + 3];
    }
    let mut sum = (s0 + s1) + (s2 + s3);
    for k in chunks * 4..n {
        sum += a[k] * b[k];
    }
    sum
}

const EMBED_CACHE_MAGIC: &[u8; 8] = b"BLZEMB01";

fn load_embed_cache(path: &Path) -> HashMap<[u8; 16], Vec<f32>> {
    let Ok(bytes) = fs::read(path) else { return HashMap::new() };
    if bytes.len() < 16 || &bytes[..8] != EMBED_CACHE_MAGIC {
        return HashMap::new();
    }
    let dim = u32::from_le_bytes(bytes[8..12].try_into().unwrap()) as usize;
    let count = u32::from_le_bytes(bytes[12..16].try_into().unwrap()) as usize;
    let stride = 16 + dim * 4;
    if dim == 0 || bytes.len() != 16 + count * stride {
        return HashMap::new();
    }
    let mut out = HashMap::with_capacity(count);
    for idx in 0..count {
        let base = 16 + idx * stride;
        let mut key = [0u8; 16];
        key.copy_from_slice(&bytes[base..base + 16]);
        let mut vec = Vec::with_capacity(dim);
        for d in 0..dim {
            let off = base + 16 + d * 4;
            vec.push(f32::from_le_bytes(bytes[off..off + 4].try_into().unwrap()));
        }
        out.insert(key, vec);
    }
    out
}

fn save_embed_cache(path: &Path, cache: &HashMap<[u8; 16], Vec<f32>>) {
    let Some(dim) = cache.values().next().map(Vec::len) else { return };
    if dim == 0 {
        return;
    }
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let mut keys: Vec<[u8; 16]> = cache.keys().copied().collect();
    keys.sort_unstable();
    let mut bytes = Vec::with_capacity(16 + keys.len() * (16 + dim * 4));
    bytes.extend_from_slice(EMBED_CACHE_MAGIC);
    bytes.extend_from_slice(&(dim as u32).to_le_bytes());
    bytes.extend_from_slice(&(keys.len() as u32).to_le_bytes());
    for k in keys {
        let v = &cache[&k];
        if v.len() != dim {
            continue;
        }
        bytes.extend_from_slice(&k);
        for &f in v {
            bytes.extend_from_slice(&f.to_le_bytes());
        }
    }
    let _ = fs::write(path, bytes);
}

/// Union-Find for clustering item-level matches.
struct UnionFind {
    parent: Vec<usize>,
}

impl UnionFind {
    fn new(n: usize) -> UnionFind {
        UnionFind { parent: (0..n).collect() }
    }

    fn find(&mut self, mut x: usize) -> usize {
        while self.parent[x] != x {
            self.parent[x] = self.parent[self.parent[x]];
            x = self.parent[x];
        }
        x
    }

    fn union(&mut self, a: usize, b: usize) {
        let ra = self.find(a);
        let rb = self.find(b);
        if ra != rb {
            self.parent[ra.max(rb)] = ra.min(rb);
        }
    }
}

/// Cluster active pairs into ranked `DupGroup`s.
/// Exact/alpha pairs always form equivalence classes; near/semantic edges merge two clusters
/// only when at least 67% of all cross-cluster pairs exist in the edge set (anti-snowball).
fn build_groups(args: &Args, items: &[Item], pairs: &[MatchPair], dismissed: &HashMap<String, Dismissal>) -> Vec<DupGroup> {
    let mut item_pairs: Vec<MatchPair> = Vec::new();
    let mut block_pairs: Vec<MatchPair> = Vec::new();

    for p in pairs {
        if !args.include_dismissed && dismissed.contains_key(&p.pair_id) {
            continue;
        }
        if p.tier == DupTier::Block {
            block_pairs.push(p.clone());
        } else {
            item_pairs.push(p.clone());
        }
    }

    // Build edge lookup and sort pairs highest-confidence first (exact/alpha before near/semantic).
    let mut edge_set: HashSet<(usize, usize)> = HashSet::with_capacity(item_pairs.len() * 2);
    for p in &item_pairs {
        let (lo, hi) = if p.a <= p.b { (p.a, p.b) } else { (p.b, p.a) };
        edge_set.insert((lo, hi));
    }
    let mut ordered: Vec<&MatchPair> = item_pairs.iter().collect();
    ordered.sort_by(|x, y| {
        let exact_x = matches!(x.tier, DupTier::Exact | DupTier::Alpha);
        let exact_y = matches!(y.tier, DupTier::Exact | DupTier::Alpha);
        exact_y
            .cmp(&exact_x)
            .then_with(|| y.score.total_cmp(&x.score))
            .then_with(|| x.pair_id.cmp(&y.pair_id))
    });

    let mut uf = UnionFind::new(items.len());
    let mut comp_members: HashMap<usize, Vec<usize>> = HashMap::new();
    for p in &ordered {
        let ra = uf.find(p.a);
        let rb = uf.find(p.b);
        if ra == rb {
            continue;
        }
        let is_exact = matches!(p.tier, DupTier::Exact | DupTier::Alpha);
        let ma = comp_members.get(&ra).map_or(&[][..], Vec::as_slice);
        let mb = comp_members.get(&rb).map_or(&[][..], Vec::as_slice);
        let la = ma.len().max(1);
        let lb = mb.len().max(1);
        let allow_merge = if is_exact || (la == 1 && lb == 1) {
            true
        } else {
            // Count how many cross-edges exist between component `ra` and component `rb`.
            let members_a: &[usize] = if ma.is_empty() { std::slice::from_ref(&ra) } else { ma };
            let members_b: &[usize] = if mb.is_empty() { std::slice::from_ref(&rb) } else { mb };
            let mut cross = 0usize;
            for &u in members_a {
                for &v in members_b {
                    let (lo, hi) = if u <= v { (u, v) } else { (v, u) };
                    if edge_set.contains(&(lo, hi)) {
                        cross += 1;
                    }
                }
            }
            cross * 3 >= la * lb * 2
        };
        if allow_merge {
            let mut va = comp_members.remove(&ra).unwrap_or_else(|| vec![ra]);
            let vb = comp_members.remove(&rb).unwrap_or_else(|| vec![rb]);
            va.extend(vb);
            uf.union(ra, rb);
            let nr = uf.find(ra);
            comp_members.insert(nr, va);
        }
    }

    let mut comp_pairs: BTreeMap<usize, Vec<MatchPair>> = BTreeMap::new();
    let mut residual_pairs: Vec<MatchPair> = Vec::new();
    for p in item_pairs {
        let ra = uf.find(p.a);
        let rb = uf.find(p.b);
        if ra == rb {
            comp_pairs.entry(ra).or_default().push(p);
        } else {
            residual_pairs.push(p);
        }
    }
    // Keep a residual bridge pair as its own 2-member group only if at least one endpoint
    // isn't already in a stronger multi-member cluster (avoids losing orphaned duplicates while
    // keeping noisy bridge edges out of tight clusters).
    let in_cluster: HashSet<usize> = comp_pairs.values().flat_map(|ps| ps.iter().flat_map(|p| [p.a, p.b])).collect();

    let mut groups = Vec::new();

    for p in residual_pairs {
        if !in_cluster.contains(&p.a) || !in_cluster.contains(&p.b) {
            let mut members = vec![p.a, p.b];
            members.sort_by(|&x, &y| {
                (&items[x].krate, &items[x].path, items[x].start_line)
                    .cmp(&(&items[y].krate, &items[y].path, items[y].start_line))
            });
            let crates: Vec<String> = members.iter().map(|&m| items[m].krate.clone()).collect::<BTreeSet<_>>().into_iter().collect();
            let removable_lines = items[p.a].lines().min(items[p.b].lines());
            let gid = p.pair_id.clone();
            groups.push(DupGroup {
                group_id: gid,
                tier: p.tier,
                members,
                pairs: vec![p.clone()],
                crates,
                score: p.score,
                removable_lines,
                block: None,
            });
        }
    }

    for (_, gpairs) in comp_pairs {
        let mut member_set = BTreeSet::new();
        for p in &gpairs {
            member_set.insert(p.a);
            member_set.insert(p.b);
        }
        let mut members: Vec<usize> = member_set.into_iter().collect();
        members.sort_by(|&x, &y| {
            (&items[x].krate, &items[x].path, items[x].start_line)
                .cmp(&(&items[y].krate, &items[y].path, items[y].start_line))
        });
        let crates: Vec<String> = members
            .iter()
            .map(|&m| items[m].krate.clone())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        let total_lines: usize = members.iter().map(|&m| items[m].lines()).sum();
        let max_lines: usize = members.iter().map(|&m| items[m].lines()).max().unwrap_or(0);
        let removable_lines = total_lines.saturating_sub(max_lines);
        let tier = gpairs.iter().map(|p| p.tier).min().unwrap_or(DupTier::Near);
        let score = gpairs.iter().map(|p| p.score).sum::<f64>() / gpairs.len() as f64;
        let member_ids: Vec<&str> = members.iter().map(|&m| items[m].canonical_id.as_str()).collect();
        let gid = group_id_for(&member_ids, None);

        groups.push(DupGroup {
            group_id: gid,
            tier,
            members,
            pairs: gpairs,
            crates,
            score,
            removable_lines,
            block: None,
        });
    }

    for p in block_pairs {
        let Some(ref blk) = p.block else { continue };
        let members = vec![p.a, p.b];
        let crates: Vec<String> = members
            .iter()
            .map(|&m| items[m].krate.clone())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        let a_lines = blk.a_end_line.saturating_sub(blk.a_start_line) + 1;
        let b_lines = blk.b_end_line.saturating_sub(blk.b_start_line) + 1;
        let removable_lines = a_lines.min(b_lines);
        let gid = p.pair_id.clone();
        groups.push(DupGroup {
            group_id: gid,
            tier: DupTier::Block,
            members,
            pairs: vec![p.clone()],
            crates,
            score: p.score,
            removable_lines,
            block: Some(blk.clone()),
        });
    }

    // Apply crate & cross-crate filters.
    groups.retain(|g| {
        if args.cross_crate_only && g.crates.len() < 2 {
            return false;
        }
        if let Some(ref target) = args.krate {
            if !g.crates.iter().any(|c| c == target) {
                return false;
            }
        }
        true
    });

    // Rank: highest impact (`removable_lines * score`), then cross-crate, then deterministic group_id.
    groups.sort_by(|a, b| {
        let ia = (a.removable_lines as f64 * a.score * 1000.0) as u64;
        let ib = (b.removable_lines as f64 * b.score * 1000.0) as u64;
        ib.cmp(&ia)
            .then_with(|| (b.crates.len() >= 2).cmp(&(a.crates.len() >= 2)))
            .then_with(|| a.tier.cmp(&b.tier))
            .then_with(|| a.group_id.cmp(&b.group_id))
    });

    groups
}

/// Load `tools/ratchets/dups-not.json` (returns empty map if file does not exist yet).
fn load_dismissals(path: &Path) -> HashMap<String, Dismissal> {
    if !path.exists() {
        return HashMap::new();
    }
    let Ok(text) = fs::read_to_string(path) else {
        fail(SCRIPT, &format!("{} could not be read", path.display()));
    };
    let parsed: Value = json::from_str(&text)
        .unwrap_or_else(|e| fail(SCRIPT, &format!("{} is not valid JSON: {e}", path.display())));
    let Some(schema) = parsed.get("schema").and_then(Value::as_str) else {
        fail(SCRIPT, &format!("{} missing `schema`", path.display()));
    };
    if schema != DISMISSALS_SCHEMA {
        fail(SCRIPT, &format!("{} has schema {schema:?}, expected {DISMISSALS_SCHEMA:?}", path.display()));
    }
    let Some(arr) = parsed.get("dismissed").and_then(Value::as_array) else {
        fail(SCRIPT, &format!("{} missing `dismissed` array", path.display()));
    };
    let mut out = HashMap::with_capacity(arr.len());
    for (idx, row) in arr.iter().enumerate() {
        let get_str = |k: &str| {
            row.get(k)
                .and_then(Value::as_str)
                .unwrap_or_else(|| fail(SCRIPT, &format!("{}: dismissed[{idx}] missing string `{k}`", path.display())))
                .to_owned()
        };
        let d = Dismissal { id: get_str("id"), a: get_str("a"), b: get_str("b"), reason: get_str("reason") };
        out.insert(d.id.clone(), d);
    }
    out
}

/// Save `tools/ratchets/dups-not.json` deterministically sorted by `id`.
fn save_dismissals(path: &Path, dismissed: &HashMap<String, Dismissal>) {
    if let Some(parent) = path.parent() {
        if let Err(e) = fs::create_dir_all(parent) {
            fail(SCRIPT, &format!("could not create {}: {e}", parent.display()));
        }
    }
    let mut rows: Vec<&Dismissal> = dismissed.values().collect();
    rows.sort_by(|x, y| x.id.cmp(&y.id));
    let arr: Vec<Value> = rows
        .into_iter()
        .map(|d| {
            let mut m = Map::new();
            m.insert("id".to_owned(), Value::from(d.id.as_str()));
            m.insert("a".to_owned(), Value::from(d.a.as_str()));
            m.insert("b".to_owned(), Value::from(d.b.as_str()));
            m.insert("reason".to_owned(), Value::from(d.reason.as_str()));
            Value::Object(m)
        })
        .collect();
    let mut doc = Map::new();
    doc.insert("schema".to_owned(), Value::from(DISMISSALS_SCHEMA));
    doc.insert("dismissed".to_owned(), Value::Array(arr));
    let mut pretty = json::to_string_pretty(&Value::Object(doc))
        .unwrap_or_else(|e| fail(SCRIPT, &format!("failed to format JSON: {e}")));
    pretty.push('\n');
    if let Err(e) = fs::write(path, pretty) {
        fail(SCRIPT, &format!("could not write {}: {e}", path.display()));
    }
}

/// Extract source lines `start..=end` (1-based inclusive) with line numbers.
fn snippet_lines(sources: &HashMap<String, String>, path: &str, start: usize, end: usize) -> Vec<(usize, String)> {
    let Some(text) = sources.get(path) else { return Vec::new() };
    let all: Vec<&str> = text.split('\n').collect();
    let lo = start.saturating_sub(1).min(all.len());
    let hi = end.min(all.len());
    (lo..hi).map(|idx| (idx + 1, all[idx].to_owned())).collect()
}

/// Compute a compact line-level unified diff between two line slices (for `next` and `show`).
fn line_diff(a: &[(usize, String)], b: &[(usize, String)]) -> Vec<String> {
    let a_trim: Vec<&str> = a.iter().map(|(_, s)| s.trim()).collect();
    let b_trim: Vec<&str> = b.iter().map(|(_, s)| s.trim()).collect();
    if a_trim.len().saturating_mul(b_trim.len()) > 200_000 {
        return vec!["(diff omitted: items exceed 450 lines each)".to_owned()];
    }
    let m = a_trim.len();
    let n = b_trim.len();
    let mut dp = vec![vec![0u16; n + 1]; m + 1];
    for i in (0..m).rev() {
        for j in (0..n).rev() {
            dp[i][j] = if a_trim[i] == b_trim[j] {
                dp[i + 1][j + 1] + 1
            } else {
                dp[i + 1][j].max(dp[i][j + 1])
            };
        }
    }
    let mut out = Vec::new();
    let (mut i, mut j) = (0usize, 0usize);
    while i < m && j < n {
        if a_trim[i] == b_trim[j] {
            out.push(format!("  {}", a[i].1));
            i += 1;
            j += 1;
        } else if dp[i + 1][j] >= dp[i][j + 1] {
            out.push(format!("- {}", a[i].1));
            i += 1;
        } else {
            out.push(format!("+ {}", b[j].1));
            j += 1;
        }
    }
    while i < m {
        out.push(format!("- {}", a[i].1));
        i += 1;
    }
    while j < n {
        out.push(format!("+ {}", b[j].1));
        j += 1;
    }
    out
}

fn print_group_detail(group: &DupGroup, items: &[Item], sources: &HashMap<String, String>, total_groups: usize, total_lines: usize, dismissed_count: usize) {
    let scope = if group.crates.len() >= 2 { "cross-crate" } else { "same-crate" };
    println!(
        "=== group {} [{} | {} | score {:.3} | ~{} removable lines | {} copies] ===",
        group.group_id,
        group.tier.as_str(),
        scope,
        group.score,
        group.removable_lines,
        group.members.len(),
    );
    println!("Progress: {total_groups} unreviewed groups (~{total_lines} removable lines) | {dismissed_count} dismissed pairs");
    println!();
    println!("Members:");
    for (idx, &m) in group.members.iter().enumerate() {
        let it = &items[m];
        let (sl, el) = match (&group.block, idx) {
            (Some(b), 0) => (b.a_start_line, b.a_end_line),
            (Some(b), 1) => (b.b_start_line, b.b_end_line),
            _ => (it.start_line, it.end_line),
        };
        println!(
            "  [{idx}] {}:{}..{} ({} lines)  {}  [crate: {}]",
            it.path,
            sl,
            el,
            el.saturating_sub(sl) + 1,
            it.name,
            it.krate,
        );
    }
    if group.pairs.len() > 1 {
        println!();
        println!("Pairs (dismiss a single pair id to split out a false positive without hiding the rest):");
        let pos_of = |item_idx: usize| group.members.iter().position(|&m| m == item_idx);
        let mut sorted_pairs = group.pairs.clone();
        sorted_pairs.sort_by(|x, y| y.score.total_cmp(&x.score).then_with(|| x.pair_id.cmp(&y.pair_id)));
        for p in sorted_pairs.iter().take(12) {
            if let (Some(ia), Some(ib)) = (pos_of(p.a), pos_of(p.b)) {
                println!(
                    "  pair {}  [{ia}] <-> [{ib}]  {:<6} score={:.3}  ident={:.3}",
                    p.pair_id,
                    p.tier.as_str(),
                    p.score,
                    p.ident_sim
                );
            }
        }
        if sorted_pairs.len() > 12 {
            println!("  ... ({} more pairs)", sorted_pairs.len() - 12);
        }
    }
    println!();
    if group.members.len() >= 2 {
        let (a, b) = (group.members[0], group.members[1]);
        let (a_s, a_e, b_s, b_e) = if let Some(ref blk) = group.block {
            (blk.a_start_line, blk.a_end_line, blk.b_start_line, blk.b_end_line)
        } else {
            (items[a].start_line, items[a].end_line, items[b].start_line, items[b].end_line)
        };
        let la = snippet_lines(sources, &items[a].path, a_s, a_e);
        let lb = snippet_lines(sources, &items[b].path, b_s, b_e);
        println!("--- [0] {}:{}..{} ({}) ---", items[a].path, a_s, a_e, items[a].name);
        for (ln, line) in la.iter().take(60) {
            println!("{ln:>5} | {line}");
        }
        if la.len() > 60 {
            println!("  ... ({} more lines)", la.len() - 60);
        }
        println!();
        println!("+++ [1] {}:{}..{} ({}) +++", items[b].path, b_s, b_e, items[b].name);
        let diff = line_diff(&la, &lb);
        let changed = diff.iter().filter(|l| l.starts_with('-') || l.starts_with('+')).count();
        if changed == 0 {
            println!("  (byte-identical trimmed lines)");
        } else {
            for line in diff.iter().take(80) {
                println!("{line}");
            }
            if diff.len() > 80 {
                println!("  ... ({} more diff lines)", diff.len() - 80);
            }
        }
    }
    println!();
    println!("If NOT a duplicate, dismiss it:");
    println!(
        "  blaze run -q -p scripts --bin dups -- '{{\"command\": \"dismiss\", \"id\": \"{}\", \"reason\": \"<why these are distinct>\"}}'",
        group.group_id
    );
}

fn print_scan_table(args: &Args, items: &[Item], groups: &[DupGroup], dismissed_count: usize, elapsed_ms: u128) {
    let total_removable: usize = groups.iter().map(|g| g.removable_lines).sum();
    let cross_groups = groups.iter().filter(|g| g.crates.len() >= 2).count();
    let mut by_tier: BTreeMap<&str, usize> = BTreeMap::new();
    for g in groups {
        *by_tier.entry(g.tier.as_str()).or_insert(0) += 1;
    }
    let tier_summary: Vec<String> = by_tier.iter().map(|(k, v)| format!("{k}={v}")).collect();
    println!(
        "dups: {} items scanned in {} ms | {} unreviewed groups ({} cross-crate, ~{} removable lines) | {} dismissed | {}",
        items.len(),
        elapsed_ms,
        groups.len(),
        cross_groups,
        total_removable,
        dismissed_count,
        if tier_summary.is_empty() { "none".to_owned() } else { tier_summary.join(", ") }
    );
    if groups.is_empty() {
        return;
    }
    // Per-crate leaderboard by removable lines (shows where deduplication pays most).
    let mut crate_lines: BTreeMap<&str, usize> = BTreeMap::new();
    for g in groups {
        let share = g.removable_lines / g.crates.len().max(1);
        for c in &g.crates {
            *crate_lines.entry(c.as_str()).or_insert(0) += share.max(1);
        }
    }
    if crate_lines.len() > 1 {
        let mut ranked: Vec<(&str, usize)> = crate_lines.into_iter().collect();
        ranked.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(b.0)));
        let top_crates: Vec<String> = ranked.iter().take(8).map(|(c, n)| format!("{c} (~{n})")).collect();
        println!("Top crates: {}", top_crates.join(", "));
    }
    println!();
    let limit = if args.top == 0 { groups.len() } else { args.top.min(groups.len()) };
    for (rank, g) in groups.iter().take(limit).enumerate() {
        let scope = if g.crates.len() >= 2 { format!("{} crates", g.crates.len()) } else { g.crates[0].clone() };
        println!(
            "{:>3}. [{}] {:<6} score={:.3}  ~{:>4} lines  {:>2} copies  ({})",
            rank + 1,
            g.group_id,
            g.tier.as_str(),
            g.score,
            g.removable_lines,
            g.members.len(),
            scope,
        );
        for (mi, &m) in g.members.iter().take(4).enumerate() {
            let it = &items[m];
            let (sl, el) = match (&g.block, mi) {
                (Some(b), 0) => (b.a_start_line, b.a_end_line),
                (Some(b), 1) => (b.b_start_line, b.b_end_line),
                _ => (it.start_line, it.end_line),
            };
            println!("       {}:{}-{}  {}", it.path, sl, el, it.name);
        }
        if g.members.len() > 4 {
            println!("       ... and {} more", g.members.len() - 4);
        }
    }
    if groups.len() > limit {
        println!();
        println!("... showing top {limit} of {} groups (pass `\"top\": 0` for all, or `\"command\": \"next\"` to triage #1)", groups.len());
    }
}

fn scan_json(items: &[Item], groups: &[DupGroup], dismissed_count: usize) -> Value {
    let total_removable: usize = groups.iter().map(|g| g.removable_lines).sum();
    let mut doc = Map::new();
    doc.insert("schema".to_owned(), Value::from(REPORT_SCHEMA));
    doc.insert("items_scanned".to_owned(), Value::from(items.len() as u64));
    doc.insert("groups_count".to_owned(), Value::from(groups.len() as u64));
    doc.insert("removable_lines".to_owned(), Value::from(total_removable as u64));
    doc.insert("dismissed_pairs".to_owned(), Value::from(dismissed_count as u64));
    let jgroups: Vec<Value> = groups
        .iter()
        .map(|g| {
            let mut gm = Map::new();
            gm.insert("id".to_owned(), Value::from(g.group_id.as_str()));
            gm.insert("tier".to_owned(), Value::from(g.tier.as_str()));
            gm.insert("score".to_owned(), Value::from((g.score * 1000.0).round() / 1000.0));
            gm.insert("removable_lines".to_owned(), Value::from(g.removable_lines as u64));
            gm.insert("cross_crate".to_owned(), Value::from(g.crates.len() >= 2));
            gm.insert("crates".to_owned(), Value::Array(g.crates.iter().map(|c| Value::from(c.as_str())).collect()));
            let members: Vec<Value> = g
                .members
                .iter()
                .enumerate()
                .map(|(mi, &m)| {
                    let it = &items[m];
                    let (sl, el) = match (&g.block, mi) {
                        (Some(b), 0) => (b.a_start_line, b.a_end_line),
                        (Some(b), 1) => (b.b_start_line, b.b_end_line),
                        _ => (it.start_line, it.end_line),
                    };
                    let mut mm = Map::new();
                    mm.insert("canonical_id".to_owned(), Value::from(it.canonical_id.as_str()));
                    mm.insert("crate".to_owned(), Value::from(it.krate.as_str()));
                    mm.insert("path".to_owned(), Value::from(it.path.as_str()));
                    mm.insert("start_line".to_owned(), Value::from(sl as u64));
                    mm.insert("end_line".to_owned(), Value::from(el as u64));
                    mm.insert("kind".to_owned(), Value::from(it.kind.as_str()));
                    mm.insert("name".to_owned(), Value::from(it.name.as_str()));
                    Value::Object(mm)
                })
                .collect();
            gm.insert("members".to_owned(), Value::Array(members));
            let pair_ids: Vec<Value> = g.pairs.iter().map(|p| Value::from(p.pair_id.as_str())).collect();
            gm.insert("pair_ids".to_owned(), Value::Array(pair_ids));
            let ident_sim = if g.pairs.is_empty() {
                1.0
            } else {
                g.pairs.iter().map(|p| p.ident_sim).sum::<f64>() / g.pairs.len() as f64
            };
            gm.insert("ident_sim".to_owned(), Value::from((ident_sim * 1000.0).round() / 1000.0));
            if let Some(ref blk) = g.block {
                gm.insert("block_tokens".to_owned(), Value::from(blk.tokens as u64));
            }
            Value::Object(gm)
        })
        .collect();
    doc.insert("groups".to_owned(), Value::Array(jgroups));
    Value::Object(doc)
}

fn html_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            _ => out.push(ch),
        }
    }
    out
}

fn write_html_report(out_path: &Path, items: &[Item], groups: &[DupGroup], sources: &HashMap<String, String>, dismissed_count: usize) {
    if let Some(parent) = out_path.parent() {
        if let Err(e) = fs::create_dir_all(parent) {
            fail(SCRIPT, &format!("could not create {}: {e}", parent.display()));
        }
    }
    let total_removable: usize = groups.iter().map(|g| g.removable_lines).sum();
    let mut body = String::new();
    for g in groups.iter().take(250) {
        let scope = if g.crates.len() >= 2 { format!("{} crates", g.crates.len()) } else { g.crates[0].clone() };
        let _ = write!(
            body,
            "<section class=\"tier-{}\"><h3><code>{}</code> · {} <small>{} copies · {} · ~{} removable lines · score {:.3}</small></h3><ul>",
            g.tier.as_str(),
            html_escape(&g.group_id),
            g.tier.as_str(),
            g.members.len(),
            html_escape(&scope),
            g.removable_lines,
            g.score,
        );
        for (mi, &m) in g.members.iter().take(20).enumerate() {
            let it = &items[m];
            let (sl, el) = match (&g.block, mi) {
                (Some(b), 0) => (b.a_start_line, b.a_end_line),
                (Some(b), 1) => (b.b_start_line, b.b_end_line),
                _ => (it.start_line, it.end_line),
            };
            let _ = write!(
                body,
                "<li><span class=crate>{}</span><code>{}:{}-{}</code> · <code>{}</code></li>",
                html_escape(&it.krate),
                html_escape(&it.path),
                sl,
                el,
                html_escape(&it.name),
            );
        }
        body.push_str("</ul>");
        let dismiss_cmd = format!(
            "blaze run -q -p scripts --bin dups -- '{{\"command\": \"dismiss\", \"id\": \"{}\", \"reason\": \"...\"}}'",
            g.group_id
        );
        let _ = write!(body, "<div class=cmd>Dismiss: <code>{}</code></div>", html_escape(&dismiss_cmd));
        if g.members.len() >= 2 {
            body.push_str("<details><summary>compare first two</summary><div class=pair>");
            for (mi, &m) in g.members.iter().take(2).enumerate() {
                let it = &items[m];
                let (sl, el) = match (&g.block, mi) {
                    (Some(b), 0) => (b.a_start_line, b.a_end_line),
                    (Some(b), 1) => (b.b_start_line, b.b_end_line),
                    _ => (it.start_line, it.end_line),
                };
                let lines = snippet_lines(sources, &it.path, sl, el);
                let joined: String = lines.iter().take(80).map(|(n, l)| format!("{n:>4} | {l}")).collect::<Vec<_>>().join("\n");
                let _ = write!(
                    body,
                    "<div><div class=cap>{}:{}-{}</div><pre>{}</pre></div>",
                    html_escape(&it.path),
                    sl,
                    el,
                    html_escape(&joined),
                );
            }
            body.push_str("</div></details>");
        }
        body.push_str("</section>\n");
    }
    let page = format!(
        r#"<!doctype html><meta charset=utf-8><title>blaze dups report</title>
<style>
body{{font:14px/1.45 -apple-system,system-ui,sans-serif;max-width:1440px;margin:2em auto;padding:0 1em;color:#222}}
h3{{margin:.2em 0;font-size:15px}} small{{color:#666;font-weight:normal}}
section{{border:1px solid #e0e0e0;border-left:4px solid #0b62c4;border-radius:6px;padding:.7em 1em;margin:.8em 0}}
section.tier-exact{{border-left-color:#c62828}} section.tier-alpha{{border-left-color:#e65100}}
section.tier-near{{border-left-color:#0b62c4}} section.tier-block{{border-left-color:#6a1b9a}}
ul{{margin:.3em 0;padding-left:1.2em}} .crate{{display:inline-block;min-width:12em;color:#555}}
.cmd{{font-size:12px;color:#555;margin:.4em 0}} .pair{{display:grid;grid-template-columns:1fr 1fr;gap:.6em;margin-top:.5em}}
.cap{{font:12px monospace;color:#555;margin-bottom:.2em}}
pre{{background:#f6f6f6;padding:.6em;overflow:auto;font-size:12px;margin:0;border-radius:4px}}
@media (prefers-color-scheme:dark){{body{{background:#141414;color:#ddd}}section{{border-color:#333}}pre{{background:#1f1f1f}}.crate,.cmd,.cap,small{{color:#999}}}}
</style>
<h1>Duplicate code report</h1>
<p>{} items scanned · <strong>{}</strong> unreviewed groups (~<strong>{}</strong> removable lines) · {} dismissed pairs.</p>
{}
"#,
        items.len(),
        groups.len(),
        total_removable,
        dismissed_count,
        body,
    );
    if let Err(e) = fs::write(out_path, page) {
        fail(SCRIPT, &format!("could not write {}: {e}", out_path.display()));
    }
    eprintln!("[dups] wrote {}", out_path.display());
}

/// Per-crate unreviewed duplicate item counts (each duplicated item counted once in its crate).
fn per_crate_counts(items: &[Item], groups: &[DupGroup]) -> BTreeMap<String, Vec<ratchet::Hit<String>>> {
    let mut seen: BTreeSet<(usize, usize)> = BTreeSet::new();
    let mut out: BTreeMap<String, Vec<ratchet::Hit<String>>> = BTreeMap::new();
    for g in groups {
        for (mi, &m) in g.members.iter().enumerate() {
            let it = &items[m];
            let line = match (&g.block, mi) {
                (Some(b), 0) => b.a_start_line,
                (Some(b), 1) => b.b_start_line,
                _ => it.start_line,
            };
            if seen.insert((m, line)) {
                out.entry(it.krate.clone()).or_default().push(ratchet::Hit {
                    path: it.path.clone(),
                    line,
                    what: format!("[{} {}] {}", g.tier.as_str(), g.group_id, it.name),
                });
            }
        }
    }
    for list in out.values_mut() {
        list.sort_by(|a, b| (&a.path, a.line, &a.what).cmp(&(&b.path, b.line, &b.what)));
    }
    out
}

fn run_pipeline(args: &Args, root: &Path) -> (Vec<Item>, HashMap<String, String>, Vec<MatchPair>, Vec<DupGroup>, HashMap<String, Dismissal>, u128) {
    let started = std::time::Instant::now();
    let tests = Tests::new(SCRIPT);
    let skip: HashSet<String> = args.skip.iter().cloned().collect();
    let wanted_kinds: HashSet<ItemKind> = args
        .item_kinds
        .iter()
        .filter_map(|k| match k.as_str() {
            "fn" => Some(ItemKind::Fn),
            "macro" => Some(ItemKind::Macro),
            "struct" => Some(ItemKind::Struct),
            "enum" => Some(ItemKind::Enum),
            _ => None,
        })
        .collect();

    let files = collect_sources(root, &args.dirs, &skip, args.include_examples, args.skip_generated, &tests);
    let jobs = std::thread::available_parallelism().map_or(4, std::num::NonZeroUsize::get).min(16);
    let mut raw_items = Vec::new();
    fan_out(
        jobs,
        &files,
        |(sf, tlines)| extract_items(sf, tlines, &wanted_kinds, args.min_lines, args.min_tokens),
        |mut chunk| raw_items.append(&mut chunk),
    );
    raw_items.sort_by(|a, b| (&a.path, a.start_line, &a.canonical_id).cmp(&(&b.path, b.start_line, &b.canonical_id)));

    let mut sources: HashMap<String, String> = HashMap::with_capacity(files.len());
    for (sf, _) in files {
        sources.insert(sf.rel_path, sf.text);
    }

    let (items, local_sentinel, vocab) = intern_items(raw_items);
    let pairs = detect_pairs(args, root, &items, local_sentinel, &vocab, &sources);
    let dismiss_path = root.join(&args.dismissals);
    let dismissed = load_dismissals(&dismiss_path);
    let groups = build_groups(args, &items, &pairs, &dismissed);
    let elapsed_ms = started.elapsed().as_millis();
    (items, sources, pairs, groups, dismissed, elapsed_ms)
}

fn write_stdout_line(s: &str) {
    use std::io::Write as _;
    let mut out = std::io::stdout().lock();
    if let Err(e) = out.write_all(s.as_bytes()).and_then(|()| out.write_all(b"\n")) {
        if e.kind() == std::io::ErrorKind::BrokenPipe {
            blaze_script::exit(0);
        }
        fail(SCRIPT, &format!("stdout write failed: {e}"));
    }
}

fn cmd_main(args: Args) {
    let root = PathBuf::from(&args.root);
    let dismiss_path = root.join(&args.dismissals);

    if args.command == "undismiss" {
        let mut target_ids = args.ids.clone();
        if let Some(ref id) = args.id {
            target_ids.push(id.clone());
        }
        if target_ids.is_empty() {
            fail(SCRIPT, "`undismiss` requires `id` or `ids`");
        }
        let mut dismissed = load_dismissals(&dismiss_path);
        let mut removed = 0usize;
        for id in &target_ids {
            if dismissed.remove(id).is_none() {
                fail(SCRIPT, &format!("no dismissed entry with id {id:?} in {}", dismiss_path.display()));
            }
            removed += 1;
        }
        save_dismissals(&dismiss_path, &dismissed);
        println!("dups: removed {removed} dismissal(s) ({} remaining)", dismissed.len());
        return;
    }

    let (items, sources, pairs, groups, mut dismissed, elapsed_ms) = run_pipeline(&args, &root);

    match args.command.as_str() {
        "scan" => {
            if let Some(ref html_rel) = args.html_out {
                let html_path = root.join(html_rel);
                write_html_report(&html_path, &items, &groups, &sources, dismissed.len());
            }
            if args.format == "json" {
                write_stdout_line(&scan_json(&items, &groups, dismissed.len()).to_string());
            } else {
                print_scan_table(&args, &items, &groups, dismissed.len(), elapsed_ms);
            }
        }
        "next" => {
            if let Some(first) = groups.first() {
                let total_lines: usize = groups.iter().map(|g| g.removable_lines).sum();
                print_group_detail(first, &items, &sources, groups.len(), total_lines, dismissed.len());
            } else {
                println!("dups: 0 unreviewed duplicates remaining! ({} dismissed pairs)", dismissed.len());
            }
        }
        "show" => {
            let Some(ref id) = args.id else {
                fail(SCRIPT, "`show` requires `id`");
            };
            let total_lines: usize = groups.iter().map(|g| g.removable_lines).sum();
            if let Some(g) = groups.iter().find(|g| g.group_id == *id || g.pairs.iter().any(|p| p.pair_id == *id)) {
                print_group_detail(g, &items, &sources, groups.len(), total_lines, dismissed.len());
            } else {
                fail(SCRIPT, &format!("no active group or pair with id {id:?}"));
            }
        }
        "dismiss" => {
            let mut target_ids = args.ids.clone();
            if let Some(ref id) = args.id {
                target_ids.push(id.clone());
            }
            if target_ids.is_empty() {
                fail(SCRIPT, "`dismiss` requires `id` or `ids`");
            }
            let reason = args.reason.as_deref().unwrap_or("").trim();
            if reason.is_empty() {
                fail(SCRIPT, "`dismiss` requires a non-empty `reason` explaining why these are not duplicates");
            }
            let mut added = 0usize;
            for id in &target_ids {
                // Match either a group_id or an individual pair_id.
                let target_pairs: Vec<&MatchPair> = if let Some(g) = groups.iter().find(|g| g.group_id == *id) {
                    g.pairs.iter().collect()
                } else if let Some(p) = pairs.iter().find(|p| p.pair_id == *id) {
                    vec![p]
                } else {
                    fail(SCRIPT, &format!("no group or pair with id {id:?} found in current scan"));
                };
                for p in target_pairs {
                    let (a_id, b_id) = if items[p.a].canonical_id <= items[p.b].canonical_id {
                        (items[p.a].canonical_id.clone(), items[p.b].canonical_id.clone())
                    } else {
                        (items[p.b].canonical_id.clone(), items[p.a].canonical_id.clone())
                    };
                    if dismissed
                        .insert(
                            p.pair_id.clone(),
                            Dismissal { id: p.pair_id.clone(), a: a_id, b: b_id, reason: reason.to_owned() },
                        )
                        .is_none()
                    {
                        added += 1;
                    }
                }
            }
            save_dismissals(&dismiss_path, &dismissed);
            println!(
                "dups: dismissed {added} pair(s) across {} id(s) in {} ({} total dismissed)",
                target_ids.len(),
                dismiss_path.display(),
                dismissed.len()
            );
        }
        "prune" => {
            let live_ids: HashSet<&str> = pairs.iter().map(|p| p.pair_id.as_str()).collect();
            let before = dismissed.len();
            let mut removed: Vec<String> = Vec::new();
            dismissed.retain(|k, _| {
                if live_ids.contains(k.as_str()) {
                    true
                } else {
                    removed.push(k.clone());
                    false
                }
            });
            removed.sort();
            if !removed.is_empty() {
                save_dismissals(&dismiss_path, &dismissed);
            }
            println!(
                "dups: pruned {} stale dismissal(s) ({before} -> {})",
                removed.len(),
                dismissed.len()
            );
            for id in removed {
                println!("  removed {id}");
            }
        }
        "findings" => {
            let mut out = Vec::new();
            for g in &groups {
                for (mi, &m) in g.members.iter().enumerate() {
                    let it = &items[m];
                    let line = match (&g.block, mi) {
                        (Some(b), 0) => b.a_start_line,
                        (Some(b), 1) => b.b_start_line,
                        _ => it.start_line,
                    };
                    out.push(Finding {
                        krate: it.krate.clone(),
                        file: it.path.clone(),
                        line,
                        kind: g.tier.as_str(),
                        detail: Some(format!(
                            "group={} copies={} removable={} score={:.3} {}",
                            g.group_id,
                            g.members.len(),
                            g.removable_lines,
                            g.score,
                            it.name
                        )),
                    });
                }
            }
            let kinds = [
                DupTier::Exact.finding_kind(),
                DupTier::Alpha.finding_kind(),
                DupTier::Near.finding_kind(),
                DupTier::Block.finding_kind(),
                DupTier::Semantic.finding_kind(),
            ];
            write_stdout_line(&finding::document(SCRIPT, &kinds, out).to_string());
        }
        "write" | "check" => {
            let hits = per_crate_counts(&items, &groups);
            let total_items: usize = hits.values().map(Vec::len).sum();
            let summary = format!("{total_items} unreviewed duplicate items across {} crates ({} groups)", hits.len(), groups.len());
            let policy = Policy {
                list: RATCHET_LIST,
                noun: "duplicate items",
                instead: INSTEAD_MSG,
                rule: RULE_MSG,
                at_zero: &[],
            };
            let run = ratchet::Run {
                script: SCRIPT,
                policy: &policy,
                header: RATCHET_HEADER,
                root: &root,
                write: args.command == "write",
                base: args.base.as_deref(),
                summary: &summary,
            };
            ratchet::write_or_check(&run, &hits, |h| format!("{}:{} ({})", h.path, h.line, h.what));
        }
        other => fail(SCRIPT, &format!("unexpected command {other:?}")),
    }
}

/// Self-test covering exact clones, local-α clones, diverged near-duplicates,
/// negative control (same syntax over different domain types must NOT match),
/// sub-function block clones, and dismiss/prune/undismiss workflow.
fn selftest() {
    let tmp = TempDir::new("dups-selftest").expect("tempdir");
    let root = tmp.path();
    fs::write(
        root.join("Cargo.toml"),
        "[workspace]\nmembers = [\"crates/alpha\", \"crates/beta\"]\n",
    )
    .unwrap();
    fs::create_dir_all(root.join("crates/alpha/src")).unwrap();
    fs::create_dir_all(root.join("crates/beta/src")).unwrap();
    fs::write(root.join("crates/alpha/Cargo.toml"), "[package]\nname = \"alpha\"\n").unwrap();
    fs::write(root.join("crates/beta/Cargo.toml"), "[package]\nname = \"beta\"\n").unwrap();

    // 1. Exact duplicate across crates (only function name + comment + pub differ)
    // 2. Local-α duplicate (local variables renamed, types/methods kept)
    // 3. Near duplicate (1 extra check + slightly different error message)
    // 4. Negative control: same Rust control-flow skeleton, totally different domain types/methods (MUST NOT MATCH!)
    // 5. Sub-function block clone inside two otherwise different functions
    fs::write(
        root.join("crates/alpha/src/lib.rs"),
        r#"
// Exact copy A
pub fn parse_header_a(bytes: &[u8], limit: usize) -> Option<(u32, u32, usize)> {
    if bytes.len() < 12 || limit == 0 {
        return None;
    }
    let magic = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
    let version = u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]);
    let payload = u32::from_le_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]) as usize;
    if magic != 0x424c5a31 || payload > limit {
        return None;
    }
    Some((magic, version, payload))
}

// Local-alpha copy A
pub fn score_buckets_a(buckets: &[u64], cap: u64) -> u64 {
    let mut acc = 0u64;
    let mut seen = 0u64;
    for entry in buckets {
        let clamped = (*entry).min(cap);
        if clamped > 16 {
            acc = acc.saturating_add(clamped * 3);
            seen += 1;
        }
    }
    if seen == 0 { 0 } else { acc / seen }
}

// Near duplicate A
pub fn verify_pack_a(entries: &[(&str, u64)], max_total: u64) -> Result<u64, &'static str> {
    let mut sum = 0u64;
    for (name, size) in entries {
        if name.is_empty() {
            return Err("empty entry name");
        }
        if name.contains("..") {
            return Err("path escape");
        }
        sum = sum.checked_add(*size).ok_or("overflow")?;
        if sum > max_total {
            return Err("pack too large");
        }
    }
    Ok(sum)
}

// Negative control A: network socket dispatch
pub fn handle_socket_frame(sock: &mut TcpStream, ring: &mut RingBuf, peer: PeerId) -> IoStatus {
    let mut frames = 0usize;
    while let Some(pkt) = ring.pop_inbound(peer) {
        if pkt.checksum_mismatch() {
            sock.record_corrupt_packet(peer);
            return IoStatus::Reset;
        }
        sock.enqueue_outbound(pkt.ack_seq());
        frames += 1;
    }
    IoStatus::Drained(frames)
}

// Function with a duplicated inner block A
pub fn big_pipeline_a(input: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(input.len());
    out.push(0xAA);
    // Shared block start
    let mut checksum = 0x811c9dc5u32;
    let mut run_len = 0usize;
    for &byte in input {
        checksum ^= u32::from(byte);
        checksum = checksum.wrapping_mul(0x01000193);
        if byte == b'\n' || byte == b'\r' {
            if run_len > 4 {
                out.push((checksum & 0xff) as u8);
            }
            run_len = 0;
        } else {
            out.push(byte ^ 0x55);
            run_len += 1;
        }
    }
    out.extend_from_slice(&checksum.to_le_bytes());
    // Shared block end
    out.push(0xFF);
    out
}
"#,
    )
    .unwrap();

    fs::write(
        root.join("crates/beta/src/lib.rs"),
        r#"
// Exact copy B (different visibility, comment, name)
fn parse_header_b(bytes: &[u8], limit: usize) -> Option<(u32, u32, usize)> {
    if bytes.len() < 12 || limit == 0 {
        return None;
    }
    let magic = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
    let version = u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]);
    let payload = u32::from_le_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]) as usize;
    if magic != 0x424c5a31 || payload > limit {
        return None;
    }
    Some((magic, version, payload))
}

// Local-alpha copy B (locals renamed: buckets->slots, cap->max_val, acc->total, seen->count, entry->val, clamped->v)
pub fn score_buckets_b(slots: &[u64], max_val: u64) -> u64 {
    let mut total = 0u64;
    let mut count = 0u64;
    for val in slots {
        let v = (*val).min(max_val);
        if v > 16 {
            total = total.saturating_add(v * 3);
            count += 1;
        }
    }
    if count == 0 { 0 } else { total / count }
}

// Near duplicate B (one extra check added)
pub fn verify_pack_b(entries: &[(&str, u64)], max_total: u64) -> Result<u64, &'static str> {
    let mut sum = 0u64;
    for (name, size) in entries {
        if name.is_empty() {
            return Err("empty entry name");
        }
        if name.contains("..") || name.starts_with('/') {
            return Err("path escape");
        }
        sum = sum.checked_add(*size).ok_or("overflow")?;
        if sum > max_total {
            return Err("pack too large");
        }
    }
    Ok(sum)
}

// Negative control B: same loop/if/return skeleton, completely different domain (shader layout)
pub fn bind_shader_uniform(pipe: &mut RenderPass, pool: &mut DescriptorPool, stage: StageId) -> DrawState {
    let mut binds = 0usize;
    while let Some(desc) = pool.next_binding(stage) {
        if desc.layout_incompatible() {
            pipe.mark_pipeline_invalid(stage);
            return DrawState::Aborted;
        }
        pipe.attach_descriptor(desc.slot_index());
        binds += 1;
    }
    DrawState::Bound(binds)
}

// Function with duplicated inner block B (completely different header/footer!)
pub fn unrelated_compressor_b(raw: &[u8], tag: u8) -> Vec<u8> {
    let mut out = Vec::new();
    for i in 0..8 {
        out.push(tag.wrapping_add(i));
    }
    let header_len = out.len();
    if header_len == 0 {
        return out;
    }
    // Shared block start (with renamed local `raw` -> `input`? No, different name `raw`!)
    let mut checksum = 0x811c9dc5u32;
    let mut run_len = 0usize;
    for &byte in raw {
        checksum ^= u32::from(byte);
        checksum = checksum.wrapping_mul(0x01000193);
        if byte == b'\n' || byte == b'\r' {
            if run_len > 4 {
                out.push((checksum & 0xff) as u8);
            }
            run_len = 0;
        } else {
            out.push(byte ^ 0x55);
            run_len += 1;
        }
    }
    out.extend_from_slice(&checksum.to_le_bytes());
    // Shared block end
    out.reverse();
    out.truncate(header_len + 64);
    out
}
"#,
    )
    .unwrap();

    let base_args = |command: &str| Args {
        command: command.to_owned(),
        root: root.to_string_lossy().into_owned(),
        base: None,
        dirs: vec!["crates".to_owned()],
        krate: None,
        skip: Vec::new(),
        include_examples: true,
        skip_generated: true,
        cross_crate_only: false,
        kinds: vec!["exact".to_owned(), "alpha".to_owned(), "near".to_owned(), "block".to_owned()],
        item_kinds: vec!["fn".to_owned(), "macro".to_owned()],
        min_lines: 6,
        min_tokens: 30,
        threshold: 0.78,
        ident_threshold: 0.40,
        block_tokens: 55,
        block_min_lines: 10,
        top: 30,
        format: "table".to_owned(),
        html_out: None,
        id: None,
        ids: Vec::new(),
        reason: None,
        include_dismissed: false,
        dismissals: DEFAULT_DISMISSALS.to_owned(),
        embed_url: None,
        embed_model: None,
        embed_mode: "raw".to_owned(),
        embed_threshold: 0.92,
        embed_cache: "target/dups/embed-cache.bin".to_owned(),
    };

    let (items, _, pairs, groups, _, _) = run_pipeline(&base_args("scan"), root);
    let has_tier = |tier: DupTier, name_a: &str, name_b: &str| {
        groups.iter().any(|g| {
            g.tier == tier
                && g.members.iter().any(|&m| items[m].name == name_a)
                && g.members.iter().any(|&m| items[m].name == name_b)
        })
    };
    assert!(has_tier(DupTier::Exact, "parse_header_a", "parse_header_b"), "missed exact clone");
    assert!(has_tier(DupTier::Alpha, "score_buckets_a", "score_buckets_b"), "missed local-alpha clone");
    assert!(has_tier(DupTier::Near, "verify_pack_a", "verify_pack_b"), "missed near duplicate");
    assert!(has_tier(DupTier::Block, "big_pipeline_a", "unrelated_compressor_b"), "missed block clone");

    // Negative control MUST NOT match!
    let false_pos = pairs.iter().any(|p| {
        let (na, nb) = (items[p.a].name.as_str(), items[p.b].name.as_str());
        (na == "handle_socket_frame" && nb == "bind_shader_uniform")
            || (na == "bind_shader_uniform" && nb == "handle_socket_frame")
    });
    assert!(!false_pos, "negative control falsely matched: unrelated domain functions collided!");

    // Test dismiss -> disappears from scan -> prune keeps live -> undismiss restores it.
    let near_group = groups.iter().find(|g| g.tier == DupTier::Near).unwrap();
    let target_group = near_group.group_id.clone();
    let target_pair = near_group.pairs[0].pair_id.clone();
    let mut dismiss_args = base_args("dismiss");
    dismiss_args.id = Some(target_group.clone());
    dismiss_args.reason = Some("selftest dismissal".to_owned());
    cmd_main(dismiss_args);

    let (_, _, _, groups_after, dismissed_after, _) = run_pipeline(&base_args("scan"), root);
    assert_eq!(dismissed_after.len(), 1, "expected 1 dismissal saved");
    assert!(
        !groups_after.iter().any(|g| g.group_id == target_group),
        "dismissed group still appeared in scan!"
    );

    // Prune should keep the live dismissal.
    cmd_main(base_args("prune"));
    let dismissed_after_prune = load_dismissals(&root.join(DEFAULT_DISMISSALS));
    assert_eq!(dismissed_after_prune.len(), 1, "prune dropped a still-live dismissal");

    // Undismiss restores it.
    let mut undismiss_args = base_args("undismiss");
    undismiss_args.id = Some(target_pair);
    cmd_main(undismiss_args);
    let (_, _, _, groups_restored, dismissed_restored, _) = run_pipeline(&base_args("scan"), root);
    assert_eq!(dismissed_restored.len(), 0, "expected 0 dismissals after undismiss");
    assert!(
        groups_restored.iter().any(|g| g.group_id == target_group),
        "undismissed group did not reappear in scan!"
    );

    // Verify `skip_generated` skips `generated.rs` (and includes it when false).
    let gen_fn = r#"
pub fn generated_table_lookup(key: u32, salt: u32) -> Option<u32> {
    if key == 0 || salt == 0 {
        return None;
    }
    let mixed = key.wrapping_mul(0x9e37_79b1) ^ salt.rotate_left(7);
    let slot = mixed & 0x0fff;
    if slot < 16 {
        return None;
    }
    Some(slot ^ 0x5a5a)
}
"#;
    fs::write(root.join("crates/alpha/src/generated.rs"), gen_fn).unwrap();
    fs::write(root.join("crates/beta/src/generated.rs"), gen_fn).unwrap();
    let (items_skip, _, _, _, _, _) = run_pipeline(&base_args("scan"), root);
    assert!(
        !items_skip.iter().any(|it| it.name == "generated_table_lookup"),
        "generated.rs should be skipped when skip_generated is true"
    );
    let mut include_gen = base_args("scan");
    include_gen.skip_generated = false;
    let (items_keep, _, _, groups_keep, _, _) = run_pipeline(&include_gen, root);
    assert!(
        groups_keep.iter().any(|g| {
            g.tier == DupTier::Exact
                && g.members.iter().filter(|&&m| items_keep[m].name == "generated_table_lookup").count() == 2
        }),
        "generated.rs should be scanned when skip_generated is false"
    );

    println!("dups selftest: ok (exact, alpha, near, block, negative-control, dismiss, prune, undismiss, skip_generated verified)");
}

fn main() {
    let mut argv = std::env::args().skip(1);
    let Some(first) = argv.next() else {
        eprintln!("{DOC}");
        fail(SCRIPT, "expected one JSON argument or `selftest`");
    };
    if argv.next().is_some() {
        eprintln!("{DOC}");
        fail(SCRIPT, "takes one argument: a JSON object (inline, path, or `-`) or `selftest`");
    }
    if first == "selftest" {
        selftest();
        return;
    }
    if first == "-h" || first == "--help" || first == "help" {
        println!("{DOC}");
        return;
    }
    let args: Args = input::parse(&first, SCRIPT);
    cmd_main(args);
}
