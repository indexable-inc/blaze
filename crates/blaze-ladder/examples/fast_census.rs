//! THE FAST TIER'S CENSUS of one program, without placing or running any code: every body the
//! program's `main` reaches, built to LIR exactly as `blaze run`'s record walk builds it
//! (run.rs `build_records_new`, over code_records.rs `RecordRun::census`), then handed to the
//! Reference emitter (`blaze_emit::emit_record_measured`, what `make_entry` calls) and to the
//! Fast tier through the tier seam (`blaze_tier::Tier::emit` on `blaze_emit::Fast`, over the
//! committed template table crates/blaze-emit/table/arm64.bztab and blaze-tier's `ARM64`).
//!
//! `cargo +nightly-2026-10-27 run --release -p blaze-ladder --example fast_census -- <program folder> <package> <edition> <shelf> [--inline]`
//! (or `mixcheck.sh blaze-ladder run fast_census <the same arguments>`)
//!
//! - `<program folder>`: holds `src/main.rs` (every `src/*.rs` is read);
//! - `<package>`, `<edition>`: as its `Cargo.toml` writes them (`hello-world`, `blaze-2026.11`);
//! - `<shelf>`: the toolchain's shelf (`<HOME>/.blaze/v2/toolchain`), or `-` for none;
//! - `--inline`: also print every table again with blaze-lir's simplify stage
//!   (`blaze_lir::simplify::simplify`: the trivial inliner over the walk's other bodies, scalar
//!   replacement, propagation, dead code) run on each body before the Fast tier.
//!
//! The walk is `blaze run`'s: the dev profile's overflow checks (D101), the unwind strategy, the
//! pinned toolchain, this host's OS with LSE (blaze-cli run_parent.rs `isa`).
//!
//! What it prints, deterministically (sorted; no time, no address):
//!
//! 1. `TOTALS: reached R; LIR built B; reference emitted E1, refused F1; fast emitted E2, refused F2`
//!    (`R` counts the walk's instances, the thread-local getter and ender among them, which are
//!    made from no text and not shown to the census; `B` the bodies the census was handed);
//! 2. `FAST REFUSALS`: each reason exactly as the Fast tier's `TierError` names it (the refused
//!    LIR op, the shape, or the table's `no template for ..`), the bodies whose first refusal it
//!    is, and three example functions (the first by name);
//! 3. no second table: the Fast tier stops at a body's first refusal (`Fast::emit` returns its
//!    `Err`), so a body's later refusals are not known; the line `EVERY REFUSAL: not counted`
//!    says so;
//! 4. `CODE`: bytes of Fast code against the Reference code's instruction bytes (and its whole
//!    code, data after the instructions included) over the bodies both emitted, and the ratio
//!    (the Reference emitter always gets the walk's LIR, what `blaze run` emits today, so with
//!    `--inline` the ratio is the simplified Fast code against today's code);
//! 5. `LIR`: the LIR instructions of the bodies the Fast tier emitted (with `--inline`, also
//!    those bodies' instructions before the stage).
//!
//! Then `REFERENCE REFUSALS` (the cell emitter's own, by its error with every number written
//! `N`), and with `--inline` `SIMPLIFY REFUSALS` the same way: a body the stage refuses is its
//! own class and goes to no tier. A tier that panics on a body counts it under `panic: <message>`
//! (rule 20). A walk that stops (the strict record walk stops at the first instance it cannot
//! build, as `blaze run` does) prints `WALK STOPPED: <error>` after the tables of what it had
//! built, and the census exits 1. A failure of the census itself exits 2, named.

#![feature(allocator_api)]

use std::alloc::Global;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::panic::{AssertUnwindSafe, catch_unwind};

use blaze_code::record::Symbol;
use blaze_emit::table_gen::EncodedTable;
use blaze_emit::{Fast, Isa, Os, RoutinePlaces, emit_record_measured};
use blaze_ladder::code_records::{Builder, Callee, InstanceNames, LirSeen, MemoryStore, RecordRun, SeenInstance, Settings};
use blaze_ladder::refusal::SourceFile;
use blaze_ladder::run::{NewFront, RunError, build_records_new};
use blaze_lir::body::{Bodies, BodyId};
use blaze_lir::func::{RefId, RefKind};
use blaze_lir::{VerifiedFunc, View};
use blaze_own::Alloc;
use blaze_tier::{ARM64, Machine, Table, TableError, Tier, TierError};

/// The template table the tests open (blaze-emit fast_tests.rs, tier_tests.rs).
const TABLE: &[u8] = include_bytes!("../../blaze-emit/table/arm64.bztab");

/// Why the census itself stopped (exit 2).
enum CensusError {
    Usage,
    /// A source file or the folder did not read.
    Read { path: String, error: std::io::Error },
    /// The committed template table did not read.
    Table(TableError),
}

impl fmt::Display for CensusError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CensusError::Usage => f.write_str("usage: fast_census <program folder> <package> <edition> <shelf or -> [--inline]"),
            CensusError::Read { path, error } => write!(f, "{path}: {error}"),
            CensusError::Table(error) => write!(f, "the template table crates/blaze-emit/table/arm64.bztab does not read: {error}"),
        }
    }
}

/// A function reference of a body, owned.
enum Owned {
    Symbol(Symbol),
    C(Box<str>),
}

/// One body the walk built, kept for the census.
struct Body {
    function: Box<str>,
    symbol: Symbol,
    /// The walk's function with the walk's proof (D163): copied, not checked again.
    func: VerifiedFunc<Global>,
    funcs: Vec<Owned, Global>,
    statics: Vec<blaze_typeck::ItemKey, Global>,
    vtables: Vec<Symbol, Global>,
    tls: Vec<blaze_typeck::ItemKey, Global>,
}

impl Body {
    /// Its function references as [`InstanceNames`] borrows them.
    fn callees(&self) -> Vec<Callee<'_>, Global> {
        let mut out = Vec::with_capacity_in(self.funcs.len(), Global);
        out.extend(self.funcs.iter().map(|c| match c {
            Owned::Symbol(s) => Callee::Symbol(*s),
            Owned::C(name) => Callee::C(name),
        }));
        out
    }
}

/// The census's [`LirSeen`]: every body, in the walk's order.
struct Collect {
    bodies: Vec<Body, Global>,
}

impl LirSeen for Collect {
    fn seen(&mut self, instance: SeenInstance<'_>) {
        let SeenInstance { function, symbol, func, names, .. } = instance;
        let mut funcs = Vec::with_capacity_in(names.funcs.len(), Global);
        funcs.extend(names.funcs.iter().map(|c| match *c {
            Callee::Symbol(s) => Owned::Symbol(s),
            Callee::C(name) => Owned::C(Box::from(name)),
        }));
        let copy = |of: &[Symbol]| {
            let mut v = Vec::with_capacity_in(of.len(), Global);
            v.extend_from_slice(of);
            v
        };
        self.bodies.push(Body {
            function: Box::from(function),
            symbol,
            func: func.to_verified_in(Global),
            funcs,
            statics: {
                let mut v = Vec::with_capacity_in(names.statics.len(), Global);
                v.extend_from_slice(names.statics);
                v
            },
            vtables: copy(names.vtables),
            tls: {
                let mut v = Vec::with_capacity_in(names.tls.len(), Global);
                v.extend_from_slice(names.tls);
                v
            },
        });
    }
}

/// The walk's bodies by symbol: what the trivial inliner splices a direct call's callee from.
struct Index<'b> {
    bodies: &'b [Body],
    /// `(symbol, index in bodies)`, sorted.
    by_symbol: Vec<([u8; 32], usize), Global>,
}

impl<'b> Index<'b> {
    fn new(bodies: &'b [Body]) -> Index<'b> {
        let mut by_symbol = Vec::with_capacity_in(bodies.len(), Global);
        by_symbol.extend(bodies.iter().enumerate().map(|(i, b)| (b.symbol.0, i)));
        by_symbol.sort_unstable();
        Index { bodies, by_symbol }
    }

    fn find(&self, symbol: [u8; 32]) -> Option<&'b Body> {
        let at = self.by_symbol.binary_search_by(|row| row.0.cmp(&symbol)).ok()?;
        self.bodies.get(self.by_symbol[at].1)
    }
}

impl Bodies for Index<'_> {
    fn callee(&self, from: BodyId, reference: RefId) -> Option<(BodyId, View<'_>)> {
        let caller = self.find(from.0)?;
        let view = caller.func.view();
        let row = view.refs.get(reference.index())?;
        if RefKind::from_word(row.kind)? != RefKind::Func {
            return None;
        }
        let Owned::Symbol(symbol) = caller.funcs.get(row.place as usize)? else { return None };
        let callee = self.find(symbol.0)?;
        Some((BodyId(symbol.0), callee.func.view()))
    }
}

/// One class of a table: the bodies it is the first refusal of, and their functions by name.
#[derive(Default)]
struct Class {
    bodies: usize,
    examples: BTreeSet<Box<str>>,
}

#[derive(Default)]
struct Classes(BTreeMap<String, Class>);

impl Classes {
    fn add(&mut self, class: String, function: &str) {
        let c = self.0.entry(class).or_default();
        c.bodies += 1;
        c.examples.insert(Box::from(function));
    }

    fn total(&self) -> usize {
        self.0.values().map(|c| c.bodies).sum()
    }

    /// Most bodies first, then by name.
    fn print(&self, out: &mut String, title: &str) {
        use std::fmt::Write as _;
        let _ = writeln!(out, "{title}: {} bodies in {} classes", self.total(), self.0.len());
        let mut rows: Vec<(&String, &Class), Global> = Vec::with_capacity_in(self.0.len(), Global);
        rows.extend(self.0.iter());
        rows.sort_by(|a, b| b.1.bodies.cmp(&a.1.bodies).then(a.0.cmp(b.0)));
        let _ = writeln!(out, "| reason | bodies (first refusal) | examples |");
        let _ = writeln!(out, "|---|---|---|");
        for (class, c) in rows {
            let examples: Vec<&str> = c.examples.iter().take(3).map(|e| &**e).collect();
            let _ = writeln!(out, "| {class} | {} | {} |", c.bodies, examples.join("; "));
        }
        let _ = writeln!(out);
    }
}

/// `text` with every run of decimal digits written `N`, so a class holds no instance's number.
fn numbers_out(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut in_digits = false;
    for ch in text.chars() {
        if ch.is_ascii_digit() {
            if !in_digits {
                out.push('N');
            }
            in_digits = true;
        } else {
            out.push(ch);
            in_digits = false;
        }
    }
    out
}

/// The Fast tier's refusal as its class: the reason exactly as the error names it, without the
/// instruction number.
fn fast_class(error: &TierError) -> String {
    match *error {
        TierError::Refused { what, .. } => what.to_string(),
        TierError::Table { error, .. } => error.to_string(),
        TierError::NoName { of, .. } => {
            let shown = format!("{of:?}");
            format!("no name for a reference of kind {}", &shown[..shown.find(['(', ' ', '{']).unwrap_or(shown.len())])
        }
        TierError::TooLarge { what, .. } => format!("too large: {what}"),
        TierError::Machine { .. } | TierError::Record { .. } => numbers_out(&error.to_string()),
    }
}

/// A panic's message, as its class.
fn panic_class(payload: &(dyn std::any::Any + Send)) -> String {
    let said = payload.downcast_ref::<&str>().map(|s| s.to_string()).or_else(|| payload.downcast_ref::<String>().cloned());
    format!("panic: {}", numbers_out(said.as_deref().unwrap_or("(no message)")))
}

/// What the Reference emitter made of one body: its instruction bytes and whole code bytes.
type Reference = Result<(u64, u64), String>;

/// One pass of the census (the walk's LIR, or the LIR after the simplify stage).
#[derive(Default)]
struct Tally {
    fast_emitted: usize,
    fast_refused: Classes,
    simplify_refused: Classes,
    /// Over the bodies both tiers emitted: Fast's code bytes, Reference's instruction bytes and
    /// its whole code's bytes.
    both: usize,
    fast_bytes: u64,
    reference_insts: u64,
    reference_code: u64,
    /// LIR instructions of the bodies Fast emitted: as Fast got them, and as the walk built them.
    lir: u64,
    lir_walk: u64,
}

/// `a / b` to three places, in integers.
fn ratio(a: u64, b: u64) -> String {
    match b {
        0 => "-".to_string(),
        _ => {
            let milli = a * 1000 / b;
            format!("{}.{:03}", milli / 1000, milli % 1000)
        }
    }
}

fn tally(bodies: &[Body], references: &[Reference], inline: bool, fast: &Fast<'_>, machine: Machine, alloc: Alloc<'_>) -> Tally {
    let index = Index::new(bodies);
    let mut t = Tally::default();
    for (body, reference) in bodies.iter().zip(references) {
        let callees = body.callees();
        let names = InstanceNames { funcs: &callees, statics: &body.statics, c_statics: &[], vtables: &body.vtables, tls: &body.tls };
        let simplified = if inline {
            let stage = catch_unwind(AssertUnwindSafe(|| {
                blaze_lir::simplify::simplify(body.func.view(), BodyId(body.symbol.0), &index, blaze_lir::inline::TRIVIAL, Global)
            }));
            match stage {
                Ok(Ok((func, _read))) => Some(func),
                Ok(Err(error)) => {
                    t.simplify_refused.add(numbers_out(&error.to_string()), &body.function);
                    continue;
                }
                Err(payload) => {
                    t.simplify_refused.add(panic_class(&*payload), &body.function);
                    continue;
                }
            }
        } else {
            None
        };
        // The simplified function is new LIR: its one verification is here, and a refusal is
        // the simplify stage's class. The walk's own function carries the walk's proof.
        let verified = match simplified.as_ref() {
            Some(func) => match blaze_lir::verify(func.view()) {
                Ok(verified) => verified,
                Err(error) => {
                    t.simplify_refused.add(numbers_out(&error.to_string()), &body.function);
                    continue;
                }
            },
            None => body.func.verified(),
        };
        let made = catch_unwind(AssertUnwindSafe(|| fast.emit(verified, &names, machine, alloc).map(|e| e.code.len() as u64)));
        let bytes = match made {
            Ok(Ok(bytes)) => bytes,
            Ok(Err(error)) => {
                t.fast_refused.add(fast_class(&error), &body.function);
                continue;
            }
            Err(payload) => {
                t.fast_refused.add(panic_class(&*payload), &body.function);
                continue;
            }
        };
        t.fast_emitted += 1;
        t.lir += simplified.as_ref().map_or_else(|| body.func.view().len(), |func| func.view().len()) as u64;
        t.lir_walk += body.func.view().len() as u64;
        if let Ok((insts, code)) = reference {
            t.both += 1;
            t.fast_bytes += bytes;
            t.reference_insts += insts;
            t.reference_code += code;
        }
    }
    t
}

fn print_tally(out: &mut String, t: &Tally, heading: &str, reached: usize, built: usize, references: &Classes, inline: bool) {
    use std::fmt::Write as _;
    let _ = writeln!(out, "## {heading}\n");
    let reference_emitted = built - references.total();
    let _ = writeln!(
        out,
        "TOTALS: reached {reached}; LIR built {built}; reference emitted {reference_emitted}, refused {}; fast emitted {}, refused {}{}\n",
        references.total(),
        t.fast_emitted,
        t.fast_refused.total(),
        if inline { format!("; simplify refused {}", t.simplify_refused.total()) } else { String::new() },
    );
    t.fast_refused.print(out, "FAST REFUSALS");
    let _ = writeln!(out, "EVERY REFUSAL: not counted: the Fast tier stops at a body's first refusal (`Fast::emit` returns its `Err`)\n");
    let _ = writeln!(
        out,
        "CODE: over {} bodies both tiers emitted: fast {} bytes; reference {} instruction bytes ({} bytes with its data); fast/reference {} ({} against the whole code)\n",
        t.both,
        t.fast_bytes,
        t.reference_insts,
        t.reference_code,
        ratio(t.fast_bytes, t.reference_insts),
        ratio(t.fast_bytes, t.reference_code),
    );
    match inline {
        false => {
            let _ = writeln!(out, "LIR: {} instructions in the {} bodies the Fast tier emitted\n", t.lir, t.fast_emitted);
        }
        true => {
            let _ = writeln!(
                out,
                "LIR: {} instructions in the {} bodies the Fast tier emitted, after the simplify stage ({} as the walk built them; after/before {})\n",
                t.lir,
                t.fast_emitted,
                t.lir_walk,
                ratio(t.lir, t.lir_walk),
            );
            t.simplify_refused.print(out, "SIMPLIFY REFUSALS");
        }
    }
}

fn census(heap: &'static blaze_own::Heap) -> Result<i32, CensusError> {
    let args: Vec<String> = std::env::args().collect();
    let (folder, package, edition, shelf, inline) = match args.as_slice() {
        [_, folder, package, edition, shelf] => (folder, package, edition, shelf, false),
        [_, folder, package, edition, shelf, flag] if flag == "--inline" => (folder, package, edition, shelf, true),
        _ => return Err(CensusError::Usage),
    };
    let alloc = heap.alloc();
    let read = |path: &std::path::Path, error| CensusError::Read { path: path.display().to_string(), error };
    let src = std::path::Path::new(folder).join("src");
    let mut sources: Vec<(String, String), Global> = Vec::new_in(Global);
    for entry in std::fs::read_dir(&src).map_err(|e| read(src.as_path(), e))? {
        let path = entry.map_err(|e| read(src.as_path(), e))?.path();
        if path.extension().is_some_and(|x| x == "rs") {
            let Some(name) = path.file_name().map(|n| n.to_string_lossy().into_owned()) else { continue };
            let text = std::fs::read_to_string(&path).map_err(|e| read(path.as_path(), e))?;
            sources.push((format!("src/{name}"), text));
        }
    }
    sources.sort();
    let files: Vec<SourceFile<'_>> = sources.iter().map(|(path, text)| SourceFile { path, text }).collect();
    let shelf = (shelf != "-").then_some(shelf.as_str());
    // `blaze run`'s walk (blaze-cli run_parent.rs `lay_out`, `isa`): the dev profile's overflow
    // checks, the unwind strategy, the pinned toolchain, this host's OS with LSE.
    let overflow = blaze_build::OverflowChecks::On;
    let new = NewFront { package, edition, shelf, overflow };
    let os = if cfg!(target_os = "macos") { Os::MacOs } else { Os::Linux };
    let isa = Isa { lse: true, os };
    let builder = Builder {
        isa,
        tier: blaze_ladder::code_records::TierName::Reference,
        settings: Settings::baseline(overflow, blaze_build::Panic::Unwind),
        toolchain: blaze_toolchain::PINNED,
        producer: blaze_ids::producer::Producers::for_tests(b"fast census").get::<blaze_ids::producer::CodeBuilder>(),
        edition: edition::PackageEdition::of_manifest(edition).expect("the package's edition is one blaze knows"),
    };
    let mut store = MemoryStore::new_in(alloc);
    let mut collect = Collect { bodies: Vec::new_in(Global) };
    let (walked, reached) = {
        let mut run = RecordRun::census(&mut store, builder, &mut collect).expect("the builder names the emitting tier");
        let walked: Result<(), RunError> = build_records_new(&files, "src/main.rs", new, &mut run, alloc);
        (walked, run.met.len())
    };
    let bodies = collect.bodies;
    // From here a tier's panic is one body's class (rule 20), not a message per body.
    std::panic::set_hook(Box::new(|_| {}));
    let table = EncodedTable::read(TABLE, alloc).map_err(CensusError::Table)?;
    let machine = table.head().machine;
    let fast = Fast { table: &table, regs: &ARM64 };
    // The Reference emitter on the walk's LIR, as `make_entry` calls it (no soft-float routine
    // bound, as the eager walk binds none): the same in both passes.
    let mut reference_refused = Classes::default();
    let mut references: Vec<Reference, Global> = Vec::with_capacity_in(bodies.len(), Global);
    for body in bodies.iter() {
        let callees = body.callees();
        let names = InstanceNames { funcs: &callees, statics: &body.statics, c_statics: &[], vtables: &body.vtables, tls: &body.tls };
        let made = catch_unwind(AssertUnwindSafe(|| {
            emit_record_measured(body.func.verified(), &names, &RoutinePlaces::NONE, isa, &[], alloc).map(|(record, insts, _called)| (u64::from(insts), record.code.len() as u64))
        }));
        let reference = match made {
            Ok(Ok(sizes)) => Ok(sizes),
            Ok(Err(error)) => Err(numbers_out(&error.to_string())),
            Err(payload) => Err(panic_class(&*payload)),
        };
        if let Err(class) = &reference {
            reference_refused.add(class.clone(), &body.function);
        }
        references.push(reference);
    }
    let mut out = String::new();
    {
        use std::fmt::Write as _;
        let _ = writeln!(out, "# Fast tier census: {package} ({edition}), table machine {:?} on {:?}, features {:#x}\n", machine.arch, machine.os, machine.features);
    }
    let plain = tally(&bodies, &references, false, &fast, machine, alloc);
    print_tally(&mut out, &plain, "The walk's LIR", reached, bodies.len(), &reference_refused, false);
    if inline {
        let simplified = tally(&bodies, &references, true, &fast, machine, alloc);
        print_tally(&mut out, &simplified, "After the simplify stage (--inline)", reached, bodies.len(), &reference_refused, true);
    }
    reference_refused.print(&mut out, "REFERENCE REFUSALS");
    let code = match walked {
        Ok(()) => 0,
        Err(error) => {
            use std::fmt::Write as _;
            let _ = writeln!(out, "WALK STOPPED: {error}");
            1
        }
    };
    print!("{out}");
    Ok(code)
}

fn main() {
    // The program's top: the process's heap, handed down.
    let heap = blaze_own::Heap::process();
    match census(heap) {
        Ok(code) => std::process::exit(code),
        Err(error) => {
            eprintln!("fast_census: {error}");
            std::process::exit(2);
        }
    }
}
