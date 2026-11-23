//! The soundness of what a definition can write (blaze-expand `writes`), against real
//! expansions: a generated `macro_rules!` definition is called with one identifier, expanded by
//! blaze-expand's `expand`, its token record outlined by blaze-file-items'
//! `outline_token_record`, and every name the expansion binds in the calling module (each
//! top-level entry's name, each `use` leaf's binding) must be one the call's summary lists; a
//! `#[macro_export]` macro needs `exports`.
//!
//! THE CONTROL: a summary that forgets the names a `use` writes (`call_writes_keeping` with
//! `by != By::Use`) must be caught; the cases are the same.
//!
//! Prints `macro writes sound: N cases, S summarised Only, B names bound, every one allowed`.

use std::cell::Cell;

use blaze_expand::{
    By, Call, CallSite, Ctx, DefAt, DefSite, Edition, ExpandCx, Expanded, Export, HostAsk, HostError, HostWhy,
    IncludeAs, Included, LocalInner, MacroDefinition, Place, PlaceOf, ProcCall, Produced, SourceIndexes, TokenStream,
    Written, expand, lex, macro_writes, Callee, Callees,
};
use blaze_file_items::{SyntaxKind, outline_token_record};
use blaze_id::{ContentId, CrateItemKey};
use blaze_mem::Frame;
use blaze_mem::tag::Tag;
use blaze_names_demand::types::Writes;
use blaze_names_source::call_writes_keeping;
use blaze_own::{AVec, Alloc, BVec, Bump, Erased, Vec};
use blaze_sim::{Broke, Config, Tape};
use blaze_tokens::Tokens;

/// Transcriber pieces, `$n` the call's one identifier. By hand: P0 writes `lit_a`, P1 `LitB`,
/// P2 and P3 the input's name (P3's `inside` is in the module), P4 `p` and `r`, P5 `s`, P6 the
/// export `mexp`, P7 nothing (an impl), P8 `ST`, P9 `En`, P10 nothing (`_`), P11 `t` and the
/// input's name (a use whose last segment is the input).
const PIECES: &[&str] = &[
    "fn lit_a() {}",
    "pub struct LitB;",
    "fn $n() {}",
    "mod $n { fn inside() {} }",
    "use $crate::x::{p, q as r};",
    "use $crate::y::s;",
    "#[macro_export] macro_rules! mexp { () => {} }",
    "impl Thing { fn hidden() {} }",
    "static ST: u8 = 0;",
    "enum En { V }",
    "const _: () = ();",
    "use $crate::w::{t, $n};",
];

/// The call's input.
const INPUTS: &[&str] = &["alpha", "Beta", "gamma"];

/// An arm that never matches the call: its writes join the summary (the union over arms).
const EXTRA_ARMS: &[&str] = &["", "(@e) => { 1 + 2 };", "(@f) => { fn from_other_arm() {} };"];

#[derive(Clone, Copy, Debug)]
enum Plant {
    Clean,
    /// The summary forgets what a `use` binds.
    ForgetsUse,
}

#[derive(Default)]
struct Tally {
    cases: Cell<u32>,
    only: Cell<u32>,
    bound: Cell<u32>,
}

/// No host: the generated definitions read no env, file, cfg or proc macro.
struct NoHost;

const NO_HOST: HostError = HostError { ask: HostAsk::ProcMacro, why: HostWhy::Failed };

impl ExpandCx for NoHost {
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

    fn run_proc_macro<'a>(&self, _: &MacroDefinition<'_>, _: ProcCall<'_>, _: Alloc<'a>) -> Result<Produced<'a>, HostError> {
        Err(NO_HOST)
    }
}

/// No nested calls are generated: none is known.
struct NoCallees;

impl<'d> Callees<'d> for NoCallees {
    fn callee(&self, _: Callee<'d>) -> Option<&'d MacroDefinition<'d>> {
        None
    }
}

fn at() -> DefAt {
    DefAt {
        site: DefSite { krate: CrateItemKey(ContentId([3; 32])), module: None },
        edition: Edition::E2021,
        export: Export::Local,
        local_inner: LocalInner::No,
        transparency: None,
    }
}

fn harness(what: &str, why: impl core::fmt::Debug) -> Broke {
    Broke::Harness(std::format!("{what}: {why:?}"))
}

/// One name the expansion binds in the calling module, and whether it is an exported macro.
struct Bound<'t> {
    name: &'t str,
    exported: bool,
}

/// What the outline of `tokens` binds at its top level: each named entry's name (not `_`), and
/// for a `use`, each leaf's binding (the identifier before a `,`, `}` or `;`: the generated
/// trees are `a::{b, c as d}` and `a::b`).
fn bound<'t>(tokens: Tokens<'t>, alloc: Alloc<'_>, out: &mut AVec<'_, Bound<'t>>) -> Result<(), Broke> {
    let outline = outline_token_record(tokens, alloc).map_err(|why| harness("the expansion's outline", why))?;
    if !outline.errors().is_empty() {
        return Err(harness("the expansion parses", outline.errors()));
    }
    for entry in outline.entries().iter().filter(|entry| entry.parent.is_none()) {
        if entry.kind == SyntaxKind::USE {
            // The last token is the `;`, so `i + 1` is inside the entry.
            for i in entry.extent.start as usize..(entry.extent.end as usize).saturating_sub(1) {
                let next = tokens.text(i + 1);
                if tokens.kind(i) == SyntaxKind::IDENT && matches!(next, "," | "}" | ";") {
                    out.push(Bound { name: tokens.text(i), exported: false });
                }
            }
            continue;
        }
        let Some(name) = outline.name(entry, tokens) else { continue };
        if name == "_" {
            continue;
        }
        let exported = entry.kind == SyntaxKind::MACRO_RULES
            && outline.attrs(entry).iter().any(|attr| (attr.start..attr.end).any(|i| tokens.text(i as usize) == "macro_export"));
        out.push(Bound { name, exported });
    }
    Ok(())
}

/// The record `bytes` at a 16-byte aligned address, as `Tokens::open` reads it.
fn aligned<'a>(bytes: &[u8], bump: Bump<'a>) -> &'a [u8] {
    let mut words: BVec<'a, u128> = Vec::new_in(bump);
    for chunk in bytes.chunks(16) {
        let mut word = [0u8; 16];
        word[..chunk.len()].copy_from_slice(chunk);
        words.push(u128::from_le_bytes(word));
    }
    let words: &'a [u128] = words.into_bump_slice();
    &blaze_format::bytes_of_slice(words)[..bytes.len()]
}

fn case(tape: &Tape, plant: Plant, tally: &Tally) -> Result<(), Broke> {
    let frame = Frame::new(Tag::Expand).map_err(|why| harness("the case's frame", why))?;
    let erased = Erased::new(&frame);
    let bump = erased.bump();
    let alloc = bump.alloc();
    // The definition: `($n:ident) => { pieces };` and maybe an arm the call does not take.
    let mut body: AVec<'_, u8> = Vec::new_in(alloc);
    body.extend_from_slice(EXTRA_ARMS[tape.index(EXTRA_ARMS.len(), "extra arm")].as_bytes());
    body.extend_from_slice(b" ($n:ident) => { ");
    for _ in 0..1 + tape.index(5, "pieces") {
        body.extend_from_slice(PIECES[tape.index(PIECES.len(), "piece")].as_bytes());
        body.push(b' ');
    }
    body.extend_from_slice(b"};");
    let body = core::str::from_utf8(&body).map_err(|why| harness("the body", why))?;
    let body = lex(body, Edition::E2021, alloc).map_err(|why| harness("the body lexes", why))?;
    let def = MacroDefinition::rules(body, at());
    let input = lex(INPUTS[tape.index(INPUTS.len(), "input")], Edition::E2021, alloc)
        .map_err(|why| harness("the input lexes", why))?;
    // The summary.
    let writes = macro_writes(&def, &NoCallees, alloc).map_err(|why| harness("the definition compiles", why))?;
    let keep = |written: &Written<'_>| match plant {
        Plant::Clean => true,
        Plant::ForgetsUse => written.by != By::Use,
    };
    let summary = call_writes_keeping(&writes, Some(&input), &keep, bump);
    // The expansion, for real.
    let site = CallSite { edition: Edition::E2021, panic_edition: Edition::E2021, ctx: Ctx::root(Edition::E2021) };
    let call = Call::fn_like(&input, site);
    let Expanded::Expansion(expansion) = expand(&def, &call, &NoHost, alloc).map_err(|why| harness("the call expands", why))?
    else {
        return Err(harness("the call expands", "an eager input, which no piece writes"));
    };
    let sources = SourceIndexes::of(&def, &call, alloc);
    let record = expansion.token_record(&sources.maps(), alloc).map_err(|why| harness("the token record", why))?;
    let tokens = Tokens::open(aligned(&record, bump)).map_err(|why| harness("the record opens", why))?;
    let mut names: AVec<'_, Bound<'_>> = Vec::new_in(alloc);
    bound(tokens, alloc, &mut names)?;
    tally.cases.set(tally.cases.get() + 1);
    let Writes::Only { names: allowed, exports } = summary else { return Ok(()) };
    tally.only.set(tally.only.get() + 1);
    for name in names.iter() {
        tally.bound.set(tally.bound.get() + 1);
        if !allowed.contains(&name.name) {
            return Err(Broke::Oracle(std::format!(
                "the expansion binds `{}`, which its writes summary {allowed:?} does not allow",
                name.name
            )));
        }
        if name.exported && !exports {
            return Err(Broke::Oracle(std::format!("the expansion exports `{}`, its summary says no export", name.name)));
        }
    }
    Ok(())
}

/// The invariant: every name an expansion binds in its calling module (items and `use` bindings) is listed in its `writes` summary.
#[test]
fn every_name_an_expansion_binds_is_in_its_summary() {
    let tally = Tally::default();
    blaze_sim::check(Config::cases(400), |tape| case(tape, Plant::Clean, &tally));
    assert!(tally.only.get() > 0, "no case was summarised Only: the property held vacuously");
    assert!(tally.bound.get() > 0, "no expansion bound a name");
    std::println!(
        "macro writes sound: {} cases, {} summarised Only, {} names bound, every one allowed",
        tally.cases.get(),
        tally.only.get(),
        tally.bound.get()
    );
}

/// THE CONTROL: the same cases with the summaries forgetting `use`; the oracle must break.
#[test]
fn the_oracle_catches_a_summary_that_forgets_use() {
    let tally = Tally::default();
    blaze_sim::control(Config::cases(400), Plant::Clean, Plant::ForgetsUse, "writes", |tape, plant| {
        case(tape, plant, &tally)
    });
}
