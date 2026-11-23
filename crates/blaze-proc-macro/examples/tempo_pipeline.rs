//! Every proc-macro call of a checkout, found by the real expansion pipeline (blaze-lower's
//! `lower_file` driver: item-position derives, attributes and calls, and body calls) and run
//! through the proc-macro hook (`hook::ProcMacros` behind `ByName::with_names` over `Lists`): found,
//! expanded, failed by cause, per macro; then the same pass again, answered from the records.
//!
//! `tempo_pipeline <checkout> <module store> <unit list> <out>`: the unit list is
//! blaze-unit-graph's listing (its `proc-macro u.. name version [features] ..` lines name every
//! macro crate of the graph, built or not); records and compiled modules go under `<out>`. The
//! checkout is read, never built or written.
//!
//! A call's path is resolved by a stand-in until blaze-scope's `Names::macro_path` resolves
//! macro paths: first its first segment is read through the file's `use` items (`uses.rs`:
//! under `use tokio::select;`, `select!` is `tokio::select!`), then among the macro crates the
//! lock reaches from the file's package (for `a::m`, from its dependency `a`), the exports of
//! that name and kind, the nearest in the lock's dependency graph winning; two at the same
//! distance are ambiguous and counted, never guessed. A call read through a `use` that names no
//! proc macro there is counted apart and never matched by name to another crate's. A
//! single-segment attribute the language defines (`test`, `inline`, ..) is the language's.

#![feature(allocator_api)]

mod common;
#[path = "tempo_pipeline/uses.rs"]
mod uses;
#[cfg(test)]
#[path = "tempo_pipeline/resolve_tests.rs"]
mod resolve_tests;

use std::{
    collections::{BTreeMap, HashMap, VecDeque},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Instant,
};

use blaze_expand::{
    DefAt, DefSite, Export, ExpandCx, HostAsk, HostError, HostWhy, IncludeAs, Included, LocalInner, MacroDefinition, Place, PlaceOf,
    ProcCall, ProcDiagnostic, ProcKind, ProcLevel, Produced, TokenStream,
};
use blaze_id::CrateItemKey;
use blaze_lower::{
    BodyCall, BodyOut, Cfg, CfgAnswer, Expand, Expansion, ExprId, FileStats, ItemRef, Outcome, Sink, TypeId, View, What, by_name::ByName, by_name::Lists, by_name::MacroNames,
    by_name::Resolved, lower_file,
    typeck::{Refused, Types, to_typeck},
};
use blaze_typeck::{ItemKey, PNode, PTy, PortableTys};
use blaze_own::Alloc;
use blaze_proc_macro::{
    hook::{Finding, ProcCx, ProcMacros, Reached, crate_key},
    host::{Limits, Runtime},
    module::{Source, files_digest, package_files},
    sandbox::Package,
};
use blaze_surface::Edition;

use common::{expansion_records, modules, package_dir, package_of, sources};
use uses::{Bound, Uses};

/// A git macro crate's commit and folder in its repository, from the identity the bootstrap
/// builder wrote beside its module (`git <commit> <path>`).
fn git_of(store: &Path, name: &str, version: &str) -> Option<(String, String)> {
    let dirs = std::fs::read_dir(store.join("proc-macro/modules")).ok()?;
    dirs.flatten().find_map(|dir| {
        let identity = std::fs::read_to_string(dir.path().join("identity")).ok()?;
        let field = |key: &str| identity.lines().find_map(|line| line.strip_prefix(key).and_then(|it| it.strip_prefix(' ')));
        if field("name")? != name || field("version")? != version {
            return None;
        }
        let (commit, path) = field("git")?.split_once(' ')?;
        Some((commit.to_owned(), path.to_owned()))
    })
}

/// Attributes the language defines: a single-segment one is never a proc macro here.
const BUILTIN_ATTRS: &[&str] = &[
    "test", "bench", "inline", "allow", "warn", "deny", "forbid", "expect", "cfg", "cfg_attr", "doc", "must_use", "deprecated", "repr",
    "non_exhaustive", "path", "macro_export", "macro_use", "automatically_derived", "cold", "track_caller", "no_mangle", "export_name",
    "link", "link_name", "ignore", "should_panic", "derive", "global_allocator", "target_feature", "unsafe", "used", "naked", "diagnostic",
    "recursion_limit", "no_std", "no_implicit_prelude", "crate_type", "feature", "coverage", "instruction_set", "debugger_visualizer",
    "collapse_debuginfo", "may_dangle", "rustfmt", "clippy", "proc_macro", "proc_macro_derive", "proc_macro_attribute", "derive_const",
];

/// One package of the lock, its dependencies by index.
struct Locked {
    name: String,
    version: String,
    path: bool,
    checksum: Option<String>,
    deps: Vec<usize>,
}

fn read_lock(text: &str) -> Vec<Locked> {
    let mut out = Vec::new();
    let mut names: Vec<Vec<String>> = Vec::new();
    for entry in text.split("[[package]]").skip(1) {
        let field = |key: &str| entry.lines().find_map(|line| line.trim().strip_prefix(key)).map(|it| it.trim().trim_matches('"').to_owned());
        let mut deps = Vec::new();
        if let Some(at) = entry.find("dependencies = [") {
            let list = &entry[at + "dependencies = [".len()..];
            let list = &list[..list.find(']').unwrap_or(list.len())];
            deps.extend(list.split(',').map(|it| it.trim().trim_matches('"').to_owned()).filter(|it| !it.is_empty()));
        }
        out.push(Locked {
            name: field("name = ").unwrap_or_default(),
            version: field("version = ").unwrap_or_default(),
            path: field("source = ").is_none(),
            checksum: field("checksum = "),
            deps: Vec::new(),
        });
        names.push(deps);
    }
    for (at, deps) in names.iter().enumerate() {
        for dep in deps {
            let mut parts = dep.splitn(3, ' ');
            let (name, version) = (parts.next().unwrap_or(""), parts.next());
            let found: Vec<usize> = (0..out.len()).filter(|i| out[*i].name == name && version.is_none_or(|v| out[*i].version == v)).collect();
            assert!(!found.is_empty(), "{} names {dep}, which the lock does not hold", out[at].name);
            out[at].deps.push(found[0]);
        }
    }
    out
}

/// Each package's distance from `from` in the lock's dependency graph (`u32::MAX`: not reached).
fn distances(lock: &[Locked], from: usize) -> Vec<u32> {
    let mut dist = vec![u32::MAX; lock.len()];
    dist[from] = 0;
    let mut queue = VecDeque::from([from]);
    while let Some(at) = queue.pop_front() {
        for dep in &lock[at].deps {
            if dist[*dep] == u32::MAX {
                dist[*dep] = dist[at] + 1;
                queue.push_back(*dep);
            }
        }
    }
    dist
}

/// One macro a registered crate exports.
struct Macro {
    package: usize,
    krate: CrateItemKey,
    kind: ProcKind,
    name: String,
    helpers: Vec<String>,
}

/// The resolver's shared half: the lock, every exported macro, distances, and what it could not
/// resolve.
struct Names {
    lock: Vec<Locked>,
    macros: Vec<Macro>,
    dist: Mutex<HashMap<usize, Arc<Vec<u32>>>>,
    unresolved: Mutex<BTreeMap<String, u64>>,
    ambiguous: Mutex<BTreeMap<String, u64>>,
    /// Calls read through a `use` of their file that name no proc macro there, by the path as
    /// written, as read, and why.
    through_use: Mutex<BTreeMap<String, u64>>,
}

impl Names {
    fn dist(&self, from: usize) -> Arc<Vec<u32>> {
        let mut held = self.dist.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        Arc::clone(held.entry(from).or_insert_with(|| Arc::new(distances(&self.lock, from))))
    }
}

/// The resolver for one file: [`Names`] from the file's package, through the file's `use`s.
struct Resolve<'n> {
    names: &'n Names,
    member: usize,
    uses: &'n Uses,
}

impl<'d> MacroNames<'d> for Resolve<'_> {
    fn resolve<'a>(
        &self,
        path: &str,
        _: blaze_lower::by_name::PathAt,
        kind: ProcKind,
        alloc: Alloc<'a>,
    ) -> Result<Resolved<'d, 'a>, blaze_lower::by_name::NoMacro<'a>>
    where
        'd: 'a,
    {
        self.find(path, kind, alloc).ok_or(blaze_lower::by_name::NoMacro::NotListed)
    }
}

impl Resolve<'_> {
    /// The proc macro `path` names among the lock's packages by distance, `None` when none or
    /// more than one nearest. Its first segment is read through the file's `use` items first.
    fn find<'d, 'a>(&self, path: &str, kind: ProcKind, alloc: Alloc<'a>) -> Option<Resolved<'d, 'a>> {
        let written: Vec<&str> = path.split([':', ' ']).filter(|it| !it.is_empty()).collect();
        let name = *written.last()?;
        if written.len() == 1 && kind == ProcKind::Attr && BUILTIN_ATTRS.contains(&name) {
            return None;
        }
        if written.len() > 1 && matches!(written[0], "rustfmt" | "clippy" | "diagnostic" | "rustdoc") {
            return None;
        }
        // The Reference, "Use declarations" and "Paths": a path's first segment is the name its
        // scope binds; a `use` of the file binding it decides which crate the call is of
        // (`use tokio::select;`: `select!` is tokio's `macro_rules!`, never commonware's proc
        // macro of the same name).
        let through: Option<Vec<&str>> = match self.uses.get(written[0]) {
            Some(Bound::Path(target)) if !(written.len() == 1 && target.len() == 1) => {
                Some(target.iter().map(String::as_str).chain(written[1..].iter().copied()).collect())
            }
            Some(Bound::Path(_)) | None => None,
            Some(Bound::Twice) => {
                self.count_through(kind, &written, &written, "the file's `use`s bind it to two paths");
                return None;
            }
        };
        let segs: &[&str] = through.as_deref().unwrap_or(&written);
        // The name as read (`use commonware_macros::select as cs;`: `cs!` is `select`).
        let name = *segs.last()?;
        if through.is_some() && matches!(segs[0], "crate" | "self" | "super") {
            self.count_through(kind, &written, segs, "a `use` of the file's own crate: not followed by the stand-in");
            return None;
        }
        let from_member = self.names.dist(self.member);
        let start = match segs.first() {
            Some(first) if segs.len() > 1 && !matches!(*first, "crate" | "self" | "super" | "$crate") => {
                let reached = (0..self.names.lock.len())
                    .filter(|i| self.names.lock[*i].name.replace('-', "_") == *first && from_member[*i] != u32::MAX)
                    .min_by_key(|i| from_member[*i]);
                // A path whose first segment no package the lock reaches is named (`core::..`,
                // `std::..`, a module of the crate) names no dependency's proc macro here.
                match reached {
                    Some(at) => at,
                    None => {
                        if through.is_some() {
                            self.count_through(kind, &written, segs, "a `use` of a crate the lock does not reach");
                        }
                        return None;
                    }
                }
            }
            _ => self.member,
        };
        let dist = self.names.dist(start);
        let label = format!("{kind:?} {}", segs.join("::"));
        let mut best: Vec<&Macro> = Vec::new();
        for m in self.names.macros.iter().filter(|m| m.kind == kind && m.name == name && dist[m.package] != u32::MAX) {
            match best.first().map(|b| dist[b.package]) {
                Some(d) if dist[m.package] > d => {}
                Some(d) if dist[m.package] == d => best.push(m),
                _ => best = vec![m],
            }
        }
        let count = move |map: &Mutex<BTreeMap<String, u64>>| *map.lock().unwrap_or_else(std::sync::PoisonError::into_inner).entry(label).or_default() += 1;
        match best[..] {
            [] if through.is_some() => {
                self.count_through(kind, &written, segs, "the crate the `use` names reaches no proc macro of that name");
                None
            }
            [] => {
                count(&self.names.unresolved);
                None
            }
            [m] => {
                let at = DefAt {
                    site: DefSite { krate: m.krate, module: None },
                    edition: Edition::E2021,
                    export: Export::Exported,
                    local_inner: LocalInner::No,
                    transparency: None,
                };
                let helpers: Vec<&str> = m.helpers.iter().map(String::as_str).collect();
                MacroDefinition::proc_macro(m.krate, kind, name, &helpers, at, alloc).ok().map(Resolved::Def)
            }
            _ => {
                count(&self.names.ambiguous);
                None
            }
        }
    }

    /// A call read through a `use` of its file and left there: counted apart, by the path as
    /// written, as read and why.
    fn count_through(&self, kind: ProcKind, written: &[&str], read: &[&str], why: &str) {
        let label = format!("{kind:?} {} = {} ({why})", written.join("::"), read.join("::"));
        *self.names.through_use.lock().unwrap_or_else(std::sync::PoisonError::into_inner).entry(label).or_default() += 1;
    }
}

/// This host's cfg (macOS on arm64), tests on, features off, as stage h answers it.
struct HostCfg;

impl Cfg for HostCfg {
    fn atom(&mut self, name: &str, value: Option<&str>) -> CfgAnswer {
        let yes = matches!(
            (name, value),
            ("test" | "unix" | "debug_assertions" | "target_has_atomic", None)
                | ("target_os", Some("macos"))
                | ("target_family", Some("unix"))
                | ("target_arch", Some("aarch64"))
                | ("target_pointer_width", Some("64"))
                | ("target_endian", Some("little"))
                | ("target_vendor", Some("apple"))
                | ("target_env", Some(""))
                | ("panic", Some("unwind"))
                | ("target_has_atomic", Some("8" | "16" | "32" | "64" | "ptr"))
        );
        if yes { CfgAnswer::Yes } else { CfgAnswer::No }
    }
}

/// A file's outside but its proc macros: the package's cargo variables and the cfg.
struct Host {
    package: Arc<Package>,
    /// The cfg predicate's scratch: the thread's handle of the entry's heap.
    alloc: Alloc<'static>,
}

impl ExpandCx for Host {
    fn env(&self, name: &str) -> Result<Option<&str>, HostError> {
        match self.package.env.get(name) {
            blaze_proc_macro::package_env::EnvAnswer::Set(value) => Ok(Some(value)),
            blaze_proc_macro::package_env::EnvAnswer::Unset => Ok(None),
            blaze_proc_macro::package_env::EnvAnswer::Unknown => Err(HostError { ask: HostAsk::Env, why: HostWhy::Refused }),
        }
    }

    fn cfg(&self, predicate: &TokenStream<'_>) -> Result<bool, HostError> {
        let mut atom = |name: &str, value: Option<&str>| -> Result<bool, ()> { Ok(HostCfg.atom(name, value) == CfgAnswer::Yes) };
        blaze_expand::cfg_predicate(predicate, self.alloc, &mut atom).map_err(|_| HostError { ask: HostAsk::Cfg, why: HostWhy::Refused })
    }

    fn include(&self, _: &str, _: IncludeAs) -> Result<Included<'_>, HostError> {
        Err(HostError { ask: HostAsk::Include, why: HostWhy::Refused })
    }

    fn place(&self, _: PlaceOf) -> Result<Place<'_>, HostError> {
        Err(HostError { ask: HostAsk::Place, why: HostWhy::Refused })
    }

    fn run_proc_macro<'a>(&self, _: &MacroDefinition<'_>, _: ProcCall<'_>, _: Alloc<'a>) -> Result<Produced<'a>, HostError> {
        Err(HostError { ask: HostAsk::ProcMacro, why: HostWhy::Refused })
    }
}

/// Per macro: calls, runs that gave output, outputs with an error diagnostic (a panic or an
/// emitted error), and refusals by the host's cause.
#[derive(Default, Clone, Copy)]
struct Tally {
    calls: u64,
    produced: u64,
    errors: u64,
    panics: u64,
    limit: u64,
    refused: u64,
    failed: u64,
}

/// [`ProcCx`] with each call tallied by its macro.
struct Counted<'p, 'r> {
    cx: ProcCx<'p, 'r, Host>,
    labels: &'p HashMap<CrateItemKey, String>,
    tally: &'p Mutex<BTreeMap<String, Tally>>,
    /// Per macro, its first few distinct panic messages.
    panics: &'p Mutex<BTreeMap<String, Vec<(String, String)>>>,
}

impl ExpandCx for Counted<'_, '_> {
    fn env(&self, name: &str) -> Result<Option<&str>, HostError> {
        self.cx.env(name)
    }

    fn cfg(&self, predicate: &TokenStream<'_>) -> Result<bool, HostError> {
        self.cx.cfg(predicate)
    }

    fn include(&self, path: &str, how: IncludeAs) -> Result<Included<'_>, HostError> {
        self.cx.include(path, how)
    }

    fn place(&self, what: PlaceOf) -> Result<Place<'_>, HostError> {
        self.cx.place(what)
    }

    fn place_of_call(&self, what: PlaceOf, call: u32) -> Result<Place<'_>, HostError> {
        self.cx.place_of_call(what, call)
    }

    fn run_proc_macro<'a>(&self, def: &MacroDefinition<'_>, call: ProcCall<'_>, alloc: Alloc<'a>) -> Result<Produced<'a>, HostError> {
        let krate = match def.kind() {
            blaze_expand::MacroKind::ProcMacro { krate, .. } => self.labels.get(&krate).map_or("?", String::as_str),
            _ => "?",
        };
        let label = format!("{:?} {krate} {}", call.kind, def.proc_name().unwrap_or("?"));
        let input = call.input;
        let result = self.cx.run_proc_macro(def, call, alloc);
        let mut tally = self.tally.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let t = tally.entry(label.clone()).or_default();
        t.calls += 1;
        match &result {
            Ok(produced) => {
                t.produced += 1;
                // syn's errors come back as `::core::compile_error! { "..." }` in the output.
                let tokens = &produced.tokens;
                if let Some(at) = (0..tokens.len()).find(|i| tokens.text_at(*i) == "compile_error") {
                    t.errors += 1;
                    let message = (at..tokens.len().min(at + 6)).map(|i| tokens.text_at(i)).collect::<Vec<_>>().join(" ");
                    let mut first = self.panics.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                    let entry = first.entry(format!("{label} (compile_error)")).or_default();
                    if !entry.iter().any(|(m, _)| *m == message) && entry.len() < 6 {
                        let given = (0..input.len())
                            .map(|i| match input.text_at(i) {
                                "" => format!("{:?}", input.tokens()[i].kind),
                                text => text.to_owned(),
                            })
                            .collect::<Vec<_>>()
                            .join(" ");
                        // The whole input: a derive's input is read in full when its macro refuses it.
                        entry.push((format!("{message}\n      input: {given}"), String::new()));
                    }
                }
                if let Some(message) = produced.diagnostics.iter().find_map(|d| match d {
                    ProcDiagnostic::Panicked { message } => Some(*message),
                    _ => None,
                }) {
                    t.panics += 1;
                    let text = message.get(&produced.text).map(str::to_owned).unwrap_or_default();
                    let mut first = self.panics.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                    let entry = first.entry(label).or_default();
                    if !entry.iter().any(|(m, _)| *m == text) && entry.len() < 4 {
                        entry.push((text, String::new()));
                    }
                } else if produced.diagnostics.iter().any(|d| matches!(d, ProcDiagnostic::Emitted { level: ProcLevel::Error, .. })) {
                    t.errors += 1;
                }
            }
            Err(e) => match e.why {
                HostWhy::Limit(_) => t.limit += 1,
                HostWhy::Refused => t.refused += 1,
                _ => t.failed += 1,
            },
        }
        result
    }
}

/// The expander, each call it failed noted by its path.
struct Noted<'n, E> {
    inner: E,
    failed: &'n Mutex<BTreeMap<String, u64>>,
    /// The attribute calls this file made, by the path's last segment: checked against the
    /// file's text for `tokio::test`'s census.
    attrs: BTreeMap<String, u64>,
}

impl<E: Expand> Noted<'_, E> {
    fn note(&self, what: &str, path: &str, e: Expansion) -> Expansion {
        if e == Expansion::Failed {
            *self.failed.lock().unwrap_or_else(std::sync::PoisonError::into_inner).entry(format!("{what} {path}")).or_default() += 1;
        }
        e
    }
}

impl<E: Expand> Expand for Noted<'_, E> {
    fn expand(
        &mut self,
        path: &str,
        path_mark: Option<blaze_lower::mark_site::TextMark>,
        input: &str,
        input_marks: blaze_lower::InputMarks<'_>,
        edition: Edition,
        cfg: &mut dyn blaze_lower::Cfg,
        out: &mut BodyOut<'_>,
    ) -> Expansion {
        let e = self.inner.expand(path, path_mark, input, input_marks, edition, cfg, out);
        self.note("item call", path, e)
    }

    fn expand_body(&mut self, call: BodyCall<'_>, cfg: &mut dyn blaze_lower::Cfg, out: &mut BodyOut<'_>) -> Expansion {
        let e = self.inner.expand_body(call, cfg, out);
        self.note("body call", call.path, e)
    }

    fn begin_body(&mut self) {
        self.inner.begin_body();
    }

    fn attr(
        &mut self,
        path: &str,
        path_mark: Option<blaze_lower::mark_site::TextMark>,
        args: Option<&TokenStream<'_>>,
        item: &TokenStream<'_>,
        edition: Edition,
        out: &mut BodyOut<'_>,
    ) -> Expansion {
        let e = self.inner.attr(path, path_mark, args, item, edition, out);
        if e == Expansion::Expanded {
            *self.attrs.entry(path.replace(' ', "")).or_default() += 1;
        }
        self.note("attribute", path, e)
    }

    fn derive(
        &mut self,
        path: &str,
        path_mark: Option<blaze_lower::mark_site::TextMark>,
        item: &TokenStream<'_>,
        facts: blaze_expand::DeriveFacts,
        edition: Edition,
        out: &mut BodyOut<'_>,
    ) -> Expansion {
        let e = self.inner.derive(path, path_mark, item, facts, edition, out);
        self.note("derive", path, e)
    }
}

/// Counts the records the driver hands over, and checks each body as stage h of blaze-real's
/// `stages` does: clean when the lowering left no form unhandled and no diagnostic, and
/// blaze-typeck's `to_typeck` takes it (written types left to inference).
struct Records(u64, u32, Bodies, Alloc<'static>);

/// Bodies by where their item came from: `[a file's item, an expansion's item, an item declared
/// in a body]` (the last of either: a derive's `const _: () = { impl .. }` is one).
#[derive(Default)]
struct Bodies {
    seen: [u64; 3],
    clean: [u64; 3],
    cfg_out: [u64; 3],
    /// The first cause that stopped a body, by cause, with the bodies it stopped.
    first: BTreeMap<String, [u64; 3]>,
}

static BODIES: Mutex<Option<Bodies>> = Mutex::new(None);

struct Unresolved;

impl Types for Unresolved {
    fn ty(&mut self, _: &View<'_>, _: TypeId, tys: &mut PortableTys<'_>) -> Result<PTy, Refused> {
        tys.intern(PNode::Infer).map_err(Refused::Check)
    }

    fn repeat_len(&mut self, _: &View<'_>, _: ExprId) -> Result<u64, Refused> {
        Ok(0)
    }

    fn const_arg(&mut self, view: &View<'_>, c: blaze_lower::ConstIdx, _: &mut PortableTys<'_>) -> Result<PTy, Refused> {
        Err(blaze_lower::typeck::no_const_arg(view, c))
    }
}

impl Bodies {
    fn check(&mut self, from: usize, view: &View<'_>, alloc: Alloc<'_>) {
        if view.cfg_out() {
            self.cfg_out[from] += 1;
            return;
        }
        self.seen[from] += 1;
        let cause = if let Some(u) = view.unhandled().next() {
            let kind = format!("{:?} ({:?})", blaze_file_items::syntax_kind(u.syntax), u.why);
            if kind.starts_with("MACRO_CALL") {
                let piece = view.text_at(u.range).unwrap_or("");
                format!("lower: a macro call, not expanded: {}", piece.split('!').next().unwrap_or(piece).trim())
            } else {
                format!("lower: unhandled {kind}")
            }
        } else if let Some(diag) = view.diags().next() {
            match diag {
                Ok(diag) => format!("lower: {:?}", diag.kind),
                Err(_) => "lower: a bad row".to_owned(),
            }
        } else {
            let mut tys = PortableTys::new_in(alloc);
            match to_typeck(view, ItemKey(0), &mut tys, &mut Unresolved, alloc) {
                Ok(_) => {
                    self.clean[from] += 1;
                    return;
                }
                Err(Refused::NoBody) => "to_typeck: no body".to_owned(),
                Err(Refused::Form { what, .. }) => format!("to_typeck: {what}"),
                Err(Refused::Bad(_)) => "to_typeck: a bad row".to_owned(),
                Err(Refused::Check(_)) => "to_typeck: a table error".to_owned(),
                Err(Refused::Text(_)) => "to_typeck: a text error".to_owned(),
            }
        };
        self.first.entry(cause).or_default()[from] += 1;
    }

    fn add(&mut self, other: Bodies) {
        for i in 0..3 {
            self.seen[i] += other.seen[i];
            self.clean[i] += other.clean[i];
            self.cfg_out[i] += other.cfg_out[i];
        }
        for (cause, n) in other.first {
            let at = self.first.entry(cause).or_default();
            for i in 0..3 {
                at[i] += n[i];
            }
        }
    }
}

impl Sink for Records {
    fn record(&mut self, item: &ItemRef<'_>, what: What, _: blaze_ids::ContentId, outcome: Outcome<'_, '_>) {
        self.0 += 1;
        let from = match item.source {
            blaze_lower::Source::Expansion { depth } => {
                self.1 = self.1.max(depth);
                1
            }
            blaze_lower::Source::File => 0,
            blaze_lower::Source::BodyItem => 2,
        };
        if let (What::Body, Outcome::Record { view, .. }) = (what, outcome) {
            self.2.check(from, &view, self.3);
        }
    }

    fn refused(&mut self, item: &ItemRef<'_>, refusal: blaze_lower::Refusal<'_>) {
        println!("  refused: {} on {}: {}", refusal.path, item.item_text().lines().next().unwrap_or(""), refusal.bound);
    }
}

/// The deepest item-position expansion chain met, and the most item macro expansions in one
/// file, with that file.
static DEEPEST: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
static MOST_IN_A_FILE: Mutex<(u64, String)> = Mutex::new((0, String::new()));

fn edition_of(manifest: &str, root: &str) -> Edition {
    let find = |text: &str| text.lines().find_map(|line| line.trim().strip_prefix("edition").map(|it| it.trim().trim_start_matches('=').trim().trim_matches('"').to_owned()));
    let written = find(manifest).filter(|it| !it.starts_with('.') && !it.contains("workspace")).or_else(|| find(root));
    match written.as_deref() {
        Some("2015") => Edition::E2015,
        Some("2018") => Edition::E2018,
        Some("2021") => Edition::E2021,
        _ => Edition::E2024,
    }
}

fn add(into: &mut FileStats, s: FileStats) {
    into.macro_calls_expanded += s.macro_calls_expanded;
    into.macro_calls_unsupported += s.macro_calls_unsupported;
    into.macro_calls_failed += s.macro_calls_failed;
    into.macro_calls_too_deep += s.macro_calls_too_deep;
    into.body_calls_expanded += s.body_calls_expanded;
    into.bodies += s.bodies;
    into.items += s.items;
    into.unhandled += s.unhandled;
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    // `--no-proc-macros`: register no macro crate, so every procedural macro call is left
    // unresolved: the "before" of the bodies' check.
    let (checkout, store, units, out, without) = match &args[..] {
        [_, checkout, store, units, out] => (checkout, store, units, out, false),
        [_, checkout, store, units, out, flag] if flag == "--no-proc-macros" => (checkout, store, units, out, true),
        _ => panic!("usage: tempo_pipeline <checkout> <module store> <unit list> <out> [--no-proc-macros]"),
    };
    let (checkout, store, out) = (Path::new(checkout), Path::new(store), PathBuf::from(out));
    let lock_text = std::fs::read_to_string(checkout.join("Cargo.lock")).expect("the checkout's lock");
    let lock = read_lock(&lock_text);
    let mut built = modules(store, checkout);
    // The process heap, named once at the entry: the runtime's, and every allocator the file
    // loop hands down.
    let heap = blaze_own::Heap::process();
    let runtime = Runtime::new(heap).expect("the runtime");
    let records = expansion_records(&out);
    let mut procs = ProcMacros::new(&runtime, &records, Limits::STANDARD);
    // The module keys' closures walk the lock blaze-manifest's one reader read (rule 30); the
    // example's own `read_lock` above is the interim builder's copy, for its distances only.
    let parsed = blaze_manifest::lock::Lock::read(lock_text.as_bytes(), heap.alloc()).unwrap_or_else(|e| panic!("the checkout's Cargo.lock: {e}"));
    let locked = blaze_proc_macro::module::LockedPackages::of(&parsed);
    let mut macros = Vec::new();
    let mut labels = HashMap::new();
    let unit_list = std::fs::read_to_string(units).expect("the unit list");
    let mut registered = 0;
    for line in unit_list.lines().filter(|_| !without).filter_map(|l| l.trim().strip_prefix("proc-macro u")).filter(|l| l.starts_with(|c: char| c.is_ascii_digit())) {
        let mut words = line.split_whitespace().skip(1);
        let (name, version) = (words.next().expect("a name"), words.next().expect("a version"));
        let package = (0..lock.len()).find(|i| lock[*i].name == name && lock[*i].version == version).expect("the macro crate in the lock");
        let features: Vec<&str> = line
            .split_once('[')
            .and_then(|(_, rest)| rest.split_once(']'))
            .map(|(list, _)| list.split(',').map(str::trim).filter(|it| !it.is_empty()).collect())
            .unwrap_or_default();
        // The source as the module is keyed by it: a path crate by its files as they are, a
        // registry crate by the lock's checksum, a git crate by its commit and its folder in the
        // repository, which the lock does not say (taken from the builder's identity).
        let checksum = lock[package].checksum.as_deref().unwrap_or_default();
        let digest;
        let git = git_of(store, name, version);
        let source = if lock[package].path {
            let dir = package_dir(checkout, name).expect("the path crate's folder");
            digest = files_digest(&mut package_files(&dir).expect("its files"));
            Source::Path { files_sha256: &digest }
        } else if let Some((commit, path)) = &git {
            Source::Git { commit, path }
        } else {
            Source::Registry { checksum: &checksum }
        };
        let key = procs.add_reached(store, &locked, &Reached { name, version, source, features: &features }).unwrap_or_else(|e| panic!("{e}"));
        // The builder's key and this crate's agree, or the module reads as missing.
        if let Some(at) = built.iter().position(|m| m.name == name && m.version == version) {
            assert_eq!(crate_key(&built.swap_remove(at).key), key, "{name} {version}: the builder's module key is the crate's");
        }
        // A crate with no module lists no exports here, so a call of it reads as unresolved
        // (its exports are in its source, which this example does not read).
        let exports = procs.exports(key);
        if exports.is_empty() {
            println!("macro crate {name} {version}: no module");
        }
        for e in exports {
            macros.push(Macro { package, krate: key, kind: e.kind, name: e.name.clone(), helpers: e.helpers.clone() });
        }
        labels.insert(key, format!("{name} {version}"));
        registered += 1;
    }
    println!("macro crates: {registered} registered, {} with a module", procs.crates().filter(|c| c.2).count());
    let names = Names {
        lock,
        macros,
        dist: Mutex::new(HashMap::new()),
        unresolved: Mutex::new(BTreeMap::new()),
        ambiguous: Mutex::new(BTreeMap::new()),
        through_use: Mutex::new(BTreeMap::new()),
    };
    let root_manifest = std::fs::read_to_string(checkout.join("Cargo.toml")).unwrap_or_default();
    let files = sources(checkout);
    let threads = std::thread::available_parallelism().map_or(8, |n| n.get().min(12));
    for pass in ["first pass", "second pass (records)"] {
        let tally: Mutex<BTreeMap<String, Tally>> = Mutex::new(BTreeMap::new());
        let panics: Mutex<BTreeMap<String, Vec<(String, String)>>> = Mutex::new(BTreeMap::new());
        let failed: Mutex<BTreeMap<String, u64>> = Mutex::new(BTreeMap::new());
        let totals = Mutex::new((FileStats::default(), 0u64, 0usize));
        let next = AtomicUsize::new(0);
        let started = Instant::now();
        std::thread::scope(|s| {
            for _ in 0..threads {
                s.spawn(|| {
                    let mut mine = (FileStats::default(), 0u64, 0usize);
                    loop {
                        let at = next.fetch_add(1, Ordering::Relaxed);
                        let Some(path) = files.get(at) else { break };
                        let Ok(text) = std::fs::read_to_string(path) else { continue };
                        let package = Arc::new(package_of(checkout, path));
                        let blaze_proc_macro::sandbox::Files::Member { workspace, root } = &package.files else { unreachable!("package_of gives a member") };
                        let dir = workspace.join(root.seen().folder());
                        let manifest = std::fs::read_to_string(dir.join("Cargo.toml")).unwrap_or_default();
                        let edition = edition_of(&manifest, &root_manifest);
                        let pkg_name = match package.env.get("CARGO_PKG_NAME") {
                            blaze_proc_macro::package_env::EnvAnswer::Set(name) => name.to_owned(),
                            blaze_proc_macro::package_env::EnvAnswer::Unset | blaze_proc_macro::package_env::EnvAnswer::Unknown => String::new(),
                        };
                        let Some(member) = (0..names.lock.len()).find(|i| names.lock[*i].path && names.lock[*i].name == pkg_name) else {
                            continue;
                        };
                        // A file that does not lex has no `use` read: said, and its calls resolved
                        // by name alone (the driver refuses its text the same way).
                        let uses = Uses::of(&text, edition, heap.alloc()).unwrap_or_else(|e| {
                            println!("  {}: does not lex ({e:?}): its `use` items not read", path.display());
                            Uses::default()
                        });
                        let resolve = Resolve { names: &names, member, uses: &uses };
                        let cx = Counted { cx: ProcCx { inner: Host { package: Arc::clone(&package), alloc: heap.alloc() }, procs: &procs, package, edition }, labels: &labels, tally: &tally, panics: &panics };
                        let rules: &[(&str, blaze_expand::Prepared<'_>)] = &[];
                        let at = DefAt {
                            site: DefSite { krate: CrateItemKey(blaze_ids::ContentId([1; 32])), module: None },
                            edition,
                            export: Export::Exported,
                            local_inner: LocalInner::No,
                            transparency: None,
                        };
                        let lists = Lists { rules, builtin_at: at, then: Some(&resolve) };
                        let mut by_name = Noted { inner: ByName::with_names(&lists, cx, heap.alloc()), failed: &failed, attrs: BTreeMap::new() };
                        let mut sink = Records(0, 0, Bodies::default(), heap.alloc());
                        let stats = lower_file(&text, edition, &mut HostCfg, &mut blaze_lower::NoModule, &mut by_name, &mut sink, heap.alloc());
                        // `#[tokio::test` written in the file (doc comments aside) against the calls expanded.
                        for (name, head) in [("tokio::test", "#[tokio::test"), ("contract", "#[contract")] {
                            let written = text.lines().filter(|l| l.trim_start().starts_with(head)).count() as u64;
                            let ran = by_name.attrs.get(name).copied().unwrap_or(0);
                            if pass.starts_with("first") && written != ran {
                                println!("  {name} in {}: written {written}, expanded {ran}", path.display());
                            }
                        }
                        // test-case takes every `#[test_case]` of its function in one call: the
                        // functions written are the runs of them (a run starts at one whose
                        // previous code line ends no attribute).
                        let mut functions = 0u64;
                        let mut previous = "";
                        for line in text.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with("//")) {
                            if (line.starts_with("#[test_case") || line.starts_with("#[test_matrix")) && !previous.ends_with(']') {
                                functions += 1;
                            }
                            previous = line;
                        }
                        let ran: u64 = by_name
                            .attrs
                            .iter()
                            .filter(|(path, _)| ["test_case", "test_matrix", "test_case::test_case", "test_case::test_matrix", "test_case::case"].contains(&path.as_str()))
                            .map(|(_, n)| *n)
                            .sum();
                        if pass.starts_with("first") && (functions != 0 || ran != 0) {
                            println!("  test_case in {}: functions written {functions}, expanded {ran}", path.display());
                        }
                        DEEPEST.fetch_max(sink.1, Ordering::Relaxed);
                        BODIES.lock().unwrap_or_else(std::sync::PoisonError::into_inner).get_or_insert_with(Bodies::default).add(sink.2);
                        {
                            let mut most = MOST_IN_A_FILE.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                            if stats.macro_calls_expanded > most.0 {
                                *most = (stats.macro_calls_expanded, path.display().to_string());
                            }
                        }
                        add(&mut mine.0, stats);
                        mine.1 += sink.0;
                        mine.2 += 1;
                    }
                    let mut t = totals.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                    add(&mut t.0, mine.0);
                    t.1 += mine.1;
                    t.2 += mine.2;
                });
            }
        });
        let elapsed = started.elapsed();
        let (stats, records_n, files_n) = *totals.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let tally = tally.into_inner().unwrap_or_else(std::sync::PoisonError::into_inner);
        let sum = tally.values().fold(Tally::default(), |a, t| Tally {
            calls: a.calls + t.calls,
            produced: a.produced + t.produced,
            errors: a.errors + t.errors,
            panics: a.panics + t.panics,
            limit: a.limit + t.limit,
            refused: a.refused + t.refused,
            failed: a.failed + t.failed,
        });
        println!("== {pass}: {files_n} files of the workspace's packages, {:.2?} on {threads} threads; {records_n} records", elapsed);
        println!(
            "  driver: item macro calls expanded {}, unsupported {}, failed {}, too deep {}; body calls expanded {}",
            stats.macro_calls_expanded, stats.macro_calls_unsupported, stats.macro_calls_failed, stats.macro_calls_too_deep, stats.body_calls_expanded
        );
        {
            let most = MOST_IN_A_FILE.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            println!("  deepest item expansion chain {}; most item expansions in one file {} ({})", DEEPEST.load(Ordering::Relaxed), most.0, most.1);
        }
        if let Some(b) = BODIES.lock().unwrap_or_else(std::sync::PoisonError::into_inner).take() {
            let pct = |c: u64, s: u64| if s == 0 { 0.0 } else { 100.0 * c as f64 / s as f64 };
            for (i, from) in ["of a file's items", "of an expansion's items", "of items declared in bodies"].iter().enumerate() {
                println!(
                    "  bodies {from}: {} bodies: {} clean ({:.1}%), {} turned off by cfg",
                    b.seen[i],
                    b.clean[i],
                    pct(b.clean[i], b.seen[i]),
                    b.cfg_out[i]
                );
            }
            let mut causes: Vec<_> = b.first.iter().collect();
            causes.sort_by_key(|(_, n)| std::cmp::Reverse(n[0] + n[1] + n[2]));
            let non_macro: u64 = b.first.iter().filter(|(c, _)| !c.starts_with("lower: a macro call")).map(|(_, n)| n[0] + n[1] + n[2]).sum();
            println!("  bodies stopped by anything but an unexpanded macro call: {non_macro}");
            for (cause, n) in causes.iter().take(12) {
                println!("    first cause {cause}: {} / {} / {}", n[0], n[1], n[2]);
            }
        }
        println!(
            "  proc-macro calls {}: output {} (panicked {}, an error emitted {}), refused {}, limit {}, failed {}",
            sum.calls, sum.produced, sum.panics, sum.errors, sum.refused, sum.limit, sum.failed
        );
        if pass.starts_with("first") {
            println!("  per macro: calls / output / panicked / error / refused / limit / failed");
            let mut rows: Vec<_> = tally.iter().collect();
            rows.sort_by(|a, b| b.1.calls.cmp(&a.1.calls).then(a.0.cmp(b.0)));
            for (label, t) in rows {
                println!("    {label}: {} / {} / {} / {} / {} / {} / {}", t.calls, t.produced, t.panics, t.errors, t.refused, t.limit, t.failed);
            }
            for (label, messages) in panics.lock().unwrap_or_else(std::sync::PoisonError::into_inner).iter() {
                for (message, _) in messages {
                    // A `compile_error` entry carries its call's whole input; a panic's message is cut.
                    match label.ends_with("(compile_error)") {
                        true => println!("  panic of {label}: {message}"),
                        false => println!("  panic of {label}: {}", message.chars().take(400).collect::<String>()),
                    }
                }
            }
            let mut by_cause: BTreeMap<String, (u64, String)> = BTreeMap::new();
            println!("  results kept: {:?}", procs.keep_all().map_err(|error| error.to_string()));
            for finding in procs.take_findings() {
                let cause = match &finding {
                    Finding::MissingModule { .. } => "missing module".to_owned(),
                    Finding::Unloadable { .. } => "module does not load".to_owned(),
                    Finding::Compiled(_) => "compiled record refused".to_owned(),
                    Finding::UnknownCrate { .. } => "unregistered crate".to_owned(),
                    Finding::NoSuchMacro { .. } => "no such macro in its module".to_owned(),
                    Finding::Call { error, .. } => format!("call: {:?}", error.error.host_error().why),
                    Finding::Record { .. } => "record".to_owned(),
                };
                let e = by_cause.entry(cause).or_insert((0, finding.to_string()));
                e.0 += 1;
            }
            println!("  findings by cause (count, first):");
            for (cause, (n, first)) in &by_cause {
                println!("    {cause}: {n}; {}", first.chars().take(300).collect::<String>());
            }
            let show = |what: &str, map: &Mutex<BTreeMap<String, u64>>| {
                let map = map.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                let mut rows: Vec<_> = map.iter().collect();
                rows.sort_by(|a, b| b.1.cmp(a.1));
                println!("  {what}: {} paths, {} calls; the most called:", rows.len(), rows.iter().map(|r| *r.1).sum::<u64>());
                for (k, n) in rows.iter().take(40) {
                    println!("    {k}: {n}");
                }
            };
            show("unresolved (no macro crate the lock reaches exports it)", &names.unresolved);
            show("ambiguous", &names.ambiguous);
            show("through the file's `use` (not sent to a proc macro of another crate)", &names.through_use);
            show("failed expansions, by what and path", &failed);
        }
    }
}
