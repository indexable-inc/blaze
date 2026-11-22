//! Lowers every `.rs` file of library/core, library/alloc and library/std on N threads and
//! prints what was lowered, what was not (unhandled forms by kind, with counts and three
//! examples each), the time per line and per body, the record bytes, and the floor aimed at.
//!
//! `cargo run --release -p blaze-lower --example lower_std -- <rust-src>/library [threads]`
//!
//! The floor: one parse of each item's text, the work no lowering from text can skip. The
//! lowering parses a fn twice (its signature as an outline, its body in full) and walks the
//! events once each; its aim is within 2x of one full parse a line. A measurement tool, not a
//! test.

#![feature(allocator_api)]

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use blaze_expand::{
    DefAt, DefSite, ExpandCx, Export, HostAsk, HostError, HostWhy, IncludeAs, Included, LocalInner, MacroDefinition,
    NestedKind, Place, PlaceOf, Prepared, ProcCall, Produced, TokenStream, find_calls, lex, prepare,
};
use blaze_id::CrateItemKey;
use blaze_ids::ContentId;
use blaze_lower::by_name::ByName;
use blaze_lower::{
    Cfg, CfgAnswer, Edition, Expand, Expansion, FileStats, ItemRef, Outcome, Sink, What, lower_file,
};
use blaze_own::Alloc;

/// A cfg set like a 64-bit Unix host's, answering every atom (no `Need`).
struct Host;

impl Cfg for Host {
    fn atom(&mut self, name: &str, value: Option<&str>) -> CfgAnswer {
        let yes = match (name, value) {
            ("unix" | "debug_assertions" | "target_thread_local", None) => true,
            ("target_os", Some(os)) => os == "linux",
            ("target_family", Some(f)) => f == "unix",
            ("target_arch", Some(a)) => a == "aarch64",
            ("target_pointer_width", Some(w)) => w == "64",
            ("target_endian", Some(e)) => e == "little",
            ("target_env", Some(e)) => e == "gnu",
            ("panic", Some(p)) => p == "unwind",
            ("target_has_atomic" | "target_has_atomic_load_store" | "target_has_atomic_equal_alignment", _) => true,
            _ => false,
        };
        if yes { CfgAnswer::Yes } else { CfgAnswer::No }
    }
}

/// No expander (`--no-expand`): every macro call is counted, not expanded.
struct NoExpand;

impl Expand for NoExpand {
    fn expand(
        &mut self,
        _: &str,
        _: Option<blaze_lower::mark_site::TextMark>,
        _: &str,
        _: blaze_lower::InputMarks<'_>,
        _: Edition,
        _: &mut dyn Cfg,
        _: &mut blaze_lower::BodyOut<'_>,
    ) -> Expansion {
        Expansion::Unsupported
    }
    fn expand_body(&mut self, _: blaze_lower::BodyCall<'_>, _: &mut dyn Cfg, _: &mut blaze_lower::BodyOut<'_>) -> Expansion {
        Expansion::Unsupported
    }
    fn derive(
        &mut self,
        _: &str,
        _: Option<blaze_lower::mark_site::TextMark>,
        _: &blaze_expand::TokenStream<'_>,
        _: blaze_expand::DeriveFacts,
        _: Edition,
        _: &mut blaze_lower::BodyOut<'_>,
    ) -> Expansion {
        Expansion::Unsupported
    }
    fn attr(
        &mut self,
        _: &str,
        _: Option<blaze_lower::mark_site::TextMark>,
        _: Option<&blaze_expand::TokenStream<'_>>,
        _: &blaze_expand::TokenStream<'_>,
        _: Edition,
        _: &mut blaze_lower::BodyOut<'_>,
    ) -> Expansion {
        Expansion::Unsupported
    }
}

/// The census's host for expansions: an empty environment, no files to include, and no
/// answer for a position (each such call is counted as failed, never guessed); `cfg!` answered
/// under [`Host`]'s cfg set.
struct Census;

impl ExpandCx for Census {
    fn env(&self, _: &str) -> Result<Option<&str>, HostError> {
        Ok(None)
    }
    fn cfg(&self, predicate: &TokenStream<'_>) -> Result<bool, HostError> {
        blaze_lower::cfg_outcome(predicate, &mut Host, Alloc::global())
    }
    fn include(&self, _: &str, _: IncludeAs) -> Result<Included<'_>, HostError> {
        Ok(Included::Missing)
    }
    fn place(&self, _: PlaceOf) -> Result<Place<'_>, HostError> {
        Err(HostError { ask: HostAsk::Place, why: HostWhy::Refused })
    }
    fn run_proc_macro<'a>(
        &self,
        _: &MacroDefinition<'_>,
        _: ProcCall<'_>,
        _: Alloc<'a>,
    ) -> Result<Produced<'a>, HostError> {
        Err(HostError { ask: HostAsk::ProcMacro, why: HostWhy::Refused })
    }
}

fn def_at() -> DefAt {
    DefAt {
        site: DefSite { krate: CrateItemKey(ContentId([1; 32])), module: None },
        edition: Edition::E2024,
        export: Export::Exported,
        local_inner: LocalInner::No,
        transparency: None,
    }
}

/// Every `macro_rules!` definition in `texts`, by name, compiled once (leaked: they live as
/// long as the run). A definition `prepare` refuses is counted and left out.
fn definitions(texts: &[String]) -> (&'static [(&'static str, Prepared<'static>)], usize) {
    let mut defs = Vec::new();
    let mut refused = 0;
    for text in texts {
        let Ok(stream) = lex(text, Edition::E2024, Alloc::global()) else { continue };
        let stream: &'static TokenStream<'static> = Box::leak(Box::new(stream));
        let Ok(found) = find_calls(stream, Alloc::global()) else { continue };
        for call in found.iter() {
            let Some(name_at) = call.name else { continue };
            let inner = call.inner();
            let Ok(body) = stream.copy_in(inner.start as usize..inner.end as usize, Alloc::global()) else { continue };
            let def = match call.kind {
                NestedKind::MacroRules => MacroDefinition::rules(body, def_at()),
                NestedKind::Macro2 => {
                    let params = match call.params {
                        None => None,
                        Some(p) => match stream.copy_in(p.start as usize + 1..p.end as usize - 1, Alloc::global()) {
                            Ok(params) => Some(params),
                            Err(_) => continue,
                        },
                    };
                    MacroDefinition::macro2(params, body, def_at())
                }
                _ => continue,
            };
            let def: &'static MacroDefinition<'static> = Box::leak(Box::new(def));
            match prepare(def) {
                Ok(prepared) => defs.push((stream.text_at(name_at as usize), prepared)),
                Err(_) => refused += 1,
            }
        }
    }
    (Box::leak(defs.into_boxed_slice()), refused)
}

#[derive(Default)]
struct Unhandled {
    count: u64,
    examples: Vec<String>,
}

#[derive(Default)]
struct Tally {
    unhandled: BTreeMap<String, Unhandled>,
    needs: u64,
}

impl Sink for Tally {
    fn refused(&mut self, item: &ItemRef<'_>, refusal: blaze_lower::Refusal<'_>) {
        let entry = self.unhandled.entry(format!("macro call refused ({:?})", refusal.bound)).or_default();
        entry.count += 1;
        if entry.examples.len() < 3 {
            entry.examples.push(format!("{} on {}", refusal.path, item.item_text().chars().take(60).collect::<String>().replace('\n', " ")));
        }
    }

    fn record(&mut self, _item: &ItemRef<'_>, _what: What, _key: ContentId, outcome: Outcome<'_, '_>) {
        let Outcome::Record { view, .. } = outcome else {
            self.needs += 1;
            return;
        };
        for u in view.unhandled() {
            let kind = format!("{:?} ({:?})", blaze_file_items::syntax_kind(u.syntax), u.why);
            let entry = self.unhandled.entry(kind).or_default();
            entry.count += 1;
            if entry.examples.len() < 3 {
                let snippet: String =
                    view.text_at(u.range).unwrap_or("<no text at this range>").chars().take(80).collect();
                entry.examples.push(snippet.replace('\n', " "));
            }
        }
    }
}

fn add(into: &mut FileStats, s: &FileStats) {
    into.items += s.items;
    into.signatures += s.signatures;
    into.bodies += s.bodies;
    into.anon_consts += s.anon_consts;
    into.needs += s.needs;
    into.cfg_out += s.cfg_out;
    into.not_lowered += s.not_lowered;
    into.errors += s.errors;
    into.macro_calls_expanded += s.macro_calls_expanded;
    into.macro_calls_unsupported += s.macro_calls_unsupported;
    into.macro_calls_failed += s.macro_calls_failed;
    into.macro_calls_too_deep += s.macro_calls_too_deep;
    into.body_items += s.body_items;
    into.body_calls_expanded += s.body_calls_expanded;
    into.unhandled += s.unhandled;
    into.record_bytes += s.record_bytes;
}

fn main() {
    let mut args = std::env::args_os().skip(1).peekable();
    let expanding = if args.peek().is_some_and(|a| a == "--no-expand") {
        args.next();
        false
    } else {
        true
    };
    let Some(library) = args.next().map(PathBuf::from) else {
        eprintln!("usage: lower_std [--no-expand] <rust-src>/library|<crate dir> [threads]");
        std::process::exit(2);
    };
    let threads: usize = match args.next().map(|a| a.to_string_lossy().parse()) {
        None => std::thread::available_parallelism().map_or(1, |n| n.get()),
        Some(Ok(n)) if n > 0 => n,
        Some(_) => {
            eprintln!("threads: a number above 0");
            std::process::exit(2);
        }
    };
    let mut files: Vec<PathBuf> = Vec::new();
    // A library's core, alloc and std, or one crate's `src`.
    let mut stack: Vec<PathBuf> = if library.join("core").join("src").is_dir() {
        ["core", "alloc", "std"].iter().map(|c| library.join(c).join("src")).collect()
    } else {
        vec![library.join("src")]
    };
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            eprintln!("not a directory that reads: {}", dir.display());
            std::process::exit(2);
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|e| e == "rs") {
                files.push(path);
            }
        }
    }
    files.sort();
    let texts: Vec<String> = files
        .iter()
        .map(|f| {
            std::fs::read_to_string(f).unwrap_or_else(|e| {
                eprintln!("{}: {e}", f.display());
                std::process::exit(2);
            })
        })
        .collect();
    let bytes: usize = texts.iter().map(String::len).sum();
    let lines: usize = texts.iter().map(|t| t.lines().count()).sum();

    // The floor: one full parse of every file, on the same threads.
    let floor = run(&texts, threads, |text| {
        let pe = parser::Edition::Edition2024;
        let lexed = parser::LexedStr::new(pe, text);
        let input = lexed.to_input(pe);
        let output = parser::TopEntryPoint::SourceFile.parse(input.tokens());
        std::hint::black_box(output.steps().count());
    });

    // The definitions the expander finds by name: the texts' own `macro_rules!`, compiled
    // once a thread (a stream's allocator handle is not shared across threads). The first
    // file of each thread pays for it inside the lowering's time.
    thread_local! {
        static RULES: std::cell::OnceCell<(&'static [(&'static str, Prepared<'static>)], usize)> =
            const { std::cell::OnceCell::new() };
    }
    let rules_of = || RULES.with(|r| *r.get_or_init(|| if expanding { definitions(&texts) } else { (&[][..], 0) }));
    let (rule_count, refused_rules) = {
        let (rules, refused) = rules_of();
        (rules.len(), refused)
    };

    let total = Mutex::new(FileStats::default());
    let tally = Mutex::new(Tally::default());
    let took = run(&texts, threads, |text| {
        let mut mine = Tally::default();
        let mut by_name = ByName::new(rules_of().0, def_at(), Census, Alloc::global());
        let expand: &mut dyn Expand = if expanding { &mut by_name } else { &mut NoExpand };
        let stats = lower_file(text, Edition::E2024, &mut Host, &mut blaze_lower::NoModule, expand, &mut mine, Alloc::global());
        add(&mut total.lock().expect("no thread panicked holding the stats"), &stats);
        let mut all = tally.lock().expect("no thread panicked holding the tally");
        all.needs += mine.needs;
        for (kind, u) in mine.unhandled {
            let entry = all.unhandled.entry(kind).or_default();
            entry.count += u.count;
            for e in u.examples {
                if entry.examples.len() < 3 {
                    entry.examples.push(e);
                }
            }
        }
    });

    let s = total.into_inner().expect("no thread panicked holding the stats");
    let t = tally.into_inner().expect("no thread panicked holding the tally");
    println!("{} files, {lines} lines, {:.1} MB, {threads} threads", files.len(), bytes as f64 / 1e6);
    println!(
        "items {} | signatures {} | bodies {} | anon consts {} | body items {} | cfg-out {} | needs {}",
        s.items, s.signatures, s.bodies, s.anon_consts, s.body_items, s.cfg_out, s.needs
    );
    println!(
        "not lowered: use/extern crate/macro_rules/macro {} (by design) | outline errors {} | item macro calls: {} expanded, {} no expander, {} failed, {} too deep",
        s.not_lowered, s.errors, s.macro_calls_expanded, s.macro_calls_unsupported, s.macro_calls_failed, s.macro_calls_too_deep
    );
    match expanding {
        true => println!(
            "expander: by name over {} `macro_rules!` and `macro` ({refused_rules} refused by `prepare`) and the builtins | body calls expanded {}",
            rule_count,
            s.body_calls_expanded
        ),
        false => println!("expander: none (--no-expand)"),
    }
    println!("unhandled forms: {} in all", s.unhandled);
    let mut by_count: Vec<_> = t.unhandled.iter().collect();
    by_count.sort_by(|a, b| b.1.count.cmp(&a.1.count).then(a.0.cmp(b.0)));
    for (kind, u) in by_count {
        println!("  {:>8}  {kind}", u.count);
        for e in &u.examples {
            println!("            {e}");
        }
    }
    let cpu = took * threads as f64;
    println!(
        "lowering: {:.1} ms wall, {:.0} ns a line, {:.0} ns a body (cpu, {threads} threads)",
        took * 1e3,
        cpu * 1e9 / lines as f64,
        cpu * 1e9 / s.bodies.max(1) as f64,
    );
    println!("record bytes: {:.1} MB ({:.1} bytes a line)", s.record_bytes as f64 / 1e6, s.record_bytes as f64 / lines as f64);
    println!(
        "floor (one full parse a line): {:.0} ns a line; aim 2x of it; lowering at {:.2}x",
        floor * threads as f64 * 1e9 / lines as f64,
        took / floor,
    );
}

/// Runs `work` over every text on `threads` threads, each taking the next from one counter:
/// the wall time in seconds.
fn run(texts: &[String], threads: usize, work: impl Fn(&str) + Sync) -> f64 {
    let next = AtomicUsize::new(0);
    let start = Instant::now();
    std::thread::scope(|scope| {
        for _ in 0..threads {
            scope.spawn(|| {
                loop {
                    let at = next.fetch_add(1, Ordering::Relaxed);
                    let Some(text) = texts.get(at) else { break };
                    work(text);
                }
            });
        }
    });
    start.elapsed().as_secs_f64()
}
