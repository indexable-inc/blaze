//! Folding functions that differ only in constants (#1292), after Relax's `merge_similar`.
//!
//! Monomorphic copies of one generic body are often the same IR except for a few constants:
//! element sizes, alignments, the addresses of the functions, statics and vtables they name.
//! A backend gives each prepared function with those constants masked ([`Masked`]);
//! [`groups`] puts together the functions whose masked IR and settings are equal, and a group
//! whose members differ in few enough constants, and save more than their thunks cost,
//! becomes one body that takes the differing constants as trailing parameters, with each
//! member a thunk that calls it with its own ([`Merged`]).
//!
//! Every member becomes a thunk: callers name members by their keys and are never rewritten,
//! so a member's address, key and signature stay what they were.
//!
//! Dev builds only (Cranelift units with [`crate::settings::Spec::fold`]): a varying function
//! becomes an indirect call in the body.
//!
//! What a bucket is, a masked IR without its constants' values ([`Form`]), is hash-consed
//! (docs/v2/plan/lanes/fastcompile/hashcons.md): its content ID is computed once, when it is
//! interned, and [`groups`] compares those IDs without hashing any IR.

#![forbid(unsafe_code)]

use blaze_hashcons::{Consed, HashCons, Interned};
use blaze_ids::{ContentId, Domain, Hasher, radix};
use serde::{Deserialize, Serialize};

use crate::{InstanceKey, Settings, Target, backend::Emitted, content::target_id, template::Refs};

/// A constant of a function's IR that can become a parameter.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub enum Constant {
    /// An integer immediate of `bits` bits, zero-extended.
    Int { bits: u8, value: u128 },
    /// A float immediate's bits.
    F32(u32),
    F64(u64),
    /// The address of the `n`th of the function's references (a function or a static): whether
    /// two members' are equal is whether their targets are, not their places.
    Place(u32),
    /// The address of one of the function's own data objects (a vtable, a type's info, a
    /// constant), by its content with what it points at by target. A varying one is made by
    /// each thunk, which then has the member's object among its data.
    Data(ContentId),
}

/// A prepared function's IR with its constants masked: what [`groups`] compares.
#[derive(PartialEq, Eq, Hash, Debug)]
pub struct Masked {
    /// The IR's canonical form with its constants left out, or a content hash of it (the
    /// Cranelift backend's): equal for two functions exactly when their masked IR is.
    pub shape: Vec<u8>,
    /// Its constants, in the order the shape names them.
    pub constants: Vec<Constant>,
    /// Its number of instructions.
    pub size: u32,
}

impl Masked {
    /// Its form, which [`groups`] buckets by, and its constants' values.
    pub fn into_parts(self) -> (Form, Vec<Constant>) {
        // alloc: once per function the daemon first folds, two bytes a constant
        let kinds = self.constants.iter().flat_map(kind).collect();
        (Form { shape: self.shape, size: self.size, kinds }, self.constants)
    }

    /// [`Masked::into_parts`]'s form, copying the shape.
    pub fn form(&self) -> Form {
        // alloc: two bytes a constant, for a caller that keeps the masked IR
        let kinds = self.constants.iter().flat_map(kind).collect();
        Form { shape: self.shape.clone(), size: self.size, kinds } // clone: as above, the shape
    }
}

/// A masked IR as [`groups`] buckets it: its shape, size and its constants' kinds, without
/// their values. Two functions of one form differ only in constants.
///
/// Hash-consed under [`MASKED_DOMAIN`], fed what `groups` hashed before it was: its content
/// ID is the bucket every group's shape and key are made from.
#[derive(PartialEq, Eq, Hash, Debug)]
pub struct Form {
    shape: Vec<u8>,
    size: u32,
    /// [`kind`] of each constant, in order: two bytes each.
    kinds: Vec<u8>,
}

/// The forms of a build, or of every build of a daemon.
pub type Forms = HashCons<Form>;

#[cfg(test)]
thread_local! {
    /// How many forms this thread has hashed: what the tests count.
    pub(crate) static HASHED: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

impl Consed for Form {
    fn domain() -> &'static Domain {
        static MASKED: Domain = Domain::new(MASKED_DOMAIN);
        &MASKED
    }

    fn hash_content(&self, hasher: &mut Hasher) {
        #[cfg(test)]
        HASHED.with(|it| it.set(it.get() + 1));
        hasher.part(&self.shape).update(&self.size.to_le_bytes());
        hasher.update(&((self.kinds.len() / 2) as u64).to_le_bytes()).update(&self.kinds);
    }
}

/// A function as [`groups`] takes it: its masked IR's interned form and its constants' values,
/// in the order the shape names them.
#[derive(Clone, Copy, Debug)]
pub struct Masking<'a> {
    pub form: Interned<'a, Form>,
    pub constants: &'a [Constant],
}

impl<'a> Masking<'a> {
    /// `masked` with its form interned in `forms`: hashed here.
    pub fn of(forms: &'a Forms, masked: &'a Masked) -> Masking<'a> {
        Masking { form: forms.intern(masked.form()), constants: &masked.constants }
    }
}

/// Functions that fold into one body.
#[derive(PartialEq, Eq, Debug)]
pub struct Group {
    /// The merged body's shape: the masked IR, every constant equal for all members with its
    /// value (a place by its target), and which parameter each varying one reads.
    pub shape: ContentId,
    pub settings: Settings,
    /// The indices among [`Masked::constants`] whose values differ between members, in order.
    pub varying: Vec<u32>,
    /// Sorted by key.
    pub members: Vec<InstanceKey>,
    /// `constants[i]` are member `i`'s values of `varying`, in its order.
    pub constants: Vec<Vec<Constant>>,
}

pub const FOLDED_DOMAIN: &str = "blaze v2 folded 1";
const MASKED_DOMAIN: &str = "blaze v2 masked 2";
const FOLD_SHAPE_DOMAIN: &str = "blaze v2 fold shape 1";

impl Group {
    /// The merged body's key: from what the body is, so one group anywhere is one body.
    pub fn key(&self) -> InstanceKey {
        let mut hasher = Hasher::new(FOLDED_DOMAIN);
        hasher.id(&self.shape).id(&self.settings.0);
        hasher.update(&(self.varying.len() as u64).to_le_bytes());
        for at in &self.varying {
            hasher.update(&at.to_le_bytes());
        }
        InstanceKey(hasher.finish())
    }

    /// The parameter of the merged body each of `varying` reads, and how many parameters
    /// there are: varying constants whose values are equal for every member are one.
    pub fn parameters(&self) -> (Vec<u32>, u32) {
        let mut reads: Vec<u32> = Vec::with_capacity(self.varying.len());
        let mut params = 0;
        for at in 0..self.varying.len() {
            let equal = |other: usize| self.constants.iter().all(|it| it[other] == it[at]);
            let param = match (0..at).find(|&other| equal(other)) {
                Some(other) => reads[other],
                None => {
                    params += 1;
                    params - 1
                }
            };
            reads.push(param);
        }
        (reads, params)
    }

    /// The references of member `member`'s thunk: the member's own, then the merged body,
    /// [`Target::Direct`] of [`Group::key`], which the thunk calls.
    pub fn thunk_refs(&self, own: &Refs) -> Vec<Target> {
        let mut refs = Vec::with_capacity(own.len() + 1);
        refs.extend_from_slice(own);
        refs.push(Target::Direct(self.key()));
        refs
    }
}

/// What [`groups`] makes a group of.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Limits {
    /// The most parameters a merged body takes.
    pub max_params: u32,
    /// The fewest members of a group.
    pub min_members: u32,
    /// What a thunk costs, in instructions, besides one per constant it passes: Relax's
    /// 6 bytes (a call and a return) at its 3 bytes an instruction.
    pub thunk_size: u32,
}

impl Default for Limits {
    fn default() -> Limits {
        Limits { max_params: 16, min_members: 2, thunk_size: 2 }
    }
}

/// A group compiled: one body and a thunk per member.
///
/// `body.function.key` is [`Group::key`]; its references are the first member's (it names
/// only places whose targets are equal for every member, the varying ones being parameters).
/// Thunk `i`'s key is `group.members[i]`; its references are [`Group::thunk_refs`] of member
/// `i`'s: its own, then [`Target::Direct`] of the body. The driver makes their templates
/// against exactly those. A thunk's data are the member's objects of the varying
/// [`Constant::Data`]; the body's, the objects equal for every member.
#[derive(PartialEq, Eq, Debug)]
pub struct Merged {
    pub body: Emitted,
    pub thunks: Vec<Emitted>,
}

/// A constant as members are compared by: a place by its target. Targets are compared
/// whole, equal exactly when their [`target_id`]s are, which is hashed only into a group's
/// shape.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Cell<'a> {
    Value(Constant),
    Target(&'a Target),
}

fn cell(key: InstanceKey, constant: Constant, refs: &Refs) -> Cell<'_> {
    match constant {
        Constant::Place(at) => {
            let Some(target) = refs.get(at as usize) else {
                panic!("{key:?}'s masked IR names place {at} of its {} references", refs.len())
            };
            Cell::Target(target)
        }
        other => Cell::Value(other),
    }
}

fn cell_into(hasher: &mut Hasher, cell: &Cell<'_>) {
    match *cell {
        Cell::Value(Constant::Int { bits, value }) => {
            hasher.update(&[0, bits]).update(&value.to_le_bytes())
        }
        Cell::Value(Constant::F32(bits)) => hasher.update(&[1]).update(&bits.to_le_bytes()),
        Cell::Value(Constant::F64(bits)) => hasher.update(&[2]).update(&bits.to_le_bytes()),
        Cell::Value(Constant::Place(at)) => hasher.update(&[3]).update(&at.to_le_bytes()),
        Cell::Value(Constant::Data(id)) => hasher.update(&[6]).id(&id),
        Cell::Target(target) => hasher.update(&[4]).id(&target_id(target)),
    };
}

/// What a constant is, without its value: members whose constants differ in it never group.
fn kind(constant: &Constant) -> [u8; 2] {
    match *constant {
        Constant::Int { bits, .. } => [0, bits],
        Constant::F32(_) => [1, 0],
        Constant::F64(_) => [2, 0],
        Constant::Place(_) => [3, 0],
        Constant::Data(_) => [4, 0],
    }
}

/// A candidate of [`groups`], with its bucket: its settings and `masked_id`, its [`Form`]'s
/// content ID.
#[derive(Clone, Copy)]
struct Candidate<'a> {
    key: InstanceKey,
    settings: Settings,
    masked_id: ContentId,
    size: u32,
    constants: &'a [Constant],
    refs: &'a Refs,
}

/// One bucket's first-fit split, its buffers reused for the next bucket.
///
/// A part is its first member and, for each of the bucket's `n` constants, whether it varies
/// among the part's members and its class: two constants of one class have equal values in
/// every member of the part. A part's parameters are its varying constants' classes, so
/// whether a member fits is one pass over its constants, not a plan of the part with it.
#[derive(Default)]
struct Split<'a> {
    /// Sorted and deduplicated by key.
    members: Vec<Candidate<'a>>,
    /// `cells[m * n + at]`: member `m`'s constant `at` as members are compared.
    cells: Vec<Cell<'a>>,
    /// Per member, its part.
    part_of: Vec<u32>,
    /// Per part, its first member.
    firsts: Vec<u32>,
    /// `varying[p * n + at]` and `classes[p * n + at]` of part `p`.
    varying: Vec<bool>,
    classes: Vec<u32>,
    /// The constants that start a new parameter in [`Split::params_with`].
    reps: Vec<u32>,
    positions: Vec<u32>,
    /// Per part, its members in bucket order.
    parts: radix::Csr<u32>,
    param_of: Vec<u32>,
    /// Per varying constant of the part at hand, the parameter it reads.
    reads: Vec<u32>,
}

impl<'a> Split<'a> {
    /// The groups of `self.members`, one bucket, into `out`.
    fn groups(&mut self, limits: &Limits, out: &mut Vec<Group>) {
        let n = self.members[0].constants.len();
        self.cells.clear();
        for member in &self.members {
            let cells = member.constants.iter().map(|&it| cell(member.key, it, member.refs));
            self.cells.extend(cells);
        }
        self.part_of.clear();
        self.firsts.clear();
        self.varying.clear();
        self.classes.clear();
        for member in 0..self.members.len() {
            let mut fits = None;
            for part in 0..self.firsts.len() {
                if self.params_with(part, member, n, limits.max_params) <= limits.max_params {
                    fits = Some(part);
                    break;
                }
            }
            let part = fits.unwrap_or_else(|| {
                self.firsts.push(member as u32);
                self.varying.resize(self.varying.len() + n, false);
                self.classes.resize(self.classes.len() + n, 0);
                self.firsts.len() - 1
            });
            self.add(part, member, n);
            self.part_of.push(part as u32);
        }

        let parts = self.firsts.len();
        self.parts.rebuild(
            parts,
            self.part_of.iter().enumerate().map(|(member, &part)| (part, member as u32)),
            &mut self.positions,
        );

        for part in 0..parts {
            let members = self.parts.row(part);
            if members.len() < limits.min_members as usize {
                continue;
            }
            let (varying_of, classes) = (&self.varying[part * n..][..n], &self.classes[part * n..][..n]);
            self.param_of.clear();
            self.param_of.resize(n, u32::MAX);
            // alloc: the group's own list, moved into it
            let (mut varying, mut params) = (Vec::new(), 0u32);
            self.reads.clear();
            for at in 0..n {
                if !varying_of[at] {
                    continue;
                }
                let param = &mut self.param_of[classes[at] as usize];
                if *param == u32::MAX {
                    *param = params;
                    params += 1;
                }
                varying.push(at as u32);
                self.reads.push(*param);
            }
            let first = self.members[members[0] as usize];
            if params == 0 || !profitable(first.size, members.len(), params, limits) {
                continue;
            }
            let mut hasher = Hasher::new(FOLD_SHAPE_DOMAIN);
            hasher.id(&first.masked_id);
            let mut reads_of = varying.iter().zip(&self.reads).peekable();
            let first_cells = &self.cells[members[0] as usize * n..][..n];
            for (at, it) in first_cells.iter().enumerate() {
                if let Some((_, param)) = reads_of.next_if(|(v, _)| **v as usize == at) {
                    hasher.update(&[5]).update(&param.to_le_bytes());
                } else {
                    cell_into(&mut hasher, it);
                }
            }
            let constants = members
                .iter()
                .map(|&it| {
                    let constants = self.members[it as usize].constants;
                    // alloc: the group's own list, exact size
                    varying.iter().map(|&at| constants[at as usize]).collect()
                })
                // alloc: the group's own list, exact size
                .collect();
            out.push(Group {
                shape: hasher.finish(),
                settings: first.settings,
                varying,
                // alloc: the group's own list, exact size
                members: members.iter().map(|&it| self.members[it as usize].key).collect(),
                constants,
            });
        }
    }

    /// How many parameters `part` would take with `member` in it; past `max`, any count
    /// above it.
    fn params_with(&mut self, part: usize, member: usize, n: usize, max: u32) -> u32 {
        let first = self.firsts[part] as usize;
        let varying = &self.varying[part * n..][..n];
        let classes = &self.classes[part * n..][..n];
        let (mine, firsts) = (&self.cells[member * n..][..n], &self.cells[first * n..][..n]);
        let values = self.members[member].constants;
        self.reps.clear();
        for at in 0..n {
            if !varying[at] && mine[at] == firsts[at] {
                continue;
            }
            let class = classes[at];
            let known = self.reps.iter().any(|&rep| {
                let rep = rep as usize;
                classes[rep] == class && values[rep] == values[at]
            });
            if !known {
                self.reps.push(at as u32);
                if self.reps.len() > max as usize {
                    break;
                }
            }
        }
        self.reps.len() as u32
    }

    /// Puts `member` in `part`: the constants it makes vary, and the classes split by its
    /// values.
    fn add(&mut self, part: usize, member: usize, n: usize) {
        let first = self.firsts[part] as usize;
        for at in 0..n {
            if self.cells[member * n + at] != self.cells[first * n + at] {
                self.varying[part * n + at] = true;
            }
        }
        let values = self.members[member].constants;
        let classes = &mut self.classes[part * n..][..n];
        self.positions.clear();
        self.positions.extend(0..n as u32);
        self.positions.sort_unstable_by(|&a, &b| {
            let (a, b) = (a as usize, b as usize);
            (classes[a], values[a]).cmp(&(classes[b], values[b]))
        });
        let mut class = 0;
        let mut last = None;
        for &at in &self.positions {
            let at = at as usize;
            let was = (classes[at], values[at]);
            if last.is_some_and(|last| last != was) {
                class += 1;
            }
            last = Some(was);
            classes[at] = class;
        }
    }
}

/// Relax's test: the bodies folded away save more than the thunks cost, and the body grows
/// by an eighth for the parameters it keeps in registers.
fn profitable(size: u32, members: usize, params: u32, limits: &Limits) -> bool {
    let (size, members) = (u64::from(size), members as u64);
    let saved = size * (members - 1);
    let thunks = members * (u64::from(limits.thunk_size) + u64::from(params));
    let body = if params == 0 { 0 } else { size / 8 };
    saved > thunks + body
}

/// The functions among `candidates` that fold, each group with its members sorted by key;
/// the same groups in the same order whatever the order of `candidates`.
///
/// Members of one group have equal settings, masked IR and references' count and kinds, and
/// equal values of every
/// constant but `varying` (a place by its target: places whose targets are equal for every
/// member are not varying). A group's members differ in at least one constant (functions
/// equal in all are code dedupe's and the memo's) and in at most [`Limits::max_params`]
/// parameters, and pass [`profitable`]. Functions of one masked IR whose constants differ in
/// too many are split, each taking the first group, in key order, it fits.
///
/// Hashes no IR: a candidate's bucket is its form's content ID, computed when it was
/// interned, with its references' count and kinds ([`with_refs`]).
pub fn groups(
    candidates: &[(InstanceKey, Settings, Masking<'_>, &Refs)],
    limits: &Limits,
) -> Vec<Group> {
    let mut all: Vec<Candidate<'_>> = Vec::with_capacity(candidates.len());
    for &(key, settings, masked, refs) in candidates {
        let form = masked.form.get();
        if form.kinds.len() != 2 * masked.constants.len() {
            panic!(
                "{key:?}'s form has {} constants and its masking {}",
                form.kinds.len() / 2,
                masked.constants.len()
            );
        }
        for (constant, of_form) in masked.constants.iter().zip(form.kinds.chunks_exact(2)) {
            if let Constant::Place(at) = *constant
                && at as usize >= refs.len()
            {
                panic!("{key:?}'s masked IR names place {at} of its {} references", refs.len());
            }
            if kind(constant)[..] != *of_form {
                panic!("{key:?}'s constant {constant:?} is not of its form's kind {of_form:?}");
            }
        }
        all.push(Candidate {
            key,
            settings,
            masked_id: with_refs(masked.form.content(), refs),
            size: form.size,
            constants: masked.constants,
            refs,
        });
    }
    // One sort makes every bucket a run, its members in key order. Stable, so of candidates
    // given twice under one key in one bucket, the first given stays.
    // alloc: one exact-size column per build
    let mut order: Vec<u32> = (0..all.len() as u32).collect();
    order.sort_by(|&a, &b| {
        let (a, b) = (&all[a as usize], &all[b as usize]);
        (a.settings, a.masked_id, a.key).cmp(&(b.settings, b.masked_id, b.key))
    });
    let mut split = Split::default();
    // alloc: the build's answer, once per build
    let mut groups = Vec::new();
    let mut start = 0;
    while start < order.len() {
        let head = all[order[start] as usize];
        let len = order[start..]
            .iter()
            .position(|&it| {
                let it = &all[it as usize];
                it.settings != head.settings || it.masked_id != head.masked_id
            })
            .unwrap_or(order.len() - start);
        let bucket = &order[start..start + len];
        start += len;
        if bucket.len() < limits.min_members as usize {
            continue;
        }
        split.members.clear();
        for &it in bucket {
            let it = all[it as usize];
            if split.members.last().is_none_or(|last| last.key != it.key) {
                split.members.push(it);
            }
        }
        if split.members.len() >= limits.min_members as usize {
            split.groups(limits, &mut groups);
        }
    }
    groups.sort_by_key(|it| it.members[0]);
    groups
}

const FOLD_REFS_DOMAIN: &str = "blaze v2 fold references 1";

/// A masked form's content ID with how many references its function has and the kind of each:
/// the merged body is made against the first member's references and kept under the group's
/// key, and each thunk against its member's, so a group's members, and every group of one key,
/// have references alike. A masked form leaves out the places it names, and a reference no
/// instruction names is in no form.
fn with_refs(form: ContentId, refs: &Refs) -> ContentId {
    let mut hasher = Hasher::new(FOLD_REFS_DOMAIN);
    hasher.id(&form).update(&(refs.len() as u64).to_le_bytes());
    for target in refs {
        hasher.update(&[crate::driver::place_kind(target)]);
    }
    hasher.finish()
}

#[cfg(test)]
mod tests {
    use blaze_ids::rng::SplitMix64 as Rng;

    use super::*;

    fn key(n: u8) -> InstanceKey {
        InstanceKey(ContentId([n; 32]))
    }

    fn settings(n: u8) -> Settings {
        Settings(ContentId([n; 32]))
    }

    fn int(value: u128) -> Constant {
        Constant::Int { bits: 64, value }
    }

    /// `Vec<T>::grow` with `T`'s size and alignment, calling place 0 and taking place 1's
    /// address.
    fn grow(size: u128, align: u128) -> Masked {
        Masked {
            shape: b"block0: v0 = iconst #0; v1 = iconst #1; call #2; v2 = func_addr #3".to_vec(),
            constants: vec![int(size), int(align), Constant::Place(0), Constant::Place(1)],
            size: 400,
        }
    }

    fn refs(call: u8, addr: u8) -> Vec<Target> {
        vec![Target::Function(key(call)), Target::Direct(key(addr))]
    }

    /// The forms of every test, as a daemon's are of every build.
    fn masking(masked: &Masked) -> Masking<'_> {
        static FORMS: std::sync::LazyLock<Forms> = std::sync::LazyLock::new(Forms::new);
        Masking::of(&FORMS, masked)
    }

    fn of<'a>(
        items: &'a [(InstanceKey, Masked, Vec<Target>)],
    ) -> Vec<(InstanceKey, Settings, Masking<'a>, &'a Refs)> {
        items.iter().map(|(k, m, r)| (*k, settings(1), masking(m), r.as_slice())).collect()
    }

    /// 3 has one reference more than 1 and 2, which no instruction names: the merged body is
    /// made against the first member's references and kept under the group's key, so 3 is in
    /// no group with them, and a group of one key always has references alike.
    #[test]
    fn functions_of_other_references_do_not_fold_together() {
        let mut more = refs(9, 10);
        more.push(Target::Extern("memcpy".to_owned()));
        let items = [(key(1), grow(4, 4), refs(9, 10)), (key(2), grow(8, 8), refs(9, 10)), (key(3), grow(16, 8), more)];
        let groups = groups(&of(&items), &Limits::default());
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].members, vec![key(1), key(2)]);

        let alike = [(key(1), grow(4, 4), refs(9, 10)), (key(2), grow(8, 8), refs(9, 10))];
        assert_eq!(groups[0].key(), super::groups(&of(&alike), &Limits::default())[0].key());
    }

    #[test]
    fn element_sizes_fold() {
        let items = [
            (key(2), grow(8, 8), refs(9, 10)),
            (key(1), grow(4, 4), refs(9, 10)),
            (key(3), grow(16, 8), refs(9, 10)),
        ];
        let groups = groups(&of(&items), &Limits::default());
        assert_eq!(groups.len(), 1);
        let group = &groups[0];
        assert_eq!(group.members, vec![key(1), key(2), key(3)]);
        assert_eq!(group.varying, vec![0, 1]);
        let want = vec![vec![int(4), int(4)], vec![int(8), int(8)], vec![int(16), int(8)]];
        assert_eq!(group.constants, want);
        assert_eq!(group.parameters(), (vec![0, 1], 2));
        assert_ne!(group.key(), key(1));
    }

    #[test]
    fn other_control_flow_does_not_fold() {
        let mut other = grow(4, 4);
        other.shape = b"block0: v0 = iconst #0; brif v0, block1, block2; ...".to_vec();
        let items = [(key(1), grow(8, 8), refs(9, 10)), (key(2), other, refs(9, 10))];
        assert!(groups(&of(&items), &Limits::default()).is_empty());
    }

    #[test]
    fn other_settings_do_not_fold() {
        let (a, b) = (grow(8, 8), grow(4, 4));
        let (ra, rb) = (refs(9, 10), refs(9, 10));
        let candidates =
            [(key(1), settings(1), masking(&a), &ra[..]), (key(2), settings(2), masking(&b), &rb[..])];
        assert!(groups(&candidates, &Limits::default()).is_empty());
    }

    #[test]
    fn a_place_varies_only_when_its_targets_do() {
        // The same targets at other places.
        let mut swapped = grow(4, 8);
        swapped.constants[2] = Constant::Place(1);
        swapped.constants[3] = Constant::Place(0);
        let swapped_refs = vec![Target::Direct(key(10)), Target::Function(key(9))];
        let items = [(key(1), grow(8, 8), refs(9, 10)), (key(2), swapped, swapped_refs)];
        let groups_same = groups(&of(&items), &Limits::default());
        assert_eq!(groups_same[0].varying, vec![0]);

        let items = [(key(1), grow(8, 8), refs(9, 10)), (key(2), grow(4, 8), refs(11, 10))];
        let groups_other = groups(&of(&items), &Limits::default());
        assert_eq!(groups_other[0].varying, vec![0, 2]);
        assert_eq!(groups_other[0].constants[1], vec![int(4), Constant::Place(0)]);
        assert_ne!(groups_same[0].key(), groups_other[0].key());
    }

    #[test]
    fn a_data_object_varies_by_content() {
        let vtable = |n| {
            let mut it = grow(8, 8);
            it.constants.push(Constant::Data(ContentId([n; 32])));
            it
        };
        let items = [(key(1), vtable(1), refs(9, 10)), (key(2), vtable(2), refs(9, 10))];
        let group = &groups(&of(&items), &Limits::default())[0];
        assert_eq!(group.varying, vec![4]);
        assert_eq!(group.constants[1], vec![Constant::Data(ContentId([2; 32]))]);
        let items = [(key(1), vtable(1), refs(9, 10)), (key(2), vtable(1), refs(9, 10))];
        assert!(groups(&of(&items), &Limits::default()).is_empty());
    }

    #[test]
    fn equal_constants_alone_are_not_folded() {
        let items = [(key(1), grow(8, 8), refs(9, 10)), (key(2), grow(8, 8), refs(9, 10))];
        assert!(groups(&of(&items), &Limits::default()).is_empty());
    }

    #[test]
    fn a_fixed_target_is_in_the_key() {
        let a = [(key(1), grow(8, 8), refs(9, 10)), (key(2), grow(4, 8), refs(9, 10))];
        let b = [(key(1), grow(8, 8), refs(12, 10)), (key(2), grow(4, 8), refs(12, 10))];
        let (a, b) = (groups(&of(&a), &Limits::default()), groups(&of(&b), &Limits::default()));
        assert_eq!(a[0].varying, b[0].varying);
        assert_ne!(a[0].key(), b[0].key());
    }

    fn many(n: usize, base: u128) -> Masked {
        Masked {
            shape: b"iconsts".to_vec(),
            constants: (0..n).map(|it| int(base + it as u128)).collect(),
            size: 10_000,
        }
    }

    #[test]
    fn the_parameter_cap_splits() {
        let limits = Limits::default();
        let items = [(key(1), many(17, 0), vec![]), (key(2), many(17, 100), vec![])];
        assert!(groups(&of(&items), &limits).is_empty());
        let items = [(key(1), many(16, 0), vec![]), (key(2), many(16, 100), vec![])];
        assert_eq!(groups(&of(&items), &limits)[0].parameters().1, 16);
        // Twenty constants that are one value per member are one parameter.
        let same = |value| Masked { constants: vec![int(value); 20], ..many(20, 0) };
        let items = [(key(1), same(1), vec![]), (key(2), same(2), vec![])];
        let group = &groups(&of(&items), &limits)[0];
        assert_eq!(group.varying.len(), 20);
        assert_eq!(group.parameters(), (vec![0; 20], 1));
        // Members far apart in their constants split into the groups they fit.
        let near = |value| {
            let mut it = many(17, 0);
            it.constants[0] = int(value);
            it
        };
        let items = [
            (key(1), near(1), vec![]),
            (key(2), many(17, 100), vec![]),
            (key(3), near(3), vec![]),
            (key(4), many(17, 200), vec![]),
        ];
        let groups = groups(&of(&items), &limits);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].members, vec![key(1), key(3)]);
    }

    #[test]
    fn small_bodies_are_not_worth_thunks() {
        let small = |size| Masked { size, ..grow(8, 8) };
        let small_b = |size| Masked { size, ..grow(4, 4) };
        let items = [(key(1), small(4), refs(9, 10)), (key(2), small_b(4), refs(9, 10))];
        assert!(groups(&of(&items), &Limits::default()).is_empty());
        let items = [(key(1), small(40), refs(9, 10)), (key(2), small_b(40), refs(9, 10))];
        assert_eq!(groups(&of(&items), &Limits::default()).len(), 1);
        assert!(!profitable(8, 2, 2, &Limits::default()));
        assert!(profitable(16, 2, 2, &Limits::default()));
    }

    #[test]
    fn the_input_order_changes_nothing() {
        let items: Vec<(InstanceKey, Masked, Vec<Target>)> = (0..12u8)
            .map(|n| {
                let size = u128::from(n);
                let masked = if n % 3 == 0 { many(17, size * 50) } else { grow(size, 8) };
                (key(n), masked, refs(9, 10 + n % 2))
            })
            .collect();
        let want = groups(&of(&items), &Limits::default());
        assert!(!want.is_empty());
        let mut order: Vec<usize> = (0..items.len()).collect();
        for round in 0..8 {
            order.rotate_left(5);
            order.swap(round % 12, (round * 7 + 3) % 12);
            let candidates = of(&items);
            let permuted: Vec<_> = order.iter().map(|&it| candidates[it]).collect();
            assert_eq!(groups(&permuted, &Limits::default()), want);
        }
    }

    /// `groups` and `Group::parameters` as they were before buckets became sorted runs and
    /// the split incremental: the model the differential tests hold them to.
    mod reference {
        use std::collections::BTreeMap;

        use super::super::*;

        pub(super) fn parameters(columns: &[Vec<Constant>]) -> (Vec<u32>, u32) {
            let mut params: Vec<&Vec<Constant>> = Vec::new();
            let mut reads = Vec::with_capacity(columns.len());
            for column in columns {
                let param = match params.iter().position(|&it| it == column) {
                    Some(param) => param,
                    None => {
                        params.push(column);
                        params.len() - 1
                    }
                };
                reads.push(param as u32);
            }
            (reads, params.len() as u32)
        }

        struct Candidate<'a> {
            key: InstanceKey,
            masked: &'a Masked,
            cells: Vec<Cell<'a>>,
        }

        struct Plan {
            varying: Vec<u32>,
            reads: Vec<u32>,
            params: u32,
        }

        fn plan(bucket: &[Candidate<'_>], members: &[usize]) -> Plan {
            let first = &bucket[members[0]];
            let varying: Vec<u32> = (0..first.cells.len())
                .filter(|&at| members.iter().any(|&it| bucket[it].cells[at] != first.cells[at]))
                .map(|at| at as u32)
                .collect();
            let columns: Vec<Vec<Constant>> = varying
                .iter()
                .map(|&at| {
                    members.iter().map(|&it| bucket[it].masked.constants[at as usize]).collect()
                })
                .collect();
            let (reads, params) = parameters(&columns);
            Plan { varying, reads, params }
        }

        pub(super) fn groups(
            candidates: &[(InstanceKey, Settings, &Masked, &Refs)],
            limits: &Limits,
        ) -> Vec<Group> {
            let mut buckets: BTreeMap<(Settings, ContentId), Vec<Candidate<'_>>> = BTreeMap::new();
            for &(key, settings, masked, refs) in candidates {
                let mut hasher = Hasher::new(MASKED_DOMAIN);
                hasher.part(&masked.shape).update(&masked.size.to_le_bytes());
                hasher.update(&(masked.constants.len() as u64).to_le_bytes());
                for constant in &masked.constants {
                    hasher.update(&kind(constant));
                }
                let cells = masked.constants.iter().map(|&it| cell(key, it, refs)).collect();
                let masked_id = super::with_refs(hasher.finish(), refs);
                buckets.entry((settings, masked_id)).or_default().push(Candidate {
                    key,
                    masked,
                    cells,
                });
            }
            let mut groups = Vec::new();
            for ((settings, masked_id), mut bucket) in buckets {
                bucket.sort_by_key(|it| it.key);
                bucket.dedup_by_key(|it| it.key);
                if bucket.len() < limits.min_members as usize {
                    continue;
                }
                let mut parts: Vec<Vec<usize>> = Vec::new();
                for member in 0..bucket.len() {
                    let fits = parts.iter().position(|part| {
                        let mut with = part.clone();
                        with.push(member);
                        plan(&bucket, &with).params <= limits.max_params
                    });
                    match fits {
                        Some(at) => parts[at].push(member),
                        None => parts.push(vec![member]),
                    }
                }
                for part in parts {
                    if part.len() < limits.min_members as usize {
                        continue;
                    }
                    let Plan { varying, reads, params } = plan(&bucket, &part);
                    let size = bucket[part[0]].masked.size;
                    if params == 0 || !profitable(size, part.len(), params, limits) {
                        continue;
                    }
                    let mut hasher = Hasher::new(FOLD_SHAPE_DOMAIN);
                    hasher.id(&masked_id);
                    let mut reads_of = varying.iter().zip(&reads).peekable();
                    for (at, it) in bucket[part[0]].cells.iter().enumerate() {
                        if let Some((_, param)) = reads_of.next_if(|(v, _)| **v as usize == at) {
                            hasher.update(&[5]).update(&param.to_le_bytes());
                        } else {
                            cell_into(&mut hasher, it);
                        }
                    }
                    let constants = part
                        .iter()
                        .map(|&it| {
                            let constants = &bucket[it].masked.constants;
                            varying.iter().map(|&at| constants[at as usize]).collect()
                        })
                        .collect();
                    groups.push(Group {
                        shape: hasher.finish(),
                        settings,
                        varying,
                        members: part.iter().map(|&it| bucket[it].key).collect(),
                        constants,
                    });
                }
            }
            groups.sort_by_key(|it| it.members[0]);
            groups
        }
    }

    /// Candidates over three shapes, few values and few targets, so buckets are large, many
    /// constants vary alike, keys repeat (with other constants) and settings differ.
    fn random(rng: &mut Rng) -> Vec<(InstanceKey, Settings, Masked, Vec<Target>)> {
        let count = 1 + rng.below(48);
        (0..count)
            .map(|_| {
                let shape = rng.below(3) as u8;
                let constants = (0..2 + 3 * shape as usize)
                    .map(|at| match (at + shape as usize) % 4 {
                        _ if rng.below(16) == 0 => Constant::F32(rng.below(2) as u32),
                        0 => int(u128::from(rng.below(3))),
                        1 => Constant::Place(rng.below(3) as u32),
                        2 => Constant::Data(ContentId([rng.below(3) as u8; 32])),
                        _ => int(u128::from(rng.below(2) * 8)),
                    })
                    .collect();
                let refs = (0..3).map(|_| Target::Function(key(100 + rng.below(3) as u8))).collect();
                let size = if rng.below(8) == 0 { 4 } else { [40, 400, 4000][shape as usize] };
                let masked = Masked { shape: vec![shape], constants, size };
                let settings = settings(if rng.below(8) == 0 { 2 } else { 1 });
                (key(rng.below(40) as u8), settings, masked, refs)
            })
            .collect()
    }

    #[test]
    fn groups_are_the_reference_groups() {
        let limits = [
            Limits::default(),
            Limits { max_params: 1, min_members: 2, thunk_size: 0 },
            Limits { max_params: 2, min_members: 3, thunk_size: 0 },
            Limits { max_params: 3, min_members: 2, thunk_size: 6 },
        ];
        let mut rng = Rng::new(1292);
        let mut found = 0;
        for _ in 0..400 {
            let items = random(&mut rng);
            let candidates: Vec<_> =
                items.iter().map(|(k, s, m, r)| (*k, *s, m, r.as_slice())).collect();
            let interned: Vec<_> =
                items.iter().map(|(k, s, m, r)| (*k, *s, masking(m), r.as_slice())).collect();
            for limits in &limits {
                let want = reference::groups(&candidates, limits);
                found += want.len();
                assert_eq!(groups(&interned, limits), want, "{limits:?}");
                for group in &want {
                    let columns: Vec<Vec<Constant>> = (0..group.varying.len())
                        .map(|at| group.constants.iter().map(|it| it[at]).collect())
                        .collect();
                    assert_eq!(group.parameters(), reference::parameters(&columns));
                }
            }
        }
        assert!(found > 400, "the inputs make groups: {found}");
    }

    #[test]
    fn targets_compare_as_their_ids_did() {
        let mut rng = Rng::new(7);
        let target = |rng: &mut Rng| {
            let n = rng.below(3) as u8;
            let name = ["a", "ab", "b"][n as usize].to_owned();
            let id = ContentId([n; 32]);
            match rng.below(7) {
                0 => Target::Function(key(n)),
                1 => Target::Direct(key(n)),
                2 => Target::Static { item: blaze_items::ItemKey(id), instance: ContentId([0; 32]) },
                3 => Target::Data(id),
                4 => Target::Extern(name),
                5 => Target::Weak(name),
                _ => Target::Position(blaze_format::SpanRef { anchor: id, index: 1 }),
            }
        };
        let (mut equal, mut other) = (0, 0);
        for _ in 0..2000 {
            let (a, b) = (target(&mut rng), target(&mut rng));
            let (cell_a, cell_b) = (Cell::Target(&a), Cell::Target(&b));
            assert_eq!(cell_a == cell_b, target_id(&a) == target_id(&b), "{a:?} {b:?}");
            if a == b { equal += 1 } else { other += 1 }
        }
        assert!(equal > 50 && other > 50);
    }

    /// A form's content ID is the bucket `groups` hashed from the masked IR before forms were
    /// interned, so every group's shape and key, and what the store keeps under them, stay.
    #[test]
    fn a_forms_content_id_is_the_bucket_groups_hashed() {
        let mut rng = Rng::new(4);
        let mut seen = 0;
        for _ in 0..200 {
            for (_, _, masked, _) in random(&mut rng) {
                let mut hasher = Hasher::new("blaze v2 masked 2");
                hasher.part(&masked.shape).update(&masked.size.to_le_bytes());
                hasher.update(&(masked.constants.len() as u64).to_le_bytes());
                for constant in &masked.constants {
                    hasher.update(&kind(constant));
                }
                assert_eq!(blaze_hashcons::content_of(&masked.form()), hasher.finish());
                let (form, constants) = (masked.form(), masked.constants.clone());
                assert_eq!(masked.into_parts(), (form, constants));
                seen += 1;
            }
        }
        assert!(seen > 1000);
    }

    /// Instances of one generic differ in constants: their masked IRs are one form, hashed once
    /// each when interned and never by `groups`.
    #[test]
    fn functions_that_differ_in_constants_are_one_form_and_groups_hashes_none() {
        let forms = Forms::new();
        let (a, b, c) = (grow(8, 8), grow(4, 4), grow(16, 8));
        let before = HASHED.with(std::cell::Cell::get);
        let (ma, mb) = (Masking::of(&forms, &a), Masking::of(&forms, &b));
        let mc = Masking::of(&forms, &c);
        assert_eq!(HASHED.with(std::cell::Cell::get) - before, 3);
        assert_eq!(forms.len(), 1);
        assert!(ma.form == mb.form && mb.form == mc.form);
        assert_eq!(mb.constants, &b.constants[..]);

        let rs = refs(9, 10);
        let candidates = [
            (key(1), settings(1), ma, &rs[..]),
            (key(2), settings(1), mb, &rs[..]),
            (key(3), settings(1), mc, &rs[..]),
        ];
        let before = HASHED.with(std::cell::Cell::get);
        let found = groups(&candidates, &Limits::default());
        assert_eq!(HASHED.with(std::cell::Cell::get), before);
        assert_eq!(found[0].members, vec![key(1), key(2), key(3)]);
    }

    #[test]
    #[should_panic(expected = "not of its form's kind")]
    fn a_masking_whose_constants_are_not_its_forms_stops() {
        let forms = Forms::new();
        let (a, b) = (grow(8, 8), grow(4, 4));
        let mut other = b.constants.clone();
        other[0] = Constant::F64(4);
        let rs = refs(9, 10);
        let ma = Masking::of(&forms, &a);
        let wrong = Masking { form: Masking::of(&forms, &b).form, constants: &other };
        let candidates = [(key(1), settings(1), ma, &rs[..]), (key(2), settings(1), wrong, &rs[..])];
        groups(&candidates, &Limits::default());
    }

    #[test]
    fn thunks_call_the_body_last() {
        let items = [(key(1), grow(8, 8), refs(9, 10)), (key(2), grow(4, 8), refs(9, 10))];
        let group = &groups(&of(&items), &Limits::default())[0];
        let refs = group.thunk_refs(&items[0].2);
        assert_eq!(refs.len(), 3);
        assert_eq!(refs[2], Target::Direct(group.key()));
    }
}
