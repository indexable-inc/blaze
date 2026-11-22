//! The request's variables and the configurations it allows.
//!
//! A variable is a boolean with a name: `feature:<package>/<feature>`, `cfg:<name>` or
//! `cfg:<name>="<value>"`. The space orders them by name, so two requests that share variables
//! order them alike, and gives each a [`VarKey`], the first 64 bits of BLAKE3's key derivation of
//! its name: what anything stored names a variable by (rule 25), never its place in one request.
//!
//! The targets are not a variable (the crate doc says why): each target's atoms are variables
//! of kind [`VarKind::TargetAtom`], and [`Space::valid_in`] holds the disjunction over the
//! targets of each one's full cube over all target atoms, so exactly the request's targets are
//! configurations. Cargo's feature implications (`a = ["b"]`) are `a -> b` in `valid` too.

#![blaze::sans_io]

use core::cmp::Ordering;
use core::fmt;
use core::ops::Range;

use blaze_int::{Int, idx};
use blaze_own::{AVec, Alloc, planted::Planted};

use crate::bdd::{Cond, PlantedBdd, Var};
use crate::error::{CondError, SpaceError};
use crate::plant::CondPlant;

/// The derivation context of a variable's key; a change of it is a change of every stored
/// condition, so it is dated.
const KEY_CONTEXT: &str = "blaze-cond 2026-11-15 cfg variable key";

/// A variable's stable identity: the first 64 bits of BLAKE3's key derivation of its name. What
/// a portable condition names a variable by.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct VarKey(pub u64);

impl VarKey {
    /// The key of a variable named `name` (`feature:tokio/rt`, `cfg:unix`).
    pub fn of(name: &[u8]) -> VarKey {
        VarKey::of_parts(&[name])
    }

    /// The key of the name that is `parts` end to end (BLAKE3's derivation streams, so this is
    /// [`VarKey::of`] of the concatenation).
    pub fn of_parts(parts: &[&[u8]]) -> VarKey {
        let mut h = blaze_blake3::Hasher::new_derive_key(KEY_CONTEXT);
        for p in parts {
            h.update(p);
        }
        let d = h.finalize();
        let mut first = [0u8; 8];
        first.copy_from_slice(&d[..8]);
        VarKey(u64::from_le_bytes(first))
    }
}

/// What a variable stands for.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub enum VarKind {
    /// A package's feature.
    Feature,
    /// A switch of the build: `test`, `debug_assertions`, a `--cfg`.
    Switch,
    /// An atom a target sets or not: `unix`, `target_os = "linux"`.
    TargetAtom,
}

/// A cfg atom: `name`, or `name = "value"`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Atom<'s> {
    pub name: &'s str,
    pub value: Option<&'s str>,
}

impl<'s> Atom<'s> {
    pub const fn flag(name: &'s str) -> Atom<'s> {
        Atom { name, value: None }
    }

    pub const fn pair(name: &'s str, value: &'s str) -> Atom<'s> {
        Atom { name, value: Some(value) }
    }
}

/// A name's bytes, in parts, so a lookup builds no string.
#[derive(Clone, Copy)]
pub(crate) struct NameParts<'s> {
    parts: [&'s [u8]; 5],
}

impl<'s> NameParts<'s> {
    pub(crate) fn feature(package: &'s str, feature: &'s str) -> NameParts<'s> {
        NameParts { parts: [b"feature:", package.as_bytes(), b"/", feature.as_bytes(), b""] }
    }

    pub(crate) fn atom(atom: Atom<'s>) -> NameParts<'s> {
        match atom.value {
            None => NameParts { parts: [b"cfg:", atom.name.as_bytes(), b"", b"", b""] },
            Some(v) => NameParts { parts: [b"cfg:", atom.name.as_bytes(), b"=\"", v.as_bytes(), b"\""] },
        }
    }

    /// The key of the name these parts spell.
    pub(crate) fn key(&self) -> VarKey {
        VarKey::of_parts(&self.parts)
    }

    /// The parts against `name`, as the concatenation would compare.
    fn cmp(&self, name: &[u8]) -> Ordering {
        let mut rest = name;
        for part in self.parts {
            let n = part.len().min(rest.len());
            match part[..n].cmp(&rest[..n]) {
                Ordering::Equal => {}
                other => return other,
            }
            if n < part.len() {
                // `name` ended inside this part.
                return Ordering::Greater;
            }
            rest = &rest[n..];
        }
        if rest.is_empty() { Ordering::Equal } else { Ordering::Less }
    }
}

#[derive(Clone)]
struct Entry {
    name: Range<u32>,
    kind: VarKind,
}

/// Builds a [`Space`]: declare the targets, features, implications and switches, then
/// [`build`](SpaceBuilder::build).
pub struct SpaceBuilder<'a> {
    pool: AVec<'a, u8>,
    entries: AVec<'a, Entry>,
    /// Each target: its name and its atoms' entries (a range of `target_atoms`).
    targets: AVec<'a, (Range<u32>, Range<u32>)>,
    target_atoms: AVec<'a, u32>,
    implications: AVec<'a, (u32, u32)>,
    alloc: Alloc<'a>,
}

impl<'a> SpaceBuilder<'a> {
    pub fn new_in(alloc: Alloc<'a>) -> SpaceBuilder<'a> {
        SpaceBuilder {
            pool: AVec::new_in(alloc),
            entries: AVec::new_in(alloc),
            targets: AVec::new_in(alloc),
            target_atoms: AVec::new_in(alloc),
            implications: AVec::new_in(alloc),
            alloc,
        }
    }

    fn text(&mut self, parts: &[&[u8]]) -> Result<Range<u32>, SpaceError> {
        let start = self.pool.len();
        for p in parts {
            self.pool.try_extend_from_slice(p)?;
        }
        let end = self.pool.len();
        let start32 = u32::try_from(start).map_err(|_| SpaceError::NameTooLong { len: end - start })?;
        let end32 = u32::try_from(end).map_err(|_| SpaceError::NameTooLong { len: end - start })?;
        Ok(start32..end32)
    }

    fn entry(&mut self, parts: NameParts<'_>, kind: VarKind) -> Result<u32, SpaceError> {
        let name = self.text(&parts.parts)?;
        let at = self.entries.len();
        self.entries.try_push(Entry { name, kind }).map_err(|r| SpaceError::Alloc(r.why))?;
        u32::try_from(at).map_err(|_| SpaceError::TooManyVars { vars: at })
    }

    /// A target and the atoms it sets (every other target atom is unset under it).
    pub fn target(&mut self, name: &str, atoms: &[Atom<'_>]) -> Result<(), SpaceError> {
        let name = self.text(&[name.as_bytes()])?;
        let start = self.target_atoms.len();
        for &atom in atoms {
            let e = self.entry(NameParts::atom(atom), VarKind::TargetAtom)?;
            self.target_atoms.try_push(e).map_err(|r| SpaceError::Alloc(r.why))?;
        }
        let end = self.target_atoms.len();
        let range = start.narrow_or_stop::<u32>()..end.narrow_or_stop::<u32>();
        self.targets.try_push((name, range)).map_err(|r| SpaceError::Alloc(r.why))?;
        Ok(())
    }

    /// A package's feature.
    pub fn feature(&mut self, package: &str, feature: &str) -> Result<(), SpaceError> {
        self.entry(NameParts::feature(package, feature), VarKind::Feature).map(|_| ())
    }

    /// `a` implies `b` (`[features] a = ["b"]`, or `a = ["dep/b"]` across packages); both are
    /// declared as features.
    pub fn implies(&mut self, a: (&str, &str), b: (&str, &str)) -> Result<(), SpaceError> {
        let ea = self.entry(NameParts::feature(a.0, a.1), VarKind::Feature)?;
        let eb = self.entry(NameParts::feature(b.0, b.1), VarKind::Feature)?;
        self.implications.try_push((ea, eb)).map_err(|r| SpaceError::Alloc(r.why))
    }

    /// A switch of the build: `test`, `debug_assertions`, a `--cfg name` or `--cfg name="v"`.
    pub fn switch(&mut self, atom: Atom<'_>) -> Result<(), SpaceError> {
        self.entry(NameParts::atom(atom), VarKind::Switch).map(|_| ())
    }

    /// The space: the variables sorted by name, one per distinct name, each keyed.
    pub fn build(self) -> Result<Space<'a>, SpaceError> {
        let SpaceBuilder { pool, entries, targets, target_atoms, implications, alloc } = self;
        // Entries sorted by name; a variable per distinct name.
        let mut sorted: AVec<'a, u32> = AVec::new_in(alloc);
        sorted.try_reserve_exact(entries.len())?;
        for i in 0..entries.len() {
            sorted.push(i.narrow_or_stop::<u32>());
        }
        let name = |e: &Entry| &pool[idx(e.name.start)..idx(e.name.end)];
        sorted.as_mut_slice().sort_unstable_by(|a, b| {
            name(&entries[idx(*a)]).cmp(name(&entries[idx(*b)])).then(a.cmp(b))
        });
        let mut vars: AVec<'a, VarInfo> = AVec::new_in(alloc);
        let mut var_of: AVec<'a, u16> = AVec::new_in(alloc);
        var_of.try_reserve_exact(entries.len())?;
        var_of.resize(entries.len(), 0);
        for &e in sorted.iter() {
            let entry = &entries[idx(e)];
            let same = vars.last().is_some_and(|last: &VarInfo| {
                &pool[idx(last.name.start)..idx(last.name.end)] == name(entry)
            });
            if same {
                let at = vars.len() - 1;
                if vars[at].kind != entry.kind {
                    return Err(SpaceError::KindConflict { at });
                }
            } else {
                if vars.len() >= usize::from(crate::MOST_VARS) {
                    return Err(SpaceError::TooManyVars { vars: vars.len() + 1 });
                }
                let key = VarKey::of(name(entry));
                vars.try_push(VarInfo { name: entry.name.clone(), kind: entry.kind, key })
                    .map_err(|r| SpaceError::Alloc(r.why))?;
            }
            var_of[idx(e)] = (vars.len() - 1).narrow_or_stop::<u16>();
        }
        // Keys, for import; two names with one key are refused (rule 21).
        let mut by_key: AVec<'a, (VarKey, u16)> = AVec::new_in(alloc);
        by_key.try_reserve_exact(vars.len())?;
        for (i, v) in vars.iter().enumerate() {
            by_key.push((v.key, i.narrow_or_stop::<u16>()));
        }
        by_key.as_mut_slice().sort_unstable();
        for pair in by_key.windows(2) {
            if pair[0].0 == pair[1].0 {
                return Err(SpaceError::KeyCollision { a: usize::from(pair[0].1), b: usize::from(pair[1].1) });
            }
        }
        // Targets: each one's atoms as sorted, distinct variables; names distinct.
        let mut cubes: AVec<'a, u16> = AVec::new_in(alloc);
        let mut target_list: AVec<'a, (Range<u32>, Range<u32>)> = AVec::new_in(alloc);
        for (t, (tname, atoms)) in targets.iter().enumerate() {
            let start = cubes.len();
            for &e in &target_atoms[idx(atoms.start)..idx(atoms.end)] {
                cubes.try_push(var_of[idx(e)]).map_err(|r| SpaceError::Alloc(r.why))?;
            }
            cubes.as_mut_slice()[start..].sort_unstable();
            let mut keep = start;
            for i in start..cubes.len() {
                if keep == start || cubes[keep - 1] != cubes[i] {
                    cubes[keep] = cubes[i];
                    keep += 1;
                }
            }
            cubes.truncate(keep);
            let this = &pool[idx(tname.start)..idx(tname.end)];
            if target_list.iter().any(|(n, _)| &pool[idx(n.start)..idx(n.end)] == this) {
                return Err(SpaceError::DuplicateTarget { at: t });
            }
            target_list
                .try_push((tname.clone(), start.narrow_or_stop::<u32>()..cubes.len().narrow_or_stop::<u32>()))
                .map_err(|r| SpaceError::Alloc(r.why))?;
        }
        let mut imps: AVec<'a, (u16, u16)> = AVec::new_in(alloc);
        for &(a, b) in implications.iter() {
            imps.try_push((var_of[idx(a)], var_of[idx(b)])).map_err(|r| SpaceError::Alloc(r.why))?;
        }
        imps.as_mut_slice().sort_unstable();
        Ok(Space { pool, vars, by_key, targets: target_list, cubes, implications: imps })
    }
}

#[derive(Clone)]
struct VarInfo {
    name: Range<u32>,
    kind: VarKind,
    key: VarKey,
}

/// A request's variables, in their stable order, and the configurations it allows.
pub struct Space<'a> {
    pool: AVec<'a, u8>,
    vars: AVec<'a, VarInfo>,
    by_key: AVec<'a, (VarKey, u16)>,
    /// Each target's name and its set atoms (a range of `cubes`, sorted variables).
    targets: AVec<'a, (Range<u32>, Range<u32>)>,
    cubes: AVec<'a, u16>,
    implications: AVec<'a, (u16, u16)>,
}

impl Space<'_> {
    /// How many variables.
    pub fn len(&self) -> usize {
        self.vars.len()
    }

    pub fn targets(&self) -> usize {
        self.targets.len()
    }

    /// A variable's name, as the space spells it.
    pub fn name(&self, var: Var) -> &str {
        let r = &self.vars[usize::from(var.index())].name;
        // The pool holds only what `&str`s put there, cut at their ends.
        core::str::from_utf8(&self.pool[idx(r.start)..idx(r.end)]).unwrap_or("?")
    }

    pub fn kind(&self, var: Var) -> VarKind {
        self.vars[usize::from(var.index())].kind
    }

    pub fn key(&self, var: Var) -> VarKey {
        self.vars[usize::from(var.index())].key
    }

    /// The variable of a key, if the space holds it.
    pub fn by_key(&self, key: VarKey) -> Option<Var> {
        self.by_key.binary_search_by_key(&key, |(k, _)| *k).ok().map(|at| Var::new(self.by_key[at].1))
    }

    fn find(&self, parts: NameParts<'_>) -> Option<Var> {
        self.vars
            .binary_search_by(|v| parts.cmp(&self.pool[idx(v.name.start)..idx(v.name.end)]).reverse())
            .ok()
            .map(|at| Var::new(at.narrow_or_stop::<u16>()))
    }

    /// The variable of a cfg atom (`unix`, `target_os = "linux"`, `test`), if the space holds
    /// it. `feature = "x"` is a feature of `package`.
    pub fn atom(&self, package: &str, atom: Atom<'_>) -> Option<Var> {
        match (atom.name, atom.value) {
            ("feature", Some(feature)) => self.find(NameParts::feature(package, feature)),
            _ => self.find(NameParts::atom(atom)),
        }
    }

    /// A package's feature, if the space holds it.
    pub fn feature(&self, package: &str, feature: &str) -> Option<Var> {
        self.find(NameParts::feature(package, feature))
    }

    /// The configurations the request allows, in `bdd`: one of its targets (each a full cube
    /// over the target atoms), every implication between features. `TRUE` with no targets and
    /// no implications.
    pub fn valid_in(&self, bdd: &mut PlantedBdd<'_, impl Planted<CondPlant>>) -> Result<Cond, CondError> {
        let mut atoms: AVec<'_, u16> = AVec::new_in(bdd.alloc());
        for (i, v) in self.vars.iter().enumerate() {
            if v.kind == VarKind::TargetAtom {
                atoms.try_push(i.narrow_or_stop::<u16>()).map_err(|r| CondError::Alloc(r.why))?;
            }
        }
        let mut targets = if self.targets.is_empty() { Cond::TRUE } else { Cond::FALSE };
        for (_, set) in self.targets.iter() {
            let set = &self.cubes[idx(set.start)..idx(set.end)];
            // The cube from the last variable up: one node per target atom.
            let mut cube = Cond::TRUE;
            for &v in atoms.iter().rev() {
                cube = if set.binary_search(&v).is_ok() {
                    bdd.mk(v, Cond::FALSE, cube)?
                } else {
                    bdd.mk(v, cube, Cond::FALSE)?
                };
            }
            targets = bdd.or(targets, cube)?;
        }
        let mut valid = targets;
        for &(a, b) in self.implications.iter() {
            let a = bdd.var(Var::new(a))?;
            let b = bdd.var(Var::new(b))?;
            let imp = bdd.or(a.not(), b)?;
            valid = bdd.and(valid, imp)?;
        }
        Ok(valid)
    }

    /// The target a full witness names: the one whose set atoms are exactly the witness's set
    /// target atoms (an atom not in the witness is unset).
    pub fn target_of(&self, witness: &[(Var, bool)]) -> Option<&str> {
        let set = |v: u16| witness.iter().any(|(w, on)| w.index() == v && *on);
        self.targets.iter().find_map(|(name, atoms)| {
            let atoms = &self.cubes[idx(atoms.start)..idx(atoms.end)];
            let all_set = atoms.iter().all(|&v| set(v));
            let only = witness
                .iter()
                .filter(|(v, on)| *on && self.kind(*v) == VarKind::TargetAtom)
                .all(|(v, _)| atoms.binary_search(&v.index()).is_ok());
            (all_set && only).then(|| core::str::from_utf8(&self.pool[idx(name.start)..idx(name.end)]).unwrap_or("?"))
        })
    }

    /// A witness as a finding's text: "feature `tokio/rt` on, `test` off, target
    /// aarch64-apple-darwin". Target atoms are named through the target.
    pub fn describe(&self, witness: &[(Var, bool)], out: &mut impl fmt::Write) -> fmt::Result {
        let mut first = true;
        for &(v, on) in witness {
            let kind = self.kind(v);
            if kind == VarKind::TargetAtom {
                continue;
            }
            if !first {
                out.write_str(", ")?;
            }
            first = false;
            let name = self.name(v);
            let state = if on { "on" } else { "off" };
            match kind {
                VarKind::Feature => write!(out, "feature `{}` {state}", name.trim_start_matches("feature:"))?,
                _ => write!(out, "`{}` {state}", name.trim_start_matches("cfg:"))?,
            }
        }
        if let Some(target) = self.target_of(witness) {
            if !first {
                out.write_str(", ")?;
            }
            write!(out, "target {target}")?;
        }
        Ok(())
    }
}
