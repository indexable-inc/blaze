//! Expands the real derive calls of a checkout (tempo's: serde's `Serialize` and
//! `Deserialize`, thiserror's `Error`, clap's `Parser`, `Args`, `Subcommand` and `ValueEnum`)
//! through blaze's proc-macro host, and checks what they gave:
//!
//! - each output outlines as items (blaze-file-items, no syntax error);
//! - the impl each derive documents is there by name (`impl .. Serialize .. for Name`);
//! - a second pass finds every expansion in the records, and says how long that took;
//! - a pass over a copy of the checkout at another path writes records equal to the byte.
//!
//! `tempo_derives <checkout> <checkout copy> <module store> <out>`: the checkouts are read,
//! never built or changed; the records go under `<out>/a` and `<out>/b`.

#![feature(allocator_api)]

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

use blaze_expand::{Delim, Edition, ProcCall, ProcDiagnostic, ProcKind, Produced, TokenKind, TokenStream, lex, write_tokens};
use blaze_own::Alloc;
use blaze_record::{ListRef, Slot, list_key};
use blaze_proc_macro::{
    host::{Limits, Loaded, Runtime},
    module::{Exported, ModuleRecord},
    record::{base_key, decode, find, keep, record_bytes},
    sandbox::Package,
};

mod common;

use common::{expansion_records, modules, package_of, sources};

/// One derive or function-like call found in a file.
struct Call {
    kind: ProcKind,
    file: String,
    /// The item with the `#[derive]` or the macro's attribute taken out, or a fn-like call's
    /// tokens, as rustc gives them.
    input: TokenStream<'static>,
    /// An attribute macro's arguments (empty for `#[name]`).
    attr: Option<TokenStream<'static>>,
    /// The path before the name (`tokio` of `tokio::test`), joined; the derives' is empty.
    path: String,
    /// The macro names the attribute lists, last path segment.
    names: Vec<String>,
    item: String,
    package: Arc<Package>,
}

fn is_punct(stream: &TokenStream<'_>, at: usize, c: char) -> bool {
    matches!(stream.tokens().get(at).map(|it| it.kind), Some(TokenKind::Punct { char, .. }) if char == c)
}

fn is_open(stream: &TokenStream<'_>, at: usize, d: Delim) -> bool {
    matches!(stream.tokens().get(at).map(|it| it.kind), Some(TokenKind::Open(delim)) if delim == d)
}

fn ident_at<'s>(stream: &'s TokenStream<'_>, at: usize) -> Option<&'s str> {
    match stream.tokens().get(at)?.kind {
        TokenKind::Ident { .. } => Some(stream.text_at(at)),
        _ => None,
    }
}

/// The `Open` matching the `Close` at `close`.
fn open_of(stream: &TokenStream<'_>, close: usize) -> Option<usize> {
    let mut depth = 0usize;
    for at in (0..=close).rev() {
        match stream.tokens()[at].kind {
            TokenKind::Close(_) => depth += 1,
            TokenKind::Open(_) => {
                depth -= 1;
                if depth == 0 {
                    return Some(at);
                }
            }
            _ => {}
        }
    }
    None
}

fn calls_in(file: &str, stream: &TokenStream<'_>, package: &Arc<Package>) -> Vec<Call> {
    let mut out = Vec::new();
    let tokens = stream.tokens();
    // The end of the item an attribute macro takes: what is inside is its to expand.
    let mut inside_until = 0;
    for hash in 0..tokens.len() {
        if hash < inside_until {
            continue;
        }
        // `sol! { .. }`, `paste::paste! { .. }`: the group's tokens are the input; not one in a
        // `macro_rules!` body (a `$` in it), which is a call only once that macro expands.
        if let Some(name) = ident_at(stream, hash)
            && FN_LIKE_MACROS.contains(&name)
            && is_punct(stream, hash + 1, '!')
            && matches!(tokens.get(hash + 2).map(|it| it.kind), Some(TokenKind::Open(_)))
            && let Some(close) = stream.close_of(hash + 2)
            && !(hash + 2..close).any(|j| is_punct(stream, j, '$'))
        {
            let mut input = TokenStream::new_in(Alloc::global());
            if (hash + 3..close).all(|j| input.push_from(stream, j).is_ok()) {
                let kind = ProcKind::FnLike;
                let path = if hash >= 3 && is_punct(stream, hash - 1, ':') { ident_at(stream, hash - 3).unwrap_or("").to_owned() } else { String::new() };
                out.push(Call { kind, file: file.to_owned(), input, attr: None, path, names: vec![name.to_owned()], item: String::new(), package: package.clone() });
            }
            continue;
        }
        if !(is_punct(stream, hash, '#') && is_open(stream, hash + 1, Delim::Bracket)) {
            continue;
        }
        let Some(attr_end) = stream.close_of(hash + 1) else { continue };
        // The attribute's path, `a::b::name`, and what follows it.
        let mut path = Vec::new();
        let mut at = hash + 2;
        while let Some(segment) = ident_at(stream, at) {
            path.push(segment.to_owned());
            if is_punct(stream, at + 1, ':') && is_punct(stream, at + 2, ':') {
                at += 3;
            } else {
                at += 1;
                break;
            }
        }
        let Some(name) = path.last().cloned() else { continue };
        if name != "derive" {
            if !ATTRIBUTE_MACROS.contains(&name.as_str()) {
                continue;
            }
            // `#[name]` or `#[name(args)]`: the group's tokens are the macro's arguments.
            let mut args = TokenStream::new_in(Alloc::global());
            if is_open(stream, at, Delim::Paren) && let Some(close) = stream.close_of(at) {
                if close + 1 != attr_end || !(at + 1..close).all(|j| args.push_from(stream, j).is_ok()) {
                    continue;
                }
            } else if at != attr_end {
                continue;
            }
            let start = attrs_start(stream, hash);
            let Some((end, item)) = item_after(stream, attr_end + 1) else { continue };
            if (start..end).any(|j| is_punct(stream, j, '$')) {
                continue;
            }
            let mut input = TokenStream::new_in(Alloc::global());
            if !(start..end).filter(|j| !(hash..=attr_end).contains(j)).all(|j| input.push_from(stream, j).is_ok()) {
                continue;
            }
            // What the item holds is the macro's to expand first: a call in it is one only in
            // its output.
            inside_until = end;
            out.push(Call {
                kind: ProcKind::Attr,
                file: file.to_owned(),
                input,
                attr: Some(args),
                path: path.join("::"),
                names: vec![name],
                item,
                package: package.clone(),
            });
            continue;
        }
        if !is_open(stream, at, Delim::Paren) {
            continue;
        }
        let Some(args_end) = stream.close_of(at) else { continue };
        let mut names = Vec::new();
        let mut last = None;
        for at in at + 1..args_end {
            if let Some(name) = ident_at(stream, at) {
                last = Some(name.to_owned());
            }
            if is_punct(stream, at, ',') {
                names.extend(last.take());
            }
        }
        names.extend(last.take());
        let start = attrs_start(stream, hash);
        let Some((end, item)) = item_after(stream, attr_end + 1) else { continue };
        // A derive in a `macro_rules!` body is a template, a call only once that macro is
        // expanded.
        if (attr_end..end).any(|j| is_punct(stream, j, '$')) {
            continue;
        }
        let mut input = TokenStream::new_in(Alloc::global());
        for j in start..end {
            if (hash..=attr_end).contains(&j) {
                continue;
            }
            if input.push_from(stream, j).is_err() {
                break;
            }
        }
        out.push(Call { kind: ProcKind::Derive, file: file.to_owned(), input, attr: None, path: String::new(), names, item, package: package.clone() });
    }
    out
}

/// The first token of the attributes stacked before the one at `hash`.
fn attrs_start(stream: &TokenStream<'_>, hash: usize) -> usize {
    let tokens = stream.tokens();
    let mut start = hash;
    while start >= 2 && matches!(tokens[start - 1].kind, TokenKind::Close(Delim::Bracket)) {
        match open_of(stream, start - 1) {
            Some(open) if open >= 1 && is_punct(stream, open - 1, '#') => start = open - 1,
            _ => break,
        }
    }
    start
}

/// The item from `from`: the end past its body's close or its `;`, and its name after
/// `struct`, `enum`, `union`, `fn`, `trait` or `mod` (empty for an `impl`).
fn item_after(stream: &TokenStream<'_>, from: usize) -> Option<(usize, String)> {
    let tokens = stream.tokens();
    let mut at = from;
    let mut item = String::new();
    while at < tokens.len() {
        match tokens[at].kind {
            TokenKind::Open(Delim::Brace) => return Some((stream.close_of(at)? + 1, item)),
            TokenKind::Open(_) => at = stream.close_of(at).map_or(tokens.len(), |it| it + 1),
            TokenKind::Punct { char: ';', .. } => return Some((at + 1, item)),
            TokenKind::Ident { .. } => {
                if item.is_empty() && at > 0 && ident_at(stream, at - 1).is_some_and(|it| matches!(it, "struct" | "enum" | "union" | "fn" | "trait" | "mod")) {
                    item = stream.text_at(at).to_owned();
                }
                at += 1;
            }
            _ => at += 1,
        }
    }
    None
}

/// The tokens as source text, for the outline.
fn print(stream: &TokenStream<'_>) -> String {
    let mut out = blaze_own::AString::new_in(Alloc::global());
    write_tokens(stream, &mut out);
    out.as_str().to_owned()
}

/// Whether the output holds `impl .. <trait_name> .. for <item>`.
fn has_impl(stream: &TokenStream<'_>, trait_name: &str, item: &str) -> bool {
    let tokens = stream.tokens();
    for at in 0..tokens.len() {
        if ident_at(stream, at) != Some("impl") {
            continue;
        }
        let mut saw_trait = false;
        let mut j = at + 1;
        while j < tokens.len() && !matches!(tokens[j].kind, TokenKind::Open(Delim::Brace)) {
            match ident_at(stream, j) {
                Some(name) if name == trait_name => saw_trait = true,
                Some("for") if saw_trait && ident_at(stream, j + 1) == Some(item) => return true,
                _ => {}
            }
            j += 1;
        }
    }
    false
}

#[derive(Default)]
struct Stats {
    calls: u32,
    expanded: u32,
    panicked: u32,
    failed: BTreeMap<String, u32>,
    outlined: u32,
    impls: u32,
    from_record: u32,
    time: Duration,
}

/// The module and macro a call's name means in its file: serde's, thiserror's or clap's, by
/// the crate the file names.
/// The attribute macros the census finds, by their last path segment.
const ATTRIBUTE_MACROS: &[&str] = &["contract", "test", "main", "test_case", "test_matrix", "test_traced", "instrument", "async_trait", "auto_impl", "pin_project", "rpc"];

/// The fn-like proc macros the census finds.
const FN_LIKE_MACROS: &[&str] = &[
    "sol",
    "paste",
    "storable_rust_ints",
    "storable_alloy_ints",
    "storable_alloy_bytes",
    "gen_storable_tests",
    "storable_arrays",
    "storable_nested_arrays",
];

/// The module and macro a call resolves to: by the crate its path or the file's `use`s name,
/// standing in for name resolution, which is blaze-expand's. Of two versions of one crate
/// (thiserror 1 and 2), the newest.
fn macro_for<'m>(modules: &'m [ModuleRecord], text: &str, call: &Call, name: &str) -> Option<(&'m ModuleRecord, &'m Exported)> {
    let kind = call.kind;
    let path = call.path.as_str();
    let krate = match (kind, name) {
        (ProcKind::Derive, "Serialize" | "Deserialize") if text.contains("serde") => "serde_derive",
        (ProcKind::Derive, "Error") if text.contains("thiserror") => "thiserror-impl",
        (ProcKind::Derive, "Parser" | "Args" | "Subcommand" | "ValueEnum") if text.contains("clap") => "clap_derive",
        (ProcKind::Derive, "Storable") => "tempo-precompiles-macros",
        (ProcKind::Derive, "Metrics") if text.contains("metrics") => "metrics-derive",
        (ProcKind::Derive, "RlpEncodable" | "RlpDecodable" | "RlpEncodableWrapper" | "RlpDecodableWrapper") => "alloy-rlp-derive",
        (ProcKind::Derive, "SchemaRead" | "SchemaWrite") => "wincode-derive",
        (ProcKind::Derive, "TransactionEnvelope") => "alloy-tx-macros",
        (ProcKind::Derive, "Compact") if text.contains("reth_codecs") => "reth-codecs-derive",
        (ProcKind::Derive, "Deref" | "DerefMut" | "Display" | "FromStr" | "From" | "Into" | "AsRef" | "Constructor") if text.contains("derive_more") => "derive_more-impl",
        (ProcKind::Attr, "contract") if text.contains("tempo_precompiles_macros") => "tempo-precompiles-macros",
        (ProcKind::Attr, "test" | "main") if path == "tokio" => "tokio-macros",
        (ProcKind::Attr, "test_case" | "test_matrix") if path == "test_case" || text.contains("use test_case") => "test-case-macros",
        (ProcKind::Attr, "test_traced") if path == "commonware_macros" || text.contains("commonware_macros") => "commonware-macros-impl",
        (ProcKind::Attr, "instrument") if path == "tracing" || text.contains("tracing") => "tracing-attributes",
        (ProcKind::Attr, "async_trait") => "async-trait",
        (ProcKind::Attr, "auto_impl") => "auto_impl",
        (ProcKind::Attr, "pin_project") => "pin-project-internal",
        (ProcKind::Attr, "rpc") if text.contains("jsonrpsee") => "jsonrpsee-proc-macros",
        (ProcKind::FnLike, "sol") => "alloy-sol-macro",
        (ProcKind::FnLike, "paste") => "paste",
        (ProcKind::FnLike, _) if text.contains("tempo_precompiles_macros") => "tempo-precompiles-macros",
        _ => return None,
    };
    let module = modules.iter().filter(|it| it.name == krate).max_by(|a, b| a.version.cmp(&b.version))?;
    Some((module, module.find(kind, name)?))
}

/// The parts' clock: nanoseconds since its first call.
fn clock() -> u64 {
    static START: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_nanos() as u64
}

/// One pass over `checkout`: every call expanded, from the records in `records` when they
/// hold it.
fn pass(checkout: &Path, modules: &[ModuleRecord], records: &Path, label: &str, show: bool) -> BTreeMap<String, Stats> {
    let edition = Edition::E2024;
    let runtime = Runtime::timed(clock).expect("the runtime");
    let store = expansion_records(records);
    let mut loaded: BTreeMap<String, std::sync::Arc<Loaded>> = BTreeMap::new();
    let mut stats: BTreeMap<String, Stats> = BTreeMap::new();
    let mut shown = false;
    let started = Instant::now();
    let mut files = 0;
    // Where the pass's time goes: the harness's own stages, and a call's key and lookup.
    let (mut t_walk, mut t_read, mut t_lex, mut t_scan, mut t_key, mut t_find, mut t_check): (Duration, Duration, Duration, Duration, Duration, Duration, Duration) = Default::default();
    let mut t_expand = Duration::ZERO;
    let mut t_decode = Duration::ZERO;
    let mut t_read_one = Duration::ZERO;
    let mut answer_bytes = 0usize;
    let mut lap = Instant::now();
    let mut stage = |into: &mut Duration| {
        let now = Instant::now();
        *into += now - lap;
        lap = now;
    };
    let all = sources(checkout);
    stage(&mut t_walk);
    for path in all {
        let Ok(text) = std::fs::read_to_string(&path) else { continue };
        stage(&mut t_read);
        if !(text.contains("#[") || text.contains("! ") || text.contains("!{") || text.contains("!(")) {
            continue;
        }
        let Ok(stream) = lex(&text, edition, Alloc::global()) else { continue };
        stage(&mut t_lex);
        files += 1;
        let rel = path.strip_prefix(checkout).unwrap_or(&path).to_string_lossy().into_owned();
        let package = Arc::new(package_of(checkout, &path));
        let calls = calls_in(&rel, &stream, &package);
        stage(&mut t_scan);
        for call in calls {
            for name in &call.names {
                let Some((module, export)) = macro_for(modules, &text, &call, name) else { continue };
                let entry = stats.entry(name.clone()).or_default();
                entry.calls += 1;
                let begun = Instant::now();
                let proc_call = ProcCall { kind: call.kind, input: &call.input, attr_args: call.attr.as_ref() };
                stage(&mut t_scan);
                let base = base_key(&module.wasm_sha256, export, edition, &proc_call);
                stage(&mut t_key);
                let found = find(&store, &base, &call.package, edition, Alloc::global());
                stage(&mut t_find);
                let produced: Produced<'static> = match found {
                    Ok(Some(produced)) => {
                        entry.from_record += 1;
                        produced
                    }
                    Ok(None) => {
                        if !loaded.contains_key(&module.key) {
                            let load = runtime.load(module).expect("the module loads");
                            println!("{label}: {} {} {} in {:.2?}", module.name, module.version, if load.compiled { "compiled" } else { "loaded from its compiled record" }, load.load_time);
                            loaded.insert(module.key.clone(), load);
                        }
                        let ran = runtime.expand(&loaded[&module.key], export, &proc_call, edition, call.package.clone(), Limits::STANDARD, Alloc::global());
                        match ran {
                            Ok(ran) => {
                                keep(&store, base, &ran.produced).expect("the record is kept");
                                ran.produced
                            }
                            Err(error) => {
                                let cause = format!("{error}");
                                let cause = cause.chars().take(160).collect::<String>();
                                if *entry.failed.get(&cause).unwrap_or(&0) == 0 {
                                    println!("{label}: FAILED {} {} ({}): {cause}", name, call.item, call.file);
                                }
                                *entry.failed.entry(cause).or_default() += 1;
                                entry.time += begun.elapsed();
                                continue;
                            }
                        }
                    }
                    Err(error) => panic!("{error}"),
                };
                entry.time += begun.elapsed();
                stage(&mut t_expand);
                // The floor of a hit (rule 67): the key, one read of a file of the record's size
                // (the record itself, in the page cache as `find`'s was) and its decode.
                let bytes = record_bytes(base, &produced).expect("the record's bytes");
                let slot = records.join("proc-macro/records/list").join(format!("{:x}", list_key(&base, Slot { generation: 0, n: 0 })));
                stage(&mut t_check);
                let read = std::fs::read(&slot).expect("the record in its first slot");
                stage(&mut t_read_one);
                let list = ListRef::open(&read, &base).expect("the record opens");
                let again = decode(list, edition, Alloc::global()).expect("the answer decodes");
                stage(&mut t_decode);
                answer_bytes += bytes.len();
                drop(again);
                if produced.diagnostics.iter().any(|it| matches!(it, ProcDiagnostic::Panicked { .. })) {
                    entry.panicked += 1;
                    let message = produced.diagnostics.iter().find_map(|it| match it {
                        ProcDiagnostic::Panicked { message } => message.get(&produced.text),
                        _ => None,
                    });
                    println!("{label}: PANICKED {name} {} ({}): {}", call.item, call.file, message.unwrap_or("?"));
                    continue;
                }
                entry.expanded += 1;
                let printed = print(&produced.tokens);
                let outline = blaze_file_items::outline(&printed, edition, Alloc::global());
                if outline.errors().is_empty() && !outline.items().is_empty() {
                    entry.outlined += 1;
                } else if show && entry.expanded - entry.outlined <= 3 {
                    let first = outline.errors().first().map(|it| outline.message(it)).unwrap_or("no items");
                    let head: String = printed.chars().take(200).collect();
                    println!("{label}: does not outline: {name} {} ({}): {first}: {head}", call.item, call.file);
                }
                if call.kind != ProcKind::Derive {
                } else if !call.item.is_empty() && has_impl(&produced.tokens, name, &call.item) {
                    entry.impls += 1;
                } else if show {
                    let head: String = printed.chars().take(300).collect();
                    println!("{label}: no `impl {name} for {}` ({}): {head}", call.item, call.file);
                }
                if show && !shown && name == "Serialize" {
                    shown = true;
                    let head: String = printed.chars().take(400).collect();
                    println!("{label}: {name} of {} ({}), {} tokens, {} reads: {head}", call.item, call.file, produced.tokens.len(), produced.reads.list().len());
                }
                stage(&mut t_check);
            }
        }
    }
    println!("{label}: {files} files with `derive` read in {:.2?}", started.elapsed());
    let calls: u32 = stats.values().map(|s| s.calls).sum();
    let per = |d: Duration| d / calls.max(1);
    println!(
        "{label}: profile: walk {t_walk:.2?}, read files {t_read:.2?}, lex {t_lex:.2?}, find calls {t_scan:.2?}, key {t_key:.2?} ({:.2?}/call), record lookup {t_find:.2?} ({:.2?}/call), expand+keep {t_expand:.2?}, print+outline+impl check {t_check:.2?}",
        per(t_key),
        per(t_find)
    );
    println!("{label}: decode alone {t_decode:.2?} ({:.2?}/call), answers {answer_bytes} bytes ({} a call)", per(t_decode), answer_bytes / calls.max(1) as usize);
    println!(
        "{label}: floor of a hit: key {:.2?} + one read {:.2?} + decode {:.2?} = {:.2?}/call; the lookup {:.2?}/call is {:.2}x it",
        per(t_key),
        per(t_read_one),
        per(t_decode),
        per(t_key + t_read_one + t_decode),
        per(t_key + t_find),
        (t_key + t_find).as_secs_f64() / (t_key + t_read_one + t_decode).as_secs_f64().max(1e-12)
    );
    // A run's own parts (`Runtime::expand`'s rows).
    for row in runtime.parts().rows() {
        if row.entered != 0 {
            println!("{label}: run part {:<12} {:>9.1} ms  entered {:>8} ({:>8.2} us each)", row.part.name(), row.nanos as f64 / 1e6, row.entered, row.nanos as f64 / 1e3 / row.entered as f64);
        }
    }
    for (name, s) in &stats {
        let failed: u32 = s.failed.values().sum();
        println!(
            "{label}: {name:<12} calls {:>4} expanded {:>4} panicked {:>3} failed {:>3} outline-as-items {:>4} impl-by-name {:>4} from-records {:>4} time {:.2?}",
            s.calls, s.expanded, s.panicked, failed, s.outlined, s.impls, s.from_record, s.time
        );
        for (cause, n) in &s.failed {
            println!("{label}:     {n} x {cause}");
        }
    }
    stats
}

/// Every file under `dir`, by its path from `dir`, with its bytes.
fn files(dir: &Path) -> BTreeMap<String, Vec<u8>> {
    let mut out = BTreeMap::new();
    let mut stack = vec![dir.to_owned()];
    while let Some(at) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&at) else { continue };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else {
                let rel = path.strip_prefix(dir).unwrap_or(&path).to_string_lossy().into_owned();
                out.insert(rel, std::fs::read(&path).unwrap_or_default());
            }
        }
    }
    out
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let [_, checkout, copy, store, out] = &args[..] else {
        panic!("usage: tempo_derives <checkout> <checkout copy> <module store> <out>");
    };
    let (checkout, copy, store, out) = (Path::new(checkout), Path::new(copy), Path::new(store), Path::new(out));
    let modules = modules(store, checkout);
    for module in &modules {
        println!("module {} {} {} ({} bytes): {} macros", module.name, module.version, module.key, module.wasm.len(), module.macros.len());
    }
    let (a, b) = (out.join("a"), out.join("b"));
    let _ = std::fs::remove_dir_all(&a);
    let _ = std::fs::remove_dir_all(&b);
    let first = Instant::now();
    pass(checkout, &modules, &a, "first run", true);
    println!("first run: {:.2?} in all", first.elapsed());
    let again = Instant::now();
    pass(checkout, &modules, &a, "from records", false);
    println!("from records: {:.2?} in all", again.elapsed());
    pass(copy, &modules, &b, "moved checkout", false);
    let runtime = Runtime::new().expect("the runtime");
    for module in &modules {
        let code = module.dir.join(format!("compiled-{}", runtime.code_key(module)));
        let bytes = std::fs::metadata(&code).map_or(0, |it| it.len());
        println!("compiled record of {} {}: {bytes} bytes (module {} bytes)", module.name, module.version, module.wasm.len());
    }
    let (ra, rb) = (files(&a.join("proc-macro/records")), files(&b.join("proc-macro/records")));
    let (lists, answers) = (ra.keys().filter(|k| k.starts_with("list")).count(), ra.keys().filter(|k| k.starts_with("answer")).count());
    let bytes: usize = ra.values().map(Vec::len).sum();
    println!("records: {lists} read-lists, {answers} answers, {bytes} bytes");
    let differ: Vec<&String> = ra.keys().chain(rb.keys()).filter(|k| ra.get(*k) != rb.get(*k)).collect();
    println!(
        "records: {} files at {}, {} at {}: {}",
        ra.len(),
        checkout.display(),
        rb.len(),
        copy.display(),
        if differ.is_empty() { "equal to the byte".to_owned() } else { format!("{} differ, first {}", differ.len(), differ[0]) }
    );
}
