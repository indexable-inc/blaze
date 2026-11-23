//! The expansion lane's census: every fn body of the toolchain's `core`, `alloc` and `bstd`
//! (free fns and members of impls and traits), lowered as front2's `body::lower_at` lowers it
//! (blaze-lower's `lower_body_expanding` with `ByName` over `Scoped` names and a host that
//! refuses every read, front2's `NoHost`), and each macro call that does not expand tallied by
//! its path as written and its typed cause (`by_name::NotExpanded`), largest first. Lowering
//! only: no check. A measurement, not a test of a property: it asserts nothing but that the
//! toolchain loads.
//!
//! Environment: `EXPAND_CENSUS_CRATES` (default `core,alloc,bstd`), `EXPAND_CENSUS_OUT` (the
//! file the table is written to). It needs the installed toolchain under
//! `$HOME/.blaze/v2/toolchain`, as tests/front2_census.rs does.

use std::collections::BTreeMap;
use std::time::Instant;

use blaze_expand::{ExpandCx, HostAsk, HostError, HostWhy, IncludeAs, Included, MacroDefinition, Place, PlaceOf, ProcCall, Produced, TokenStream};
use blaze_ladder::crates::{DirShelf, Inputs, Toolchain, UserCrate};
use blaze_ladder::front2::Front2;
use blaze_ladder::front2::heads::CrateCfg;
use blaze_ladder::refusal::SourceFile;
use blaze_lower::by_name::{ByName, NotExpanded};
use blaze_lower::scoped::{Prepares, Scoped};
use blaze_own::Alloc;
use blaze_scope::resolve::Names;
use blaze_surface::ItemDetail;

const EDITION: &str = "blaze-2026.11";
const MAIN: &str = "fn main(cx: &Cx, io: Io) -> Result<(), Error> {
    io.need_out()?.line(cx, \"Hello, world!\")?;
    Ok(())
}
";
const EXAMPLES: usize = 2;

/// front2's host (body.rs `NoHost`): every read refused, by what it asked.
struct NoHost;

fn refused(ask: HostAsk) -> HostError {
    HostError { ask, why: HostWhy::Refused }
}

impl ExpandCx for NoHost {
    fn env(&self, _: &str) -> Result<Option<&str>, HostError> {
        Err(refused(HostAsk::Env))
    }
    fn cfg(&self, _: &TokenStream<'_>) -> Result<bool, HostError> {
        Err(refused(HostAsk::Cfg))
    }
    fn include(&self, _: &str, _: IncludeAs) -> Result<Included<'_>, HostError> {
        Err(refused(HostAsk::Include))
    }
    fn place(&self, _: PlaceOf) -> Result<Place<'_>, HostError> {
        Err(refused(HostAsk::Place))
    }
    fn run_proc_macro<'a>(&self, _: &MacroDefinition<'_>, _: ProcCall<'_>, _: Alloc<'a>) -> Result<Produced<'a>, HostError> {
        Err(refused(HostAsk::ProcMacro))
    }
}

/// A path as written, its spaces gone (`$crate :: panic :: panic_2021` -> `$crate::panic::panic_2021`).
fn squeeze(path: &str) -> String {
    path.chars().filter(|c| !c.is_whitespace()).collect()
}

/// The cause without the numbers that differ per call (offsets, token indices).
fn cause_class(why: &str) -> String {
    let mut out = String::new();
    let mut digits = false;
    for c in why.chars() {
        if c.is_ascii_digit() {
            if !digits {
                out.push('N');
            }
            digits = true;
        } else {
            digits = false;
            out.push(c);
        }
    }
    if out.len() > 200 { format!("{}..", &out[..out.char_indices().nth(200).map_or(out.len(), |(i, _)| i)]) } else { out }
}

#[derive(Default)]
struct Row {
    calls: usize,
    bodies: usize,
    examples: Vec<String>,
}

#[test]
fn expand_census() {
    let start = Instant::now();
    let want = std::env::var("EXPAND_CENSUS_CRATES").unwrap_or_else(|_| "core,alloc,bstd".to_string());
    let out_path = std::env::var("EXPAND_CENSUS_OUT").ok();
    let home = std::env::var("HOME").expect("HOME is set");
    let shelf = format!("{home}/.blaze/v2/toolchain");
    let tc = Toolchain::open(&mut DirShelf::new(shelf.clone())).unwrap_or_else(|e| panic!("the toolchain at {shelf}: {e}"));
    let files = [("/hello-world/src/main.rs".to_string(), MAIN.to_string())];
    let edition = edition::PackageEdition::of_manifest(EDITION).expect("a known edition");
    let user = UserCrate { name: "hello_world", dir: "/hello-world/", root: "/hello-world/src/main.rs", files: &files, edition, panic: blaze_build::Panic::Unwind };
    let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("the crates: {e}"));
    let crates = inputs.crates();
    let names: Names<'_> = inputs.resolve(&crates, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
    let crate_names: Vec<&str> = crates.iter().map(|c| c.name).collect();
    let sources = [SourceFile { path: "src/main.rs", text: MAIN }];
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the command's frame");
    let front = Front2::new(&names, &frame, &sources, blaze_own::Alloc::global()).unwrap_or_else(|e| panic!("front2: {e}"));
    let prepared = Prepares::new_in(blaze_own::testing::bump());
    let mut out = format!("expand census: names resolved in {:?}\n", start.elapsed());

    // (path, cause) -> row; and per path alone.
    let mut by_cause: BTreeMap<(String, String), Row> = BTreeMap::new();
    let mut by_path: BTreeMap<String, (usize, usize)> = BTreeMap::new();
    let (mut bodies, mut stopped, mut calls) = (0usize, 0usize, 0usize);
    for w in want.split(',') {
        let Some(krate) = crate_names.iter().position(|n| *n == w) else { panic!("no crate named {w:?} in {crate_names:?}") };
        let cn = names.krate(krate);
        let atoms = cn.cfg.atoms();
        let ed = names.edition(krate);
        // (text, module, shown name)
        let mut texts: Vec<(&str, blaze_id::ItemKey, String)> = Vec::new();
        let mut member_of: std::collections::BTreeSet<blaze_id::ItemKey> = Default::default();
        let items: Vec<(blaze_id::ItemKey, ItemDetail)> = names.items(krate).filter_map(|(k, _, _)| front.resolver.detail(k).map(|d| (k, d))).collect();
        for (k, d) in &items {
            if matches!(d, ItemDetail::Impl | ItemDetail::Trait) {
                member_of.extend(names.members(*k).unwrap_or(&[]).iter().map(|m| m.key));
            }
        }
        for (k, d) in &items {
            let Some(it) = names.item_text(*k) else { continue };
            let module = cn.path_of(it.module).unwrap_or("?");
            match d {
                ItemDetail::Fn { .. } if !member_of.contains(k) => {
                    texts.push((it.text, it.module, format!("{w}: {module}::{}", it.name.unwrap_or("?"))));
                }
                ItemDetail::Impl | ItemDetail::Trait => {
                    let head = it.impl_header.map_or_else(|| format!("trait {}", it.name.unwrap_or("?")), |h| h.chars().take(60).collect());
                    for m in names.members(*k).unwrap_or(&[]) {
                        if matches!(m.detail, ItemDetail::Fn { .. }) {
                            texts.push((m.text, it.module, format!("{w}: {module}::<{head}>::{}", m.name.unwrap_or("?"))));
                        }
                    }
                }
                _ => {}
            }
        }
        let (mut cb, mut cs) = (0usize, 0usize);
        for (text, module, shown) in &texts {
            if text.trim_end().ends_with(';') {
                continue;
            }
            cb += 1;
            let scoped = Scoped { names: &names, module: *module, prepared: Some(&prepared) };
            let mut expand = ByName::with_names(&scoped, NoHost, Alloc::global());
            if blaze_lower::lower_body_expanding(text, ed, &mut CrateCfg(atoms), &mut blaze_lower::NoModule, &mut expand, Alloc::global()).is_err() {
                continue;
            }
            let mut seen: Vec<String> = Vec::new();
            for f in expand.findings() {
                let kind = match &f.why {
                    NotExpanded::NoMacro(_) => "no macro",
                    NotExpanded::ReadList(_) => "read list",
                    NotExpanded::Failed(_) => "failed",
                };
                let path = squeeze(f.path.as_str());
                let cause = format!("{kind}: {}", cause_class(&format!("{}", f.why)));
                calls += 1;
                let row = by_cause.entry((path.clone(), cause.clone())).or_default();
                row.calls += 1;
                let p = by_path.entry(path.clone()).or_default();
                p.0 += 1;
                if !seen.contains(&path) {
                    p.1 += 1;
                }
                if !seen.iter().any(|s| *s == format!("{path}\u{0}{cause}")) {
                    row.bodies += 1;
                    seen.push(format!("{path}\u{0}{cause}"));
                }
                seen.push(path);
                if row.examples.len() < EXAMPLES {
                    row.examples.push(format!("{shown} at {:?}: {}", f.at, f.why));
                }
            }
            if !expand.findings().is_empty() {
                cs += 1;
            }
        }
        bodies += cb;
        stopped += cs;
        out.push_str(&format!("crate {w}: {cb} bodies lowered, {cs} with a macro call not expanded\n"));
    }
    out.push_str(&format!("TOTAL: {bodies} bodies, {stopped} with a macro call not expanded, {calls} such calls\n"));
    let mut paths: Vec<_> = by_path.into_iter().collect();
    paths.sort_by(|a, b| b.1.0.cmp(&a.1.0).then_with(|| a.0.cmp(&b.0)));
    out.push_str("BY MACRO (calls, bodies), largest first:\n");
    for (p, (c, b)) in &paths {
        out.push_str(&format!("{c:6} {b:6}  {p}\n"));
    }
    let mut rows: Vec<_> = by_cause.into_iter().collect();
    rows.sort_by(|a, b| b.1.calls.cmp(&a.1.calls).then_with(|| a.0.cmp(&b.0)));
    out.push_str("BY MACRO AND CAUSE (calls, bodies), largest first:\n");
    for ((p, c), r) in &rows {
        out.push_str(&format!("{:6} {:6}  {p}  --  {c}\n", r.calls, r.bodies));
        for x in &r.examples {
            out.push_str(&format!("               e.g. {x}\n"));
        }
    }
    out.push_str(&format!("expand census: {:?} in all\n", start.elapsed()));
    print!("{out}");
    if let Some(p) = out_path {
        std::fs::write(&p, &out).unwrap_or_else(|e| panic!("writing {p}: {e}"));
    }
}
