//! One crate of a set resolved APART from the others: every other crate's names once
//! ([`Apart::solve`]), then that one crate's over them ([`Apart::finish`]), as many times as
//! there are texts for it. For a runner of many small programs over one library (blaze-ladder's
//! `front2_rungs` example: 117 programs over the toolchain's crates), where
//! [`resolve_crates`](crate::resolve::resolve_crates) would resolve the library once per
//! program.
//!
//! The two steps together give what one `resolve_crates` call over the same inputs gives:
//! - no other crate depends on the one apart (checked: [`ApartError::DependedOn`],
//!   [`ApartError::IsSysroot`]), so no other crate's names read it, whatever order the graph's
//!   waves put it in;
//! - the crate graph the others were resolved under is the graph of the finishing call:
//!   [`Apart::finish`] refuses inputs whose graph facts differ from the first step's
//!   ([`ApartError::Differs`]): the crate apart may change only its texts (`root`,
//!   `shown_from`, `script`, and what the sources answer for it), every other crate nothing;
//! - the build's one set of prepared macro definitions ([`Prepares`]) is the same in both steps.

use core::fmt;

use blaze_hash::HashMap;
use blaze_own::{AVec, Alloc, Bump};

use crate::crate_scope::Dep;
use crate::expansions::KeptExpansions;
use crate::graph::{CrateId, Sysroot};
use crate::prepares::Prepares;
use crate::resolve::{Built, CrateInput, CrateNames, Names, NamesError, SourceTexts, graph_of, resolve_one};
use crate::task::{First, Par};

/// Why a set was not resolved with one crate apart.
#[derive(Clone, Debug)]
pub enum ApartError<'a> {
    /// The resolution itself refused.
    Names(NamesError<'a>),
    /// `apart` is not an index of the `crates` given.
    NoSuchCrate { apart: usize, crates: usize },
    /// Crate `by` names the crate apart as its dependency `named`: its names would read it.
    DependedOn { apart: &'a str, by: &'a str, named: &'a str },
    /// The crate apart is one of the sysroot's: every crate reaches it without a dependency line.
    IsSysroot { apart: &'a str },
    /// [`Apart::finish`] was given another number of crates than [`Apart::solve`].
    Count { solved: usize, given: usize },
    /// [`Apart::finish`] was given crate `krate` with another `what` than [`Apart::solve`] was.
    Differs { krate: &'a str, what: &'static str },
}

impl fmt::Display for ApartError<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ApartError::Names(error) => write!(f, "{error}"),
            ApartError::NoSuchCrate { apart, crates } => write!(f, "the crate to resolve apart is crate {apart}, past the {crates} inputs"),
            ApartError::DependedOn { apart, by, named } => {
                write!(f, "crate `{apart}` cannot be resolved apart: crate `{by}` depends on it (as `{named}`)")
            }
            ApartError::IsSysroot { apart } => write!(f, "crate `{apart}` cannot be resolved apart: it is a sysroot crate, which every crate reaches"),
            ApartError::Count { solved, given } => {
                write!(f, "the crates given to finish ({given}) are not the {solved} the others were resolved among")
            }
            ApartError::Differs { krate, what } => {
                write!(f, "crate `{krate}`: its {what} is not what the other crates were resolved under (only the texts of the crate apart may change)")
            }
        }
    }
}

impl core::error::Error for ApartError<'_> {}

impl<'a> From<NamesError<'a>> for ApartError<'a> {
    fn from(error: NamesError<'a>) -> ApartError<'a> {
        ApartError::Names(error)
    }
}

/// Every crate of a set resolved but one, and what that one's resolution needs of them.
pub struct Apart<'a> {
    /// The index of the crate left out.
    apart: usize,
    /// That crate as [`Apart::solve`] was given it: its graph facts are what the others were
    /// resolved under.
    stand_in: CrateInput<'a>,
    sysroot: Sysroot,
    /// By crate index; `None` at `apart` only.
    finished: AVec<'a, Option<Dep<'a>>>,
    whole: AVec<'a, Option<&'a CrateNames<'a>>>,
    by_prefix: HashMap<[u8; 8], usize, Alloc<'a>>,
    prepares: &'a Prepares<'a>,
    first: First,
    alloc: Bump<'a>,
}

impl<'a> Apart<'a> {
    /// The names of every crate of `crates` but `crates[apart]`, each after its dependencies,
    /// as [`resolve_crates_with`](crate::resolve::resolve_crates_with) resolves them.
    /// `crates[apart]` stands in for the crate [`Apart::finish`] is given: its name, key,
    /// edition, implicit crates, features, dependencies and cfg are read (the graph's node); its
    /// texts are not.
    pub fn solve<S: SourceTexts<'a>, P: Par>(
        crates: &[CrateInput<'a>],
        apart: usize,
        sysroot: Sysroot,
        sources: &S,
        par: &P,
        first: First,
        bump: Bump<'a>,
    ) -> Result<Apart<'a>, ApartError<'a>> {
        let alloc = bump.alloc();
        let Some(stand_in) = crates.get(apart).copied() else {
            return Err(ApartError::NoSuchCrate { apart, crates: crates.len() });
        };
        let is = |role: Option<CrateId>| role.is_some_and(|id| id.index() == apart);
        if is(sysroot.core) || is(sysroot.alloc) || is(sysroot.std) || is(sysroot.bstd) {
            return Err(ApartError::IsSysroot { apart: stand_in.name });
        }
        for c in crates {
            if let Some(&(named, _)) = c.deps.iter().find(|d| d.1 == apart) {
                return Err(ApartError::DependedOn { apart: stand_in.name, by: c.name, named });
            }
        }
        let graph = graph_of(crates, sysroot, alloc)?;
        let mut finished: AVec<'a, Option<Dep<'a>>> = AVec::with_capacity_in(crates.len(), alloc);
        finished.extend(crates.iter().map(|_| None));
        let mut whole: AVec<'a, Option<&'a CrateNames<'a>>> = AVec::with_capacity_in(crates.len(), alloc);
        whole.extend(crates.iter().map(|_| None));
        let mut by_prefix = HashMap::new_in(alloc);
        let prepares = Prepares::new_in(bump);
        for wave in graph.waves() {
            for &id in wave {
                if id.index() == apart {
                    continue;
                }
                let Some(input) = crates.get(id.index()) else {
                    continue;
                };
                let done = {
                    let built = Built { crates: &finished, by_prefix: &by_prefix, asked: None };
                    resolve_one::<S, P>(&graph, input, id, id.index(), &built, sources, par, first, prepares, &KeptExpansions::none(alloc), bump)?
                };
                let done: &'a CrateNames<'a> = bump.alloc_val(done);
                by_prefix.insert(done.key.prefix(), id.index());
                finished[id.index()] = Some(Dep::Whole(done));
                whole[id.index()] = Some(done);
            }
        }
        for (at, c) in whole.iter().enumerate() {
            if c.is_none() && at != apart {
                return Err(ApartError::Names(NamesError::NotResolved { krate: crates[at].name }));
            }
        }
        Ok(Apart { apart, stand_in, sysroot, finished, whole, by_prefix, prepares, first, alloc: bump })
    }

    /// The index of the crate left out.
    pub fn apart(&self) -> usize {
        self.apart
    }

    /// Every crate's names: the crate apart resolved now, from `sources`, over the others.
    /// `crates` are [`Apart::solve`]'s but for the texts of the crate apart (its `root`,
    /// `shown_from` and `script`); any other difference is refused by name. `self` is not
    /// changed: the call may be made again with other texts.
    pub fn finish<S: SourceTexts<'a>, P: Par>(&self, crates: &[CrateInput<'a>], sources: &S, par: &P) -> Result<Names<'a>, ApartError<'a>> {
        if crates.len() != self.whole.len() {
            return Err(ApartError::Count { solved: self.whole.len(), given: crates.len() });
        }
        let input = crates[self.apart];
        let was = &self.stand_in;
        let what = if input.name != was.name {
            Some("name")
        } else if input.key != was.key {
            Some("key")
        } else if input.edition != was.edition {
            Some("edition")
        } else if input.implicit != was.implicit {
            Some("implicit crates")
        } else if input.features != was.features {
            Some("features")
        } else if input.deps != was.deps {
            Some("dependencies")
        } else if input.cfg != was.cfg {
            Some("cfg")
        } else {
            None
        };
        if let Some(what) = what {
            return Err(ApartError::Differs { krate: input.name, what });
        }
        for (at, c) in self.whole.iter().enumerate() {
            if c.is_some_and(|c| c.key != crates[at].key) {
                return Err(ApartError::Differs { krate: crates[at].name, what: "key" });
            }
        }
        let alloc = self.alloc.alloc();
        let graph = graph_of(crates, self.sysroot, alloc)?;
        let id = CrateId(self.apart as u32);
        let done = {
            let built = Built { crates: &self.finished, by_prefix: &self.by_prefix, asked: None };
            resolve_one::<S, P>(&graph, &input, id, self.apart, &built, sources, par, self.first, self.prepares, &KeptExpansions::none(alloc), self.alloc)?
        };
        let done: &'a CrateNames<'a> = self.alloc.alloc_val(done);
        let mut all: AVec<'a, &'a CrateNames<'a>> = AVec::with_capacity_in(crates.len(), alloc);
        for (at, c) in self.whole.iter().enumerate() {
            match c {
                _ if at == self.apart => all.push(done),
                Some(c) => all.push(*c),
                None => return Err(ApartError::Names(NamesError::NotResolved { krate: crates[at].name })),
            }
        }
        Names::of_crates(crates, self.sysroot, &all, alloc).map_err(ApartError::Names)
    }
}
