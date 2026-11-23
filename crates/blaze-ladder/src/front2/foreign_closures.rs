//! A closure's facts for a body OTHER than its owner (hello-d, 2026-11-16): `fn main() {
//! run(|x: u8| x) }` with `fn run<F: Fn(u8) -> u8>(f: F)`. Resolving `Fn::call` in
//! `run::<{closure in main}>` imports the closure type `Closure { owner: main, ordinal: 0 }`,
//! and the checker asks the world `closure(main, 0)` (blaze-typeck instance.rs `Asked::
//! closure_of`, machine.rs `PNode::Closure`): its kind and signature, which only `main`'s
//! check found (typeck.md, "A closure's identity").
//!
//! THE ONE HOME. Where a body's check finishes clean (front2 body_reads.rs: `check_body_asked`,
//! the build's path, where its read list's digest is filed; and `check_body_filed`, the records
//! path of `blaze check`, project.rs), each closure of its record is filed here
//! ([`ClosureRows::file`]): its kind, signature and proven capture tuple in the head store's type table,
//! in the owner's parameters (`Param(i)` stays `Param(i)`, as `ClosureFacts` is read: the
//! closure type's `args[i]`). The world's `closure` answers from this table
//! ([`ClosureRows::get`]); the answer is a question of the checker (`Question::Closure`), so it
//! is in the asker's read list with its digest (rule 21: the key covers the input).
//!
//! WHY NO WANT. A closure type is made only by its owner's check: every type naming
//! `Closure { owner, .. }` is a copy of a type that check wrote. So in one command the owner's
//! record is filed before any question about its closure can be asked, and there is nothing a
//! want could make that the check did not. An owner with no clean record (its check failed, or
//! it was never checked here) is refused BY NAME ([`Missing::NotChecked`]: the owner's path and
//! the ordinal), never answered with a guess; a row whose signature could not be copied is
//! refused with why ([`Missing::Refused`]).
//!
//! Rows and owners are kept sorted by key: the table's content is a function of the bodies
//! checked, whatever order they were checked in.

use core::alloc::Allocator;
use core::fmt;

use blaze_own::Vec;
use blaze_typeck as tc;
use blaze_typeck::{ClosureKind, ItemKey, PNode, PTy, PortableTys};

/// One closure of a checked body: kind, signature and optional exact capture tuple
/// in the store's table, or why its facts could not be copied.
#[derive(Clone, Copy, Debug)]
struct Row {
    ordinal: u32,
    got: Result<(ClosureKind, PTy, Option<PTy>), RowRefusal>,
}

/// Why a closure's row holds no signature.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RowRefusal {
    /// blaze-build `layout::closure_facts` refused it (a node it does not copy): its text.
    Copy(&'static str),
}

/// Each closure of each body checked clean in this command, by owner and ordinal.
pub struct ClosureRows<A: Allocator + Clone> {
    /// `(owner, first row, row count)`, sorted by owner.
    owners: Vec<(ItemKey, u32, u32), A>,
    rows: Vec<Row, A>,
}

/// Why `closure(owner, ordinal)` has no answer from this table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Missing {
    /// The owner has no clean check filed in this command (its check failed, or it was not
    /// checked): its record cannot be had.
    NotChecked,
    /// The owner's record has fewer closures than the ordinal asked.
    NoSuchClosure { count: u32 },
    /// The row is filed, refused.
    Refused(RowRefusal),
}

impl fmt::Display for Missing {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Missing::NotChecked => write!(f, "its owner has no clean check in this command (the owner's check failed or never ran), so the owner's record cannot be had"),
            Missing::NoSuchClosure { count } => write!(f, "its owner's checked record holds {count} closures"),
            Missing::Refused(RowRefusal::Copy(why)) => write!(f, "its signature could not be copied from the owner's record: {why}"),
        }
    }
}

impl<A: Allocator + Copy> ClosureRows<A> {
    pub fn new_in(alloc: A) -> ClosureRows<A> {
        ClosureRows { owners: Vec::new_in(alloc), rows: Vec::new_in(alloc) }
    }

    /// Files each closure of `checked`, the clean record of `body` (its owner `body.owner`), its
    /// signature copied as written into `into` (the head store's table; an `async` block the
    /// closure returns is its identity, which needs the body's expressions). An owner already
    /// filed keeps its first rows: a body checked again in one command read the same answers
    /// (its check reads the same records whatever was checked before it: check.rs), so its
    /// closures are the same. The copy's scratch is `into`'s own allocator, handed down by the
    /// table's maker (D102).
    pub fn file(&mut self, body: &tc::BodyView<'_>, checked: &tc::Checked<'_>, into: &mut PortableTys<'_>) {
        let owner = body.owner;
        let at = match self.owners.binary_search_by_key(&owner, |o| o.0) {
            Ok(_) => return,
            Err(at) => at,
        };
        let scratch = into.alloc();
        let count = checked.closures.len() as u32;
        let generics = param_count(checked.types.view());
        let first = self.rows.len() as u32;
        let kinds = checked.closures.column::<1>();
        for ordinal in 0..count {
            // The capture tuple is the record's own (`Checked::closures` column 3: the captured
            // places' stored types, blaze-typeck capture_places.rs), the one the owner's solver
            // read; the layout's environment is the whole-binding rows, which a place capture
            // is not (rule 30: one answer in the owner and out of it).
            let got = match blaze_build::layout::closure_facts(body, checked, ordinal, generics, into, scratch) {
                Ok((_, sig, _)) => match checked.closures.column::<3>()[ordinal as usize] {
                    None => Ok((kinds[ordinal as usize], sig, None)),
                    Some(upvars) => match places_env(body, checked, ordinal, upvars, into, scratch) {
                        Ok(env) => Ok((kinds[ordinal as usize], sig, Some(env))),
                        Err(e) => Err(RowRefusal::Copy(build_error_text(&e))),
                    },
                },
                Err(e) => Err(RowRefusal::Copy(build_error_text(&e))),
            };
            self.rows.push(Row { ordinal, got });
        }
        self.owners.insert_shift(at, (owner, first, count));
    }

    /// The kind and signature (in the store's table) of closure `ordinal` of `owner`.
    pub fn get(&self, owner: ItemKey, ordinal: u32) -> Result<(ClosureKind, PTy), Missing> {
        self.with_captures(owner, ordinal).map(|(kind, sig, _)| (kind, sig))
    }

    pub(super) fn with_captures(&self, owner: ItemKey, ordinal: u32) -> Result<(ClosureKind, PTy, Option<PTy>), Missing> {
        let Ok(at) = self.owners.binary_search_by_key(&owner, |o| o.0) else {
            return Err(Missing::NotChecked);
        };
        let (_, first, count) = self.owners[at];
        if ordinal >= count {
            return Err(Missing::NoSuchClosure { count });
        }
        let row = self.rows[(first + ordinal) as usize];
        debug_assert_eq!(row.ordinal, ordinal);
        row.got.map_err(Missing::Refused)
    }

    /// How many owners filed rows.
    pub fn owners(&self) -> usize {
        self.owners.len()
    }
}

/// One more than the greatest type or const parameter index any node of `v` names (0 for
/// none): the identity substitution [`ClosureRows::file`] copies through covers every
/// parameter the record names, so none is "past its arguments".
fn param_count(v: tc::PTys<'_>) -> u32 {
    let mut n = 0u32;
    for node in v.nodes() {
        if let PNode::Param(i) | PNode::ConstParam(i) = *node {
            n = n.max(i.saturating_add(1));
        }
    }
    n
}

/// Closure `ordinal`'s capture tuple `upvars` (its record's column 3) copied as written into
/// `into`, in the owner's parameters: the identity substitution blaze-build `layout::
/// closure_facts` copies the signature through (`Param(i)` stays `Param(i)`), made the same way
/// from the closure's own type.
fn places_env(
    body: &tc::BodyView<'_>,
    checked: &tc::Checked<'_>,
    ordinal: u32,
    upvars: PTy,
    into: &mut PortableTys<'_>,
    scratch: blaze_own::Alloc<'_>,
) -> Result<PTy, blaze_build::BuildError> {
    let malformed = blaze_build::BuildError::Malformed {
        what: "a closure's checked type does not identify its owner, ordinal and generic arguments",
    };
    let from = checked.types.view();
    let Some(&e) = checked.closures.column::<0>().get(ordinal as usize) else {
        return Err(malformed);
    };
    let Some(&ty) = checked.expr_ty.get(e.ix()) else {
        return Err(malformed);
    };
    let Ok(PNode::Closure { args, .. }) = from.node(ty) else {
        return Err(malformed);
    };
    let Ok(identity) = from.list(args) else {
        return Err(malformed);
    };
    let mut params: Vec<PTy, blaze_own::Alloc<'_>> = Vec::with_capacity_in(identity.len(), scratch);
    for &arg in identity {
        let Ok(node) = from.node(arg) else {
            return Err(malformed);
        };
        let Ok(p) = into.intern(node) else {
            return Err(blaze_build::BuildError::Malformed { what: "a closure's facts table is full" });
        };
        params.push(p);
    }
    blaze_build::copy_written(from, upvars, &params, body, into, scratch)
}

/// The refusal's static text: `copy_written` refuses with `Malformed` or `UnsupportedSig`, each
/// carrying one; any other is named by its kind.
fn build_error_text(e: &blaze_build::BuildError) -> &'static str {
    match e {
        blaze_build::BuildError::Malformed { what } | blaze_build::BuildError::UnsupportedSig { what } => what,
        _ => "blaze-build refused a node of the signature or a capture (not `Malformed` or `UnsupportedSig`)",
    }
}

/// The text of the world's gap for a closure of another body that has no answer: the owner by
/// its path (rule 19) and key, the ordinal, and why.
pub fn gap_text(owner: ItemKey, owner_path: &str, ordinal: u32, why: Missing) -> impl fmt::Display + '_ {
    struct Text<'p> {
        owner: ItemKey,
        owner_path: &'p str,
        ordinal: u32,
        why: Missing,
    }
    impl fmt::Display for Text<'_> {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(f, "closure #{} of {} ({:?}), used outside its owner: {}", self.ordinal, self.owner_path, self.owner, self.why)
        }
    }
    Text { owner, owner_path, ordinal, why }
}
