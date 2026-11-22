//! The `macro_rules!` engine on the standard library: every `name!(..)` call in library/core,
//! alloc and std, paired with each definition of that name in its crate (else core's), is
//! expanded, and so are the nested calls of each expansion, to a depth of eight. With `--check
//! FILE` each definition's expansions are compared with the recorded baseline, a digest of
//! their renderings ([`compare::render`]) recorded from mbe's engine while it was beside this
//! one (`rules_compare/library-nightly-2026-10-27.digests`; the two agreed on every call, token
//! for token and in bytes). Then the cost on the same calls: expansions a second and bytes
//! allocated a call, beside the floor (writing the expansion's tokens and origins once; in
//! bytes, its tables at their lengths).
//!
//! `examplerun.sh blaze-expand rules_core <rust-src>/library [--reps N] [--show N]
//! [--check FILE | --record FILE] [--only]`

#![feature(allocator_api)]

#[path = "rules_compare/compare.rs"]
mod compare;

use std::alloc::{GlobalAlloc, Layout, System};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use blaze_expand::{
    Call, CallSite, Ctx, DefAt, DefSite, Edition, ExpandCx, ExpandError, Expanded, Export,
    HostAsk, HostError, HostWhy, IncludeAs, Included, LocalInner, MacroDefinition, NestedKind,
    Place, PlaceOf, Prepared, ProcCall, Produced, TokenKind, TokenStream, call_key,
    expand_prepared, find_calls, lex, prepare,
};
use blaze_id::{ContentId, CrateItemKey};
use blaze_own::Alloc;

/// Counts the bytes the process allocates.
struct Counting;

static BYTES: AtomicU64 = AtomicU64::new(0);

// SAFETY: forwards to `System`, counting.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        BYTES.fetch_add(layout.size() as u64, Ordering::Relaxed);
        // SAFETY: the caller's contract.
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: the caller's contract.
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        BYTES.fetch_add(layout.size() as u64, Ordering::Relaxed);
        // SAFETY: the caller's contract.
        unsafe { System.alloc_zeroed(layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        BYTES.fetch_add(new_size.saturating_sub(layout.size()) as u64, Ordering::Relaxed);
        // SAFETY: the caller's contract.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

/// The outside of the expansions: nothing a declarative macro reads.
struct NoCx;

const REFUSED: HostError = HostError { ask: HostAsk::ProcMacro, why: HostWhy::Failed };

impl ExpandCx for NoCx {
    fn env(&self, _: &str) -> Result<Option<&str>, HostError> {
        Ok(None)
    }
    fn cfg(&self, _: &TokenStream<'_>) -> Result<bool, HostError> {
        Ok(false)
    }
    fn include(&self, _: &str, _: IncludeAs) -> Result<Included<'_>, HostError> {
        Ok(Included::Missing)
    }
    fn place(&self, _: PlaceOf) -> Result<Place<'_>, HostError> {
        Ok(Place::Number(1))
    }
    fn run_proc_macro<'a>(
        &self,
        _: &MacroDefinition<'_>,
        _: ProcCall<'_>,
        _: Alloc<'a>,
    ) -> Result<Produced<'a>, HostError> {
        Err(REFUSED)
    }
}

const CRATES: [&str; 3] = ["core", "alloc", "std"];

struct Def {
    krate: usize,
    name: String,
    prepared: Prepared<'static>,
}

/// One call: its macro's name, its input, where it is written.
struct CallAt {
    krate: usize,
    name: String,
    input: &'static TokenStream<'static>,
    file: usize,
    depth: u32,
}

fn site() -> CallSite {
    CallSite { edition: Edition::E2024, panic_edition: Edition::E2024, ctx: Ctx::root(Edition::E2024) }
}

fn files(dir: &Path, out: &mut Vec<PathBuf>) {
    let (mut stack, mut entries) = (Vec::from([dir.to_path_buf()]), Vec::new());
    while let Some(path) = stack.pop() {
        if path.is_dir() {
            let Ok(read) = std::fs::read_dir(&path) else { continue };
            entries.clear();
            entries.extend(read.filter_map(|it| it.ok().map(|it| it.path())));
            entries.sort();
            stack.extend(entries.drain(..).rev());
        } else if path.extension().is_some_and(|it| it == "rs") {
            out.push(path);
        }
    }
}

/// What `--record FILE` and `--check FILE` do with the baseline: one line per definition, its
/// crate, name and index among its crate's definitions of that name, and a digest over the
/// rendering ([`compare::render`]) of every call of it, in the run's order.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Baseline {
    Record,
    Check,
}

/// The last identifier of a path.
fn last_ident(stream: &TokenStream<'_>, start: u32, end: u32) -> Option<String> {
    (start..end)
        .rev()
        .find(|&at| matches!(stream.tokens()[at as usize].kind, TokenKind::Ident { .. }))
        .map(|at| stream.text_at(at as usize).to_owned())
}

fn leak<T>(value: T) -> &'static T {
    Box::leak(Box::new(value))
}

fn main() {
    // An entry that takes no grant yet (a plain `fn main`): the run's memory is one frame
    // taken from the OS here, kept for the process's life since the leaked rows below hold
    // what it made, and handed down.
    let frame: &'static blaze_mem::Frame =
        leak(blaze_mem::Frame::with_reserve(1 << 36, blaze_mem::Tag::Expand).expect("the entry's frame"));
    let erased: &'static blaze_own::Erased<&'static blaze_mem::Frame> = leak(blaze_own::Erased::new(frame));
    let alloc: Alloc<'static> = erased.handle();
    // The panic and its frames on stdout, which the run prints whole.
    std::panic::set_hook(Box::new(|info| {
        let trace = std::backtrace::Backtrace::force_capture().to_string();
        let frames: Vec<&str> = trace
            .lines()
            .filter(|line| line.contains("blaze_expand") || line.contains("rules_core"))
            .take(24)
            .collect();
        println!("{info}\n{}", frames.join("\n"));
    }));
    let mut args = std::env::args().skip(1);
    let mut library = None;
    let mut reps = 3u32;
    let mut show = 12usize;
    let mut only = false;
    let mut baseline = None;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--reps" => reps = args.next().and_then(|it| it.parse().ok()).unwrap_or(reps),
            "--show" => show = args.next().and_then(|it| it.parse().ok()).unwrap_or(show),
            "--record" => baseline = args.next().map(|it| (PathBuf::from(it), Baseline::Record)),
            "--check" => baseline = args.next().map(|it| (PathBuf::from(it), Baseline::Check)),
            "--only" => only = true,
            "--oracle" => blaze_expand::rules_oracle(true),
            _ => library = Some(PathBuf::from(arg)),
        }
    }
    let Some(library) = library else {
        eprintln!(
            "usage: rules_core <rust-src>/library [--reps N] [--show N] [--check FILE | --record FILE] [--only]"
        );
        std::process::exit(2);
    };

    // Lex every file; list the definitions and the calls outside them.
    let mut paths = Vec::new();
    let mut defs: Vec<Def> = Vec::new();
    let mut calls: Vec<CallAt> = Vec::new();
    let mut refused_defs = 0usize;
    for (krate, name) in CRATES.iter().enumerate() {
        let mut list = Vec::new();
        files(&library.join(name).join("src"), &mut list);
        for path in list {
            let Ok(text) = std::fs::read_to_string(&path) else { continue };
            let Ok(stream) = lex(&text, Edition::E2024, alloc) else { continue };
            let stream: &'static TokenStream<'static> = leak(stream);
            let file = paths.len();
            paths.push(path);
            let found = find_calls(stream, alloc).expect("the calls are listed");
            for call in found.iter() {
                let inner = call.inner();
                let input = stream
                    .copy_in(inner.start as usize..inner.end as usize, alloc)
                    .expect("a call's input copies");
                match call.kind {
                    NestedKind::MacroRules => {
                        let Some(name_at) = call.name else { continue };
                        let at = DefAt {
                            site: DefSite { krate: CrateItemKey(ContentId([krate as u8 + 1; 32])), module: None },
                            edition: Edition::E2024,
                            export: Export::Exported,
                            local_inner: LocalInner::No,
                            transparency: None,
                        };
                        let def: &'static MacroDefinition<'static> = leak(MacroDefinition::rules(input, at));
                        match prepare(def) {
                            Ok(prepared) => defs.push(Def {
                                krate,
                                name: stream.text_at(name_at as usize).to_owned(),
                                prepared,
                            }),
                            Err(_) => refused_defs += 1,
                        }
                    }
                    NestedKind::FnLike => {
                        let Some(name) = last_ident(stream, call.path.start, call.path.end) else { continue };
                        calls.push(CallAt { krate, name, input: leak(input), file, depth: 0 });
                    }
                    _ => {}
                }
            }
        }
    }
    let mut by_name: BTreeMap<(usize, &str), Vec<usize>> = BTreeMap::new();
    for (idx, def) in defs.iter().enumerate() {
        by_name.entry((def.krate, def.name.as_str())).or_default().push(idx);
    }
    let resolve = |krate: usize, name: &str| -> Vec<usize> {
        by_name
            .get(&(krate, name))
            .or_else(|| by_name.get(&(0, name)))
            .cloned()
            .unwrap_or_default()
    };

    // The run, breadth first over nested calls.
    let mut pairs: Vec<(usize, &'static TokenStream<'static>)> = Vec::new();
    let mut expanded = 0usize;
    let mut diagnosed = 0usize;
    let mut refused = 0usize;
    let mut queue: std::collections::VecDeque<CallAt> = calls.into_iter().collect();
    let top_calls = queue.len();
    let mut unresolved = 0usize;
    let mut digests = vec![compare::FNV_START; defs.len()];
    const MAX_PAIRS: usize = 400_000;
    while let Some(call) = queue.pop_front() {
        let targets = resolve(call.krate, &call.name);
        if targets.is_empty() {
            unresolved += 1;
            continue;
        }
        for idx in targets {
            if pairs.len() >= MAX_PAIRS {
                break;
            }
            let def = &defs[idx];
            let fn_call = Call::fn_like(call.input, site());
            let result = expand_prepared(&def.prepared, &fn_call, &NoCx, alloc);
            pairs.push((idx, call.input));
            digests[idx] = compare::fnv(digests[idx], compare::render(&result).as_bytes());
            if let Err(error) = &result {
                refused += 1;
                if refused <= show {
                    println!(
                        "  refused: {}! in {} (depth {}): {error}",
                        def.name,
                        paths[call.file].strip_prefix(&library).unwrap_or(&paths[call.file]).display(),
                        call.depth
                    );
                }
            }
            // Expand the nested calls of the expansion.
            if let Ok(Expanded::Expansion(exp)) = &result {
                if exp.diagnostics.is_empty() {
                    expanded += 1;
                } else {
                    diagnosed += 1;
                }
                if call.depth >= 8 || !exp.diagnostics.is_empty() {
                    continue;
                }
                let key = call_key(def.prepared.def(), &fn_call);
                for nested in exp.nested.iter() {
                    if nested.kind != NestedKind::FnLike {
                        continue;
                    }
                    let Some(name) = last_ident(&exp.tokens, nested.path.start, nested.path.end) else { continue };
                    let inner = nested.inner();
                    let Ok(mut input) =
                        exp.tokens.copy_in(inner.start as usize..inner.end as usize, alloc)
                    else {
                        continue;
                    };
                    input.seal(key.0);
                    queue.push_back(CallAt {
                        krate: call.krate,
                        name,
                        input: leak(input),
                        file: call.file,
                        depth: call.depth + 1,
                    });
                }
            }
        }
    }
    println!(
        "rules_core: {} definitions ({refused_defs} refused by `prepare`), {top_calls} calls in the source, {unresolved} calls of no `macro_rules!` here",
        defs.len()
    );
    println!(
        "rules_core: {} calls expanded (nested to depth 8): {expanded} expanded, {diagnosed} gave a diagnostic, {refused} refused",
        pairs.len()
    );
    if let Some((path, mode)) = &baseline {
        let mut seen: BTreeMap<(usize, &str), usize> = BTreeMap::new();
        let mut lines = String::new();
        for (idx, def) in defs.iter().enumerate() {
            let nth = seen.entry((def.krate, def.name.as_str())).or_default();
            lines.push_str(&format!("{} {} {} {:016x}\n", CRATES[def.krate], def.name, nth, digests[idx]));
            *nth += 1;
        }
        match mode {
            Baseline::Record => {
                std::fs::write(path, &lines).expect("the baseline is written");
                println!("rules_core: recorded {} definitions' digests in {}", defs.len(), path.display());
            }
            Baseline::Check => {
                let recorded = std::fs::read_to_string(path).expect("the baseline is read");
                let differ: Vec<(&str, &str)> = recorded
                    .lines()
                    .zip(lines.lines())
                    .filter(|(a, b)| a != b)
                    .collect();
                let same_len = recorded.lines().count() == lines.lines().count();
                println!(
                    "rules_core: against the baseline {}: {} definitions, {} differ{}",
                    path.display(),
                    defs.len(),
                    differ.len(),
                    if same_len { "" } else { "; the lists differ in length" }
                );
                for (a, b) in differ.iter().take(show.max(1)) {
                    println!("    recorded {a}\n    now      {b}");
                }
                if !differ.is_empty() || !same_len {
                    std::process::exit(1);
                }
            }
        }
    }

    // The cost: every pair, `reps` times, twice (the second is reported).
    if reps == 0 {
        return;
    }
    let run = |pairs: &[(usize, &'static TokenStream<'static>)]| {
        let bytes0 = BYTES.load(Ordering::Relaxed);
        let start = Instant::now();
        let mut n = 0u64;
        for _ in 0..reps {
            for &(idx, input) in pairs {
                let call = Call::fn_like(input, site());
                let result = expand_prepared(&defs[idx].prepared, &call, &NoCx, alloc);
                std::hint::black_box(&result);
                drop(result);
                n += 1;
            }
        }
        let secs = start.elapsed().as_secs_f64();
        let bytes = BYTES.load(Ordering::Relaxed) - bytes0;
        (n as f64 / secs, bytes as f64 / n as f64, secs)
    };
    if only {
        // For a sampling profiler: the expansions alone.
        let (_, _, secs) = run(&pairs);
        println!("rules_core: {reps} rounds alone in {secs:.2} s");
        return;
    }
    let _warm = run(&pairs);
    let (rate, bytes, secs) = run(&pairs);
    // The floor: the expansion's tokens and origins written once, token by token, per call;
    // and in bytes, the expansion's tables at their lengths.
    let results: Vec<Result<Expanded<'static>, ExpandError>> = pairs
        .iter()
        .map(|&(idx, input)| {
            let call = Call::fn_like(input, site());
            expand_prepared(&defs[idx].prepared, &call, &NoCx, alloc)
        })
        .collect();
    let start = Instant::now();
    let mut n = 0u64;
    for _ in 0..reps {
        for result in &results {
            if let Ok(Expanded::Expansion(exp)) = result {
                let tokens = exp.tokens.copy_in(0..exp.tokens.len(), alloc);
                let origins = exp.origins.clone();
                std::hint::black_box((&tokens, &origins));
            }
            n += 1;
        }
    }
    let floor_secs = start.elapsed().as_secs_f64();
    let exact: usize = results
        .iter()
        .map(|result| match result {
            Ok(Expanded::Expansion(exp)) => {
                exp.tokens.len() * size_of::<blaze_expand::Token>()
                    + exp.tokens.text_table().len()
                    + exp.tokens.contexts().len() * size_of::<blaze_expand::CtxNode>()
                    + exp.origins.len() * size_of::<blaze_expand::TokenOrigin>()
                    + exp.nested.len() * size_of::<blaze_expand::NestedCall>()
                    + exp.notes.len()
                    + exp.diagnostics.len() * size_of::<blaze_expand::Diagnostic>()
            }
            _ => 0,
        })
        .sum();
    let floor_bytes = exact as f64 / results.len() as f64;
    let floor_rate = n as f64 / floor_secs;
    println!("rules_core cost over {} calls x {reps}:", pairs.len());
    println!("  expand: {rate:.0} expansions/s, {bytes:.0} bytes allocated a call ({secs:.2} s)");
    println!(
        "  floor (write the output once; its tables at their lengths): {floor_rate:.0} expansions/s, {floor_bytes:.0} bytes a call"
    );
    println!(
        "  against the floor: {:.2}x its time, {:.2}x its bytes",
        floor_rate / rate,
        bytes / floor_bytes
    );
    // `--oracle`: both matchers on every arm tried; a difference fails the run.
    let [equal, different, exact, failed, calls] = blaze_expand::rules_oracle_counts();
    if calls > 0 {
        println!(
            "rules oracle: {calls} calls: {equal} arms equal, {different} different; {exact} arms left to the exact mode; {failed} calls no arm matched"
        );
        if different > 0 {
            std::process::exit(1);
        }
    }
}
