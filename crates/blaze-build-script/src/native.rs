//! D65: the build scripts that run native, decided from text before the run
//! (build-scripts.md, "Native scripts (D65)").
//!
//! A script whose dependency closure holds a crate of [`TOOLCHAIN_CLASS`] starts a C compiler
//! or loads libclang, which wasm cannot: it is built for the host and run confined, with the
//! pinned clang the only compiler it can see. Every other script stays wasm. The choice is part
//! of the run's key ([`crate::key::RunnerKey`]); a wasm failure is never retried native
//! (rule 3).
//!
//! Confinement itself lives in `bstd::procs::hermetic` (D177), whose plan renders both the
//! macOS Seatbelt profile and the Linux Landlock ruleset from one [`bstd::procs::hermetic::HermeticSpec`].
#![blaze::sans_io]

use blaze_own::{AVec, Alloc, Vec};
use blaze_unit_graph::{UnitGraph, UnitId};

/// The crates that make a build script start a compiler or load libclang: `cc` (a C or C++
/// compiler and `ar`), `cmake` (cmake, then the compiler), `bindgen` and `clang-sys` (libclang
/// in the script's own process), `pkg-config` (the pkg-config program, which only native
/// scripts can be refused by name). Sorted. `autocfg` is not here: it spawns `rustc`, which the
/// wasm runner's probe shim answers.
pub const TOOLCHAIN_CLASS: [&str; 5] = ["bindgen", "cc", "clang-sys", "cmake", "pkg-config"];

/// The crates that do not compile for `wasm32-wasip1` / `wasm64-blaze-wasip1`: a script whose
/// dependency closure holds one has no wasm module in any mode, so it runs native in `check` too
/// ([`crate::toolchain::placement`]). `libloading` has no `Library` on wasi; `clang-sys` needs it
/// (its `runtime` feature, bindgen's default) or links libclang (`static`); `bindgen` holds
/// `clang-sys`. Protobuf/gRPC codegen build-deps (`prost-build`, `protobuf-build`,
/// `protoc-bin-vendored`, `tonic-build`, `tonic-prost-build`) spawn native `protoc` / ship host
/// binaries and require Tier-2 native execution. turso 2026-11-21: pg_query 18.1.0's and
/// turso_sdk_kit's scripts failed to build as wasm at clang-sys (`cannot find Library in
/// libloading`), though turso_sdk_kit's build.rs never calls bindgen on macOS: the class is what
/// the closure compiles, not what the script does. Sorted.
pub const NO_WASM_CLASS: [&str; 8] = [
    "bindgen",
    "clang-sys",
    "libloading",
    "prost-build",
    "protobuf-build",
    "protoc-bin-vendored",
    "tonic-build",
    "tonic-prost-build",
];

/// Why a build script's own Rust source text requires Tier-2 native host execution
/// ([`source_requires_native`]). Decided deterministically before compilation or execution
/// (rules 3 and 54).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum NativeSourceReason {
    /// Unconditional reference to host-OS-specific `std::os::{unix,linux,macos,fd}` APIs
    /// (absent on `wasm32-wasip1` / `wasm64-blaze-wasip1`), not guarded by a WASI-excluding
    /// `#[cfg(...)]`.
    HostOsStd,
    /// Unconditional foreign-function block (`extern "C" { ... }`, `extern "system" { ... }`,
    /// `extern { ... }`, or `unsafe extern ... { ... }`), not guarded by a WASI-excluding
    /// `#[cfg(...)]`.
    ExternC,
    /// Runtime host-OS-family check (`cfg!(target_family = "unix")` or `cfg!(unix)`) that
    /// expects the build script itself to run on a Unix host rather than WASI.
    HostFamilyCfg,
}

/// What a script's dependency closure says of where it can run ([`closure`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Closure {
    /// Native when the closure holds a crate of [`TOOLCHAIN_CLASS`] or [`NO_WASM_CLASS`].
    pub runner: Runner,
    /// The closure's unit of [`NO_WASM_CLASS`] with the least id: the script cannot be wasm.
    pub no_wasm: Option<UnitId>,
}

/// Where a build script runs.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Runner {
    /// `wasm32-wasip1` in Wasmtime.
    Wasm,
    /// Native, in the OS sandbox: `because` is the unit of [`TOOLCHAIN_CLASS`] in its closure
    /// with the least id (one answer for one graph).
    Native { because: UnitId },
}

/// Where `script` (a build-script unit) runs: [`closure`]'s runner.
pub fn runner(graph: &UnitGraph<'_>, script: UnitId, alloc: Alloc<'_>) -> Runner {
    closure(graph, script, alloc).runner
}

/// What `script`'s dependency closure says: native when a unit of a package named in
/// [`TOOLCHAIN_CLASS`] or [`NO_WASM_CLASS`] is in it, else wasm; and its least
/// [`NO_WASM_CLASS`] unit. The walk keeps its own stack (no recursion) and visits each unit once.
pub fn closure(graph: &UnitGraph<'_>, script: UnitId, alloc: Alloc<'_>) -> Closure {
    let count = graph.units.len();
    let mut seen: AVec<'_, bool> = Vec::with_capacity_in(count, alloc);
    for _ in 0..count {
        seen.push(false);
    }
    let mut stack: AVec<'_, UnitId> = Vec::new_in(alloc);
    for dep in graph.units[script.0 as usize].deps.iter() {
        stack.push(dep.unit);
    }
    let least = |held: Option<UnitId>, at: UnitId| {
        Some(match held {
            Some(found) if found.0 < at.0 => found,
            _ => at,
        })
    };
    let mut because: Option<UnitId> = None;
    let mut no_wasm: Option<UnitId> = None;
    while let Some(at) = stack.pop() {
        let index = at.0 as usize;
        if seen[index] {
            continue;
        }
        seen[index] = true;
        let unit = &graph.units[index];
        let name = unit.name.as_str();
        let cannot_be_wasm = NO_WASM_CLASS.binary_search(&name).is_ok();
        if cannot_be_wasm {
            no_wasm = least(no_wasm, at);
        }
        if cannot_be_wasm || TOOLCHAIN_CLASS.binary_search(&name).is_ok() {
            because = least(because, at);
        }
        for dep in unit.deps.iter() {
            stack.push(dep.unit);
        }
    }
    let runner = match because {
        Some(because) => Runner::Native { because },
        None => Runner::Wasm,
    };
    Closure { runner, no_wasm }
}

/// Single source of truth for deterministic pre-run build-script placement (rule 30): combines
/// closure-based placement ([`crate::toolchain::placement`]) with static inspection of the build
/// script's own Rust source ([`source_requires_native`]).
///
/// When `source` is `Some(src)` and [`source_requires_native`] detects an unguarded host-OS API,
/// `extern "..." { ... }` block, or Unix host-family `cfg!(...)`, places the script on
/// [`Runner::Native`] (`because: script`) in every mode before any compilation or execution
/// begins (rules 3 and 54).
pub fn placement_with_source(
    closure: Closure,
    mode: crate::toolchain::Mode,
    package: &str,
    script: UnitId,
    source: Option<&str>,
) -> Runner {
    let base = crate::toolchain::placement(closure, mode, package);
    if matches!(base, Runner::Native { .. }) {
        return base;
    }
    if let Some(src) = source
        && source_requires_native(src).is_some()
    {
        return Runner::Native { because: script };
    }
    base
}

/// Inspects a build script's Rust source text (zero allocations, sans-IO, exact Rust lexical
/// scanning) and returns `Some(reason)` when the source unconditionally requires Tier-2 native
/// host execution rather than `wasm32-wasip1` / `wasm64-blaze-wasip1`.
pub fn source_requires_native(source: &str) -> Option<NativeSourceReason> {
    let mut lex = Lexer::new(source);
    let mut brace_stack = [false; 64];
    let mut brace_len: usize = 0;
    let mut brace_overflow: usize = 0;
    let mut excluded_depth: usize = 0;
    let mut paren_depth: usize = 0;
    let mut pending_excluded = false;
    let mut pending_depth: usize = 0;
    let mut stmt_needs_semi = false;
    let mut prev: Option<Token<'_>> = None;
    let mut prev_prev: Option<Token<'_>> = None;

    while let Some(tok) = lex.next_token() {
        let brace_depth = brace_len + brace_overflow;

        if tok == Token::Pound {
            let mut look = lex;
            let inner = match look.next_token() {
                Some(Token::Bang) => {
                    if look.next_token() == Some(Token::LBracket) {
                        true
                    } else {
                        prev_prev = prev;
                        prev = Some(tok);
                        continue;
                    }
                }
                Some(Token::LBracket) => false,
                _ => {
                    prev_prev = prev;
                    prev = Some(tok);
                    continue;
                }
            };
            lex = look;
            let cfg_excluded = parse_attr_and_check_wasi_excluded(&mut lex);
            if cfg_excluded {
                if inner {
                    if brace_depth == 0 {
                        return None;
                    }
                    if brace_overflow == 0
                        && let Some(slot) = brace_len.checked_sub(1).and_then(|idx| brace_stack.get_mut(idx))
                        && !*slot
                    {
                        *slot = true;
                        excluded_depth += 1;
                    }
                } else if excluded_depth == 0 {
                    pending_excluded = true;
                    pending_depth = brace_depth;
                }
            }
            prev_prev = prev;
            prev = Some(Token::RBracket);
            continue;
        }

        let is_excluded = excluded_depth > 0 || pending_excluded;

        if pending_excluded && brace_depth == pending_depth && paren_depth == 0 {
            match tok {
                Token::Ident("let" | "use" | "static" | "type" | "const") => {
                    stmt_needs_semi = true;
                }
                Token::Ident("fn") => {
                    stmt_needs_semi = false;
                }
                Token::Semi => {
                    pending_excluded = false;
                    stmt_needs_semi = false;
                }
                _ => {}
            }
        }

        match tok {
            Token::LParen | Token::LBracket => {
                paren_depth += 1;
            }
            Token::RParen | Token::RBracket => {
                paren_depth = paren_depth.saturating_sub(1);
            }
            Token::LBrace => {
                let this_excluded = pending_excluded && brace_depth == pending_depth;
                if let Some(slot) = brace_stack.get_mut(brace_len) {
                    *slot = this_excluded;
                    brace_len += 1;
                } else {
                    brace_overflow += 1;
                }
                if this_excluded {
                    excluded_depth += 1;
                }
            }
            Token::RBrace => {
                if brace_overflow > 0 {
                    brace_overflow -= 1;
                } else if let Some(new_len) = brace_len.checked_sub(1) {
                    brace_len = new_len;
                    if brace_stack.get(brace_len).copied().unwrap_or(false) {
                        excluded_depth = excluded_depth.saturating_sub(1);
                    }
                }
                let new_depth = brace_len + brace_overflow;
                if pending_excluded {
                    if new_depth < pending_depth {
                        pending_excluded = false;
                        stmt_needs_semi = false;
                    } else if new_depth == pending_depth && !stmt_needs_semi {
                        let mut look = lex;
                        if look.next_token() != Some(Token::Ident("else")) {
                            pending_excluded = false;
                        }
                    }
                }
            }
            _ => {}
        }

        if !is_excluded {
            match tok {
                Token::Ident("std") if is_root_std_path(prev, prev_prev) => {
                    let mut look = lex;
                    if look.next_token() == Some(Token::ColonColon)
                        && look.next_token() == Some(Token::Ident("os"))
                        && look.next_token() == Some(Token::ColonColon)
                    {
                        match look.next_token() {
                            Some(Token::Ident("unix" | "linux" | "macos" | "fd")) => {
                                return Some(NativeSourceReason::HostOsStd);
                            }
                            Some(Token::LBrace) if brace_group_mentions_host_os(&mut look) => {
                                return Some(NativeSourceReason::HostOsStd);
                            }
                            _ => {}
                        }
                    }
                }
                Token::Ident("extern") => {
                    let mut look = lex;
                    match look.next_token() {
                        Some(Token::LBrace) => return Some(NativeSourceReason::ExternC),
                        Some(Token::Str(_)) if look.next_token() == Some(Token::LBrace) => {
                            return Some(NativeSourceReason::ExternC);
                        }
                        _ => {}
                    }
                }
                Token::Ident("cfg") => {
                    let mut look = lex;
                    if look.next_token() == Some(Token::Bang)
                        && let Some(open) = look.next_token()
                        && let Some(close) = closing_delim(open)
                    {
                        let mut body = look;
                        if cfg_macro_requires_unix_family(&mut body, 0) {
                            return Some(NativeSourceReason::HostFamilyCfg);
                        }
                        skip_delimited(&mut look, open, close);
                        lex = look;
                        prev_prev = prev;
                        prev = Some(close);
                        continue;
                    }
                }
                _ => {}
            }
        }

        prev_prev = prev;
        prev = Some(tok);
    }

    None
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Token<'a> {
    Ident(&'a str),
    Str(&'a str),
    RawStr,
    ColonColon,
    Pound,
    Bang,
    Eq,
    Comma,
    Semi,
    LParen,
    RParen,
    LBracket,
    RBracket,
    LBrace,
    RBrace,
    Other,
}

#[derive(Clone, Copy)]
struct Lexer<'a> {
    src: &'a str,
    at: usize,
}

impl<'a> Lexer<'a> {
    fn new(src: &'a str) -> Self {
        Self { src, at: 0 }
    }

    fn peek_byte(&self, offset: usize) -> Option<u8> {
        self.src.as_bytes().get(self.at.checked_add(offset)?).copied()
    }

    fn rest(&self) -> &'a str {
        self.src.get(self.at..).unwrap_or("")
    }

    fn skip_ws_and_comments(&mut self) {
        loop {
            while let Some(b) = self.peek_byte(0) {
                if b.is_ascii_whitespace() {
                    self.at += 1;
                } else if b >= 0x80 {
                    let Some(ch) = self.rest().chars().next() else { break };
                    if ch.is_whitespace() {
                        self.at += ch.len_utf8();
                    } else {
                        break;
                    }
                } else {
                    break;
                }
            }
            match (self.peek_byte(0), self.peek_byte(1)) {
                (Some(b'/'), Some(b'/')) => {
                    self.at += 2;
                    while let Some(b) = self.peek_byte(0) {
                        self.at += 1;
                        if b == b'\n' {
                            break;
                        }
                    }
                }
                (Some(b'/'), Some(b'*')) => {
                    self.at += 2;
                    let mut depth: usize = 1;
                    while depth > 0 {
                        match (self.peek_byte(0), self.peek_byte(1)) {
                            (Some(b'/'), Some(b'*')) => {
                                self.at += 2;
                                depth += 1;
                            }
                            (Some(b'*'), Some(b'/')) => {
                                self.at += 2;
                                depth -= 1;
                            }
                            (Some(_), _) => {
                                self.at += 1;
                            }
                            (None, _) => break,
                        }
                    }
                }
                _ => break,
            }
        }
    }

    fn try_scan_raw_string(&mut self, prefix_len: usize) -> bool {
        let mut off = prefix_len;
        let mut hashes: usize = 0;
        while self.peek_byte(off) == Some(b'#') {
            hashes += 1;
            off += 1;
        }
        if self.peek_byte(off) != Some(b'"') {
            return false;
        }
        self.at += off + 1;
        while let Some(b) = self.peek_byte(0) {
            self.at += 1;
            if b == b'"' {
                let mut matched = 0;
                while matched < hashes && self.peek_byte(matched) == Some(b'#') {
                    matched += 1;
                }
                if matched == hashes {
                    self.at += hashes;
                    break;
                }
            }
        }
        true
    }

    fn scan_quoted_string(&mut self) -> &'a str {
        // Opening `"` already verified at `self.at`.
        self.at += 1;
        let start = self.at;
        let mut end = self.at;
        while let Some(b) = self.peek_byte(0) {
            if b == b'\\' {
                self.at += 1;
                if self.peek_byte(0).is_some() {
                    self.at += 1;
                }
            } else if b == b'"' {
                end = self.at;
                self.at += 1;
                break;
            } else {
                self.at += 1;
            }
        }
        self.src.get(start..end).unwrap_or("")
    }

    fn scan_char_or_lifetime(&mut self) {
        // Opening `'` at `self.at`.
        self.at += 1;
        if self.peek_byte(0) == Some(b'\\') {
            // Escaped char literal: `'\\`, `'\''`, `'\n'`, `'\u{...}'`, etc.
            self.at += 1;
            if self.peek_byte(0).is_some() {
                self.at += 1;
            }
            while let Some(b) = self.peek_byte(0) {
                self.at += 1;
                if b == b'\'' || b == b'\n' {
                    break;
                }
            }
            return;
        }
        if let Some(ch) = self.rest().chars().next() {
            let after = ch.len_utf8();
            if self.peek_byte(after) == Some(b'\'') {
                // Single-char literal `'x'`.
                self.at += after + 1;
                return;
            }
            // Lifetime `'a`, `'static`, `'_`, `'r#foo`.
            if ch == 'r' && self.peek_byte(1) == Some(b'#') {
                self.at += 2;
            }
            self.skip_ident_tail();
        }
    }

    fn scan_byte_char_literal(&mut self) {
        // `b'` at `self.at`.
        self.at += 2;
        while let Some(b) = self.peek_byte(0) {
            if b == b'\\' {
                self.at += 1;
                if self.peek_byte(0).is_some() {
                    self.at += 1;
                }
            } else {
                self.at += 1;
                if b == b'\'' || b == b'\n' {
                    break;
                }
            }
        }
    }

    fn skip_ident_tail(&mut self) {
        while let Some(b) = self.peek_byte(0) {
            if b.is_ascii_alphanumeric() || b == b'_' {
                self.at += 1;
            } else if b >= 0x80 {
                let Some(ch) = self.rest().chars().next() else { break };
                if ch.is_alphanumeric() || ch == '_' {
                    self.at += ch.len_utf8();
                } else {
                    break;
                }
            } else {
                break;
            }
        }
    }

    fn next_token(&mut self) -> Option<Token<'a>> {
        self.skip_ws_and_comments();
        let b = self.peek_byte(0)?;
        match b {
            b'"' => Some(Token::Str(self.scan_quoted_string())),
            b'\'' => {
                self.scan_char_or_lifetime();
                Some(Token::Other)
            }
            b'r' => {
                if self.try_scan_raw_string(1) {
                    return Some(Token::RawStr);
                }
                if self.peek_byte(1) == Some(b'#') {
                    // Raw identifier `r#ident`: never treated as a keyword (`r#extern` != `extern`).
                    self.at += 2;
                    self.skip_ident_tail();
                    return Some(Token::Other);
                }
                let start = self.at;
                self.at += 1;
                self.skip_ident_tail();
                Some(Token::Ident(self.src.get(start..self.at).unwrap_or("")))
            }
            b'b' | b'c' => {
                if self.peek_byte(1) == Some(b'"') {
                    self.at += 1;
                    let _ = self.scan_quoted_string();
                    return Some(Token::RawStr);
                }
                if b == b'b' && self.peek_byte(1) == Some(b'\'') {
                    self.scan_byte_char_literal();
                    return Some(Token::Other);
                }
                if self.peek_byte(1) == Some(b'r') && self.try_scan_raw_string(2) {
                    return Some(Token::RawStr);
                }
                let start = self.at;
                self.at += 1;
                self.skip_ident_tail();
                Some(Token::Ident(self.src.get(start..self.at).unwrap_or("")))
            }
            b':' if self.peek_byte(1) == Some(b':') => {
                self.at += 2;
                Some(Token::ColonColon)
            }
            b'#' => {
                self.at += 1;
                Some(Token::Pound)
            }
            b'!' => {
                self.at += 1;
                Some(Token::Bang)
            }
            b'=' => {
                self.at += 1;
                Some(Token::Eq)
            }
            b',' => {
                self.at += 1;
                Some(Token::Comma)
            }
            b';' => {
                self.at += 1;
                Some(Token::Semi)
            }
            b'(' => {
                self.at += 1;
                Some(Token::LParen)
            }
            b')' => {
                self.at += 1;
                Some(Token::RParen)
            }
            b'[' => {
                self.at += 1;
                Some(Token::LBracket)
            }
            b']' => {
                self.at += 1;
                Some(Token::RBracket)
            }
            b'{' => {
                self.at += 1;
                Some(Token::LBrace)
            }
            b'}' => {
                self.at += 1;
                Some(Token::RBrace)
            }
            _ if b.is_ascii_alphabetic() || b == b'_' => {
                let start = self.at;
                self.at += 1;
                self.skip_ident_tail();
                Some(Token::Ident(self.src.get(start..self.at).unwrap_or("")))
            }
            _ if b >= 0x80 => {
                let start = self.at;
                if let Some(ch) = self.rest().chars().next() {
                    self.at += ch.len_utf8();
                    if ch.is_alphabetic() || ch == '_' {
                        self.skip_ident_tail();
                        return Some(Token::Ident(self.src.get(start..self.at).unwrap_or("")));
                    }
                } else {
                    self.at += 1;
                }
                Some(Token::Other)
            }
            _ => {
                self.at += 1;
                Some(Token::Other)
            }
        }
    }
}

fn closing_delim(open: Token<'_>) -> Option<Token<'_>> {
    match open {
        Token::LParen => Some(Token::RParen),
        Token::LBracket => Some(Token::RBracket),
        Token::LBrace => Some(Token::RBrace),
        _ => None,
    }
}

fn skip_delimited(lex: &mut Lexer<'_>, open: Token<'_>, close: Token<'_>) {
    let mut depth: usize = 1;
    while let Some(tok) = lex.next_token() {
        if tok == open {
            depth += 1;
        } else if tok == close {
            depth -= 1;
            if depth == 0 {
                break;
            }
        }
    }
}

/// Returns `true` when `std` is at the start of a root path (`std::...` or `::std::...`) rather
/// than a submodule of another path (`my_mod::std::...`).
fn is_root_std_path(prev: Option<Token<'_>>, prev_prev: Option<Token<'_>>) -> bool {
    if prev != Some(Token::ColonColon) {
        return true;
    }
    match prev_prev {
        Some(Token::Ident(word)) => matches!(
            word,
            "use"
                | "pub"
                | "return"
                | "let"
                | "mut"
                | "ref"
                | "in"
                | "if"
                | "else"
                | "match"
                | "while"
                | "for"
                | "loop"
                | "as"
                | "where"
                | "impl"
                | "fn"
                | "const"
                | "static"
                | "type"
                | "struct"
                | "enum"
                | "trait"
                | "unsafe"
                | "async"
                | "move"
                | "dyn"
                | "crate"
        ),
        _ => true,
    }
}

/// Inspects a grouped `use std::os::{ ... }` list right after `{` for `unix`, `linux`, `macos`,
/// or `fd` at top level of the group.
fn brace_group_mentions_host_os(lex: &mut Lexer<'_>) -> bool {
    let mut depth: usize = 1;
    let mut at_item_start = true;
    while let Some(tok) = lex.next_token() {
        match tok {
            Token::LBrace => {
                depth += 1;
                at_item_start = false;
            }
            Token::RBrace => {
                depth -= 1;
                if depth == 0 {
                    return false;
                }
            }
            Token::Comma if depth == 1 => {
                at_item_start = true;
            }
            Token::Ident("unix" | "linux" | "macos" | "fd") if depth == 1 && at_item_start => {
                return true;
            }
            _ => {
                at_item_start = false;
            }
        }
    }
    false
}

/// 3-valued truth value of a `#[cfg(...)]` predicate evaluated on a WASI target
/// (`target_family = "wasm"`, `target_os = "wasi"`).
#[derive(Clone, Copy, PartialEq, Eq)]
enum TriBool {
    True,
    False,
    Unknown,
}

/// Consumes the body of an attribute `[...]` (starting immediately after `[`) and returns `true`
/// if it is `cfg(...)` that evaluates to `TriBool::False` on WASI.
fn parse_attr_and_check_wasi_excluded(lex: &mut Lexer<'_>) -> bool {
    let mut look = *lex;
    let excluded = if look.next_token() == Some(Token::Ident("cfg")) && look.next_token() == Some(Token::LParen) {
        eval_cfg_wasi(&mut look, 0) == TriBool::False
    } else {
        false
    };
    skip_delimited(lex, Token::LBracket, Token::RBracket);
    excluded
}

fn eval_cfg_wasi(lex: &mut Lexer<'_>, depth: usize) -> TriBool {
    if depth > 16 {
        return TriBool::Unknown;
    }
    let Some(tok) = lex.next_token() else { return TriBool::Unknown };
    let Token::Ident(name) = tok else { return TriBool::Unknown };
    let mut look = *lex;
    match look.next_token() {
        Some(Token::LParen) => {
            *lex = look;
            match name {
                "all" => {
                    let mut acc = TriBool::True;
                    loop {
                        let mut peek = *lex;
                        if matches!(peek.next_token(), None | Some(Token::RParen)) {
                            *lex = peek;
                            break;
                        }
                        let sub = eval_cfg_wasi(lex, depth + 1);
                        acc = match (acc, sub) {
                            (TriBool::False, _) | (_, TriBool::False) => TriBool::False,
                            (TriBool::True, TriBool::True) => TriBool::True,
                            _ => TriBool::Unknown,
                        };
                        let mut after = *lex;
                        match after.next_token() {
                            Some(Token::Comma) => *lex = after,
                            Some(Token::RParen) | None => {
                                *lex = after;
                                break;
                            }
                            _ => {
                                skip_delimited(lex, Token::LParen, Token::RParen);
                                break;
                            }
                        }
                    }
                    acc
                }
                "any" => {
                    let mut acc = TriBool::False;
                    loop {
                        let mut peek = *lex;
                        if matches!(peek.next_token(), None | Some(Token::RParen)) {
                            *lex = peek;
                            break;
                        }
                        let sub = eval_cfg_wasi(lex, depth + 1);
                        acc = match (acc, sub) {
                            (TriBool::True, _) | (_, TriBool::True) => TriBool::True,
                            (TriBool::False, TriBool::False) => TriBool::False,
                            _ => TriBool::Unknown,
                        };
                        let mut after = *lex;
                        match after.next_token() {
                            Some(Token::Comma) => *lex = after,
                            Some(Token::RParen) | None => {
                                *lex = after;
                                break;
                            }
                            _ => {
                                skip_delimited(lex, Token::LParen, Token::RParen);
                                break;
                            }
                        }
                    }
                    acc
                }
                "not" => {
                    let sub = eval_cfg_wasi(lex, depth + 1);
                    let mut after = *lex;
                    if after.next_token() == Some(Token::RParen) {
                        *lex = after;
                    } else {
                        skip_delimited(lex, Token::LParen, Token::RParen);
                    }
                    match sub {
                        TriBool::True => TriBool::False,
                        TriBool::False => TriBool::True,
                        TriBool::Unknown => TriBool::Unknown,
                    }
                }
                _ => {
                    skip_delimited(lex, Token::LParen, Token::RParen);
                    TriBool::Unknown
                }
            }
        }
        Some(Token::Eq) => {
            *lex = look;
            let val = match lex.next_token() {
                Some(Token::Str(s)) => s,
                _ => return TriBool::Unknown,
            };
            match (name, val) {
                ("target_family", "wasm") => TriBool::True,
                ("target_family", _) => TriBool::False,
                ("target_os", "wasi") => TriBool::True,
                ("target_os", _) => TriBool::False,
                ("target_arch", "wasm32" | "wasm64") => TriBool::Unknown,
                ("target_arch", _) => TriBool::False,
                ("target_env", "gnu" | "msvc" | "musl" | "sgx" | "ohos") => TriBool::False,
                _ => TriBool::Unknown,
            }
        }
        _ => match name {
            "unix" | "windows" => TriBool::False,
            _ => TriBool::Unknown,
        },
    }
}

/// Returns `true` when a runtime `cfg!(...)` predicate checks specifically for a Unix host family
/// (`cfg!(target_family = "unix")` or `cfg!(unix)`), without matching WASI/Wasm.
///
/// Deliberately does NOT match `cfg!(target_os = "linux")` / `cfg!(target_os = "macos")` /
/// `cfg!(windows)`, which pure-Rust Wasm scripts (`anyhow`, `proc-macro2`, `thiserror`) use for
/// best-effort host quirks (e.g. Linux `ENOTEMPTY` cleanup) and work cleanly under WASI.
fn cfg_macro_requires_unix_family(lex: &mut Lexer<'_>, depth: usize) -> bool {
    if depth > 16 {
        return false;
    }
    let Some(Token::Ident(name)) = lex.next_token() else { return false };
    let mut look = *lex;
    match look.next_token() {
        Some(Token::LParen) => {
            *lex = look;
            match name {
                "all" => {
                    let mut saw_unix = false;
                    let mut saw_wasi = false;
                    loop {
                        let mut peek = *lex;
                        if matches!(peek.next_token(), None | Some(Token::RParen)) {
                            *lex = peek;
                            break;
                        }
                        let mut eval_copy = *lex;
                        if eval_cfg_wasi(&mut eval_copy, depth + 1) == TriBool::True {
                            saw_wasi = true;
                        }
                        if cfg_macro_requires_unix_family(lex, depth + 1) {
                            saw_unix = true;
                        }
                        let mut after = *lex;
                        match after.next_token() {
                            Some(Token::Comma) => *lex = after,
                            Some(Token::RParen) | None => {
                                *lex = after;
                                break;
                            }
                            _ => {
                                skip_delimited(lex, Token::LParen, Token::RParen);
                                break;
                            }
                        }
                    }
                    saw_unix && !saw_wasi
                }
                "any" => {
                    let mut saw_unix = false;
                    let mut handles_wasi = false;
                    loop {
                        let mut peek = *lex;
                        if matches!(peek.next_token(), None | Some(Token::RParen)) {
                            *lex = peek;
                            break;
                        }
                        let mut eval_copy = *lex;
                        if eval_cfg_wasi(&mut eval_copy, depth + 1) == TriBool::True {
                            handles_wasi = true;
                        }
                        if cfg_macro_requires_unix_family(lex, depth + 1) {
                            saw_unix = true;
                        }
                        let mut after = *lex;
                        match after.next_token() {
                            Some(Token::Comma) => *lex = after,
                            Some(Token::RParen) | None => {
                                *lex = after;
                                break;
                            }
                            _ => {
                                skip_delimited(lex, Token::LParen, Token::RParen);
                                break;
                            }
                        }
                    }
                    saw_unix && !handles_wasi
                }
                _ => {
                    skip_delimited(lex, Token::LParen, Token::RParen);
                    false
                }
            }
        }
        Some(Token::Eq) => {
            *lex = look;
            let Some(Token::Str(val)) = lex.next_token() else { return false };
            name == "target_family" && val == "unix"
        }
        _ => name == "unix",
    }
}
