//! The `macro_rules!` engine on one crate's own macros, by macro: every `name!(..)` call in
//! the folder given (the pinned library's vendored libc: `<rust-src>/library/vendor/libc-*/src`),
//! paired with the folder's definition of that name, is expanded, and so are the nested calls of
//! each expansion (a tt-muncher's own calls of itself: libc's `custom_struct!` and `cfg_if!`),
//! to a depth of 256. Then the cost of each macro named by `--names a,b` (else the eight with
//! the most input): calls, input and output tokens, nanoseconds a call and an input token,
//! beside the floor (writing the expansion's tokens and origins once). A digest of every
//! expansion's rendering ([`compare::render`]) is printed: a change that is only about speed
//! leaves it equal.
//!
//! `mixcheck.sh blaze-expand run rules_libc <folder> [--names a,b] [--reps N]
//! [--sample FILE SECS]`: with `--sample` the named macros' calls are expanded for SECS seconds
//! while macOS `sample` writes this process's call tree to FILE.

#![feature(allocator_api)]

#[path = "rules_compare/compare.rs"]
mod compare;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use blaze_expand::{
    Call, CallSite, Ctx, DefAt, DefSite, Edition, ExpandCx, Expanded, Export, HostAsk, HostError,
    HostWhy, IncludeAs, Included, LocalInner, MacroDefinition, NestedKind, Place, PlaceOf,
    Prepared, ProcCall, Produced, TokenKind, TokenStream, call_key, expand_prepared, find_calls,
    lex, prepare,
};
use blaze_id::{ContentId, CrateItemKey};
use blaze_own::Alloc;

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

struct Def {
    name: String,
    prepared: Prepared<'static>,
}

/// One call: its macro's name, its input, how deep under a call in the source it was made.
struct CallAt {
    name: String,
    input: &'static TokenStream<'static>,
    depth: u32,
}

/// One macro's calls and what they cost.
#[derive(Default)]
struct Row {
    calls: Vec<(usize, &'static TokenStream<'static>)>,
    input: usize,
    output: usize,
}

const DEPTH: u32 = 256;
const MAX_PAIRS: usize = 600_000;

fn site() -> CallSite {
    CallSite { edition: Edition::E2021, panic_edition: Edition::E2021, ctx: Ctx::root(Edition::E2021) }
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
    let mut args = std::env::args().skip(1);
    let mut folder: Option<PathBuf> = None;
    let mut names: Vec<String> = Vec::new();
    let mut reps = 20usize;
    let mut sample: Option<(String, u64)> = None;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--names" => names = args.next().map(|it| it.split(',').map(str::to_owned).collect()).unwrap_or_default(),
            "--reps" => reps = args.next().and_then(|it| it.parse().ok()).unwrap_or(reps),
            "--oracle" => blaze_expand::rules_oracle(true),
            "--sample" => {
                let file = args.next().expect("--sample FILE SECS");
                let secs = args.next().and_then(|it| it.parse().ok()).expect("--sample FILE SECS");
                sample = Some((file, secs));
            }
            _ => folder = Some(PathBuf::from(arg)),
        }
    }
    let Some(folder) = folder else {
        eprintln!("usage: rules_libc <folder> [--names a,b] [--reps N] [--sample FILE SECS]");
        std::process::exit(2);
    };

    // Lex every file; list the definitions and the calls outside them.
    let mut list = Vec::new();
    files(&folder, &mut list);
    let mut defs: Vec<Def> = Vec::new();
    let mut calls: Vec<CallAt> = Vec::new();
    let mut refused_defs = 0usize;
    for path in &list {
        let Ok(text) = std::fs::read_to_string(path) else { continue };
        let Ok(stream) = lex(&text, Edition::E2021, alloc) else { continue };
        let stream: &'static TokenStream<'static> = leak(stream);
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
                        site: DefSite { krate: CrateItemKey(ContentId([1; 32])), module: None },
                        edition: Edition::E2021,
                        export: Export::Exported,
                        local_inner: LocalInner::No,
                        transparency: None,
                    };
                    let def: &'static MacroDefinition<'static> = leak(MacroDefinition::rules(input, at));
                    match prepare(def) {
                        Ok(prepared) => defs.push(Def { name: stream.text_at(name_at as usize).to_owned(), prepared }),
                        Err(_) => refused_defs += 1,
                    }
                }
                NestedKind::FnLike => {
                    let Some(name) = last_ident(stream, call.path.start, call.path.end) else { continue };
                    calls.push(CallAt { name, input: leak(input), depth: 0 });
                }
                _ => {}
            }
        }
    }
    // A name defined twice (one definition per platform's cfg): the first, in the files' order.
    let mut by_name: BTreeMap<&str, usize> = BTreeMap::new();
    for (idx, def) in defs.iter().enumerate() {
        by_name.entry(def.name.as_str()).or_insert(idx);
    }

    // The run, breadth first over nested calls.
    let mut rows: Vec<Row> = (0..defs.len()).map(|_| Row::default()).collect();
    let mut queue: std::collections::VecDeque<CallAt> = calls.into_iter().collect();
    let top_calls = queue.len();
    let (mut pairs, mut expanded, mut diagnosed, mut refused, mut unresolved, mut cut) = (0usize, 0usize, 0usize, 0usize, 0usize, 0usize);
    let mut digest = compare::FNV_START;
    while let Some(call) = queue.pop_front() {
        let Some(&idx) = by_name.get(call.name.as_str()) else {
            unresolved += 1;
            continue;
        };
        if pairs >= MAX_PAIRS {
            cut += 1;
            continue;
        }
        let def = &defs[idx];
        let fn_call = Call::fn_like(call.input, site());
        let result = expand_prepared(&def.prepared, &fn_call, &NoCx, alloc);
        pairs += 1;
        digest = compare::fnv(digest, def.name.as_bytes());
        digest = compare::fnv(digest, compare::render(&result).as_bytes());
        rows[idx].calls.push((idx, call.input));
        rows[idx].input += call.input.len();
        match &result {
            Err(_) => refused += 1,
            Ok(Expanded::Expansion(exp)) => {
                rows[idx].output += exp.tokens.len();
                if !exp.diagnostics.is_empty() {
                    diagnosed += 1;
                    continue;
                }
                expanded += 1;
                if call.depth >= DEPTH {
                    cut += 1;
                    continue;
                }
                let key = call_key(def.prepared.def(), &fn_call);
                for nested in exp.nested.iter() {
                    if nested.kind != NestedKind::FnLike {
                        continue;
                    }
                    let Some(name) = last_ident(&exp.tokens, nested.path.start, nested.path.end) else { continue };
                    let inner = nested.inner();
                    let Ok(mut input) = exp.tokens.copy_in(inner.start as usize..inner.end as usize, alloc) else {
                        continue;
                    };
                    input.seal(key.0);
                    queue.push_back(CallAt { name, input: leak(input), depth: call.depth + 1 });
                }
            }
            Ok(_) => {}
        }
    }
    println!(
        "rules_libc: {} files, {} definitions ({refused_defs} refused by `prepare`), {top_calls} calls in the source, {unresolved} calls of no `macro_rules!` here",
        list.len(),
        defs.len()
    );
    println!(
        "rules_libc: {pairs} calls expanded (nested to depth {DEPTH}; {cut} cut): {expanded} expanded, {diagnosed} gave a diagnostic, {refused} refused; digest {digest:016x}"
    );

    // The macros measured: the ones named, else the eight with the most input.
    let mut chosen: Vec<usize> = match names.is_empty() {
        false => names
            .iter()
            .map(|name| *by_name.get(name.as_str()).unwrap_or_else(|| panic!("no `macro_rules! {name}` in the folder")))
            .collect(),
        true => {
            let mut all: Vec<usize> = (0..defs.len()).filter(|&idx| !rows[idx].calls.is_empty()).collect();
            all.sort_by_key(|&idx| (core::cmp::Reverse(rows[idx].input), idx));
            all.truncate(8);
            all
        }
    };
    chosen.dedup();
    let once = |list: &[(usize, &'static TokenStream<'static>)]| {
        for &(idx, input) in list {
            let call = Call::fn_like(input, site());
            let result = expand_prepared(&defs[idx].prepared, &call, &NoCx, alloc);
            std::hint::black_box(&result);
        }
    };
    if let Some((file, secs)) = &sample {
        // For the sampling profiler: the chosen macros' expansions alone, for `secs` seconds.
        let pid = std::process::id().to_string();
        let mut child = std::process::Command::new("/usr/bin/sample")
            .args([pid.as_str(), &secs.to_string(), "-file", file.as_str()])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("`sample` starts");
        let until = Instant::now() + Duration::from_secs(*secs + 2);
        let mut rounds = 0u64;
        while Instant::now() < until {
            for &idx in &chosen {
                once(&rows[idx].calls);
            }
            rounds += 1;
        }
        let status = child.wait().expect("`sample` ends");
        println!("rules_libc: {rounds} rounds while sampled into {file} ({status})");
        return;
    }
    if reps == 0 {
        return;
    }
    println!("rules_libc cost, {reps} rounds (the second pass of two is reported):");
    let (mut all_ns, mut all_calls, mut all_in, mut all_floor) = (0f64, 0usize, 0usize, 0f64);
    for &idx in &chosen {
        let row = &rows[idx];
        let pass = || {
            let start = Instant::now();
            for _ in 0..reps {
                once(&row.calls);
            }
            start.elapsed().as_nanos() as f64 / reps as f64
        };
        let _warm = pass();
        let ns = pass();
        // The floor: each expansion's tokens and origins written once.
        let results: Vec<_> = row
            .calls
            .iter()
            .map(|&(idx, input)| expand_prepared(&defs[idx].prepared, &Call::fn_like(input, site()), &NoCx, alloc))
            .collect();
        let start = Instant::now();
        for _ in 0..reps {
            for result in &results {
                if let Ok(Expanded::Expansion(exp)) = result {
                    let tokens = exp.tokens.copy_in(0..exp.tokens.len(), alloc);
                    let origins = exp.origins.clone();
                    std::hint::black_box((&tokens, &origins));
                }
            }
        }
        let floor = start.elapsed().as_nanos() as f64 / reps as f64;
        let n = row.calls.len();
        println!(
            "  {}!: {n} calls, {} input tokens ({:.1} a call), {} output tokens: {:.2} us a call, {:.1} ns an input token, {:.1} ns an output token; floor {:.2} us a call ({:.2}x)",
            defs[idx].name,
            row.input,
            row.input as f64 / n as f64,
            row.output,
            ns / n as f64 / 1e3,
            ns / row.input.max(1) as f64,
            ns / row.output.max(1) as f64,
            floor / n as f64 / 1e3,
            ns / floor.max(1.0),
        );
        all_ns += ns;
        all_calls += n;
        all_in += row.input;
        all_floor += floor;
    }
    println!(
        "  all {} macros: {all_calls} calls: {:.2} us a call, {:.1} ns an input token; floor {:.2} us a call ({:.2}x)",
        chosen.len(),
        all_ns / all_calls.max(1) as f64 / 1e3,
        all_ns / all_in.max(1) as f64,
        all_floor / all_calls.max(1) as f64 / 1e3,
        all_ns / all_floor.max(1.0),
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
