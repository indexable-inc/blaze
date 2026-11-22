//! Inherent constant leaves of a head's constant expression: `[u8; InlineExtension::MAX]` (http's
//! `struct InlineExtension([u8; InlineExtension::MAX], u8)`), `[u8; Self::MAX_SIZE]` (const-oid's
//! `ObjectIdentifier`), `-> [u8; Self::ENCODED_LEN]` (a method of the impl that declares it),
//! `<W>::N * 8`.
//!
//! The Reference, "Array types": "The size is a constant expression of type `usize`"; "Constant
//! evaluation", "Const context": array type length expressions are const contexts, and "Paths",
//! "`Self`": in an implementation `Self` "refers to the type being implemented"; in a struct
//! definition it is the struct. "Associated Items", "Associated Constants": an inherent impl's
//! `const NAME: T = e;` is a constant associated with the type. So the value of the leaf is the
//! value of THAT constant item: an impl member with its own key and its own body.
//!
//! Why here and not in the anonymous constant's own check ([`super::anon`]): checked as the
//! body `const _: usize = InlineExtension::MAX;`, the path's prefix is a type, and the checker
//! reads that type's whole row to find out whether `MAX` is one of its variants (blaze-typeck
//! rules/lit_path.rs, `Def::Adt`), which lowers its field types, which hold this very length:
//! the evaluation needed itself (census 14: "Self::MAX_SIZE: constant ..: a body it needs:
//! checking ..: the constant .. an array length names: Nested"). rustc keeps an ADT's variants
//! apart from its fields' types; here the leaf is answered without the type's row at all:
//!
//! 1. [`substitute`] finds each path `T::NAME` (and `<T>::NAME`) of the text whose prefix is
//!    `Self` naming a ground ADT, or a path the names library resolves to a struct, enum or
//!    union with no generic parameters; each is asked under its need key ([`need_key`]: the
//!    ADT's key and the name, nothing else, so every head that writes it shares one value),
//!    and the text is written with each answered leaf replaced by its value as a typed literal
//!    (`39usize`), as [`super::const_qpath`] writes a qualified leaf's;
//! 2. the driver's `check::eval_len` calls [`eval_asked`] (and the evaluator's work list
//!    calls [`first`], consts.rs `Consts::first`), which makes the type's inherent impl HEADERS
//!    (`Fill::Inherent`, the body check's own fill: an impl's header needs the type's generics,
//!    never its fields), finds the one associated constant of that name among them, and
//!    evaluates it by its own key (`Consts::value`, the one route a constant item's body takes).
//!
//! A leaf the type's inherent impls declare no constant for (a variant `E::A`, a trait's
//! constant), and a constant whose type is not `bool`, `char` or an integer (`Date::MIN:
//! Date` in `{ Date::MIN.to_julian_day() }`: no literal can stand for it), is NOT a leaf: the
//! text keeps it and the anonymous constant reads it as written (with that machine's own
//! refusal, by name, where it has none). Refused, each by name, filed TYPED in the table under
//! the need key ([`InherentFail`], read by [`refusal`]; the head is refused naming the leaf and
//! this): a refused inherent impl of the type (the constant may be in it), two inherent
//! constants of one name, a constant with no head or no type node, its evaluation's own
//! refusal, a leaf that needs itself. NOT taken (the head's refusal stays as before, by name):
//! `Self` that is generic or not an ADT (`impl<T> W<T> { fn f() -> [u8; Self::N] }` needs an
//! unevaluated const term in blaze-typeck's type form, which has none: owed), an absolute path.
//! More than [`MOST_LEAVES`] leaves in one store is refused by name (rule 18).

use blaze_id::ItemKey as LibKey;
use blaze_ids::ContentId;
use blaze_lower::scoped::Prepares;
use blaze_own::{Alloc, String, Vec};
use blaze_typeck as tc;
use blaze_typeck::{ItemKey, Name, PNode, PTys, Shape};

use super::anon::prim_name;
use super::check::{Records, make_for};
use super::consts::{ConstFail, Consts, int_of};
use super::keys::{anon_const_key, checker_of};
use super::source::NamesSource;
use super::types::{Env, Heads, SelfTy};
use super::world_items::Fill;
use crate::resolve::{Kind, ResolveError, Target};

/// The most inherent leaves one store keeps (rule 18): past it a head is refused by name.
pub const MOST_LEAVES: usize = 512;

/// The most segments of a leaf's path (rule 18); a longer path is left in the text.
const MOST_SEGS: usize = 8;

/// The most candidate paths read in one expression (rule 18): past it the head is refused by
/// name ([`Stop::Full`]).
const MOST_IN_TEXT: usize = 32;

/// Where one leaf is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    /// Asked by a head; not answered yet.
    Asked,
    /// Being answered now (a head made meanwhile that asks it again needs itself).
    Running,
    /// Answered: its bits are in `Maps::const_bits` under the need key, of this type.
    Answered(PNode),
    /// The type's inherent impls declare no constant of that name: no leaf; the text keeps it.
    NotLeaf,
    /// Refused: the typed refusal is kept in the table ([`refusal`]).
    Refused,
}

/// One asked leaf.
#[derive(Clone, Copy, Debug)]
pub struct Entry {
    /// What `Why::NeedConst` and `Maps::const_bits` are keyed by ([`need_key`]).
    pub need: LibKey,
    /// The type's checker key.
    pub adt: ItemKey,
    /// The type's library key, where the leaf wrote a path (`None` for `Self`, whose type's
    /// key is filed already: its head is the one being made).
    pub lib: Option<LibKey>,
    pub name: Name,
    pub state: State,
}

/// The asked leaves, kept with the heads (`Maps::inherents`), at most [`MOST_LEAVES`] (rule
/// 18). In the store's allocator, never inline: a fixed array of `MOST_LEAVES` entries made
/// the heads `Store` some 70 KB by value, and its default, built on the stack in a debug build,
/// overflowed a test thread's stack (wave 1119, `engine_body_reads_memory_tests`).
pub struct Table {
    entries: Vec<Entry, Alloc<'static>>,
    /// Each refused leaf's typed refusal, by need key: made at the first refusal, in the
    /// allocator of the store's type table (`PortableTys::alloc`), which the driver hands in.
    refusals: Option<Vec<(LibKey, InherentFail), Alloc<'static>>>,
}

impl Table {
    /// No leaf asked yet; the entries in `alloc` (the store's, `Maps::new_in`).
    pub fn new_in(alloc: Alloc<'static>) -> Table {
        Table { entries: Vec::new_in(alloc), refusals: None }
    }
}

/// Reads the typed refusal of the leaf `need` (`None`: not refused) with `f`, while the table
/// is borrowed: the head that wrote the leaf is refused naming the leaf and this.
pub fn refusal<R>(table: &core::cell::RefCell<Table>, need: LibKey, f: impl FnOnce(Option<&InherentFail>) -> R) -> R {
    let t = table.borrow();
    let found = t.refusals.as_ref().and_then(|v| v.as_slice().iter().find(|(k, _)| *k == need)).map(|(_, why)| why);
    f(found)
}

/// The leaf being evaluated now ([`State::Running`]) of the type `adt` whose name `declares`
/// says the impl being made declares: a constant of that impl whose type names it waits for
/// its value, and the impl's header is made without that constant's row (heads.rs
/// `Store::impl_`, `ConstWait`). `None`: no such leaf.
pub fn running_sibling(table: &core::cell::RefCell<Table>, adt: ItemKey, declares: &dyn Fn(Name) -> bool) -> Option<LibKey> {
    let t = table.borrow();
    t.entries.iter().find(|e| e.state == State::Running && e.adt == adt && declares(e.name)).map(|e| e.need)
}

impl Table {
    fn find(&self, need: LibKey) -> Option<&Entry> {
        self.entries.iter().find(|e| e.need == need)
    }

    fn set(&mut self, need: LibKey, state: State) {
        if let Some(e) = self.entries.iter_mut().find(|e| e.need == need) {
            e.state = state;
        }
    }

    /// Whether `need` is a leaf's need key asked here.
    pub fn has(&self, need: LibKey) -> bool {
        self.find(need).is_some()
    }

    /// The state of the leaf `need`, once asked.
    pub fn state(&self, need: LibKey) -> Option<State> {
        self.find(need).map(|e| e.state)
    }

    /// Keeps `entry` (one asked twice is kept once); `false` when the table is full.
    fn ask(&mut self, entry: Entry) -> bool {
        if self.has(entry.need) {
            return true;
        }
        if self.entries.len() == MOST_LEAVES {
            return false;
        }
        self.entries.push(entry);
        true
    }

    /// The `i`th entry still [`State::Asked`] at or after slot `from`, with its slot.
    fn next_asked(&self, from: usize) -> Option<(usize, Entry)> {
        self.entries.get(from..)?.iter().enumerate().find(|(_, e)| e.state == State::Asked).map(|(j, e)| (from + j, *e))
    }
}

/// The need key of the inherent leaf `NAME` of the type `adt`: a function of the two and
/// nothing else. Its first 16 bytes are a member key under the type's checker key (so the
/// names library finds its crate by the prefix), the rest a hash under this module's own
/// domain: no library item, anonymous constant or qualified leaf has it.
pub fn need_key(adt: ItemKey, name: &str) -> LibKey {
    let first = anon_const_key(adt, name).0.to_be_bytes();
    let rest = ContentId::of("blaze-ladder front2 inherent constant need 1", &first);
    let mut bytes = [0u8; 32];
    bytes[..16].copy_from_slice(&first);
    bytes[16..].copy_from_slice(&rest.0[..16]);
    blaze_id::ItemKey(ContentId(bytes))
}

/// The checker key of `Self` where it is an ADT with no generic arguments (a struct's own
/// definition, an inherent impl of a non-generic type and its members); `None` otherwise.
pub fn ground_self_adt(tys: PTys<'_>, env: &Env<'_>) -> Option<ItemKey> {
    let SelfTy::Ty(t, _) = env.self_ else { return None };
    match tys.get(t).copied()? {
        PNode::Adt(k, args) if args.len == 0 => Some(k),
        _ => None,
    }
}

/// Why [`substitute`] has no text yet.
#[derive(Debug)]
pub enum Stop {
    /// A leaf has no value yet: the driver answers this need key (and every leaf asked with
    /// it) and the head is made again.
    Need(LibKey),
    /// The leaf at `start..end` was tried and refused: its typed refusal is read by
    /// [`refusal`] under `need`.
    Failed { start: usize, end: usize, need: LibKey },
    /// The leaf at `start..end` is asked while it is being answered: it needs itself.
    Cycle { start: usize, end: usize },
    /// The table holds [`MOST_LEAVES`] already, or the text has more than [`MOST_IN_TEXT`]
    /// candidate paths.
    Full,
}

fn ident_start(b: u8) -> bool {
    b.is_ascii_alphabetic() || b == b'_' || b >= 0x80
}

fn ident_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b >= 0x80
}

fn skip_ws(b: &[u8], mut i: usize) -> usize {
    while i < b.len() && b[i].is_ascii_whitespace() {
        i += 1;
    }
    i
}

/// The last byte before `i` that is not whitespace.
fn prev_byte(b: &[u8], i: usize) -> Option<u8> {
    b[..i].iter().rev().copied().find(|c| !c.is_ascii_whitespace())
}

/// One candidate: `prefix::NAME` at `start..end` of the text; `prefix` is `segs[..n - 1]`.
struct Candidate<'t> {
    start: usize,
    end: usize,
    segs: [&'t str; MOST_SEGS],
    n: usize,
}

/// Reads the path `a::b::c` starting at the identifier at `i`: its segments and end, and
/// whether it is a value path a leaf can be (not followed by `::<`, `(`, `!` or `{`). `None`
/// for a longer path than [`MOST_SEGS`].
fn read_path<'t>(text: &'t str, i: usize) -> (usize, Option<([&'t str; MOST_SEGS], usize)>) {
    let b = text.as_bytes();
    let mut segs = [""; MOST_SEGS];
    let mut n = 0usize;
    let mut at = i;
    let mut ok = true;
    loop {
        let s = at;
        while at < b.len() && ident_byte(b[at]) {
            at += 1;
        }
        if n == MOST_SEGS {
            ok = false;
        } else {
            segs[n] = text.get(s..at).unwrap_or("");
            n += 1;
        }
        let j = skip_ws(b, at);
        if b.get(j) == Some(&b':') && b.get(j + 1) == Some(&b':') {
            let k = skip_ws(b, j + 2);
            match b.get(k) {
                Some(&c) if ident_start(c) => {
                    at = k;
                    continue;
                }
                // `size_of::<Self>()`: a turbofish, never a constant's path.
                _ => return (k, None),
            }
        }
        let next = b.get(j).copied();
        if matches!(next, Some(b'(' | b'!' | b'{')) || !ok {
            return (at, None);
        }
        return (at, Some((segs, n)));
    }
}

/// The candidates of `text`, in order: `T::NAME` paths and `<T>::NAME` (a `<` where an
/// operand starts, a plain path inside, `>::NAME` after it). A path after `.`, `::` or `'`,
/// an absolute path, and a number's suffix are none.
fn candidates<'t>(text: &'t str, out: &mut [Option<Candidate<'t>>; MOST_IN_TEXT]) -> Option<usize> {
    let b = text.as_bytes();
    let mut i = 0usize;
    let mut count = 0usize;
    while i < b.len() {
        let c = b[i];
        if c.is_ascii_digit() {
            while i < b.len() && ident_byte(b[i]) {
                i += 1;
            }
            continue;
        }
        if c == b'<' {
            // `<T>::NAME` where an operand may start: not after an operand, where `<`
            // compares or shifts.
            let operand_before = prev_byte(b, i).is_some_and(|p| ident_byte(p) || p == b')' || p == b']' || p == b'>');
            let j = skip_ws(b, i + 1);
            if !operand_before && b.get(j).is_some_and(|&x| ident_start(x)) {
                let (end, path) = read_path(text, j);
                let k = skip_ws(b, end);
                let after = skip_ws(b, k + 1);
                if let Some((inner, inner_n)) = path
                    && b.get(k) == Some(&b'>')
                    && b.get(after) == Some(&b':')
                    && b.get(after + 1) == Some(&b':')
                {
                    let ns = skip_ws(b, after + 2);
                    let ne = (ns..b.len()).find(|&x| !ident_byte(b[x])).unwrap_or(b.len());
                    let tail = skip_ws(b, ne);
                    let call = matches!(b.get(tail), Some(b'(' | b'!' | b'{')) || b.get(tail) == Some(&b':');
                    if ne > ns && b.get(ns).is_some_and(|&x| ident_start(x)) && !call && inner_n < MOST_SEGS {
                        let mut segs = inner;
                        segs[inner_n] = text.get(ns..ne).unwrap_or("");
                        if count == MOST_IN_TEXT {
                            return None;
                        }
                        out[count] = Some(Candidate { start: i, end: ne, segs, n: inner_n + 1 });
                        count += 1;
                        i = ne;
                        continue;
                    }
                }
                i = end.max(i + 1);
                continue;
            }
            i += 1;
            continue;
        }
        if !ident_start(c) {
            i += 1;
            continue;
        }
        let after_path = prev_byte(b, i).is_some_and(|p| p == b'.' || p == b':' || p == b'\'');
        let (end, path) = read_path(text, i);
        if let Some((segs, n)) = path
            && n >= 2
            && !after_path
        {
            if count == MOST_IN_TEXT {
                return None;
            }
            out[count] = Some(Candidate { start: i, end, segs, n });
            count += 1;
        }
        i = end.max(i + 1);
    }
    Some(count)
}

/// The type a candidate's prefix names, if it is one a leaf is answered for: its checker key
/// and its library key (`None` for `Self`).
fn leaf_type<H: Heads + ?Sized>(heads: &H, resolve: &dyn Fn(bool, &[&str]) -> Result<Target, ResolveError>, self_adt: Option<ItemKey>, prefix: &[&str]) -> Option<(ItemKey, Option<LibKey>)> {
    if prefix == ["Self"] {
        return self_adt.map(|k| (k, None));
    }
    if prefix.iter().any(|s| *s == "Self") {
        return None;
    }
    let found = resolve(false, prefix).ok()?;
    if found.consumed as usize != prefix.len() || !matches!(found.kind, Kind::Struct | Kind::Enum | Kind::Union) {
        return None;
    }
    match heads.adt_generics(found.key) {
        // `W::N` with `W<T>`: not a ground type as written.
        Some((n, _)) if n != 0 => None,
        _ => Some((checker_of(found.key), Some(found.key))),
    }
}

/// `text` with every answered inherent leaf replaced by its value (a typed literal).
/// `Ok(None)`: the text has no leaf (or only ones that are none: [`State::NotLeaf`]), or these
/// heads have no evaluator behind them ([`Heads::inherent_leaves`] is `None`): the text is then
/// read as written. `resolve` reads a path in the head's module, in the type namespace;
/// `self_adt` is [`ground_self_adt`].
pub fn substitute<'a, H: Heads + ?Sized>(
    heads: &H,
    resolve: &dyn Fn(bool, &[&str]) -> Result<Target, ResolveError>,
    self_adt: Option<ItemKey>,
    text: &str,
    alloc: Alloc<'a>,
) -> Result<Option<String<Alloc<'a>>>, Stop> {
    let Some(table) = heads.inherent_leaves() else { return Ok(None) };
    let mut found: [Option<Candidate<'_>>; MOST_IN_TEXT] = [const { None }; MOST_IN_TEXT];
    let Some(count) = candidates(text, &mut found) else { return Err(Stop::Full) };
    let mut out = String::new_in(alloc);
    let mut copied = 0usize;
    let mut first_need: Option<LibKey> = None;
    let mut any = false;
    for c in found[..count].iter().flatten() {
        let (prefix, name) = (&c.segs[..c.n - 1], c.segs[c.n - 1]);
        let Some((adt, lib)) = leaf_type(heads, resolve, self_adt, prefix) else { continue };
        let need = need_key(adt, name);
        let state = table.borrow().state(need);
        match state {
            Some(State::Answered(ty)) => {
                let Some(bits) = heads.const_bits(need) else { continue };
                let Some(before) = text.get(copied..c.start) else { return Ok(None) };
                out.push_str(before);
                if !super::const_qpath::push_literal(&mut out, ty, bits) {
                    return Ok(None);
                }
                copied = c.end;
                any = true;
            }
            Some(State::NotLeaf) => {}
            Some(State::Running) => return Err(Stop::Cycle { start: c.start, end: c.end }),
            Some(State::Refused) => return Err(Stop::Failed { start: c.start, end: c.end, need }),
            Some(State::Asked) => {
                first_need = first_need.or(Some(need));
            }
            None => {
                if !table.borrow_mut().ask(Entry { need, adt, lib, name: Name::of(name), state: State::Asked }) {
                    return Err(Stop::Full);
                }
                first_need = first_need.or(Some(need));
            }
        }
    }
    if let Some(need) = first_need {
        return Err(Stop::Need(need));
    }
    if !any {
        return Ok(None);
    }
    let Some(tail) = text.get(copied..) else { return Ok(None) };
    out.push_str(tail);
    Ok(Some(out))
}

/// Why a leaf is refused.
#[derive(Debug)]
pub enum InherentFail {
    /// Making the type's inherent impl headers failed.
    Fill { adt: ItemKey, error: super::check::CheckFail },
    /// The type's library key has no checker key of its own (rule 21).
    Key { adt: LibKey, clash: super::keys::KeyClash },
    /// An inherent impl of the type is refused (its own refusal is the impl index's gap of
    /// the type, `Buckets::inherent_gap`): the constant may be declared in it.
    /// `why` is the gap's own words (`Buckets::inherent_gap`), printed with it (rule 19).
    RefusedImpl { adt: ItemKey, impl_: LibKey, why: std::boxed::Box<str> },
    /// The constant's own row was refused (its type), by why: a constant whose type needs
    /// its own value (the Reference, "Constant evaluation": "Recursively defining constants is
    /// not allowed") or any other refusal of that row alone (heads.rs `Store::impl_const`).
    OwnRow { constant: ItemKey, why: std::boxed::Box<str> },
    /// The inherent impls were made and the index holds no answer for the type.
    NoAnswer { adt: ItemKey },
    /// Two inherent impls of the type declare a constant of the name (E0592).
    Two { adt: ItemKey, a: ItemKey, b: ItemKey },
    /// The constant has no signature row, or its type no node (a constant of a type that is
    /// not `bool`, `char` or an integer is no leaf at all: `find`).
    Type { constant: ItemKey },
    /// Its value is not an integer of at most 16 bytes.
    NotInteger { constant: ItemKey },
    /// Its evaluation's own refusal.
    Eval { constant: ItemKey, error: ConstFail },
    /// It was asked while no constant evaluator is at hand.
    NoEvaluator { need: ItemKey },
}

impl core::fmt::Display for InherentFail {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            InherentFail::Fill { adt, error } => write!(f, "the inherent impls of the type {adt:?}: {error}"),
            InherentFail::Key { adt, clash } => write!(f, "the type {adt:?}: {clash}"),
            InherentFail::RefusedImpl { adt, impl_, why } => write!(f, "the inherent impl {impl_:?} of the type {adt:?} is refused, and the constant may be declared there: {why}"),
            InherentFail::OwnRow { constant, why } => write!(f, "the inherent constant {constant:?}: its own head was refused: {why}"),
            InherentFail::NoAnswer { adt } => write!(f, "the inherent impls of the type {adt:?} were made and the impl index holds no answer for it"),
            InherentFail::Two { adt, a, b } => write!(f, "two inherent impls of the type {adt:?} declare a constant of this name ({a:?}, {b:?}): E0592"),
            InherentFail::Type { constant } => write!(f, "the inherent constant {constant:?} has no signature row, or its type no node in the records' table"),
            InherentFail::NotInteger { constant } => write!(f, "the inherent constant {constant:?}: its value is not an integer of at most 16 bytes"),
            InherentFail::Eval { constant, error } => write!(f, "the inherent constant {constant:?}: {error}"),
            InherentFail::NoEvaluator { need } => write!(f, "the inherent constant leaf {need:?} is asked where no constant evaluator is at hand"),
        }
    }
}

/// The inherent constant `name` of `adt`: its key and type; `None` when the type's inherent
/// impls declare none of that name (or the type is generic: no leaf). Makes the impls'
/// headers first (`Fill::Inherent`).
fn find<'n, S: NamesSource<'n>>(r: &mut Records<'_, 'n, S>, e: Entry) -> Result<Option<(ItemKey, PNode)>, InherentFail> {
    if let Some(lib) = e.lib {
        // Filed in the key map, as a qualified leaf's type is (const_qpath.rs `eval_one`), so
        // the inherent fill finds the type's crate.
        r.keys.item(lib).map_err(|clash| InherentFail::Key { adt: lib, clash })?;
        // `W::N` whose head was not made when the leaf was asked: a generic `W` is no leaf.
        if r.store.maps.adt_generics(lib).is_some_and(|(n, _)| n != 0) {
            return Ok(None);
        }
    }
    let shape = Shape::Adt(e.adt);
    make_for(r, Fill::Inherent(shape, e.name), checker_of(e.need)).map_err(|error| InherentFail::Fill { adt: e.adt, error })?;
    let Some(b) = r.buckets.as_ref() else { return Err(InherentFail::NoAnswer { adt: e.adt }) };
    if let Some((impl_, why)) = b.inherent_gap(shape, e.name) {
        return Err(InherentFail::RefusedImpl { adt: e.adt, impl_, why });
    }
    let items = b.inherent(shape, e.name).ok_or(InherentFail::NoAnswer { adt: e.adt })?;
    // A constant is counted by its key: two entries of one key are one constant.
    let mut consts = items.iter().filter(|a| matches!(a.kind, tc::AssocKind::Const));
    let Some(one) = consts.next() else { return Ok(None) };
    if let Some(other) = consts.find(|a| a.item != one.item) {
        return Err(InherentFail::Two { adt: e.adt, a: one.item, b: other.item });
    }
    let constant = one.item;
    // A row refused alone (its impl's head made without it: heads.rs `Store::impl_`) is that
    // refusal, by its words, never "no signature row".
    if !r.store.fns.contains_key(&constant)
        && let Some(why) = r.store.fn_gaps.get(&constant)
    {
        return Err(InherentFail::OwnRow { constant, why: std::format!("{why:?}").into_boxed_str() });
    }
    let head = r.store.fns.get(&constant).ok_or(InherentFail::Type { constant })?;
    let ty = r.store.tys.view().node(head.output).map_err(|_| InherentFail::Type { constant })?;
    // A constant of any other type (time's `Date::MIN: Date` in `ri32<{
    // Date::MIN.to_julian_day() }, ..>`) has no literal to stand in the text: it is no leaf, and
    // the whole expression is the anonymous constant's, which reads the constant as a value of
    // its type and goes on (the Reference, "Constant evaluation": a constant expression may
    // read a constant of any type, call `const fn`s on it, and the result is the length's).
    if prim_name(ty).is_none() {
        return Ok(None);
    }
    Ok(Some((constant, ty)))
}

/// Files the outcome of one leaf: its value and type, or its refusal by name.
fn file<'n, S: NamesSource<'n>>(r: &mut Records<'_, 'n, S>, need: LibKey, out: Result<Option<(u128, PNode)>, InherentFail>) {
    let state = match out {
        Ok(Some((bits, ty))) => {
            r.store.maps.file_bits(need, bits);
            State::Answered(ty)
        }
        Ok(None) => State::NotLeaf,
        Err(e) => {
            // Kept typed, in the allocator of the store's type table (the store's own).
            let alloc = r.store.tys.alloc();
            let mut t = r.store.maps.inherents.borrow_mut();
            t.refusals.get_or_insert_with(|| Vec::new_in(alloc)).push((need, e));
            State::Refused
        }
    };
    r.store.maps.inherents.borrow_mut().set(need, state);
    // The impl constants whose rows waited on this leaf (heads.rs `ConstWait`: a sibling
    // constant's type named it while it was evaluated) are made now, with its value or its
    // refusal in the text.
    r.store.remake_waiting(r.names, need);
}

/// Answers the leaf under need key `c` and every other asked leaf with no answer yet (one round
/// for a head of many leaves), with the evaluator `r` holds. `false`: `c` is no leaf's need key
/// (nothing done). Called by `check::eval_len` before it evaluates `c` as a constant.
pub fn eval_asked<'n, S: NamesSource<'n>>(r: &mut Records<'_, 'n, S>, c: LibKey) -> bool {
    if !r.store.maps.inherents.borrow().has(c) {
        return false;
    }
    let mut from = 0usize;
    loop {
        let next = r.store.maps.inherents.borrow().next_asked(from);
        let Some((slot, e)) = next else { break };
        from = slot + 1;
        r.store.maps.inherents.borrow_mut().set(e.need, State::Running);
        let out = match find(r, e) {
            Ok(Some((constant, ty))) => match r.consts.take() {
                Some((consts, prepared)) => {
                    let got = consts.value(r, prepared, constant).map(int_of);
                    r.consts = Some((consts, prepared));
                    match got {
                        Ok(Some(bits)) => Ok(Some((bits, ty))),
                        Ok(None) => Err(InherentFail::NotInteger { constant }),
                        Err(error) => Err(InherentFail::Eval { constant, error }),
                    }
                }
                None => Err(InherentFail::NoEvaluator { need: checker_of(e.need) }),
            },
            Ok(None) => Ok(None),
            Err(e) => Err(e),
        };
        file(r, e.need, out);
    }
    true
}

/// The evaluator's work list (consts.rs `Consts::first`) asks the leaf `c` while an evaluation
/// is under way: answered with that evaluator. `None`: `c` is no leaf's need key. `Some(Err)`:
/// the leaf's constant needs another constant first (`const_open::needed_first`): the leaf
/// stays asked and the work list evaluates that one, then asks again.
pub fn first<'n, S: NamesSource<'n>>(consts: &mut Consts<'n>, r: &mut Records<'_, 'n, S>, prepared: &Prepares<'n>, c: LibKey) -> Option<Result<(), ConstFail>> {
    let e = *r.store.maps.inherents.borrow().find(c)?;
    if e.state != State::Asked {
        return Some(Ok(()));
    }
    r.store.maps.inherents.borrow_mut().set(c, State::Running);
    let out = match find(r, e) {
        Ok(Some((constant, ty))) => match consts.value(r, prepared, constant).map(int_of) {
            Ok(Some(bits)) => Ok(Some((bits, ty))),
            Ok(None) => Err(InherentFail::NotInteger { constant }),
            Err(error) if super::const_open::needed_first(&error).is_some() => {
                r.store.maps.inherents.borrow_mut().set(c, State::Asked);
                return Some(Err(error));
            }
            Err(error) => Err(InherentFail::Eval { constant, error }),
        },
        Ok(None) => Ok(None),
        Err(e) => Err(e),
    };
    file(r, c, out);
    Some(Ok(()))
}
