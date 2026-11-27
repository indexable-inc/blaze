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
    pub rows: Map<ItemKey, Option<MemberRow<'n>>, Alloc<'n>>,
    pub containers: u32,
}

/// One body-local item derivation entry in [`LocalIndex`]: optional enclosing trait/impl member
/// row to file first, plus the outermost-first lowering chain.
#[derive(Clone)]
pub(super) struct LocalEntry<'n> {
    pub member: Option<(ItemKey, MemberRow<'n>)>,
    pub chain: Chain,
}

/// Per-crate cache of body-local item derivation chains built once on the first unhinted
/// `locate_local` miss in a crate, eliminating repeated O(crate_fns) lex scans.
pub(super) struct LocalIndex<'n> {
    pub by_key: Map<ItemKey, LocalEntry<'n>, Alloc<'n>>,
    pub after_call: Vec<LocalEntry<'n>, Global>,
    pub after_call_next: usize,
    pub nested_queue: Vec<Chain, Global>,
    pub read: usize,
}

/// Narrow sans-IO seam for indexing crate members without reading member source text
/// (`MemberWant::NameOnly`), directly testable with a counting spy.
trait MemberCatalog<'n>: Copy {
    fn catalog_each_lang(self, krate: usize, each: &mut dyn FnMut(&'n str, LibKey));
    fn catalog_each_item(self, krate: usize, each: &mut dyn FnMut(LibKey, LibKey)) -> bool;
    fn catalog_kind_of_item(self, key: LibKey) -> Option<ItemKind>;
    fn catalog_is_ctor(self, key: LibKey) -> bool;
    fn catalog_each_member_matching(
        self,
        owner: LibKey,
        want: &mut dyn FnMut(ItemKind, Option<&'n str>) -> crate::front2::source::MemberWant,
        each: &mut dyn FnMut(crate::front2::source::MemberOf<'n>),
    ) -> bool;
}

impl<'n, S: NamesSource<'n>> MemberCatalog<'n> for S {
    fn catalog_each_lang(self, krate: usize, each: &mut dyn FnMut(&'n str, LibKey)) {
        self.each_lang(krate, each);
    }
    fn catalog_each_item(self, krate: usize, each: &mut dyn FnMut(LibKey, LibKey)) -> bool {
        self.each_item(krate, each)
    }
    fn catalog_kind_of_item(self, key: LibKey) -> Option<ItemKind> {
        self.kind_of_item(key)
    }
    fn catalog_is_ctor(self, key: LibKey) -> bool {
        self.is_ctor(key)
    }
    fn catalog_each_member_matching(
        self,
        owner: LibKey,
        want: &mut dyn FnMut(ItemKind, Option<&'n str>) -> crate::front2::source::MemberWant,
        each: &mut dyn FnMut(crate::front2::source::MemberOf<'n>),
    ) -> bool {
        self.each_member_matching(owner, want, each)
    }
}

fn put_member_row<'n>(
    rows: &mut Map<ItemKey, Option<MemberRow<'n>>, Alloc<'n>>,
    key: ItemKey,
    row: MemberRow<'n>,
) {
    match rows.get(&key) {
        Some(Some(first)) if *first != row => {
            rows.insert(key, None);
        }
        Some(_) => {}
        None => {
            rows.insert(key, Some(row));
        }
    }
}

/// Indexes one trait, impl, or constructor struct's member keys using `MemberWant::NameOnly`
/// so pack `.rs` source files are never opened or outlined during member lookup.
fn index_container_members<'n, C: MemberCatalog<'n>>(
    catalog: C,
    container: LibKey,
    kind: ItemKind,
    rows: &mut Map<ItemKey, Option<MemberRow<'n>>, Alloc<'n>>,
) {
    let ck = checker_of(container);
    if kind == ItemKind::Struct {
        put_member_row(rows, member_key_of(ck, MemberKind::Ctor, ""), MemberRow { container, kind: MemberKind::Ctor, name: "" });
        return;
    }
    catalog.catalog_each_member_matching(
        container,
        &mut |mkind, mname| {
            if mname.is_some() && member_kind(mkind).is_some() {
                crate::front2::source::MemberWant::NameOnly
            } else {
                crate::front2::source::MemberWant::Skip
            }
        },
        &mut |m| {
            let (Some(name), Some(mk)) = (m.name, member_kind(m.kind)) else { return };
            put_member_row(rows, member_key_of(ck, mk, name), MemberRow { container, kind: mk, name });
        },
    );
}

/// Builds a targeted [`MemberIndex`] over `krate`'s `#[lang]` containers alone (traits, impls,
/// and constructor structs) using `MemberWant::NameOnly`, avoiding a full `each_item` scan of
/// large crates like `core` when resolving lang trait methods such as `PartialEq::eq`.
fn build_lang_member_index<'n, C: MemberCatalog<'n>>(
    catalog: C,
    krate: usize,
    alloc: Alloc<'n>,
) -> MemberIndex<'n> {
    let mut containers: AVec<'n, (LibKey, ItemKind)> = AVec::new_in(alloc);
    catalog.catalog_each_lang(krate, &mut |_name, lib| match catalog.catalog_kind_of_item(lib) {
        Some(kind @ (ItemKind::Trait | ItemKind::Impl)) => containers.push((lib, kind)),
        Some(ItemKind::Struct) if catalog.catalog_is_ctor(lib) => containers.push((lib, ItemKind::Struct)),
        _ => {}
    });
    let mut rows = Map::new_in(alloc);
    for &(container, kind) in containers.iter() {
        index_container_members(catalog, container, kind, &mut rows);
    }
    MemberIndex { rows, containers: u32::try_from(containers.len()).unwrap_or(u32::MAX) }
}

/// Builds the full crate-wide [`MemberIndex`] for `krate` using `MemberWant::NameOnly` (zero
/// file text reads / zero pack file outlines) in `alloc`.
fn build_crate_member_index<'n, C: MemberCatalog<'n>>(
    catalog: C,
    krate: usize,
    alloc: Alloc<'n>,
) -> Option<MemberIndex<'n>> {
    let mut containers: AVec<'n, (LibKey, ItemKind)> = AVec::new_in(alloc);
    let listed = catalog.catalog_each_item(krate, &mut |lib, _module| match catalog.catalog_kind_of_item(lib) {
        Some(kind @ (ItemKind::Trait | ItemKind::Impl)) => containers.push((lib, kind)),
        Some(ItemKind::Struct) if catalog.catalog_is_ctor(lib) => containers.push((lib, ItemKind::Struct)),
        _ => {}
    });
    if !listed {
        return None;
    }
    let mut rows = Map::new_in(alloc);
    for &(container, kind) in containers.iter() {
        index_container_members(catalog, container, kind, &mut rows);
    }
    Some(MemberIndex { rows, containers: u32::try_from(containers.len()).unwrap_or(u32::MAX) })
}

/// Builds a shared [`super::shared::SharedMemberIndex`] over `krate`'s `#[lang]` containers alone.
fn build_shared_lang_member_index<'n, C: MemberCatalog<'n>>(
    catalog: C,
    krate: usize,
    scratch: Alloc<'n>,
    shared: blaze_own::SharedAlloc<'static>,
) -> super::shared::SharedMemberIndex {
    let mut containers: AVec<'n, (LibKey, ItemKind)> = AVec::new_in(scratch);
    catalog.catalog_each_lang(krate, &mut |_name, lib| match catalog.catalog_kind_of_item(lib) {
        Some(kind @ (ItemKind::Trait | ItemKind::Impl)) => containers.push((lib, kind)),
        Some(ItemKind::Struct) if catalog.catalog_is_ctor(lib) => containers.push((lib, ItemKind::Struct)),
        _ => {}
    });
    let mut ix = super::shared::SharedMemberIndex::new_in(u32::try_from(containers.len()).unwrap_or(u32::MAX), shared);
    for &(container, kind) in containers.iter() {
        let ck = checker_of(container);
        if kind == ItemKind::Struct {
            ix.put(member_key_of(ck, MemberKind::Ctor, ""), container, MemberKind::Ctor, "");
            continue;
        }
        catalog.catalog_each_member_matching(
            container,
            &mut |mkind, mname| {
                if mname.is_some() && member_kind(mkind).is_some() {
                    crate::front2::source::MemberWant::NameOnly
                } else {
                    crate::front2::source::MemberWant::Skip
                }
            },
            &mut |m| {
                let (Some(name), Some(mk)) = (m.name, member_kind(m.kind)) else { return };
                ix.put(member_key_of(ck, mk, name), container, mk, name);
            },
        );
    }
    ix
}

/// Builds the full crate-wide shared [`super::shared::SharedMemberIndex`] for `krate` using
/// `MemberWant::NameOnly` (zero source file opens / zero outlines).
fn build_shared_crate_member_index<'n, C: MemberCatalog<'n>>(
    catalog: C,
    krate: usize,
    scratch: Alloc<'n>,
    shared: blaze_own::SharedAlloc<'static>,
) -> Option<super::shared::SharedMemberIndex> {
    let mut containers: AVec<'n, (LibKey, ItemKind)> = AVec::new_in(scratch);
    let listed = catalog.catalog_each_item(krate, &mut |lib, _module| match catalog.catalog_kind_of_item(lib) {
        Some(kind @ (ItemKind::Trait | ItemKind::Impl)) => containers.push((lib, kind)),
        Some(ItemKind::Struct) if catalog.catalog_is_ctor(lib) => containers.push((lib, ItemKind::Struct)),
        _ => {}
    });
    if !listed {
        return None;
    }
    let mut ix = super::shared::SharedMemberIndex::new_in(u32::try_from(containers.len()).unwrap_or(u32::MAX), shared);
    for &(container, kind) in containers.iter() {
        let ck = checker_of(container);
        if kind == ItemKind::Struct {
            ix.put(member_key_of(ck, MemberKind::Ctor, ""), container, MemberKind::Ctor, "");
            continue;
        }
        catalog.catalog_each_member_matching(
            container,
            &mut |mkind, mname| {
                if mname.is_some() && member_kind(mkind).is_some() {
                    crate::front2::source::MemberWant::NameOnly
                } else {
                    crate::front2::source::MemberWant::Skip
                }
            },
            &mut |m| {
                let (Some(name), Some(mk)) = (m.name, member_kind(m.kind)) else { return };
                ix.put(member_key_of(ck, mk, name), container, mk, name);
            },
        );
    }
    Some(ix)
}

/// Probes a single trait/impl container for `target` using `MemberWant::NameOnly`.
fn probe_container_member<'n, C: MemberCatalog<'n>>(
    catalog: C,
    container: LibKey,
    target: ItemKey,
) -> Option<MemberRow<'n>> {
    let ck = checker_of(container);
    let mut found = None;
    catalog.catalog_each_member_matching(
        container,
        &mut |mkind, mname| {
            if mname.is_some() && member_kind(mkind).is_some() {
                crate::front2::source::MemberWant::NameOnly
            } else {
                crate::front2::source::MemberWant::Skip
            }
        },
        &mut |m| {
            if found.is_some() {
                return;
            }
            let (Some(name), Some(mk)) = (m.name, member_kind(m.kind)) else { return };
            if member_key_of(ck, mk, name) == target {
                found = Some(MemberRow { container, kind: mk, name });
            }
        },
    );
    found
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
    /// Hints the cold lookup of a body-local item `key` (`LibKey::INNER` set) by lowering each
    /// hinted enclosing body in `enclosers` (outermost first) until `key` is filed in
    /// `self.front.keys`. Used by parallel `CodePool` workers (and partial-warm serial runs)
    /// when a macro-generated local fn (e.g. `const_eval_select!` -> `is_null::runtime`) was
    /// declared by a caller compiled on another worker or loaded from CAS, filing `key` in O(1)
    /// without scanning every body in the crate.
    ///
    /// A hint that cannot be located or does not file `key` is ignored so the caller falls
    /// through cleanly to [`LazyProgram::locate`].
    pub(crate) fn prepare_local(&mut self, key: ItemKey, enclosers: &[ItemKey]) {
        if key.0.to_be_bytes()[8] & LibKey::INNER == 0 {
            return;
        }
        if self.front.keys.origin(key).is_some() || self.front.keys.library_in(self.front.names, key).is_some() {
            return;
        }
        for &encloser in enclosers {
            if self.front.keys.origin(key).is_some() || self.front.keys.library_in(self.front.names, key).is_some() {
                return;
            }
            if self.store.locals.body(encloser).is_some() {
                continue;
            }
            if self.locate(encloser).is_err() {
                continue;
            }
            let target = if let Some(lib) = self.front.keys.library_in(self.front.names, encloser) {
                LowerTarget::Free(lib)
            } else if matches!(self.front.keys.origin(encloser), Some(crate::front2::keys::Origin::Member { kind: MemberKind::Fn | MemberKind::Const, .. })) {
                LowerTarget::Member(encloser)
            } else {
                continue;
            };
            let _ = self.lower_chain(key, &[target]);
        }
    }

    /// Files `key` in the key map from the records (module docs), or refuses naming what was
    /// searched. `Ok` when the key map already knows it or the names source finds its row.
    pub(super) fn locate(&mut self, key: ItemKey) -> Result<(), LazyRefusal> {
        if self.front.keys.origin(key).is_some() || self.front.keys.library_in(self.front.names, key).is_some() {
            return Ok(());
        }
        let names = self.front.names;
        let mut whole = [0u8; 32];
        if let Some(head) = whole.get_mut(..16) {
            head.copy_from_slice(&key.0.to_be_bytes());
        }
        let Some(krate) = names.crate_of(LibKey(blaze_ids::ContentId(whole))) else {
            return Err(LazyRefusal::NotFound { key, krate: Box::from("no crate of the program"), searched: Box::from("its key's crate prefix is no crate's of the program") });
        };
        // Fast path: when a command-wide `SharedLazyFront` is attached, consult its crate-wide
        // and lang-container member indexes shared across all workers (built at most once per
        // crate across all 128 workers and worker epochs).
        if let Some(shared) = self.shared()
            && let Some(crate_key) = names.crate_key(krate)
        {
            if let Some(shared_ix) = shared.get_crate_members(crate_key) {
                if let Some(found) = shared_ix.get(key) {
                    let Some((container, kind, name)) = found else {
                        return Err(LazyRefusal::NotFound { key, krate: self.crate_shown(krate), searched: Box::from("two members of its crate are keyed alike (rule 21)") });
                    };
                    return self.file_member_parts(key, container, kind, name);
                }
                let rows = format!("its item, module and variant rows; the {} members' keys of its {} traits, impls and tuple or unit structs", shared_ix.len(), shared_ix.containers);
                let inner = key.0.to_be_bytes()[8] & LibKey::INNER != 0;
                if !inner {
                    return Err(LazyRefusal::NotFound { key, krate: self.crate_shown(krate), searched: format!("{rows}; not an item declared in a body (its key's inner bit is clear)").into_boxed_str() });
                }
                return self.locate_local(key, krate, &rows);
            }
            let lang_ix = match shared.get_lang_members(crate_key) {
                Some(ix) => ix,
                None => {
                    let built = build_shared_lang_member_index(names, krate, self.alloc, shared.alloc());
                    shared.put_lang_members(crate_key, built)
                }
            };
            if let Some(found) = lang_ix.get(key) {
                let Some((container, kind, name)) = found else {
                    return Err(LazyRefusal::NotFound { key, krate: self.crate_shown(krate), searched: Box::from("two members of its crate are keyed alike (rule 21)") });
                };
                return self.file_member_parts(key, container, kind, name);
            }
            if let Some(row) = self.probe_loaded_containers(key) {
                return self.file_member(key, row);
            }
            let Some(shared_ix) = self.shared_crate_member_index(krate)? else {
                return Err(LazyRefusal::Unanswered { question: "each_item", item: self.crate_shown(krate) });
            };
            if let Some(found) = shared_ix.get(key) {
                let Some((container, kind, name)) = found else {
                    return Err(LazyRefusal::NotFound { key, krate: self.crate_shown(krate), searched: Box::from("two members of its crate are keyed alike (rule 21)") });
                };
                return self.file_member_parts(key, container, kind, name);
            }
            let rows = format!("its item, module and variant rows; the {} members' keys of its {} traits, impls and tuple or unit structs", shared_ix.len(), shared_ix.containers);
            let inner = key.0.to_be_bytes()[8] & LibKey::INNER != 0;
            if !inner {
                return Err(LazyRefusal::NotFound { key, krate: self.crate_shown(krate), searched: format!("{rows}; not an item declared in a body (its key's inner bit is clear)").into_boxed_str() });
            }
            return self.locate_local(key, krate, &rows);
        }
        // Fast path: if the full crate index is already cached, check it; otherwise probe `#[lang]`
        // containers and already-loaded store containers (with `MemberWant::NameOnly`, zero file
        // text reads and zero `each_item` scan) before falling back to a crate-wide member index.
        if let Some(found) = self.member_rows.get(&krate).and_then(|ix| ix.rows.get(&key).copied()) {
            let Some(row) = found else {
                return Err(LazyRefusal::NotFound { key, krate: self.crate_shown(krate), searched: Box::from("two members of its crate are keyed alike (rule 21)") });
            };
            return self.file_member(key, row);
        }
        if !self.member_rows.contains_key(&krate)
            && let Some(found) = self.probe_fast_member(key, krate)
        {
            let Some(row) = found else {
                return Err(LazyRefusal::NotFound { key, krate: self.crate_shown(krate), searched: Box::from("two members of its crate are keyed alike (rule 21)") });
            };
            return self.file_member(key, row);
        }
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

    /// Returns the shared crate-wide [`super::shared::SharedMemberIndex`] for `krate` when a
    /// [`super::shared::SharedLazyFront`] is attached, building and publishing it once on miss.
    pub(super) fn shared_crate_member_index(
        &mut self,
        krate: usize,
    ) -> Result<Option<std::sync::Arc<super::shared::SharedMemberIndex, blaze_own::SharedAlloc<'static>>>, LazyRefusal> {
        let Some(shared) = self.shared() else {
            return Ok(None);
        };
        let Some(crate_key) = self.front.names.crate_key(krate) else {
            return Ok(None);
        };
        if let Some(ix) = shared.get_crate_members(crate_key) {
            return Ok(Some(ix));
        }
        let Some(built) = build_shared_crate_member_index(self.front.names, krate, self.alloc, shared.alloc()) else {
            return Err(LazyRefusal::Unanswered { question: "each_item", item: self.crate_shown(krate) });
        };
        Ok(Some(shared.put_crate_members(crate_key, built)))
    }

    /// Probes already-loaded `store.maps.traits` / `store.impls` matching `key`'s crate prefix.
    fn probe_loaded_containers(&self, key: ItemKey) -> Option<MemberRow<'n>> {
        let names = self.front.names;
        let key_bytes = key.0.to_be_bytes();
        let prefix = key_bytes.get(..8)?;
        for (&trait_lib, _) in self.store.maps.traits.iter() {
            if trait_lib.0.0.get(..8) == Some(prefix)
                && let Some(row) = probe_container_member(names, trait_lib, key)
            {
                return Some(row);
            }
        }
        for (&impl_ck, _) in self.store.impls.iter() {
            let impl_bytes = impl_ck.0.to_be_bytes();
            if impl_bytes.get(..8) == Some(prefix)
                && let Some(impl_lib) = self.front.keys.library_in(names, impl_ck)
                && let Some(row) = probe_container_member(names, impl_lib, key)
            {
                return Some(row);
            }
        }
        None
    }

    /// Probes targeted containers in `krate` (`#[lang]` items cached once per crate, plus already
    /// loaded `store.maps.traits` / `store.impls` matching `key`'s crate prefix) using
    /// `MemberWant::NameOnly` so common `core`/`std` members like `PartialEq::eq` resolve without
    /// scanning all ~5k containers in `core`.
    fn probe_fast_member(&mut self, key: ItemKey, krate: usize) -> Option<Option<MemberRow<'n>>> {
        let names = self.front.names;
        if !self.lang_member_rows.contains_key(&krate) {
            let ix = build_lang_member_index(names, krate, self.alloc);
            self.lang_member_rows.insert(krate, ix);
        }
        if let Some(found) = self.lang_member_rows.get(&krate).and_then(|ix| ix.rows.get(&key).copied()) {
            return Some(found);
        }
        self.probe_loaded_containers(key).map(Some)
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
    /// or unit struct's constructor (`MemberKind::Ctor`, named ""). Uses `MemberWant::NameOnly`
    /// so zero source files are opened or outlined. Returns how many containers were read.
    pub(super) fn member_index(&mut self, krate: usize) -> Result<u32, LazyRefusal> {
        if let Some(ix) = self.member_rows.get(&krate) {
            return Ok(ix.containers);
        }
        let Some(ix) = build_crate_member_index(self.front.names, krate, self.alloc) else {
            return Err(LazyRefusal::Unanswered { question: "each_item", item: self.crate_shown(krate) });
        };
        let n = ix.containers;
        self.member_rows.insert(krate, ix);
        Ok(n)
    }

    /// Files the member `row` asked for as `key`: its container as an item, then the member.
    fn file_member(&mut self, key: ItemKey, row: MemberRow<'n>) -> Result<(), LazyRefusal> {
        self.file_member_parts(key, row.container, row.kind, row.name)
    }

    fn file_member_parts(&mut self, key: ItemKey, container_lib: LibKey, kind: MemberKind, name: &str) -> Result<(), LazyRefusal> {
        let clash = |c: crate::front2::keys::KeyClash| LazyRefusal::Head { item: format!("{container_lib:x}").into_boxed_str(), why: format!("{c}").into_boxed_str() };
        let container = self.front.keys.item(container_lib).map_err(clash)?;
        let filed = self.front.keys.member(container, kind, name).map_err(clash)?;
        if filed != key {
            return Err(LazyRefusal::NotFound { key, krate: self.refused_shown(container), searched: Box::from("its member index row filed under another key (keys.rs `member_key_of` and `Keys::member` disagree)") });
        }
        Ok(())
    }

    /// Builds `krate`'s [`LocalIndex`] once on the first unhinted `locate_local` miss: scans
    /// every fn in `krate` with `body_holds_items == true`, derives every text-keyable local item's
    /// lowering chain into `by_key`, and records any `Unkeyed::AfterCall` chains for one-time
    /// demand lowering.
    fn ensure_local_index(&mut self, key: ItemKey, krate: usize, rows: &str) -> Result<(), LazyRefusal> {
        if self.local_rows.contains_key(&krate) {
            return Ok(());
        }
        let names = self.front.names;
        let alloc = self.alloc;
        let (Some(crate_key), Some(edition), Some(atoms)) = (names.crate_key(krate), names.edition_of(krate), names.cfg_atoms(krate)) else {
            return Err(LazyRefusal::Unanswered { question: "crate_key, edition_of, cfg_atoms", item: self.crate_shown(krate) });
        };
        // The crate's fns whose body holds items, free and members, in key order.
        let mut tops: AVec<'n, Searched<'n>> = AVec::new_in(alloc);
        let mut containers: AVec<'n, LibKey> = AVec::new_in(alloc);
        let mut unread = 0u32;
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
        let mut matching_names: AVec<'n, &'n str> = AVec::new_in(alloc);
        for &container in containers.iter() {
            matching_names.clear();
            names.each_member_matching(
                container,
                &mut |kind, mname| {
                    if kind == ItemKind::Function && mname.is_some() {
                        crate::front2::source::MemberWant::NameOnly
                    } else {
                        crate::front2::source::MemberWant::Skip
                    }
                },
                &mut |m| {
                    if m.body_holds_items
                        && let Some(name) = m.name
                    {
                        matching_names.push(name);
                    }
                },
            );
            if matching_names.is_empty() {
                continue;
            }
            names.each_member_matching(
                container,
                &mut |kind, mname| {
                    if kind == ItemKind::Function
                        && mname.is_some_and(|n| matching_names.iter().any(|&want| want == n))
                    {
                        crate::front2::source::MemberWant::WithText
                    } else {
                        crate::front2::source::MemberWant::Skip
                    }
                },
                &mut |m| {
                    if m.kind == ItemKind::Function
                        && m.body_holds_items
                        && let Some(name) = m.name
                    {
                        let mk = member_key_of(checker_of(container), MemberKind::Fn, name);
                        names.read_member_body(m.key, m.written_by);
                        tops.push(Searched { lib: m.key, member: Some((container, mk)), text: m.text, name });
                    }
                },
            );
        }
        if unread > 0 {
            return Err(LazyRefusal::Unanswered { question: "text_of", item: format!("{unread} fns of {} whose bodies hold items", self.crate_shown(krate)).into_boxed_str() });
        }
        let reading = Reading { edition, atoms, alloc };
        let mut by_key: Map<ItemKey, LocalEntry<'n>, Alloc<'n>> = Map::new_in(alloc);
        let mut after_call: Vec<LocalEntry<'n>, Global> = Vec::new_in(Global);
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
            let member_row = member.map(|(container, mk)| (mk, MemberRow { container, kind: MemberKind::Fn, name }));
            let chain_of = |within: u32| -> Option<Chain> {
                let keys = walked.chain_to(within, alloc).ok()?.ok()?;
                let mut c: Chain = Vec::new_in(Global);
                c.push(first);
                c.extend(keys.iter().skip(1).map(|&k| LowerTarget::Free(k)));
                Some(c)
            };
            for l in walked.locals.iter() {
                match l.key {
                    Ok(k) => {
                        let ck = checker_of(k);
                        if !by_key.contains_key(&ck)
                            && let Some(chain) = chain_of(l.within)
                        {
                            by_key.insert(ck, LocalEntry { member: member_row, chain });
                        }
                    }
                    Err(Unkeyed::AfterCall) => {
                        if let Some(chain) = chain_of(l.within)
                            && !after_call.iter().any(|e| e.chain == chain)
                        {
                            after_call.push(LocalEntry { member: member_row, chain });
                        }
                    }
                    Err(_) => {}
                }
            }
        }
        self.local_rows.insert(krate, LocalIndex { by_key, after_call, after_call_next: 0, nested_queue: Vec::new_in(Global), read });
        Ok(())
    }

    /// An item declared in a body: the start's chain, else the crate's cached [`LocalIndex`]
    /// (built once per crate), by text, else by lowering any unlowered `AfterCall` bodies.
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
        self.ensure_local_index(key, krate, rows)?;
        if let Some(entry) = self.local_rows.get(&krate).and_then(|ix| ix.by_key.get(&key)).cloned() {
            if let Some((mk, mrow)) = entry.member {
                self.file_member(mk, mrow)?;
            }
            return self.lower_chain(key, &entry.chain).and_then(|()| self.filed_local(key, krate, "the chain its body's text derives"));
        }
        if self.front.keys.local_kind(key).is_some() {
            return Ok(());
        }
        // By lowering: advance through any not-yet-lowered `AfterCall` chains in `krate`, then
        // any nested local fn chains they produce; each chain is lowered at most once per crate.
        let mut lowered = 0usize;
        let read = self.local_rows.get(&krate).map_or(0, |ix| ix.read);
        loop {
            let next_work: Option<(Option<(ItemKey, MemberRow<'n>)>, Chain)> = {
                let Some(ix) = self.local_rows.get_mut(&krate) else { break };
                if let Some(chain) = ix.nested_queue.pop() {
                    Some((None, chain))
                } else if ix.after_call_next < ix.after_call.len() {
                    let entry = ix.after_call[ix.after_call_next].clone();
                    ix.after_call_next += 1;
                    Some((entry.member, entry.chain))
                } else {
                    None
                }
            };
            let Some((member_row, chain)) = next_work else { break };
            lowered += chain.len();
            if lowered > MOST_SEARCHED {
                return Err(LazyRefusal::NotFound { key, krate: self.crate_shown(krate), searched: format!("{rows}; more bodies than the most one search lowers ({MOST_SEARCHED})").into_boxed_str() });
            }
            if let Some((mk, mrow)) = member_row {
                self.file_member(mk, mrow)?;
            }
            if self.lower_chain(key, &chain).is_err() {
                continue;
            }
            // The fns its lowering kept that hold items of their own: queue for later if needed.
            if let Some(&last) = chain.last() {
                let owner = match last {
                    LowerTarget::Free(k) => checker_of(k),
                    LowerTarget::Member(k) => k,
                };
                if let Some(body) = self.store.locals.body(owner)
                    && let Some(ix) = self.local_rows.get_mut(&krate)
                {
                    for row in body.rows.iter().filter(|r| r.kind == crate::front2::locals::LocalKind::Fn) {
                        let mut next = chain.clone();
                        next.push(LowerTarget::Free(row.key));
                        ix.nested_queue.push(next);
                    }
                }
            }
            if self.front.keys.local_kind(key).is_some() {
                return Ok(());
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

#[cfg(test)]
mod locate_tests {
    use super::*;
    use core::cell::Cell;

    struct SpyMember {
        key: LibKey,
        kind: ItemKind,
        name: Option<&'static str>,
    }

    struct SpyContainer {
        lib: LibKey,
        kind: ItemKind,
        lang: Option<&'static str>,
        ctor: bool,
        members: &'static [SpyMember],
    }

    struct SpyCatalog<'a> {
        containers: &'a [SpyContainer],
        each_item_calls: Cell<usize>,
        name_only_calls: Cell<usize>,
        with_text_calls: Cell<usize>,
        skip_calls: Cell<usize>,
    }

    impl<'a> SpyCatalog<'a> {
        fn new(containers: &'a [SpyContainer]) -> Self {
            Self {
                containers,
                each_item_calls: Cell::new(0),
                name_only_calls: Cell::new(0),
                with_text_calls: Cell::new(0),
                skip_calls: Cell::new(0),
            }
        }
    }

    impl MemberCatalog<'static> for &SpyCatalog<'_> {
        fn catalog_each_lang(self, _krate: usize, each: &mut dyn FnMut(&'static str, LibKey)) {
            for c in self.containers {
                if let Some(lang) = c.lang {
                    each(lang, c.lib);
                }
            }
        }

        fn catalog_each_item(self, _krate: usize, each: &mut dyn FnMut(LibKey, LibKey)) -> bool {
            self.each_item_calls.set(self.each_item_calls.get() + 1);
            for c in self.containers {
                each(c.lib, LibKey(blaze_ids::ContentId([0; 32])));
            }
            true
        }

        fn catalog_kind_of_item(self, key: LibKey) -> Option<ItemKind> {
            self.containers.iter().find(|c| c.lib == key).map(|c| c.kind)
        }

        fn catalog_is_ctor(self, key: LibKey) -> bool {
            self.containers.iter().find(|c| c.lib == key).is_some_and(|c| c.ctor)
        }

        fn catalog_each_member_matching(
            self,
            owner: LibKey,
            want: &mut dyn FnMut(ItemKind, Option<&'static str>) -> crate::front2::source::MemberWant,
            each: &mut dyn FnMut(crate::front2::source::MemberOf<'static>),
        ) -> bool {
            let Some(c) = self.containers.iter().find(|c| c.lib == owner) else {
                return false;
            };
            for m in c.members {
                match want(m.kind, m.name) {
                    crate::front2::source::MemberWant::Skip => {
                        self.skip_calls.set(self.skip_calls.get() + 1);
                    }
                    crate::front2::source::MemberWant::NameOnly => {
                        self.name_only_calls.set(self.name_only_calls.get() + 1);
                        each(crate::front2::source::MemberOf {
                            key: m.key,
                            name: m.name,
                            item: blaze_surface::ItemIndex(0),
                            kind: m.kind,
                            body_holds_items: false,
                            detail: None,
                            text: "",
                            written_by: None,
                        });
                    }
                    crate::front2::source::MemberWant::WithText => {
                        self.with_text_calls.set(self.with_text_calls.get() + 1);
                        each(crate::front2::source::MemberOf {
                            key: m.key,
                            name: m.name,
                            item: blaze_surface::ItemIndex(0),
                            kind: m.kind,
                            body_holds_items: false,
                            detail: None,
                            text: "fn dummy() {}",
                            written_by: None,
                        });
                    }
                }
            }
            true
        }
    }

    #[test]
    fn member_lookup_uses_name_only_and_lang_fast_path_skips_each_item() {
        let partial_eq = LibKey(blaze_ids::ContentId([0x11; 32]));
        let other_impl = LibKey(blaze_ids::ContentId([0x22; 32]));
        let tuple_struct = LibKey(blaze_ids::ContentId([0x33; 32]));
        let eq_member = LibKey(blaze_ids::ContentId([0x41; 32]));
        let mac_member = LibKey(blaze_ids::ContentId([0x42; 32]));
        let push_member = LibKey(blaze_ids::ContentId([0x43; 32]));

        const EQ_MEMBERS: [SpyMember; 2] = [
            SpyMember { key: LibKey(blaze_ids::ContentId([0x41; 32])), kind: ItemKind::Function, name: Some("eq") },
            SpyMember { key: LibKey(blaze_ids::ContentId([0x42; 32])), kind: ItemKind::MacroCall, name: None },
        ];
        const IMPL_MEMBERS: [SpyMember; 1] = [
            SpyMember { key: LibKey(blaze_ids::ContentId([0x43; 32])), kind: ItemKind::Function, name: Some("push") },
        ];
        let _ = (eq_member, mac_member, push_member);
        let containers = [
            SpyContainer { lib: partial_eq, kind: ItemKind::Trait, lang: Some("eq"), ctor: false, members: &EQ_MEMBERS },
            SpyContainer { lib: other_impl, kind: ItemKind::Impl, lang: None, ctor: false, members: &IMPL_MEMBERS },
            SpyContainer { lib: tuple_struct, kind: ItemKind::Struct, lang: None, ctor: true, members: &[] },
        ];
        let spy = SpyCatalog::new(&containers);
        let alloc = blaze_own::testing::heap().alloc();

        // 1. Targeted lang fast path resolves `PartialEq::eq` with 0 `each_item` calls and 0 `WithText` calls!
        let lang_ix = build_lang_member_index(&spy, 0, alloc);
        let eq_key = member_key_of(checker_of(partial_eq), MemberKind::Fn, "eq");
        assert_eq!(
            lang_ix.rows.get(&eq_key).copied().flatten(),
            Some(MemberRow { container: partial_eq, kind: MemberKind::Fn, name: "eq" }),
        );
        assert_eq!(spy.each_item_calls.get(), 0, "lang fast path must never scan all crate items");
        assert_eq!(spy.with_text_calls.get(), 0, "member indexing must never request WithText");
        assert_eq!(spy.name_only_calls.get(), 1, "only the fn member is read as NameOnly");
        assert_eq!(spy.skip_calls.get(), 1, "MacroCall member is skipped without reading");

        // 2. Single-container probe finds non-lang impl method with 0 `each_item` and 0 `WithText`.
        let push_key = member_key_of(checker_of(other_impl), MemberKind::Fn, "push");
        assert_eq!(
            probe_container_member(&spy, other_impl, push_key),
            Some(MemberRow { container: other_impl, kind: MemberKind::Fn, name: "push" }),
        );
        assert_eq!(spy.each_item_calls.get(), 0);
        assert_eq!(spy.with_text_calls.get(), 0);

        // 3. Full crate fallback also uses NameOnly (0 WithText) and indexes tuple struct ctors.
        let full_ix = build_crate_member_index(&spy, 0, alloc).expect("full member index");
        assert_eq!(spy.each_item_calls.get(), 1);
        assert_eq!(spy.with_text_calls.get(), 0, "full crate member_index must never read source text");
        let ctor_key = member_key_of(checker_of(tuple_struct), MemberKind::Ctor, "");
        assert_eq!(
            full_ix.rows.get(&ctor_key).copied().flatten(),
            Some(MemberRow { container: tuple_struct, kind: MemberKind::Ctor, name: "" }),
        );
        assert_eq!(
            full_ix.rows.get(&push_key).copied().flatten(),
            Some(MemberRow { container: other_impl, kind: MemberKind::Fn, name: "push" }),
        );
    }
}

