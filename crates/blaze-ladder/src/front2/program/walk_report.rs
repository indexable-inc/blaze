//! THE WALK FROM A PROGRAM'S ROOTS, AS A REPORT: the measurer's `REACHABLE:` and `BUILDS:` lines
//! and their class lists (port27/front2's walk lists), from ANY list of roots of a resolved
//! world: the hello world's `main` (tests/front2_main.rs `his_main_through_the_checker`, which
//! held this code until 2026-11-16), a real member's bin `main` or a test unit's `#[test]` fns
//! (blaze-real examples/walk_member.rs, over tempo's world).
//!
//! 1. Every root is lowered and checked in ONE set of records; then ONE walk goes from all of
//!    them (reach.rs `reach_all`), so a fn two roots reach is lowered, checked and counted once.
//!    A root that does not check is no dead end: the walk goes on from its calls by name.
//! 2. `REACHABLE:` counts the reachable functions that type-check, and lists the walk's failure
//!    classes by count (ties in the order first met), each with its first fn.
//! 3. `BUILDS:` is the builds survey (run.rs `build_survey`) of the program made from the same
//!    roots ([`BuildFrom`]): what the CODE BUILDER makes of the clean fns, the refusals listed by
//!    class, by count then class text.
//!
//! What the walk cannot take is REFUSED BY NAME in the report, never skipped: a root that does
//! not lower (`WORK LIST (lowering ..)`, the line the hello world's test has always printed for
//! it), a root whose names do not spell (`ROOT REFUSED: ..`), a root that does not check
//! (`ROOT NOT CHECKED: ..`: walked on from its calls by name). The world's own gaps (a
//! dependency without records, a proc-macro crate without a module, a build script without
//! exported outputs) are outside the walk: the world's maker names them (walk_member.rs). The
//! world reaches the walk whole: the items' proc-macro expansions and build-script outputs are
//! in the names already (resolved with them); a BODY's macro calls expand through
//! [`World::body_sources`]; `prepared` is the declarative macros' prepared definitions a
//! lowering asks for ([`Prepares`]).
//!
//! The builds survey's program ([`Front2Program::make_from`]) is made with the same body
//! sources as the walk ([`World::body_sources`]), so a body's proc-macro call, `env!` or
//! include is answered there as in the walk.
//!
//! The lines are the test's own format strings, moved: one phrase of `REACHABLE:`, the test's
//! `his main`, is the caller's [`Ask::label`], and the test passes exactly `his main`.

use super::*;

use crate::front2::body::lower_fn;

use core::fmt::{self, Write as _};

use blaze_own::{AString, AVec};

/// The resolved world a walk runs over: the names (every crate, every dependency, with the
/// proc-macro expansions and build-script outputs they were resolved with), the frames, and
/// the program's own files (empty for a real workspace, where every crate is a library).
pub struct World<'w, 'n> {
    pub names: &'n Names<'n>,
    /// The command's frame: the walk's front end keeps its bodies' local items here.
    pub frame: &'n blaze_mem::Frame,
    /// The builds survey's frame: the program it builds is made apart from the walk.
    pub survey_frame: &'n blaze_mem::Frame,
    /// The declarative macros' prepared definitions, shared by every lowering.
    pub prepared: &'n Prepares<'n>,
    pub sources: &'w [SourceFile<'w>],
    /// By crate index, the crate's name: how a fn is named in the class lists ([`Shown`]).
    pub crate_names: &'w [&'w str],
    /// The texts, environment and proc-macro runner a body's macro calls expand with (the
    /// store's `sources`: what the world's names were expanded with); `None` for a world with
    /// no proc macros and no `include!` (the hello world's).
    pub body_sources: Option<&'n dyn crate::front2::body_host::BodySources>,
    pub receivers: Receivers,
}

/// Whether the walk's store looks up the pointer types a method's receiver is reached through
/// (`Box`, `Rc`, `Arc`, `Pin`: receiver.rs `Pointers::find`), as the census and the survey's
/// program do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Receivers {
    /// Looked up in the world's index.
    Found,
    /// Not looked up: the hello world's walk as measured since list-A (its numbers are the
    /// measurer's series; a change to it is a change of that series, made on purpose).
    NotLookedUp,
}

/// What the walk reads of a root that checked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RootRead {
    /// Its callees only (reach.rs `First::Checked`): the hello world's walk as measured since
    /// list-A (its numbers are the measurer's series).
    Callees,
    /// Its callees and its lowering whole (`First::CheckedBody`): also what the builder calls
    /// for what the root's body writes (a `format_args!`, an overloaded index), as every body
    /// the walk meets is read.
    Whole,
}

/// The roots the builds survey's program is made from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BuildFrom {
    /// `main` of crate 0's root, or the fn exported as the process's entry: the program a run
    /// builds ([`Front2Program::survey`]).
    Main,
    /// The walk's roots that lowered and spelled, each as a test suite's root (program.rs
    /// `Starts::Tests`): a root's signature fixes its entry (test_entry.rs), and a root
    /// returning a value is entered through its `Termination::report` fn, as a process's
    /// `main` is.
    Roots,
}

/// One walk's question.
pub struct Ask<'a> {
    /// The items to start from, by their library keys; a key given twice is one root.
    pub roots: &'a [LibKey],
    pub build_from: BuildFrom,
    pub read: RootRead,
    /// The roots as `REACHABLE:` names them (`his main`, `its main`, `12 roots`).
    pub label: &'a str,
}

/// One stage of a walk, for a caller's own lines between the walk's ([`WalkHooks`]).
pub struct Stage<'a, 'r, 'n> {
    pub records: &'a mut Records<'r, 'n>,
    pub prepared: &'n Prepares<'n>,
    /// Every root that lowered, in the order given.
    pub bodies: &'a [Body<'n>],
    /// The indices in [`Self::bodies`] of the roots the walk starts from (those that spelled).
    pub walked: &'a [usize],
    /// Each walked root's check, in the order of [`Self::walked`].
    pub checked: &'a mut [Result<Checked<'static>, CheckFail>],
}

/// A caller's own lines, written in the report where the walk calls them; both are called only
/// when at least one root is walked.
pub trait WalkHooks<'n> {
    /// After every root is checked, before the walk.
    fn checked(&mut self, _at: &mut Stage<'_, '_, 'n>, _out: &mut dyn fmt::Write) -> fmt::Result {
        Ok(())
    }

    /// After `REACHABLE:` and `BUILDS:`.
    fn walked(&mut self, _at: &mut Stage<'_, '_, 'n>, _out: &mut dyn fmt::Write) -> fmt::Result {
        Ok(())
    }
}

/// No lines of the caller's own.
pub struct NoHooks;

impl WalkHooks<'_> for NoHooks {}

/// The builds survey's counts, as `BUILDS:` prints them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BuildCounts {
    pub built: usize,
    pub tried: usize,
    pub no_instance: usize,
    pub left_out: usize,
}

/// What a walk counted.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct WalkCounts {
    /// The distinct roots given.
    pub roots: usize,
    /// Roots refused before the walk: their lowering failed.
    pub not_lowered: usize,
    /// Roots refused before the walk: their names did not spell.
    pub not_spelled: usize,
    /// Roots the walk started from.
    pub walked: usize,
    /// Of those, the ones that checked clean of gaps and failures.
    pub roots_checked: usize,
    /// The fns the walk reached beyond its roots.
    pub reached: usize,
    /// `REACHABLE:`'s first number: the roots and reached fns that type-check.
    pub type_check: usize,
    /// `REACHABLE:`'s second number: the reachable functions counted.
    pub reachable: usize,
    /// `None` when the survey made no program or stopped (its line says why).
    pub builds: Option<BuildCounts>,
}

/// Why a walk did not run.
#[derive(Debug)]
pub enum WalkError {
    /// The front end did not start over the world.
    Front(crate::front2::FrontError),
    /// The sink refused a line.
    Write(fmt::Error),
}

impl fmt::Display for WalkError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WalkError::Front(e) => write!(f, "front2: {e}"),
            WalkError::Write(e) => write!(f, "the walk's report could not be written: {e}"),
        }
    }
}

impl From<fmt::Error> for WalkError {
    fn from(e: fmt::Error) -> WalkError {
        WalkError::Write(e)
    }
}

/// `crate::name (key)` of a library key, as the walk lists name a fn: an impl, which has no
/// name, by its header as written (`impl<A> AsRef<OsStr> for OsString<A>`).
pub struct Shown<'s, 'n> {
    pub names: &'s Names<'n>,
    pub crates: &'s [&'s str],
    pub key: LibKey,
}

impl fmt::Display for Shown<'_, '_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let key = self.key;
        let krate = self.names.crate_of(key).and_then(|c| self.crates.get(c).copied()).unwrap_or("?");
        match self.names.item_text(key) {
            Some(t) => match t.name {
                Some(name) => write!(f, "{krate}::{name} ({key:x})"),
                None => {
                    write!(f, "{krate}::")?;
                    let header = t.text.split('{').next().unwrap_or("");
                    for (i, word) in header.split_whitespace().enumerate() {
                        if i > 0 {
                            f.write_str(" ")?;
                        }
                        f.write_str(word)?;
                    }
                    write!(f, " ({key:x})")
                }
            },
            None => write!(f, "{krate}::(no text) ({key:x})"),
        }
    }
}

/// The most characters of a refusal's text a root's line holds.
const MOST_SAID: usize = 300;

/// `what` on one line (a newline written as a space), cut at [`MOST_SAID`] characters.
fn write_cut(out: &mut dyn fmt::Write, what: &dyn fmt::Display, alloc: Alloc<'_>) -> fmt::Result {
    let mut text = AString::new_in(alloc);
    write!(text, "{what}")?;
    let end = text.char_indices().nth(MOST_SAID).map_or(text.len(), |(at, _)| at);
    for c in text[..end].chars() {
        out.write_char(if c == '\n' { ' ' } else { c })?;
    }
    Ok(())
}

/// The walk of `world` from `ask`'s roots, its report written to `out` (module docs), `hooks`'
/// lines where they belong; the counts returned. Returns early, after the roots' refusals and
/// with nothing walked, when no root lowered and spelled.
pub fn walk<'n>(world: &World<'_, 'n>, ask: &Ask<'_>, hooks: &mut dyn WalkHooks<'n>, out: &mut dyn fmt::Write, alloc: Alloc<'static>) -> Result<WalkCounts, WalkError> {
    let names = world.names;
    let prepared = world.prepared;
    let mut front = Front2::new(names, world.frame, world.sources, alloc).map_err(WalkError::Front)?;
    let mut store = Store::new(front.index.lang("sized"), front.frame, alloc);
    if world.receivers == Receivers::Found {
        store.pointers = crate::front2::receiver::Pointers::find(&front.index, front.resolver);
    }
    store.sources = world.body_sources;
    let mut buckets = None;
    let mut roots: AVec<'_, LibKey> = AVec::with_capacity_in(ask.roots.len(), alloc);
    for &root in ask.roots {
        if !roots.contains(&root) {
            roots.push(root);
        }
    }
    let mut counts = WalkCounts { roots: roots.len(), ..WalkCounts::default() };
    let shown = |key: LibKey| Shown { names, crates: world.crate_names, key };

    // Each root lowered, in the order given; one that does not lower is refused by name.
    let mut mains: AVec<'_, Body<'n>> = AVec::with_capacity_in(roots.len(), alloc);
    for &root in roots.iter() {
        match lower_fn(names, prepared, &mut front.keys, &mut store, root) {
            Ok(body) => mains.push(body),
            Err(e) => {
                counts.not_lowered += 1;
                write!(out, "WORK LIST (lowering {}): {e}\n", shown(root))?;
            }
        }
    }
    let mut views = AVec::with_capacity_in(mains.len(), alloc);
    for m in mains.iter() {
        views.push(m.lowered.view());
    }
    let mut spells = AVec::with_capacity_in(mains.len(), alloc);
    for v in views.iter() {
        spells.push(Spelling::of(v));
    }
    let mut walked: AVec<'_, usize> = AVec::with_capacity_in(mains.len(), alloc);
    let mut scopes = AVec::with_capacity_in(mains.len(), alloc);
    for (i, (m, spell)) in mains.iter().zip(spells.iter()).enumerate() {
        match spell {
            Ok(spell) => {
                walked.push(i);
                scopes.push(m.scope(spell));
            }
            Err(why) => {
                counts.not_spelled += 1;
                write!(out, "ROOT REFUSED: {}: its names do not spell: {why:?}\n", shown(m.item))?;
            }
        }
    }
    counts.walked = walked.len();
    if walked.is_empty() {
        return Ok(counts);
    }

    // Every walked root checked over one set of records.
    let mut consts = Consts::default();
    let mut records = Records {
        names,
        keys: &mut front.keys,
        index: &front.index,
        store: &mut store,
        buckets: &mut buckets,
        order: BuildOrder::AsAsked,
        consts: Some((&mut consts, prepared)),
    };
    let mut checked: AVec<'_, Result<Checked<'static>, CheckFail>> = AVec::with_capacity_in(walked.len(), alloc);
    for (&i, scope) in walked.iter().zip(scopes.iter()) {
        checked.push(check_body(&mut records, &mains[i], scope));
    }
    for (&i, c) in walked.iter().zip(checked.iter()) {
        match c {
            Ok(_) => counts.roots_checked += 1,
            Err(e) => {
                write!(out, "ROOT NOT CHECKED (walked on from its calls by name): {}: ", shown(mains[i].item))?;
                write_cut(out, e, alloc)?;
                out.write_str("\n")?;
            }
        }
    }
    hooks.checked(&mut Stage { records: &mut records, prepared, bodies: &mains, walked: &walked, checked: &mut checked }, out)?;

    // C: every fn reachable from the roots, each in its own env (a member's: its container's
    // generics first, then its own; `Self` the impl's self type or the trait's parameter 0).
    // A root that does not check is no dead end: the walk goes on from its calls by name.
    let census = {
        let read = ask.read;
        // `reach_all` takes its starts in the global allocator's vector (reach.rs).
        let starts = walked
            .iter()
            .zip(checked.iter_mut())
            .zip(scopes.iter())
            .map(|((&i, c), scope)| {
                let body = &mains[i];
                let first = match (c, read) {
                    (Ok(done), RootRead::Callees) => First::Checked(done),
                    (Ok(done), RootRead::Whole) => First::CheckedBody(body, done),
                    (Err(_), _) => First::Unchecked(body),
                };
                Start { owner: body.owner, first, scope }
            })
            .collect();
        reach_all(&mut records, prepared, starts, &[], crate::front2::reach::Within::ALL)
    };
    let ok = census.fns.iter().filter(|(_, r)| matches!(r, Reached::Checked)).count();
    let no_body = census.fns.iter().filter(|(_, r)| matches!(r, Reached::NoBody)).count();
    let ctors = census.fns.iter().filter(|(_, r)| matches!(r, Reached::Ctor)).count();
    let elsewhere = census.fns.iter().filter(|(_, r)| matches!(r, Reached::AtInstance | Reached::AtObject)).count();
    counts.reached = census.fns.len();
    counts.type_check = ok + counts.roots_checked;
    counts.reachable = census.fns.len() + walked.len() - no_body - ctors - elsewhere;
    write!(
        out,
        "REACHABLE: {} of {} reachable functions type-check ({} and {} reached; {ctors} ctors; {elsewhere} trait calls walked where the instance or object is made; {no_body} required trait methods not walked; {} calls not followed: their caller did not check and only its types name them; capped: {})\n",
        counts.type_check,
        counts.reachable,
        ask.label,
        census.fns.len(),
        census.not_followed,
        census.capped
    )?;
    // The failure classes: a count and the first fn, by count, ties in the order first met.
    let mut causes: AVec<'_, (&str, usize, AString<'_>)> = AVec::new_in(alloc);
    for (k, r) in census.fns.iter() {
        if let Reached::Failed { class, detail, .. } = r {
            match causes.iter_mut().find(|x| x.0 == &**class) {
                Some(x) => x.1 += 1,
                None => {
                    let mut first = AString::new_in(alloc);
                    match records.keys.library_in(records.names, *k) {
                        Some(l) => write!(first, "{}: {detail}", shown(l))?,
                        None => write!(first, "{k:?}: {detail}")?,
                    }
                    causes.push((class, 1, first));
                }
            }
        }
    }
    // A stable order with no buffer of the sort's own: each class's first place breaks a tie.
    let mut order: AVec<'_, (usize, usize)> = AVec::with_capacity_in(causes.len(), alloc);
    for (at, c) in causes.iter().enumerate() {
        order.push((c.1, at));
    }
    order.sort_unstable_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    for &(_, at) in order.iter() {
        let (class, n, first) = &causes[at];
        write!(out, "  {n} x {class}; the first: {first}\n")?;
    }

    // D: what the CODE BUILDER makes of the clean ones: the program with every clean fn tabled
    // (the rest left out), each instance from the roots built as the run builds it, through the
    // seam; a refusal is filed by its typed name and the walk goes on.
    // The survey's two lists are the global allocator's vectors of its API (program.rs, run.rs).
    let mut left_out = Default::default();
    let program = match ask.build_from {
        BuildFrom::Main => Front2Program::make_from(names, world.survey_frame, world.sources, Some(&mut left_out), Starts::Main, world.body_sources, alloc).map(|made| made.program),
        BuildFrom::Roots => {
            let mut from: AVec<'_, LibKey> = AVec::with_capacity_in(walked.len(), alloc);
            for &i in walked.iter() {
                from.push(mains[i].item);
            }
            Front2Program::make_from(names, world.survey_frame, world.sources, Some(&mut left_out), Starts::Tests(&from), world.body_sources, alloc).map(|made| made.program)
        }
    };
    match program {
        Err(e) => write!(out, "BUILDS: no survey: {e}\n")?,
        Ok(mut program) => {
            let mut refused = Default::default();
            // The survey builds as `blaze run` and `blaze test` do: the dev profile's overflow
            // checks (D101), which also makes it name a missing `panic_const_*` lang fn.
            match crate::run::build_survey(&mut program, &mut refused, blaze_build::OverflowChecks::On, alloc) {
                Err(e) => write!(out, "BUILDS: the survey stopped: {e}\n")?,
                Ok(survey) => {
                    write!(
                        out,
                        "BUILDS: {} of {} clean reachable functions' instances ({} refused; {} generic with no instance; {} left out unchecked)\n",
                        survey.built,
                        survey.tried,
                        survey.tried - survey.built,
                        survey.no_instance.len(),
                        left_out.len()
                    )?;
                    counts.builds = Some(BuildCounts { built: survey.built, tried: survey.tried, no_instance: survey.no_instance.len(), left_out: left_out.len() });
                    let mut classes: AVec<'_, (&str, usize, &str)> = AVec::new_in(alloc);
                    for (f, class) in refused.iter() {
                        match classes.iter_mut().find(|x| x.0 == &**class) {
                            Some(x) => x.1 += 1,
                            None => classes.push((class, 1, f)),
                        }
                    }
                    // By count, then the class's text: no two classes are equal.
                    classes.sort_unstable_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
                    for (class, n, first) in classes.iter() {
                        write!(out, "  {n} x {class}; the first: `{first}`\n")?;
                    }
                }
            }
        }
    }
    hooks.walked(&mut Stage { records: &mut records, prepared, bodies: &mains, walked: &walked, checked: &mut checked }, out)?;
    Ok(counts)
}
