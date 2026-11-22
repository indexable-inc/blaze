//! Where a key the lazy program is asked about COLD comes from: a checker key the key map has
//! no row for yet, because nothing made in this command named it (the process's first call; a
//! callee a FOUND code record declares, whose caller was never checked here).
//!
//! The from-source program files every key before it is asked (`Front2::new` files each item,
//! variant, module and impl; a container's head files its members; a body's lowering files its
//! local items, `Locals::keep`). Over records nothing is filed when the program is made, so
//! each kind of fn is found here from what the records hold, and filed:
//!
//! - AN ITEM (a fn written in a file, or by a macro call's expansion at module level: an
//!   attribute, a derive, the implicit `bstd::main`, a `macro_rules!` call, a call a call
//!   wrote; a fn of a block the names library writes as a module): a row of its crate's items
//!   record, found by its key's first 16 bytes (`Keys::library_in`, source.rs
//!   `find_beginning`). Not here: `target_of` asks that first.
//! - A MEMBER of a trait or an impl, and a struct's constructor: its key is a function of its
//!   container's key, its kind and its name (keys.rs `member_key_of`), so the crate's
//!   containers' member rows are each keyed so ([`LazyProgram::member_rows`], once per crate
//!   per command) and the one asked for is filed under its container. Its container's head is
//!   then made by `target_of`'s want, on demand.
//! - AN ITEM DECLARED IN A BODY (`ItemKey::INNER`): its key is derived from its enclosing
//!   body's (locals.rs), so the bodies around it are found by the one derivation over TEXT
//!   (nested_exports.rs `walk_every`, the derivation `Locals::keep` keys by) and lowered,
//!   outermost first, which files it as the from-source program files it. The process's own
//!   exported start declared in a body (bstd's `main!` declares `bstd_hosted_start` in the
//!   `main` it writes) is found by the start rule's chain (nested_exports.rs
//!   `enclosing_bodies`) without a search. A body whose local keys the text cannot give (a
//!   statement macro call before them: [`Unkeyed::AfterCall`]) is lowered to read them.
//! - A CLOSURE's or a COROUTINE's instance is tabled as its owner fn (run/make_one.rs
//!   `name_ref`): its owner is one of the kinds above.
//!
//! A key none of these finds is [`LazyRefusal::NotFound`], naming the crate and what was
//! searched.

use super::*;

use crate::front2::keys::member_key_of;
use crate::front2::lower_made::{LowerTarget, lower_made};
use crate::front2::nested_exports::{Reading, Top, Unkeyed, walk_every};
use crate::front2::start::Entry2;
use blaze_surface::{ItemDetail, ItemKind};

/// The most bodies one cold search lowers or reads (rule 18).
pub const MOST_SEARCHED: usize = 1 << 14;

/// One member of a crate's containers, as [`LazyProgram::member_rows`] keys it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct MemberRow<'n> {
    pub container: LibKey,
    pub kind: MemberKind,
    pub name: &'n str,
}

/// What a crate's member rows were keyed to: each member's checker key, `None` where two
/// members' keys are one (rule 21: refused when asked), and how many containers were read.
pub(super) struct MemberIndex<'n> {
    pub rows: HashMap<ItemKey, Option<MemberRow<'n>>, Global>,
    pub containers: u32,
}

/// A body the search lowers: a free fn's (its library key), a member fn's (its checker key).
type Chain = Vec<LowerTarget, Global>;

/// A fn whose body holds items, read by text in the search: its library key, for a member its
/// container's library key and its own checker key, its text and its name.
#[derive(Clone, Copy)]
struct Searched<'n> {
    lib: LibKey,
    member: Option<(LibKey, ItemKey)>,
    text: &'n str,
    name: &'n str,
}

impl<'n, S: PositionSource<'n>> LazyProgram<'n, S> {
    /// Files `key` in the key map from the records (module docs), or refuses naming what was
    /// searched. `Ok` when the key map already knows it or the names source finds its row.
    pub(super) fn locate(&mut self, key: ItemKey) -> Result<(), LazyRefusal> {
        if self.front.keys.origin(key).is_some() || self.front.keys.library_in(self.front.names, key).is_some() {
            return Ok(());
        }
        let names = self.front.names;
        let mut whole = [0u8; 32];
        whole[..16].copy_from_slice(&key.0.to_be_bytes());
        let Some(krate) = names.crate_of(LibKey(blaze_ids::ContentId(whole))) else {
            return Err(LazyRefusal::NotFound { key, krate: Box::from("no crate of the program"), searched: Box::from("its key's crate prefix is no crate's of the program") });
        };
        // A member (its key's eighth byte is a hash byte, so its inner bit says nothing).
        let containers = self.member_index(krate)?;
        if let Some(found) = self.member_rows.get(&krate).and_then(|ix| ix.rows.get(&key).copied()) {
            let Some(row) = found else {
                return Err(LazyRefusal::NotFound { key, krate: self.crate_shown(krate), searched: Box::from("two members of its crate are keyed alike (rule 21)") });
            };
            return self.file_member(key, row);
        }
        let rows = format!("its item, module and variant rows; the {} members' keys of its {containers} traits, impls and tuple or unit structs", self.member_rows.get(&krate).map_or(0, |ix| ix.rows.len()));
        let inner = key.0.to_be_bytes()[8] & LibKey::INNER != 0;
        if !inner {
            return Err(LazyRefusal::NotFound { key, krate: self.crate_shown(krate), searched: format!("{rows}; not an item declared in a body (its key's inner bit is clear)").into_boxed_str() });
        }
        self.locate_local(key, krate, &rows)
    }

    /// Crate `krate` as a refusal shows it: its index and its key.
    fn crate_shown(&self, krate: usize) -> Box<str> {
        match self.front.names.crate_key(krate) {
            Some(k) => format!("crate {krate} ({k:x})"),
            None => format!("crate {krate}"),
        }
        .into_boxed_str()
    }

    /// The member index of `krate`, made once: each trait's and impl's members keyed as their
    /// container's head files them (heads.rs: a fn, const or type by its name), and each tuple
    /// or unit struct's constructor (`MemberKind::Ctor`, named ""). Returns how many containers
    /// were read.
    fn member_index(&mut self, krate: usize) -> Result<u32, LazyRefusal> {
        if let Some(ix) = self.member_rows.get(&krate) {
            return Ok(ix.containers);
        }
        let names = self.front.names;
        let mut containers: Vec<(LibKey, ItemKind), Global> = Vec::new_in(Global);
        let listed = names.each_item(krate, &mut |lib, _module| match names.kind_of_item(lib) {
            Some(kind @ (ItemKind::Trait | ItemKind::Impl)) => containers.push((lib, kind)),
            Some(ItemKind::Struct) if names.is_ctor(lib) => containers.push((lib, ItemKind::Struct)),
            _ => {}
        });
        if !listed {
            return Err(LazyRefusal::Unanswered { question: "each_item", item: self.crate_shown(krate) });
        }
        let mut ix = MemberIndex { rows: HashMap::new_in(Global), containers: containers.len() as u32 };
        let mut put = |key: ItemKey, row: MemberRow<'n>| match ix.rows.get(&key) {
            Some(Some(first)) if *first != row => {
                ix.rows.insert(key, None);
            }
            Some(_) => {}
            None => {
                ix.rows.insert(key, Some(row));
            }
        };
        for &(container, kind) in containers.iter() {
            let ck = checker_of(container);
            if kind == ItemKind::Struct {
                put(member_key_of(ck, MemberKind::Ctor, ""), MemberRow { container, kind: MemberKind::Ctor, name: "" });
                continue;
            }
            names.each_member(container, &mut |m| {
                let (Some(name), Some(kind)) = (m.name, member_kind(m.kind)) else { return };
                put(member_key_of(ck, kind, name), MemberRow { container, kind, name });
            });
        }
        let n = ix.containers;
        self.member_rows.insert(krate, ix);
        Ok(n)
    }

    /// Files the member `row` asked for as `key`: its container as an item, then the member.
    fn file_member(&mut self, key: ItemKey, row: MemberRow<'n>) -> Result<(), LazyRefusal> {
        let clash = |c: crate::front2::keys::KeyClash| LazyRefusal::Head { item: format!("{:x}", row.container).into_boxed_str(), why: format!("{c}").into_boxed_str() };
        let container = self.front.keys.item(row.container).map_err(clash)?;
        let filed = self.front.keys.member(container, row.kind, row.name).map_err(clash)?;
        if filed != key {
            return Err(LazyRefusal::NotFound { key, krate: self.refused_shown(container), searched: Box::from("its member index row filed under another key (keys.rs `member_key_of` and `Keys::member` disagree)") });
        }
        Ok(())
    }

    /// An item declared in a body: the start's chain, else the crate's fns that hold items, by
    /// text, else by lowering a body whose keys the text cannot give (module docs).
    fn locate_local(&mut self, key: ItemKey, krate: usize, rows: &str) -> Result<(), LazyRefusal> {
        let names = self.front.names;
        let alloc = self.alloc;
        // The process's start, declared in a body: its chain by the start rule.
        if let Some(Entry2::Hosted(lib) | Entry2::CMain(lib)) = self.start
            && checker_of(lib) == key
        {
            let chain = crate::front2::nested_exports::enclosing_bodies(names, alloc, lib).map_err(|r| LazyRefusal::Row { function: self.refused_shown(key), refusal: Box::new(r) })?;
            let mut targets: Chain = Vec::new_in(Global);
            targets.extend(chain.iter().map(|&k| LowerTarget::Free(k)));
            return self.lower_chain(key, &targets).and_then(|()| self.filed_local(key, krate, "the start rule's chain of the bodies around the process's exported start"));
        }
        let (Some(crate_key), Some(edition), Some(atoms)) = (names.crate_key(krate), names.edition_of(krate), names.cfg_atoms(krate)) else {
            return Err(LazyRefusal::Unanswered { question: "crate_key, edition_of, cfg_atoms", item: self.crate_shown(krate) });
        };
        // The crate's fns whose body holds items, free and members, in key order.
        let mut tops: Vec<Searched<'n>, Global> = Vec::new_in(Global);
        let mut containers: Vec<LibKey, Global> = Vec::new_in(Global);
        let mut unread = 0u32;
        // The kind first: only a fn's detail is asked (the records answer a fn's from its row's
        // body-holds-items flag; another kind's detail is the file's outline, not read here).
        let listed = names.each_item(krate, &mut |lib, _module| match names.kind_of_item(lib) {
            Some(ItemKind::Function) => {
                if let Some(ItemDetail::Fn { body_holds_items: true }) = names.detail_of(lib) {
                    match names.text_of(lib) {
                        Some(it) => tops.push(Searched { lib, member: None, text: it.text, name: it.name.unwrap_or("_") }),
                        None => unread += 1,
                    }
                }
            }
            Some(ItemKind::Trait | ItemKind::Impl) => containers.push(lib),
            _ => {}
        });
        if !listed {
            return Err(LazyRefusal::Unanswered { question: "each_item", item: self.crate_shown(krate) });
        }
        for &container in containers.iter() {
            names.each_member(container, &mut |m| {
                if m.kind == ItemKind::Function
                    && m.body_holds_items
                    && let Some(name) = m.name
                {
                    let mk = member_key_of(checker_of(container), MemberKind::Fn, name);
                    names.read_member_body(m.key, m.written_by);
                    tops.push(Searched { lib: m.key, member: Some((container, mk)), text: m.text, name });
                }
            });
        }
        if unread > 0 {
            return Err(LazyRefusal::Unanswered { question: "text_of", item: format!("{unread} fns of {} whose bodies hold items", self.crate_shown(krate)).into_boxed_str() });
        }
        // By text: the first body whose derived keys hold `key` gives its chain.
        let reading = Reading { edition, atoms, alloc };
        let mut lowered_later: Vec<Chain, Global> = Vec::new_in(Global);
        let mut read = 0usize;
        for &Searched { lib, member, text, name } in tops.iter() {
            read += 1;
            if read > MOST_SEARCHED {
                return Err(LazyRefusal::NotFound { key, krate: self.crate_shown(krate), searched: format!("{rows}; more fns whose bodies hold items than the most one search reads ({MOST_SEARCHED})").into_boxed_str() });
            }
            let top = Top { text, name, library: Some((crate_key, lib)) };
            let walked = walk_every(top, member, reading).map_err(|r| LazyRefusal::Head { item: self.refused_shown(checker_of(lib)), why: format!("the items declared in its body did not read: {r}").into_boxed_str() })?;
            let first: LowerTarget = match member {
                Some((_, mk)) => LowerTarget::Member(mk),
                None => LowerTarget::Free(lib),
            };
            let chain_of = |within: u32| -> Option<Chain> {
                let keys = walked.chain_to(within, alloc).ok()?.ok()?;
                let mut c: Chain = Vec::new_in(Global);
                c.push(first);
                c.extend(keys.iter().skip(1).map(|&k| LowerTarget::Free(k)));
                Some(c)
            };
            if let Some(l) = walked.locals.iter().find(|l| matches!(l.key, Ok(k) if checker_of(k) == key)) {
                let Some(chain) = chain_of(l.within) else { continue };
                if let Some((container, mk)) = member {
                    self.file_member(mk, MemberRow { container, kind: MemberKind::Fn, name })?;
                }
                return self.lower_chain(key, &chain).and_then(|()| self.filed_local(key, krate, "the chain its body's text derives"));
            }
            // A body whose locals' keys the text cannot give: lowered below, if no text gives
            // the key.
            for l in walked.locals.iter() {
                if l.key == Err(Unkeyed::AfterCall)
                    && let Some(chain) = chain_of(l.within)
                    && !lowered_later.iter().any(|c| *c == chain)
                {
                    if let Some((container, mk)) = member {
                        self.file_member(mk, MemberRow { container, kind: MemberKind::Fn, name })?;
                    }
                    lowered_later.push(chain);
                }
            }
        }
        // By lowering: each such body (its chain first), then the bodies its lowering kept.
        let mut lowered = 0usize;
        let mut queue = lowered_later;
        while let Some(chain) = queue.pop() {
            lowered += chain.len();
            if lowered > MOST_SEARCHED {
                return Err(LazyRefusal::NotFound { key, krate: self.crate_shown(krate), searched: format!("{rows}; more bodies than the most one search lowers ({MOST_SEARCHED})").into_boxed_str() });
            }
            self.lower_chain(key, &chain)?;
            if self.front.keys.local_kind(key).is_some() {
                return Ok(());
            }
            // The fns its lowering kept that hold items of their own: each read by text in turn.
            let Some(&last) = chain.last() else { continue };
            let owner = match last {
                LowerTarget::Free(k) => checker_of(k),
                LowerTarget::Member(k) => k,
            };
            let Some(body) = self.store.locals.body(owner) else { continue };
            for row in body.rows.iter().filter(|r| r.kind == crate::front2::locals::LocalKind::Fn) {
                let mut next = chain.clone();
                next.push(LowerTarget::Free(row.key));
                queue.push(next);
            }
        }
        Err(LazyRefusal::NotFound {
            key,
            krate: self.crate_shown(krate),
            searched: format!("{rows}; the items declared in its {read} fns whose bodies hold items, by text, and {lowered} bodies lowered").into_boxed_str(),
        })
    }

    /// Each body of `chain` lowered, outermost first: what files its local items (locals.rs
    /// `Locals::keep`, run by the lowering) and makes the heads the next one reads.
    fn lower_chain(&mut self, key: ItemKey, chain: &[LowerTarget]) -> Result<(), LazyRefusal> {
        let prepared = self.prepared;
        for &target in chain.iter() {
            let owner = match target {
                LowerTarget::Free(k) => checker_of(k),
                LowerTarget::Member(k) => k,
            };
            if self.store.locals.body(owner).is_some() {
                continue;
            }
            let lowered = {
                let mut r = self.records(true);
                lower_made(&mut r, prepared, target).map(drop)
            };
            lowered.map_err(|error| LazyRefusal::Lower { function: format!("{} (lowered to find the item {:032x} declared in it)", self.shown(owner), key.0).into_boxed_str(), error })?;
        }
        Ok(())
    }

    /// `key` filed after its chain was lowered, or refused: the chain's derivation and the
    /// lowering's disagree.
    fn filed_local(&self, key: ItemKey, krate: usize, how: &str) -> Result<(), LazyRefusal> {
        match self.front.keys.local_kind(key) {
            Some(_) => Ok(()),
            None => Err(LazyRefusal::NotFound { key, krate: self.crate_shown(krate), searched: format!("{how} was lowered and filed no local item under its key").into_boxed_str() }),
        }
    }
}

/// The member kind a member row of `kind` is keyed by (heads.rs); `None` for a kind no head
/// keys (a macro call in a container).
fn member_kind(kind: ItemKind) -> Option<MemberKind> {
    match kind {
        ItemKind::Function => Some(MemberKind::Fn),
        ItemKind::Const => Some(MemberKind::Const),
        ItemKind::TypeAlias => Some(MemberKind::Type),
        _ => None,
    }
}

