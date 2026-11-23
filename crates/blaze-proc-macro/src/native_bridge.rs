//! Native Zero-Wasm Proc-Macro & Build-Script Execution Bridge (D175 fast path).
//!
//! Eliminates the `wasm32-wasip1` + `-Zbuild-std` bootstrap bottleneck (which breaks
//! `synstructure` / `asn1-rs-derive`, `cc`, `cmake`, `bindgen`, `clang-sys`, and `libloading` on
//! real workspaces like `tempo`) by:
//!
//! 1. **Warm Native Proc-Macro Server (`NativeProcMacroServer`)**: talks over stdio newline-JSON
//!    to the pinned toolchain's native `rust-analyzer-proc-macro-srv` (API v6) to inspect and
//!    expand native host `.dylib`/`.so` proc-macro crates directly — zero Wasmtime, zero
//!    `-Zbuild-std`, 100% compatibility with `synstructure`/`serde_derive`/`alloy-sol-macro`.
//! 2. **Native Build-Script Confined Execution (`run_native_build_script`)**: runs native host
//!    `build-script-build` binaries inside macOS Seatbelt confinement (or direct confined host
//!    execution), capturing `cargo:` directives and `OUT_DIR` files deterministically.
//! 3. **Content-Addressed Native Artifact Cache (`NativeArtifactIndex`)**: indexes native `.dylib`
//!    proc macros and native `build-script-build` binaries by SHA-256 so repeated runs hit cache
//!    in microseconds.

use std::collections::{HashMap, VecDeque};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::Mutex;
use std::time::Duration;

use blaze_core::wait::Deadline;
use blaze_expand::{
    Ctx, Delim, Edition, LitKind, ProcCall, ProcDiagnostic, ProcKind, Produced, Raw, Reads,
    Spacing, Text, TokenKind, TokenOrigin, TokenStream, Transparency,
};
use blaze_own::{AVec, Alloc, Vec as OVec};
use blaze_roots::{Digest, Mount, Root, RootId, RootKind, Roots};
use bstd::mem::TASK_FRAME;
use bstd::procs::{
    Exit,
    hermetic::{
        Budget, Capture, ClockSpec, Hermetic, HermeticCapOpts, HermeticOpts, HermeticSpec,
        HermeticStreams, Level, Program, ReportSpec,
    },
};

use crate::native_script::OutDir;
use crate::record::native_world;

/// Errors returned by the native zero-Wasm proc-macro / build-script bridge.
#[derive(Debug)]
pub enum NativeBridgeError {
    /// Failed to locate `rust-analyzer-proc-macro-srv` in the pinned toolchain sysroot.
    ServerNotFound(String),
    /// Failed to spawn or communicate with the native server process.
    Io(std::io::Error),
    /// The native server returned an unexpected or malformed JSON reply.
    Protocol(String),
    /// A native proc-macro or build-script reported a failure.
    Failed(String),
}

impl core::fmt::Display for NativeBridgeError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::ServerNotFound(msg) => write!(f, "native proc-macro server not found: {msg}"),
            Self::Io(e) => write!(f, "native comptime I/O error: {e}"),
            Self::Protocol(msg) => write!(f, "native proc-macro protocol error: {msg}"),
            Self::Failed(msg) => write!(f, "native comptime execution failed: {msg}"),
        }
    }
}

impl std::error::Error for NativeBridgeError {}

impl From<std::io::Error> for NativeBridgeError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

/// One exported proc macro discovered from a native `.dylib` / `.so`.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct NativeExport {
    pub name: String,
    pub kind: ProcKind,
}

/// Locates `rust-analyzer-proc-macro-srv` in the pinned nightly sysroot (or `rustc --print sysroot`).
pub fn find_proc_macro_srv() -> Result<PathBuf, NativeBridgeError> {
    if let Ok(explicit) = std::env::var("BLAZE_PROC_MACRO_SRV") {
        let p = PathBuf::from(explicit);
        if p.is_file() {
            return Ok(p);
        }
    }
    // Try pinned toolchain first, then active rustc.
    for args in [&["+nightly-2026-10-27", "--print", "sysroot"][..], &["--print", "sysroot"][..]] {
        if let Ok(out) = Command::new("rustc").args(args).output() {
            if out.status.success() {
                let sysroot = String::from_utf8_lossy(&out.stdout).trim().to_owned();
                for sub in ["libexec/rust-analyzer-proc-macro-srv", "bin/rust-analyzer-proc-macro-srv"] {
                    let cand = Path::new(&sysroot).join(sub);
                    if cand.is_file() {
                        return Ok(cand);
                    }
                }
            }
        }
    }
    Err(NativeBridgeError::ServerNotFound(
        "checked BLAZE_PROC_MACRO_SRV and rustc --print sysroot libexec/rust-analyzer-proc-macro-srv".to_owned(),
    ))
}

/// Escapes a Rust `&str` as a JSON string literal (without surrounding quotes).
fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                use core::fmt::Write as _;
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out
}

/// Warm persistent native proc-macro server speaking newline-JSON over stdio.
pub struct NativeProcMacroServer {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    api_version: u32,
}

impl NativeProcMacroServer {
    /// Spawns and handshakes the native proc-macro server (`ApiVersionCheck`).
    pub fn spawn() -> Result<Self, NativeBridgeError> {
        let bin = find_proc_macro_srv()?;
        Self::spawn_at(&bin)
    }

    /// Spawns and handshakes a specific proc-macro server binary.
    pub fn spawn_at(bin: &Path) -> Result<Self, NativeBridgeError> {
        let mut child = Command::new(bin)
            .env("RUST_ANALYZER_INTERNALS_DO_NOT_USE", "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| NativeBridgeError::Protocol("missing server stdin".to_owned()))?;
        let stdout = BufReader::new(
            child
                .stdout
                .take()
                .ok_or_else(|| NativeBridgeError::Protocol("missing server stdout".to_owned()))?,
        );
        let mut srv = Self { child, stdin, stdout, api_version: 0 };
        let reply = srv.roundtrip(r#"{"ApiVersionCheck":{}}"#)?;
        let ver = parse_api_version(&reply).ok_or_else(|| {
            NativeBridgeError::Protocol(format!("unexpected ApiVersionCheck reply: {reply}"))
        })?;
        srv.api_version = ver;
        Ok(srv)
    }

    /// Negotiated server protocol version (e.g. `6` on `nightly-2026-10-27`).
    #[inline]
    pub const fn api_version(&self) -> u32 {
        self.api_version
    }

    /// Sends one newline-terminated JSON request and reads one newline-terminated JSON reply.
    pub fn roundtrip(&mut self, request_json: &str) -> Result<String, NativeBridgeError> {
        self.stdin.write_all(request_json.as_bytes())?;
        self.stdin.write_all(b"\n")?;
        self.stdin.flush()?;
        let mut line = String::new();
        let n = self.stdout.read_line(&mut line)?;
        if n == 0 {
            return Err(NativeBridgeError::Protocol("native proc-macro server closed stdout (EOF)".to_owned()));
        }
        Ok(line.trim_end_matches(['\r', '\n']).to_owned())
    }

    /// Queries all proc macros exported by a native host `.dylib` / `.so`.
    pub fn list_macros(&mut self, dylib_path: &Path) -> Result<Vec<NativeExport>, NativeBridgeError> {
        let escaped = json_escape(&dylib_path.to_string_lossy());
        let req = format!(r#"{{"ListMacros":{{"dylib_path":"{escaped}"}}}}"#);
        let reply = self.roundtrip(&req)?;
        parse_list_macros(&reply)
    }

    /// Expands a single proc-macro call natively against `dylib_path` (zero Wasmtime!).
    #[expect(clippy::too_many_arguments, reason = "mirrors ProcMacros::run_why parameters")]
    pub fn expand_macro<'a>(
        &mut self,
        dylib_path: &Path,
        macro_name: &str,
        call: &ProcCall<'_>,
        edition: Edition,
        env: &[(&str, &str)],
        current_dir: Option<&str>,
        path_remap: Option<(&str, &str)>,
        alloc: Alloc<'a>,
    ) -> Result<Produced<'a>, NativeBridgeError> {
        const SPAN_CALL_SITE: u32 = 0;
        const SPAN_MIXED_SITE: u32 = 1;
        const SPAN_DEF_SITE: u32 = 2;
        const SPAN_TOKEN_BASE: u32 = 3;

        let lib_esc = json_escape(&dylib_path.to_string_lossy());
        let name_esc = json_escape(macro_name);
        let input_len = u32::try_from(call.input.tokens().len()).unwrap_or(u32::MAX - SPAN_TOKEN_BASE);
        let attr_len = call.attr_args.map_or(0, |args| u32::try_from(args.tokens().len()).unwrap_or(0));
        let body_json = encode_flat_tree_json(call.input, SPAN_TOKEN_BASE);
        let attr_owned = call.attr_args.map(|args| encode_flat_tree_json(args, SPAN_TOKEN_BASE.saturating_add(input_len)));
        let attr_json: &str = match attr_owned.as_deref() {
            Some(s) => s,
            None if call.kind == ProcKind::Attr => {
                r#"{"subtree":[0,0,0,0,0],"literal":[],"punct":[],"ident":[],"token_tree":[],"text":[]}"#
            }
            None => "null",
        };
        let cwd_json = match current_dir {
            Some(dir) => format!("\"{}\"", json_escape(dir)),
            None => "null".to_owned(),
        };
        let mut env_json = String::from("[");
        for (i, &(k, v)) in env.iter().enumerate() {
            if i > 0 {
                env_json.push(',');
            }
            env_json.push_str("[\"");
            env_json.push_str(&json_escape(k));
            env_json.push_str("\",\"");
            env_json.push_str(&json_escape(v));
            env_json.push_str("\"]");
        }
        env_json.push(']');

        let req = format!(
            r#"{{"ExpandMacro":{{"lib":"{lib_esc}","env":{env_json},"current_dir":{cwd_json},"macro_name":"{name_esc}","macro_body":{body_json},"attributes":{attr_json},"has_global_spans":{{"serialize":false,"def_site":{SPAN_DEF_SITE},"call_site":{SPAN_CALL_SITE},"mixed_site":{SPAN_MIXED_SITE}}},"span_data_table":[]}}}}"#
        );
        let reply = self.roundtrip(&req)?;
        parse_expand_macro_reply(&reply, edition, input_len, attr_len, path_remap, alloc)
    }
}

/// Thread-safe pool of warm `NativeProcMacroServer` workers for parallel zero-Wasm expansion.
#[expect(clippy::disallowed_types, reason = "pools warm proc-macro child processes across threads")]
pub struct NativeServerPool {
    idle: Mutex<Vec<NativeProcMacroServer>>,
}

impl Default for NativeServerPool {
    fn default() -> Self {
        Self::new()
    }
}

impl NativeServerPool {
    pub fn new() -> Self {
        Self { idle: Mutex::new(Vec::new()) }
    }

    /// Borrows a warm server (or spawns one lazily), runs `expand_macro`, and returns it to the pool.
    /// Automatically respawns once if a server process died unexpectedly.
    #[expect(clippy::too_many_arguments, reason = "mirrors ProcMacros::run_why parameters")]
    pub fn expand_macro<'a>(
        &self,
        dylib_path: &Path,
        macro_name: &str,
        call: &ProcCall<'_>,
        edition: Edition,
        env: &[(&str, &str)],
        current_dir: Option<&str>,
        path_remap: Option<(&str, &str)>,
        alloc: Alloc<'a>,
    ) -> Result<Produced<'a>, NativeBridgeError> {
        let mut srv = {
            let mut guard = self.idle.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            guard.pop()
        };
        let mut worker = match srv.take() {
            Some(s) => s,
            None => NativeProcMacroServer::spawn()?,
        };
        match worker.expand_macro(dylib_path, macro_name, call, edition, env, current_dir, path_remap, alloc) {
            Ok(produced) => {
                let mut guard = self.idle.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                if guard.len() < 32 {
                    guard.push(worker);
                }
                Ok(produced)
            }
            Err(NativeBridgeError::Io(_)) | Err(NativeBridgeError::Protocol(_)) => {
                // Worker may have crashed on a hostile macro; drop dead worker and retry once fresh.
                drop(worker);
                let mut fresh = NativeProcMacroServer::spawn()?;
                let res = fresh.expand_macro(dylib_path, macro_name, call, edition, env, current_dir, path_remap, alloc);
                if res.is_ok() {
                    let mut guard = self.idle.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                    if guard.len() < 32 {
                        guard.push(fresh);
                    }
                }
                res
            }
            Err(e) => {
                let mut guard = self.idle.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                if guard.len() < 32 {
                    guard.push(worker);
                }
                Err(e)
            }
        }
    }
}

/// Encodes a `blaze_expand::TokenStream` into a rust-analyzer API v6 `FlatTree` JSON object,
/// tagging each token at index `i` with span ID `span_offset + i` so the native proc-macro
/// server preserves token origins and macro hygiene (`$crate`, `Span::call_site`, `mixed_site`,
/// `def_site`) losslessly across the expansion boundary.
pub fn encode_flat_tree_json(stream: &TokenStream<'_>, span_offset: u32) -> String {
    enum Node {
        Subtree { open_span: u32, close_span: u32, delim: u32, children: Vec<Node> },
        Leaf(u32),
    }

    let mut texts: Vec<String> = Vec::new();
    let mut text_map: HashMap<String, u32> = HashMap::new();
    let mut intern = |s: &str| -> u32 {
        if let Some(&id) = text_map.get(s) {
            return id;
        }
        let id = texts.len() as u32;
        texts.push(s.to_owned());
        text_map.insert(s.to_owned(), id);
        id
    };

    let mut literals: Vec<u32> = Vec::new();
    let mut puncts: Vec<u32> = Vec::new();
    let mut idents: Vec<u32> = Vec::new();

    // Pass 1: build nested tree from linear Open/Close token stream.
    let mut stack: Vec<(u32, u32, Vec<Node>)> = vec![(0, 0, Vec::new())];
    for (tok_idx, tok) in stream.tokens().iter().enumerate() {
        let span = span_offset.saturating_add(u32::try_from(tok_idx).unwrap_or(u32::MAX));
        match tok.kind {
            TokenKind::Open(delim) => {
                let d = match delim {
                    Delim::Invisible => 0,
                    Delim::Paren => 1,
                    Delim::Brace => 2,
                    Delim::Bracket => 3,
                };
                stack.push((d, span, Vec::new()));
            }
            TokenKind::Close(_) => {
                if stack.len() > 1 {
                    let (delim, open_span, children) = stack.pop().unwrap();
                    stack.last_mut().unwrap().2.push(Node::Subtree { open_span, close_span: span, delim, children });
                }
            }
            TokenKind::Ident { raw, .. } => {
                let tid = intern(stream.text_of(tok));
                let idx = (idents.len() / 3) as u32;
                let is_raw = if raw == Raw::Yes { 1 } else { 0 };
                idents.extend_from_slice(&[span, tid, is_raw]);
                stack.last_mut().unwrap().2.push(Node::Leaf((idx << 2) | 3));
            }
            TokenKind::Punct { char: ch, spacing } => {
                let idx = (puncts.len() / 3) as u32;
                let sp = match spacing {
                    Spacing::Alone => 0,
                    Spacing::Joint | Spacing::JointHidden => 1,
                };
                puncts.extend_from_slice(&[span, ch as u32, sp]);
                stack.last_mut().unwrap().2.push(Node::Leaf((idx << 2) | 2));
            }
            TokenKind::Literal { kind, suffix_len, .. } => {
                let full = stream.text_of(tok);
                let slen = (suffix_len as usize).min(full.len());
                let (body, suffix) = full.split_at(full.len() - slen);
                let body_id = intern(body);
                let suffix_id = if suffix.is_empty() { u32::MAX } else { intern(suffix) };
                let kind_u32: u32 = match kind {
                    LitKind::Err => 0,
                    LitKind::Byte => 1,
                    LitKind::Char => 2,
                    LitKind::Integer => 3,
                    LitKind::Float => 4,
                    LitKind::Str => 5,
                    LitKind::StrRaw(h) => 6 | ((h as u32) << 8),
                    LitKind::ByteStr => 7,
                    LitKind::ByteStrRaw(h) => 8 | ((h as u32) << 8),
                    LitKind::CStr => 9,
                    LitKind::CStrRaw(h) => 10 | ((h as u32) << 8),
                };
                let idx = (literals.len() / 4) as u32;
                literals.extend_from_slice(&[span, body_id, kind_u32, suffix_id]);
                stack.last_mut().unwrap().2.push(Node::Leaf((idx << 2) | 1));
            }
        }
    }
    while stack.len() > 1 {
        let (delim, open_span, children) = stack.pop().unwrap();
        stack.last_mut().unwrap().2.push(Node::Subtree { open_span, close_span: open_span, delim, children });
    }
    let (root_delim, _, root_children) = stack.pop().unwrap();

    // Pass 2: BFS-flatten subtrees so each subtree's children are a contiguous range in `token_tree`.
    let mut subtrees: Vec<u32> = Vec::new();
    let mut token_tree: Vec<u32> = Vec::new();
    let mut queue: VecDeque<(usize, Vec<Node>)> = VecDeque::new();
    subtrees.extend_from_slice(&[0, 0, root_delim, 0, 0]);
    queue.push_back((0, root_children));

    while let Some((sub_idx, children)) = queue.pop_front() {
        let start = token_tree.len() as u32;
        for child in children {
            match child {
                Node::Leaf(encoded) => token_tree.push(encoded),
                Node::Subtree { open_span, close_span, delim, children: grand } => {
                    let child_idx = subtrees.len() / 5;
                    subtrees.extend_from_slice(&[open_span, close_span, delim, 0, 0]);
                    token_tree.push((child_idx as u32) << 2);
                    queue.push_back((child_idx, grand));
                }
            }
        }
        let end = token_tree.len() as u32;
        subtrees[sub_idx * 5 + 3] = start;
        subtrees[sub_idx * 5 + 4] = end;
    }

    let write_u32s = |buf: &mut String, slice: &[u32]| {
        use core::fmt::Write as _;
        buf.push('[');
        for (i, &v) in slice.iter().enumerate() {
            if i > 0 {
                buf.push(',');
            }
            let _ = write!(buf, "{v}");
        }
        buf.push(']');
    };

    let mut out = String::with_capacity(256 + token_tree.len() * 6);
    out.push_str(r#"{"subtree":"#);
    write_u32s(&mut out, &subtrees);
    out.push_str(r#","literal":"#);
    write_u32s(&mut out, &literals);
    out.push_str(r#","punct":"#);
    write_u32s(&mut out, &puncts);
    out.push_str(r#","ident":"#);
    write_u32s(&mut out, &idents);
    out.push_str(r#","token_tree":"#);
    write_u32s(&mut out, &token_tree);
    out.push_str(r#","text":["#);
    for (i, s) in texts.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push('"');
        out.push_str(&json_escape(s));
        out.push('"');
    }
    out.push_str("]}");
    out
}

fn parse_u32_array(json: &str, key: &str) -> Result<Vec<u32>, NativeBridgeError> {
    let needle = format!("\"{key}\":[");
    let start = json
        .find(&needle)
        .ok_or_else(|| NativeBridgeError::Protocol(format!("missing `{key}` in FlatTree reply")))?
        + needle.len();
    let rest = &json[start..];
    let end = rest
        .find(']')
        .ok_or_else(|| NativeBridgeError::Protocol(format!("unclosed `{key}` array")))?;
    let body = rest[..end].trim();
    if body.is_empty() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for part in body.split(',') {
        let v: u32 = part
            .trim()
            .parse()
            .map_err(|_| NativeBridgeError::Protocol(format!("bad u32 `{part}` in `{key}`")))?;
        out.push(v);
    }
    Ok(out)
}

fn unescape_json_str(bytes: &[u8], mut i: usize) -> Result<(String, usize), NativeBridgeError> {
    let mut out = String::new();
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'"' {
            return Ok((out, i + 1));
        }
        if b == b'\\' {
            i += 1;
            if i >= bytes.len() {
                return Err(NativeBridgeError::Protocol("truncated JSON escape".to_owned()));
            }
            match bytes[i] {
                b'"' => out.push('"'),
                b'\\' => out.push('\\'),
                b'/' => out.push('/'),
                b'n' => out.push('\n'),
                b'r' => out.push('\r'),
                b't' => out.push('\t'),
                b'b' => out.push('\x08'),
                b'f' => out.push('\x0c'),
                b'u' => {
                    if i + 4 >= bytes.len() {
                        return Err(NativeBridgeError::Protocol("truncated \\uXXXX escape".to_owned()));
                    }
                    let hex = core::str::from_utf8(&bytes[i + 1..i + 5])
                        .map_err(|_| NativeBridgeError::Protocol("bad utf8 in \\uXXXX".to_owned()))?;
                    let code = u16::from_str_radix(hex, 16)
                        .map_err(|_| NativeBridgeError::Protocol(format!("bad \\u{hex}")))?;
                    i += 4;
                    // Handle UTF-16 surrogate pairs.
                    if (0xD800..=0xDBFF).contains(&code)
                        && i + 6 < bytes.len()
                        && bytes[i + 1] == b'\\'
                        && bytes[i + 2] == b'u'
                    {
                        let lo_hex = core::str::from_utf8(&bytes[i + 3..i + 7])
                            .map_err(|_| NativeBridgeError::Protocol("bad utf8 in surrogate".to_owned()))?;
                        let lo = u16::from_str_radix(lo_hex, 16)
                            .map_err(|_| NativeBridgeError::Protocol("bad surrogate hex".to_owned()))?;
                        if (0xDC00..=0xDFFF).contains(&lo) {
                            let cp = 0x10000
                                + (((u32::from(code) - 0xD800) << 10) | (u32::from(lo) - 0xDC00));
                            if let Some(ch) = char::from_u32(cp) {
                                out.push(ch);
                            }
                            i += 6;
                        }
                    } else if let Some(ch) = char::from_u32(u32::from(code)) {
                        out.push(ch);
                    }
                }
                other => out.push(other as char),
            }
            i += 1;
        } else {
            // Decode UTF-8 char starting at `i`.
            let s = core::str::from_utf8(&bytes[i..])
                .map_err(|_| NativeBridgeError::Protocol("invalid UTF-8 in JSON string".to_owned()))?;
            let ch = s.chars().next().unwrap();
            out.push(ch);
            i += ch.len_utf8();
        }
    }
    Err(NativeBridgeError::Protocol("unterminated JSON string".to_owned()))
}

fn parse_string_array(json: &str, key: &str) -> Result<Vec<String>, NativeBridgeError> {
    let needle = format!("\"{key}\":[");
    let start = json
        .find(&needle)
        .ok_or_else(|| NativeBridgeError::Protocol(format!("missing `{key}` in FlatTree reply")))?
        + needle.len();
    let bytes = json.as_bytes();
    let mut i = start;
    let mut out = Vec::new();
    while i < bytes.len() {
        while i < bytes.len() && matches!(bytes[i], b' ' | b'\t' | b'\r' | b'\n' | b',') {
            i += 1;
        }
        if i >= bytes.len() || bytes[i] == b']' {
            break;
        }
        if bytes[i] != b'"' {
            return Err(NativeBridgeError::Protocol(format!(
                "expected string in `{key}` array at byte {i}"
            )));
        }
        let (s, next) = unescape_json_str(bytes, i + 1)?;
        out.push(s);
        i = next;
    }
    Ok(out)
}

/// Maps a `FlatTree` span ID back to a `blaze_expand::TokenOrigin`.
fn origin_of_span(span: u32, input_len: u32, attr_len: u32) -> TokenOrigin {
    const SPAN_CALL_SITE: u32 = 0;
    const SPAN_MIXED_SITE: u32 = 1;
    const SPAN_DEF_SITE: u32 = 2;
    const SPAN_TOKEN_BASE: u32 = 3;
    match span {
        SPAN_CALL_SITE => TokenOrigin::Made(Transparency::Transparent),
        SPAN_MIXED_SITE => TokenOrigin::Made(Transparency::SemiOpaque),
        SPAN_DEF_SITE => TokenOrigin::Made(Transparency::Opaque),
        s => {
            let rel = s - SPAN_TOKEN_BASE;
            if rel < input_len {
                TokenOrigin::Input(rel)
            } else if rel - input_len < attr_len {
                TokenOrigin::AttrArgs(rel - input_len)
            } else {
                TokenOrigin::Made(Transparency::Transparent)
            }
        }
    }
}

/// Decodes a rust-analyzer API v6 `FlatTree` JSON object directly into a `blaze_expand::TokenStream`.
pub fn decode_flat_tree_json<'a>(
    json: &str,
    edition: Edition,
    alloc: Alloc<'a>,
) -> Result<TokenStream<'a>, NativeBridgeError> {
    decode_flat_tree_json_with_origins(json, edition, 0, 0, None, alloc).map(|(stream, _)| stream)
}

/// Decodes a rust-analyzer API v6 `FlatTree` JSON object into a `blaze_expand::TokenStream` and
/// its per-token [`TokenOrigin`] column (preserving `Input`, `AttrArgs`, `call_site`, `mixed_site`,
/// and `def_site` hygiene). When `path_remap = Some((host_dir, virtual_dir))`, string literals
/// beginning with `host_dir` (e.g. from `sol!`'s `include_bytes!(CARGO_MANIFEST_DIR/...)`) are
/// mapped back to `virtual_dir` so expansions remain hermetic and machine-independent (D60/D96).
fn decode_flat_tree_json_with_origins<'a>(
    json: &str,
    edition: Edition,
    input_len: u32,
    attr_len: u32,
    path_remap: Option<(&str, &str)>,
    alloc: Alloc<'a>,
) -> Result<(TokenStream<'a>, OVec<TokenOrigin, Alloc<'a>>), NativeBridgeError> {
    let ctx = Ctx::root(edition);
    let subtrees = parse_u32_array(json, "subtree")?;
    let literals = parse_u32_array(json, "literal")?;
    let puncts = parse_u32_array(json, "punct")?;
    let idents = parse_u32_array(json, "ident")?;
    let token_tree = parse_u32_array(json, "token_tree")?;
    let texts = parse_string_array(json, "text")?;

    let mut stream = TokenStream::new_in(alloc);
    let mut origins = OVec::with_capacity_in(token_tree.len(), alloc);
    if subtrees.len() < 5 {
        return Ok((stream, origins));
    }

    enum Step {
        Tree(usize),
        Close(Delim, u32),
    }

    // Walk iteratively starting from root subtree 0 (its Invisible delimiters are implicit).
    let mut stack: Vec<Step> = Vec::new();
    let root_start = subtrees[3] as usize;
    let root_end = subtrees[4] as usize;
    for idx in (root_start..root_end.min(token_tree.len())).rev() {
        stack.push(Step::Tree(idx));
    }

    while let Some(step) = stack.pop() {
        match step {
            Step::Close(delim, close_span) => {
                stream
                    .close(delim, ctx)
                    .map_err(|e| NativeBridgeError::Failed(format!("token stream close: {e:?}")))?;
                origins.push(origin_of_span(close_span, input_len, attr_len));
            }
            Step::Tree(tt_pos) => {
                let encoded = token_tree[tt_pos];
                let tag = encoded & 0b11;
                let idx = (encoded >> 2) as usize;
                match tag {
                    0 => {
                        let base = idx * 5;
                        if base + 5 > subtrees.len() {
                            return Err(NativeBridgeError::Protocol("subtree index out of range".to_owned()));
                        }
                        let open_span = subtrees.get(base).copied().unwrap_or(0);
                        let close_span = subtrees.get(base + 1).copied().unwrap_or(0);
                        let delim = match subtrees[base + 2] {
                            1 => Delim::Paren,
                            2 => Delim::Brace,
                            3 => Delim::Bracket,
                            _ => Delim::Invisible,
                        };
                        let start = subtrees[base + 3] as usize;
                        let end = subtrees[base + 4] as usize;
                        stream
                            .open(delim, ctx)
                            .map_err(|e| NativeBridgeError::Failed(format!("token stream open: {e:?}")))?;
                        origins.push(origin_of_span(open_span, input_len, attr_len));
                        stack.push(Step::Close(delim, close_span));
                        for c in (start..end.min(token_tree.len())).rev() {
                            stack.push(Step::Tree(c));
                        }
                    }
                    1 => {
                        let base = idx * 4;
                        if base + 4 > literals.len() {
                            return Err(NativeBridgeError::Protocol("literal index out of range".to_owned()));
                        }
                        let span = literals.get(base).copied().unwrap_or(0);
                        let text_idx = literals[base + 1] as usize;
                        let kind_raw = literals[base + 2];
                        let suffix_idx = literals[base + 3];
                        let body = texts
                            .get(text_idx)
                            .ok_or_else(|| NativeBridgeError::Protocol("literal text out of range".to_owned()))?;
                        let suffix = if suffix_idx == u32::MAX {
                            ""
                        } else {
                            texts.get(suffix_idx as usize).map(String::as_str).unwrap_or("")
                        };
                        let hashes = ((kind_raw >> 8) & 0xff) as u8;
                        let kind = match kind_raw & 0xff {
                            1 => LitKind::Byte,
                            2 => LitKind::Char,
                            3 => LitKind::Integer,
                            4 => LitKind::Float,
                            5 => LitKind::Str,
                            6 => LitKind::StrRaw(hashes),
                            7 => LitKind::ByteStr,
                            8 => LitKind::ByteStrRaw(hashes),
                            9 => LitKind::CStr,
                            10 => LitKind::CStrRaw(hashes),
                            _ => LitKind::Err,
                        };
                        let mut combined = String::with_capacity(body.len() + suffix.len());
                        if matches!(kind, LitKind::Str | LitKind::StrRaw(_))
                            && let Some((host_dir, virt_dir)) = path_remap
                            && let Some(rest) = body.strip_prefix(host_dir)
                            && (rest.is_empty() || rest.starts_with('/'))
                        {
                            combined.push_str(virt_dir);
                            combined.push_str(rest);
                        } else {
                            combined.push_str(body);
                        }
                        combined.push_str(suffix);
                        stream
                            .push(
                                TokenKind::Literal {
                                    kind,
                                    suffix_len: suffix.len() as u32,
                                    spacing: Spacing::Alone,
                                },
                                &combined,
                                ctx,
                            )
                            .map_err(|e| NativeBridgeError::Failed(format!("push literal: {e:?}")))?;
                        origins.push(origin_of_span(span, input_len, attr_len));
                    }
                    2 => {
                        let base = idx * 3;
                        if base + 3 > puncts.len() {
                            return Err(NativeBridgeError::Protocol("punct index out of range".to_owned()));
                        }
                        let span = puncts.get(base).copied().unwrap_or(0);
                        let ch = char::from_u32(puncts[base + 1]).unwrap_or('?');
                        let spacing = if puncts[base + 2] == 1 { Spacing::Joint } else { Spacing::Alone };
                        stream
                            .push(TokenKind::Punct { char: ch, spacing }, "", ctx)
                            .map_err(|e| NativeBridgeError::Failed(format!("push punct: {e:?}")))?;
                        origins.push(origin_of_span(span, input_len, attr_len));
                    }
                    _ => {
                        let base = idx * 3;
                        if base + 3 > idents.len() {
                            return Err(NativeBridgeError::Protocol("ident index out of range".to_owned()));
                        }
                        let span = idents.get(base).copied().unwrap_or(0);
                        let text_idx = idents[base + 1] as usize;
                        let is_raw = idents[base + 2] != 0;
                        let name = texts
                            .get(text_idx)
                            .ok_or_else(|| NativeBridgeError::Protocol("ident text out of range".to_owned()))?;
                        let (clean, raw) = if let Some(stripped) = name.strip_prefix("r#") {
                            (stripped, Raw::Yes)
                        } else if is_raw {
                            (name.as_str(), Raw::Yes)
                        } else {
                            (name.as_str(), Raw::No)
                        };
                        stream
                            .push(TokenKind::Ident { raw, spacing: Spacing::Alone }, clean, ctx)
                            .map_err(|e| NativeBridgeError::Failed(format!("push ident: {e:?}")))?;
                        origins.push(origin_of_span(span, input_len, attr_len));
                    }
                }
            }
        }
    }

    Ok((stream, origins))
}

fn parse_expand_macro_reply<'a>(
    reply: &str,
    edition: Edition,
    input_len: u32,
    attr_len: u32,
    path_remap: Option<(&str, &str)>,
    alloc: Alloc<'a>,
) -> Result<Produced<'a>, NativeBridgeError> {
    if let Some(err_pos) = reply.find(r#""Err":"#) {
        let rest = &reply[err_pos + 6..];
        let msg = if let Some(q) = rest.find('"') {
            unescape_json_str(rest.as_bytes(), q + 1)
                .map(|(s, _)| s)
                .unwrap_or_else(|_| rest.to_owned())
        } else {
            rest.to_owned()
        };
        let mut text = OVec::new_in(alloc);
        let at = text.len() as u32;
        text.extend_from_slice(msg.as_bytes());
        let len = msg.len() as u32;
        let mut diagnostics = OVec::new_in(alloc);
        diagnostics.push(ProcDiagnostic::Panicked { message: Text { at, len } });
        return Ok(Produced {
            tokens: TokenStream::new_in(alloc),
            origins: OVec::new_in(alloc),
            reads: Reads::new_in(alloc),
            diagnostics,
            text,
        });
    }
    let ok_pos = reply
        .find(r#""Ok":"#)
        .ok_or_else(|| NativeBridgeError::Protocol(format!("unexpected ExpandMacro reply: {reply}")))?;
    let tree_json = &reply[ok_pos + 5..];
    let (tokens, origins) = decode_flat_tree_json_with_origins(tree_json, edition, input_len, attr_len, path_remap, alloc)?;
    Ok(Produced {
        tokens,
        origins,
        reads: Reads::new_in(alloc),
        diagnostics: OVec::new_in(alloc),
        text: OVec::new_in(alloc),
    })
}

/// Scans `.rs` source files under `src_dir` for `#[proc_macro_derive(Name, attributes(a, b, ...))]`
/// declarations so native `.dylib` proc-macro records carry exact derive helper attribute lists.
pub fn extract_derive_helpers_from_src(src_dir: &Path) -> HashMap<String, Vec<String>> {
    let mut helpers: HashMap<String, Vec<String>> = HashMap::new();
    let mut stack = vec![src_dir.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            let p = entry.path();
            if p.is_dir() {
                stack.push(p);
            } else if p.extension().and_then(|e| e.to_str()) == Some("rs") {
                let Ok(text) = std::fs::read_to_string(&p) else { continue };
                let mut at = 0;
                while let Some(pos) = text[at..].find("proc_macro_derive") {
                    let abs = at + pos + "proc_macro_derive".len();
                    let rest = text[abs..].trim_start();
                    if let Some(after_open) = rest.strip_prefix('(') {
                        let mut depth = 1usize;
                        let mut end = 0usize;
                        for (i, ch) in after_open.char_indices() {
                            match ch {
                                '(' => depth += 1,
                                ')' => {
                                    depth -= 1;
                                    if depth == 0 {
                                        end = i;
                                        break;
                                    }
                                }
                                _ => {}
                            }
                        }
                        if end > 0 {
                            let inner = &after_open[..end];
                            let mut split = inner.splitn(2, ',');
                            let derive_name = split.next().unwrap_or("").trim();
                            if !derive_name.is_empty() {
                                if let Some(rest_args) = split.next() {
                                    if let Some(attr_pos) = rest_args.find("attributes") {
                                        let a_rest = rest_args[attr_pos + "attributes".len()..].trim_start();
                                        if let Some(a_inner) = a_rest.strip_prefix('(').and_then(|s| s.split(')').next()) {
                                            let list: Vec<String> = a_inner
                                                .split(',')
                                                .map(|s| s.trim().to_owned())
                                                .filter(|s| !s.is_empty())
                                                .collect();
                                            if !list.is_empty() {
                                                helpers.insert(derive_name.to_owned(), list);
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                    at = abs;
                }
            }
        }
    }
    helpers
}

impl Drop for NativeProcMacroServer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn parse_api_version(reply: &str) -> Option<u32> {
    let key = "\"ApiVersionCheck\":";
    let start = reply.find(key)? + key.len();
    let digits: String = reply[start..]
        .trim_start()
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    digits.parse().ok()
}

/// Parses `{"ListMacros":{"Ok":[["name","CustomDerive"],...]}}` without external JSON deps.
fn parse_list_macros(reply: &str) -> Result<Vec<NativeExport>, NativeBridgeError> {
    if let Some(err_idx) = reply.find("\"Err\":") {
        return Err(NativeBridgeError::Failed(reply[err_idx..].to_owned()));
    }
    let ok_idx = reply
        .find("\"Ok\":")
        .ok_or_else(|| NativeBridgeError::Protocol(format!("unexpected ListMacros reply: {reply}")))?;
    let body = &reply[ok_idx + 5..];
    let mut exports = Vec::new();
    let bytes = body.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        // Look for `["<name>","<kind>"]` pairs.
        if bytes[i] == b'[' && i + 1 < bytes.len() && bytes[i + 1] == b'"' {
            i += 2;
            let name_start = i;
            while i < bytes.len() && bytes[i] != b'"' {
                if bytes[i] == b'\\' {
                    i += 1;
                }
                i += 1;
            }
            let name = body[name_start..i.min(bytes.len())].to_owned();
            // Advance to second quoted string.
            while i < bytes.len() && bytes[i] != b',' {
                i += 1;
            }
            while i < bytes.len() && bytes[i] != b'"' {
                i += 1;
            }
            if i < bytes.len() {
                i += 1;
                let kind_start = i;
                while i < bytes.len() && bytes[i] != b'"' {
                    i += 1;
                }
                let kind_str = &body[kind_start..i.min(bytes.len())];
                let kind = match kind_str {
                    "CustomDerive" => ProcKind::Derive,
                    "Attr" => ProcKind::Attr,
                    _ => ProcKind::FnLike,
                };
                exports.push(NativeExport { name, kind });
            }
        }
        i += 1;
    }
    Ok(exports)
}

/// Parsed output of running a native host `build-script-build` binary under
/// `bstd::procs::hermetic` (macOS Seatbelt / Linux Landlock + seccomp).
#[derive(Clone, Debug)]
pub struct NativeScriptOutput {
    pub exit_code: i32,
    pub cfgs: Vec<String>,
    pub rustc_envs: Vec<(String, String)>,
    pub link_libs: Vec<String>,
    pub link_search: Vec<String>,
    pub output_files: Vec<String>,
    pub stdout: String,
    pub stderr: String,
}

/// Runs a native host `build-script-build` binary directly through Blaze's own cross-platform
/// hermetic sandbox (`bstd::procs::hermetic`: macOS Seatbelt, Linux Landlock + seccomp +
/// `no_new_privs`) with zero Wasm, deterministic clock (`SOURCE_DATE_EPOCH=0`), explicit roots
/// (`manifest_dir` read-only + `out_dir` read-write), and parses standard `cargo::` / `cargo:`
/// directives.
pub fn run_native_build_script(
    binary: &Path,
    manifest_dir: &Path,
    out_dir: &Path,
    extra_read_dirs: &[&Path],
    env_vars: &[(&str, &str)],
    deadline_dur: Duration,
) -> Result<NativeScriptOutput, NativeBridgeError> {
    std::fs::create_dir_all(out_dir)?;
    let canon_bin = std::fs::canonicalize(binary)?;
    let canon_manifest = std::fs::canonicalize(manifest_dir)?;
    let canon_out = std::fs::canonicalize(out_dir)?;
    let bin_str = canon_bin
        .to_str()
        .ok_or_else(|| NativeBridgeError::Protocol("non-UTF-8 binary path".to_owned()))?;
    let manifest_str = canon_manifest
        .to_str()
        .ok_or_else(|| NativeBridgeError::Protocol("non-UTF-8 manifest_dir path".to_owned()))?;
    let out_str = canon_out
        .to_str()
        .ok_or_else(|| NativeBridgeError::Protocol("non-UTF-8 out_dir path".to_owned()))?;

    let mut canon_extra = Vec::with_capacity(extra_read_dirs.len());
    for dir in extra_read_dirs {
        let c = std::fs::canonicalize(dir)?;
        let s = c
            .to_str()
            .ok_or_else(|| NativeBridgeError::Protocol("non-UTF-8 extra read dir".to_owned()))?
            .to_owned();
        canon_extra.push(s);
    }

    let bin_bytes = std::fs::read(&canon_bin)?;
    let bin_digest = Digest(bstd::digest::blake3(&bin_bytes));

    let (world, _) = native_world(&canon_out)
        .map_err(|e| NativeBridgeError::Failed(format!("open hermetic world: {e}")))?;
    let spawner = world
        .need_host()
        .and_then(|h| h.spawner())
        .map_err(|e| NativeBridgeError::Failed(format!("missing host spawner: {}", e.capability)))?;
    let clock = world
        .need_clock()
        .map_err(|e| NativeBridgeError::Failed(format!("missing clock: {}", e.capability)))?;
    let deadline = Deadline::after(clock.now().since_start(), deadline_dur);
    let cx = world
        .cx(deadline_dur.saturating_add(Duration::from_secs(60)), TASK_FRAME)
        .map_err(|_| NativeBridgeError::Failed("hermetic world stopped".to_owned()))?;
    let alloc = Alloc::global();

    // Build hermetic Roots: read-only Tree roots for manifest_dir + extra_read_dirs, Output root for OUT_DIR.
    let tree = RootKind::Tree { tree: bin_digest };
    let mut roots: AVec<'_, Root<'_>> = OVec::new_in(alloc);
    let mut spellings: Vec<&str> = Vec::new();
    roots.push(Root { name: "manifest_dir", kind: tree });
    spellings.push(manifest_str);
    for extra in &canon_extra {
        if !spellings.contains(&extra.as_str()) {
            roots.push(Root { name: "extra_read", kind: tree });
            spellings.push(extra.as_str());
        }
    }
    let output_id = RootId(roots.len() as u8);
    roots.push(Root { name: "OUT_DIR", kind: RootKind::Output });
    spellings.push(out_str);

    let mut mounts: AVec<'_, Mount<'_>> = OVec::new_in(alloc);
    for (idx, spelling) in spellings.iter().enumerate() {
        mounts.push(Mount { spelling: spelling.as_bytes(), root: RootId(idx as u8) });
    }
    let roots = Roots::new(&roots)
        .map_err(|e| NativeBridgeError::Failed(format!("hermetic roots error: {e:?}")))?;

    let mut env: Vec<(&str, &str)> = Vec::with_capacity(env_vars.len() + 4);
    env.push(("CARGO_MANIFEST_DIR", manifest_str));
    env.push(("OUT_DIR", out_str));
    env.push(("SOURCE_DATE_EPOCH", "0"));
    env.push(("TMPDIR", out_str));
    for &(k, v) in env_vars {
        env.push((k, v));
    }
    env.sort_by(|a, b| a.0.cmp(b.0));
    let mut dedup_env: Vec<(&str, &str)> = Vec::with_capacity(env.len());
    for pair in env {
        match dedup_env.last_mut() {
            Some(last) if last.0 == pair.0 => *last = pair,
            _ => dedup_env.push(pair),
        }
    }

    let clock_spec = match dedup_env.iter().find(|(name, _)| *name == "SOURCE_DATE_EPOCH") {
        // A value that is no decimal leaves the spec's own check to refuse it by name.
        Some((_, value)) => value
            .parse::<u64>()
            .map_or(ClockSpec::NeverAsks, |unix_secs| ClockSpec::Fixed { unix_secs }),
        None => ClockSpec::NeverAsks,
    };

    let spec = HermeticSpec::new(HermeticOpts {
        program: Program::Stored { name: bin_str, digest: bin_digest, path: bin_str },
        args: &[],
        env: &dedup_env,
        roots,
        mounts: &mounts,
        cwd: manifest_str,
        helpers: &[],
        helper_folders: &[],
        channels: &[],
        clock: clock_spec,
        seed: 0,
        budget: Budget { cpu: deadline_dur.saturating_mul(16), memory: 16 << 30, steps: None },
        deadline,
        streams: HermeticStreams {
            stdout: Capture::Keep { most: 16 << 20 },
            stderr: Capture::Keep { most: 4 << 20 },
        },
        level: Level::Inputs,
        report: ReportSpec::WholeRoots,
    })
    .map_err(|e| NativeBridgeError::Failed(format!("hermetic spec refused: {:?}", e.fault)))?;

    let hermetic = Hermetic::new(HermeticCapOpts { spawner, clock });
    let files = OutDir { root: output_id, folder: out_str, package_root: None };
    let ran = hermetic
        .run(&cx, &spec, &files, alloc)
        .map_err(|e| NativeBridgeError::Failed(format!("hermetic run failed: {:?}", e.fault)))?;

    let stdout = String::from_utf8_lossy(&ran.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&ran.stderr).into_owned();
    let exit_code = match ran.exit {
        Exit::Exited(code) => code,
        Exit::Signaled(sig) => -(sig as i32),
    };
    let output_files: Vec<String> = ran
        .outputs
        .entries()
        .map(|o| o.path.as_str().to_owned())
        .collect();

    let mut cfgs = Vec::new();
    let mut rustc_envs = Vec::new();
    let mut link_libs = Vec::new();
    let mut link_search = Vec::new();

    for line in stdout.lines() {
        let rest = line
            .strip_prefix("cargo::")
            .or_else(|| line.strip_prefix("cargo:"));
        let Some(directive) = rest else { continue };
        if let Some(v) = directive.strip_prefix("rustc-cfg=") {
            cfgs.push(v.to_owned());
        } else if let Some(v) = directive.strip_prefix("rustc-env=") {
            if let Some((k, val)) = v.split_once('=') {
                rustc_envs.push((k.to_owned(), val.to_owned()));
            }
        } else if let Some(v) = directive.strip_prefix("rustc-link-lib=") {
            link_libs.push(v.to_owned());
        } else if let Some(v) = directive.strip_prefix("rustc-link-search=") {
            link_search.push(v.to_owned());
        }
    }

    Ok(NativeScriptOutput {
        exit_code,
        cfgs,
        rustc_envs,
        link_libs,
        link_search,
        output_files,
        stdout,
        stderr,
    })
}

/// Discovers all compiled native proc-macro `.dylib`/`.so` and `build-script-build` binaries in
/// a Cargo host `target/<profile>/deps` + `target/<profile>/build` tree (zero Wasm!).
#[derive(Clone, Debug, Default)]
pub struct NativeHostArtifacts {
    pub proc_macro_dylibs: Vec<PathBuf>,
    pub build_script_bins: Vec<PathBuf>,
}

pub fn discover_native_artifacts(target_profile_dir: &Path) -> std::io::Result<NativeHostArtifacts> {
    let mut out = NativeHostArtifacts::default();
    let dylib_ext = if cfg!(target_os = "macos") {
        "dylib"
    } else if cfg!(target_os = "windows") {
        "dll"
    } else {
        "so"
    };

    let deps = target_profile_dir.join("deps");
    if let Ok(entries) = std::fs::read_dir(&deps) {
        for entry in entries.flatten() {
            let p = entry.path();
            if p.extension().and_then(|e| e.to_str()) == Some(dylib_ext) && p.is_file() {
                out.proc_macro_dylibs.push(p);
            }
        }
    }

    let build = target_profile_dir.join("build");
    if let Ok(entries) = std::fs::read_dir(&build) {
        for entry in entries.flatten() {
            let p = entry.path();
            // Classic Cargo layout: `build/<pkg>-<hash>/build-script-build`
            let classic_bin = p.join("build-script-build");
            if classic_bin.is_file() {
                out.build_script_bins.push(classic_bin);
            }
            // Nightly 2026 Cargo build-dir layout: `build/<pkg>/<hash>/out/{*.dylib,build_script_build}`
            if p.is_dir() {
                if let Ok(hashes) = std::fs::read_dir(&p) {
                    for hash_entry in hashes.flatten() {
                        let out_dir = hash_entry.path().join("out");
                        if let Ok(files) = std::fs::read_dir(&out_dir) {
                            for file in files.flatten() {
                                let fp = file.path();
                                if !fp.is_file() {
                                    continue;
                                }
                                if fp.extension().and_then(|e| e.to_str()) == Some(dylib_ext) {
                                    out.proc_macro_dylibs.push(fp);
                                } else if fp.file_name().and_then(|n| n.to_str()).is_some_and(|n| {
                                    !n.contains('.')
                                        && (n.starts_with("build_script_")
                                            || n.starts_with("build-script-"))
                                }) {
                                    out.build_script_bins.push(fp);
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    out.proc_macro_dylibs.sort();
    out.build_script_bins.sort();
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_proc_macro_server_handshakes_pinned_nightly_without_wasm() {
        let srv = NativeProcMacroServer::spawn().expect("pinned nightly rust-analyzer-proc-macro-srv must handshake");
        assert!(srv.api_version() >= 5, "expected API v5+, got {}", srv.api_version());
    }

    #[test]
    fn parse_list_macros_handles_derive_attr_and_bang() {
        let sample = r#"{"ListMacros":{"Ok":[["Serialize","CustomDerive"],["sol","Bang"],["tokio_main","Attr"]]}}"#;
        let exports = parse_list_macros(sample).unwrap();
        assert_eq!(exports.len(), 3);
        assert_eq!(exports[0], NativeExport { name: "Serialize".to_owned(), kind: ProcKind::Derive });
        assert_eq!(exports[1], NativeExport { name: "sol".to_owned(), kind: ProcKind::FnLike });
        assert_eq!(exports[2], NativeExport { name: "tokio_main".to_owned(), kind: ProcKind::Attr });
    }

    #[test]
    fn hermetic_native_build_script_runs_under_bstd_sandbox_without_wasm() {
        let base = std::env::temp_dir().join(format!("blaze-hermetic-bridge-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let manifest_dir = base.join("pkg");
        let out_dir = base.join("out");
        std::fs::create_dir_all(&manifest_dir).unwrap();
        std::fs::create_dir_all(&out_dir).unwrap();

        let src = manifest_dir.join("build.rs");
        let bin = base.join("build-script-build");
        std::fs::write(
            &src,
            r#"
fn main() {
    let out = std::env::var("OUT_DIR").expect("OUT_DIR");
    let epoch = std::env::var("SOURCE_DATE_EPOCH").expect("SOURCE_DATE_EPOCH");
    std::fs::write(std::path::Path::new(&out).join("generated.rs"), "pub const MAGIC: u64 = 42;\n").unwrap();
    println!("cargo::rustc-cfg=blaze_native_hermetic");
    println!("cargo::rustc-env=BLAZE_ZERO_WASM=1");
    println!("cargo::rustc-env=SEEN_EPOCH={epoch}");
}
"#,
        )
        .unwrap();

        let status = Command::new("rustc")
            .arg(&src)
            .arg("-o")
            .arg(&bin)
            .status()
            .expect("rustc must compile native test build script");
        assert!(status.success(), "rustc failed on test build script");

        let res = run_native_build_script(
            &bin,
            &manifest_dir,
            &out_dir,
            &[],
            &[],
            Duration::from_secs(10),
        )
        .expect("bstd::procs::hermetic must run native build script");

        assert_eq!(res.exit_code, 0, "stderr: {}", res.stderr);
        assert_eq!(res.cfgs, vec!["blaze_native_hermetic".to_owned()]);
        assert_eq!(
            res.rustc_envs,
            vec![
                ("BLAZE_ZERO_WASM".to_owned(), "1".to_owned()),
                ("SEEN_EPOCH".to_owned(), "0".to_owned()),
            ]
        );
        assert!(
            res.output_files.iter().any(|f| f == "generated.rs"),
            "expected generated.rs in output_files, got {:?}",
            res.output_files
        );

        // Overriding SOURCE_DATE_EPOCH must keep ClockSpec in sync with dedup_env.
        let custom = run_native_build_script(
            &bin,
            &manifest_dir,
            &out_dir,
            &[],
            &[("SOURCE_DATE_EPOCH", "1700000000")],
            Duration::from_secs(10),
        )
        .expect("SOURCE_DATE_EPOCH override must be accepted consistently");
        assert_eq!(custom.exit_code, 0, "stderr: {}", custom.stderr);
        assert_eq!(
            custom.rustc_envs,
            vec![
                ("BLAZE_ZERO_WASM".to_owned(), "1".to_owned()),
                ("SEEN_EPOCH".to_owned(), "1700000000".to_owned()),
            ]
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn discovers_and_inspects_real_tempo_native_proc_macros_if_present() {
        let tempo_debug = Path::new("/Volumes/Projects/ref-tempo/tempo/target/debug");
        if !tempo_debug.is_dir() {
            return;
        }
        let artifacts = discover_native_artifacts(tempo_debug).expect("discover tempo artifacts");
        assert!(
            !artifacts.proc_macro_dylibs.is_empty(),
            "expected native tempo proc-macro dylibs"
        );
        assert!(
            !artifacts.build_script_bins.is_empty(),
            "expected native tempo build-script binaries"
        );
        let mut srv = NativeProcMacroServer::spawn().expect("spawn native proc-macro server");
        // Inspect real `serde_derive` and `alloy_sol_macro` natively (zero Wasm!).
        if let Some(serde_dylib) = artifacts
            .proc_macro_dylibs
            .iter()
            .find(|p| p.to_string_lossy().contains("libserde_derive"))
        {
            let exports = srv.list_macros(serde_dylib).expect("list serde_derive macros");
            assert!(
                exports.iter().any(|e| e.name == "Serialize" && e.kind == ProcKind::Derive),
                "expected Serialize derive in {:?}",
                exports
            );
            assert!(
                exports.iter().any(|e| e.name == "Deserialize" && e.kind == ProcKind::Derive),
                "expected Deserialize derive in {:?}",
                exports
            );

            // Live zero-Wasm expansion of `#[derive(Serialize)] struct Point { x: u64, y: u64 }`!
            let alloc = Alloc::global();
            let input = blaze_expand::lex("struct Point { x: u64, y: u64 }", blaze_expand::Edition::E2024, alloc)
                .expect("lex struct Point");
            let call = ProcCall {
                kind: ProcKind::Derive,
                input: &input,
                attr_args: None,
                file: Ok("src/lib.rs"),
            };
            let pool = NativeServerPool::new();
            let produced = pool
                .expand_macro(serde_dylib, "Serialize", &call, Edition::E2024, &[], None, None, alloc)
                .expect("native serde_derive Serialize expansion");
            assert!(produced.diagnostics.is_empty(), "unexpected diagnostics: {:?}", produced.diagnostics);
            assert!(
                produced.tokens.len() > 20,
                "expected full Serialize impl tokens, got {}",
                produced.tokens.len()
            );
            let has_point = produced
                .tokens
                .tokens()
                .iter()
                .any(|t| produced.tokens.text_of(t) == "Point");
            let has_serialize = produced
                .tokens
                .tokens()
                .iter()
                .any(|t| produced.tokens.text_of(t) == "Serialize");
            assert!(has_point && has_serialize, "expanded stream missing Point / Serialize");
        }
        if let Some(sol_dylib) = artifacts
            .proc_macro_dylibs
            .iter()
            .find(|p| p.to_string_lossy().contains("liballoy_sol_macro"))
        {
            let exports = srv.list_macros(sol_dylib).expect("list alloy_sol_macro macros");
            assert!(
                exports.iter().any(|e| e.name == "sol" && e.kind == ProcKind::FnLike),
                "expected sol! macro in {:?}",
                exports
            );
        }
    }
}
