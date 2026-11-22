//! Fast structural duplicate-function detection (`find_duplicates`) and code-health complexity
//! analysis (`analyze_health`) in a single token pass over Rust source files.
//!
//! - **Alpha-normalized exact clones**: local identifier names are canonicalized in order of
//!   first appearance (`v0`, `v1`, ...) while preserving Rust keywords, operators, and nesting
//!   structure, then hashed with BLAKE3 (`blaze-blake3`) into a 128-bit fingerprint. Two
//!   functions with identical logic and renamed variables match deterministically.
//! - **Near-duplicate detection**: 5-gram token shingles sorted and compared by Jaccard
//!   similarity (`>= 85%`) catch copy-pasted functions with minor tweaks.
//! - **Code-health smells**: flags deep nesting (`>= 5` brace levels inside a function),
//!   oversized/high-branch god functions, and redundant boolean `if cond { true } else { false }`
//!   expressions.
//!
//! Pure, deterministic, `#![no_std]`, `#![forbid(unsafe_code)]`, explicit allocator only.

use blaze_lex::{Cpu, FrontmatterAllowed, TokenKind, tokenize};
use blaze_own::{AVec, Alloc};

use crate::Span;

/// One extracted function item with its alpha-normalized BLAKE3 fingerprint and 5-gram shingles.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FnFingerprint {
    pub file_id: u32,
    pub range: Span,
    pub name_span: Span,
    pub token_count: u32,
    pub exact_hash: [u8; 16],
    /// Compact deterministic sample of up to 8 smallest 64-bit token 5-gram shingles (MinHash).
    pub minhash: [u64; 8],
    pub shingle_count: u8,
}

/// Kind of duplicate detected between two or more function bodies.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum CloneKind {
    /// Identical structure after alpha-renaming local identifiers.
    ExactClone = 1,
    /// High MinHash shingle overlap (`>= 7/8` = `87.5%`) indicating copy-paste with minor edits.
    NearClone = 2,
}

/// One duplicate pair/group finding, sorted by wasted token volume descending.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DuplicateFinding {
    pub kind: CloneKind,
    pub first_file: u32,
    pub first_range: Span,
    pub second_file: u32,
    pub second_range: Span,
    pub wasted_tokens: u32,
    pub similarity_pct: u8,
}

/// Kind of code-health / maintainability smell detected in a function.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum HealthKind {
    /// Function body reaches brace nesting depth `>= max_depth`.
    DeepNesting = 1,
    /// Function exceeds token count or branch-point complexity budget.
    HighComplexity = 2,
    /// Redundant `if cond { true } else { false }` or `if cond { false } else { true }`.
    RedundantBoolIf = 3,
}

impl HealthKind {
    pub const fn label(self) -> &'static str {
        match self {
            HealthKind::DeepNesting => "deep-nesting",
            HealthKind::HighComplexity => "high-complexity",
            HealthKind::RedundantBoolIf => "redundant-bool-if",
        }
    }
}

/// Thresholds for [`analyze_health`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HealthLimits {
    /// Minimum non-trivia tokens for duplicate detection (default: `24`).
    pub min_dup_tokens: u32,
    /// Maximum brace nesting inside a function before flagging `DeepNesting` (default: `5`).
    pub max_nesting: u32,
    /// Maximum branch keywords (`if`, `match`, `for`, `while`, `loop`, `&&`, `||`) before flagging (default: `24`).
    pub max_branches: u32,
    /// Maximum non-trivia tokens inside a single function before flagging (default: `320`).
    pub max_fn_tokens: u32,
}

impl HealthLimits {
    pub const DEFAULT: HealthLimits = HealthLimits {
        min_dup_tokens: 24,
        max_nesting: 5,
        max_branches: 24,
        max_fn_tokens: 320,
    };
}

/// One code-health finding on a function or expression.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HealthFinding {
    pub kind: HealthKind,
    pub range: Span,
    pub metric: u32,
    pub limit: u32,
}

#[inline]
fn is_rust_keyword(w: &str) -> bool {
    matches!(
        w,
        "as" | "async"
            | "await"
            | "break"
            | "const"
            | "continue"
            | "crate"
            | "dyn"
            | "else"
            | "enum"
            | "extern"
            | "false"
            | "fn"
            | "for"
            | "if"
            | "impl"
            | "in"
            | "let"
            | "loop"
            | "match"
            | "mod"
            | "move"
            | "mut"
            | "pub"
            | "ref"
            | "return"
            | "self"
            | "Self"
            | "static"
            | "struct"
            | "super"
            | "trait"
            | "true"
            | "type"
            | "unsafe"
            | "use"
            | "where"
            | "while"
            | "Ok"
            | "Err"
            | "Some"
            | "None"
    )
}

use core::hash::{Hash as _, Hasher as _};

#[derive(Default)]
struct MixHasher(u64);

impl core::hash::Hasher for MixHasher {
    #[inline]
    fn finish(&self) -> u64 {
        mix64(self.0 ^ 0x9E37_79B9_7F4A_7C15)
    }

    #[inline]
    fn write(&mut self, bytes: &[u8]) {
        let mut h = self.0;
        for &b in bytes {
            h = h.wrapping_mul(0x0100_0000_01B3) ^ u64::from(b);
        }
        self.0 = h;
    }
}

#[inline]
fn token_kind_code(kind: TokenKind) -> u64 {
    let mut h = MixHasher(0xCBF2_9CE4_8422_2325);
    core::mem::discriminant(&kind).hash(&mut h);
    h.finish()
}

#[inline]
fn mix64(mut x: u64) -> u64 {
    x ^= x >> 30;
    x = x.wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x ^= x >> 27;
    x = x.wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^ (x >> 31)
}

/// Extracts alpha-normalized function fingerprints from `text` (`file_id`) for functions with at
/// least `min_tokens` non-trivia body tokens.
pub fn fingerprint_file<'a>(
    file_id: u32,
    text: &str,
    min_tokens: u32,
    alloc: Alloc<'a>,
) -> AVec<'a, FnFingerprint> {
    let mut out = AVec::new_in(alloc);

    // Collect compact non-trivia tokens in one pass.
    #[derive(Clone, Copy)]
    struct Tok {
        kind: TokenKind,
        start: u32,
        end: u32,
    }
    let mut toks = AVec::<Tok>::new_in(alloc);
    let mut pos = 0u32;
    for t in tokenize(text, FrontmatterAllowed::No, Cpu::BASELINE) {
        let start = pos;
        let end = pos + t.len;
        pos = end;
        if matches!(
            t.kind,
            TokenKind::Whitespace | TokenKind::LineComment { .. } | TokenKind::BlockComment { .. }
        ) {
            continue;
        }
        toks.push(Tok { kind: t.kind, start, end });
    }

    // Reusable alpha-renaming dictionary (stores slice byte ranges of distinct local idents).
    let mut locals = AVec::<&str>::new_in(alloc);
    let mut norm_ids = AVec::<u64>::new_in(alloc);

    let mut i = 0usize;
    while i + 3 < toks.len() {
        let t = toks[i];
        if t.kind != TokenKind::Ident || &text[t.start as usize..t.end as usize] != "fn" {
            i += 1;
            continue;
        }
        let name_tok = toks[i + 1];
        if name_tok.kind != TokenKind::Ident {
            i += 1;
            continue;
        }
        // Scan forward for opening `{` of function body at paren/angle depth 0 (skip `;` decls).
        let mut j = i + 2;
        let mut pdepth = 0usize;
        let mut body_open = None;
        while j < toks.len() {
            match toks[j].kind {
                TokenKind::OpenParen | TokenKind::OpenBracket | TokenKind::Lt => pdepth += 1,
                TokenKind::CloseParen | TokenKind::CloseBracket | TokenKind::Gt => {
                    pdepth = pdepth.saturating_sub(1);
                }
                TokenKind::Semi if pdepth == 0 => break,
                TokenKind::OpenBrace if pdepth == 0 => {
                    body_open = Some(j);
                    break;
                }
                _ => {}
            }
            j += 1;
        }
        let Some(open) = body_open else {
            i = j + 1;
            continue;
        };

        // Find matching `}` for the function body.
        let mut bdepth = 1usize;
        let mut k = open + 1;
        while k < toks.len() && bdepth > 0 {
            match toks[k].kind {
                TokenKind::OpenBrace => bdepth += 1,
                TokenKind::CloseBrace => bdepth -= 1,
                _ => {}
            }
            if bdepth > 0 {
                k += 1;
            }
        }
        if bdepth != 0 {
            break;
        }
        let close = k;
        let body_tokens = (close.saturating_sub(open + 1)) as u32;
        if body_tokens >= min_tokens {
            locals.clear();
            norm_ids.clear();
            let mut hasher = blaze_blake3::Hasher::new_derive_key("blaze-lint dup alpha 1");
            for tok in &toks[open + 1..close] {
                let code: u64 = if tok.kind == TokenKind::Ident {
                    let word = &text[tok.start as usize..tok.end as usize];
                    if is_rust_keyword(word) {
                        hasher.update(&[1u8]);
                        hasher.update(word.as_bytes());
                        hasher.update(&[0u8]);
                        let mut kh = 0x517C_C1B7_2722_0A95u64;
                        for &b in word.as_bytes() {
                            kh = mix64(kh ^ u64::from(b));
                        }
                        kh
                    } else {
                        let id = match locals.iter().position(|&w| w == word) {
                            Some(pos) => pos as u32,
                            None => {
                                let pos = locals.len() as u32;
                                locals.push(word);
                                pos
                            }
                        };
                        hasher.update(&[2u8]);
                        hasher.update(&id.to_le_bytes());
                        mix64(0xCAFE_BABEu64 ^ u64::from(id))
                    }
                } else {
                    let kind_code = token_kind_code(tok.kind);
                    hasher.update(&[3u8]);
                    hasher.update(&kind_code.to_le_bytes());
                    kind_code
                };
                norm_ids.push(code);
            }
            let full = hasher.finalize();
            let mut exact_hash = [0u8; 16];
            exact_hash.copy_from_slice(&full[..16]);

            // Compute 8 smallest 5-gram MinHash shingles.
            let mut minhash = [u64::MAX; 8];
            let mut shingle_count = 0u8;
            if norm_ids.len() >= 5 {
                for w in norm_ids.windows(5) {
                    let sh = mix64(
                        w[0].wrapping_mul(31)
                            ^ w[1].wrapping_mul(131)
                            ^ w[2].wrapping_mul(1313)
                            ^ w[3].wrapping_mul(13131)
                            ^ w[4],
                    );
                    if !minhash.contains(&sh) && sh < minhash[7] {
                        minhash[7] = sh;
                        // Insertion-sort 8 elements (branch-light).
                        let mut idx = 7;
                        while idx > 0 && minhash[idx] < minhash[idx - 1] {
                            minhash.swap(idx, idx - 1);
                            idx -= 1;
                        }
                        if shingle_count < 8 {
                            shingle_count += 1;
                        }
                    }
                }
            }

            out.push(FnFingerprint {
                file_id,
                range: Span::new(t.start, toks[close].end),
                name_span: Span::new(name_tok.start, name_tok.end),
                token_count: body_tokens,
                exact_hash,
                minhash,
                shingle_count,
            });
        }
        i = close + 1;
    }

    out
}

/// Finds exact alpha-equivalent clones and near-duplicate functions (`>= 87.5%` MinHash overlap)
/// across a slice of [`FnFingerprint`]s, returning findings sorted by wasted tokens descending.
pub fn find_duplicates<'a>(
    fingerprints: &[FnFingerprint],
    alloc: Alloc<'a>,
) -> AVec<'a, DuplicateFinding> {
    let mut out = AVec::new_in(alloc);
    if fingerprints.len() < 2 {
        return out;
    }

    // 1. Exact alpha-normalized clones in O(N log N) via sorted index table.
    let mut by_hash = AVec::<usize>::with_capacity_in(fingerprints.len(), alloc);
    for idx in 0..fingerprints.len() {
        by_hash.push(idx);
    }
    by_hash.sort_unstable_by(|&a, &b| {
        fingerprints[a]
            .exact_hash
            .cmp(&fingerprints[b].exact_hash)
            .then_with(|| fingerprints[a].file_id.cmp(&fingerprints[b].file_id))
            .then_with(|| fingerprints[a].range.start.cmp(&fingerprints[b].range.start))
    });

    let mut exact_matched = AVec::<bool>::new_in(alloc);
    exact_matched.resize(fingerprints.len(), false);

    let mut i = 0;
    while i < by_hash.len() {
        let head = by_hash[i];
        let mut j = i + 1;
        while j < by_hash.len() && fingerprints[by_hash[j]].exact_hash == fingerprints[head].exact_hash
        {
            let dup = by_hash[j];
            exact_matched[dup] = true;
            out.push(DuplicateFinding {
                kind: CloneKind::ExactClone,
                first_file: fingerprints[head].file_id,
                first_range: fingerprints[head].range,
                second_file: fingerprints[dup].file_id,
                second_range: fingerprints[dup].range,
                wasted_tokens: fingerprints[dup].token_count,
                similarity_pct: 100,
            });
            j += 1;
        }
        i = j;
    }

    // 2. Near-clones via MinHash bucket matching on the smallest shingle (`minhash[0]`), bounded
    // to functions of similar size (`0.8x..1.25x` token count) for O(N log N) scaling.
    let mut by_sig = AVec::<usize>::with_capacity_in(fingerprints.len(), alloc);
    for (idx, fp) in fingerprints.iter().enumerate() {
        if !exact_matched[idx] && fp.shingle_count == 8 {
            by_sig.push(idx);
        }
    }
    by_sig.sort_unstable_by(|&a, &b| {
        fingerprints[a]
            .minhash[0]
            .cmp(&fingerprints[b].minhash[0])
            .then_with(|| fingerprints[a].token_count.cmp(&fingerprints[b].token_count))
    });

    let mut s = 0;
    while s < by_sig.len() {
        let a_idx = by_sig[s];
        let a = &fingerprints[a_idx];
        let mut t = s + 1;
        while t < by_sig.len() && fingerprints[by_sig[t]].minhash[0] == a.minhash[0] {
            let b_idx = by_sig[t];
            let b = &fingerprints[b_idx];
            if a.exact_hash != b.exact_hash
                && b.token_count * 4 <= a.token_count * 5
                && a.token_count * 4 <= b.token_count * 5
            {
                let shared = a.minhash.iter().filter(|&&h| b.minhash.contains(&h)).count();
                if shared >= 7 {
                    let pct = ((shared * 100) / 8) as u8;
                    out.push(DuplicateFinding {
                        kind: CloneKind::NearClone,
                        first_file: a.file_id,
                        first_range: a.range,
                        second_file: b.file_id,
                        second_range: b.range,
                        wasted_tokens: a.token_count.min(b.token_count),
                        similarity_pct: pct,
                    });
                }
            }
            t += 1;
        }
        s += 1;
    }

    out.sort_unstable_by(|a, b| {
        b.wasted_tokens
            .cmp(&a.wasted_tokens)
            .then_with(|| a.kind.cmp(&b.kind))
            .then_with(|| a.first_file.cmp(&b.first_file))
            .then_with(|| a.first_range.start.cmp(&b.first_range.start))
    });
    out
}

/// Scans `text` in a single pass for maintainability smells: deep brace nesting, high-complexity
/// god functions, and redundant boolean `if cond { true } else { false }` branches.
pub fn analyze_health<'a>(
    text: &str,
    limits: HealthLimits,
    alloc: Alloc<'a>,
) -> AVec<'a, HealthFinding> {
    let mut out = AVec::new_in(alloc);

    #[derive(Clone, Copy)]
    struct Tok {
        kind: TokenKind,
        start: u32,
        end: u32,
    }
    let mut toks = AVec::<Tok>::new_in(alloc);
    let mut pos = 0u32;
    for t in tokenize(text, FrontmatterAllowed::No, Cpu::BASELINE) {
        let start = pos;
        let end = pos + t.len;
        pos = end;
        if matches!(
            t.kind,
            TokenKind::Whitespace | TokenKind::LineComment { .. } | TokenKind::BlockComment { .. }
        ) {
            continue;
        }
        toks.push(Tok { kind: t.kind, start, end });
    }

    // 1. Scan functions for deep nesting and excessive complexity.
    let mut i = 0usize;
    while i + 3 < toks.len() {
        let t = toks[i];
        if t.kind == TokenKind::Ident && &text[t.start as usize..t.end as usize] == "fn" {
            let name_tok = toks[i + 1];
            let mut j = i + 2;
            let mut pdepth = 0usize;
            let mut open = None;
            while j < toks.len() {
                match toks[j].kind {
                    TokenKind::OpenParen | TokenKind::OpenBracket | TokenKind::Lt => pdepth += 1,
                    TokenKind::CloseParen | TokenKind::CloseBracket | TokenKind::Gt => {
                        pdepth = pdepth.saturating_sub(1);
                    }
                    TokenKind::Semi if pdepth == 0 => break,
                    TokenKind::OpenBrace if pdepth == 0 => {
                        open = Some(j);
                        break;
                    }
                    _ => {}
                }
                j += 1;
            }
            if let Some(body_open) = open {
                let mut depth = 1u32;
                let mut max_depth = 1u32;
                let mut branches = 0u32;
                let mut k = body_open + 1;
                while k < toks.len() && depth > 0 {
                    match toks[k].kind {
                        TokenKind::OpenBrace => {
                            depth += 1;
                            if depth > max_depth {
                                max_depth = depth;
                            }
                        }
                        TokenKind::CloseBrace => depth -= 1,
                        TokenKind::Ident => {
                            let w = &text[toks[k].start as usize..toks[k].end as usize];
                            if matches!(w, "if" | "match" | "for" | "while" | "loop") {
                                branches += 1;
                            }
                        }
                        TokenKind::And
                            if k + 1 < toks.len()
                                && toks[k + 1].kind == TokenKind::And
                                && toks[k].end == toks[k + 1].start =>
                        {
                            branches += 1;
                        }
                        TokenKind::Or
                            if k + 1 < toks.len()
                                && toks[k + 1].kind == TokenKind::Or
                                && toks[k].end == toks[k + 1].start =>
                        {
                            branches += 1;
                        }
                        _ => {}
                    }
                    if depth > 0 {
                        k += 1;
                    }
                }
                if depth == 0 {
                    let fn_span = Span::new(t.start, name_tok.end);
                    let nesting = max_depth.saturating_sub(1);
                    let body_tokens = k.saturating_sub(body_open + 1) as u32;
                    if nesting >= limits.max_nesting {
                        out.push(HealthFinding {
                            kind: HealthKind::DeepNesting,
                            range: fn_span,
                            metric: nesting,
                            limit: limits.max_nesting,
                        });
                    }
                    if branches >= limits.max_branches || body_tokens >= limits.max_fn_tokens {
                        let metric = if branches >= limits.max_branches { branches } else { body_tokens };
                        let limit = if branches >= limits.max_branches { limits.max_branches } else { limits.max_fn_tokens };
                        out.push(HealthFinding {
                            kind: HealthKind::HighComplexity,
                            range: fn_span,
                            metric,
                            limit,
                        });
                    }
                }
            }
        }
        // 2. Redundant `{ true } else { false }` or `{ false } else { true }`.
        if i + 6 < toks.len()
            && toks[i].kind == TokenKind::OpenBrace
            && toks[i + 1].kind == TokenKind::Ident
            && toks[i + 2].kind == TokenKind::CloseBrace
            && toks[i + 3].kind == TokenKind::Ident
            && &text[toks[i + 3].start as usize..toks[i + 3].end as usize] == "else"
            && toks[i + 4].kind == TokenKind::OpenBrace
            && toks[i + 5].kind == TokenKind::Ident
            && toks[i + 6].kind == TokenKind::CloseBrace
        {
            let a = &text[toks[i + 1].start as usize..toks[i + 1].end as usize];
            let b = &text[toks[i + 5].start as usize..toks[i + 5].end as usize];
            if matches!((a, b), ("true", "false") | ("false", "true")) {
                out.push(HealthFinding {
                    kind: HealthKind::RedundantBoolIf,
                    range: Span::new(toks[i].start, toks[i + 6].end),
                    metric: 1,
                    limit: 0,
                });
            }
        }
        i += 1;
    }

    out.sort_unstable_by(|a, b| a.range.start.cmp(&b.range.start).then_with(|| a.kind.cmp(&b.kind)));
    out
}
