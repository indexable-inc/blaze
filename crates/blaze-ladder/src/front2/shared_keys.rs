//! ONE FRONT-END PASS FOR A LIBRARY AND ITS TESTS (docs/v2/plan/design/test.md, "End state",
//! piece 18; target.md: "an item's key holds no cfg, and every record's read list names the cfg
//! outcomes it read").
//!
//! THE FINDING (why the two units share nothing today, with the evidence):
//!
//! 1. The unit is in every item key. blaze-scope units.rs `unit_key` hashes `kind_name(&u.kind)`
//!    ("lib" or "test") into the unit's `CrateItemKey`; blaze-scope task/lower.rs `make_key`
//!    writes that crate's prefix into bytes 0..8 of every `ItemKey`; and `key_of` hashes the
//!    PARENT'S WHOLE KEY (`&parent.0.0`, its prefix included) into a child's bytes 8..32. So an
//!    item of identical text has a different key in the library and in its test unit in EVERY
//!    byte, not only the prefix (the root's own bytes 8..32 are equal: `make_key(krate, &[b"crate
//!    root"])` hashes no crate; each level below takes the prefix in).
//! 2. front2 inherits it: keys.rs `checker_of` keeps bytes 0..16 (prefix and 8 bytes of that
//!    hash), and `member_key` hashes the container's whole 16 bytes. Every fact of a body's read
//!    list (reads.rs `Fact::key`) names own items by those keys, so no read list of the library
//!    equals one of the test unit, even for a body that reads no cfg.
//! 3. The cfg `test` is never READ, it is FOLDED IN: heads.rs `CrateCfg` answers every atom a
//!    lowering asks from the crate's whole set (body.rs `lower_body_expanding(.., &mut
//!    CrateCfg(atoms), ..)`, heads.rs `lower`), and nothing records which atoms were asked; the
//!    unit kind in the crate key stands in for "the cfg may differ" for every item at once.
//!
//! THE DESIGN (this file builds front2's part, additively):
//!
//! - A record is keyed by CODE IDENTITY ([`shared_key`]): the source package (the unit key
//!   WITHOUT its kind: [`UnitBase::source`]), the item's RELATIVE key ([`Relative`]) and its
//!   text's digest. No cfg is in it.
//! - Its read list names what it read RELATIVELY ([`relative_reads`]): each fact's key made
//!   unit-independent, and the cfg atoms its text evaluates, each with its outcome
//!   ([`cfg_reads`]: every `#[cfg(..)]`, `#[cfg_attr(pred, ..)]`'s predicate and `cfg!(..)` in
//!   the item's text). A lookup ([`SharedRecords::look`]) finds the record by key and takes it
//!   only when every read answers today as it did: an item that reads no cfg and resolves the
//!   same is FOUND in the test unit; one that reads `test`, or whose names or candidates differ
//!   (a `#[cfg(test)]` helper, a `#[cfg(test)] impl`), is RECOMPUTED.
//! - The relative key is a strategy at one seam (rule 78): [`PrefixSwap`] (the end state, once
//!   blaze-scope's `key_of` hashes the parent's relative bytes: then two units' keys differ only
//!   in the prefix) and [`ByPlace`] (today: each own key through its place, the file's path and
//!   the item's chain, which two units of one source share). An own key neither maps stays
//!   ABSOLUTE: it never equals another unit's, so its reader is recomputed, never wrongly found.
//!
//! OWED, by name (other lanes' crates; exact changes in the writer's report): blaze-scope
//! `key_of` hashing `&parent.0.0[8..]`; blaze-scope `unit_key` giving the source key beside the
//! unit key; blaze-typeck's answer digests (reads.rs: `Read::answer`) hashing own keys relatively
//! (today an answer naming an own item differs between units, so its reader is recomputed); the
//! body lowering recording its cfg asks (heads.rs `CrateCfg` replaced by a recording host) so
//! [`cfg_reads`] is the lowering's own list rather than a scan of the text; a replay that answers
//! a filed fact in today's world without checking the body (read-addressed.md, blaze-typeck
//! replay.rs): [`SharedRecords::look`] takes today's answers as a list until then.

use blaze_expand::{Delim, TokenKind, cfg_predicate_at, lex};
use blaze_ids::{ContentId, Hasher};
use blaze_own::{AVec, Alloc};
use blaze_scope::resolve::{At, Names};
use blaze_surface::Edition;

use super::keys::checker_of;
use super::reads::{Fact, Read};

/// The domain of a shared record's key ([`shared_key`]).
pub const SHARED_DOMAIN: &str = "blaze-ladder front2 shared record key 1";
/// The domain of an item text's digest inside a shared key.
const TEXT_DOMAIN: &str = "blaze-ladder front2 shared item text 1";
/// The domain of a cfg atom's digest ([`atom_digest`]).
const ATOM_DOMAIN: &str = "blaze-ladder front2 cfg atom 1";
/// The domain of a place's relative key ([`ByPlace`]).
const PLACE_DOMAIN: &str = "blaze-ladder front2 relative place 1";

/// The prefix a relative key carries in place of its unit's crate prefix. No crate has it: a
/// crate prefix is 8 bytes of a content hash, and all ones is refused where a unit is made (the
/// owed blaze-scope change); until then a collision has odds 2^-64 and is a wrong share, named.
pub const SELF_PREFIX: u64 = u64::MAX;

/// The relative key of the unit's crate itself (a `CrateBucket` fact's key).
pub const SELF_CRATE: u128 = (SELF_PREFIX as u128) << 64;
/// The relative key of the unit's root module.
pub const SELF_ROOT: u128 = SELF_CRATE | 1;

/// One cfg atom an item's text evaluates, by digest, and whether the unit's cfg holds it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CfgRead {
    pub atom: u64,
    pub holds: bool,
}

/// The digest of the cfg atom `name` (`= "value"`).
pub fn atom_digest(name: &str, value: Option<&str>) -> u64 {
    let mut h = Hasher::new(ATOM_DOMAIN);
    h.part(name.as_bytes());
    match value {
        Some(v) => h.part(&[1]).part(v.as_bytes()),
        None => h.part(&[0]),
    };
    let id = h.finish();
    u64::from_le_bytes([id.0[0], id.0[1], id.0[2], id.0[3], id.0[4], id.0[5], id.0[6], id.0[7]])
}

/// Why an item's cfg reads were not listed: the item is then never shared (its caller files no
/// shared record), never guessed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CfgReadError {
    /// The text did not lex in its edition.
    Lex,
    /// A `cfg` group whose predicate is no predicate (a token index of the item's stream).
    Malformed { token: u32 },
    /// A cfg form this scan does not read (`cfg_select!`): named, not skipped.
    Unread { form: &'static str },
}

/// The index of the token closing the group opened at `open`, or `None` when it is never closed.
fn close_of(tokens: &[blaze_expand::Token], open: usize) -> Option<usize> {
    let mut depth = 0usize;
    for (i, t) in tokens.iter().enumerate().skip(open) {
        match t.kind {
            TokenKind::Open(_) => depth += 1,
            TokenKind::Close(_) => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
    }
    None
}

/// The cfg atoms `text` (one item's, its attributes included) evaluates under `cfg`, sorted by
/// atom, once each. A superset of what its lowering asks (a predicate inside a macro's
/// definition is read too): a superset only recomputes more, never shares wrongly.
pub fn cfg_reads<'a>(text: &str, edition: Edition, cfg: &[(&str, Option<&str>)], alloc: Alloc<'a>) -> Result<AVec<'a, CfgRead>, CfgReadError> {
    let stream = lex(text, edition, alloc).map_err(|_| CfgReadError::Lex)?;
    let tokens = stream.tokens();
    let mut out: AVec<'a, CfgRead> = AVec::new_in(alloc);
    let ident = |i: usize| match tokens.get(i).map(|t| t.kind) {
        Some(TokenKind::Ident { .. }) => Some(stream.text_of(&tokens[i])),
        _ => None,
    };
    let open_paren = |i: usize| matches!(tokens.get(i).map(|t| t.kind), Some(TokenKind::Open(Delim::Paren)));
    let bang = |i: usize| matches!(tokens.get(i).map(|t| t.kind), Some(TokenKind::Punct { char: '!', .. }));
    let malformed = |i: usize| CfgReadError::Malformed { token: u32::try_from(i).unwrap_or(u32::MAX) };
    let mut i = 0;
    while i < tokens.len() {
        // The predicate's token range, when a cfg form starts at `i`.
        let range = match ident(i) {
            Some("cfg") if open_paren(i + 1) => Some((i + 2, close_of(tokens, i + 1).ok_or(malformed(i))?)),
            Some("cfg") if bang(i + 1) && open_paren(i + 2) => Some((i + 3, close_of(tokens, i + 2).ok_or(malformed(i))?)),
            Some("cfg_attr") if open_paren(i + 1) => {
                let close = close_of(tokens, i + 1).ok_or(malformed(i))?;
                // The predicate ends at the group's first comma at its own depth.
                let mut depth = 0usize;
                let mut end = close;
                for (j, t) in tokens.iter().enumerate().take(close).skip(i + 2) {
                    match t.kind {
                        TokenKind::Open(_) => depth += 1,
                        TokenKind::Close(_) => depth = depth.saturating_sub(1),
                        TokenKind::Punct { char: ',', .. } if depth == 0 => {
                            end = j;
                            break;
                        }
                        _ => {}
                    }
                }
                Some((i + 2, end))
            }
            Some("cfg_select") if bang(i + 1) => return Err(CfgReadError::Unread { form: "cfg_select!" }),
            _ => None,
        };
        if let Some((start, end)) = range {
            let mut atom = |name: &str, value: Option<&str>| -> Result<bool, ()> {
                let holds = cfg.iter().any(|&(n, v)| n == name && v == value);
                out.push(CfgRead { atom: atom_digest(name, value), holds });
                Ok(holds)
            };
            cfg_predicate_at(&stream, start..end, alloc, &mut atom).map_err(|_| malformed(start))?;
        }
        i += 1;
    }
    out.sort_unstable();
    out.dedup();
    Ok(out)
}

/// How a unit's own key is made unit-independent (rule 78: one seam, each strategy tested
/// equal where both apply). A key of another crate is returned as it is: a dependency's keys
/// are equal in both units already.
pub trait Relative {
    fn relative(&self, key: u128) -> u128;
}

/// THE END STATE: once blaze-scope's `key_of` hashes the parent's relative bytes, two units of
/// one source key an item alike but for the crate prefix (bytes 0..8), which is swapped for
/// [`SELF_PREFIX`].
#[derive(Clone, Copy, Debug)]
pub struct PrefixSwap {
    /// The unit's crate prefix, as a key's high 64 bits.
    pub own: u64,
    /// The unit's crate key's first 16 bytes (a `CrateBucket` fact's key).
    pub own_crate: u128,
}

impl Relative for PrefixSwap {
    fn relative(&self, key: u128) -> u128 {
        if key == self.own_crate {
            return SELF_CRATE;
        }
        match (key >> 64) as u64 == self.own {
            true => (u128::from(SELF_PREFIX) << 64) | (key & u128::from(u64::MAX)),
            false => key,
        }
    }
}

/// TODAY'S FORM: each own item's checker key mapped through its place: its file's path and its
/// item chain (blaze-file-items `item_chain`), sorted by key for a binary search. An item a macro
/// wrote (no file of its own) and a member of a trait or impl are not mapped: they stay absolute.
pub struct ByPlace<'a> {
    pub own: u64,
    pub own_crate: u128,
    pub root: u128,
    rows: AVec<'a, (u128, u128)>,
}

impl<'a> ByPlace<'a> {
    /// The places of crate `krate` of `names`.
    pub fn of_names(names: &Names<'_>, krate: usize, alloc: Alloc<'a>) -> ByPlace<'a> {
        let key = names.krate(krate).key;
        let mut head = [0u8; 16];
        head.copy_from_slice(&key.0.0[..16]);
        let own_crate = u128::from_be_bytes(head);
        let mut rows: AVec<'a, (u128, u128)> = AVec::new_in(alloc);
        for (lib, _module, place) in names.items(krate) {
            let At::File(file) = place.at else { continue };
            let Some((text, outline)) = names.outline_at(place.krate, place.at) else { continue };
            let Some(source) = names.krate(krate).files.get(file as usize) else { continue };
            let chain = blaze_file_items::item_chain(outline, outline.item(place.item), text);
            let id = Hasher::new(PLACE_DOMAIN).part(source.path.as_bytes()).id(&chain).finish();
            let low = u64::from_le_bytes([id.0[0], id.0[1], id.0[2], id.0[3], id.0[4], id.0[5], id.0[6], id.0[7]]);
            rows.push((checker_of(lib).0, (u128::from(SELF_PREFIX) << 64) | u128::from(low)));
        }
        rows.sort_unstable();
        rows.dedup();
        ByPlace { own: (own_crate >> 64) as u64, own_crate, root: checker_of(names.root(krate)).0, rows }
    }
}

impl Relative for ByPlace<'_> {
    fn relative(&self, key: u128) -> u128 {
        if key == self.own_crate {
            return SELF_CRATE;
        }
        if key == self.root {
            return SELF_ROOT;
        }
        if (key >> 64) as u64 != self.own {
            return key;
        }
        match self.rows.binary_search_by(|(k, _)| k.cmp(&key)) {
            Ok(at) => self.rows[at].1,
            // Unmapped: absolute, so never equal to another unit's (module docs).
            Err(_) => key,
        }
    }
}

/// What a unit is, for sharing: its source (the unit key without its kind), how its own keys are
/// made relative, its cfg and its edition.
pub struct UnitBase<'c, R: Relative> {
    pub source: ContentId,
    pub relative: R,
    pub cfg: &'c [(&'c str, Option<&'c str>)],
    pub edition: Edition,
}

/// A record's shared key: the source, the item's relative key and its text's digest. No cfg.
pub fn shared_key<R: Relative>(base: &UnitBase<'_, R>, item: u128, text: &str) -> ContentId {
    let text_id = ContentId::of(TEXT_DOMAIN, text.as_bytes());
    Hasher::new(SHARED_DOMAIN).id(&base.source).part(&base.relative.relative(item).to_be_bytes()).id(&text_id).finish()
}

/// A record's read list in relative form: the cfg atoms its text evaluates, then the facts its
/// check read with every key relative; each sorted, once each.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SharedReads<'a> {
    pub cfg: AVec<'a, CfgRead>,
    pub facts: AVec<'a, Read>,
}

/// `facts` (a body's read list, reads.rs) with each key made relative, beside `cfg`.
pub fn relative_reads<'a, R: Relative>(base: &UnitBase<'_, R>, cfg: AVec<'a, CfgRead>, facts: &[Read], alloc: Alloc<'a>) -> SharedReads<'a> {
    let mut out: AVec<'a, Read> = AVec::with_capacity_in(facts.len(), alloc);
    for r in facts {
        out.push(Read { fact: Fact { kind: r.fact.kind, key: base.relative.relative(r.fact.key), detail: r.fact.detail }, answer: r.answer });
    }
    out.sort_unstable();
    out.dedup();
    SharedReads { cfg, facts: out }
}

/// What a lookup came to.
#[derive(Debug, PartialEq, Eq)]
pub enum Look<'v, V> {
    /// A record under the key whose every read answers today as it did: taken, not recomputed.
    Found(&'v V),
    /// No record under the key: computed fresh.
    NoRecord,
    /// Records under the key, none valid: the first read that moved in the first record.
    CfgMoved { atom: u64 },
    FactMoved { fact: Fact },
}

/// The first read of `filed` that `today` does not answer alike, as a [`Look`]; `None` when all do.
fn moved<'v, V>(filed: &SharedReads<'_>, today: &SharedReads<'_>) -> Option<Look<'v, V>> {
    if filed.cfg != today.cfg {
        let at = filed.cfg.iter().zip(today.cfg.iter()).position(|(a, b)| a != b).unwrap_or(filed.cfg.len().min(today.cfg.len()));
        let atom = filed.cfg.get(at).or(today.cfg.get(at)).map_or(0, |c| c.atom);
        return Some(Look::CfgMoved { atom });
    }
    for r in filed.facts.iter() {
        let same = today.facts.binary_search_by(|t| t.fact.cmp(&r.fact)).is_ok_and(|at| today.facts[at].answer == r.answer);
        if !same {
            return Some(Look::FactMoved { fact: r.fact });
        }
    }
    None
}

/// Shared records: (key, reads, value) rows sorted by key (two records under one key differ in
/// their reads: a body checked under two cfg outcomes). The in-memory form of the store's rows
/// for this proof; the store files the same rows by the same keys.
pub struct SharedRecords<'a, V> {
    rows: AVec<'a, (ContentId, SharedReads<'a>, V)>,
}

impl<'a, V> SharedRecords<'a, V> {
    pub fn new_in(alloc: Alloc<'a>) -> SharedRecords<'a, V> {
        SharedRecords { rows: AVec::new_in(alloc) }
    }

    /// Files a record; its place keeps the rows sorted by key.
    pub fn file(&mut self, key: ContentId, reads: SharedReads<'a>, value: V) {
        let at = self.rows.partition_point(|(k, _, _)| k.0 <= key.0);
        self.rows.insert(at, (key, reads, value));
    }

    /// The record under `key` whose reads `today` answers alike (module docs: until a replay
    /// answers filed facts in today's world, `today` is the read list of today's own check, so
    /// this proves WHICH records may be shared, not yet the saving).
    pub fn look(&self, key: ContentId, today: &SharedReads<'_>) -> Look<'_, V> {
        let start = self.rows.partition_point(|(k, _, _)| k.0 < key.0);
        let mut first: Option<Look<'_, V>> = None;
        for (k, reads, value) in self.rows[start..].iter() {
            if k.0 != key.0 {
                break;
            }
            match moved(reads, today) {
                None => return Look::Found(value),
                Some(why) => {
                    if first.is_none() {
                        first = Some(why);
                    }
                }
            }
        }
        first.unwrap_or(Look::NoRecord)
    }

    pub fn len(&self) -> usize {
        self.rows.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }
}
